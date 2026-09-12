// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license.

use base::SafeDescriptor;

/// Host-endian native ABI only; the virtio layer serializes fields explicitly.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SharedAllocationDescription {
    pub version: u32,
    pub size: u32,
    pub buffer_id: u64,
    pub allocation_size: u64,
    pub modifier: u64,
    pub plane_offset: u64,
    pub plane_stride: u32,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub plane_count: u32,
    pub layout_flags: u32,
}

const _: () = assert!(std::mem::size_of::<SharedAllocationDescription>() == 64);

impl SharedAllocationDescription {
    pub fn valid(&self, width: u32, height: u32) -> bool {
        if self.version != 1 || self.size != 64 || self.buffer_id == 0
            || width == 0 || height == 0 || width > 8192 || height > 8192
            || self.width != width || self.height != height || self.modifier != 0
            || self.fourcc != u32::from_le_bytes(*b"AB24") || self.plane_count != 1
            || self.layout_flags != 1 || self.plane_stride % 4 != 0
            || u64::from(self.plane_stride) < u64::from(width)*4
            || self.allocation_size == 0 || self.allocation_size > 256*1024*1024 {
            return false;
        }
        u64::from(height-1).checked_mul(u64::from(self.plane_stride))
            .and_then(|bytes| bytes.checked_add(u64::from(width)*4))
            .and_then(|bytes| bytes.checked_add(self.plane_offset))
            .is_some_and(|end| end <= self.allocation_size)
    }
}

/// An authentic native allocation and its already proven data descriptor.
/// Dropping this object requires all renderer, mapper and consumer owners to
/// have retired. No API accepts a guest-provided native pointer or fd number.
pub trait HostSharedAllocation {
    fn description(&self) -> SharedAllocationDescription;
    fn duplicate_descriptor(&self) -> anyhow::Result<SafeDescriptor>;
}
