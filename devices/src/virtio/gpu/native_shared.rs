// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license.

//! Actual host AHB owner -> checked native import -> independent guest map.
//! No render/present admission: Rutabaga forbids every submission in a context
//! holding one of these imports until checked unmap and native retirement.
use super::*;
use gpu_display::shared_allocation::{HostSharedAllocation, SharedAllocationDescription as NativeDescription};
use vm_control::native_allocation::{NativeAllocationIdentity, NativeAllocationMapLedger};
use vm_control::shared_allocation::{DynamicMappingWindow, ExternalMappingFailure};
use crate::virtio::gpu::shared_allocation_protocol::{
    SharedAllocationRequest, SharedAllocationResponse, SharedAllocationDescription,
    SharedAllocationAck, CMD_ALLOCATE_SHARED_ALLOCATION, CMD_ACK_SHARED_ALLOCATION,
    CMD_DESTROY_SHARED_ALLOCATION,
    SharedOwnerRequest, SharedOwnerResponse, RecoverableAllocationRequest,
    CMD_QUERY_SHARED_OWNER, CMD_CLEANUP_SHARED_OWNER, CMD_ALLOCATE_RECOVERABLE,
    OWNER_SESSION, OWNER_UNKNOWN, OWNER_RETAINED, OWNER_RELEASED,
};
use rand::RngCore;

const MAX_ALLOCATIONS: usize = 3;
// Tombstones are never evicted within an incarnation. Fail before side effects
// when full; bounded diagnostic allocation is not an unbounded resource service.
const MAX_RECOVERY_RECORDS: usize = 4096;

#[derive(Clone, Copy)]
struct RecoveryRecord {
    identity: NativeAllocationIdentity,
    released: bool,
}

struct Owner {
    identity: NativeAllocationIdentity,
    allocation: Option<Box<dyn HostSharedAllocation>>,
    description: NativeDescription,
    renderer_imported: bool,
    map_info: u32,
}

impl Drop for Owner {
    fn drop(&mut self) {
        if let Some(allocation) = self.allocation.take() {
            // Device teardown/reset is not an acknowledgement from KGSL or
            // Gunyah. Unproven owners stay alive until process termination.
            std::mem::forget(allocation);
        }
    }
}

pub(super) struct NativeAllocationOwners {
    ledger: NativeAllocationMapLedger,
    owners: Map<u32, Owner>,
    host_epoch: [u64; 2],
    recovery: Map<u32, RecoveryRecord>,
}

impl Default for NativeAllocationOwners {
    fn default() -> Self {
        let mut bytes = [0u8; 16];
        // A fresh object may have an empty journal while old Drop-leaked AHBs
        // still exist. Never use reset counters or empty maps as lifetime proof.
        let host_epoch = if rand::rngs::OsRng.try_fill_bytes(&mut bytes).is_ok() {
            [u64::from_le_bytes(bytes[..8].try_into().unwrap()),
             u64::from_le_bytes(bytes[8..].try_into().unwrap())]
        } else { [0; 2] };
        Self { ledger: NativeAllocationMapLedger::new(MAX_ALLOCATIONS), owners: Map::new(),
            host_epoch, recovery: Map::new() }
    }
}

impl NativeAllocationOwners {
    pub(super) fn contains(&self, resource_id: u32) -> bool { self.owners.contains_key(&resource_id) }
    pub(super) fn is_empty(&self) -> bool { self.owners.is_empty() }
}

impl VirtioGpu {
    fn owner_epoch_matches(&self, epoch: [data_model::Le64; 2]) -> bool {
        self.native_allocations.host_epoch != [0; 2] &&
            epoch.map(|part| part.to_native()) == self.native_allocations.host_epoch
    }

    fn shared_owner_response(&self, mut query: SharedAllocationHeader, state: u32)
        -> VirtioGpuResult {
        query.size = 128.into();
        let mut response = SharedOwnerResponse {
            query, host_epoch: self.native_allocations.host_epoch.map(Into::into),
            state: state.into(), owner_context: query.hdr.ctx_id,
            ..Default::default()
        };
        if let Some(record) = self.native_allocations.recovery.get(&query.resource_id.to_native()) {
            response.query.token = record.identity.token.into();
            if let Some(owner) = self.native_allocations.owners.get(&record.identity.resource_id) {
                response.allocation_size = owner.description.allocation_size.into();
                if let Some(receipt) = self.native_allocations.ledger.mapping_receipt(record.identity) {
                    response.bar_offset = receipt.offset.into();
                    response.mapped_size = receipt.size.into();
                }
            }
        }
        Ok(OkSharedOwner(response))
    }

