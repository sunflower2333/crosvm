// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license.

//! Direct scanout acceptance and per-buffer release are different events.
//! Waiting for the current front buffer before admitting its replacement
//! deadlocks SurfaceControl. Neither event proves GPU producer completion:
//! the guest must retire its writes and enforce writer leases independently.

use super::*;

/// `CROSVM_NATIVE_AHB_TRACE=1` writes one ftrace marker per present, release
/// wait and observed release, so a host trace aligns them with SurfaceFlinger's
/// latch and present. Off by default; unset, it costs one cached load.
pub(super) fn native_ahb_trace(args: std::fmt::Arguments) {
    use std::io::Write;
    static MARKER: std::sync::OnceLock<Option<std::fs::File>> = std::sync::OnceLock::new();
    let marker = MARKER.get_or_init(|| {
        if !std::env::var("CROSVM_NATIVE_AHB_TRACE").is_ok_and(|v| v == "1") {
            return None;
        }
        ["/sys/kernel/tracing/trace_marker", "/sys/kernel/debug/tracing/trace_marker"]
            .iter()
            .find_map(|path| std::fs::OpenOptions::new().write(true).open(path).ok())
    });
    if let Some(mut file) = marker.as_ref() {
        let _ = file.write_all(format!("NAHB {}", args).as_bytes());
    }
}

struct NativeAhbWait {
    resource_id: u32,
    sequence: u64,
    refresh_generation: Option<u64>,
    descriptor: ReturnDescriptor,
}

// These retries belong to one guest-owned PRESENT reservation, never a
// periodic redraw. Leave ample margin inside the guest's five-second wait.
const NATIVE_AHB_PRESENT_RETRY_INTERVAL: Duration = Duration::from_millis(8);
const NATIVE_AHB_PRESENT_RETRY_DEADLINE: Duration = Duration::from_millis(128);
const NATIVE_AHB_PRESENT_MAX_ATTEMPTS: u8 = 8;

struct NativeAhbPendingPresent {
    resource_id: u32,
    sequence: u64,
    generation: u64,
    attempts: u8,
    next_retry: std::time::Instant,
    deadline: std::time::Instant,
    descriptor: ReturnDescriptor,
}

#[derive(Default)]
pub(super) struct NativeAhbReleaseState {
    pub(super) sequences: BTreeMap<u32, u64>,
    pub(super) accepted: u64,
    pub(super) released: u64,
    last_sequence: u64,
    waits: Vec<NativeAhbWait>,
    presents: Vec<NativeAhbPendingPresent>,
}

impl NativeAhbReleaseState {
    pub(super) fn waiting_count(&self) -> usize {
        self.waits.len()
    }

    pub(super) fn has_waiters(&self) -> bool {
        !self.waits.is_empty() || !self.presents.is_empty()
    }

    pub(super) fn reset_resource_identities(&mut self) -> anyhow::Result<()> {
        if self.has_waiters() {
            return Err(anyhow!("cannot reset native AHB identities with pending native operations"));
        }
        self.sequences.clear();
        // Keep process counters and last_sequence monotonic across resets.
        // An old release sequence must never identify a new allocation.
        Ok(())
    }
}

#[cfg(test)]
mod reset_tests {
    use super::*;

    #[test]
    fn reset_forgets_resource_identity_without_reusing_sequences_or_counters() {
        let mut state = NativeAhbReleaseState::default();
        state.sequences.insert(23, 71);
        state.last_sequence = 71;
        state.accepted = 42;
        state.released = 42;
        state.reset_resource_identities().unwrap();
        assert!(state.sequences.is_empty());
        assert_eq!(state.last_sequence, 71);
        assert_eq!(state.accepted, 42);
        assert_eq!(state.released, 42);
    }
}

