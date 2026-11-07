// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license.
//! Serialized GPU copy and owned primary publication. Admission is not completion.
use super::*;
use std::collections::BTreeSet;
use zerocopy::IntoBytes;

pub(super) const BLT_REQUIRED_FEATURES: u64 = (1 << VIRTIO_GPU_F_NATIVE_BLT)
    | (1 << VIRTIO_GPU_F_NATIVE_AHB_V2)
    | (1 << VIRTIO_GPU_F_NATIVE_AHB_RELEASE)
    | (1 << VIRTIO_GPU_F_NATIVE_AHB_PAGING)
    | (1 << VIRTIO_GPU_F_VIRGL)
    | (1 << VIRTIO_GPU_F_RESOURCE_BLOB)
    | (1 << VIRTIO_GPU_F_CONTEXT_INIT)
    | (1 << VIRTIO_GPU_F_CREATE_GUEST_HANDLE);

// Only this backend's generated capabilities can authorize the protocol.
// Caller-supplied base_features cannot manufacture its prerequisites.
pub(super) fn blt_feature_mask(
    combined: u64, generated: u64, android_display: bool, virgl_renderer: bool,
) -> u64 {
    let bit = 1 << VIRTIO_GPU_F_NATIVE_BLT;
    let prerequisites = BLT_REQUIRED_FEATURES & !bit;
    let masked = combined & !bit;
    if android_display && virgl_renderer && generated & prerequisites == prerequisites {
        masked | bit
    } else {
        masked
    }
}

pub(super) fn blt_negotiated(offered: u64, acknowledged: u64) -> bool {
    offered & acknowledged & BLT_REQUIRED_FEATURES == BLT_REQUIRED_FEATURES
}