    /// QUERY is observational. CLEANUP returns an authoritative terminal receipt
    /// only after real retirement, or sealing a request that never allocated.
    pub fn recover_shared_owner(&mut self, request: SharedOwnerRequest, cleanup: bool)
        -> VirtioGpuResult {
        if !cleanup && request.is_session() {
            if self.native_allocations.host_epoch == [0; 2] ||
                (request.host_epoch.map(|part| part.to_native()) != [0; 2] &&
                 !self.owner_epoch_matches(request.host_epoch)) {
                return Err(ErrInvalidParameter);
            }
            return self.shared_owner_response(request.query, OWNER_SESSION);
        }
        let command = if cleanup { CMD_CLEANUP_SHARED_OWNER } else { CMD_QUERY_SHARED_OWNER };
        if !request.valid_owner(command) || !self.owner_epoch_matches(request.host_epoch) {
            return Err(ErrInvalidParameter);
        }
        let wanted = Self::shared_identity(request.query);
        let record = self.native_allocations.recovery.get(&wanted.resource_id).copied();
        let Some(record) = record else {
            if !cleanup { return self.shared_owner_response(request.query, OWNER_UNKNOWN); }
            // No guessed token or absence-as-release. Install a terminal seal
            // before replying, so even a delayed tracked/legacy ALLOCATE fails.
            if wanted.token != 0 || self.native_allocations.recovery.len() >= MAX_RECOVERY_RECORDS
                || self.native_allocations.ledger.contains_resource(wanted.resource_id)
                || self.native_allocations.owners.contains_key(&wanted.resource_id)
                || self.resources.contains_key(&wanted.resource_id) {
                return Err(ErrInvalidParameter);
            }
            self.native_allocations.recovery.insert(wanted.resource_id,
                RecoveryRecord { identity: wanted, released: true });
            return self.shared_owner_response(request.query, OWNER_RELEASED);
        };
        if record.identity.context_id != wanted.context_id ||
            record.identity.surface_generation != wanted.surface_generation ||
            (wanted.token != 0 && record.identity.token != wanted.token) {
            return Err(ErrInvalidParameter);
        }
        if record.released { return self.shared_owner_response(request.query, OWNER_RELEASED); }
        if cleanup {
            // Mapping receipt and renderer identity remain host-owned. A
            // transient KGSL/Gunyah failure returns RETAINED and can be retried.
            if self.native_allocations.ledger.mapping_receipt(record.identity).is_some() &&
                self.unmap_native_allocation(wanted.resource_id).is_err() {
                return self.shared_owner_response(request.query, OWNER_RETAINED);
            }
            if self.retire_native_allocation(record.identity).is_ok() {
                return self.shared_owner_response(request.query, OWNER_RELEASED);
            }
        }
        self.shared_owner_response(request.query, OWNER_RETAINED)
    }

    pub fn allocate_recoverable(&mut self, mut request: RecoverableAllocationRequest)
        -> VirtioGpuResult {
        if !request.allocation.query.valid_mutation(CMD_ALLOCATE_RECOVERABLE, 96, true) ||
            !self.owner_epoch_matches(request.host_epoch) { return Err(ErrInvalidParameter); }
        // Framing was validated above. Reuse the exact allocation implementation
        // while recording ownership before any external allocation/import.
        request.allocation.query.hdr.type_ = CMD_ALLOCATE_SHARED_ALLOCATION.into();
        request.allocation.query.size = 80.into();
        self.allocate_shared_allocation_impl(request.allocation, true)
    }

    fn native_generation(&self) -> u64 {
        self.display.borrow().color_capabilities()
            .filter(|caps| caps.valid() && caps.display_id >= 0)
            .map(|caps| caps.generation).unwrap_or(0)
    }

    pub(super) fn native_allocation_window(&self) -> Option<DynamicMappingWindow> {
        let window = self.mapper.lock().as_ref()?.external_mapping_window()?;
        let scanout = self.scanouts.get(&0)?;
        let (unavailable, _) = vm_control::shared_allocation::discovery_unavailable(
            Some(window), self.native_generation(), scanout.width, scanout.height);
        if unavailable & 0xf != 0 || !self.rutabaga.host_dmabuf_import_supported() {
            return None;
        }
        Some(window)
    }

    fn shared_identity(query: SharedAllocationHeader) -> NativeAllocationIdentity {
        NativeAllocationIdentity { surface_generation: query.generation.to_native(),
            token: query.token.to_native(), context_id: query.hdr.ctx_id.to_native(),
            resource_id: query.resource_id.to_native() }
    }