impl Frontend {
    fn trace_native_ahb_lifecycle(&self, event: &str, resource_id: u32, sequence: u64, count: u64) {
        // Keep tracing cheap enough for high-refresh presentation. These
        // counters describe actual transactions and last-reader retirement,
        // not GPU completion or an inferred number of pixel copies.
        if count <= 4 || count.is_power_of_two() {
            info!(
                "NATIVE-AHB-LIFETIME: event={} res={} buffer_id={:?} sequence={} accepted={} released={} readers={} waiters={} paging_operations={} paging_failures={}",
                event,
                resource_id,
                self.virtio_gpu.native_ahb_buffer_id(resource_id),
                sequence,
                self.native_ahb_release.accepted,
                self.native_ahb_release.released,
                self.pending_flip_fences.iter().filter(|reader| reader.ticket.is_none()).count(),
                self.native_ahb_release.waits.len(),
                self.native_ahb_paging.operations,
                self.native_ahb_paging.failures,
            );
        }
    }

    pub(super) fn note_native_ahb_released(&mut self, resource_id: u32, sequence: u64) {
        native_ahb_trace(format_args!("release r={} seq={}", resource_id, sequence));
        self.native_ahb_release.released = self.native_ahb_release.released.saturating_add(1);
        self.trace_native_ahb_lifecycle("released", resource_id, sequence, self.native_ahb_release.released);
    }

    fn finish_native_ahb_operation(
        mut descriptor: ReturnDescriptor,
        response: GpuResponse,
    ) -> ReturnDescriptor {
        descriptor.len = response
            .encode(0, 0, 0, 0, &mut descriptor.desc_chain.writer)
            .unwrap_or(0);
        descriptor
    }

    pub(super) fn native_ahb_has_reader(&self, resource_id: u32) -> bool {
        self.pending_flip_fences
            .iter()
            .any(|reader| reader.resource_id == resource_id)
    }

    pub(super) fn native_ahb_present_pending(&self, resource_id: u32) -> bool {
        self.native_ahb_release.presents.iter().any(|present| present.resource_id == resource_id)
    }

    pub(super) fn native_ahb_operation_pending(&self, resource_id: u32) -> bool {
        self.native_ahb_present_pending(resource_id)
            || self.native_ahb_release.waits.iter().any(|wait| wait.resource_id == resource_id)
    }

    pub(super) fn native_ahb_present_retry_deadline(&self) -> Option<std::time::Instant> {
        self.native_ahb_release.presents.iter()
            .map(|present| present.next_retry.min(present.deadline)).min()
    }

    pub(super) fn process_native_ahb_present_retries(&mut self, now: std::time::Instant,
        ctrl_queue: &dyn QueueReader, cursor_queue: Option<&dyn QueueReader>) -> anyhow::Result<()> {
        let completed = self.retry_native_ahb_presents(now);
        self.publish_native_ahb_completions(completed, ctrl_queue, cursor_queue)
    }

    pub(super) fn publish_native_ahb_completions(&self, completed: Vec<ReturnDescriptor>,
        ctrl_queue: &dyn QueueReader, cursor_queue: Option<&dyn QueueReader>) -> anyhow::Result<()> {
        // Match ordinary queue publication: an asynchronous renderer error
        // must either precede publication and retain it, or follow it. Do not
        // hold this lock across the backend call, which may invoke callbacks.
        let mut state = self.fence_state.lock();
        if state.renderer_error.is_some() {
            for descriptor in completed {
                state.queue.push_complete(VirtioGpuRing::Global, descriptor);
            }
        } else {
            return_fenced_descriptors(completed, ctrl_queue, cursor_queue);
        }
        state.check_renderer()
    }

