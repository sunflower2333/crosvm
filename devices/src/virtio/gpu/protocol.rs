// Copyright 2019 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![allow(dead_code)]
#![allow(non_camel_case_types)]

pub use super::native_blt_wire::*;

use std::cmp::min;
use std::convert::From;
use std::fmt;
use std::fmt::Display;
use std::io;
use std::io::Write;
use std::marker::PhantomData;
use std::mem::size_of;
use std::mem::size_of_val;
use std::str::from_utf8;

use base::Error as BaseError;
use base::TubeError;
use data_model::Le32;
use data_model::Le64;
use gpu_display::GpuDisplayError;
use gpu_display::SharedAllocationDescription;
use remain::sorted;
use rutabaga_gfx::RutabagaError;
use thiserror::Error;
use vm_memory::udmabuf::UdmabufError;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

pub use super::super::device_constants::gpu::virtio_gpu_config;
pub use super::super::device_constants::gpu::VIRTIO_GPU_F_CONTEXT_INIT;
pub use super::super::device_constants::gpu::VIRTIO_GPU_F_CREATE_GUEST_HANDLE;
pub use super::super::device_constants::gpu::VIRTIO_GPU_F_EDID;
pub use super::super::device_constants::gpu::VIRTIO_GPU_F_FENCE_PASSING;
pub use super::super::device_constants::gpu::VIRTIO_GPU_F_NATIVE_AHB_V2;
pub use super::super::device_constants::gpu::VIRTIO_GPU_F_NATIVE_AHB_RELEASE;
pub use super::super::device_constants::gpu::VIRTIO_GPU_F_NATIVE_AHB_PAGING;
pub use super::super::device_constants::gpu::VIRTIO_GPU_F_DISPLAY_TIMING;
pub use super::super::device_constants::gpu::VIRTIO_GPU_F_RESOURCE_BLOB;
pub use super::super::device_constants::gpu::VIRTIO_GPU_F_RESOURCE_UUID;
pub use super::super::device_constants::gpu::VIRTIO_GPU_F_VIRGL;
use super::edid::EdidBytes;
use super::Reader;
use super::Writer;

pub const VIRTIO_GPU_UNDEFINED: u32 = 0x0;

/* 2d commands */
pub const VIRTIO_GPU_CMD_GET_DISPLAY_INFO: u32 = 0x100;
pub const VIRTIO_GPU_CMD_RESOURCE_CREATE_2D: u32 = 0x101;
pub const VIRTIO_GPU_CMD_RESOURCE_UNREF: u32 = 0x102;
pub const VIRTIO_GPU_CMD_SET_SCANOUT: u32 = 0x103;
pub const VIRTIO_GPU_CMD_RESOURCE_FLUSH: u32 = 0x104;
pub const VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D: u32 = 0x105;
pub const VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING: u32 = 0x106;
pub const VIRTIO_GPU_CMD_RESOURCE_DETACH_BACKING: u32 = 0x107;
pub const VIRTIO_GPU_CMD_GET_CAPSET_INFO: u32 = 0x108;
pub const VIRTIO_GPU_CMD_GET_CAPSET: u32 = 0x109;
pub const VIRTIO_GPU_CMD_GET_EDID: u32 = 0x10a;
pub const VIRTIO_GPU_CMD_RESOURCE_ASSIGN_UUID: u32 = 0x10b;
pub const VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB: u32 = 0x10c;
pub const VIRTIO_GPU_CMD_SET_SCANOUT_BLOB: u32 = 0x10d;

/// Private explicit-layout allocation command, gated by `VIRTIO_GPU_F_NATIVE_AHB_V2`.
pub const VIRTIO_GPU_CMD_RESOURCE_CREATE_NATIVE_AHB: u32 = 0xd211;
pub const VIRTIO_GPU_CMD_PRESENT_NATIVE_AHB: u32 = 0xd213;
pub const VIRTIO_GPU_CMD_WAIT_NATIVE_AHB_RELEASE: u32 = 0xd214;
pub const VIRTIO_GPU_RESP_OK_NATIVE_AHB_OPERATION: u32 = 0xd215;
pub const VIRTIO_GPU_CMD_PAGE_NATIVE_AHB: u32 = 0xd216;
/// Guest holds a PresentPending reservation until this conditional re-present responds.
pub const VIRTIO_GPU_CMD_REFRESH_NATIVE_AHB: u32 = 0xd218;
pub const VIRTIO_GPU_RESP_OK_NATIVE_AHB_PAGING: u32 = 0xd217;
pub const VIRTIO_GPU_CMD_GET_DISPLAY_TIMING: u32 = 0xd219;
pub const VIRTIO_GPU_RESP_OK_DISPLAY_TIMING: u32 = 0xd21a;

#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_get_display_timing {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub scanout_id: Le32,
    pub version: Le32,
    pub cookie: Le64,
}

#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_display_timing {
    pub version: Le32, pub size: Le32, pub flags: Le32, pub reserved: Le32,
    pub attachment_epoch: Le64, pub mode_epoch: Le64, pub sequence: Le64,
    pub host_now_ns: Le64, pub present_ns: Le64, pub latch_ns: Le64,
    pub period_ns: Le64, pub uncertainty_ns: Le64,
}

impl From<gpu_display::DisplayTimingSample> for virtio_gpu_display_timing {
    fn from(s: gpu_display::DisplayTimingSample) -> Self {
        Self { version: s.version.into(), size: s.size.into(), flags: s.flags.into(),
            reserved: s.reserved.into(), attachment_epoch: s.attachment_epoch.into(),
            mode_epoch: s.mode_epoch.into(), sequence: s.sequence.into(),
            host_now_ns: s.host_now_ns.into(), present_ns: s.present_ns.into(),
            latch_ns: s.latch_ns.into(), period_ns: s.period_ns.into(),
            uncertainty_ns: s.uncertainty_ns.into() }
    }
}
pub const VIRTIO_GPU_NATIVE_AHB_PAGE_READ: u32 = 1;
pub const VIRTIO_GPU_NATIVE_AHB_PAGE_WRITE: u32 = 2;
pub const VIRTIO_GPU_NATIVE_AHB_PAGE_FILL: u32 = 3;
pub const VIRTIO_GPU_NATIVE_AHB_PAGE_MAX_BYTES: u32 = 64 * 1024;
/// Admission limit, not a claim of dedicated physical VRAM. Includes deferred
/// display holders and evicted surfaces whose identity is still retained.
/// DWM plus a 3040x1904 shared swap chain exceeded the former 256 MiB pool.
/// Keep paired with VIOGPU_WDDM_HOST_SURFACE_BUDGET in the guest miniport;
/// per-image bounds, live/deferred accounting and padded-size checks still apply.
pub const VIRTIO_GPU_NATIVE_AHB_BUDGET: u64 = 512 * 1024 * 1024;

// Paired contract, disabled by default until the guest producer and all
// consumers agree. An explicit host diagnostic can offer the transport; the
// guest must still negotiate every prerequisite and validate its own producer.
pub const VIRTIO_GPU_F_NATIVE_AHB_GEOMETRY: u32 = 11;
pub const VIRTIO_GPU_NATIVE_AHB_GEOMETRY_ENABLED: bool = false;
pub const VIRTIO_GPU_CMD_CONFIGURE_NATIVE_GEOMETRY: u32 = 0xd21b;
pub const VIRTIO_GPU_CMD_BIND_NATIVE_GEOMETRY: u32 = 0xd21c;
pub const VIRTIO_GPU_CMD_PRESENT_NATIVE_GEOMETRY: u32 = 0xd21d;
pub const VIRTIO_GPU_CMD_REFRESH_NATIVE_GEOMETRY: u32 = 0xd21e;
pub const VIRTIO_GPU_CMD_QUERY_NATIVE_GEOMETRY: u32 = 0xd21f;
pub const VIRTIO_GPU_RESP_OK_NATIVE_GEOMETRY: u32 = 0xd220;
pub const VIRTIO_GPU_CMD_QUERY_NATIVE_PROFILE: u32 = 0xd221;
pub const VIRTIO_GPU_RESP_OK_NATIVE_PROFILE: u32 = 0xd222;
pub const VIRTIO_GPU_CMD_CONFIGURE_NATIVE_PROFILE: u32 = 0xd223;

#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct VirtioGpuScanoutProfile {
    pub version: Le32, pub size: Le32, pub flags: Le32, pub content_rotation: Le32,
    pub storage_width: Le32, pub storage_height: Le32, pub logical_width: Le32, pub logical_height: Le32,
    pub endpoint_generation: Le64, pub profile_generation: Le64, pub reserved_tail: [Le64; 2],
}
impl From<VirtioGpuScanoutProfile> for gpu_display::ScanoutProfile {
    fn from(p: VirtioGpuScanoutProfile) -> Self {
        Self { version: p.version.into(), size: p.size.into(), flags: p.flags.into(),
            content_rotation: p.content_rotation.into(), storage_width: p.storage_width.into(),
            storage_height: p.storage_height.into(), logical_width: p.logical_width.into(),
            logical_height: p.logical_height.into(), endpoint_generation: p.endpoint_generation.into(),
            profile_generation: p.profile_generation.into(), reserved_tail: p.reserved_tail.map(u64::from) }
    }
}
impl From<gpu_display::ScanoutProfile> for VirtioGpuScanoutProfile {
    fn from(p: gpu_display::ScanoutProfile) -> Self {
        Self { version: p.version.into(), size: p.size.into(), flags: p.flags.into(),
            content_rotation: p.content_rotation.into(), storage_width: p.storage_width.into(),
            storage_height: p.storage_height.into(), logical_width: p.logical_width.into(),
            logical_height: p.logical_height.into(), endpoint_generation: p.endpoint_generation.into(),
            profile_generation: p.profile_generation.into(), reserved_tail: p.reserved_tail.map(Le64::from) }
    }
}
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_native_profile_response {
    pub hdr: virtio_gpu_ctrl_hdr, pub scanout_id: Le32, pub reserved: Le32,
    pub profile: VirtioGpuScanoutProfile, pub reset_generation: Le64, pub reserved_tail: Le64,
}
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_configure_native_profile {
    pub config: virtio_gpu_configure_native_geometry,
    pub endpoint_generation: Le64, pub profile_generation: Le64,
}
const _: () = assert!(size_of::<VirtioGpuScanoutProfile>() == 64);
const _: () = assert!(size_of::<virtio_gpu_native_profile_response>() == 112);
const _: () = assert!(size_of::<virtio_gpu_configure_native_profile>() == 112);

