//! Compile the entire production native_shared.rs allocation/query/cleanup
//! implementation with actual protocol, native metadata and map ledgers.
//! Only allocator/renderer/mapper OS boundaries are deterministic fixtures.
//! This is not native AHB, KGSL or Gunyah runtime proof.
#![allow(dead_code)]
#[cfg(all(feature = "legacy-rand", feature = "android-rand"))]
compile_error!("select one actual rand dependency generation");
#[cfg(feature = "android-rand")]
extern crate rand_android as rand;
extern crate self as base;
extern crate self as gpu_display;
extern crate self as vm_control;
extern crate self as rutabaga_gfx;

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap as Map;
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
use std::rc::Rc;
use parking_lot::Mutex;
use shared_allocation::{DynamicMappingWindow, ExternalMappingReceipt, ExternalMappingLedger};

pub type SafeDescriptor = OwnedFd;
pub type RutabagaDescriptor = OwnedFd;
trait IntoRawDescriptor { fn into_raw_descriptor(self) -> i32; }
impl IntoRawDescriptor for OwnedFd { fn into_raw_descriptor(self) -> i32 { self.into_raw_fd() } }
trait RutabagaFromRawDescriptor { unsafe fn from_raw_descriptor(fd: i32) -> Self; }
impl RutabagaFromRawDescriptor for OwnedFd {
    unsafe fn from_raw_descriptor(fd: i32) -> Self { Self::from_raw_fd(fd) }
}
macro_rules! error { ($($arg:tt)*) => { eprintln!($($arg)*) }; }

#[path = "../../../../vm_control/src/shared_allocation.rs"]
mod mapping_contract;
#[path = "../../../../vm_control/src/native_allocation.rs"]
pub mod native_allocation;
#[path = "../../../../gpu_display/src/shared_allocation.rs"]
mod native_display;
pub mod shared_allocation {
    pub use crate::mapping_contract::*;
    pub use crate::native_display::*;
}
#[path = "../../../src/virtio/gpu/control_header.rs"]
pub mod control_header;
#[path = "../../../src/virtio/gpu/shared_allocation_protocol.rs"]
pub mod shared_allocation_protocol;
pub mod virtio { pub mod gpu { pub use crate::shared_allocation_protocol; } }
use shared_allocation_protocol::*;

pub const DRM_FORMAT_ABGR8888: u32 = u32::from_le_bytes(*b"AB24");
pub const RUTABAGA_HANDLE_TYPE_MEM_DMABUF: u32 = 4;
pub const RUTABAGA_MAP_CACHE_CACHED: u32 = 1;
pub const RUTABAGA_MAP_CACHE_WC: u32 = 2;
struct RutabagaHandle { os_handle: OwnedFd, handle_type: u32 }
#[derive(Debug)]
enum GpuResponse {
    OkNoData, OkMapInfo { map_info: u32, pool_offset: Option<u32> },
    OkAllocatedSharedAllocation(SharedAllocationResponse), OkSharedOwner(SharedOwnerResponse),
    ErrInvalidParameter, ErrInvalidResourceId, ErrInvalidScanoutId, ErrOutOfMemory, ErrUnspec,
}
use GpuResponse::*;
type VirtioGpuResult = Result<GpuResponse, GpuResponse>;
impl From<anyhow::Error> for GpuResponse { fn from(_: anyhow::Error) -> Self { ErrUnspec } }