#[derive(Default)]
pub(super) struct NativeBltState {
    pub(super) poisoned: BTreeSet<u32>,
    operations: u64,
    completed_bytes: u64,
    failures: u64,
}
fn plain_header(h: virtio_gpu_ctrl_hdr) -> bool {
    h.flags.to_native() == 0
        && h.fence_id.to_native() == 0
        && h.ctx_id.to_native() == 0
        && h.ring_idx == 0
        && h.padding == [0; 3]
}
impl Frontend {
    fn complete_native_blt_cache(&mut self, token: u32) -> std::result::Result<(), GpuResponse> {
        // This upload belongs to the classic GL renderer, not the guest's
        // native-context timeline. The token was reserved before mutation.
        self.virtio_gpu.create_fence(RutabagaFence {
            flags: VIRTIO_GPU_FLAG_FENCE,
            fence_id: u64::from(token),
            ctx_id: 0,
            ring_idx: 0,
        })?;
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            self.virtio_gpu.event_poll();
            if std::time::Instant::now() >= deadline {
                return Err(GpuResponse::ErrUnspec);
            }
            {
                let state = self.fence_state.lock();
                state.check_renderer().map_err(|_| GpuResponse::ErrUnspec)?;
                if state
                    .queue
                    .completed_renderer
                    .get(&VirtioGpuRing::Global)
                    .is_some_and(|completed| *completed >= u64::from(token))
                {
                    return Ok(());
                }
            }
            if std::time::Instant::now() >= deadline {
                return Err(GpuResponse::ErrUnspec);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    pub(super) fn process_native_blt_query(
        &mut self,
        info: virtio_gpu_query_native_blt,
        mut chain: DescriptorChain,
        queue: GpuQueue,
    ) -> ReturnDescriptor {
        let response = (|| {
            if !self.native_blt_negotiated
                || queue != GpuQueue::Control
                || !plain_header(info.hdr)
                || info.reserved.to_native() != 0
                || info.cookie.to_native() == 0
                || chain.reader.available_bytes() != 0
                || chain.writer.available_bytes() < 64
            {
                return Err(GpuResponse::ErrInvalidParameter);
            }
            let id = info.resource_id.to_native();
            if self.display_failed || self.native_blt.poisoned.contains(&id) {
                return Err(GpuResponse::ErrUnspec);
            }
            let (incarnation, buffer_id, kind) = self
                .virtio_gpu
                .native_blt_identity(id)
                .ok_or(GpuResponse::ErrInvalidResourceId)?;
            Ok(GpuResponse::OkNativeBltIdentity(
                virtio_gpu_native_blt_identity {
                    resource_id: info.resource_id,
                    kind: kind.into(),
                    cookie: info.cookie,
                    incarnation: incarnation.into(),
                    reset_generation: self.virtio_gpu.native_blt_reset_generation().into(),
                    buffer_id: buffer_id.into(),
                    ..Default::default()
                },
            ))
        })()
        .unwrap_or_else(|e| e);
        let len = response.encode(0, 0, 0, 0, &mut chain.writer).unwrap_or(0);
        ReturnDescriptor {
            desc_chain: chain,
            len,
            queue,
            ctx_id: 0,
        }
    }
    pub(super) fn process_native_blt(
        &mut self,
        mem: &GuestMemory,
        info: virtio_gpu_native_blt,
        mut chain: DescriptorChain,
        queue: GpuQueue,
    ) -> ReturnDescriptor {
        let response = (|| {
            let count = info.region_count.to_native() as usize;
            let flags = info.flags.to_native();
            let copy =
                flags == VIRTIO_GPU_NATIVE_BLT_COPY_ONLY || flags == VIRTIO_GPU_NATIVE_BLT_PUBLISH;
            let publish = flags == VIRTIO_GPU_NATIVE_BLT_PUBLISH
                || flags == VIRTIO_GPU_NATIVE_BLT_PUBLISH_ONLY
                || flags == VIRTIO_GPU_NATIVE_BLT_REFRESH_ONLY;
            let refresh = flags == VIRTIO_GPU_NATIVE_BLT_REFRESH_ONLY;
            // Complete admission precedes destination mutation or publication.
            if !self.native_blt_negotiated
                || queue != GpuQueue::Control
                || !plain_header(info.hdr)
                || info.cookie.to_native() == 0
                || (!copy && !publish)
                || (!copy && info.source.as_bytes().iter().any(|b| *b != 0))
                || info.reset_generation.to_native()
                    != self.virtio_gpu.native_blt_reset_generation()
                || (copy && count == 0)
                || (!copy && count != 0)
                || count > 256
                || chain.reader.available_bytes() != count * 24
                || chain.writer.available_bytes() < 80
            {
                return Err(GpuResponse::ErrInvalidParameter);
            }
            let s = info.source.resource_id.to_native();
            let d = info.destination.resource_id.to_native();
            if d == 0 || (copy && (s == 0 || s == d)) {
                return Err(GpuResponse::ErrInvalidParameter);
            }
            if self.display_failed
                || [s, d].iter().any(|id| {
                    self.native_blt.poisoned.contains(id)
                        || self.native_ahb_paging.poisoned.contains(id)
                })
            {
                return Err(GpuResponse::ErrUnspec);
            }
            if !refresh
                && [s, d].iter().any(|id| {
                    self.native_ahb_has_reader(*id) || self.native_ahb_operation_pending(*id)
                })
            {
                return Err(GpuResponse::ErrInvalidParameter);
            }
            let mut regions = Vec::with_capacity(count);
            for _ in 0..count {
                let region: VirtioGpuNativeBltRegion = chain
                    .reader
                    .read_obj()
                    .map_err(|_| GpuResponse::ErrInvalidParameter)?;
                regions.push(gpu_display::NativeBltRegion::from(region));
            }
            // Resource identities and the entire clip list are checked before
            // calling the synchronous backend. References remain borrowed until
            // the real GPU fence completes, so no UNREF/reset can interleave.
            let expected = if copy {
                self.virtio_gpu
                    .validate_native_blt(mem, info.source, info.destination, &regions)?
            } else {
                0
            };
            let output = if publish {
                match self
                    .virtio_gpu
                    .prepare_native_blt_output(mem, info.destination, refresh)
                {
                    Ok(ticket) => Some(ticket),
                    Err(e) => {
                        if self.virtio_gpu.native_blt_outputs_poisoned() {
                            self.display_failed = true;
                        }
                        return Err(e);
                    }
                }
            } else {
                None
            };
            let cache_token = if copy && info.destination.buffer_id.to_native() == 0 {
                if !self.maps_global_fences || !self.fence_state.lock().uses_global_tokens {
                    return Err(GpuResponse::ErrInvalidParameter);
                }
                Some(
                    self.fence_state
                        .lock()
                        .global_tokens
                        .reserve()
                        .ok_or(GpuResponse::ErrOutOfMemory)?,
                )
            } else {
                None
            };
            self.native_blt.operations = self.native_blt.operations.saturating_add(1);
            let result = if copy {
                self.virtio_gpu
                    .execute_native_blt(mem, info.source, info.destination, &regions)
            } else {
                Ok(virtio_gpu::NativeBltCompletion {
                    copied: 0,
                    uploaded: 0,
                    readback: 0,
                })
            }
            .and_then(|mut done| {
                if done.copied != expected {
                    return Err(GpuResponse::ErrUnspec);
                }
                if let Some(token) = cache_token {
                    self.complete_native_blt_cache(token)?;
                }
                if let Some(ticket) = output {
                    done.uploaded =
                        done.uploaded
                            .saturating_add(self.virtio_gpu.publish_native_blt_output(
                                mem,
                                info.destination,
                                ticket,
                                refresh,
                            )?);
                }
                Ok(done)
            });
            match result {
                Ok(done) if done.copied == expected => {
                    self.native_blt.completed_bytes =
                        self.native_blt.completed_bytes.saturating_add(done.copied);
                    Ok(GpuResponse::OkNativeBlt(virtio_gpu_native_blt_response {
                        cookie: info.cookie,
                        reset_generation: info.reset_generation,
                        source_incarnation: info.source.incarnation,
                        destination_incarnation: info.destination.incarnation,
                        copied_bytes: done.copied.into(),
                        uploaded_bytes: done.uploaded.into(),
                        readback_bytes: done.readback.into(),
                        ..Default::default()
                    }))
                }
                result => {
                    // Never allow a failed/short completion to become a normal
                    // present, paging access or unref/reuse of either identity.
                    // Existing native contexts may already hold imports of
                    // these buffers. Stop the whole activation's GPU intake,
                    // including the next descriptor in this same queue batch.
                    // The current error response still reaches the used ring.
                    self.display_failed = true;
                    self.native_blt
                        .poisoned
                        .extend([s, d].into_iter().filter(|id| *id != 0));
                    self.native_ahb_paging
                        .poisoned
                        .extend([s, d].into_iter().filter(|id| *id != 0));
                    self.native_blt.failures = self.native_blt.failures.saturating_add(1);
                    error!(
                        "NATIVE-BLT: quarantined src={} dst={} expected={} result={:?}",
                        s, d, expected, result
                    );
                    Err(GpuResponse::ErrUnspec)
                }
            }
        })()
        .unwrap_or_else(|e| e);
        let len = response.encode(0, 0, 0, 0, &mut chain.writer).unwrap_or(0);
        ReturnDescriptor {
            desc_chain: chain,
            len,
            queue,
            ctx_id: 0,
        }
    }
}