#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct VirtioGpuScanoutGeometry {
    pub version: Le32, pub size: Le32,
    pub storage_width: Le32, pub storage_height: Le32,
    pub logical_width: Le32, pub logical_height: Le32,
    pub content_rotation: Le32, pub reserved: Le32,
    pub reset_generation: Le64, pub mode_generation: Le64,
    pub reserved_tail: [Le64; 2],
}
impl From<VirtioGpuScanoutGeometry> for gpu_display::ScanoutGeometry {
    fn from(g: VirtioGpuScanoutGeometry) -> Self {
        Self { version: g.version.into(), size: g.size.into(),
            storage_width: g.storage_width.into(), storage_height: g.storage_height.into(),
            logical_width: g.logical_width.into(), logical_height: g.logical_height.into(),
            content_rotation: g.content_rotation.into(), reserved: g.reserved.into(),
            reset_generation: g.reset_generation.into(), mode_generation: g.mode_generation.into(),
            reserved_tail: g.reserved_tail.map(u64::from) }
    }
}
impl From<gpu_display::ScanoutGeometry> for VirtioGpuScanoutGeometry {
    fn from(g: gpu_display::ScanoutGeometry) -> Self {
        Self { version: g.version.into(), size: g.size.into(),
            storage_width: g.storage_width.into(), storage_height: g.storage_height.into(),
            logical_width: g.logical_width.into(), logical_height: g.logical_height.into(),
            content_rotation: g.content_rotation.into(), reserved: g.reserved.into(),
            reset_generation: g.reset_generation.into(), mode_generation: g.mode_generation.into(),
            reserved_tail: g.reserved_tail.map(Le64::from) }
    }
}
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_query_native_geometry {
    pub hdr: virtio_gpu_ctrl_hdr, pub scanout_id: Le32, pub reserved: Le32,
}
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_configure_native_geometry {
    pub hdr: virtio_gpu_ctrl_hdr, pub scanout_id: Le32, pub reserved: Le32,
    pub geometry: VirtioGpuScanoutGeometry,
}
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_bind_native_geometry {
    pub hdr: virtio_gpu_ctrl_hdr, pub resource_id: Le32, pub scanout_id: Le32,
    pub reset_generation: Le64, pub mode_generation: Le64,
}
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_present_native_geometry {
    pub operation: virtio_gpu_native_ahb_operation,
    pub reset_generation: Le64, pub mode_generation: Le64,
}
const _: () = assert!(size_of::<VirtioGpuScanoutGeometry>() == 64);
const _: () = assert!(size_of::<virtio_gpu_query_native_geometry>() == 32);
const _: () = assert!(size_of::<virtio_gpu_configure_native_geometry>() == 96);
const _: () = assert!(size_of::<virtio_gpu_bind_native_geometry>() == 48);
const _: () = assert!(size_of::<virtio_gpu_present_native_geometry>() == 56);

#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_native_ahb_paging {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub resource_id: Le32,
    pub operation: Le32,
    pub offset: Le64,
    pub length: Le32,
    pub pattern: Le32,
}
const _: () = assert!(size_of::<virtio_gpu_native_ahb_paging>() == 48);

/// Independent per-buffer release observation, never a renderer fence.
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_native_ahb_operation {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub resource_id: Le32,
    pub reserved: Le32,
    pub sequence: Le64,
}
const _: () = assert!(size_of::<virtio_gpu_native_ahb_operation>() == 40);

/* 3d commands */
pub const VIRTIO_GPU_CMD_CTX_CREATE: u32 = 0x200;
pub const VIRTIO_GPU_CMD_CTX_DESTROY: u32 = 0x201;
pub const VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE: u32 = 0x202;
pub const VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE: u32 = 0x203;
pub const VIRTIO_GPU_CMD_RESOURCE_CREATE_3D: u32 = 0x204;
pub const VIRTIO_GPU_CMD_TRANSFER_TO_HOST_3D: u32 = 0x205;
pub const VIRTIO_GPU_CMD_TRANSFER_FROM_HOST_3D: u32 = 0x206;
pub const VIRTIO_GPU_CMD_SUBMIT_3D: u32 = 0x207;
pub const VIRTIO_GPU_CMD_RESOURCE_MAP_BLOB: u32 = 0x208;
pub const VIRTIO_GPU_CMD_RESOURCE_UNMAP_BLOB: u32 = 0x209;

/* cursor commands */
pub const VIRTIO_GPU_CMD_UPDATE_CURSOR: u32 = 0x300;
pub const VIRTIO_GPU_CMD_MOVE_CURSOR: u32 = 0x301;

/* success responses */
pub const VIRTIO_GPU_RESP_OK_NODATA: u32 = 0x1100;
pub const VIRTIO_GPU_RESP_OK_DISPLAY_INFO: u32 = 0x1101;
pub const VIRTIO_GPU_RESP_OK_CAPSET_INFO: u32 = 0x1102;
pub const VIRTIO_GPU_RESP_OK_CAPSET: u32 = 0x1103;
pub const VIRTIO_GPU_RESP_OK_EDID: u32 = 0x1104;
pub const VIRTIO_GPU_RESP_OK_RESOURCE_UUID: u32 = 0x1105;
pub const VIRTIO_GPU_RESP_OK_MAP_INFO: u32 = 0x1106;

/// Private response for `USE_NATIVE_AHB` CREATE_BLOB negotiation.  This lives
/// outside the upstream response range because only the matching private blob
/// flag gives it meaning; ordinary virtio-gpu clients never see it.
pub const VIRTIO_GPU_RESP_OK_NATIVE_AHB: u32 = 0xd212;

// DroidVM gfxstream pre-alloc: set in `virtio_gpu_resp_map_info.map_info` when the blob is
// GpuPool-resident. The `pool_offset` field then carries the pool BYTE OFFSET (not a memparcel
// handle); the guest maps pool_base + offset out of its already-blessed RAM and never accepts.
pub const VIRTIO_GPU_MAP_INFO_POOL: u32 = 1 << 31;

/* CHROMIUM(b/277982577): success responses */
pub const VIRTIO_GPU_RESP_OK_RESOURCE_PLANE_INFO: u32 = 0x11FF;

/* error responses */
pub const VIRTIO_GPU_RESP_ERR_UNSPEC: u32 = 0x1200;
pub const VIRTIO_GPU_RESP_ERR_OUT_OF_MEMORY: u32 = 0x1201;
pub const VIRTIO_GPU_RESP_ERR_INVALID_SCANOUT_ID: u32 = 0x1202;
pub const VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID: u32 = 0x1203;
pub const VIRTIO_GPU_RESP_ERR_INVALID_CONTEXT_ID: u32 = 0x1204;
pub const VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER: u32 = 0x1205;

pub const VIRTIO_GPU_BLOB_MEM_GUEST: u32 = 0x0001;
pub const VIRTIO_GPU_BLOB_MEM_HOST3D: u32 = 0x0002;
pub const VIRTIO_GPU_BLOB_MEM_HOST3D_GUEST: u32 = 0x0003;

pub const VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE: u32 = 0x0001;
pub const VIRTIO_GPU_BLOB_FLAG_USE_SHAREABLE: u32 = 0x0002;
pub const VIRTIO_GPU_BLOB_FLAG_USE_CROSS_DEVICE: u32 = 0x0004;
/* Create a OS-specific handle from guest memory (not upstreamed). */
pub const VIRTIO_GPU_BLOB_FLAG_CREATE_GUEST_HANDLE: u32 = 0x0008;
/* Allocate an Android gralloc AHB and present that same allocation directly. */
pub const VIRTIO_GPU_BLOB_FLAG_USE_NATIVE_AHB: u32 = 0x0010;

pub const VIRTIO_GPU_NATIVE_AHB_REQUEST_VERSION: u32 = 2;
pub const VIRTIO_GPU_NATIVE_AHB_REQUEST_SIZE: u32 = 24;

/// v2 request. `size` covers the 24 bytes beginning at `version`, excluding
/// the standard control header and resource id.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct virtio_gpu_resource_create_native_ahb_body {
    pub resource_id: Le32,
    pub version: Le32,
    pub size: Le32,
    pub width: Le32,
    pub height: Le32,
    pub fourcc: Le32,
    pub flags: Le32,
}