    pub(super) fn retry_native_ahb_presents(&mut self, now: std::time::Instant) -> Vec<ReturnDescriptor> {
        let mut completed = Vec::new();
        let mut index = 0;
        while index < self.native_ahb_release.presents.len() {
            // Retain the original reservation on renderer failure. A later
            // callback cannot restart this stopped activation's presentation.
            if self.fence_state.lock().renderer_error.is_some() {
                break;
            }
            // One backend call may consume time while other reservations
            // wait. Never start their retries using a stale batch timestamp.
            let now = now.max(std::time::Instant::now());
            let pending = &self.native_ahb_release.presents[index];
            if now < pending.next_retry.min(pending.deadline) {
                index += 1;
                continue;
            }
            let mut pending = self.native_ahb_release.presents.remove(index);
            let id = pending.resource_id;
            let display = self.virtio_gpu.display().clone();
            let generation = display.borrow().consumer_generation();
            let attached = display.borrow().has_consumer();
            let valid = !self.display_failed && self.virtio_gpu.is_native_ahb_resource(id)
                && self.virtio_gpu.native_resource_geometry_current(id)
                && !self.native_ahb_paging.poisoned.contains(&id) && !self.native_ahb_has_reader(id);
            if !valid {
                info!("NATIVE-AHB-RETRY: event=abandoned res={} sequence={} reason=invalid", id, pending.sequence);
                completed.push(Self::finish_native_ahb_operation(pending.descriptor, GpuResponse::ErrUnspec));
                continue;
            }
            if !attached || generation != pending.generation || !self.virtio_gpu.is_scanout_resource(id) {
                // This reservation took no reader. Closed/superseded work can
                // finish undisplayed; it must never replay into a new owner.
                completed.push(self.finish_native_ahb_present(id, pending.sequence, pending.descriptor,
                    Ok(GpuResponse::OkNoData), Vec::new(), None));
                continue;
            }
            if now >= pending.deadline {
                info!("NATIVE-AHB-RETRY: event=abandoned res={} sequence={} reason=deadline attempts={}",
                    id, pending.sequence, pending.attempts);
                completed.push(Self::finish_native_ahb_operation(pending.descriptor, GpuResponse::ErrUnspec));
                continue;
            }
            pending.attempts += 1;
            // command=0 denotes host-deferred work, not a new guest packet.
            // operationSequence joins this attempt to its decoded original.
            let _fifo_trace = gpu_display::NativeFifoTraceScope::begin(0, id, 0, 0,
                pending.descriptor.ctx_id, 0, 0, pending.descriptor.desc_chain.index() as u32);
            gpu_display::native_fifo_trace_operation(pending.sequence);
            // The guest still owns PresentPending, so no producer may change
            // this buffer while its original descriptor is retained.
            self.virtio_gpu.prepare_native_ahb_refresh(id);
            display.borrow().set_native_refresh_generation(pending.generation);
            let result = self.virtio_gpu.flush_resource(id);
            display.borrow().set_native_refresh_generation(0);
            let readers = self.virtio_gpu.take_pending_flip_fences();
            if readers.is_empty() && (display.borrow().consumer_generation() != pending.generation
                                     || !display.borrow().has_consumer()) {
                completed.push(self.finish_native_ahb_present(id, pending.sequence, pending.descriptor,
                    Ok(GpuResponse::OkNoData), readers, None));
                continue;
            }
            if readers.is_empty() && result.is_ok() && pending.attempts < NATIVE_AHB_PRESENT_MAX_ATTEMPTS {
                pending.next_retry = now + NATIVE_AHB_PRESENT_RETRY_INTERVAL;
                self.native_ahb_release.presents.insert(index, pending);
                index += 1;
                continue;
            }
            let result = if readers.is_empty() && result.is_ok() { Err(GpuResponse::ErrUnspec) } else { result };
            info!("NATIVE-AHB-RETRY: event=complete res={} sequence={} generation={} attempts={} reader={}",
                id, pending.sequence, pending.generation, pending.attempts, !readers.is_empty());
            completed.push(self.finish_native_ahb_present(id, pending.sequence, pending.descriptor, result, readers, None));
        }
        completed
    }

