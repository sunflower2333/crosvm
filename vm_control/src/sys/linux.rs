// Copyright 2022 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#[cfg(feature = "gpu")]
pub(crate) mod gpu;

use std::env;
use std::path::Path;
use std::time::Duration;

use base::error;
use base::AsRawDescriptor;
use base::Descriptor;
use base::Error as SysError;
use base::MappedRegion;
use base::MemoryMappingArena;
use base::MmapError;
use base::Protection;
use base::SafeDescriptor;
use base::Tube;
use base::UnixSeqpacket;
use hypervisor::MemCacheType;
use hypervisor::MemSlot;
use hypervisor::Vm;
use libc::EINVAL;
use libc::ERANGE;
use once_cell::sync::Lazy;
use resources::Alloc;
use resources::SystemAllocator;
use serde::Deserialize;
use serde::Serialize;
use vm_memory::GuestAddress;

use crate::client::HandleRequestResult;
use crate::VmMappedMemoryRegion;
use crate::VmRequest;
use crate::VmResponse;

pub fn handle_request<T: AsRef<Path> + std::fmt::Debug>(
    request: &VmRequest,
    socket_path: T,
) -> HandleRequestResult {
    handle_request_with_timeout(request, socket_path, None)
}

pub fn handle_request_with_timeout<T: AsRef<Path> + std::fmt::Debug>(
    request: &VmRequest,
    socket_path: T,
    timeout: Option<Duration>,
) -> HandleRequestResult {
    match UnixSeqpacket::connect(&socket_path) {
        Ok(s) => {
            let socket = Tube::try_from(s).map_err(|_| ())?;
            if timeout.is_some() {
                if let Err(e) = socket.set_recv_timeout(timeout) {
                    error!(
                        "failed to set recv timeout on socket at '{:?}': {}",
                        socket_path, e
                    );
                    return Err(());
                }
            }
            if let Err(e) = socket.send(request) {
                error!(
                    "failed to send request to socket at '{:?}': {}",
                    socket_path, e
                );
                return Err(());
            }
            match socket.recv() {
                Ok(response) => Ok(response),
                Err(e) => {
                    error!(
                        "failed to recv response from socket at '{:?}': {}",
                        socket_path, e
                    );
                    Err(())
                }
            }
        }
        Err(e) => {
            error!("failed to connect to socket at '{:?}': {}", socket_path, e);
            Err(())
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub enum VmMemoryMappingRequest {
    /// Flush the content of a memory mapping to its backing file.
    /// `slot` selects the arena (as returned by `Vm::add_mmap_arena`).
    /// `offset` is the offset of the mapping to sync within the arena.
    /// `size` is the size of the mapping to sync within the arena.
    MsyncArena {
        slot: MemSlot,
        offset: usize,
        size: usize,
    },

    /// Gives a MADV_PAGEOUT advice to the memory region mapped at `slot`, with the address range
    /// starting at `offset` from the start of the region, and with size `size`.
    MadvisePageout {
        slot: MemSlot,
        offset: usize,
        size: usize,
    },

    /// Gives a MADV_REMOVE advice to the memory region mapped at `slot`, with the address range
    /// starting at `offset` from the start of the region, and with size `size`.
    MadviseRemove {
        slot: MemSlot,
        offset: usize,
        size: usize,
    },
}

#[derive(Serialize, Deserialize, Debug)]
pub enum VmMemoryMappingResponse {
    Ok,
    Err(SysError),
}

impl VmMemoryMappingRequest {
    /// Executes this request on the given Vm.
    ///
    /// # Arguments
    /// * `vm` - The `Vm` to perform the request on.
    ///
    /// This does not return a result, instead encapsulating the success or failure in a
    /// `VmMsyncResponse` with the intended purpose of sending the response back over the socket
    /// that received this `VmMsyncResponse`.
    pub fn execute(&self, vm: &mut impl Vm) -> VmMemoryMappingResponse {
        use self::VmMemoryMappingRequest::*;
        match *self {
            MsyncArena { slot, offset, size } => match vm.msync_memory_region(slot, offset, size) {
                Ok(()) => VmMemoryMappingResponse::Ok,
                Err(e) => VmMemoryMappingResponse::Err(e),
            },
            MadvisePageout { slot, offset, size } => {
                match vm.madvise_pageout_memory_region(slot, offset, size) {
                    Ok(()) => VmMemoryMappingResponse::Ok,
                    Err(e) => VmMemoryMappingResponse::Err(e),
                }
            }
            MadviseRemove { slot, offset, size } => {
                match vm.madvise_remove_memory_region(slot, offset, size) {
                    Ok(()) => VmMemoryMappingResponse::Ok,
                    Err(e) => VmMemoryMappingResponse::Err(e),
                }
            }
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub enum FsMappingRequest {
    /// Create an anonymous memory mapping that spans the entire region described by `Alloc`.
    AllocateSharedMemoryRegion(Alloc),
    /// Create a memory mapping.
    CreateMemoryMapping {
        /// The slot for a MemoryMappingArena, previously returned by a response to an
        /// `AllocateSharedMemoryRegion` request.
        slot: u32,
        /// The file descriptor that should be mapped.
        fd: SafeDescriptor,
        /// The size of the mapping.
        size: usize,
        /// The offset into the file from where the mapping should start.
        file_offset: u64,
        /// The memory protection to be used for the mapping.  Protections other than readable and
        /// writable will be silently dropped.
        prot: Protection,
        /// The offset into the shared memory region where the mapping should be placed.
        mem_offset: usize,
    },
    /// Remove a memory mapping.
    RemoveMemoryMapping {
        /// The slot for a MemoryMappingArena.
        slot: u32,
        /// The offset into the shared memory region.
        offset: usize,
        /// The size of the mapping.
        size: usize,
    },
}

pub fn prepare_shared_memory_region(
    vm: &mut dyn Vm,
    allocator: &mut SystemAllocator,
    alloc: Alloc,
    cache: MemCacheType,
    source: Option<Box<dyn MappedRegion>>,
) -> Result<VmMappedMemoryRegion, SysError> {
    prepare_shared_memory_region_at_offset(vm, allocator, alloc, cache, 0, source)
}

/// Prepare a fixed mapping for a subrange of an allocated PCI BAR. The allocation remains the
/// BAR-sized address range, while only `[guest_offset, guest_offset + source.size())` is installed
/// in the hypervisor. This is used for the drm2kgsl BAR guard, whose first 2 MiB are intentionally
/// left unmapped.
pub fn prepare_shared_memory_region_at_offset(
    vm: &mut dyn Vm,
    allocator: &mut SystemAllocator,
    alloc: Alloc,
    cache: MemCacheType,
    guest_offset: u64,
    source: Option<Box<dyn MappedRegion>>,
) -> Result<VmMappedMemoryRegion, SysError> {
    if !matches!(alloc, Alloc::PciBar { .. }) {
        return Err(SysError::new(EINVAL));
    }
    match allocator.mmio_allocator_any().get(&alloc) {
        Some((range, _)) => {
            let allocation_size: u64 = match range.len() {
                Some(v) => v,
                None => return Err(SysError::new(ERANGE)),
            };
            // The drm2kgsl pre-backed path needs the BAR GPA in the guest DT so its
            // virtio-gpu driver can resolve the ordinary guarded MAP_BLOB offset. This
            // helper is reached before FDT generation, while the allocator still knows the
            // complete BAR range. Keep the metadata opt-in to that path only.
            if env::var_os("CROSVM_DRM2KGSL_BAR_PREBACKED").is_some() {
                env::set_var("CROSVM_DRM2KGSL_BAR_GPA", format!("{:#x}", range.start));
                env::set_var("CROSVM_DRM2KGSL_BAR_SIZE", allocation_size.to_string());
            }
            if guest_offset > allocation_size {
                return Err(SysError::new(EINVAL));
            }
            let size: usize = match source
                .as_ref()
                .map(|region| region.size() as u64)
                .or_else(|| allocation_size.checked_sub(guest_offset))
                .filter(|size| *size <= allocation_size - guest_offset)
                .and_then(|size| size.try_into().ok())
            {
                Some(v) => v,
                None => return Err(SysError::new(ERANGE)),
            };
            let region: Box<dyn MappedRegion> = match source {
                Some(region) if region.size() == size => region,
                Some(_) => return Err(SysError::new(EINVAL)),
                None if guest_offset == 0 => match MemoryMappingArena::new(size) {
                    Ok(arena) => Box::new(arena),
                    Err(MmapError::SystemCallFailed(e)) => return Err(e),
                    _ => return Err(SysError::new(EINVAL)),
                },
                None => return Err(SysError::new(EINVAL)),
            };

            let guest_address = range
                .start
                .checked_add(guest_offset)
                .ok_or_else(|| SysError::new(ERANGE))?;
            match vm.add_memory_region(GuestAddress(guest_address), region, false, false, cache) {
                Ok(slot) => {
                    if env::var_os("CROSVM_DRM2KGSL_BAR_PREBACKED").is_some() {
                        base::info!(
                            "GPU-BAR-MAP: allocation={:?} bar_gpa={:#x} guest_offset={:#x} \
                             guest_start={:#x} size={:#x} cache={:?} slot={}",
                            alloc,
                            range.start,
                            guest_offset,
                            guest_address,
                            size,
                            cache,
                            slot,
                        );
                    }
                    Ok(VmMappedMemoryRegion {
                        allocation: alloc,
                        guest_address: GuestAddress(guest_address),
                        slot,
                        allocation_offset: guest_offset,
                        size,
                    })
                }
                Err(e) => Err(e),
            }
        }
        None => Err(SysError::new(EINVAL)),
    }
}

static SHOULD_PREPARE_MEMORY_REGION: Lazy<bool> = Lazy::new(|| {
    if cfg!(target_arch = "x86_64") {
        // The legacy x86 MMU allocates an rmap and a page tracking array
        // that take 2.5MiB per 1GiB of user memory region address space,
        // so avoid mapping the whole shared memory region if we're not
        // using the tdp mmu.
        match std::fs::read("/sys/module/kvm/parameters/tdp_mmu") {
            Ok(bytes) if !bytes.is_empty() => bytes[0] == b'Y',
            _ => false,
        }
    } else if cfg!(target_pointer_width = "64") {
        true
    } else {
        // Not enough address space on 32-bit systems
        false
    }
});

pub fn should_prepare_memory_region() -> bool {
    *SHOULD_PREPARE_MEMORY_REGION
}

impl FsMappingRequest {
    pub fn execute(&self, vm: &mut dyn Vm, allocator: &mut SystemAllocator) -> VmResponse {
        use self::FsMappingRequest::*;
        match *self {
            AllocateSharedMemoryRegion(alloc) => {
                match prepare_shared_memory_region(
                    vm,
                    allocator,
                    alloc,
                    MemCacheType::CacheCoherent,
                    None,
                ) {
                    Ok(VmMappedMemoryRegion { slot, .. }) => VmResponse::RegisterMemory { slot },
                    Err(e) => VmResponse::Err(e),
                }
            }
            CreateMemoryMapping {
                slot,
                ref fd,
                size,
                file_offset,
                prot,
                mem_offset,
            } => {
                let raw_fd: Descriptor = Descriptor(fd.as_raw_descriptor());

                match vm.add_fd_mapping(slot, mem_offset, size, &raw_fd, file_offset, prot) {
                    Ok(()) => VmResponse::Ok,
                    Err(e) => VmResponse::Err(e),
                }
            }
            RemoveMemoryMapping { slot, offset, size } => {
                match vm.remove_mapping(slot, offset, size) {
                    Ok(()) => VmResponse::Ok,
                    Err(e) => VmResponse::Err(e),
                }
            }
        }
    }
}
