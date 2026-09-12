// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license.

//! DVSA v1 discovery and allocation/mapping primitives. No command grants a
//! render/present lease; ACQUIRE/PRESENT and full SDR feature bit0 stay off.
use data_model::{Le32, Le64};
use vm_control::shared_allocation::{discovery_unavailable, DynamicMappingWindow};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
use super::control_header::virtio_gpu_ctrl_hdr;

pub const CMD_DISCOVER_SHARED_ALLOCATION: u32 = 0xd110;
pub const RESP_SHARED_ALLOCATION: u32 = 0xd210;
pub const SHARED_ALLOCATION_MAGIC: u32 = 0x41535644;
pub const CMD_ALLOCATE_SHARED_ALLOCATION: u32 = 0xd111;
pub const CMD_DESTROY_SHARED_ALLOCATION: u32 = 0xd114;
pub const CMD_ACK_SHARED_ALLOCATION: u32 = 0xd115;
pub const RESP_ALLOCATED_SHARED_ALLOCATION: u32 = 0xd211;
pub const CMD_QUERY_SHARED_OWNER: u32 = 0xd116;
pub const CMD_ALLOCATE_RECOVERABLE: u32 = 0xd117;
pub const CMD_CLEANUP_SHARED_OWNER: u32 = 0xd118;
pub const RESP_SHARED_OWNER: u32 = 0xd216;
pub const OWNER_SESSION: u32 = 1;
pub const OWNER_RETAINED: u32 = 2;
pub const OWNER_RELEASED: u32 = 3;
pub const OWNER_UNKNOWN: u32 = 4;
pub const FEATURE_ALLOCATION_MAPPING: u64 = 1 << 1;

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct SharedAllocationRequest {
    pub query: SharedAllocationHeader,
    pub width: Le32, pub height: Le32, pub fourcc: Le32, pub flags: Le32,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct SharedAllocationDescription {
    pub buffer_id: Le64, pub allocation_size: Le64, pub modifier: Le64, pub plane_offset: Le64,
    pub plane_stride: Le32, pub width: Le32, pub height: Le32, pub fourcc: Le32,
    pub plane_count: Le32, pub layout_flags: Le32, pub reserved: Le64,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct SharedAllocationResponse {
    pub query: SharedAllocationHeader,
    pub description: SharedAllocationDescription,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct SharedAllocationAck {
    pub query: SharedAllocationHeader,
    pub bar_offset: Le64, pub mapped_size: Le64,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct SharedOwnerRequest {
    pub query: SharedAllocationHeader,
    pub host_epoch: [Le64; 2],
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct RecoverableAllocationRequest {
    pub allocation: SharedAllocationRequest,
    pub host_epoch: [Le64; 2],
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct SharedOwnerResponse {
    pub query: SharedAllocationHeader,
    pub host_epoch: [Le64; 2],
    pub state: Le32, pub owner_context: Le32,
    pub allocation_size: Le64, pub bar_offset: Le64, pub mapped_size: Le64,
    pub reserved: [Le64; 2],
}

const _: () = assert!(std::mem::size_of::<SharedOwnerRequest>() == 80);
const _: () = assert!(std::mem::size_of::<RecoverableAllocationRequest>() == 96);
const _: () = assert!(std::mem::size_of::<SharedOwnerResponse>() == 128);

impl SharedOwnerRequest {
    pub fn is_session(&self) -> bool {
        let mut query = self.query;
        if query.hdr.type_.to_native() != CMD_QUERY_SHARED_OWNER || query.size.to_native() != 80
            || query.hdr.flags.to_native() != 0 { return false; }
        query.hdr.type_ = CMD_DISCOVER_SHARED_ALLOCATION.into();
        query.size = 64.into();
        query.valid_discovery()
    }

    pub fn valid_owner(&self, command: u32) -> bool {
        self.query.valid_mutation(command, 80, self.query.token.to_native() == 0)
    }
}

impl SharedOwnerResponse {
    pub fn encode(mut self, mut hdr: virtio_gpu_ctrl_hdr, out: &mut impl std::io::Write)
        -> std::io::Result<usize> {
        hdr.type_ = RESP_SHARED_OWNER.into();
        hdr.padding = [0; 3];
        self.query.hdr = hdr;
        out.write_all(self.as_bytes())?;
        Ok(std::mem::size_of::<Self>())
    }
}

const _: () = assert!(std::mem::size_of::<SharedAllocationRequest>() == 80);
const _: () = assert!(std::mem::size_of::<SharedAllocationResponse>() == 128);
const _: () = assert!(std::mem::size_of::<SharedAllocationAck>() == 80);

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct SharedAllocationHeader {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub magic: Le32, pub version: Le32, pub size: Le32, pub scanout_id: Le32,
    pub generation: Le64, pub token: Le64, pub resource_id: Le32, pub reserved: Le32,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct SharedAllocationDiscovery {
    pub query: SharedAllocationHeader,
    pub feature_bits: Le64, pub unavailable_reasons: Le64,
    pub bar_size: Le64, pub reserved_prefix: Le64, pub dynamic_capacity: Le64,
    pub mapping_alignment: Le64,
    pub min_allocations: Le32, pub max_allocations: Le32,
    pub triple_bytes_lower_bound: Le64,
}

const _: () = assert!(std::mem::size_of::<SharedAllocationHeader>() == 64);
const _: () = assert!(std::mem::size_of::<SharedAllocationDiscovery>() == 128);

impl SharedAllocationHeader {
    pub fn valid_mutation(&self, command: u32, size: u32, allocating: bool) -> bool {
        self.magic.to_native() == SHARED_ALLOCATION_MAGIC && self.version.to_native() == 1
            && self.size.to_native() == size && self.scanout_id.to_native() == 0
            && self.generation.to_native() != 0 && (self.token.to_native() == 0) == allocating
            && self.resource_id.to_native() != 0 && self.reserved.to_native() == 0
            && self.hdr.type_.to_native() == command && self.hdr.ctx_id.to_native() != 0
            && self.hdr.ring_idx == 0 && self.hdr.padding == [0; 3]
            && self.hdr.flags.to_native() == 0 && self.hdr.fence_id.to_native() == 0
    }

    pub fn valid_discovery(&self) -> bool {
        self.magic.to_native() == SHARED_ALLOCATION_MAGIC && self.version.to_native() == 1
            && self.size.to_native() == 64 && self.scanout_id.to_native() == 0
            && self.generation.to_native() == 0 && self.token.to_native() == 0
            && self.resource_id.to_native() == 0 && self.reserved.to_native() == 0
            && self.hdr.type_.to_native() == CMD_DISCOVER_SHARED_ALLOCATION
            && self.hdr.ctx_id.to_native() == 0 && self.hdr.ring_idx == 0
            && self.hdr.padding == [0; 3] && self.hdr.flags.to_native() & !1 == 0
            && (self.hdr.flags.to_native() == 1 || self.hdr.fence_id.to_native() == 0)
    }
}

impl SharedAllocationDiscovery {
    /// This is the production response encoding path. The frontend supplies
    /// ordinary fence metadata; padding and response type are host-owned.
    pub fn encode(mut self, mut hdr: virtio_gpu_ctrl_hdr, out: &mut impl std::io::Write)
        -> std::io::Result<usize> {
        hdr.type_ = RESP_SHARED_ALLOCATION.into();
        hdr.padding = [0; 3];
        self.query.hdr = hdr;
        out.write_all(self.as_bytes())?;
        Ok(std::mem::size_of::<Self>())
    }

    pub fn from_host(mut query: SharedAllocationHeader, window: Option<DynamicMappingWindow>,
        generation: u64, width: u32, height: u32) -> Self {
        let (unavailable, lower_bound) = discovery_unavailable(window, generation, width, height);
        query.size = 128.into();
        query.generation = generation.into();
        let absent = DynamicMappingWindow { bar_size: 0, reserved_prefix: 0, alignment: 0 };
        let window = window.unwrap_or(absent);
        Self { query, feature_bits: 0.into(), unavailable_reasons: unavailable.into(),
            bar_size: window.bar_size.into(), reserved_prefix: window.reserved_prefix.into(),
            dynamic_capacity: window.available_bytes().into(), mapping_alignment: window.alignment.into(),
            min_allocations: 3.into(), max_allocations: 0.into(),
            triple_bytes_lower_bound: lower_bound.into() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_mutations_bind_context_generation_token_and_exact_framing() {
        let mut query = SharedAllocationHeader::default();
        query.hdr.type_ = CMD_ALLOCATE_SHARED_ALLOCATION.into(); query.hdr.ctx_id = 81.into();
        query.magic = SHARED_ALLOCATION_MAGIC.into(); query.version = 1.into(); query.size = 80.into();
        query.generation = 17.into(); query.resource_id = 91.into();
        assert!(query.valid_mutation(CMD_ALLOCATE_SHARED_ALLOCATION, 80, true));
        for offset in [0, 4, 8, 20, 21, 22, 23, 24, 28, 32, 36, 48, 60] {
            let mut bytes = query.as_bytes().to_vec(); bytes[offset] ^= 0x80;
            assert!(!SharedAllocationHeader::read_from_bytes(&bytes).unwrap()
                .valid_mutation(CMD_ALLOCATE_SHARED_ALLOCATION, 80, true));
        }
        let mut zero_context = query; zero_context.hdr.ctx_id = 0.into();
        assert!(!zero_context.valid_mutation(CMD_ALLOCATE_SHARED_ALLOCATION, 80, true));
        let mut zero_generation = query; zero_generation.generation = 0.into();
        assert!(!zero_generation.valid_mutation(CMD_ALLOCATE_SHARED_ALLOCATION, 80, true));
        query.hdr.type_ = CMD_ACK_SHARED_ALLOCATION.into();
        assert!(!query.valid_mutation(CMD_ACK_SHARED_ALLOCATION, 80, false));
        query.token = 21.into();
        assert!(query.valid_mutation(CMD_ACK_SHARED_ALLOCATION, 80, false));
        query.hdr.type_ = CMD_DESTROY_SHARED_ALLOCATION.into(); query.size = 64.into();
        assert!(query.valid_mutation(CMD_DESTROY_SHARED_ALLOCATION, 64, false));
        assert!(!query.valid_mutation(CMD_DESTROY_SHARED_ALLOCATION, 80, false));
    }

    #[test]
    fn production_encoder_preserves_fence_identity_and_propagates_short_output() {
        let mut query = SharedAllocationHeader::default();
        query.magic = SHARED_ALLOCATION_MAGIC.into(); query.version = 1.into();
        let response = SharedAllocationDiscovery::from_host(query, None, 0, 0, 0);
        let hdr = virtio_gpu_ctrl_hdr {
            type_: 0xdead.into(), flags: 1.into(), fence_id: 0x0102030405060708.into(),
            ctx_id: 9.into(), ring_idx: 3, padding: [0xff; 3],
        };
        let mut bytes = Vec::new();
        assert_eq!(response.encode(hdr, &mut bytes).unwrap(), 128);
        let actual = SharedAllocationDiscovery::read_from_bytes(&bytes).unwrap();
        assert_eq!(actual.query.hdr.type_.to_native(), RESP_SHARED_ALLOCATION);
        assert_eq!(actual.query.hdr.flags.to_native(), 1);
        assert_eq!(actual.query.hdr.fence_id.to_native(), 0x0102030405060708);
        assert_eq!(actual.query.hdr.ctx_id.to_native(), 9);
        assert_eq!(actual.query.hdr.ring_idx, 3);
        assert_eq!(actual.query.hdr.padding, [0; 3]);
        let mut short = [0u8; 127];
        assert_eq!(response.encode(hdr, &mut short.as_mut_slice()).unwrap_err().kind(),
            std::io::ErrorKind::WriteZero);
    }

    #[test]
    fn wire_identity_and_reserved_fields_are_strict() {
        let mut query = SharedAllocationHeader::default();
        query.hdr.type_ = CMD_DISCOVER_SHARED_ALLOCATION.into();
        query.magic = SHARED_ALLOCATION_MAGIC.into(); query.version = 1.into(); query.size = 64.into();
        assert!(query.valid_discovery());
        let bytes = query.as_bytes();
        assert_eq!(&bytes[24..28], b"DVSA");
        assert!(SharedAllocationHeader::read_from_bytes(&bytes[..63]).is_err());
        for offset in [0, 4, 8, 16, 20, 21, 22, 23, 24, 28, 32, 36, 40, 48, 56, 60] {
            let mut bad = bytes.to_vec(); bad[offset] ^= 0x80;
            assert!(!SharedAllocationHeader::read_from_bytes(&bad).unwrap().valid_discovery());
        }
        let response = SharedAllocationDiscovery::from_host(query, None, 0, 0, 0);
        assert_eq!(response.feature_bits.to_native(), 0);
        assert_eq!(response.max_allocations.to_native(), 0);
        assert_eq!(response.query.size.to_native(), 128);
        assert_eq!(response.as_bytes().len(), 128);
    }
}