const _: () = assert!(std::mem::size_of::<virtio_gpu_resource_create_native_ahb_body>() == 28);

/// Little-endian wire form of the authenticated Android gralloc description.
/// Keep this separate from gpu_display's host-endian ABI type: the latter is
/// passed through the Android FFI and must never become a wire struct by cast.
#[repr(C)]
#[derive(
    Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout, PartialEq, Eq,
)]
pub struct VirtioGpuNativeAhbDescription {
    pub version: Le32,
    pub size: Le32,
    pub buffer_id: Le64,
    pub allocation_size: Le64,
    pub modifier: Le64,
    pub plane_offset: Le64,
    pub plane_stride: Le32,
    pub width: Le32,
    pub height: Le32,
    pub fourcc: Le32,
    pub plane_count: Le32,
    pub layout_flags: Le32,
}

const _: () = assert!(std::mem::size_of::<VirtioGpuNativeAhbDescription>() == 64);

impl From<SharedAllocationDescription> for VirtioGpuNativeAhbDescription {
    fn from(description: SharedAllocationDescription) -> Self {
        Self {
            version: description.version.into(),
            size: description.size.into(),
            buffer_id: description.buffer_id.into(),
            allocation_size: description.allocation_size.into(),
            modifier: description.modifier.into(),
            plane_offset: description.plane_offset.into(),
            plane_stride: description.plane_stride.into(),
            width: description.width.into(),
            height: description.height.into(),
            fourcc: description.fourcc.into(),
            plane_count: description.plane_count.into(),
            layout_flags: description.layout_flags.into(),
        }
    }
}

pub const VIRTIO_GPU_SHM_ID_NONE: u8 = 0x0000;
pub const VIRTIO_GPU_SHM_ID_HOST_VISIBLE: u8 = 0x0001;

pub fn virtio_gpu_cmd_str(cmd: u32) -> &'static str {
    match cmd {
        VIRTIO_GPU_CMD_GET_DISPLAY_INFO => "VIRTIO_GPU_CMD_GET_DISPLAY_INFO",
        VIRTIO_GPU_CMD_RESOURCE_CREATE_2D => "VIRTIO_GPU_CMD_RESOURCE_CREATE_2D",
        VIRTIO_GPU_CMD_RESOURCE_UNREF => "VIRTIO_GPU_CMD_RESOURCE_UNREF",
        VIRTIO_GPU_CMD_SET_SCANOUT => "VIRTIO_GPU_CMD_SET_SCANOUT",
        VIRTIO_GPU_CMD_SET_SCANOUT_BLOB => "VIRTIO_GPU_CMD_SET_SCANOUT_BLOB",
        VIRTIO_GPU_CMD_RESOURCE_FLUSH => "VIRTIO_GPU_CMD_RESOURCE_FLUSH",
        VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D => "VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D",
        VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING => "VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING",
        VIRTIO_GPU_CMD_RESOURCE_DETACH_BACKING => "VIRTIO_GPU_CMD_RESOURCE_DETACH_BACKING",
        VIRTIO_GPU_CMD_GET_CAPSET_INFO => "VIRTIO_GPU_CMD_GET_CAPSET_INFO",
        VIRTIO_GPU_CMD_GET_CAPSET => "VIRTIO_GPU_CMD_GET_CAPSET",
        VIRTIO_GPU_CMD_GET_EDID => "VIRTIO_GPU_CMD_GET_EDID",
        VIRTIO_GPU_CMD_CTX_CREATE => "VIRTIO_GPU_CMD_CTX_CREATE",
        VIRTIO_GPU_CMD_CTX_DESTROY => "VIRTIO_GPU_CMD_CTX_DESTROY",
        VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE => "VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE",
        VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE => "VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE",
        VIRTIO_GPU_CMD_RESOURCE_ASSIGN_UUID => "VIRTIO_GPU_CMD_RESOURCE_ASSIGN_UUID",
        VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB => "VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB",
        VIRTIO_GPU_CMD_RESOURCE_CREATE_3D => "VIRTIO_GPU_CMD_RESOURCE_CREATE_3D",
        VIRTIO_GPU_CMD_TRANSFER_TO_HOST_3D => "VIRTIO_GPU_CMD_TRANSFER_TO_HOST_3D",
        VIRTIO_GPU_CMD_TRANSFER_FROM_HOST_3D => "VIRTIO_GPU_CMD_TRANSFER_FROM_HOST_3D",
        VIRTIO_GPU_CMD_SUBMIT_3D => "VIRTIO_GPU_CMD_SUBMIT_3D",
        VIRTIO_GPU_CMD_RESOURCE_MAP_BLOB => "VIRTIO_GPU_RESOURCE_MAP_BLOB",
        VIRTIO_GPU_CMD_RESOURCE_UNMAP_BLOB => "VIRTIO_GPU_RESOURCE_UNMAP_BLOB",
        VIRTIO_GPU_CMD_UPDATE_CURSOR => "VIRTIO_GPU_CMD_UPDATE_CURSOR",
        VIRTIO_GPU_CMD_MOVE_CURSOR => "VIRTIO_GPU_CMD_MOVE_CURSOR",
        VIRTIO_GPU_RESP_OK_NODATA => "VIRTIO_GPU_RESP_OK_NODATA",
        VIRTIO_GPU_RESP_OK_DISPLAY_INFO => "VIRTIO_GPU_RESP_OK_DISPLAY_INFO",
        VIRTIO_GPU_RESP_OK_CAPSET_INFO => "VIRTIO_GPU_RESP_OK_CAPSET_INFO",
        VIRTIO_GPU_RESP_OK_CAPSET => "VIRTIO_GPU_RESP_OK_CAPSET",
        VIRTIO_GPU_RESP_OK_RESOURCE_PLANE_INFO => "VIRTIO_GPU_RESP_OK_RESOURCE_PLANE_INFO",
        VIRTIO_GPU_RESP_OK_RESOURCE_UUID => "VIRTIO_GPU_RESP_OK_RESOURCE_UUID",
        VIRTIO_GPU_RESP_OK_MAP_INFO => "VIRTIO_GPU_RESP_OK_MAP_INFO",
        VIRTIO_GPU_RESP_ERR_UNSPEC => "VIRTIO_GPU_RESP_ERR_UNSPEC",
        VIRTIO_GPU_RESP_ERR_OUT_OF_MEMORY => "VIRTIO_GPU_RESP_ERR_OUT_OF_MEMORY",
        VIRTIO_GPU_RESP_ERR_INVALID_SCANOUT_ID => "VIRTIO_GPU_RESP_ERR_INVALID_SCANOUT_ID",
        VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID => "VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID",
        VIRTIO_GPU_RESP_ERR_INVALID_CONTEXT_ID => "VIRTIO_GPU_RESP_ERR_INVALID_CONTEXT_ID",
        VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER => "VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER",
        _ => "UNKNOWN",
    }
}

pub const VIRTIO_GPU_FLAG_FENCE: u32 = 1 << 0;
pub const VIRTIO_GPU_FLAG_INFO_RING_IDX: u32 = 1 << 1;
pub const VIRTIO_GPU_FLAG_FENCE_HOST_SHAREABLE: u32 = 1 << 2;

#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_ctrl_hdr {
    pub type_: Le32,
    pub flags: Le32,
    pub fence_id: Le64,
    pub ctx_id: Le32,
    pub ring_idx: u8,
    pub padding: [u8; 3],
}

/* data passed in the cursor vq */

#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_cursor_pos {
    pub scanout_id: Le32,
    pub x: Le32,
    pub y: Le32,
    pub padding: Le32,
}

/* VIRTIO_GPU_CMD_UPDATE_CURSOR, VIRTIO_GPU_CMD_MOVE_CURSOR */
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_update_cursor {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub pos: virtio_gpu_cursor_pos, /* update & move */
    pub resource_id: Le32,          /* update only */
    pub hot_x: Le32,                /* update only */
    pub hot_y: Le32,                /* update only */
    pub padding: Le32,
}

/* data passed in the control vq, 2d related */

#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_rect {
    pub x: Le32,
    pub y: Le32,
    pub width: Le32,
    pub height: Le32,
}

/* VIRTIO_GPU_CMD_RESOURCE_UNREF */
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_resource_unref {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub resource_id: Le32,
    pub padding: Le32,
}

/* VIRTIO_GPU_CMD_RESOURCE_CREATE_2D: create a 2d resource with a format */
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_resource_create_2d {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub resource_id: Le32,
    pub format: Le32,
    pub width: Le32,
    pub height: Le32,
}

/* VIRTIO_GPU_CMD_SET_SCANOUT */
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_set_scanout {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub r: virtio_gpu_rect,
    pub scanout_id: Le32,
    pub resource_id: Le32,
}

/* VIRTIO_GPU_CMD_RESOURCE_FLUSH */
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_resource_flush {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub r: virtio_gpu_rect,
    pub resource_id: Le32,
    pub padding: Le32,
}

/* VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D: simple transfer to_host */
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_transfer_to_host_2d {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub r: virtio_gpu_rect,
    pub offset: Le64,
    pub resource_id: Le32,
    pub padding: Le32,
}

#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_mem_entry {
    pub addr: Le64,
    pub length: Le32,
    pub padding: Le32,
}

/* VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING */
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_resource_attach_backing {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub resource_id: Le32,
    pub nr_entries: Le32,
}