    /// Called before dropping any AHB or making its resource-id reusable.
    fn retire_native_allocation(&mut self, identity: NativeAllocationIdentity) -> VirtioGpuResult {
        let owner = self.native_allocations.owners.get(&identity.resource_id).ok_or(ErrInvalidResourceId)?;
        if owner.identity != identity || !self.native_allocations.ledger.can_retire_unmapped(identity) {
            return Err(ErrInvalidParameter);
        }
        if owner.renderer_imported {
            self.rutabaga.unref_host_dmabuf(identity.resource_id)?;
            self.native_allocations.owners.get_mut(&identity.resource_id).unwrap().renderer_imported = false;
        }
        if !self.native_allocations.ledger.retire_unmapped(identity) { return Err(ErrUnspec); }
        if let Some(mut owner) = self.native_allocations.owners.remove(&identity.resource_id) {
            // Both real owners retired; this is the only ordinary AHB release.
            drop(owner.allocation.take());
        }
        if let Some(record) = self.native_allocations.recovery.get_mut(&identity.resource_id) {
            record.released = true;
        }
        Ok(OkNoData)
    }

    pub fn allocate_shared_allocation(&mut self, request: SharedAllocationRequest) -> VirtioGpuResult {
        self.allocate_shared_allocation_impl(request, false)
    }

    fn allocate_shared_allocation_impl(&mut self, request: SharedAllocationRequest, recoverable: bool)
        -> VirtioGpuResult {
        if !request.query.valid_mutation(CMD_ALLOCATE_SHARED_ALLOCATION, 80, true)
            || request.flags.to_native() != 0 || request.fourcc.to_native() != DRM_FORMAT_ABGR8888 {
            return Err(ErrInvalidParameter);
        }
        let generation = self.native_generation();
        let window = self.native_allocation_window().ok_or(ErrInvalidParameter)?;
        let (width, height) = (request.width.to_native(), request.height.to_native());
        let scanout = self.scanouts.get(&0).ok_or(ErrInvalidScanoutId)?;
        let resource_id = request.query.resource_id.to_native();
        if request.query.generation.to_native() != generation || width != scanout.width
            || height != scanout.height || self.resources.contains_key(&resource_id)
            || self.native_allocations.recovery.contains_key(&resource_id)
            || (recoverable && self.native_allocations.recovery.len() >= MAX_RECOVERY_RECORDS) {
            return Err(ErrInvalidParameter);
        }
        let identity = self.native_allocations.ledger.reserve(generation,
            request.query.hdr.ctx_id.to_native(), resource_id).ok_or(ErrOutOfMemory)?;
        if recoverable {
            self.native_allocations.recovery.insert(resource_id, RecoveryRecord { identity, released: false });
        }
        let allocated = self.display.borrow_mut().allocate_shared_buffer(width, height);
        let allocation = match allocated {
            Ok(value) => value,
            Err(error) => {
                if self.native_allocations.ledger.retire_unmapped(identity) {
                    if let Some(record) = self.native_allocations.recovery.get_mut(&resource_id) {
                        record.released = true;
                    }
                }
                return Err(error.into());
            }
        };
        let description = allocation.description();
        if !description.valid(width, height) || !window.permits(window.reserved_prefix, description.allocation_size)
            || description.allocation_size.checked_mul(MAX_ALLOCATIONS as u64)
                .map_or(true, |size| size > window.available_bytes())
            || self.native_allocations.owners.values().any(|owner|
                owner.description.buffer_id == description.buffer_id) {
            if self.native_allocations.ledger.retire_unmapped(identity) {
                if let Some(record) = self.native_allocations.recovery.get_mut(&resource_id) {
                    record.released = true;
                }
            }
            return Err(ErrInvalidParameter);
        }
        self.native_allocations.owners.insert(resource_id, Owner {
            identity, allocation: Some(allocation), description, renderer_imported: false, map_info: 0,
        });
        let imported: VirtioGpuResult = (|| {
            let fd = self.native_allocations.owners[&resource_id].allocation.as_ref().unwrap()
                .duplicate_descriptor().map_err(|_| ErrUnspec)?;
            // SAFETY: exactly one owned descriptor moves to the renderer.
            let handle = RutabagaHandle { os_handle: unsafe {
                RutabagaDescriptor::from_raw_descriptor(fd.into_raw_descriptor())
            }, handle_type: RUTABAGA_HANDLE_TYPE_MEM_DMABUF };
            self.rutabaga.resource_import_host_dmabuf(resource_id, description.allocation_size, handle)?;
            self.native_allocations.owners.get_mut(&resource_id).unwrap().renderer_imported = true;
            let attached = self.rutabaga.context_attach_host_dmabuf(identity.context_id, resource_id)?;
            if attached.ctx_id != identity.context_id || attached.resource_id != resource_id
                || attached.allocation_size != description.allocation_size {
                return Err(ErrInvalidParameter);
            }
            self.native_allocations.owners.get_mut(&resource_id).unwrap().map_info = attached.map_info;
            if !self.native_allocations.ledger.imported(identity, description.allocation_size) {
                return Err(ErrUnspec);
            }
            Ok(OkNoData)
        })();
        if let Err(error) = imported {
            // Failed checked release deliberately retains quota/AHB/context.
            let _ = self.retire_native_allocation(identity);
            return Err(error);
        }
        let mut query = request.query;
        query.token = identity.token.into(); query.size = 128.into();
        Ok(OkAllocatedSharedAllocation(SharedAllocationResponse { query,
            description: SharedAllocationDescription {
                buffer_id: description.buffer_id.into(), allocation_size: description.allocation_size.into(),
                modifier: description.modifier.into(), plane_offset: description.plane_offset.into(),
                plane_stride: description.plane_stride.into(), width: width.into(), height: height.into(),
                fourcc: description.fourcc.into(), plane_count: description.plane_count.into(),
                layout_flags: description.layout_flags.into(), reserved: 0.into(),
            } }))
    }

