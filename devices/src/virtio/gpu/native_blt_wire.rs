// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license.
//! Paired with viogpu_native_blt_wire.h. All fields are explicitly little endian.
use super::protocol::virtio_gpu_ctrl_hdr;
use data_model::{Le32, Le64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
pub const VIRTIO_GPU_F_NATIVE_BLT: u32 = 12;
pub const VIRTIO_GPU_NATIVE_BLT_ENABLED: bool = false;
pub const VIRTIO_GPU_CMD_QUERY_NATIVE_BLT: u32 = 0xd224;
pub const VIRTIO_GPU_RESP_OK_NATIVE_BLT_IDENTITY: u32 = 0xd225;
pub const VIRTIO_GPU_CMD_NATIVE_BLT: u32 = 0xd226;
pub const VIRTIO_GPU_RESP_OK_NATIVE_BLT: u32 = 0xd227;
pub const VIRTIO_GPU_NATIVE_BLT_COPY_ONLY: u32 = 1;
pub const VIRTIO_GPU_NATIVE_BLT_PUBLISH: u32 = 2;
/// Initial mode-bind snapshot: source is zero, regions empty; preserve the
/// primary itself and publish an exclusively owned copy of its current pixels.
pub const VIRTIO_GPU_NATIVE_BLT_PUBLISH_ONLY: u32 = 4;
/// Refresh the last accepted owned snapshot without reading the primary.
pub const VIRTIO_GPU_NATIVE_BLT_REFRESH_ONLY: u32 = 8;
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct VirtioGpuNativeBltEndpoint {
    pub resource_id: Le32,
    pub width: Le32,
    pub height: Le32,
    pub stride: Le32,
    pub fourcc: Le32,
    pub reserved: Le32,
    pub incarnation: Le64,
    pub size: Le64,
    pub buffer_id: Le64,
}
impl VirtioGpuNativeBltEndpoint {
    pub fn layout(self) -> gpu_display::NativeBltCpuLayout {
        gpu_display::NativeBltCpuLayout {
            width: self.width.into(),
            height: self.height.into(),
            stride: self.stride.into(),
            fourcc: self.fourcc.into(),
            size: self.size.into(),
        }
    }
}
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct VirtioGpuNativeBltRegion {
    pub source_x: Le32,
    pub source_y: Le32,
    pub destination_x: Le32,
    pub destination_y: Le32,
    pub width: Le32,
    pub height: Le32,
}
impl From<VirtioGpuNativeBltRegion> for gpu_display::NativeBltRegion {
    fn from(r: VirtioGpuNativeBltRegion) -> Self {
        Self {
            source_x: r.source_x.into(),
            source_y: r.source_y.into(),
            destination_x: r.destination_x.into(),
            destination_y: r.destination_y.into(),
            width: r.width.into(),
            height: r.height.into(),
        }
    }
}
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct virtio_gpu_query_native_blt {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub resource_id: Le32,
    pub reserved: Le32,
    pub cookie: Le64,
}
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct virtio_gpu_native_blt_identity {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub resource_id: Le32,
    pub kind: Le32,
    pub cookie: Le64,
    pub incarnation: Le64,
    pub reset_generation: Le64,
    pub buffer_id: Le64,
}
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct virtio_gpu_native_blt {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub cookie: Le64,
    pub reset_generation: Le64,
    pub flags: Le32,
    pub region_count: Le32,
    pub source: VirtioGpuNativeBltEndpoint,
    pub destination: VirtioGpuNativeBltEndpoint,
}
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct virtio_gpu_native_blt_response {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub cookie: Le64,
    pub reset_generation: Le64,
    pub source_incarnation: Le64,
    pub destination_incarnation: Le64,
    pub copied_bytes: Le64,
    pub uploaded_bytes: Le64,
    pub readback_bytes: Le64,
}
const _: () = assert!(std::mem::size_of::<VirtioGpuNativeBltEndpoint>() == 48);
const _: () = assert!(std::mem::size_of::<VirtioGpuNativeBltRegion>() == 24);
const _: () = assert!(std::mem::size_of::<virtio_gpu_query_native_blt>() == 40);
const _: () = assert!(std::mem::size_of::<virtio_gpu_native_blt_identity>() == 64);
const _: () = assert!(std::mem::size_of::<virtio_gpu_native_blt>() == 144);
const _: () = assert!(std::mem::size_of::<virtio_gpu_native_blt_response>() == 80);
