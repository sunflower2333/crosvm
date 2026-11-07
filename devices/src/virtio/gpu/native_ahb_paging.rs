// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license.

//! Explicit VidMm preservation, never a presentation fallback. The guest must
//! retire GPU producers, acquire exclusive per-allocation ownership and observe
//! Android release before using this protocol. No Guest CPU BAR map is implied.

use std::collections::BTreeSet;
use std::io::Read;

use super::*;

#[derive(Default)]
pub(super) struct NativeAhbPagingState {
    pub(super) poisoned: BTreeSet<u32>,
    pub(super) completed_read_bytes: u64,
    pub(super) completed_write_bytes: u64,
    pub(super) completed_fill_bytes: u64,
    pub(super) operations: u64,
    pub(super) failures: u64,
    pub(super) rejections: u64,
}

impl Frontend {
    pub(super) fn process_native_ahb_paging(
        &mut self,
        info: virtio_gpu_native_ahb_paging,
        mut desc_chain: DescriptorChain,
        source: GpuQueue,
    ) -> ReturnDescriptor {
        let started = std::time::Instant::now();
        let id = info.resource_id.to_native();
        let operation = info.operation.to_native();
        let offset = info.offset.to_native();
        let length = info.length.to_native();
        let pattern = info.pattern.to_native();
        let write_length = if operation == VIRTIO_GPU_NATIVE_AHB_PAGE_WRITE {
            length as usize
        } else {
            0
        };
        let read_length = if operation == VIRTIO_GPU_NATIVE_AHB_PAGE_READ {
            length as usize
        } else {
            0
        };
        let response = (|| {
            // Validate both descriptor directions and all reserved fields
            // before allocating a payload or changing even a single byte.
            if source != GpuQueue::Control || info.hdr.flags.to_native() != 0
                || info.hdr.ctx_id.to_native() != 0 || info.hdr.fence_id.to_native() != 0
                || info.hdr.ring_idx != 0 || info.hdr.padding != [0; 3]
                || length == 0 || length > VIRTIO_GPU_NATIVE_AHB_PAGE_MAX_BYTES
                || !matches!(operation, VIRTIO_GPU_NATIVE_AHB_PAGE_READ | VIRTIO_GPU_NATIVE_AHB_PAGE_WRITE | VIRTIO_GPU_NATIVE_AHB_PAGE_FILL)
                || (operation != VIRTIO_GPU_NATIVE_AHB_PAGE_FILL && pattern != 0)
                || (operation == VIRTIO_GPU_NATIVE_AHB_PAGE_FILL && (offset % 4 != 0 || length % 4 != 0))
                || desc_chain.reader.available_bytes() != write_length
                || desc_chain.writer.available_bytes() < std::mem::size_of_val(&info) + read_length
            {
                return Err(GpuResponse::ErrInvalidParameter);
            }
            if self.display_failed || self.native_ahb_paging.poisoned.contains(&id) {
                return Err(GpuResponse::ErrUnspec);
            }
            if self.native_ahb_has_reader(id) || self.native_ahb_present_pending(id) {
                return Err(GpuResponse::ErrInvalidParameter);
            }
            let allocation = self.virtio_gpu.native_ahb_allocation(id)
                .ok_or(GpuResponse::ErrInvalidResourceId)?;
            let size = allocation.description().allocation_size;
            if offset.checked_add(u64::from(length)).is_none_or(|end| end > size) {
                return Err(GpuResponse::ErrInvalidParameter);
            }
            let mut data = vec![0u8; read_length.max(write_length)];
            if write_length != 0 && desc_chain.reader.read_exact(&mut data).is_err() {
                return Err(GpuResponse::ErrInvalidParameter);
            }
            self.native_ahb_paging.operations = self.native_ahb_paging.operations.saturating_add(1);
            if self.native_ahb_paging.operations <= 4 {
                info!("NATIVE-AHB-PAGING: begin res={} operation={} offset={} length={} count={}",
                    id, operation, offset, length, self.native_ahb_paging.operations);
            }
            let result = match operation {
                VIRTIO_GPU_NATIVE_AHB_PAGE_READ => allocation.read_bytes(offset, &mut data),
                VIRTIO_GPU_NATIVE_AHB_PAGE_WRITE => allocation.write_bytes(offset, &data),
                VIRTIO_GPU_NATIVE_AHB_PAGE_FILL => allocation.fill_bytes(offset, u64::from(length), pattern),
                _ => unreachable!(),
            };
            if let Err(error) = result {
                // A failed DMA sync END may follow actual writes. Never allow
                // a failed preservation operation to grant rendering/reuse.
                self.native_ahb_paging.poisoned.insert(id);
                self.native_ahb_paging.failures = self.native_ahb_paging.failures.saturating_add(1);
                error!("NATIVE-AHB-PAGING: failed res={} operation={} offset={} length={} error={:#}",
                    id, operation, offset, length, error);
                return Err(GpuResponse::ErrUnspec);
            }
            let counter = match operation {
                VIRTIO_GPU_NATIVE_AHB_PAGE_READ => &mut self.native_ahb_paging.completed_read_bytes,
                VIRTIO_GPU_NATIVE_AHB_PAGE_WRITE => &mut self.native_ahb_paging.completed_write_bytes,
                _ => &mut self.native_ahb_paging.completed_fill_bytes,
            };
            *counter = counter.saturating_add(u64::from(length));
            let count = self.native_ahb_paging.operations;
            if count <= 4 || count.is_power_of_two() {
                info!("NATIVE-AHB-PAGING: res={} buffer_id={:?} operations={} completed_read_bytes={} completed_write_bytes={} completed_fill_bytes={} failures={} operation={} offset={} length={} elapsed_us={}",
                    id, self.virtio_gpu.native_ahb_buffer_id(id), count,
                    self.native_ahb_paging.completed_read_bytes,
                    self.native_ahb_paging.completed_write_bytes,
                    self.native_ahb_paging.completed_fill_bytes, self.native_ahb_paging.failures,
                    operation, offset, length, started.elapsed().as_micros());
            }
            data.truncate(read_length);
            Ok(GpuResponse::OkNativeAhbPaging { info, data })
        })().unwrap_or_else(|error| {
            // Rejections never reach the per-operation counters above; name
            // them so a stalled VidMm paging fence is attributable.
            self.native_ahb_paging.rejections = self.native_ahb_paging.rejections.saturating_add(1);
            if self.native_ahb_paging.rejections <= 16 || self.native_ahb_paging.rejections.is_power_of_two() {
                error!("NATIVE-AHB-PAGING: rejected res={} operation={} offset={} length={} reader_bytes={} \
                        has_reader={} poisoned={} error={}",
                    id, operation, offset, length, desc_chain.reader.available_bytes(),
                    self.native_ahb_has_reader(id), self.native_ahb_paging.poisoned.contains(&id), error);
            }
            error
        });
        let len = response
            .encode(0, 0, 0, 0, &mut desc_chain.writer)
            .unwrap_or(0);
        ReturnDescriptor {
            desc_chain,
            len,
            queue: source,
            ctx_id: 0,
        }
    }
}