/* VIRTIO_GPU_CMD_RESOURCE_DETACH_BACKING */
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_resource_detach_backing {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub resource_id: Le32,
    pub padding: Le32,
}

#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_display_one {
    pub r: virtio_gpu_rect,
    pub enabled: Le32,
    pub flags: Le32,
}

/* VIRTIO_GPU_RESP_OK_DISPLAY_INFO */
pub const VIRTIO_GPU_MAX_SCANOUTS: usize = 16;
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_resp_display_info {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub pmodes: [virtio_gpu_display_one; VIRTIO_GPU_MAX_SCANOUTS],
}

/* data passed in the control vq, 3d related */

#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_box {
    pub x: Le32,
    pub y: Le32,
    pub z: Le32,
    pub w: Le32,
    pub h: Le32,
    pub d: Le32,
}

/* VIRTIO_GPU_CMD_TRANSFER_TO_HOST_3D, VIRTIO_GPU_CMD_TRANSFER_FROM_HOST_3D */
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_transfer_host_3d {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub box_: virtio_gpu_box,
    pub offset: Le64,
    pub resource_id: Le32,
    pub level: Le32,
    pub stride: Le32,
    pub layer_stride: Le32,
}

/* VIRTIO_GPU_CMD_RESOURCE_CREATE_3D */
pub const VIRTIO_GPU_RESOURCE_FLAG_Y_0_TOP: u32 = 1 << 0;
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_resource_create_3d {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub resource_id: Le32,
    pub target: Le32,
    pub format: Le32,
    pub bind: Le32,
    pub width: Le32,
    pub height: Le32,
    pub depth: Le32,
    pub array_size: Le32,
    pub last_level: Le32,
    pub nr_samples: Le32,
    pub flags: Le32,
    pub padding: Le32,
}

/* VIRTIO_GPU_CMD_CTX_CREATE */
pub const VIRTIO_GPU_CONTEXT_INIT_CAPSET_ID_MASK: u32 = 1 << 0;
#[derive(Copy, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_ctx_create {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub nlen: Le32,
    pub context_init: Le32,
    pub debug_name: [u8; 64],
}

impl Default for virtio_gpu_ctx_create {
    fn default() -> Self {
        // SAFETY: trivially safe
        unsafe { ::std::mem::zeroed() }
    }
}

impl Clone for virtio_gpu_ctx_create {
    fn clone(&self) -> virtio_gpu_ctx_create {
        *self
    }
}

impl fmt::Debug for virtio_gpu_ctx_create {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let debug_name = from_utf8(&self.debug_name[..min(64, self.nlen.to_native() as usize)])
            .unwrap_or("<invalid>");
        f.debug_struct("virtio_gpu_ctx_create")
            .field("hdr", &self.hdr)
            .field("debug_name", &debug_name)
            .finish()
    }
}

/* VIRTIO_GPU_CMD_CTX_DESTROY */
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_ctx_destroy {
    pub hdr: virtio_gpu_ctrl_hdr,
}

/* VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE, VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE */
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_ctx_resource {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub resource_id: Le32,
    pub padding: Le32,
}

/* VIRTIO_GPU_CMD_SUBMIT_3D */
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_cmd_submit {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub size: Le32,

    // The in-fence IDs are prepended to the cmd_buf and memory layout
    // of the VIRTIO_GPU_CMD_SUBMIT_3D buffer looks like this:
    //   _________________
    //   | CMD_SUBMIT_3D |
    //   -----------------
    //   |  header       |
    //   |  in-fence IDs |
    //   |  cmd_buf      |
    //   -----------------
    //
    // This makes in-fence IDs naturally aligned to the sizeof(u64) inside
    // of the virtio buffer.
    pub num_in_fences: Le32,
}

pub const VIRTIO_GPU_CAPSET_VIRGL: u32 = 1;
pub const VIRTIO_GPU_CAPSET_VIRGL2: u32 = 2;
pub const VIRTIO_GPU_CAPSET_GFXSTREAM: u32 = 3;
pub const VIRTIO_GPU_CAPSET_VENUS: u32 = 4;
pub const VIRTIO_GPU_CAPSET_CROSS_DOMAIN: u32 = 5;

/* VIRTIO_GPU_CMD_GET_CAPSET_INFO */
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_get_capset_info {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub capset_index: Le32,
    pub padding: Le32,
}

/* VIRTIO_GPU_RESP_OK_CAPSET_INFO */
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_resp_capset_info {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub capset_id: Le32,
    pub capset_max_version: Le32,
    pub capset_max_size: Le32,
    pub padding: Le32,
}

/* VIRTIO_GPU_CMD_GET_CAPSET */
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_get_capset {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub capset_id: Le32,
    pub capset_version: Le32,
}

/* VIRTIO_GPU_RESP_OK_CAPSET */
#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct virtio_gpu_resp_capset {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub capset_data: PhantomData<[u8]>,
}

/* VIRTIO_GPU_CMD_GET_EDID */
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_get_edid {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub scanout: Le32,
    pub padding: Le32,
}

/* VIRTIO_GPU_RESP_OK_EDID */
#[derive(Copy, Clone, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_resp_get_edid {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub size: Le32,
    pub padding: Le32,
    pub edid: [u8; 1024],
}

/* VIRTIO_GPU_RESP_OK_RESOURCE_PLANE_INFO */
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_resp_resource_plane_info {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub count: Le32,
    pub padding: Le32,
    pub format_modifier: Le64,
    pub strides: [Le32; 4],
    pub offsets: [Le32; 4],
}

pub const PLANE_INFO_MAX_COUNT: usize = 4;

pub const VIRTIO_GPU_EVENT_DISPLAY: u32 = 1 << 0;
pub const VIRTIO_GPU_EVENT_NATIVE_AHB_REFRESH: u32 = 1 << 1;
/// Negotiated profile reconciliation; ACK before QUERY_PROFILE. No ownership grant.
pub const VIRTIO_GPU_EVENT_NATIVE_SCANOUT_PROFILE: u32 = 1 << 2;

#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_resource_create_blob {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub resource_id: Le32,
    pub blob_mem: Le32,
    pub blob_flags: Le32,
    pub nr_entries: Le32,
    pub blob_id: Le64,
    pub size: Le64,
}

#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_resource_map_blob {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub resource_id: Le32,
    pub padding: Le32,
    pub offset: Le64,
}

#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_resource_unmap_blob {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub resource_id: Le32,
    pub padding: Le32,
}

#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_resp_map_info {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub map_info: Le32,
    // Was `padding`. Carries the pool byte offset when VIRTIO_GPU_MAP_INFO_POOL is set in
    // `map_info` (the blob was sub-allocated from the boot-shared pool); 0 otherwise.
    pub pool_offset: Le32,
}

#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_resource_assign_uuid {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub resource_id: Le32,
    pub padding: Le32,
}

#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_resp_resource_uuid {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub uuid: [u8; 16],
}

/* VIRTIO_GPU_CMD_SET_SCANOUT_BLOB */
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_set_scanout_blob {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub r: virtio_gpu_rect,
    pub scanout_id: Le32,
    pub resource_id: Le32,
    pub width: Le32,
    pub height: Le32,
    pub format: Le32,
    pub padding: Le32,
    pub strides: [Le32; 4],
    pub offsets: [Le32; 4],
}

/* simple formats for fbcon/X use */
pub const VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM: u32 = 1;
pub const VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM: u32 = 2;
pub const VIRTIO_GPU_FORMAT_A8R8G8B8_UNORM: u32 = 3;
pub const VIRTIO_GPU_FORMAT_X8R8G8B8_UNORM: u32 = 4;
pub const VIRTIO_GPU_FORMAT_R8G8B8A8_UNORM: u32 = 67;
pub const VIRTIO_GPU_FORMAT_X8B8G8R8_UNORM: u32 = 68;
pub const VIRTIO_GPU_FORMAT_A8B8G8R8_UNORM: u32 = 121;
pub const VIRTIO_GPU_FORMAT_R8G8B8X8_UNORM: u32 = 134;