#[derive(Clone, Copy)]
struct Caps { generation: u64, display_id: i32 }
impl Caps { fn valid(&self) -> bool { self.generation != 0 } }
struct Allocation {
    description: native_display::SharedAllocationDescription,
    releases: Rc<Cell<u32>>,
}
impl Drop for Allocation { fn drop(&mut self) { self.releases.set(self.releases.get() + 1); } }
impl native_display::HostSharedAllocation for Allocation {
    fn description(&self) -> native_display::SharedAllocationDescription { self.description }
    fn duplicate_descriptor(&self) -> anyhow::Result<SafeDescriptor> {
        Ok(std::fs::File::open("/dev/null")?.into())
    }
}
struct Display {
    generation: u64, next: u64, allocate_failed: bool, malformed: bool,
    releases: Rc<Cell<u32>>,
}
impl Display {
    fn color_capabilities(&self) -> Option<Caps> {
        Some(Caps { generation: self.generation, display_id: 0 })
    }
    fn allocate_shared_buffer(&mut self, width: u32, height: u32)
        -> anyhow::Result<Box<dyn native_display::HostSharedAllocation>> {
        if self.allocate_failed { anyhow::bail!("injected native allocator failure"); }
        self.next += 1;
        Ok(Box::new(Allocation {
            description: native_display::SharedAllocationDescription {
                version: 1, size: 64, buffer_id: self.next,
                allocation_size: 65536, width, height, plane_stride: width * 4,
                fourcc: DRM_FORMAT_ABGR8888, plane_count: 1,
                layout_flags: if self.malformed { 0 } else { 1 }, ..Default::default()
            }, releases: self.releases.clone(),
        }))
    }
}
struct Attached { ctx_id: u32, resource_id: u32, allocation_size: u64, map_info: u32 }
#[derive(Default)]
struct Renderer {
    imports: Map<u32, (u64, RutabagaHandle)>,
    attach_failed: bool, release_failed: bool, import_failed: bool,
    releases: u32, release_calls: u32,
}
impl Renderer {
    fn host_dmabuf_import_supported(&self) -> bool { true }
    fn resource_import_host_dmabuf(&mut self, id: u32, size: u64, handle: RutabagaHandle)
        -> Result<(), GpuResponse> {
        if self.import_failed { return Err(ErrUnspec); }
        assert!(self.imports.insert(id, (size, handle)).is_none());
        Ok(())
    }
    fn context_attach_host_dmabuf(&mut self, ctx_id: u32, resource_id: u32)
        -> Result<Attached, GpuResponse> {
        if self.attach_failed || ctx_id != 81 { return Err(ErrUnspec); }
        Ok(Attached { ctx_id, resource_id, allocation_size: self.imports[&resource_id].0, map_info: 1 })
    }
    fn unref_host_dmabuf(&mut self, resource_id: u32) -> Result<(), GpuResponse> {
        self.release_calls += 1;
        if self.release_failed { return Err(ErrUnspec); }
        assert!(self.imports.remove(&resource_id).is_some(), "no double native release");
        self.releases += 1;
        Ok(())
    }
}
struct Protection;
impl Protection { fn read_write() -> Self { Self } }
enum MemCacheType { CacheCoherent, CacheNonCoherent }
enum VmMemorySource { Descriptor { descriptor: OwnedFd, offset: u64, size: u64 } }
struct Mapper {
    ledger: ExternalMappingLedger,
    unmap_failed: bool, map_uncertain: bool, map_preflight_failed: bool, unmaps: u32,
}
fn window() -> DynamicMappingWindow {
    DynamicMappingWindow { bar_size: 128 << 20, reserved_prefix: 8 << 20, alignment: 16384 }
}
impl Mapper {
    fn external_mapping_window(&self) -> Option<DynamicMappingWindow> { Some(window()) }
    fn add_external_mapping(&mut self, source: VmMemorySource, offset: u64,
        _: Protection, _: MemCacheType) -> anyhow::Result<ExternalMappingReceipt> {
        if self.map_preflight_failed { anyhow::bail!("preflight"); }
        let VmMemorySource::Descriptor { size, .. } = source;
        let receipt = self.ledger.reserve(window(), offset, size).unwrap();
        if self.map_uncertain {
            self.ledger.quarantine(receipt);
            return Err(shared_allocation::ExternalMappingFailure { receipt }.into());
        }
        assert!(self.ledger.commit(receipt));
        Ok(receipt)
    }
    fn remove_external_mapping(&mut self, receipt: ExternalMappingReceipt) -> anyhow::Result<()> {
        assert!(self.ledger.begin_remove(receipt));
        if self.unmap_failed { anyhow::bail!("injected Gunyah unshare failure"); }
        assert!(self.ledger.complete_remove(receipt));
        self.unmaps += 1;
        Ok(())
    }
    fn external_mapping_live(&self, receipt: ExternalMappingReceipt) -> bool { self.ledger.is_live(receipt) }
}
struct Scanout { width: u32, height: u32 }
struct VirtioGpu {
    native_allocations: native_shared::NativeAllocationOwners,
    display: RefCell<Display>,
    mapper: Mutex<Option<Mapper>>,
    scanouts: Map<u32, Scanout>,
    resources: Map<u32, ()>,
    rutabaga: Renderer,
}
#[path = "../../../src/virtio/gpu/native_shared.rs"]
mod native_shared;

