// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license.
//! Bounded owned display snapshots. Android holds these allocations, never a
//! writable Blt primary. Every reuse requires all actual per-buffer releases.
use super::*;
use crate::virtio::gpu::flip_fence_signaled;
use crate::virtio::gpu::protocol::VIRTIO_GPU_NATIVE_AHB_BUDGET;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OutputTicket {
    pub slot: usize,
    pub surface: u32,
    pub geometry: ScanoutGeometry,
    pub generation: u64,
}
struct Output {
    allocation: Option<Box<dyn HostSharedAllocation>>,
    ticket: OutputTicket,
    import: u32,
    readers: Vec<FlipCompletionFence>,
    uncertain: bool,
}
impl Drop for Output {
    fn drop(&mut self) {
        // A failed submission or an unobserved release cannot grant gralloc
        // reuse, including when the worker is torn down. Quarantine the actual
        // holder for this process lifetime, rather than just remembering an ID.
        if self.uncertain || !self.readers.is_empty() {
            if let Some(allocation) = self.allocation.take() {
                std::mem::forget(allocation);
            }
        }
    }
}
#[derive(Default)]
pub(super) struct NativeBltOutputs {
    slots: Vec<Option<Output>>,
    current: Option<(u32, u64, usize)>,
    pub poisoned: bool,
    pub copied_bytes: u64,
}
impl NativeBltOutputs {
    pub fn has_current(&self) -> bool {
        self.current.is_some()
    }
    pub fn retire_resource(&mut self, resource: u32) {
        if self.current.is_some_and(|current| current.0 == resource) {
            self.current = None;
        }
    }
    pub fn has_state(&self) -> bool {
        self.poisoned || self.current.is_some() || self.slots.iter().any(Option::is_some)
    }
    pub fn is_current(&self, resource: u32, incarnation: u64) -> bool {
        self.current.is_some_and(|c| c.0 == resource && c.1 == incarnation)
    }
    pub fn pending(&mut self) -> Result<bool, GpuResponse> {
        self.poll()?;
        if self.poisoned || self.slots.iter().flatten().any(|slot| slot.uncertain) {
            return Err(ErrUnspec);
        }
        Ok(self.slots.iter().flatten().any(|slot| !slot.readers.is_empty()))
    }
    pub fn clear_released(&mut self, display: &mut GpuDisplay) -> Result<(), GpuResponse> {
        if self.pending()? {
            return Err(ErrUnspec);
        }
        // Imports are retired while their surfaces still exist. A detach
        // request alone never reaches this point: every typed release was read.
        for slot in self.slots.drain(..).flatten() {
            display.release_import(slot.import, slot.ticket.surface);
        }
        self.current = None;
        Ok(())
    }
    pub fn bytes(&self) -> u64 {
        self.slots.iter().flatten().fold(0u64, |n, s| {
            n.saturating_add(s.allocation.as_ref().unwrap().description().allocation_size)
        })
    }
    pub fn clear_current(&mut self) {
        self.current = None;
    }
    fn poll(&mut self) -> Result<(), GpuResponse> {
        for slot in self.slots.iter_mut().flatten() {
            let mut i = 0;
            while i < slot.readers.len() {
                match flip_fence_signaled(&slot.readers[i]) {
                    Ok(true) => {
                        slot.readers.remove(i);
                    }
                    Ok(false) => i += 1,
                    Err(e) => {
                        base::error!("owned Blt output release failed: {e:#}");
                        slot.uncertain = true;
                        self.poisoned = true;
                        return Err(ErrUnspec);
                    }
                }
            }
        }
        Ok(())
    }
    pub fn prepare(
        &mut self,
        display: &mut GpuDisplay,
        geometry: ScanoutGeometry,
        surface: u32,
        generation: u64,
        fourcc: u32,
        guest_bytes: u64,
    ) -> Result<OutputTicket, GpuResponse> {
        if self.poisoned {
            return Err(ErrUnspec);
        }
        self.poll()?;
        for (index, slot) in self.slots.iter().enumerate() {
            if self.current.is_some_and(|c| c.2 == index) {
                continue;
            }
            if let Some(slot) = slot {
                if !slot.uncertain
                    && slot.readers.is_empty()
                    && slot.ticket.geometry == geometry
                    && slot.ticket.surface == surface
                    && slot.ticket.generation == generation
                    && slot.allocation.as_ref().unwrap().description().fourcc == fourcc
                {
                    return Ok(OutputTicket {
                        slot: index,
                        surface,
                        geometry,
                        generation,
                    });
                }
            }
        }
        let index = self
            .slots
            .iter()
            .enumerate()
            .position(|(index, slot)| {
                !self.current.is_some_and(|c| c.2 == index) && slot.as_ref()
                    .is_none_or(|s| !s.uncertain && s.readers.is_empty())
            })
            .unwrap_or(self.slots.len());
        if index >= 3 {
            return Err(ErrOutOfMemory);
        }
        if index < self.slots.len() {
            if let Some(old) = self.slots[index].take() {
                display.release_import(old.import, old.ticket.surface);
            }
        }
        let minimum = u64::from(geometry.storage_width) * u64::from(geometry.storage_height) * 4;
        if guest_bytes
            .saturating_add(self.bytes())
            .saturating_add(minimum)
            > VIRTIO_GPU_NATIVE_AHB_BUDGET
        {
            return Err(ErrOutOfMemory);
        }
        let allocation = display
            .allocate_host_shared_allocation(
                geometry.storage_width,
                geometry.storage_height,
                fourcc,
            )
            .map_err(|_| ErrOutOfMemory)?;
        let desc = allocation.description();
        if !desc.valid(geometry.storage_width, geometry.storage_height) || desc.fourcc != fourcc {
            return Err(ErrInvalidParameter);
        }
        if guest_bytes
            .saturating_add(self.bytes())
            .saturating_add(desc.allocation_size)
            > VIRTIO_GPU_NATIVE_AHB_BUDGET
        {
            return Err(ErrOutOfMemory);
        }
        let import = display
            .import_resource(
                surface,
                DisplayExternalResourceImport::SharedAhb {
                    allocation: allocation.as_ref(),
                },
            )
            .map_err(|_| ErrUnspec)?;
        let ticket = OutputTicket {
            slot: index,
            surface,
            geometry,
            generation,
        };
        let slot = Output {
            allocation: Some(allocation),
            ticket,
            import,
            readers: Vec::new(),
            uncertain: false,
        };
        if index == self.slots.len() {
            self.slots.push(Some(slot));
        } else {
            self.slots[index] = Some(slot);
        }
        Ok(ticket)
    }
    pub fn begin_copy(
        &mut self,
        ticket: OutputTicket,
    ) -> Result<&dyn HostSharedAllocation, GpuResponse> {
        let slot = self
            .slots
            .get_mut(ticket.slot)
            .and_then(Option::as_mut)
            .ok_or(ErrInvalidParameter)?;
        if self.poisoned || slot.uncertain || slot.ticket != ticket || !slot.readers.is_empty()
            || self.current.is_some_and(|c| c.2 == ticket.slot) {
            return Err(ErrInvalidParameter);
        }
        slot.uncertain = true;
        Ok(slot.allocation.as_deref().unwrap())
    }
    pub fn copy_completed(&mut self, ticket: OutputTicket, bytes: u64) {
        self.slots[ticket.slot].as_mut().unwrap().uncertain = false;
        self.copied_bytes = self.copied_bytes.saturating_add(bytes);
    }
    pub fn current_ticket(
        &mut self,
        display: &GpuDisplay,
        resource: u32,
        incarnation: u64,
        surface: u32,
        geometry: ScanoutGeometry,
        generation: u64,
    ) -> Result<OutputTicket, GpuResponse> {
        self.poll()?;
        let (_, _, index) = self
            .current
            .filter(|c| c.0 == resource && c.1 == incarnation)
            .ok_or(ErrInvalidParameter)?;
        let slot = self.slots[index].as_ref().ok_or(ErrInvalidParameter)?;
        if self.poisoned || slot.uncertain || slot.ticket.geometry != geometry {
            return Err(ErrInvalidParameter);
        }
        let already_presented = slot.ticket.surface == surface
            && slot.ticket.generation == generation
            && display.native_refresh_generation() == 0;
        if !already_presented && !slot.readers.is_empty() {
            // Android rejects the same AHB identity across consumer generations
            // until its actual release. Refuse before any flip or mutation.
            return Err(ErrOutOfMemory);
        }
        Ok(OutputTicket {
            slot: index,
            surface,
            geometry,
            generation,
        })
    }
    pub fn publish(
        &mut self,
        display: &mut GpuDisplay,
        ticket: OutputTicket,
        resource: u32,
        incarnation: u64,
    ) -> Result<(), GpuResponse> {
        let slot = self
            .slots
            .get_mut(ticket.slot)
            .and_then(Option::as_mut)
            .ok_or(ErrInvalidParameter)?;
        if self.poisoned || slot.uncertain || slot.ticket.geometry != ticket.geometry {
            return Err(ErrInvalidParameter);
        }
        if self.current == Some((resource, incarnation, ticket.slot))
            && slot.ticket == ticket
            && display.has_consumer()
            && display.consumer_generation() == ticket.generation
            && display.native_refresh_generation() == 0 {
            return Ok(());
        }
        if !slot.readers.is_empty() {
            return Err(ErrOutOfMemory);
        }
        if slot.ticket.surface != ticket.surface || slot.ticket.generation != ticket.generation {
            // Re-importing the immutable snapshot does not acquire its Vulkan
            // ownership and does not replace it with newer CopyOnly pixels.
            let import = display
                .import_resource(
                    ticket.surface,
                    DisplayExternalResourceImport::SharedAhb {
                        allocation: slot.allocation.as_deref().unwrap(),
                    },
                )
                .map_err(|_| ErrUnspec)?;
            display.release_import(slot.import, slot.ticket.surface);
            slot.import = import;
            slot.ticket = ticket;
        }
        slot.uncertain = true;
        display.set_native_refresh_generation(ticket.generation);
        let result = if ticket.geometry.mode_generation == 0 {
            // This is the exact ordinary identity presenter, with the same
            // consumer-generation check and typed per-buffer release contract.
            display.flip_to(ticket.surface, slot.import, None, None, None)
        } else {
            display.flip_to_geometry(ticket.surface, slot.import, ticket.geometry)
        };
        display.set_native_refresh_generation(0);
        // Even a failed flip can have started an Android reader. Retain it.
        let fence = display.take_flip_completion_fence(ticket.surface);
        let has_fence = fence.is_some();
        if let Some(fence) = fence {
            slot.readers.push(fence);
        }
        if result.is_err() || !has_fence || display.consumer_generation() != ticket.generation {
            self.poisoned = true;
            return Err(ErrUnspec);
        }
        slot.uncertain = false;
        self.current = Some((resource, incarnation, ticket.slot));
        Ok(())
    }
}