/// A virtio gpu command and associated metadata specific to each command.
#[derive(Copy, Clone)]
pub enum GpuCommand {
    GetDisplayTiming(virtio_gpu_get_display_timing),
    QueryNativeBlt(virtio_gpu_query_native_blt),
    NativeBlt(virtio_gpu_native_blt),
    GetDisplayInfo(virtio_gpu_ctrl_hdr),
    ResourceCreate2d(virtio_gpu_resource_create_2d),
    ResourceUnref(virtio_gpu_resource_unref),
    SetScanout(virtio_gpu_set_scanout),
    SetScanoutBlob(virtio_gpu_set_scanout_blob),
    ResourceFlush(virtio_gpu_resource_flush),
    TransferToHost2d(virtio_gpu_transfer_to_host_2d),
    ResourceAttachBacking(virtio_gpu_resource_attach_backing),
    ResourceDetachBacking(virtio_gpu_resource_detach_backing),
    GetCapsetInfo(virtio_gpu_get_capset_info),
    GetCapset(virtio_gpu_get_capset),
    GetEdid(virtio_gpu_get_edid),
    CtxCreate(virtio_gpu_ctx_create),
    CtxDestroy(virtio_gpu_ctx_destroy),
    CtxAttachResource(virtio_gpu_ctx_resource),
    CtxDetachResource(virtio_gpu_ctx_resource),
    ResourceCreate3d(virtio_gpu_resource_create_3d),
    TransferToHost3d(virtio_gpu_transfer_host_3d),
    TransferFromHost3d(virtio_gpu_transfer_host_3d),
    CmdSubmit3d(virtio_gpu_cmd_submit),
    ResourceCreateBlob(virtio_gpu_resource_create_blob),
    ResourceCreateNativeAhb(
        virtio_gpu_ctrl_hdr,
        virtio_gpu_resource_create_native_ahb_body,
    ),
    PresentNativeAhb(virtio_gpu_native_ahb_operation),
    ConfigureNativeGeometry(virtio_gpu_configure_native_geometry),
    QueryNativeGeometry(virtio_gpu_query_native_geometry),
    QueryNativeProfile(virtio_gpu_query_native_geometry),
    ConfigureNativeProfile(virtio_gpu_configure_native_profile),
    BindNativeGeometry(virtio_gpu_bind_native_geometry),
    PresentNativeGeometry(virtio_gpu_present_native_geometry),
    RefreshNativeGeometry(virtio_gpu_present_native_geometry),
    WaitNativeAhbRelease(virtio_gpu_native_ahb_operation),
    RefreshNativeAhb(virtio_gpu_native_ahb_operation),
    PageNativeAhb(virtio_gpu_native_ahb_paging),
    ResourceMapBlob(virtio_gpu_resource_map_blob),
    ResourceUnmapBlob(virtio_gpu_resource_unmap_blob),
    UpdateCursor(virtio_gpu_update_cursor),
    MoveCursor(virtio_gpu_update_cursor),
    ResourceAssignUuid(virtio_gpu_resource_assign_uuid),
}

/// An error indicating something went wrong decoding a `GpuCommand`. These correspond to
/// `VIRTIO_GPU_CMD_*`.
#[sorted]
#[derive(Error, Debug)]
pub enum GpuCommandDecodeError {
    /// The type of the command was invalid.
    #[error("invalid command type ({0})")]
    InvalidType(u32),
    /// An I/O error occurred.
    #[error("an I/O error occurred: {0}")]
    IO(io::Error),
}

impl From<io::Error> for GpuCommandDecodeError {
    fn from(e: io::Error) -> GpuCommandDecodeError {
        GpuCommandDecodeError::IO(e)
    }
}

impl fmt::Debug for GpuCommand {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::GpuCommand::*;
        match self {
            QueryNativeBlt(info) => f.debug_tuple("QueryNativeBlt").field(info).finish(),
            NativeBlt(info) => f.debug_tuple("NativeBlt").field(info).finish(),
            GetDisplayTiming(info) => f.debug_tuple("GetDisplayTiming").field(info).finish(),
            GetDisplayInfo(_info) => f.debug_struct("GetDisplayInfo").finish(),
            ResourceCreate2d(_info) => f.debug_struct("ResourceCreate2d").finish(),
            ResourceUnref(_info) => f.debug_struct("ResourceUnref").finish(),
            SetScanout(_info) => f.debug_struct("SetScanout").finish(),
            SetScanoutBlob(_info) => f.debug_struct("SetScanoutBlob").finish(),
            ResourceFlush(_info) => f.debug_struct("ResourceFlush").finish(),
            TransferToHost2d(_info) => f.debug_struct("TransferToHost2d").finish(),
            ResourceAttachBacking(_info) => f.debug_struct("ResourceAttachBacking").finish(),
            ResourceDetachBacking(_info) => f.debug_struct("ResourceDetachBacking").finish(),
            GetCapsetInfo(_info) => f.debug_struct("GetCapsetInfo").finish(),
            GetCapset(_info) => f.debug_struct("GetCapset").finish(),
            GetEdid(_info) => f.debug_struct("GetEdid").finish(),
            CtxCreate(_info) => f.debug_struct("CtxCreate").finish(),
            CtxDestroy(_info) => f.debug_struct("CtxDestroy").finish(),
            CtxAttachResource(_info) => f.debug_struct("CtxAttachResource").finish(),
            CtxDetachResource(_info) => f.debug_struct("CtxDetachResource").finish(),
            ResourceCreate3d(_info) => f.debug_struct("ResourceCreate3d").finish(),
            TransferToHost3d(_info) => f.debug_struct("TransferToHost3d").finish(),
            TransferFromHost3d(_info) => f.debug_struct("TransferFromHost3d").finish(),
            CmdSubmit3d(_info) => f.debug_struct("CmdSubmit3d").finish(),
            ResourceCreateBlob(_info) => f.debug_struct("ResourceCreateBlob").finish(),
            ResourceCreateNativeAhb(_, _info) => f.debug_struct("ResourceCreateNativeAhb").finish(),
            ConfigureNativeGeometry(info) => f.debug_tuple("ConfigureNativeGeometry").field(info).finish(),
            QueryNativeGeometry(info) => f.debug_tuple("QueryNativeGeometry").field(info).finish(),
            QueryNativeProfile(info) => f.debug_tuple("QueryNativeProfile").field(info).finish(),
            ConfigureNativeProfile(info) => f.debug_tuple("ConfigureNativeProfile").field(info).finish(),
            BindNativeGeometry(info) => f.debug_tuple("BindNativeGeometry").field(info).finish(),
            PresentNativeGeometry(info) | RefreshNativeGeometry(info) => f.debug_tuple("NativeGeometryPresent").field(info).finish(),
            PresentNativeAhb(info) => f.debug_tuple("PresentNativeAhb").field(info).finish(),
            WaitNativeAhbRelease(info) => f.debug_tuple("WaitNativeAhbRelease").field(info).finish(),
            RefreshNativeAhb(info) => f.debug_tuple("RefreshNativeAhb").field(info).finish(),
            PageNativeAhb(info) => f.debug_tuple("PageNativeAhb").field(info).finish(),
            ResourceMapBlob(_info) => f.debug_struct("ResourceMapBlob").finish(),
            ResourceUnmapBlob(_info) => f.debug_struct("ResourceUnmapBlob").finish(),
            UpdateCursor(_info) => f.debug_struct("UpdateCursor").finish(),
            MoveCursor(_info) => f.debug_struct("MoveCursor").finish(),
            ResourceAssignUuid(_info) => f.debug_struct("ResourceAssignUuid").finish(),
        }
    }
}

impl GpuCommand {
    /// Decodes a command from the given chunk of memory.
    pub fn decode(cmd: &mut Reader) -> Result<GpuCommand, GpuCommandDecodeError> {
        use self::GpuCommand::*;
        let hdr = cmd.peek_obj::<virtio_gpu_ctrl_hdr>()?;
        Ok(match hdr.type_.into() {
            VIRTIO_GPU_CMD_GET_DISPLAY_TIMING => GetDisplayTiming(cmd.read_obj()?),
            VIRTIO_GPU_CMD_GET_DISPLAY_INFO => GetDisplayInfo(cmd.read_obj()?),
            VIRTIO_GPU_CMD_RESOURCE_CREATE_2D => ResourceCreate2d(cmd.read_obj()?),
            VIRTIO_GPU_CMD_RESOURCE_UNREF => ResourceUnref(cmd.read_obj()?),
            VIRTIO_GPU_CMD_SET_SCANOUT => SetScanout(cmd.read_obj()?),
            VIRTIO_GPU_CMD_SET_SCANOUT_BLOB => SetScanoutBlob(cmd.read_obj()?),
            VIRTIO_GPU_CMD_RESOURCE_FLUSH => ResourceFlush(cmd.read_obj()?),
            VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D => TransferToHost2d(cmd.read_obj()?),
            VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING => ResourceAttachBacking(cmd.read_obj()?),
            VIRTIO_GPU_CMD_RESOURCE_DETACH_BACKING => ResourceDetachBacking(cmd.read_obj()?),
            VIRTIO_GPU_CMD_GET_CAPSET_INFO => GetCapsetInfo(cmd.read_obj()?),
            VIRTIO_GPU_CMD_GET_CAPSET => GetCapset(cmd.read_obj()?),
            VIRTIO_GPU_CMD_GET_EDID => GetEdid(cmd.read_obj()?),
            VIRTIO_GPU_CMD_CTX_CREATE => CtxCreate(cmd.read_obj()?),
            VIRTIO_GPU_CMD_CTX_DESTROY => CtxDestroy(cmd.read_obj()?),
            VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE => CtxAttachResource(cmd.read_obj()?),
            VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE => CtxDetachResource(cmd.read_obj()?),
            VIRTIO_GPU_CMD_RESOURCE_CREATE_3D => ResourceCreate3d(cmd.read_obj()?),
            VIRTIO_GPU_CMD_TRANSFER_TO_HOST_3D => TransferToHost3d(cmd.read_obj()?),
            VIRTIO_GPU_CMD_TRANSFER_FROM_HOST_3D => TransferFromHost3d(cmd.read_obj()?),
            VIRTIO_GPU_CMD_SUBMIT_3D => CmdSubmit3d(cmd.read_obj()?),
            VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB => ResourceCreateBlob(cmd.read_obj()?),
            VIRTIO_GPU_CMD_RESOURCE_CREATE_NATIVE_AHB => {
                ResourceCreateNativeAhb(cmd.read_obj()?, cmd.read_obj()?)
            }
            VIRTIO_GPU_CMD_PRESENT_NATIVE_AHB => PresentNativeAhb(cmd.read_obj()?),
            VIRTIO_GPU_CMD_CONFIGURE_NATIVE_GEOMETRY => ConfigureNativeGeometry(cmd.read_obj()?),
            VIRTIO_GPU_CMD_QUERY_NATIVE_GEOMETRY => QueryNativeGeometry(cmd.read_obj()?),
            VIRTIO_GPU_CMD_QUERY_NATIVE_PROFILE => QueryNativeProfile(cmd.read_obj()?),
            VIRTIO_GPU_CMD_CONFIGURE_NATIVE_PROFILE => ConfigureNativeProfile(cmd.read_obj()?),
            VIRTIO_GPU_CMD_BIND_NATIVE_GEOMETRY => BindNativeGeometry(cmd.read_obj()?),
            VIRTIO_GPU_CMD_PRESENT_NATIVE_GEOMETRY => PresentNativeGeometry(cmd.read_obj()?),
            VIRTIO_GPU_CMD_REFRESH_NATIVE_GEOMETRY => RefreshNativeGeometry(cmd.read_obj()?),
            VIRTIO_GPU_CMD_WAIT_NATIVE_AHB_RELEASE => WaitNativeAhbRelease(cmd.read_obj()?),
            VIRTIO_GPU_CMD_REFRESH_NATIVE_AHB => RefreshNativeAhb(cmd.read_obj()?),
            VIRTIO_GPU_CMD_PAGE_NATIVE_AHB => PageNativeAhb(cmd.read_obj()?),
            VIRTIO_GPU_CMD_QUERY_NATIVE_BLT => QueryNativeBlt(cmd.read_obj()?),
            VIRTIO_GPU_CMD_NATIVE_BLT => NativeBlt(cmd.read_obj()?),
            VIRTIO_GPU_CMD_RESOURCE_MAP_BLOB => ResourceMapBlob(cmd.read_obj()?),
            VIRTIO_GPU_CMD_RESOURCE_UNMAP_BLOB => ResourceUnmapBlob(cmd.read_obj()?),
            VIRTIO_GPU_CMD_UPDATE_CURSOR => UpdateCursor(cmd.read_obj()?),
            VIRTIO_GPU_CMD_MOVE_CURSOR => MoveCursor(cmd.read_obj()?),
            VIRTIO_GPU_CMD_RESOURCE_ASSIGN_UUID => ResourceAssignUuid(cmd.read_obj()?),
            _ => return Err(GpuCommandDecodeError::InvalidType(hdr.type_.into())),
        })
    }