#[cfg(test)]
mod tests {
    use super::*;
    use zerocopy::{FromBytes, IntoBytes};
    fn gpu() -> VirtioGpu {
        VirtioGpu {
            native_allocations: Default::default(),
            display: RefCell::new(Display { generation: 17, next: 100, allocate_failed: false,
                malformed: false, releases: Rc::new(Cell::new(0)) }),
            mapper: Mutex::new(Some(Mapper { ledger: Default::default(), unmap_failed: false,
                map_uncertain: false, map_preflight_failed: false, unmaps: 0 })),
            scanouts: Map::from([(0, Scanout { width: 128, height: 128 })]),
            resources: Map::new(), rutabaga: Renderer::default(),
        }
    }
    fn query(command: u32, generation: u64, context: u32, resource: u32, token: u64)
        -> SharedAllocationHeader {
        SharedAllocationHeader {
            hdr: control_header::virtio_gpu_ctrl_hdr { type_: command.into(), ctx_id: context.into(),
                ..Default::default() },
            magic: SHARED_ALLOCATION_MAGIC.into(), version: 1.into(), size: 80.into(),
            generation: generation.into(), resource_id: resource.into(), token: token.into(), ..Default::default()
        }
    }
    fn session(gpu: &mut VirtioGpu) -> [data_model::Le64; 2] {
        response(gpu.recover_shared_owner(SharedOwnerRequest {
            query: query(CMD_QUERY_SHARED_OWNER, 0, 0, 0, 0), ..Default::default()
        }, false), OWNER_SESSION).host_epoch
    }
    fn allocate(epoch: [data_model::Le64; 2]) -> RecoverableAllocationRequest {
        let mut header = query(CMD_ALLOCATE_RECOVERABLE, 17, 81, 91, 0);
        header.size = 96.into();
        RecoverableAllocationRequest { host_epoch: epoch, allocation: SharedAllocationRequest {
            query: header, width: 128.into(), height: 128.into(), fourcc: DRM_FORMAT_ABGR8888.into(),
            flags: 0.into(),
        } }
    }
    fn owner(epoch: [data_model::Le64; 2], cleanup: bool) -> SharedOwnerRequest {
        SharedOwnerRequest { host_epoch: epoch,
            query: query(if cleanup { CMD_CLEANUP_SHARED_OWNER } else { CMD_QUERY_SHARED_OWNER }, 17, 81, 91, 0) }
    }
    fn response(result: VirtioGpuResult, state: u32) -> SharedOwnerResponse {
        let OkSharedOwner(reply) = result.unwrap() else { panic!("wrong reply"); };
        assert_eq!(reply.state.to_native(), state);
        // Exercise the real ordinary frontend response encoder and decoder.
        let mut wire = Vec::new();
        assert_eq!(reply.encode(control_header::virtio_gpu_ctrl_hdr::default(), &mut wire).unwrap(), 128);
        let decoded = SharedOwnerResponse::read_from_bytes(&wire).unwrap();
        assert_eq!(decoded.query.hdr.type_.to_native(), RESP_SHARED_OWNER);
        assert_eq!(decoded.query.hdr.ctx_id.to_native(), 0);
        assert_eq!(decoded.query.size.to_native(), 128);
        assert_eq!(decoded.owner_context, reply.owner_context);
        decoded
    }
    #[test]
    fn lost_allocate_reply_tokenless_lookup_then_cleanup_is_authoritative_and_idempotent() {
        let mut gpu = gpu(); let epoch = session(&mut gpu);
        // Real ALLOCATE production dispatch, deliberately discard its result.
        assert!(gpu.allocate_recoverable(allocate(epoch)).is_ok());
        let queried = response(gpu.recover_shared_owner(owner(epoch, false), false), OWNER_RETAINED);
        assert_ne!(queried.query.token.to_native(), 0);
        assert_eq!(queried.owner_context.to_native(), 81);
        let receipt = response(gpu.recover_shared_owner(owner(epoch, true), true), OWNER_RELEASED);
        assert_eq!(receipt.query.token, queried.query.token);
        assert_eq!(gpu.display.borrow().releases.get(), 1);
        assert_eq!(gpu.rutabaga.releases, 1);
        for _ in 0..8 {
            response(gpu.recover_shared_owner(owner(epoch, true), true), OWNER_RELEASED);
            response(gpu.recover_shared_owner(owner(epoch, false), false), OWNER_RELEASED);
        }
        assert_eq!(gpu.rutabaga.release_calls, 1);
        assert!(gpu.allocate_recoverable(allocate(epoch)).is_err());
        let mut legacy = allocate(epoch).allocation;
        legacy.query.hdr.type_ = CMD_ALLOCATE_SHARED_ALLOCATION.into(); legacy.query.size = 80.into();
        assert!(gpu.allocate_shared_allocation(legacy).is_err());
    }
    #[test]
    fn failed_attach_and_kgsl_release_keep_ahb_until_real_retry_succeeds() {
        let mut gpu = gpu(); let epoch = session(&mut gpu);
        gpu.rutabaga.attach_failed = true; gpu.rutabaga.release_failed = true;
        assert!(gpu.allocate_recoverable(allocate(epoch)).is_err());
        response(gpu.recover_shared_owner(owner(epoch, false), false), OWNER_RETAINED);
        response(gpu.recover_shared_owner(owner(epoch, true), true), OWNER_RETAINED);
        assert!(gpu.native_allocations.contains(91));
        assert_eq!(gpu.display.borrow().releases.get(), 0);
        gpu.rutabaga.release_failed = false;
        response(gpu.recover_shared_owner(owner(epoch, true), true), OWNER_RELEASED);
        assert_eq!(gpu.display.borrow().releases.get(), 1);
    }
    #[test]
    fn cleanup_failure_never_restores_mapping_or_ack_admission() {
        let mut gpu = gpu(); let epoch = session(&mut gpu);
        assert!(gpu.allocate_recoverable(allocate(epoch)).is_ok());
        gpu.rutabaga.release_failed = true;
        response(gpu.recover_shared_owner(owner(epoch, true), true), OWNER_RETAINED);
        assert!(gpu.map_native_allocation(91, 8 << 20).is_err());
        assert_eq!(gpu.mapper.lock().as_ref().unwrap().unmaps, 0);
        assert_eq!(gpu.display.borrow().releases.get(), 0);
        gpu.rutabaga.release_failed = false;
        response(gpu.recover_shared_owner(owner(epoch, true), true), OWNER_RELEASED);
    }
    #[test]
    fn never_submitted_or_preallocation_error_requires_terminal_seal_not_empty_query() {
        let mut gpu = gpu(); let epoch = session(&mut gpu);
        response(gpu.recover_shared_owner(owner(epoch, false), false), OWNER_UNKNOWN);
        response(gpu.recover_shared_owner(owner(epoch, true), true), OWNER_RELEASED);
        assert!(gpu.allocate_recoverable(allocate(epoch)).is_err(), "late ALLOCATE must not recreate sealed owner");
        assert_eq!(gpu.display.borrow().next, 100);
        assert_eq!(gpu.rutabaga.release_calls, 0);
    }
    #[test]
    fn failed_allocator_metadata_and_import_have_explicit_retirement_receipts() {
        for fault in 0..3 {
            let mut gpu = gpu(); let epoch = session(&mut gpu);
            gpu.display.borrow_mut().allocate_failed = fault == 0;
            gpu.display.borrow_mut().malformed = fault == 1;
            gpu.rutabaga.import_failed = fault == 2;
            assert!(gpu.allocate_recoverable(allocate(epoch)).is_err());
            response(gpu.recover_shared_owner(owner(epoch, false), false), OWNER_RELEASED);
            assert!(gpu.native_allocations.is_empty());
            assert_eq!(gpu.display.borrow().releases.get(), if fault == 0 { 0 } else { 1 });
        }
    }
    #[test]
    fn old_surface_cleanup_is_allowed_but_forged_owner_and_host_epoch_are_not() {
        let mut gpu = gpu(); let epoch = session(&mut gpu);
        assert!(gpu.allocate_recoverable(allocate(epoch)).is_ok());
        gpu.display.borrow_mut().generation = 18;
        for field in 0..5 {
            let mut bad = owner(epoch, true);
            match field {
                0 => bad.query.generation = 18.into(), 1 => bad.query.hdr.ctx_id = 82.into(),
                2 => bad.query.token = 999.into(), 3 => bad.host_epoch[0] = (!epoch[0].to_native()).into(),
                _ => bad.host_epoch = [0.into(); 2],
            }
            assert!(gpu.recover_shared_owner(bad, true).is_err());
        }
        assert_eq!(gpu.rutabaga.release_calls, 0);
        response(gpu.recover_shared_owner(owner(epoch, true), true), OWNER_RELEASED);
    }
    #[test]
    fn reconstructed_host_journal_never_matches_old_incarnation() {
        let mut first = gpu(); let epoch = session(&mut first);
        assert!(first.allocate_recoverable(allocate(epoch)).is_ok());
        let mut replacement = gpu(); let next = session(&mut replacement);
        assert_ne!(next, epoch);
        assert!(replacement.recover_shared_owner(owner(epoch, true), true).is_err());
        assert!(replacement.allocate_recoverable(allocate(epoch)).is_err());
        assert!(first.native_allocations.contains(91));
        response(first.recover_shared_owner(owner(epoch, true), true), OWNER_RELEASED);
    }
    #[test]
    fn map_or_unmap_reply_loss_and_backend_failure_retire_once_using_real_ledgers() {
        for uncertain in [false, true] {
            let mut gpu = gpu(); let epoch = session(&mut gpu);
            assert!(gpu.allocate_recoverable(allocate(epoch)).is_ok());
            gpu.mapper.lock().as_mut().unwrap().map_uncertain = uncertain;
            assert_eq!(gpu.map_native_allocation(91, 8 << 20).is_err(), uncertain);
            let mapped = response(gpu.recover_shared_owner(owner(epoch, false), false), OWNER_RETAINED);
            assert_eq!(mapped.bar_offset.to_native(), 8 << 20);
            assert_eq!(mapped.mapped_size.to_native(), 65536);
            gpu.mapper.lock().as_mut().unwrap().unmap_failed = true;
            response(gpu.recover_shared_owner(owner(epoch, true), true), OWNER_RETAINED);
            assert_eq!(gpu.rutabaga.release_calls, 0);
            gpu.mapper.lock().as_mut().unwrap().unmap_failed = false;
            gpu.rutabaga.release_failed = true;
            response(gpu.recover_shared_owner(owner(epoch, true), true), OWNER_RETAINED);
            assert_eq!(gpu.mapper.lock().as_ref().unwrap().unmaps, 1);
            gpu.rutabaga.release_failed = false;
            // Discard successful cleanup reply then retry: one unmap/release.
            assert!(gpu.recover_shared_owner(owner(epoch, true), true).is_ok());
            response(gpu.recover_shared_owner(owner(epoch, true), true), OWNER_RELEASED);
            assert_eq!(gpu.mapper.lock().as_ref().unwrap().unmaps, 1);
            assert_eq!(gpu.rutabaga.releases, 1);
        }
    }
    #[test]
    fn bounded_terminal_journal_never_evicts_identity_or_allocates_when_full() {
        let mut gpu = gpu(); let epoch = session(&mut gpu);
        for resource in 100..4196 {
            let mut request = owner(epoch, true); request.query.resource_id = resource.into();
            response(gpu.recover_shared_owner(request, true), OWNER_RELEASED);
        }
        assert!(gpu.allocate_recoverable(allocate(epoch)).is_err());
        assert!(gpu.recover_shared_owner(owner(epoch, true), true).is_err());
        assert_eq!(gpu.display.borrow().next, 100);
        let mut first = owner(epoch, true); first.query.resource_id = 100.into();
        response(gpu.recover_shared_owner(first, true), OWNER_RELEASED);
    }
    #[test]
    fn recovery_request_fixed_fields_and_encoder_length_are_strict() {
        let mut gpu = gpu(); let epoch = session(&mut gpu);
        let good = owner(epoch, false);
        for offset in [0, 4, 8, 20, 21, 22, 23, 24, 28, 32, 36, 60] {
            let mut wire = good.as_bytes().to_vec(); wire[offset] ^= 0x80;
            let bad = SharedOwnerRequest::read_from_bytes(&wire).unwrap();
            assert!(gpu.recover_shared_owner(bad, false).is_err());
        }
        let good = response(gpu.recover_shared_owner(owner(epoch, false), false), OWNER_UNKNOWN);
        let mut short = [0u8; 127];
        assert_eq!(good.encode(control_header::virtio_gpu_ctrl_hdr::default(), &mut short.as_mut_slice())
            .unwrap_err().kind(), std::io::ErrorKind::WriteZero);
    }
}