    pub(super) fn complete_native_ahb_waits(&mut self, resource_id: u32) -> Vec<ReturnDescriptor> {
        if self.native_ahb_has_reader(resource_id) {
            return Vec::new();
        }
        let mut completed = Vec::new();
        let mut index = 0;
        while index < self.native_ahb_release.waits.len() {
            let wait = &self.native_ahb_release.waits[index];
            if wait.resource_id != resource_id {
                index += 1;
                continue;
            }
            let wait = self.native_ahb_release.waits.remove(index);
            native_ahb_trace(format_args!("wait-done r={} seq={}", resource_id, wait.sequence));
            let valid = self.virtio_gpu.is_native_ahb_resource(resource_id)
                && !self.native_ahb_paging.poisoned.contains(&resource_id)
                && self.native_ahb_release.sequences.get(&resource_id) == Some(&wait.sequence);
            if valid {
                if let Some(generation) = wait.refresh_generation {
                    completed.extend(self.present_native_ahb(wait.resource_id, wait.descriptor,
                                                           Some((wait.sequence, generation))));
                    continue;
                }
            }
            let response = if valid {
                GpuResponse::OkNativeAhbOperation {
                    resource_id,
                    sequence: wait.sequence,
                }
            } else {
                GpuResponse::ErrInvalidResourceId
            };
            completed.push(Self::finish_native_ahb_operation(wait.descriptor, response));
        }
        completed
    }

    /// A stopped activation may only retire existing readers. Cancel
    /// refresh reservations as unchanged and reject unadmitted ordinary
    /// PRESENTs before reset/suspend drains the real fences.
    pub(super) fn cancel_native_ahb_refreshes(&mut self) -> Vec<ReturnDescriptor> {
        let mut completed: Vec<_> = self.native_ahb_release.presents.drain(..).map(|pending|
            Self::finish_native_ahb_operation(pending.descriptor, GpuResponse::ErrUnspec)).collect();
        let mut index = 0;
        while index < self.native_ahb_release.waits.len() {
            if self.native_ahb_release.waits[index].refresh_generation.is_none() {
                index += 1;
                continue;
            }
            let wait = self.native_ahb_release.waits.remove(index);
            completed.push(Self::finish_native_ahb_operation(wait.descriptor,
                GpuResponse::OkNativeAhbOperation { resource_id: wait.resource_id, sequence: wait.sequence }));
        }
        completed
    }

    pub(super) fn process_native_ahb_operation(
        &mut self,
        info: virtio_gpu_native_ahb_operation,
        desc_chain: DescriptorChain,
        source: GpuQueue,
    ) -> Option<ReturnDescriptor> {
        self.process_native_ahb_operation_bound(info, desc_chain, source, false)
    }