    pub(super) fn map_native_allocation(&mut self, resource_id: u32, offset: u64) -> VirtioGpuResult {
        let generation = self.native_generation();
        let owner = self.native_allocations.owners.get(&resource_id).ok_or(ErrInvalidResourceId)?;
        let identity = owner.identity;
        let fd = owner.allocation.as_ref().unwrap().duplicate_descriptor().map_err(|_| ErrUnspec)?;
        let size = owner.description.allocation_size;
        let map_info = owner.map_info;
        let cache = if map_info == RUTABAGA_MAP_CACHE_CACHED {
            MemCacheType::CacheCoherent
        } else if map_info == rutabaga_gfx::RUTABAGA_MAP_CACHE_WC {
            MemCacheType::CacheNonCoherent
        } else { return Err(ErrInvalidParameter); };
        let mut mapping = self.mapper.lock();
        let mapper = mapping.as_mut().ok_or(ErrUnspec)?;
        let window = mapper.external_mapping_window().ok_or(ErrInvalidParameter)?;
        if !self.native_allocations.ledger.begin_mapping(identity, generation, window, offset) {
            return Err(ErrInvalidParameter);
        }
        let receipt = match mapper.add_external_mapping(VmMemorySource::Descriptor {
            descriptor: fd, offset: 0, size,
        }, offset, Protection::read_write(), cache) {
            Ok(receipt) => receipt,
            Err(error) => {
                self.native_allocations.ledger.mapping_failed(identity,
                    error.downcast_ref::<ExternalMappingFailure>().map(|failure| failure.receipt));
                error!("DVSA map retained owner: {:#}", error);
                return Err(ErrUnspec);
            }
        };
        if !self.native_allocations.ledger.mapped(identity, receipt) {
            self.native_allocations.ledger.mapping_failed(identity, Some(receipt));
            return Err(ErrUnspec);
        }
        Ok(OkMapInfo { map_info, pool_offset: None })
    }

    pub fn acknowledge_shared_allocation(&mut self, request: SharedAllocationAck) -> VirtioGpuResult {
        if !request.query.valid_mutation(CMD_ACK_SHARED_ALLOCATION, 80, false) {
            return Err(ErrInvalidParameter);
        }
        let identity = Self::shared_identity(request.query);
        let generation = self.native_generation();
        let receipt = self.native_allocations.ledger.mapping_receipt(identity).ok_or(ErrInvalidParameter)?;
        if receipt.offset != request.bar_offset.to_native() || receipt.size != request.mapped_size.to_native() {
            return Err(ErrInvalidParameter);
        }
        let mapper_live = self.mapper.lock().as_ref().is_some_and(|mapper| mapper.external_mapping_live(receipt));
        if !self.native_allocations.ledger.acknowledge_mapping(identity, generation, receipt, mapper_live) {
            return Err(ErrInvalidParameter);
        }
        Ok(OkNoData)
    }

    pub(super) fn unmap_native_allocation(&mut self, resource_id: u32) -> VirtioGpuResult {
        let identity = self.native_allocations.owners.get(&resource_id).ok_or(ErrInvalidResourceId)?.identity;
        let receipt = self.native_allocations.ledger.begin_unmap(identity).ok_or(ErrInvalidParameter)?;
        self.mapper.lock().as_mut().ok_or(ErrUnspec)?.remove_external_mapping(receipt)
            .map_err(|error| { error!("DVSA unmap retained owner: {:#}", error); ErrUnspec })?;
        if !self.native_allocations.ledger.unmapped(identity, receipt) { return Err(ErrUnspec); }
        Ok(OkNoData)
    }

    pub fn destroy_shared_allocation(&mut self, request: SharedAllocationHeader) -> VirtioGpuResult {
        if !request.valid_mutation(CMD_DESTROY_SHARED_ALLOCATION, 64, false) {
            return Err(ErrInvalidParameter);
        }
        self.retire_native_allocation(Self::shared_identity(request))
    }
}