    /// Gets the generic `virtio_gpu_ctrl_hdr` from this command.
    pub fn ctrl_hdr(&self) -> &virtio_gpu_ctrl_hdr {
        use self::GpuCommand::*;
        match self {
            GetDisplayTiming(info) => &info.hdr,
            QueryNativeBlt(info) => &info.hdr,
            NativeBlt(info) => &info.hdr,
            GetDisplayInfo(info) => info,
            ResourceCreate2d(info) => &info.hdr,
            ResourceUnref(info) => &info.hdr,
            SetScanout(info) => &info.hdr,
            SetScanoutBlob(info) => &info.hdr,
            ResourceFlush(info) => &info.hdr,
            TransferToHost2d(info) => &info.hdr,
            ResourceAttachBacking(info) => &info.hdr,
            ResourceDetachBacking(info) => &info.hdr,
            GetCapsetInfo(info) => &info.hdr,
            GetCapset(info) => &info.hdr,
            GetEdid(info) => &info.hdr,
            CtxCreate(info) => &info.hdr,
            CtxDestroy(info) => &info.hdr,
            CtxAttachResource(info) => &info.hdr,
            CtxDetachResource(info) => &info.hdr,
            ResourceCreate3d(info) => &info.hdr,
            TransferToHost3d(info) => &info.hdr,
            TransferFromHost3d(info) => &info.hdr,
            CmdSubmit3d(info) => &info.hdr,
            ResourceCreateBlob(info) => &info.hdr,
            ResourceCreateNativeAhb(hdr, _) => hdr,
            PresentNativeAhb(info) | WaitNativeAhbRelease(info) | RefreshNativeAhb(info) => &info.hdr,
            ConfigureNativeGeometry(info) => &info.hdr,
            QueryNativeGeometry(info) => &info.hdr,
            QueryNativeProfile(info) => &info.hdr,
            ConfigureNativeProfile(info) => &info.config.hdr,
            BindNativeGeometry(info) => &info.hdr,
            PresentNativeGeometry(info) | RefreshNativeGeometry(info) => &info.operation.hdr,
            PageNativeAhb(info) => &info.hdr,
            ResourceMapBlob(info) => &info.hdr,
            ResourceUnmapBlob(info) => &info.hdr,
            UpdateCursor(info) => &info.hdr,
            MoveCursor(info) => &info.hdr,
            ResourceAssignUuid(info) => &info.hdr,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct GpuResponsePlaneInfo {
    pub stride: u32,
    pub offset: u32,
}

/// A response to a `GpuCommand`. These correspond to `VIRTIO_GPU_RESP_*`.
#[derive(Debug)]
pub enum GpuResponse {
    OkNativeBltIdentity(virtio_gpu_native_blt_identity),
    OkNativeBlt(virtio_gpu_native_blt_response),
    OkDisplayTiming { cookie: u64, sample: virtio_gpu_display_timing },
    OkNoData,
    OkDisplayInfo(Vec<(u32, u32, bool)>),
    OkNativeAhbDescription(VirtioGpuNativeAhbDescription),
    OkNativeAhbOperation { resource_id: u32, sequence: u64 },
    OkNativeGeometry { scanout_id: u32, configured: bool, geometry: VirtioGpuScanoutGeometry },
    OkNativeProfile { scanout_id: u32, profile: VirtioGpuScanoutProfile, reset_generation: u64 },
    OkNativeAhbPaging { info: virtio_gpu_native_ahb_paging, data: Vec<u8> },
    OkCapsetInfo {
        capset_id: u32,
        version: u32,
        size: u32,
    },
    OkCapset(Vec<u8>),
    OkEdid(Box<EdidBytes>),
    OkResourcePlaneInfo {
        format_modifier: u64,
        plane_info: Vec<GpuResponsePlaneInfo>,
    },
    OkResourceUuid {
        uuid: [u8; 16],
    },
    OkMapInfo {
        map_info: u32,
        /// Byte offset within the pre-shared pool, when `map_info` sets VIRTIO_GPU_MAP_INFO_POOL.
        pool_offset: Option<u32>,
    },
    ErrUnspec,
    ErrTube(TubeError),
    ErrBase(BaseError),
    ErrRutabaga(RutabagaError),
    ErrDisplay(GpuDisplayError),
    ErrScanout {
        num_scanouts: u32,
    },
    ErrEdid(String),
    ErrOutOfMemory,
    ErrInvalidScanoutId,
    ErrInvalidResourceId,
    ErrInvalidContextId,
    ErrInvalidParameter,
    ErrUdmabuf(UdmabufError),
}

impl From<TubeError> for GpuResponse {
    fn from(e: TubeError) -> GpuResponse {
        GpuResponse::ErrTube(e)
    }
}

impl From<RutabagaError> for GpuResponse {
    fn from(e: RutabagaError) -> GpuResponse {
        GpuResponse::ErrRutabaga(e)
    }
}

impl From<GpuDisplayError> for GpuResponse {
    fn from(e: GpuDisplayError) -> GpuResponse {
        GpuResponse::ErrDisplay(e)
    }
}

impl From<UdmabufError> for GpuResponse {
    fn from(e: UdmabufError) -> GpuResponse {
        GpuResponse::ErrUdmabuf(e)
    }
}

impl Display for GpuResponse {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::GpuResponse::*;
        match self {
            OkNativeBltIdentity(_) => write!(f, "ok native Blt identity"),
            OkNativeBlt(_) => write!(f, "ok completed native Blt"),
            OkNoData => write!(f, "ok no data"),
            OkDisplayInfo(_) => write!(f, "ok display info"),
            OkNativeAhbDescription(_) => write!(f, "ok native ahb description"),
            OkDisplayTiming { .. } => write!(f, "ok display timing"),
            OkNativeAhbOperation { .. } => write!(f, "ok native ahb operation"),
            OkNativeGeometry { .. } => write!(f, "ok native scanout geometry"),
            OkNativeProfile { .. } => write!(f, "ok native scanout profile"),
            OkNativeAhbPaging { .. } => write!(f, "ok native ahb paging"),
            OkCapsetInfo { .. } => write!(f, "ok capset info"),
            OkCapset(_) => write!(f, "ok capset"),
            OkEdid(_) => write!(f, "ok edid"),
            OkResourcePlaneInfo { .. } => write!(f, "ok resource plane info"),
            OkResourceUuid { .. } => write!(f, "ok resource uuid"),
            OkMapInfo { map_info, .. } => write!(f, "ok map info: {}", map_info),
            ErrUnspec => write!(f, "unspecified error"),
            ErrTube(e) => write!(f, "tube error: {}", e),
            ErrBase(e) => write!(f, "base error: {}", e),
            ErrRutabaga(e) => write!(f, "renderer error: {}", e),
            ErrDisplay(e) => write!(f, "display error: {}", e),
            ErrScanout { num_scanouts } => write!(f, "non-zero scanout: {}", num_scanouts),
            ErrEdid(e) => write!(f, "edid error: {}", e),
            ErrOutOfMemory => write!(f, "out of memory error"),
            ErrInvalidScanoutId => write!(f, "invalid scanout id"),
            ErrInvalidResourceId => write!(f, "invalid resource id"),
            ErrInvalidContextId => write!(f, "invalid context id"),
            ErrInvalidParameter => write!(f, "invalid parameter"),
            ErrUdmabuf(e) => write!(f, "udmabuf error: {}", e),
        }
    }
}

impl std::error::Error for GpuResponse {}

/// An error indicating something went wrong decoding a `GpuCommand`.
#[sorted]
#[derive(Error, Debug)]
pub enum GpuResponseEncodeError {
    #[error("display timing response buffer has only {available} bytes")]
    DisplayTimingBufferTooSmall { available: usize },
    /// An I/O error occurred.
    #[error("an I/O error occurred: {0}")]
    IO(io::Error),
    /// Native AHB metadata must never be silently truncated to OK_NODATA.
    #[error("native AHB description response buffer has only {available} bytes")]
    NativeAhbDescriptionBufferTooSmall { available: usize },
    #[error("native AHB paging response buffer has only {available} bytes")]
    NativeAhbPagingBufferTooSmall { available: usize },
    /// More displays than are valid were in a `OkDisplayInfo`.
    #[error("{0} is more displays than are valid")]
    TooManyDisplays(usize),
    /// More planes than are valid were in a `OkResourcePlaneInfo`.
    #[error("{0} is more planes than are valid")]
    TooManyPlanes(usize),
}

impl From<io::Error> for GpuResponseEncodeError {
    fn from(e: io::Error) -> GpuResponseEncodeError {
        GpuResponseEncodeError::IO(e)
    }
}

pub type VirtioGpuResult = std::result::Result<GpuResponse, GpuResponse>;

impl GpuResponse {
    /// Encodes a this `GpuResponse` into `resp` and the given set of metadata.
    pub fn encode(
        &self,
        flags: u32,
        fence_id: u64,
        ctx_id: u32,
        ring_idx: u8,
        resp: &mut Writer,
    ) -> Result<u32, GpuResponseEncodeError> {
        let hdr = virtio_gpu_ctrl_hdr {
            type_: Le32::from(self.get_type()),
            flags: Le32::from(flags),
            fence_id: Le64::from(fence_id),
            ctx_id: Le32::from(ctx_id),
            ring_idx,
            padding: Default::default(),
        };
        let len = match *self {
            GpuResponse::OkNativeBltIdentity(mut identity) => {
                identity.hdr=hdr; resp.write_obj(identity)?; size_of_val(&identity)
            }
            GpuResponse::OkNativeBlt(mut result) => {
                result.hdr=hdr; resp.write_obj(result)?; size_of_val(&result)
            }
            GpuResponse::OkDisplayInfo(ref info) => {
                if info.len() > VIRTIO_GPU_MAX_SCANOUTS {
                    return Err(GpuResponseEncodeError::TooManyDisplays(info.len()));
                }
                let mut disp_info = virtio_gpu_resp_display_info {
                    hdr,
                    pmodes: Default::default(),
                };
                for (disp_mode, &(width, height, enabled)) in disp_info.pmodes.iter_mut().zip(info)
                {
                    disp_mode.r.width = Le32::from(width);
                    disp_mode.r.height = Le32::from(height);
                    disp_mode.enabled = Le32::from(enabled as u32);
                }
                resp.write_obj(disp_info)?;
                size_of_val(&disp_info)
            }
            GpuResponse::OkNativeAhbDescription(description) => {
                if resp.available_bytes()
                    < size_of::<virtio_gpu_ctrl_hdr>() + size_of_val(&description)
                {
                    return Err(GpuResponseEncodeError::NativeAhbDescriptionBufferTooSmall {
                        available: resp.available_bytes(),
                    });
                }
                resp.write_obj(hdr)?;
                resp.write_obj(description)?;
                size_of::<virtio_gpu_ctrl_hdr>() + size_of_val(&description)
            }
            GpuResponse::OkDisplayTiming { cookie, sample } => {
                let length = size_of_val(&hdr) + size_of::<Le64>() + size_of_val(&sample);
                if resp.available_bytes() < length {
                    return Err(GpuResponseEncodeError::DisplayTimingBufferTooSmall {
                        available: resp.available_bytes(),
                    });
                }
                resp.write_obj(hdr)?;
                resp.write_obj(Le64::from(cookie))?;
                resp.write_obj(sample)?;
                length
            }
            GpuResponse::OkNativeGeometry { scanout_id, configured, geometry } => {
                let response = virtio_gpu_configure_native_geometry {
                    hdr, scanout_id: scanout_id.into(), reserved: u32::from(configured).into(), geometry,
                };
                resp.write_obj(response)?;
                size_of::<virtio_gpu_configure_native_geometry>()
            }
            GpuResponse::OkNativeProfile { scanout_id, profile, reset_generation } => {
                resp.write_obj(virtio_gpu_native_profile_response { hdr, scanout_id: scanout_id.into(),
                    reserved: 0.into(), profile, reset_generation: reset_generation.into(), reserved_tail: 0.into() })?;
                size_of::<virtio_gpu_native_profile_response>()
            }
            GpuResponse::OkNativeAhbOperation { resource_id, sequence } => {
                resp.write_obj(virtio_gpu_native_ahb_operation {
                    hdr,
                    resource_id: resource_id.into(),
                    reserved: 0.into(),
                    sequence: sequence.into(),
                })?;
                size_of::<virtio_gpu_native_ahb_operation>()
            }
            GpuResponse::OkNativeAhbPaging { mut info, ref data } => {
                let length = size_of::<virtio_gpu_native_ahb_paging>() + data.len();
                if resp.available_bytes() < length {
                    return Err(GpuResponseEncodeError::NativeAhbPagingBufferTooSmall {
                        available: resp.available_bytes(),
                    });
                }
                info.hdr = hdr;
                resp.write_obj(info)?;
                resp.write_all(data)?;
                length
            }
            GpuResponse::OkCapsetInfo {
                capset_id,
                version,
                size,
            } => {
                resp.write_obj(virtio_gpu_resp_capset_info {
                    hdr,
                    capset_id: Le32::from(capset_id),
                    capset_max_version: Le32::from(version),
                    capset_max_size: Le32::from(size),
                    padding: Le32::from(0),
                })?;
                size_of::<virtio_gpu_resp_capset_info>()
            }
            GpuResponse::OkCapset(ref data) => {
                resp.write_obj(hdr)?;
                resp.write_all(data)?;
                size_of_val(&hdr) + data.len()
            }
            GpuResponse::OkEdid(ref edid_bytes) => {
                let mut edid_resp = virtio_gpu_resp_get_edid {
                    hdr,
                    size: Le32::from(1024),
                    padding: Le32::from(0),
                    edid: [0; 1024],
                };

                edid_resp.edid[0..edid_bytes.len()].copy_from_slice(edid_bytes.as_bytes());
                resp.write_obj(edid_resp)?;
                size_of::<virtio_gpu_resp_get_edid>()
            }
            GpuResponse::OkResourcePlaneInfo {
                format_modifier,
                ref plane_info,
            } => {
                if plane_info.len() > PLANE_INFO_MAX_COUNT {
                    return Err(GpuResponseEncodeError::TooManyPlanes(plane_info.len()));
                }
                let mut strides = [Le32::default(); PLANE_INFO_MAX_COUNT];
                let mut offsets = [Le32::default(); PLANE_INFO_MAX_COUNT];
                for (plane_index, plane) in plane_info.iter().enumerate() {
                    strides[plane_index] = plane.stride.into();
                    offsets[plane_index] = plane.offset.into();
                }
                let plane_info = virtio_gpu_resp_resource_plane_info {
                    hdr,
                    count: Le32::from(plane_info.len() as u32),
                    padding: 0.into(),
                    format_modifier: format_modifier.into(),
                    strides,
                    offsets,
                };
                if resp.available_bytes() >= size_of_val(&plane_info) {
                    resp.write_obj(plane_info)?;
                    size_of_val(&plane_info)
                } else {
                    // In case there is too little room in the response slice to store the
                    // entire virtio_gpu_resp_resource_plane_info, convert response to a regular
                    // VIRTIO_GPU_RESP_OK_NODATA and attempt to return that.
                    resp.write_obj(virtio_gpu_ctrl_hdr {
                        type_: Le32::from(VIRTIO_GPU_RESP_OK_NODATA),
                        ..hdr
                    })?;
                    size_of_val(&hdr)
                }
            }
            GpuResponse::OkResourceUuid { uuid } => {
                let resp_info = virtio_gpu_resp_resource_uuid { hdr, uuid };

                resp.write_obj(resp_info)?;
                size_of_val(&resp_info)
            }
            GpuResponse::OkMapInfo {
                map_info,
                pool_offset,
            } => {
                let base = virtio_gpu_resp_map_info {
                    hdr,
                    map_info: Le32::from(map_info),
                    pool_offset: Le32::from(pool_offset.unwrap_or(0)),
                };
                resp.write_obj(base)?;
                size_of_val(&base)
            }
            _ => {
                resp.write_obj(hdr)?;
                size_of_val(&hdr)
            }
        };
        Ok(len as u32)
    }

    /// Gets the `VIRTIO_GPU_*` enum value that corresponds to this variant.
    pub fn get_type(&self) -> u32 {
        match self {
            GpuResponse::OkDisplayTiming { .. } => VIRTIO_GPU_RESP_OK_DISPLAY_TIMING,
            GpuResponse::OkNoData => VIRTIO_GPU_RESP_OK_NODATA,
            GpuResponse::OkDisplayInfo(_) => VIRTIO_GPU_RESP_OK_DISPLAY_INFO,
            GpuResponse::OkNativeBltIdentity(_) => VIRTIO_GPU_RESP_OK_NATIVE_BLT_IDENTITY,
            GpuResponse::OkNativeBlt(_) => VIRTIO_GPU_RESP_OK_NATIVE_BLT,
            GpuResponse::OkNativeAhbDescription(_) => VIRTIO_GPU_RESP_OK_NATIVE_AHB,
            GpuResponse::OkNativeAhbOperation { .. } => VIRTIO_GPU_RESP_OK_NATIVE_AHB_OPERATION,
            GpuResponse::OkNativeGeometry { .. } => VIRTIO_GPU_RESP_OK_NATIVE_GEOMETRY,
            GpuResponse::OkNativeProfile { .. } => VIRTIO_GPU_RESP_OK_NATIVE_PROFILE,
            GpuResponse::OkNativeAhbPaging { .. } => VIRTIO_GPU_RESP_OK_NATIVE_AHB_PAGING,
            GpuResponse::OkCapsetInfo { .. } => VIRTIO_GPU_RESP_OK_CAPSET_INFO,
            GpuResponse::OkCapset(_) => VIRTIO_GPU_RESP_OK_CAPSET,
            GpuResponse::OkEdid(_) => VIRTIO_GPU_RESP_OK_EDID,
            GpuResponse::OkResourcePlaneInfo { .. } => VIRTIO_GPU_RESP_OK_RESOURCE_PLANE_INFO,
            GpuResponse::OkResourceUuid { .. } => VIRTIO_GPU_RESP_OK_RESOURCE_UUID,
            GpuResponse::OkMapInfo { .. } => VIRTIO_GPU_RESP_OK_MAP_INFO,
            GpuResponse::ErrUnspec => VIRTIO_GPU_RESP_ERR_UNSPEC,
            GpuResponse::ErrTube(_) => VIRTIO_GPU_RESP_ERR_UNSPEC,
            GpuResponse::ErrBase(_) => VIRTIO_GPU_RESP_ERR_UNSPEC,
            GpuResponse::ErrRutabaga(_) => VIRTIO_GPU_RESP_ERR_UNSPEC,
            GpuResponse::ErrDisplay(_) => VIRTIO_GPU_RESP_ERR_UNSPEC,
            GpuResponse::ErrUdmabuf(_) => VIRTIO_GPU_RESP_ERR_UNSPEC,
            GpuResponse::ErrScanout { num_scanouts: _ } => VIRTIO_GPU_RESP_ERR_UNSPEC,
            GpuResponse::ErrEdid(_) => VIRTIO_GPU_RESP_ERR_UNSPEC,
            GpuResponse::ErrOutOfMemory => VIRTIO_GPU_RESP_ERR_OUT_OF_MEMORY,
            GpuResponse::ErrInvalidScanoutId => VIRTIO_GPU_RESP_ERR_INVALID_SCANOUT_ID,
            GpuResponse::ErrInvalidResourceId => VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID,
            GpuResponse::ErrInvalidContextId => VIRTIO_GPU_RESP_ERR_INVALID_CONTEXT_ID,
            GpuResponse::ErrInvalidParameter => VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cros_async::MemRegion;
    use smallvec::smallvec;
    use vm_memory::GuestAddress;
    use vm_memory::GuestMemory;

    #[test]
    fn display_timing_wire_keeps_cookie_epochs_and_unknown_accuracy() {
        assert_eq!(size_of::<virtio_gpu_get_display_timing>(), 40);
        assert_eq!(size_of::<virtio_gpu_display_timing>(), 80);
        let sample = gpu_display::DisplayTimingSample { version: 1, size: 80, flags: 1,
            attachment_epoch: 0x1122334455667788, mode_epoch: 7, sequence: 9,
            host_now_ns: 500, present_ns: 400, latch_ns: 300, uncertainty_ns: u64::MAX,
            ..Default::default() };
        let mem = GuestMemory::new(&[(GuestAddress(0), 0x1000)]).unwrap();
        let mut writer = Writer::new_from_regions(&mem, smallvec![MemRegion { offset: 0x100, len: 112 }]);
        let response = GpuResponse::OkDisplayTiming { cookie: 0x8877665544332211, sample: sample.into() };
        assert_eq!(response.encode(0, 0, 0, 0, &mut writer).unwrap(), 112);
        let header: virtio_gpu_ctrl_hdr = mem.read_obj_from_addr(GuestAddress(0x100)).unwrap();
        assert_eq!(header.type_.to_native(), VIRTIO_GPU_RESP_OK_DISPLAY_TIMING);
        let cookie: Le64 = mem.read_obj_from_addr(GuestAddress(0x118)).unwrap();
        assert_eq!(cookie.to_native(), 0x8877665544332211);
        let wire: virtio_gpu_display_timing = mem.read_obj_from_addr(GuestAddress(0x120)).unwrap();
        assert_eq!(wire.attachment_epoch.to_native(), sample.attachment_epoch);
        assert_eq!(wire.uncertainty_ns.to_native(), u64::MAX);
        assert_eq!(wire.period_ns.to_native(), 0);
        for len in [0, 24, 32, 80, 111] {
            let mut short = Writer::new_from_regions(&mem, smallvec![MemRegion { offset: 0x200, len }]);
            assert!(matches!(response.encode(0, 0, 0, 0, &mut short),
                Err(GpuResponseEncodeError::DisplayTimingBufferTooSmall { .. })));
            assert_eq!(short.bytes_written(), 0);
        }
    }

    fn native_description() -> SharedAllocationDescription {
        SharedAllocationDescription {
            version: 1,
            size: 64,
            buffer_id: 0x0102_0304_0506_0708,
            allocation_size: 0x12_3000,
            modifier: 0,
            plane_offset: 0x4000,
            plane_stride: 12_288,
            width: 3_040,
            height: 1_904,
            fourcc: u32::from_le_bytes(*b"AB24"),
            plane_count: 1,
            layout_flags: 1,
        }
    }

    #[test]
    fn native_ahb_response_is_fixed_little_endian_wire_layout() {
        const RESPONSE_SIZE: usize =
            size_of::<virtio_gpu_ctrl_hdr>() + size_of::<VirtioGpuNativeAhbDescription>();
        assert_eq!(RESPONSE_SIZE, 88);

        let mem = GuestMemory::new(&[(GuestAddress(0), 0x1000)]).unwrap();
        let mut writer = Writer::new_from_regions(
            &mem,
            smallvec![MemRegion {
                offset: 0x100,
                len: RESPONSE_SIZE,
            }],
        );
        let wire: VirtioGpuNativeAhbDescription = native_description().into();
        let response = GpuResponse::OkNativeAhbDescription(wire);
        assert_eq!(
            response
                .encode(1, 0x1122_3344_5566_7788, 7, 3, &mut writer)
                .unwrap(),
            RESPONSE_SIZE as u32,
        );

        let header: virtio_gpu_ctrl_hdr = mem.read_obj_from_addr(GuestAddress(0x100)).unwrap();
        assert_eq!(header.type_.to_native(), VIRTIO_GPU_RESP_OK_NATIVE_AHB);
        assert_eq!(header.flags.to_native(), 1);
        assert_eq!(header.fence_id.to_native(), 0x1122_3344_5566_7788);
        assert_eq!(header.ctx_id.to_native(), 7);
        assert_eq!(header.ring_idx, 3);
        assert_eq!(header.padding, [0; 3]);

        let encoded: VirtioGpuNativeAhbDescription = mem
            .read_obj_from_addr(GuestAddress(
                0x100 + size_of::<virtio_gpu_ctrl_hdr>() as u64,
            ))
            .unwrap();
        assert_eq!(encoded, wire);
        assert_eq!(encoded.allocation_size.to_native(), 0x12_3000);
        assert_eq!(encoded.plane_stride.to_native(), 12_288);
    }

    #[test]
    fn native_ahb_response_refuses_short_buffer_without_nodata_fallback() {
        let response = GpuResponse::OkNativeAhbDescription(native_description().into());
        let mem = GuestMemory::new(&[(GuestAddress(0), 0x1000)]).unwrap();
        let mut writer = Writer::new_from_regions(
            &mem,
            smallvec![MemRegion {
                offset: 0x100,
                len: 87,
            }],
        );
        assert!(matches!(
            response.encode(0, 0, 0, 0, &mut writer),
            Err(GpuResponseEncodeError::NativeAhbDescriptionBufferTooSmall { available: 87 })
        ));
        let first: u8 = mem.read_obj_from_addr(GuestAddress(0x100)).unwrap();
        assert_eq!(first, 0);
    }

    #[test]
    fn native_ahb_v2_request_has_unambiguous_fixed_layout() {
        assert_eq!(
            size_of::<virtio_gpu_ctrl_hdr>()
                + size_of::<virtio_gpu_resource_create_native_ahb_body>(),
            52
        );
        assert_eq!(VIRTIO_GPU_CMD_RESOURCE_CREATE_NATIVE_AHB, 0xd211);
        assert_eq!(VIRTIO_GPU_NATIVE_AHB_REQUEST_VERSION, 2);
        assert_eq!(VIRTIO_GPU_NATIVE_AHB_REQUEST_SIZE, 24);
    }
}