    pub(super) fn process_native_ahb_operation_bound(
        &mut self, info: virtio_gpu_native_ahb_operation,
        desc_chain: DescriptorChain, source: GpuQueue, bound_geometry: bool,
    ) -> Option<ReturnDescriptor> {
        let resource_id = info.resource_id.to_native();
        let sequence = info.sequence.to_native();
        let descriptor = ReturnDescriptor {
            desc_chain,
            len: 0,
            queue: source,
            ctx_id: 0,
        };
        // Reject short replies before any side effects. The private commands
        // do not participate in cumulative renderer or WDDM fence timelines.
        let valid = source == GpuQueue::Control
            && descriptor.desc_chain.reader.available_bytes() == 0
            && descriptor.desc_chain.writer.available_bytes()
                >= std::mem::size_of::<virtio_gpu_native_ahb_operation>()
            && info.hdr.flags.to_native() == 0
            && info.hdr.fence_id.to_native() == 0
            && info.hdr.ctx_id.to_native() == 0
            && info.hdr.ring_idx == 0
            && info.hdr.padding == [0; 3]
            && info.reserved.to_native() == 0
            && (info.hdr.type_.to_native() == VIRTIO_GPU_CMD_WAIT_NATIVE_AHB_RELEASE
                || ((bound_geometry || self.virtio_gpu.native_resource_geometry(resource_id).is_none())
                    && self.virtio_gpu.native_resource_geometry_current(resource_id)));
        if !valid {
            return Some(Self::finish_native_ahb_operation(
                descriptor,
                GpuResponse::ErrInvalidParameter,
            ));
        }
        if !self.virtio_gpu.is_native_ahb_resource(resource_id)
            || self.native_ahb_paging.poisoned.contains(&resource_id) {
            return Some(Self::finish_native_ahb_operation(
                descriptor,
                GpuResponse::ErrInvalidResourceId,
            ));
        }
        if self.native_ahb_present_pending(resource_id) {
            return Some(Self::finish_native_ahb_operation(descriptor, GpuResponse::ErrInvalidParameter));
        }
        if info.hdr.type_.to_native() == VIRTIO_GPU_CMD_REFRESH_NATIVE_AHB {
            let expected = self.native_ahb_release.sequences.get(&resource_id).copied().unwrap_or(0);
            // An earlier direct sequence does not authorize replacing the
            // currently visible owned snapshot with newer CopyOnly pixels.
            // The paired Blt client must use REFRESH_ONLY for this primary.
            if sequence == 0 || sequence != expected || self.display_failed
                || self.virtio_gpu.native_blt_output_is_current(resource_id) {
                return Some(Self::finish_native_ahb_operation(descriptor, GpuResponse::ErrInvalidParameter));
            }
            let generation = self.virtio_gpu.display().borrow().native_refresh_generation();
            info!("NATIVE-AHB-REFRESH: event=request res={} previous={} generation={} old_reader={}",
                resource_id, sequence, generation, self.native_ahb_has_reader(resource_id));
            if generation == 0 || !self.virtio_gpu.is_scanout_resource(resource_id) {
                // Exact same acceptance sequence: no new transaction and,
                // critically, no statement that its existing reader released.
                return Some(Self::finish_native_ahb_operation(descriptor,
                    GpuResponse::OkNativeAhbOperation { resource_id, sequence }));
            }
            if self.native_ahb_has_reader(resource_id) {
                if self.native_ahb_release.waits.len() >= 256 ||
                    self.native_ahb_release.waits.iter().any(|wait| wait.resource_id == resource_id) {
                    return Some(Self::finish_native_ahb_operation(descriptor, GpuResponse::ErrInvalidParameter));
                }
                self.native_ahb_release.waits.push(NativeAhbWait {
                    resource_id, sequence, refresh_generation: Some(generation), descriptor,
                });
                return None;
            }
            return self.present_native_ahb(resource_id, descriptor, Some((sequence, generation)));
        }
        if info.hdr.type_.to_native() == VIRTIO_GPU_CMD_WAIT_NATIVE_AHB_RELEASE {
            native_ahb_trace(format_args!(
                "wait r={} seq={} held={}",
                resource_id,
                sequence,
                self.native_ahb_has_reader(resource_id) as u8
            ));
            let expected = self
                .native_ahb_release
                .sequences
                .get(&resource_id)
                .copied()
                .unwrap_or(0);
            if sequence != expected || (sequence == 0 && self.native_ahb_has_reader(resource_id)) {
                error!(
                    "NATIVE-AHB: rejecting release WAIT res {} sequence {} expected {} reader={}",
                    resource_id,
                    sequence,
                    expected,
                    self.native_ahb_has_reader(resource_id)
                );
                return Some(Self::finish_native_ahb_operation(
                    descriptor,
                    GpuResponse::ErrInvalidParameter,
                ));
            }
            if !self.native_ahb_has_reader(resource_id) {
                return Some(Self::finish_native_ahb_operation(
                    descriptor,
                    GpuResponse::OkNativeAhbOperation {
                        resource_id,
                        sequence,
                    },
                ));
            }
            if self.native_ahb_release.waits.len() >= 256 {
                return Some(Self::finish_native_ahb_operation(
                    descriptor,
                    GpuResponse::ErrOutOfMemory,
                ));
            }
            self.native_ahb_release.waits.push(NativeAhbWait {
                resource_id,
                sequence,
                refresh_generation: None,
                descriptor,
            });
            return None;
        }

        if sequence != 0 || self.native_ahb_has_reader(resource_id) {
            error!(
                "NATIVE-AHB: rejecting PRESENT res {} sequence {} reader={}",
                resource_id,
                sequence,
                self.native_ahb_has_reader(resource_id)
            );
            return Some(Self::finish_native_ahb_operation(
                descriptor,
                GpuResponse::ErrInvalidParameter,
            ));
        }
        self.present_native_ahb(resource_id, descriptor, None)
    }

    fn present_native_ahb(&mut self, resource_id: u32, descriptor: ReturnDescriptor,
                         refresh: Option<(u64, u64)>) -> Option<ReturnDescriptor> {
        // A deferred reattachment has no live decode scope. A nested begin
        // returns zero and leaves the real packet's TLS identity untouched.
        let _fifo_trace = gpu_display::NativeFifoTraceScope::begin(0, resource_id,
            refresh.map_or(0, |(sequence, _)| sequence), 0, descriptor.ctx_id, 0, 0,
            descriptor.desc_chain.index() as u32);
        if !self.virtio_gpu.native_resource_geometry_current(resource_id) {
            return Some(Self::finish_native_ahb_operation(descriptor, GpuResponse::ErrInvalidParameter));
        }
        if let Some((sequence, generation)) = refresh {
            if !self.virtio_gpu.is_scanout_resource(resource_id) ||
                self.virtio_gpu.display().borrow().native_refresh_generation() != generation {
                return Some(Self::finish_native_ahb_operation(descriptor,
                    GpuResponse::OkNativeAhbOperation { resource_id, sequence }));
            }
            self.virtio_gpu.display().borrow().set_native_refresh_generation(generation);
            self.virtio_gpu.prepare_native_ahb_refresh(resource_id);
            // One record per real reattachment refresh, never per normal frame.
            // This distinguishes a reserved replay from a later guest repaint.
            info!("NATIVE-AHB-REFRESH: event=submit res={} previous={} generation={} old_reader={}",
                resource_id, sequence, generation, self.native_ahb_has_reader(resource_id));
        }
        let Some(sequence) = self.native_ahb_release.last_sequence.checked_add(1) else {
            self.virtio_gpu.display().borrow().set_native_refresh_generation(0);
            return Some(Self::finish_native_ahb_operation(
                descriptor,
                GpuResponse::ErrOutOfMemory,
            ));
        };
        self.native_ahb_release.last_sequence = sequence;
        gpu_display::native_fifo_trace_operation(sequence);
        native_ahb_trace(format_args!("present r={} seq={}", resource_id, sequence));
        let present_started = std::time::Instant::now();
        let generation = self.virtio_gpu.display().borrow().consumer_generation();
        let result = self.virtio_gpu.flush_resource(resource_id);
        self.virtio_gpu.display().borrow().set_native_refresh_generation(0);
        let readers = self.virtio_gpu.take_pending_flip_fences();
        let has_reader = !readers.is_empty();
        if let Some((previous, generation)) = refresh {
            if !has_reader && self.virtio_gpu.display().borrow().native_refresh_generation() != generation {
                // Attach raced the actual bridge submit. Its generation guard
                // rejected the old request under the attachment mutex.
                return Some(Self::finish_native_ahb_operation(descriptor,
                    GpuResponse::OkNativeAhbOperation { resource_id, sequence: previous }));
            }
            if !has_reader {
                // Still the requested attached consumer, but no direct reader
                // was admitted. Do not advertise a successful static recovery.
                return Some(Self::finish_native_ahb_operation(descriptor, GpuResponse::ErrUnspec));
            }
        }
        if refresh.is_none() && !has_reader && result.is_ok()
            && self.virtio_gpu.is_scanout_resource(resource_id)
            && self.virtio_gpu.display().borrow().has_consumer()
            && self.pending_flip_fences.iter().any(|reader| reader.ticket.is_none() && reader.resource_id != resource_id)
        {
            if self.native_ahb_release.presents.len() >= 256 {
                return Some(Self::finish_native_ahb_operation(descriptor, GpuResponse::ErrOutOfMemory));
            }
            let now = std::time::Instant::now();
            self.native_ahb_release.presents.push(NativeAhbPendingPresent {
                resource_id, sequence, generation, attempts: 1,
                next_retry: now + NATIVE_AHB_PRESENT_RETRY_INTERVAL,
                deadline: present_started + NATIVE_AHB_PRESENT_RETRY_DEADLINE, descriptor,
            });
            info!("NATIVE-AHB-RETRY: event=reserved res={} sequence={} generation={}", resource_id, sequence, generation);
            return None;
        }
        Some(self.finish_native_ahb_present(resource_id, sequence, descriptor, result, readers, refresh))
    }

    fn finish_native_ahb_present(&mut self, resource_id: u32, sequence: u64, descriptor: ReturnDescriptor,
                                result: VirtioGpuResult, readers: Vec<virtio_gpu::PendingDisplayFence>,
                                refresh: Option<(u64, u64)>) -> ReturnDescriptor {
        let has_reader = !readers.is_empty();
        // Keep all readers even when the presenter reports a later error.
        // The caller must recover; an error response never grants reuse.
        for reader in readers {
            self.pending_flip_fences.push(PendingFlipFence {
                resource_id: reader.resource_id,
                fence_id: sequence,
                ticket: None,
                fence: reader.fence,
                deadline: None,
                registered: false,
            });
        }
        if has_reader {
            self.native_ahb_release
                .sequences
                .insert(resource_id, sequence);
        }
        // A current front buffer can stay on screen indefinitely. Start the
        // watchdog only for native readers whose scanout was replaced/unbound.
        for reader in &mut self.pending_flip_fences {
            if reader.ticket.is_none()
                && reader.resource_id != resource_id
                && reader.deadline.is_none()
                && !self.virtio_gpu.is_scanout_resource(reader.resource_id)
            {
                reader.deadline = Some(std::time::Instant::now() + FLIP_FENCE_TIMEOUT);
            }
        }
        let response = match result {
            Ok(_) if has_reader => {
                self.native_ahb_release.accepted = self.native_ahb_release.accepted.saturating_add(1);
                self.trace_native_ahb_lifecycle("accepted", resource_id, sequence, self.native_ahb_release.accepted);
                GpuResponse::OkNativeAhbOperation { resource_id, sequence }
            }
            Ok(_) => {
                // Nothing took the buffer: no Android surface is attached (the
                // native display is closed) or the frame was dropped. Nothing
                // reads the AHB, so the present is complete and already
                // released; failing it would poison the guest's primary.
                self.native_ahb_release
                    .sequences
                    .insert(resource_id, sequence);
                self.native_ahb_release.accepted = self.native_ahb_release.accepted.saturating_add(1);
                self.trace_native_ahb_lifecycle("undisplayed", resource_id, sequence, self.native_ahb_release.accepted);
                GpuResponse::OkNativeAhbOperation { resource_id, sequence }
            }
            Err(error) => error,
        };
        if refresh.is_none() {
            self.virtio_gpu.trace_native_ahb_generation_present(resource_id, sequence, has_reader,
                matches!(&response, GpuResponse::OkNativeAhbOperation { .. }));
        }
        if let Some((previous, generation)) = refresh {
            info!("NATIVE-AHB-REFRESH: event=complete res={} previous={} sequence={} generation={} new_reader={} accepted={}",
                resource_id, previous, sequence, generation, has_reader,
                matches!(&response, GpuResponse::OkNativeAhbOperation { .. }));
        }
        Self::finish_native_ahb_operation(descriptor, response)
    }
}
