// Copyright 2018 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod control_header;
mod display_color_protocol;
mod shared_allocation_protocol;
mod edid;
mod external_scanout;
mod fence_completion;
#[cfg(test)]
mod fence_tests;
mod parameters;
mod protocol;
mod snapshot;
mod virtio_gpu;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::env;
use std::io::Read;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use ::snapshot::AnySnapshot;
use anyhow::anyhow;
use anyhow::Context;
use base::custom_serde::deserialize_map_from_kv_vec;
use base::custom_serde::serialize_map_as_kv_vec;
use base::debug;
use base::error;
use base::info;
#[cfg(any(target_os = "android", target_os = "linux"))]
use base::linux::move_task_to_cgroup;
#[cfg(any(target_os = "android", target_os = "linux"))]
use base::set_rt_fifo;
#[cfg(any(target_os = "android", target_os = "linux"))]
use base::set_rt_prio_limit;
use base::warn;
use base::AsRawDescriptor;
use base::Event;
use base::EventToken;
use base::FromRawDescriptor;
use base::RawDescriptor;
use base::ReadNotifier;
#[cfg(windows)]
use base::RecvTube;
use base::Result;
use base::SafeDescriptor;
use base::SendTube;
use base::Tube;
use base::VmEventType;
use base::WaitContext;
use base::WorkerThread;
use data_model::*;
pub use external_scanout::ExternalScanout;
pub use gpu_display::EventDevice;
use gpu_display::*;
use hypervisor::MemCacheType;
pub use parameters::AudioDeviceMode;
pub use parameters::GpuParameters;
pub use parameters::VramExceedPolicy;
use rutabaga_gfx::*;
use serde::Deserialize;
use serde::Serialize;
use sync::Mutex;
pub use vm_control::gpu::DisplayMode as GpuDisplayMode;
pub use vm_control::gpu::DisplayParameters as GpuDisplayParameters;
use vm_control::gpu::GpuControlCommand;
use vm_control::gpu::GpuControlResult;
pub use vm_control::gpu::MouseMode as GpuMouseMode;
pub use vm_control::gpu::DEFAULT_DISPLAY_HEIGHT;
pub use vm_control::gpu::DEFAULT_DISPLAY_WIDTH;
pub use vm_control::gpu::DEFAULT_REFRESH_RATE;
#[cfg(windows)]
use vm_control::ModifyWaitContext;
use vm_control::VmMemorySource;
use vm_memory::GuestAddress;
use vm_memory::GuestMemory;
use zerocopy::IntoBytes;

const DRM2KGSL_BAR_BASE_GUARD: u64 = 2 << 20;

use self::fence_completion::ContextFenceTokens;
use self::fence_completion::FenceQueue;
use self::fence_completion::GlobalFenceTokens;
pub use self::protocol::virtio_gpu_config;
pub use self::protocol::VIRTIO_GPU_F_CONTEXT_INIT;
pub use self::protocol::VIRTIO_GPU_F_CREATE_GUEST_HANDLE;
pub use self::protocol::VIRTIO_GPU_F_EDID;
pub use self::protocol::VIRTIO_GPU_F_FENCE_PASSING;
pub use self::protocol::VIRTIO_GPU_F_RESOURCE_BLOB;
pub use self::protocol::VIRTIO_GPU_F_RESOURCE_UUID;
pub use self::protocol::VIRTIO_GPU_F_VIRGL;
pub use self::protocol::VIRTIO_GPU_MAX_SCANOUTS;
pub use self::protocol::VIRTIO_GPU_SHM_ID_HOST_VISIBLE;
use self::protocol::*;
use self::virtio_gpu::to_rutabaga_descriptor;
pub use self::virtio_gpu::ProcessDisplayResult;
use self::virtio_gpu::VirtioGpu;
use self::virtio_gpu::VirtioGpuSnapshot;
use super::copy_config;
use super::resource_bridge::ResourceRequest;
use super::DescriptorChain;
use super::DeviceType;
use super::Interrupt;
use super::Queue;
use super::Reader;
use super::SharedMemoryMapper;
use super::SharedMemoryPrepareType;
use super::SharedMemoryRegion;
use super::VirtioDevice;
use super::Writer;
use crate::virtio::resource_bridge::ResourceResponse;
use crate::PciAddress;

// First queue is for virtio gpu commands. Second queue is for cursor commands, which we expect
// there to be fewer of.
// A 1 GiB Windows backing allocation can contain 262144 noncontiguous 4 KiB
// pages. Its 4 MiB entry payload needs up to 1025 direct descriptors, plus
// command and response fragments. Keep the queue power-of-two and leave room
// for other control commands; the cursor queue is independent.
const QUEUE_SIZES: &[u16] = &[2048, 16];

/// Most `virtio_gpu_mem_entry` records this will build a list from in one command.
///
/// `nr_entries` arrives as a raw Le32 in the command header and is not validated anywhere
/// upstream: the sites below used it directly as a `Vec::with_capacity`, before reading a single
/// entry. Two things follow from that, and both need bounding.
///
/// The obvious one -- a guest claiming 0xFFFFFFFF entries while supplying none -- is less bad than
/// it looks: `with_capacity` reserves without touching, and the first short read drops the Vec. It
/// still has to go, because `Vec::with_capacity` is infallible, so a refused reservation is
/// `handle_alloc_error()` and the GPU device runs in-process: that aborts the VM.
///
/// The one that actually costs memory needs no lying at all. Even a queue with 512 descriptors
/// and the chain walk only stops at `count >= queue_size` -- it does not require the descriptors to
/// be distinct -- so 512 of them may all point at one guest buffer. A guest can then honestly
/// present ~4 GiB of readable payload (the chain length is summed into a u32, which is the only
/// existing ceiling) out of ~8 MiB of its own memory, with every entry naming the same valid page.
/// Every read succeeds, and the result is RETAINED in `resource.backing_iovecs` until detach or
/// unref -- twice, once here and once in rutabaga. That is ~8.5 GB of host heap per resource id
/// for 8 MiB of guest memory, repeatable, and it never exceeds the guest's own memory quota.
///
/// 1M entries is a 16 MiB Vec, enough to describe a 4 GiB scatter-gather list of 4 KiB pages, so
/// it bounds the retained cost without rejecting anything a real driver produces.
const MAX_MEM_ENTRIES: usize = 1 << 20;

#[cfg(test)]
mod large_backing_tests {
    use super::*;
    use crate::virtio::descriptor_chain::VIRTQ_DESC_F_NEXT;
    use crate::virtio::descriptor_chain::VIRTQ_DESC_F_WRITE;
    use crate::virtio::queue::split_descriptor_chain::Desc;
    use crate::virtio::queue::split_descriptor_chain::SplitDescriptorChain;

    #[test]
    fn fragmented_backing_uses_complete_direct_chain() {
        for pages in [140070usize, 262144] {
            // Worst alignment: two command pages, a partial first payload
            // page, remaining payload pages, and two response pages.
            let bytes = pages * size_of::<virtio_gpu_mem_entry>();
            let mut lengths = vec![1u32, 95];
            lengths.push(1);
            let mut remaining = bytes - 1;
            while remaining != 0 {
                let fragment = remaining.min(4096);
                lengths.push(fragment as u32);
                remaining -= fragment;
            }
            let outputs = lengths.len();
            lengths.extend([1, 23]);
            let mem = GuestMemory::new(&[(GuestAddress(0), 0x20000)]).unwrap();
            for (index, &length) in lengths.iter().enumerate() {
                let flags = if index + 1 < lengths.len() {
                    VIRTQ_DESC_F_NEXT
                } else {
                    0
                } | if index >= outputs {
                    VIRTQ_DESC_F_WRITE
                } else {
                    0
                };
                let descriptor = Desc {
                    addr: 0x10000u64.into(),
                    len: length.into(),
                    flags: flags.into(),
                    next: ((index + 1) as u16).into(),
                };
                mem.write_obj_at_addr(descriptor, GuestAddress((index * 16) as u64))
                    .unwrap();
            }
            let chain = SplitDescriptorChain::new(&mem, GuestAddress(0), QUEUE_SIZES[0], 0);
            let packet = DescriptorChain::new(chain, &mem, 0).unwrap();
            assert_eq!(packet.count as usize, lengths.len());
            assert_eq!(packet.reader.available_bytes(), bytes + 96);
            assert_eq!(packet.writer.available_bytes(), 24);
            assert_eq!(checked_entry_count(pages as u32, bytes).unwrap(), pages);
            assert!(checked_entry_count(pages as u32, bytes - 1).is_err());
            // A peer that only negotiates the old queue fails safely.
            let old = SplitDescriptorChain::new(&mem, GuestAddress(0), 512, 0);
            assert!(DescriptorChain::new(old, &mem, 0).is_err());
        }
        assert!(checked_entry_count(u32::MAX, usize::MAX).is_err());
    }
}

/// Entries the guest actually presented, or an error. Clamping to `available_bytes` is what kills
/// the over-reservation: capacity can no longer exceed the bytes really in the chain.
fn checked_entry_count(
    nr_entries: u32,
    available_bytes: usize,
) -> std::result::Result<usize, GpuResponse> {
    let n = nr_entries as usize;
    if n > MAX_MEM_ENTRIES || n > available_bytes / size_of::<virtio_gpu_mem_entry>() {
        return Err(GpuResponse::ErrUnspec);
    }
    Ok(n)
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GpuMode {
    #[serde(rename = "2d", alias = "2D")]
    Mode2D,
    #[cfg(feature = "virgl_renderer")]
    #[serde(rename = "virglrenderer", alias = "3d", alias = "3D")]
    ModeVirglRenderer,
    #[cfg(feature = "gfxstream")]
    #[serde(rename = "gfxstream")]
    ModeGfxstream,
}

impl Default for GpuMode {
    fn default() -> Self {
        #[cfg(all(windows, feature = "gfxstream"))]
        return GpuMode::ModeGfxstream;

        #[cfg(all(unix, feature = "virgl_renderer"))]
        return GpuMode::ModeVirglRenderer;

        #[cfg(not(any(
            all(windows, feature = "gfxstream"),
            all(unix, feature = "virgl_renderer"),
        )))]
        return GpuMode::Mode2D;
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GpuWsi {
    #[serde(alias = "vk")]
    Vulkan,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct VirtioScanoutBlobData {
    pub width: u32,
    pub height: u32,
    pub drm_format: DrmFormat,
    pub strides: [u32; 4],
    pub offsets: [u32; 4],
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
enum VirtioGpuRing {
    Global,
    ContextSpecific { ctx_id: u32, ring_idx: u8 },
}

#[derive(Default)]
pub struct FenceState {
    queue: FenceQueue<VirtioGpuRing, ReturnDescriptor>,
    global_tokens: GlobalFenceTokens,
    context_tokens: ContextFenceTokens,
    uses_global_tokens: bool,
    context_retirement_event: Option<Event>,
    waiting_for_context_destroy: bool,
    renderer_error: Option<RutabagaFenceError>,
}

#[derive(Serialize, Deserialize)]
struct FenceStateSnapshot {
    // Customize serialization to avoid errors when trying to use objects as keys in JSON
    // dictionaries.
    #[serde(
        serialize_with = "serialize_map_as_kv_vec",
        deserialize_with = "deserialize_map_from_kv_vec"
    )]
    completed_fences: BTreeMap<VirtioGpuRing, u64>,
    // None identifies the legacy guest-cookie format (or another component).
    #[serde(default)]
    global_tokens_last_issued: Option<u32>,
    #[serde(default)]
    context_tokens_last_issued: Option<u64>,
}

impl FenceState {
    fn check_renderer(&self) -> anyhow::Result<()> {
        if let Some(failure) = self.renderer_error {
            return Err(anyhow!(
                "renderer fence failed, ownership retained: {:?}",
                failure
            ));
        }
        Ok(())
    }

    fn snapshot(&self) -> anyhow::Result<FenceStateSnapshot> {
        self.check_renderer()?;
        if !self.queue.is_empty()
            || !self.global_tokens.is_empty()
            || !self.context_tokens.is_empty()
        {
            return Err(anyhow!("cannot snapshot GPU with pending fences"));
        }
        Ok(FenceStateSnapshot {
            completed_fences: self.queue.completed_renderer.clone(),
            global_tokens_last_issued: self
                .uses_global_tokens
                .then(|| self.global_tokens.last_issued()),
            context_tokens_last_issued: self
                .uses_global_tokens
                .then(|| self.context_tokens.last_issued()),
        })
    }

    fn restore(&mut self, snapshot: FenceStateSnapshot) -> anyhow::Result<()> {
        self.check_renderer()?;
        if !self.queue.is_empty()
            || !self.global_tokens.is_empty()
            || !self.context_tokens.is_empty()
        {
            return Err(anyhow!("cannot restore GPU with pending fences"));
        }
        if self.uses_global_tokens != snapshot.global_tokens_last_issued.is_some() {
            return Err(anyhow!("incompatible GPU global fence snapshot format"));
        }
        if self.uses_global_tokens != snapshot.context_tokens_last_issued.is_some() {
            return Err(anyhow!("incompatible GPU context fence snapshot format"));
        }
        if let Some(last_issued) = snapshot.context_tokens_last_issued {
            if snapshot.completed_fences.iter().any(|(ring, id)| {
                matches!(ring, VirtioGpuRing::ContextSpecific { .. }) && *id > last_issued
            }) {
                return Err(anyhow!("GPU context completion exceeds issued tokens"));
            }
        }
        if let Some(last_issued) = snapshot.global_tokens_last_issued {
            if snapshot
                .completed_fences
                .get(&VirtioGpuRing::Global)
                .is_some_and(|id| *id > u64::from(last_issued))
            {
                return Err(anyhow!(
                    "GPU fence snapshot completion exceeds issued tokens"
                ));
            }
            if !self.global_tokens.restore_last_issued(last_issued) {
                return Err(anyhow!("GPU global fences are still pending"));
            }
        }
        if let Some(last_issued) = snapshot.context_tokens_last_issued {
            // Both registries were checked empty before modifying either.
            self.context_tokens.restore_last_issued(last_issued);
        }
        self.queue.completed_renderer = snapshot.completed_fences;
        Ok(())
    }
}

pub trait QueueReader {
    fn pop(&self) -> Option<DescriptorChain>;
    fn add_used(&self, desc_chain: DescriptorChain, len: u32);
    fn signal_used(&self);
}

#[derive(Clone)]
struct SharedQueueReader {
    queue: Arc<Mutex<Queue>>,
}

impl SharedQueueReader {
    fn new(queue: Queue) -> Self {
        Self {
            queue: Arc::new(Mutex::new(queue)),
        }
    }
}

impl QueueReader for SharedQueueReader {
    fn pop(&self) -> Option<DescriptorChain> {
        self.queue.lock().pop()
    }

    fn add_used(&self, desc_chain: DescriptorChain, len: u32) {
        self.queue.lock().add_used(desc_chain, len)
    }

    fn signal_used(&self) {
        self.queue.lock().trigger_interrupt();
    }
}

/// Initializes the virtio_gpu state tracker.
fn build(
    display_backends: &[DisplayBackend],
    display_params: Vec<GpuDisplayParameters>,
    display_event: Arc<AtomicBool>,
    rutabaga: Rutabaga,
    mapper: Arc<Mutex<Option<Box<dyn SharedMemoryMapper>>>>,
    external_blob: bool,
    fixed_blob_mapping: bool,
    #[cfg(windows)] wndproc_thread: &mut Option<WindowProcedureThread>,
    udmabuf: bool,
    #[cfg(windows)] gpu_display_wait_descriptor_ctrl_wr: SendTube,
    snapshot_scratch_directory: Option<PathBuf>,
    dmabuf_import_capped: bool,
) -> Option<VirtioGpu> {
    // `display_backends` is a try-in-turn chain, so a backend declining is how it is meant to
    // work, not a fault: `--gpu` on an Android host carries an X entry that no Android host has
    // ever been able to open. Reporting each refusal at error level as it happened made the
    // ordinary case read as a failure -- "failed to open display: unsupported by the
    // implementation" on every single boot -- while saying nothing about what did open. Collect
    // them instead and report once, below, where the outcome is known.
    let mut display_opt = None;
    let mut declined: Vec<String> = Vec::new();
    for display_backend in display_backends {
        match display_backend.build(
            #[cfg(windows)]
            wndproc_thread,
            #[cfg(windows)]
            gpu_display_wait_descriptor_ctrl_wr
                .try_clone()
                .expect("failed to clone wait context ctrl channel"),
        ) {
            Ok(c) => {
                display_opt = Some((display_backend, c));
                break;
            }
            // A backend that could not take its configured address is not declining, so it does
            // not go on the list that the loop shrugs at and moves past: the next backend in the
            // chain is a fallback for "this host has no such display", not for "the display this
            // VM was configured with is unreachable". Fail here instead, which reaches
            // Worker::new's caller and stops the VM coming up at all.
            Err(e @ GpuDisplayError::Listen(_)) => {
                error!("{}: {}", display_backend.name(), e);
                return None;
            }
            Err(e) => declined.push(format!("{}: {}", display_backend.name(), e)),
        };
    }

    let (chosen, mut display) = match display_opt {
        Some(d) => d,
        None => {
            error!(
                "failed to open any display backend ({})",
                declined.join("; ")
            );
            return None;
        }
    };

    // The ceiling the exporter bound to this device's screen was configured with. Applied before
    // anything asks the display what it can do, so `try_import_resource_to_display`'s probe sees a
    // display that refuses dmabufs and caches `CpuFallback` on the first resource, exactly as it
    // does against a sink that genuinely has no GPU half.
    if dmabuf_import_capped {
        info!("gpu: transport capped to cpu copy on this screen (transport-cap=cpu)");
        display.cap_transport_to_cpu();
    }

    // One line, and it names the outcome rather than the attempts. Landing on the stub is the
    // case worth spelling out: it is what a GPU device does when no exporter is bound to its
    // screen, which is a legitimate configuration -- the picture is somebody else's, the simplefb
    // screen's or nobody's -- and it used to be indistinguishable from a broken display. Whatever
    // was declined on the way is carried in the same line so a bound exporter that failed to open
    // still reports its reason here.
    let declined = if declined.is_empty() {
        String::new()
    } else {
        format!(" (after {})", declined.join("; "))
    };
    match chosen {
        DisplayBackend::Stub => info!(
            "gpu: no exporter opened this device's screen; rendering to the stub display, where \
             frames are discarded{}",
            declined
        ),
        _ => info!("gpu: display backend {} opened{}", chosen.name(), declined),
    }

    VirtioGpu::new(
        display,
        display_params,
        display_event,
        rutabaga,
        mapper,
        external_blob,
        fixed_blob_mapping,
        udmabuf,
        snapshot_scratch_directory,
    )
}

/// Resources used by the fence handler.
pub struct FenceHandlerActivationResources<Q>
where
    Q: QueueReader + Send + Clone + 'static,
{
    pub mem: GuestMemory,
    pub ctrl_queue: Q,
    pub cursor_queue: Option<Q>,
}

fn return_fenced_descriptors(
    completed: Vec<ReturnDescriptor>,
    ctrl_queue: &dyn QueueReader,
    cursor_queue: Option<&dyn QueueReader>,
) {
    let mut signal_ctrl = false;
    let mut signal_cursor = false;
    for desc in completed {
        match desc.queue {
            GpuQueue::Control => {
                ctrl_queue.add_used(desc.desc_chain, desc.len);
                signal_ctrl = true;
            }
            GpuQueue::Cursor => {
                // Only the built-in worker processes cursor messages; its
                // activation always supplies the shared cursor queue.
                cursor_queue
                    .expect("cursor descriptor without its activation queue")
                    .add_used(desc.desc_chain, desc.len);
                signal_cursor = true;
            }
        }
    }
    if signal_ctrl {
        ctrl_queue.signal_used();
    }
    if signal_cursor {
        cursor_queue.unwrap().signal_used();
    }
}

/// Create a handler that writes into the completed fence queue
pub fn create_fence_handler<Q>(
    fence_handler_resources: Arc<Mutex<Option<FenceHandlerActivationResources<Q>>>>,
    fence_state: Arc<Mutex<FenceState>>,
) -> RutabagaFenceHandler
where
    Q: QueueReader + Send + Clone + 'static,
{
    RutabagaFenceHandler::new(move |completed_fence: RutabagaFence| {
        let resources = fence_handler_resources.lock();
        let ring = match completed_fence.flags & VIRTIO_GPU_FLAG_INFO_RING_IDX {
            0 => VirtioGpuRing::Global,
            _ => VirtioGpuRing::ContextSpecific {
                ctx_id: completed_fence.ctx_id,
                ring_idx: completed_fence.ring_idx,
            },
        };

        let mut fence_state = fence_state.lock();
        if fence_state.renderer_error.is_some() {
            return;
        }
        if ring == VirtioGpuRing::Global
            && fence_state.uses_global_tokens
            && !fence_state.global_tokens.complete(completed_fence.fence_id)
        {
            // Never interpret unknown/late guest cookies as progress.
            return;
        }
        if let VirtioGpuRing::ContextSpecific { ctx_id, ring_idx } = ring {
            if fence_state.uses_global_tokens
                && !fence_state
                    .context_tokens
                    .complete(ctx_id, ring_idx, completed_fence.fence_id)
            {
                return;
            }
        }
        fence_state
            .queue
            .complete_renderer(ring, completed_fence.fence_id);
        // Suspend does not necessarily stop backend callbacks. Keep
        // completion state while queues are unavailable, then publish
        // these same descriptors when that activation resumes.
        if let Some(ref resources) = *resources {
            return_fenced_descriptors(
                fence_state.queue.drain_ready(),
                &resources.ctrl_queue,
                resources
                    .cursor_queue
                    .as_ref()
                    .map(|q| q as &dyn QueueReader),
            );
        }
        if fence_state.waiting_for_context_destroy {
            if let Some(event) = &fence_state.context_retirement_event {
                let _ = event.signal();
            }
        }
    })
}

pub fn create_fence_error_handler(
    fence_state: Arc<Mutex<FenceState>>,
) -> RutabagaFenceErrorHandler {
    RutabagaFenceErrorHandler::new(move |failure: RutabagaFenceError| {
        let Ok(ring_idx) = u8::try_from(failure.ring_idx) else {
            return;
        };
        let mut state = fence_state.lock();
        if failure.error >= 0
            || state.renderer_error.is_some()
            || !state
                .context_tokens
                .contains(failure.ctx_id, ring_idx, failure.fence_id)
        {
            return;
        }
        state.renderer_error = Some(failure);
        state.queue.stop();
        // Wake even when no context destroy is parked, including inactive
        // queues. Keep tokens/descriptors and never invoke renderer teardown
        // from its synchronous callback thread.
        if let Some(event) = &state.context_retirement_event {
            let _ = event.signal();
        }
    })
}

pub struct ReturnDescriptor {
    pub desc_chain: DescriptorChain,
    pub len: u32,
    queue: GpuQueue,
    ctx_id: u32,
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum GpuQueue {
    Control,
    Cursor,
}

/// A RESOURCE_FLUSH whose virtio fence completion is parked on a display fence: the zero-copy
/// flip handed back a sync_file that signals when the display has finished reading the flipped
/// buffer, and the guest (which dma_fence-waits this virtio fence in its plane update) must not
/// reuse the dmabuf before then. The descriptor already sits in `fence_state.queue`;
/// this records what to complete and when to give up.
struct PendingFlipFence {
    fence_id: u64,
    ticket: u64,
    fence: SafeDescriptor,
    /// A deadline requests VM recovery; it never makes this buffer reusable.
    deadline: std::time::Instant,
    /// Whether the fd has been added to the worker's WaitContext yet.
    registered: bool,
}

struct DeferredContextDestroy {
    hdr: virtio_gpu_ctrl_hdr,
    descriptor: ReturnDescriptor,
    deadline: std::time::Instant,
}

/// This is a hang watchdog, not presentation pacing or a successful completion.
/// Allow delayed/loaded displays to signal normally instead of releasing their
/// source buffers after an arbitrary number of vsyncs.
const FLIP_FENCE_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(any(target_os = "android", target_os = "linux"))]
#[derive(Default)]
#[repr(C)]
struct SyncFileInfo {
    name: [u8; 32],
    status: i32,
    flags: u32,
    num_fences: u32,
    pad: u32,
    sync_fence_info: u64,
}

#[cfg(any(target_os = "android", target_os = "linux"))]
base::ioctl_iowr_nr!(SYNC_IOC_FILE_INFO, 0x3e, 4, SyncFileInfo);

/// POLLIN includes failed dma_fences. Query the sync_file's aggregate status
/// before granting reuse: 1 is success, 0 is active, and a negative value is
/// a backend error. No individual fence-info array is needed.
#[cfg(any(target_os = "android", target_os = "linux"))]
fn flip_fence_signaled(fence: &SafeDescriptor) -> anyhow::Result<bool> {
    let mut pfd = libc::pollfd {
        fd: fence.as_raw_descriptor(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: pfd is valid for one element throughout this nonblocking call.
    let result = unsafe { libc::poll(&mut pfd, 1, 0) };
    if result < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(error).context("polling display completion fence");
    }
    if pfd.revents & (libc::POLLERR | libc::POLLNVAL | libc::POLLHUP) != 0 {
        return Err(anyhow!("display fence poll error flags {:#x}", pfd.revents));
    }
    if result == 0 || pfd.revents & libc::POLLIN == 0 {
        return Ok(false);
    }
    let mut info = SyncFileInfo::default();
    // SAFETY: the mutable buffer matches the sync_file UAPI. num_fences=0
    // requests only the aggregate status, without a userspace array pointer.
    if unsafe { base::ioctl_with_mut_ref(fence, SYNC_IOC_FILE_INFO, &mut info) } < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(error).context("querying display sync_file status");
    }
    match info.status {
        0 => Ok(false),
        1 => Ok(true),
        status => Err(anyhow!("display sync_file failed with status {}", status)),
    }
}

#[cfg(not(any(target_os = "android", target_os = "linux")))]
fn flip_fence_signaled(_fence: &SafeDescriptor) -> anyhow::Result<bool> {
    Err(anyhow!(
        "display sync_file fences require a Linux or Android host"
    ))
}

pub struct Frontend {
    fence_state: Arc<Mutex<FenceState>>,
    virtio_gpu: VirtioGpu,
    pending_flip_fences: Vec<PendingFlipFence>,
    next_flip_ticket: u64,
    display_failed: bool,
    maps_global_fences: bool,
    context_retirement_event: Event,
    deferred_context_destroy: Option<DeferredContextDestroy>,
}

impl Frontend {
    fn new(virtio_gpu: VirtioGpu, fence_state: Arc<Mutex<FenceState>>) -> anyhow::Result<Frontend> {
        let maps_global_fences = virtio_gpu.uses_virgl_global_fences();
        fence_state.lock().uses_global_tokens = maps_global_fences;
        let context_retirement_event = Event::new()?;
        fence_state.lock().context_retirement_event = Some(context_retirement_event.try_clone()?);
        Ok(Frontend {
            fence_state,
            virtio_gpu,
            pending_flip_fences: Vec::new(),
            next_flip_ticket: 0,
            display_failed: false,
            maps_global_fences,
            context_retirement_event,
            deferred_context_destroy: None,
        })
    }

    fn context_has_pending_work(&self, ctx_id: u32) -> bool {
        let state = self.fence_state.lock();
        state.context_tokens.has_context(ctx_id)
            || state.queue.any_pending(|desc| desc.ctx_id == ctx_id)
            || !self.pending_flip_fences.is_empty()
    }

    fn finish_context_destroy(
        &mut self,
        mut pending: DeferredContextDestroy,
    ) -> Option<ReturnDescriptor> {
        let ctx_id = pending.hdr.ctx_id.to_native();
        let response = match self.virtio_gpu.destroy_context(ctx_id) {
            Ok(response) => {
                self.fence_state.lock().queue.completed_renderer.retain(|ring, _|
                    !matches!(ring, VirtioGpuRing::ContextSpecific { ctx_id: old, .. } if *old == ctx_id));
                response
            }
            Err(response) => response,
        };
        let fenced = pending.hdr.flags.to_native() & VIRTIO_GPU_FLAG_FENCE != 0;
        let (flags, cookie, response_ctx, ring_idx) = if fenced {
            (
                pending.hdr.flags.to_native(),
                pending.hdr.fence_id.to_native(),
                ctx_id,
                pending.hdr.ring_idx,
            )
        } else {
            (0, 0, 0, 0)
        };
        pending.descriptor.len = response
            .encode(
                flags,
                cookie,
                response_ctx,
                ring_idx,
                &mut pending.descriptor.desc_chain.writer,
            )
            .unwrap_or(0);
        if fenced {
            let ring = if flags & VIRTIO_GPU_FLAG_INFO_RING_IDX == 0 {
                VirtioGpuRing::Global
            } else {
                VirtioGpuRing::ContextSpecific { ctx_id, ring_idx }
            };
            // Teardown itself completes this command; creating another context
            // fence after destroying that context would always fail.
            self.fence_state
                .lock()
                .queue
                .push_complete(ring, pending.descriptor);
            None
        } else {
            Some(pending.descriptor)
        }
    }

    fn process_context_destroy_request(
        &mut self,
        hdr: virtio_gpu_ctrl_hdr,
        desc_chain: DescriptorChain,
        source: GpuQueue,
    ) -> Option<ReturnDescriptor> {
        let pending = DeferredContextDestroy {
            hdr,
            descriptor: ReturnDescriptor {
                desc_chain,
                len: 0,
                queue: source,
                ctx_id: hdr.ctx_id.to_native(),
            },
            deadline: std::time::Instant::now() + FLIP_FENCE_TIMEOUT,
        };
        // Enable the wakeup before observing pending work, so a callback cannot
        // disappear between the check and parking the teardown descriptor.
        self.fence_state.lock().waiting_for_context_destroy = true;
        if self.context_has_pending_work(hdr.ctx_id.to_native()) {
            self.deferred_context_destroy = Some(pending);
            None
        } else {
            self.fence_state.lock().waiting_for_context_destroy = false;
            self.finish_context_destroy(pending)
        }
    }

    /// Resume a parked teardown before processing newer resource mutations.
    /// Only teardown serializes command intake; ordinary frame traffic does not.
    pub fn process_context_retirement(
        &mut self,
        mem: &GuestMemory,
        ctrl_queue: &dyn QueueReader,
        cursor_queue: Option<&dyn QueueReader>,
    ) -> anyhow::Result<()> {
        self.fence_state.lock().check_renderer()?;
        if self.display_failed {
            return Err(anyhow!("GPU recovery already pending"));
        }
        let Some(pending) = self.deferred_context_destroy.as_ref() else {
            return Ok(());
        };
        if self.context_has_pending_work(pending.hdr.ctx_id.to_native()) {
            if std::time::Instant::now() >= pending.deadline {
                self.display_failed = true;
                self.fence_state.lock().queue.stop();
                return Err(anyhow!(
                    "context {} teardown timed out; resources remain owned",
                    pending.hdr.ctx_id.to_native()
                ));
            }
            return Ok(());
        }
        let pending = self.deferred_context_destroy.take().unwrap();
        self.fence_state.lock().waiting_for_context_destroy = false;
        if let Some(desc) = self.finish_context_destroy(pending) {
            return_fenced_descriptors(vec![desc], ctrl_queue, cursor_queue);
        }
        return_fenced_descriptors(
            self.fence_state.lock().queue.drain_ready(),
            ctrl_queue,
            cursor_queue,
        );
        if self.process_queue(mem, ctrl_queue) {
            ctrl_queue.signal_used();
        }
        if let Some(cursor_queue) = cursor_queue {
            if self.process_cursor_queue(mem, cursor_queue) {
                cursor_queue.signal_used();
            }
        }
        Ok(())
    }

    pub fn context_retirement_event(&self) -> Result<Event> {
        self.context_retirement_event.try_clone()
    }

    pub fn context_retirement_deadline(&self) -> Option<std::time::Instant> {
        self.deferred_context_destroy
            .as_ref()
            .map(|pending| pending.deadline)
    }

    pub fn check_quiescent(&self) -> anyhow::Result<()> {
        let state = self.fence_state.lock();
        state.check_renderer()?;
        if self.display_failed
            || self.deferred_context_destroy.is_some()
            || !self.pending_flip_fences.is_empty()
            || !state.queue.is_empty()
            || !state.global_tokens.is_empty()
            || !state.context_tokens.is_empty()
        {
            return Err(anyhow!("GPU still owns descriptors or pending work"));
        }
        Ok(())
    }

    /// Registers any not-yet-registered flip fences with the worker's WaitContext so their
    /// signal wakes the worker exactly when the display is done, rather than on the timeout.
    fn register_flip_fences(&mut self, wait_ctx: &WaitContext<WorkerToken>) -> anyhow::Result<()> {
        for pending in self.pending_flip_fences.iter_mut() {
            if !pending.registered {
                wait_ctx
                    .add(&pending.fence, WorkerToken::FlipFence)
                    .context("registering display completion fence")?;
                pending.registered = true;
            }
        }
        Ok(())
    }

    /// The nearest deadline among pending flip fences, if any -- the worker bounds its wait by
    /// this so the hang watchdog runs even when no other event wakes the worker.
    fn next_flip_fence_deadline(&self) -> Option<std::time::Instant> {
        self.pending_flip_fences
            .iter()
            .map(|f| f.deadline)
            .chain(self.context_retirement_deadline())
            .min()
    }

    /// Complete only successfully signaled fences. On timeout/error keep all
    /// ownership and let the worker request recovery without publishing reuse.
    fn complete_flip_fences(
        &mut self,
        ctrl_queue: &dyn QueueReader,
        cursor_queue: &dyn QueueReader,
        wait_ctx: &WaitContext<WorkerToken>,
    ) -> anyhow::Result<()> {
        self.complete_flip_fences_with(ctrl_queue, cursor_queue, wait_ctx, flip_fence_signaled)
    }

    fn complete_flip_fences_with(
        &mut self,
        ctrl_queue: &dyn QueueReader,
        cursor_queue: &dyn QueueReader,
        wait_ctx: &WaitContext<WorkerToken>,
        mut is_signaled: impl FnMut(&SafeDescriptor) -> anyhow::Result<bool>,
    ) -> anyhow::Result<()> {
        let now = std::time::Instant::now();
        let mut i = 0;
        while i < self.pending_flip_fences.len() {
            let pending = &self.pending_flip_fences[i];
            let signaled = is_signaled(&pending.fence)?;
            if !signaled && now < pending.deadline {
                i += 1;
                continue;
            }
            if !signaled {
                return Err(anyhow!(
                    "flip fence {} timed out after {:?}; source remains owned by display",
                    pending.fence_id,
                    FLIP_FENCE_TIMEOUT
                ));
            }
            let pending = self.pending_flip_fences.remove(i);
            if pending.registered {
                let _ = wait_ctx.delete(&pending.fence);
            }
            // One command may start several scanout/cursor readers. A single
            // successful reader cannot release their shared source allocation.
            if self
                .pending_flip_fences
                .iter()
                .any(|f| f.ticket == pending.ticket)
            {
                continue;
            }
            // A display fence releases only its own source buffer. It cannot
            // complete earlier renderer work or advance a renderer watermark.
            let mut fence_state = self.fence_state.lock();
            fence_state.queue.complete_display(pending.ticket);
            let completed = fence_state.queue.drain_ready();
            // Signal before observing another fence, which may report failure.
            return_fenced_descriptors(completed, ctrl_queue, Some(cursor_queue));
        }
        Ok(())
    }

    /// Returns the internal connection to the compositor and its associated state.
    pub fn display(&mut self) -> &Rc<RefCell<GpuDisplay>> {
        self.virtio_gpu.display()
    }

    /// Processes the internal `display` events and returns `true` if any display was closed.
    pub fn process_display(&mut self) -> ProcessDisplayResult {
        self.virtio_gpu.process_display()
    }

    /// Processes incoming requests on `resource_bridge`.
    pub fn process_resource_bridge(&mut self, resource_bridge: &Tube) -> anyhow::Result<()> {
        let response = match resource_bridge.recv() {
            Ok(ResourceRequest::GetBuffer { id }) => self.virtio_gpu.export_resource(id),
            Ok(ResourceRequest::GetFence { seqno }) => {
                if self.maps_global_fences {
                    // Guest cookies are not host tokens. Enable this only with
                    // an export/alias registry and safe backend fd ownership.
                    ResourceResponse::Invalid
                } else {
                    self.virtio_gpu.export_fence(seqno)
                }
            }
            Ok(ResourceRequest::GetSignaledFence) => {
                if self.maps_global_fences {
                    self.virtio_gpu.export_signaled_fence()
                } else {
                    // Preserve the existing non-Virgl backend's fence-zero contract.
                    self.virtio_gpu.export_fence(0)
                }
            }
            Err(e) => return Err(e).context("Error receiving resource bridge request"),
        };

        resource_bridge
            .send(&response)
            .context("Error sending resource bridge response")?;

        Ok(())
    }

    /// Processes the GPU control command and returns the result with a bool indicating if the
    /// GPU device's config needs to be updated.
    pub fn process_gpu_control_command(&mut self, cmd: GpuControlCommand) -> GpuControlResult {
        self.virtio_gpu.process_gpu_control_command(cmd)
    }

    fn process_gpu_command(
        &mut self,
        mem: &GuestMemory,
        cmd: GpuCommand,
        reader: &mut Reader,
    ) -> VirtioGpuResult {
        let is_drm_submit = matches!(
            &cmd,
            GpuCommand::CmdSubmit3d(info)
                if self.virtio_gpu.context_uses_capset(
                    info.hdr.ctx_id.to_native(),
                    RUTABAGA_CAPSET_DRM,
                )
        );
        if !is_drm_submit {
            self.virtio_gpu.force_ctx_0();
        }

        match cmd {
            GpuCommand::GetDisplayInfo(_) => Ok(GpuResponse::OkDisplayInfo(
                self.virtio_gpu.display_info().to_vec(),
            )),
            GpuCommand::ResourceCreate2d(info) => {
                let resource_id = info.resource_id.to_native();

                let resource_create_3d = ResourceCreate3D {
                    target: RUTABAGA_PIPE_TEXTURE_2D,
                    format: info.format.to_native(),
                    bind: RUTABAGA_PIPE_BIND_RENDER_TARGET,
                    width: info.width.to_native(),
                    height: info.height.to_native(),
                    depth: 1,
                    array_size: 1,
                    last_level: 0,
                    nr_samples: 0,
                    flags: 0,
                };

                self.virtio_gpu
                    .resource_create_3d(resource_id, resource_create_3d)
            }
            GpuCommand::ResourceUnref(info) => self
                .virtio_gpu
                .unref_resource(mem, info.resource_id.to_native()),
            GpuCommand::GetDisplayColor(info) => {
                if reader.available_bytes() != 0 { return Err(GpuResponse::ErrInvalidParameter); }
                self.virtio_gpu.get_display_color(info)
            }
            GpuCommand::QuerySharedOwner(info) | GpuCommand::CleanupSharedOwner(info) => {
                if reader.available_bytes() != 0 { return Err(GpuResponse::ErrInvalidParameter); }
                let cleanup = info.query.hdr.type_.to_native() == shared_allocation_protocol::CMD_CLEANUP_SHARED_OWNER;
                self.virtio_gpu.recover_shared_owner(info, cleanup)
            }
            GpuCommand::AllocateRecoverable(info) => {
                if reader.available_bytes() != 0 { return Err(GpuResponse::ErrInvalidParameter); }
                self.virtio_gpu.allocate_recoverable(info)
            }
            GpuCommand::DiscoverSharedAllocation(info) => {
                if reader.available_bytes() != 0 { return Err(GpuResponse::ErrInvalidParameter); }
                self.virtio_gpu.discover_shared_allocation(info)
            }
            GpuCommand::AllocateSharedAllocation(info) => {
                if reader.available_bytes() != 0 { return Err(GpuResponse::ErrInvalidParameter); }
                self.virtio_gpu.allocate_shared_allocation(info)
            }
            GpuCommand::AcknowledgeSharedAllocation(info) => {
                if reader.available_bytes() != 0 { return Err(GpuResponse::ErrInvalidParameter); }
                self.virtio_gpu.acknowledge_shared_allocation(info)
            }
            GpuCommand::DestroySharedAllocation(info) => {
                if reader.available_bytes() != 0 { return Err(GpuResponse::ErrInvalidParameter); }
                self.virtio_gpu.destroy_shared_allocation(info)
            }
            GpuCommand::SetResourceColor(info) => {
                if reader.available_bytes() != 0 { return Err(GpuResponse::ErrInvalidParameter); }
                self.virtio_gpu.set_resource_color(info)
            }
            GpuCommand::SetTargetTransform(info) => {
                use display_color_protocol::TransformPayload;
                if reader.available_bytes() != std::mem::size_of::<TransformPayload>() {
                    return Err(GpuResponse::ErrInvalidParameter);
                }
                let payload: TransformPayload = reader.read_obj().map_err(|_| GpuResponse::ErrInvalidParameter)?;
                self.virtio_gpu.set_target_transform(info, payload)
            }
            GpuCommand::SetScanout(info) => self.virtio_gpu.set_scanout(
                info.r,
                info.scanout_id.to_native(),
                info.resource_id.to_native(),
                None,
            ),
            GpuCommand::ResourceFlush(info) => {
                self.virtio_gpu.flush_resource(info.resource_id.to_native())
            }
            GpuCommand::TransferToHost2d(info) => {
                let resource_id = info.resource_id.to_native();
                let transfer = Transfer3D::new_2d(
                    info.r.x.to_native(),
                    info.r.y.to_native(),
                    info.r.width.to_native(),
                    info.r.height.to_native(),
                    info.offset.to_native(),
                );
                self.virtio_gpu.transfer_write(0, resource_id, transfer)
            }
            GpuCommand::ResourceAttachBacking(info) => {
                let available_bytes = reader.available_bytes();
                if available_bytes != 0 {
                    let entry_count =
                        checked_entry_count(info.nr_entries.to_native(), available_bytes)?;
                    let mut vecs = Vec::with_capacity(entry_count);
                    for _ in 0..entry_count {
                        match reader.read_obj::<virtio_gpu_mem_entry>() {
                            Ok(entry) => {
                                let addr = GuestAddress(entry.addr.to_native());
                                let len = entry.length.to_native() as usize;
                                vecs.push((addr, len))
                            }
                            Err(_) => return Err(GpuResponse::ErrUnspec),
                        }
                    }
                    self.virtio_gpu
                        .attach_backing(info.resource_id.to_native(), mem, vecs)
                } else {
                    error!("missing data for command {:?}", cmd);
                    Err(GpuResponse::ErrUnspec)
                }
            }
            GpuCommand::ResourceDetachBacking(info) => {
                self.virtio_gpu.detach_backing(info.resource_id.to_native())
            }
            // pos is the guest's crtc_x/crtc_y for the cursor plane -- the image's top-left corner,
            // already hotspot-compensated, and SIGNED. Reading it unsigned turns a pointer against
            // the left edge into a position near 4.29e9.
            GpuCommand::UpdateCursor(info) => self.virtio_gpu.update_cursor(
                info.resource_id.to_native(),
                info.pos.scanout_id.to_native(),
                info.pos.x.to_native() as i32,
                info.pos.y.to_native() as i32,
                info.hot_x.into(),
                info.hot_y.into(),
            ),
            GpuCommand::MoveCursor(info) => self.virtio_gpu.move_cursor(
                info.pos.scanout_id.to_native(),
                info.pos.x.to_native() as i32,
                info.pos.y.to_native() as i32,
            ),
            GpuCommand::ResourceAssignUuid(info) => {
                let resource_id = info.resource_id.to_native();
                self.virtio_gpu.resource_assign_uuid(resource_id)
            }
            GpuCommand::GetCapsetInfo(info) => self
                .virtio_gpu
                .get_capset_info(info.capset_index.to_native()),
            GpuCommand::GetCapset(info) => self
                .virtio_gpu
                .get_capset(info.capset_id.to_native(), info.capset_version.to_native()),
            GpuCommand::CtxCreate(info) => {
                let context_name: Option<String> = String::from_utf8(info.debug_name.to_vec()).ok();
                self.virtio_gpu.create_context(
                    info.hdr.ctx_id.to_native(),
                    info.context_init.to_native(),
                    context_name.as_deref(),
                )
            }
            GpuCommand::CtxDestroy(info) => {
                self.virtio_gpu.destroy_context(info.hdr.ctx_id.to_native())
            }
            GpuCommand::CtxAttachResource(info) => self
                .virtio_gpu
                .context_attach_resource(info.hdr.ctx_id.to_native(), info.resource_id.to_native()),
            GpuCommand::CtxDetachResource(info) => self
                .virtio_gpu
                .context_detach_resource(info.hdr.ctx_id.to_native(), info.resource_id.to_native()),
            GpuCommand::ResourceCreate3d(info) => {
                let resource_id = info.resource_id.to_native();
                let resource_create_3d = ResourceCreate3D {
                    target: info.target.to_native(),
                    format: info.format.to_native(),
                    bind: info.bind.to_native(),
                    width: info.width.to_native(),
                    height: info.height.to_native(),
                    depth: info.depth.to_native(),
                    array_size: info.array_size.to_native(),
                    last_level: info.last_level.to_native(),
                    nr_samples: info.nr_samples.to_native(),
                    flags: info.flags.to_native(),
                };

                self.virtio_gpu
                    .resource_create_3d(resource_id, resource_create_3d)
            }
            GpuCommand::TransferToHost3d(info) => {
                let ctx_id = info.hdr.ctx_id.to_native();
                let resource_id = info.resource_id.to_native();

                let transfer = Transfer3D {
                    x: info.box_.x.to_native(),
                    y: info.box_.y.to_native(),
                    z: info.box_.z.to_native(),
                    w: info.box_.w.to_native(),
                    h: info.box_.h.to_native(),
                    d: info.box_.d.to_native(),
                    level: info.level.to_native(),
                    stride: info.stride.to_native(),
                    layer_stride: info.layer_stride.to_native(),
                    offset: info.offset.to_native(),
                };

                self.virtio_gpu
                    .transfer_write(ctx_id, resource_id, transfer)
            }
            GpuCommand::TransferFromHost3d(info) => {
                let ctx_id = info.hdr.ctx_id.to_native();
                let resource_id = info.resource_id.to_native();

                let transfer = Transfer3D {
                    x: info.box_.x.to_native(),
                    y: info.box_.y.to_native(),
                    z: info.box_.z.to_native(),
                    w: info.box_.w.to_native(),
                    h: info.box_.h.to_native(),
                    d: info.box_.d.to_native(),
                    level: info.level.to_native(),
                    stride: info.stride.to_native(),
                    layer_stride: info.layer_stride.to_native(),
                    offset: info.offset.to_native(),
                };

                self.virtio_gpu
                    .transfer_read(ctx_id, resource_id, transfer, None)
            }
            GpuCommand::CmdSubmit3d(info) => {
                if self.maps_global_fences && info.num_in_fences.to_native() != 0 {
                    return Err(GpuResponse::ErrInvalidParameter);
                }
                if reader.available_bytes() != 0 {
                    let num_in_fences = info.num_in_fences.to_native() as usize;
                    let cmd_size = info.size.to_native() as usize;
                    // Same shape as nr_entries above: both are guest u32s used as an allocation
                    // size before anything is read. This one is transient rather than retained,
                    // but `vec![0; n]` writes, so it is the one that actually touches the pages.
                    let avail = reader.available_bytes();
                    if cmd_size > avail || num_in_fences > avail / size_of::<Le64>() {
                        return Err(GpuResponse::ErrUnspec);
                    }
                    let mut cmd_buf = vec![0; cmd_size];
                    let mut fence_ids: Vec<u64> = Vec::with_capacity(num_in_fences);
                    let ctx_id = info.hdr.ctx_id.to_native();

                    for _ in 0..num_in_fences {
                        match reader.read_obj::<Le64>() {
                            Ok(fence_id) => {
                                fence_ids.push(fence_id.to_native());
                            }
                            Err(_) => return Err(GpuResponse::ErrUnspec),
                        }
                    }

                    if reader.read_exact(&mut cmd_buf[..]).is_ok() {
                        self.virtio_gpu
                            .submit_command(ctx_id, &mut cmd_buf[..], &fence_ids[..])
                    } else {
                        Err(GpuResponse::ErrInvalidParameter)
                    }
                } else {
                    // Silently accept empty command buffers to allow for
                    // benchmarking.
                    Ok(GpuResponse::OkNoData)
                }
            }
            GpuCommand::ResourceCreateBlob(info) => {
                let resource_id = info.resource_id.to_native();
                let ctx_id = info.hdr.ctx_id.to_native();

                let resource_create_blob = ResourceCreateBlob {
                    blob_mem: info.blob_mem.to_native(),
                    blob_flags: info.blob_flags.to_native(),
                    blob_id: info.blob_id.to_native(),
                    size: info.size.to_native(),
                };

                let entry_count = info.nr_entries.to_native();
                if reader.available_bytes() == 0 && entry_count > 0 {
                    return Err(GpuResponse::ErrUnspec);
                }
                let entry_count = checked_entry_count(entry_count, reader.available_bytes())?;

                let mut vecs = Vec::with_capacity(entry_count);
                for _ in 0..entry_count {
                    match reader.read_obj::<virtio_gpu_mem_entry>() {
                        Ok(entry) => {
                            let addr = GuestAddress(entry.addr.to_native());
                            let len = entry.length.to_native() as usize;
                            vecs.push((addr, len))
                        }
                        Err(_) => return Err(GpuResponse::ErrUnspec),
                    }
                }

                self.virtio_gpu.resource_create_blob(
                    ctx_id,
                    resource_id,
                    resource_create_blob,
                    vecs,
                    mem,
                )
            }
            GpuCommand::SetScanoutBlob(info) => {
                let scanout_id = info.scanout_id.to_native();
                let resource_id = info.resource_id.to_native();
                let virtio_gpu_format = info.format.to_native();
                let width = info.width.to_native();
                let height = info.height.to_native();
                let mut strides: [u32; 4] = [0; 4];
                let mut offsets: [u32; 4] = [0; 4];

                // As of v4.19, virtio-gpu kms only really uses these formats.  If that changes,
                // the following may have to change too.
                let drm_format = match virtio_gpu_format {
                    VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM => DrmFormat::new(b'X', b'R', b'2', b'4'),
                    VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM => DrmFormat::new(b'A', b'R', b'2', b'4'),
                    VIRTIO_GPU_FORMAT_R8G8B8A8_UNORM => DrmFormat::new(b'A', b'B', b'2', b'4'),
                    _ => {
                        error!("unrecognized virtio-gpu format {}", virtio_gpu_format);
                        return Err(GpuResponse::ErrUnspec);
                    }
                };

                for plane_index in 0..PLANE_INFO_MAX_COUNT {
                    offsets[plane_index] = info.offsets[plane_index].to_native();
                    strides[plane_index] = info.strides[plane_index].to_native();
                }

                let scanout = VirtioScanoutBlobData {
                    width,
                    height,
                    drm_format,
                    strides,
                    offsets,
                };

                self.virtio_gpu
                    .set_scanout(info.r, scanout_id, resource_id, Some(scanout))
            }
            GpuCommand::ResourceMapBlob(info) => {
                let resource_id = info.resource_id.to_native();
                let offset = info.offset.to_native();
                self.virtio_gpu.resource_map_blob(resource_id, offset)
            }
            GpuCommand::ResourceUnmapBlob(info) => {
                let resource_id = info.resource_id.to_native();
                self.virtio_gpu.resource_unmap_blob(resource_id)
            }
            GpuCommand::GetEdid(info) => self.virtio_gpu.get_edid(info.scanout.to_native()),
        }
    }

    /// Processes virtio messages on `queue`.
    pub fn process_queue(&mut self, mem: &GuestMemory, queue: &dyn QueueReader) -> bool {
        self.process_queue_from(mem, queue, GpuQueue::Control)
    }

    fn process_cursor_queue(&mut self, mem: &GuestMemory, queue: &dyn QueueReader) -> bool {
        self.process_queue_from(mem, queue, GpuQueue::Cursor)
    }

    fn process_queue_from(
        &mut self,
        mem: &GuestMemory,
        queue: &dyn QueueReader,
        source: GpuQueue,
    ) -> bool {
        if self.display_failed || self.deferred_context_destroy.is_some() {
            return false;
        }
        let mut signal_used = false;
        loop {
            if self.fence_state.lock().renderer_error.is_some() {
                break;
            }
            let Some(desc) = queue.pop() else {
                break;
            };
            if let Some(ret_desc) = self.process_descriptor(mem, desc, source) {
                let mut state = self.fence_state.lock();
                if state.renderer_error.is_some() {
                    state.queue.push_complete(VirtioGpuRing::Global, ret_desc);
                } else {
                    queue.add_used(ret_desc.desc_chain, ret_desc.len);
                    signal_used = true;
                }
            }
            if self.display_failed || self.deferred_context_destroy.is_some() {
                break;
            }
        }

        // An inline renderer callback may precede response publication. Drain
        // here as well, retaining same-ring order behind pending display work.
        for completed in self
            .fence_state
            .lock()
            .queue
            .drain_ready_if(|d| d.queue == source)
        {
            queue.add_used(completed.desc_chain, completed.len);
            signal_used = true;
        }

        // This batch may have parked teardown behind an inline completion
        // that the drain above just retired. Recheck even without another
        // guest kick or backend callback; never wait until the watchdog.
        if self.deferred_context_destroy.is_some() {
            let _ = self.context_retirement_event.signal();
        }

        signal_used
    }

    fn process_descriptor(
        &mut self,
        mem: &GuestMemory,
        mut desc_chain: DescriptorChain,
        source: GpuQueue,
    ) -> Option<ReturnDescriptor> {
        let decoded = GpuCommand::decode(&mut desc_chain.reader);
        if self.maps_global_fences {
            if let Ok(GpuCommand::CtxDestroy(info)) = &decoded {
                if info.hdr.flags.to_native() & VIRTIO_GPU_FLAG_FENCE_HOST_SHAREABLE == 0 {
                    return self.process_context_destroy_request(info.hdr, desc_chain, source);
                }
            }
        }
        let reader = &mut desc_chain.reader;
        let writer = &mut desc_chain.writer;
        let mut resp = Err(GpuResponse::ErrUnspec);
        let mut gpu_cmd = None;
        let mut len = 0;
        let mut global_token = None;
        let mut context_token = None;
        let mut command_ctx_id = 0;
        let mut command_rejected = false;
        match decoded {
            Ok(cmd) => {
                let hdr = cmd.ctrl_hdr();
                let flags = hdr.flags.to_native();
                command_ctx_id = hdr.ctx_id.to_native();
                if self.maps_global_fences {
                    if flags & VIRTIO_GPU_FLAG_FENCE_HOST_SHAREABLE != 0 {
                        command_rejected = true;
                        resp = Err(GpuResponse::ErrInvalidParameter);
                    } else if flags & (VIRTIO_GPU_FLAG_FENCE | VIRTIO_GPU_FLAG_INFO_RING_IDX)
                        == VIRTIO_GPU_FLAG_FENCE
                    {
                        // Reserve before command execution: exhaustion must
                        // not acknowledge work already submitted to the GPU.
                        global_token = self.fence_state.lock().global_tokens.reserve();
                        command_rejected = global_token.is_none();
                    } else if flags & VIRTIO_GPU_FLAG_FENCE != 0 {
                        context_token = self
                            .fence_state
                            .lock()
                            .context_tokens
                            .reserve(command_ctx_id, hdr.ring_idx);
                        command_rejected = context_token.is_none();
                    }
                }
                if !command_rejected {
                    resp = self.process_gpu_command(mem, cmd, reader);
                }
                gpu_cmd = Some(cmd);
            }
            Err(e) => debug!("descriptor decode error: {}", e),
        }

        if matches!(&resp, Err(GpuResponse::ErrDisplay(gpu_display::GpuDisplayError::ImportRetirement))) {
            // Even an error response would return the descriptor and permit guest
            // backing reuse. Stop publication and keep this resource/frontend until
            // VM teardown, just as for a failed asynchronous display reader.
            self.display_failed = true;
            let mut state = self.fence_state.lock();
            state.queue.stop();
            state.queue.push_complete(VirtioGpuRing::Global, ReturnDescriptor {
                desc_chain, len: 0, queue: source, ctx_id: command_ctx_id,
            });
            return None;
        }

        let mut gpu_response = match resp {
            Ok(gpu_response) => gpu_response,
            Err(gpu_response) => {
                if let Some(gpu_cmd) = gpu_cmd {
                    // Detaching a resource from a context that is already gone is what teardown
                    // looks like, not a fault: the guest driver destroys the context and frees
                    // its id in postclose, and resources of that context are detached as their
                    // GEM handles go away, which can land either side of it. The resource is
                    // being torn down regardless, so nothing is lost -- but at error level it
                    // fills the log with 732 lines per session that read like a real failure.
                    let expected_during_teardown = matches!(
                        (&gpu_cmd, &gpu_response),
                        (
                            GpuCommand::CtxDetachResource(_),
                            GpuResponse::ErrRutabaga(RutabagaError::InvalidContextId)
                        )
                    );
                    // A display with nowhere to put pixels refuses the cursor plane, and a guest
                    // moving its pointer asks again for every motion sample. The stub backend --
                    // where a GPU device lands when no exporter is bound to its screen -- returns
                    // `Unsupported` from `create_surface` for any parented surface, so a VM whose
                    // picture is somebody else's (the simplefb screen's, or nobody's) printed one
                    // ERROR per pointer sample for as long as it ran. The response the guest gets
                    // is unchanged, and the guest does not read it: cursor commands ride the
                    // cursor queue, which the driver posts to without inspecting the reply. What
                    // changes is that the log stops describing a working configuration as broken;
                    // the one INFO line at display-open already said there is no screen here.
                    let cursor_on_a_screenless_display = matches!(
                        (&gpu_cmd, &gpu_response),
                        (
                            GpuCommand::UpdateCursor(_) | GpuCommand::MoveCursor(_),
                            GpuResponse::ErrDisplay(GpuDisplayError::Unsupported)
                        )
                    );
                    if expected_during_teardown {
                        debug!(
                            "gpu command {:?} arrived after its context went away: {:?}",
                            gpu_cmd, gpu_response
                        );
                    } else if cursor_on_a_screenless_display {
                        debug!(
                            "gpu command {:?}: this display has no plane to put a cursor on",
                            gpu_cmd
                        );
                    } else {
                        error!(
                            "error processing gpu command {:?}: {:?}",
                            gpu_cmd, gpu_response
                        );
                    }
                }
                gpu_response
            }
        };

        // Retain every display reader, even on error, without FLAG_FENCE or
        // without response space. Returning the descriptor grants reuse too.
        let flip_fences = self.virtio_gpu.take_pending_flip_fences();

        if writer.available_bytes() != 0
            || global_token.is_some()
            || context_token.is_some()
            || !flip_fences.is_empty()
        {
            let mut fence_id = 0;
            let mut ctx_id = 0;
            let mut flags = 0;
            let mut ring_idx = 0;
            // Whether a fence was actually created in rutabaga. We only defer the
            // descriptor to wait for a fence if one truly exists. A failed
            // create_fence (e.g. a context-specific fence targeting a context that
            // was invalidated/detached during churn -> ErrRutabaga(InvalidContextId))
            // must NOT leave the guest blocked on a fence that will never complete:
            // that strands the descriptor forever and hard-hangs the whole VM
            // (all vCPUs idle waiting on a GPU fence). Respond with the error instead.
            let mut fence_created = false;
            // A fenced flush with a display fence skips rutabaga entirely: its virtio fence
            // completes when the display fence fires (see complete_flip_fences), not when the
            // renderer is done.  This is what backpressures the guest compositor to the display.
            let deferred_flip = !flip_fences.is_empty();
            if deferred_flip {
                if let Some(token) = global_token.take() {
                    self.fence_state.lock().global_tokens.cancel(token);
                }
                if let Some(token) = context_token.take() {
                    self.fence_state.lock().context_tokens.cancel(token);
                }
            }
            if let Some(cmd) = gpu_cmd {
                let ctrl_hdr = cmd.ctrl_hdr();
                if ctrl_hdr.flags.to_native() & VIRTIO_GPU_FLAG_FENCE != 0 {
                    flags = ctrl_hdr.flags.to_native();
                    fence_id = ctrl_hdr.fence_id.to_native();
                    ctx_id = ctrl_hdr.ctx_id.to_native();
                    ring_idx = ctrl_hdr.ring_idx;

                    if command_rejected {
                        // No command was executed and no renderer fence exists.
                    } else if deferred_flip {
                        // Display readers own completion of this descriptor.
                    } else {
                        let fence = RutabagaFence {
                            flags,
                            fence_id: global_token
                                .map(u64::from)
                                .or(context_token)
                                .unwrap_or(fence_id),
                            ctx_id,
                            ring_idx,
                        };
                        gpu_response = match self.virtio_gpu.create_fence(fence) {
                            Ok(_) => {
                                fence_created = true;
                                gpu_response
                            }
                            Err(fence_resp) => {
                                let mut state = self.fence_state.lock();
                                if state.renderer_error.is_none() {
                                    if let Some(token) = global_token.take() {
                                        state.global_tokens.cancel(token);
                                    }
                                    if let Some(token) = context_token.take() {
                                        state.context_tokens.cancel(token);
                                    }
                                }
                                warn!("create_fence {} -> {:?}", fence_id, fence_resp);
                                fence_resp
                            }
                        };
                    }
                }
            }

            // Prepare the response now, even if it is going to wait until
            // fence is complete.
            match gpu_response.encode(flags, fence_id, ctx_id, ring_idx, writer) {
                Ok(l) => len = l,
                Err(e) => debug!("ctrl queue response encode error: {}", e),
            }

            if deferred_flip {
                let ring = match flags & VIRTIO_GPU_FLAG_INFO_RING_IDX {
                    0 => VirtioGpuRing::Global,
                    _ => VirtioGpuRing::ContextSpecific { ctx_id, ring_idx },
                };
                let ticket = self.next_flip_ticket;
                self.next_flip_ticket = ticket.checked_add(1).expect("display ticket exhausted");
                self.fence_state.lock().queue.push_display(
                    ring,
                    ticket,
                    ReturnDescriptor {
                        desc_chain,
                        len,
                        queue: source,
                        ctx_id: command_ctx_id,
                    },
                );
                let deadline = std::time::Instant::now() + FLIP_FENCE_TIMEOUT;
                for fence in flip_fences {
                    self.pending_flip_fences.push(PendingFlipFence {
                        fence_id,
                        ticket,
                        fence,
                        deadline,
                        registered: false,
                    });
                }
                return None;
            }

            if flags & VIRTIO_GPU_FLAG_FENCE != 0 {
                let ring = match flags & VIRTIO_GPU_FLAG_INFO_RING_IDX {
                    0 => VirtioGpuRing::Global,
                    _ => VirtioGpuRing::ContextSpecific { ctx_id, ring_idx },
                };

                // Never bypass an earlier display dependency, even if the
                // renderer called back inline. process_queue drains ready work.
                let descriptor = ReturnDescriptor {
                    desc_chain,
                    len,
                    queue: source,
                    ctx_id: command_ctx_id,
                };
                let mut state = self.fence_state.lock();
                if fence_created {
                    state.queue.push_renderer(
                        ring,
                        global_token
                            .map(u64::from)
                            .or(context_token)
                            .unwrap_or(fence_id),
                        descriptor,
                    );
                } else {
                    // Preserve the encoded create_fence error while obeying
                    // earlier display dependencies on the same timeline.
                    state.queue.push_complete(ring, descriptor);
                }
                return None;
            }

            // No fence (or already completed fence), respond now.
        }
        Some(ReturnDescriptor {
            desc_chain,
            len,
            queue: source,
            ctx_id: command_ctx_id,
        })
    }

    pub fn event_poll(&self) {
        self.virtio_gpu.event_poll();
    }
}

#[derive(EventToken, PartialEq, Eq, Clone, Copy, Debug)]
enum WorkerToken {
    CtrlQueue,
    CursorQueue,
    Display,
    GpuControl,
    Sleep,
    Kill,
    ResourceBridge {
        index: usize,
    },
    VirtioGpuPoll,
    /// A zero-copy flip's completion fence signaled; one token covers every pending fence and
    /// the handler polls each to find the signaled ones (they are few and short-lived).
    FlipFence,
    ContextRetirement,
    /// The VMM's simplefb bridge offered a frame (see `ExternalScanout`).
    ExternalFrame,
    #[cfg(windows)]
    DisplayDescriptorRequest,
}

struct EventManager<'a> {
    pub wait_ctx: WaitContext<WorkerToken>,
    events: Vec<(&'a dyn AsRawDescriptor, WorkerToken)>,
}

impl<'a> EventManager<'a> {
    pub fn new() -> Result<EventManager<'a>> {
        Ok(EventManager {
            wait_ctx: WaitContext::new()?,
            events: vec![],
        })
    }

    pub fn build_with(
        triggers: &[(&'a dyn AsRawDescriptor, WorkerToken)],
    ) -> Result<EventManager<'a>> {
        let mut manager = EventManager::new()?;
        manager.wait_ctx.add_many(triggers)?;

        for (descriptor, token) in triggers {
            manager.events.push((*descriptor, *token));
        }
        Ok(manager)
    }

    pub fn add(&mut self, descriptor: &'a dyn AsRawDescriptor, token: WorkerToken) -> Result<()> {
        self.wait_ctx.add(descriptor, token)?;
        self.events.push((descriptor, token));
        Ok(())
    }

    pub fn delete(&mut self, token: WorkerToken) {
        self.events.retain(|event| {
            if event.1 == token {
                self.wait_ctx.delete(event.0).ok();
                return false;
            }
            true
        });
    }
}

#[derive(Serialize, Deserialize)]
struct WorkerSnapshot {
    fence_state_snapshot: FenceStateSnapshot,
    virtio_gpu_snapshot: VirtioGpuSnapshot,
}

struct WorkerActivateRequest {
    resources: GpuActivationResources,
}

enum WorkerRequest {
    Activate(WorkerActivateRequest),
    Suspend,
    Reset,
    Snapshot,
    Restore(WorkerSnapshot),
}

enum WorkerResponse {
    Ok,
    Suspend(GpuDeactivationResources),
    Snapshot(WorkerSnapshot),
}

struct GpuActivationResources {
    mem: GuestMemory,
    interrupt: Interrupt,
    ctrl_queue: SharedQueueReader,
    cursor_queue: SharedQueueReader,
}

struct GpuDeactivationResources {
    queues: Option<Vec<Queue>>,
}

struct Worker {
    request_receiver: mpsc::Receiver<WorkerRequest>,
    response_sender: mpsc::Sender<anyhow::Result<WorkerResponse>>,
    exit_evt_wrtube: SendTube,
    gpu_control_tube: Tube,
    resource_bridges: ResourceBridges,
    suspend_evt: Event,
    kill_evt: Event,
    state: Frontend,
    fence_state: Arc<Mutex<FenceState>>,
    fence_handler_resources: Arc<Mutex<Option<FenceHandlerActivationResources<SharedQueueReader>>>>,
    #[cfg(windows)]
    gpu_display_wait_descriptor_ctrl_rd: RecvTube,
    activation_resources: Option<GpuActivationResources>,
    /// Frames from the VMM's simplefb bridge, shown while the guest is not displaying through
    /// this device (see `ExternalScanout`).
    external_scanout: Option<Arc<ExternalScanout>>,
}

#[derive(Copy, Clone)]
enum WorkerStopReason {
    Sleep,
    Kill,
}

enum WorkerState {
    Inactive,
    Active,
    Error,
}

impl Worker {
    fn new(
        rutabaga_builder: RutabagaBuilder,
        rutabaga_server_descriptor: Option<RutabagaDescriptor>,
        display_backends: Vec<DisplayBackend>,
        display_params: Vec<GpuDisplayParameters>,
        display_event: Arc<AtomicBool>,
        mapper: Arc<Mutex<Option<Box<dyn SharedMemoryMapper>>>>,
        event_devices: Vec<EventDevice>,
        external_blob: bool,
        fixed_blob_mapping: bool,
        udmabuf: bool,
        request_receiver: mpsc::Receiver<WorkerRequest>,
        response_sender: mpsc::Sender<anyhow::Result<WorkerResponse>>,
        exit_evt_wrtube: SendTube,
        gpu_control_tube: Tube,
        resource_bridges: ResourceBridges,
        suspend_evt: Event,
        kill_evt: Event,
        #[cfg(windows)] mut wndproc_thread: Option<WindowProcedureThread>,
        #[cfg(windows)] gpu_display_wait_descriptor_ctrl_rd: RecvTube,
        #[cfg(windows)] gpu_display_wait_descriptor_ctrl_wr: SendTube,
        snapshot_scratch_directory: Option<PathBuf>,
        dmabuf_import_capped: bool,
        external_scanout: Option<Arc<ExternalScanout>>,
    ) -> anyhow::Result<Worker> {
        let fence_state = Arc::new(Mutex::new(Default::default()));
        let fence_handler_resources = Arc::new(Mutex::new(None));
        let fence_handler =
            create_fence_handler(fence_handler_resources.clone(), fence_state.clone());
        let rutabaga = rutabaga_builder
            .set_fence_error_handler(create_fence_error_handler(fence_state.clone()))
            .build(fence_handler, rutabaga_server_descriptor)?;
        let mut virtio_gpu = build(
            &display_backends,
            display_params,
            display_event,
            rutabaga,
            mapper,
            external_blob,
            fixed_blob_mapping,
            #[cfg(windows)]
            &mut wndproc_thread,
            udmabuf,
            #[cfg(windows)]
            gpu_display_wait_descriptor_ctrl_wr,
            snapshot_scratch_directory,
            dmabuf_import_capped,
        )
        .ok_or_else(|| anyhow!("failed to build virtio gpu"))?;

        for event_device in event_devices {
            virtio_gpu
                .import_event_device(event_device)
                // We lost the `EventDevice`, so fail hard.
                .context("failed to import event device")?;
        }

        Ok(Worker {
            request_receiver,
            response_sender,
            exit_evt_wrtube,
            gpu_control_tube,
            resource_bridges,
            suspend_evt,
            kill_evt,
            state: Frontend::new(virtio_gpu, fence_state.clone())?,
            fence_state,
            fence_handler_resources,
            #[cfg(windows)]
            gpu_display_wait_descriptor_ctrl_rd,
            activation_resources: None,
            external_scanout,
        })
    }

    fn run(&mut self) {
        // This loop effectively only runs while the worker is inactive. Once activated via
        // a `WorkerRequest::Activate`, the worker will remain in `run_until_sleep_or_exit()`
        // until suspended via `kill_evt` or `suspend_evt` being signaled.
        //
        // "Inactive" is not the same as "nothing to display". A guest with no virtio-gpu driver
        // resets the device on its way out of the firmware and never activates it again -- that
        // is exactly what Windows does, whose Basic Display Driver only knows the linear
        // framebuffer the firmware left behind -- and the simplefb bridge feeding that
        // framebuffer to this device is still running. Waiting on the request channel alone would
        // park this thread for the rest of the VM's life with the firmware's last frame frozen on
        // screen, which is the one case the whole arbitration exists for. So while there is a
        // bridge, wait with a timeout and serve it in between.
        loop {
            let request = if self.external_scanout.is_some() && !self.state.display_failed {
                match self.request_receiver.recv_timeout(EXTERNAL_IDLE_POLL) {
                    Ok(r) => r,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if let Some(external) = self.external_scanout.clone() {
                            serve_external_scanout(&external, &mut self.state.virtio_gpu);
                        }
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        info!("virtio gpu worker connection ended, exiting.");
                        return;
                    }
                }
            } else {
                match self.request_receiver.recv() {
                    Ok(r) => r,
                    Err(_) => {
                        info!("virtio gpu worker connection ended, exiting.");
                        return;
                    }
                }
            };

            match request {
                WorkerRequest::Activate(request) => {
                    let response = self.on_activate(request).map(|_| WorkerResponse::Ok);
                    let activated = response.is_ok();
                    self.response_sender
                        .send(response)
                        .expect("failed to send gpu worker response for activate");

                    if !activated {
                        continue;
                    }

                    let stop_reason = self
                        .run_until_sleep_or_exit()
                        .expect("failed to run gpu worker processing");

                    if let WorkerStopReason::Kill = stop_reason {
                        break;
                    }
                }
                WorkerRequest::Suspend => {
                    let response = self.on_suspend().map(WorkerResponse::Suspend);
                    self.response_sender
                        .send(response)
                        .expect("failed to send gpu worker response for suspend");
                }
                WorkerRequest::Reset => {
                    let response = self.on_reset().map(|_| WorkerResponse::Ok);
                    self.response_sender
                        .send(response)
                        .expect("failed to send gpu worker response for reset");
                }
                WorkerRequest::Snapshot => {
                    let response = self.on_snapshot().map(WorkerResponse::Snapshot);
                    self.response_sender
                        .send(response)
                        .expect("failed to send gpu worker response for snapshot");
                }
                WorkerRequest::Restore(snapshot) => {
                    let response = self.on_restore(snapshot).map(|_| WorkerResponse::Ok);
                    self.response_sender
                        .send(response)
                        .expect("failed to send gpu worker response for restore");
                }
            }
        }
    }

    fn on_activate(&mut self, request: WorkerActivateRequest) -> anyhow::Result<()> {
        self.fence_state.lock().check_renderer()?;
        if self.state.display_failed {
            return Err(anyhow!("display recovery requires VM teardown"));
        }
        self.fence_handler_resources
            .lock()
            .replace(FenceHandlerActivationResources {
                mem: request.resources.mem.clone(),
                ctrl_queue: request.resources.ctrl_queue.clone(),
                cursor_queue: Some(request.resources.cursor_queue.clone()),
            });

        self.state
            .virtio_gpu
            .resume(&request.resources.mem)
            .context("gpu worker failed to activate virtio frontend")?;

        return_fenced_descriptors(
            self.fence_state.lock().queue.drain_ready(),
            &request.resources.ctrl_queue,
            Some(&request.resources.cursor_queue),
        );
        self.activation_resources = Some(request.resources);

        Ok(())
    }

    fn on_suspend(&mut self) -> anyhow::Result<GpuDeactivationResources> {
        self.check_display_deactivation()?;
        self.state
            .virtio_gpu
            .suspend()
            .context("failed to suspend VirtioGpu")?;

        self.fence_handler_resources.lock().take();

        let queues = if let Some(activation_resources) = self.activation_resources.take() {
            Some(vec![
                match Arc::try_unwrap(activation_resources.ctrl_queue.queue) {
                    Ok(x) => x.into_inner(),
                    Err(_) => panic!("too many refs on ctrl_queue"),
                },
                match Arc::try_unwrap(activation_resources.cursor_queue.queue) {
                    Ok(x) => x.into_inner(),
                    Err(_) => panic!("too many refs on cursor_queue"),
                },
            ])
        } else {
            None
        };

        Ok(GpuDeactivationResources { queues })
    }

    fn on_reset(&mut self) -> anyhow::Result<()> {
        self.check_display_deactivation()?;
        if self.state.maps_global_fences {
            let mut fences = self.fence_state.lock();
            if !fences.queue.is_empty()
                || !fences.global_tokens.is_empty()
                || !fences.context_tokens.is_empty()
            {
                // Old descriptors cannot be written to a new activation's
                // virtqueue. Keep them until renderer/VM teardown is proven.
                fences.queue.stop();
                self.state.display_failed = true;
                let _ = self.exit_evt_wrtube.send::<VmEventType>(&VmEventType::Exit);
                return Err(anyhow!("cannot reset GPU with pending renderer fences"));
            }
        }
        // Deactivate without tearing down: release the virtqueues/interrupt and the fence
        // handler's activation resources, but keep the worker thread + rutabaga/render server
        // alive so the next activate() succeeds. (rutabaga is intentionally NOT suspended here --
        // on_activate() resumes unconditionally, which already happens on the very first activate,
        // so an unpaired resume is safe.)
        self.fence_handler_resources.lock().take();
        self.activation_resources = None;
        // Drop all guest resources/contexts so the next guest (e.g. the OS after UEFI firmware
        // used then reset the device) can recreate resource ids from a clean slate.
        self.state
            .virtio_gpu
            .reset()
            .context("failed to reset VirtioGpu")?;
        Ok(())
    }

    /// The existing display backend has no reset operation that proves its
    /// asynchronous reads have stopped. Preserve its resources and request VM
    /// exit instead of acknowledging reset/suspend and releasing guest pages.
    fn check_display_deactivation(&mut self) -> anyhow::Result<()> {
        if self.state.display_failed
            || self.fence_state.lock().renderer_error.is_some()
            || !self.state.pending_flip_fences.is_empty()
            || self.state.deferred_context_destroy.is_some()
        {
            self.state.display_failed = true;
            self.fence_state.lock().queue.stop();
            let _ = self.exit_evt_wrtube.send::<VmEventType>(&VmEventType::Exit);
            return Err(anyhow!(
                "cannot deactivate GPU with unresolved display ownership"
            ));
        }
        Ok(())
    }

    fn hold_failed_display(&mut self, error: anyhow::Error) -> anyhow::Result<WorkerStopReason> {
        error!("GPU completion failed: {:#}; requesting VM exit", error);
        self.state.display_failed = true;
        self.fence_state.lock().queue.stop();
        // Do not return a successful GPU fence or drop the frontend here.
        // Keep renderer resources, imports and descriptors until VM teardown.
        if let Err(e) = self.exit_evt_wrtube.send::<VmEventType>(&VmEventType::Exit) {
            error!("failed to request VM exit after display failure: {}", e);
        }
        let mut reported_wait_error = false;
        loop {
            // Event::wait_timeout uses the existing event descriptor/handle;
            // it does not allocate another epoll fd in an fd-exhaustion path.
            match self.kill_evt.wait_timeout(Duration::from_millis(100)) {
                Ok(base::EventWaitResult::Signaled) => return Ok(WorkerStopReason::Kill),
                Ok(base::EventWaitResult::TimedOut) => (),
                Err(e) => {
                    if !reported_wait_error {
                        error!(
                            "failed waiting for VM teardown; retaining display resources: {}",
                            e
                        );
                        reported_wait_error = true;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            if matches!(
                self.suspend_evt.wait_timeout(Duration::ZERO),
                Ok(base::EventWaitResult::Signaled)
            ) {
                // on_reset/on_suspend reject while retaining the same frontend.
                return Ok(WorkerStopReason::Sleep);
            }
        }
    }

    fn on_snapshot(&mut self) -> anyhow::Result<WorkerSnapshot> {
        if self.state.display_failed
            || !self.state.pending_flip_fences.is_empty()
            || self.state.deferred_context_destroy.is_some()
        {
            return Err(anyhow!(
                "cannot snapshot GPU with unresolved display ownership"
            ));
        }
        Ok(WorkerSnapshot {
            fence_state_snapshot: self.fence_state.lock().snapshot()?,
            virtio_gpu_snapshot: self
                .state
                .virtio_gpu
                .snapshot()
                .context("failed to snapshot VirtioGpu")?,
        })
    }

    fn on_restore(&mut self, snapshot: WorkerSnapshot) -> anyhow::Result<()> {
        if self.state.display_failed
            || !self.state.pending_flip_fences.is_empty()
            || self.state.deferred_context_destroy.is_some()
        {
            return Err(anyhow!(
                "cannot restore GPU with unresolved display ownership"
            ));
        }
        self.fence_state
            .lock()
            .restore(snapshot.fence_state_snapshot)?;

        self.state
            .virtio_gpu
            .restore(snapshot.virtio_gpu_snapshot)
            .context("failed to restore VirtioGpu")?;

        Ok(())
    }

    fn run_until_sleep_or_exit(&mut self) -> anyhow::Result<WorkerStopReason> {
        let renderer_status = self.fence_state.lock().check_renderer();
        if let Err(e) = renderer_status {
            return self.hold_failed_display(e);
        }
        if self.state.display_failed {
            return self.hold_failed_display(anyhow!("display ownership is still unresolved"));
        }
        let activation_resources = self
            .activation_resources
            .as_ref()
            .context("virtio gpu worker missing activation resources")?;

        let display_desc =
            SafeDescriptor::try_from(&*self.state.display().borrow() as &dyn AsRawDescriptor)
                .context("failed getting event descriptor for display")?;

        let ctrl_evt = activation_resources
            .ctrl_queue
            .queue
            .lock()
            .event()
            .try_clone()
            .context("failed to clone queue event")?;
        let cursor_evt = activation_resources
            .cursor_queue
            .queue
            .lock()
            .event()
            .try_clone()
            .context("failed to clone queue event")?;

        let mut event_manager = EventManager::build_with(&[
            (&ctrl_evt, WorkerToken::CtrlQueue),
            (&cursor_evt, WorkerToken::CursorQueue),
            (&display_desc, WorkerToken::Display),
            (
                self.gpu_control_tube.get_read_notifier(),
                WorkerToken::GpuControl,
            ),
            (&self.suspend_evt, WorkerToken::Sleep),
            (&self.kill_evt, WorkerToken::Kill),
            #[cfg(windows)]
            (
                self.gpu_display_wait_descriptor_ctrl_rd.get_read_notifier(),
                WorkerToken::DisplayDescriptorRequest,
            ),
        ])
        .context("failed creating gpu worker WaitContext")?;

        let poll_desc: SafeDescriptor;
        let context_retirement_event = self.state.context_retirement_event()?;
        event_manager.add(&context_retirement_event, WorkerToken::ContextRetirement)?;
        if let Some(desc) = self.state.virtio_gpu.poll_descriptor() {
            poll_desc = desc;
            event_manager
                .add(&poll_desc, WorkerToken::VirtioGpuPoll)
                .context("failed adding poll event to WaitContext")?;
        }

        let external_evt = self
            .external_scanout
            .as_ref()
            .map(|e| e.event().try_clone());
        if let Some(Ok(evt)) = &external_evt {
            event_manager
                .add(evt, WorkerToken::ExternalFrame)
                .context("failed adding the external scanout event to WaitContext")?;
        }

        self.resource_bridges
            .add_to_wait_context(&mut event_manager.wait_ctx);

        // Registrations belonged to the previous WaitContext before sleep.
        // Every new event loop must register outstanding sync_files again.
        for pending in &mut self.state.pending_flip_fences {
            pending.registered = false;
        }
        if let Err(e) = self.state.register_flip_fences(&event_manager.wait_ctx) {
            return self.hold_failed_display(e);
        }

        // TODO(davidriley): The entire main loop processing is somewhat racey and incorrect with
        // respect to cursor vs control queue processing.  As both currently and originally
        // written, while the control queue is only processed/read from after the the cursor queue
        // is finished, the entire queue will be processed at that time.  The end effect of this
        // racyiness is that control queue descriptors that are issued after cursors descriptors
        // might be handled first instead of the other way around.  In practice, the cursor queue
        // isn't used so this isn't a huge issue.

        loop {
            if self.state.display_failed {
                return self.hold_failed_display(anyhow!("native display retirement failed; backing retained"));
            }
            let renderer_status = self.fence_state.lock().check_renderer();
            if let Err(e) = renderer_status {
                return self.hold_failed_display(e);
            }
            // A pending flip fence bounds the wait: its fd wakes us the moment the display is
            // done. The deadline requests recovery without falsely completing
            // display work or permitting the guest to reuse its source buffer.
            // Teardown retirement at the end of the previous iteration can
            // resume queue work and issue new display reads. Register those
            // before waiting as well, not just in the completion sweep.
            if let Err(e) = self.state.register_flip_fences(&event_manager.wait_ctx) {
                return self.hold_failed_display(e);
            }
            let events = match self.state.next_flip_fence_deadline() {
                Some(deadline) => {
                    let timeout = deadline
                        .saturating_duration_since(std::time::Instant::now())
                        .max(Duration::from_millis(1));
                    event_manager
                        .wait_ctx
                        .wait_timeout(timeout)
                        .context("failed polling for gpu worker events")?
                }
                None => event_manager
                    .wait_ctx
                    .wait()
                    .context("failed polling for gpu worker events")?,
            };

            let mut signal_used_cursor = false;
            let renderer_status = self.fence_state.lock().check_renderer();
            if let Err(e) = renderer_status {
                return self.hold_failed_display(e);
            }
            let mut signal_used_ctrl = false;
            let mut ctrl_available = false;
            let mut display_available = false;
            let mut needs_config_interrupt = false;

            // Remove event triggers that have been hung-up to prevent unnecessary worker wake-ups
            // (see b/244486346#comment62 for context).  FlipFence fds are exempt: their
            // completion sweep below owns their registration lifecycle.
            for event in events
                .iter()
                .filter(|e| e.is_hungup && !matches!(e.token, WorkerToken::FlipFence))
            {
                error!(
                    "unhandled virtio-gpu worker event hang-up detected: {:?}",
                    event.token
                );
                event_manager.delete(event.token);
            }

            for event in events.iter().filter(|e| e.is_readable) {
                match event.token {
                    WorkerToken::ExternalFrame => {
                        // The simplefb bridge has a frame. It only offers one while the guest is
                        // not displaying through this device, and `present_external` checks that
                        // again here, in the thread that owns the display -- so the two sources
                        // can never be mid-frame at the same time.
                        if let Some(external) = self.external_scanout.clone() {
                            let _ = external.event().wait();
                            serve_external_scanout(&external, &mut self.state.virtio_gpu);
                        }
                    }
                    WorkerToken::CtrlQueue => {
                        let _ = ctrl_evt.wait();
                        // Set flag that control queue is available to be read, but defer reading
                        // until rest of the events are processed.
                        ctrl_available = true;
                    }
                    WorkerToken::CursorQueue => {
                        let _ = cursor_evt.wait();
                        if self.state.process_cursor_queue(
                            &activation_resources.mem,
                            &activation_resources.cursor_queue,
                        ) {
                            signal_used_cursor = true;
                        }
                    }
                    WorkerToken::Display => {
                        // We only need to process_display once-per-wake, regardless of how many
                        // WorkerToken::Display events are received.
                        display_available = true;
                    }
                    #[cfg(windows)]
                    WorkerToken::DisplayDescriptorRequest => {
                        if let Ok(req) = self
                            .gpu_display_wait_descriptor_ctrl_rd
                            .recv::<ModifyWaitContext>()
                        {
                            match req {
                                ModifyWaitContext::Add(desc) => {
                                    if let Err(e) =
                                        event_manager.wait_ctx.add(&desc, WorkerToken::Display)
                                    {
                                        error!(
                                            "failed to add extra descriptor from display \
                                             to GPU worker wait context: {:?}",
                                            e
                                        )
                                    }
                                }
                            }
                        } else {
                            error!("failed to receive ModifyWaitContext request.")
                        }
                    }
                    WorkerToken::GpuControl => {
                        let req = self
                            .gpu_control_tube
                            .recv()
                            .context("failed to recv from gpu control socket")?;
                        let resp = self.state.process_gpu_control_command(req);

                        if let GpuControlResult::DisplaysUpdated = resp {
                            needs_config_interrupt = true;
                        }

                        self.gpu_control_tube
                            .send(&resp)
                            .context("failed to send gpu control socket response")?;
                    }
                    WorkerToken::ResourceBridge { index } => {
                        self.resource_bridges.set_should_process(index);
                    }
                    WorkerToken::VirtioGpuPoll => {
                        self.state.event_poll();
                    }
                    WorkerToken::FlipFence => {
                        // Handled by the completion sweep after queue processing; the wake-up
                        // itself is all this event needed to accomplish.
                    }
                    WorkerToken::ContextRetirement => {
                        let _ = context_retirement_event.wait();
                    }
                    WorkerToken::Sleep => {
                        return Ok(WorkerStopReason::Sleep);
                    }
                    WorkerToken::Kill => {
                        return Ok(WorkerStopReason::Kill);
                    }
                }
            }

            if display_available {
                match self.state.process_display() {
                    ProcessDisplayResult::CloseRequested => {
                        let _ = self.exit_evt_wrtube.send::<VmEventType>(&VmEventType::Exit);
                    }
                    ProcessDisplayResult::Error(gpu_display::GpuDisplayError::ImportRetirement) => {
                        return self.hold_failed_display(anyhow!("Surface change could not retire display imports"));
                    }
                    ProcessDisplayResult::Error(_e) => {
                        base::error!("Display processing failed, disabling display event handler.");
                        event_manager.delete(WorkerToken::Display);
                    }
                    ProcessDisplayResult::Success => (),
                    ProcessDisplayResult::CapabilitiesChanged => {
                        needs_config_interrupt = true;
                    }
                };
            }

            if ctrl_available
                && self
                    .state
                    .process_queue(&activation_resources.mem, &activation_resources.ctrl_queue)
            {
                signal_used_ctrl = true;
            }

            // Republish who owns the display after every batch of guest commands. This is where
            // ownership actually changes -- a scanout bound or unbound, a device reset at OS
            // handover -- and the bridge reads the flag to decide whether to bother producing a
            // frame at all. Updating it only when a frame arrives would make the first "the guest
            // owns it" observation permanent: the bridge stops offering, nothing wakes this
            // thread, and the flag can never go back. That is exactly the handover this exists
            // for (firmware paints through virtio-gpu, then Windows, which has no virtio-gpu
            // driver, never binds a scanout again).
            if let Some(external) = &self.external_scanout {
                external.set_guest_owns(self.state.virtio_gpu.guest_owns_display());
            }

            // Process the entire control queue before the resource bridge in case a resource is
            // created or destroyed by the control queue. Processing the resource bridge first may
            // lead to a race condition.
            // TODO(davidriley): This is still inherently racey if both the control queue request
            // and the resource bridge request come in at the same time after the control queue is
            // processed above and before the corresponding bridge is processed below.
            self.resource_bridges
                .process_resource_bridges(&mut self.state, &mut event_manager.wait_ctx);

            // Flip fences: register the ones queue processing just parked, then complete every
            // one that successfully signaled. Runs every iteration -- the wait above was woken
            // either by the fence fd itself or bounded by its deadline.
            let flip_result = self
                .state
                .register_flip_fences(&event_manager.wait_ctx)
                .and_then(|_| {
                    self.state.complete_flip_fences(
                        &activation_resources.ctrl_queue,
                        &activation_resources.cursor_queue,
                        &event_manager.wait_ctx,
                    )
                });
            // Queue processing above may already have published used entries.
            // Deliver those interrupts even if the fence sweep failed; recovery
            // retains only work whose ownership has not yet been released.
            if signal_used_ctrl {
                activation_resources.ctrl_queue.signal_used();
            }

            if signal_used_cursor {
                activation_resources.cursor_queue.signal_used();
            }

            if needs_config_interrupt {
                activation_resources.interrupt.signal_config_changed();
            }

            if let Err(e) = flip_result {
                return self.hold_failed_display(e);
            }
            if let Err(e) = self.state.process_context_retirement(
                &activation_resources.mem,
                &activation_resources.ctrl_queue,
                Some(&activation_resources.cursor_queue),
            ) {
                return self.hold_failed_display(e);
            }
        }
    }
}

/// Indicates a backend that should be tried for the gpu to use for display.
///
/// Several instances of this enum are used in an ordered list to give the gpu device many backends
/// to use as fallbacks in case some do not work.
#[derive(Clone)]
pub enum DisplayBackend {
    #[cfg(any(target_os = "android", target_os = "linux"))]
    /// Use the wayland backend with the given socket path if given.
    Wayland(Option<PathBuf>),
    #[cfg(any(target_os = "android", target_os = "linux"))]
    /// Open a connection to the X server at the given display if given.
    X(Option<String>),
    /// Emulate a display without actually displaying it.
    Stub,
    #[cfg(windows)]
    /// Open a window using WinAPI.
    WinApi,
    #[cfg(feature = "android_display")]
    /// The display buffer is backed by an Android surface. The surface is set via an AIDL service
    /// that the backend hosts. Currently, the AIDL service is registered to the service manager
    /// using the name given here. The entity holding the surface is expected to locate the service
    /// via this name, and pass the surface to it.
    Android(String),
    #[cfg(feature = "vnc")]
    /// Start a VNC server for remote display access on a TCP address.
    ///
    /// Named fields rather than a tuple: the list is long enough that "which of these is which" at
    /// the call site is exactly the kind of question a struct answers for free.
    VncTcp {
        addr: String,
        width: u32,
        height: u32,
        password: Option<String>,
        /// Whether this binding may run the hardware H.264 encoder and serve the stream to RFB
        /// clients that ask for encoding 50. Resolved by the caller from the transport ceiling
        /// (see `VncConfig::h264_enabled`); there is no port, the stream rides `addr`.
        hw_encode: bool,
        /// This binding's own absolute pointer and keyboard, parked until the sink is built.
        ///
        /// Behind an `Arc<Mutex<..>>` because of what this enum is: a CLONEABLE entry in a
        /// try-in-turn chain whose `build` takes `&self`. An `EventDevice` owns a socket and can
        /// be neither cloned nor moved out of a `&self`, so the devices are parked here
        /// and taken by whichever `build` call succeeds. The chain stops at the first
        /// success, so they are taken at most once; if this entry declines, the next
        /// backend leaves them alone and they are dropped with the config.
        ///
        /// Empty inside means `view-only=true` -- a binding that was given no input devices --
        /// which is a different thing from devices already taken, but neither can reach a
        /// second `build`. The lock is an artifact of `&self`, not of sharing: after
        /// `build`, one thread owns them.
        vnc_input: Arc<Mutex<VncBindingInput>>,
    },
}

impl DisplayBackend {
    /// What this entry is called when the chain reports which of them opened and which declined.
    ///
    /// Deliberately not the `Debug` derive: the Android and VNC variants carry a service name and
    /// a listen address, and a log line about which backend opened is not the place to repeat
    /// either. What a reader needs is which kind it was.
    fn name(&self) -> &'static str {
        match self {
            #[cfg(any(target_os = "android", target_os = "linux"))]
            DisplayBackend::Wayland(_) => "wayland",
            #[cfg(any(target_os = "android", target_os = "linux"))]
            DisplayBackend::X(_) => "x",
            DisplayBackend::Stub => "stub",
            #[cfg(windows)]
            DisplayBackend::WinApi => "winapi",
            #[cfg(feature = "android_display")]
            DisplayBackend::Android(_) => "android",
            #[cfg(feature = "vnc")]
            DisplayBackend::VncTcp { .. } => "vnc",
        }
    }

    fn build(
        &self,
        #[cfg(windows)] wndproc_thread: &mut Option<WindowProcedureThread>,
        #[cfg(windows)] gpu_display_wait_descriptor_ctrl: SendTube,
    ) -> std::result::Result<GpuDisplay, GpuDisplayError> {
        match self {
            #[cfg(any(target_os = "android", target_os = "linux"))]
            DisplayBackend::Wayland(path) => GpuDisplay::open_wayland(path.as_ref()),
            #[cfg(any(target_os = "android", target_os = "linux"))]
            DisplayBackend::X(display) => GpuDisplay::open_x(display.as_deref()),
            DisplayBackend::Stub => GpuDisplay::open_stub(),
            #[cfg(windows)]
            DisplayBackend::WinApi => match wndproc_thread.take() {
                Some(wndproc_thread) => GpuDisplay::open_winapi(
                    wndproc_thread,
                    /* win_metrics= */ None,
                    gpu_display_wait_descriptor_ctrl,
                    None,
                ),
                None => {
                    error!("wndproc_thread is none");
                    Err(GpuDisplayError::Allocate)
                }
            },
            #[cfg(feature = "android_display")]
            DisplayBackend::Android(service_name) => GpuDisplay::open_android(service_name),
            #[cfg(feature = "vnc")]
            DisplayBackend::VncTcp {
                addr,
                width,
                height,
                password,
                hw_encode,
                vnc_input,
            } => {
                let input = std::mem::take(&mut *vnc_input.lock());
                GpuDisplay::open_vnc_tcp(
                    addr,
                    *width,
                    *height,
                    password.clone(),
                    *hw_encode,
                    input.tablet,
                    input.keyboard,
                )
            }
        }
    }
}

/// How long the worker parks on the request channel before looking at the simplefb bridge again,
/// while no driver has the device activated. The bridge produces at 30 fps, so this is one frame:
/// long enough that a device nobody is displaying through costs nothing, short enough that a
/// Windows desktop is not a third of a second behind itself.
const EXTERNAL_IDLE_POLL: Duration = Duration::from_millis(33);

/// Take whatever the simplefb bridge is offering and put it on the display.
///
/// Both callers are the gpu worker thread: the event loop while the device is active, and the
/// request loop while it is not. Ownership is re-evaluated on the way out, because this is the
/// only place it is evaluated at all -- the bridge only ever reads the flag.
fn serve_external_scanout(external: &ExternalScanout, virtio_gpu: &mut VirtioGpu) {
    let (w, h, stride) = (external.width(), external.height(), external.stride());
    external.take_frame(|frame| {
        if let Err(e) = virtio_gpu.present_external(w, h, stride, frame) {
            error!("failed to present an external frame: {:?}", e);
        }
    });
    external.set_guest_owns(virtio_gpu.guest_owns_display());
}

pub struct Gpu {
    exit_evt_wrtube: SendTube,
    pub gpu_control_tube: Option<Tube>,
    mapper: Arc<Mutex<Option<Box<dyn SharedMemoryMapper>>>>,
    resource_bridges: Option<ResourceBridges>,
    event_devices: Option<Vec<EventDevice>>,
    worker_suspend_evt: Option<Event>,
    worker_request_sender: Option<mpsc::Sender<WorkerRequest>>,
    worker_response_receiver: Option<mpsc::Receiver<anyhow::Result<WorkerResponse>>>,
    worker_state: WorkerState,
    worker_thread: Option<WorkerThread<()>>,
    display_backends: Vec<DisplayBackend>,
    display_params: Vec<GpuDisplayParameters>,
    /// What the guest is told in `virtio_gpu_config.num_scanouts`. Zero means render-only.
    num_scanouts: u32,
    /// Frames from the VMM's simplefb bridge, if this VM has one (see `ExternalScanout`).
    external_scanout: Option<Arc<ExternalScanout>>,
    display_event: Arc<AtomicBool>,
    rutabaga_builder: RutabagaBuilder,
    pci_address: Option<PciAddress>,
    pci_bar_size: u64,
    external_blob: bool,
    fixed_blob_mapping: bool,
    rutabaga_component: RutabagaComponentType,
    #[cfg(windows)]
    wndproc_thread: Option<WindowProcedureThread>,
    base_features: u64,
    udmabuf: bool,
    rutabaga_server_descriptor: Option<SafeDescriptor>,
    #[cfg(windows)]
    /// Because the Windows GpuDisplay can't expose an epollfd, it has to inform the GPU worker
    /// which descriptors to add to its wait context. That's what this Tube is used for (it is
    /// provided to each display backend.
    gpu_display_wait_descriptor_ctrl_wr: SendTube,
    #[cfg(windows)]
    /// The GPU worker uses this Tube to receive the descriptors that should be added to its wait
    /// context.
    gpu_display_wait_descriptor_ctrl_rd: Option<RecvTube>,
    capset_mask: u64,
    #[cfg(any(target_os = "android", target_os = "linux"))]
    gpu_cgroup_path: Option<PathBuf>,
    snapshot_scratch_directory: Option<PathBuf>,
    /// Whether the exporter bound to this device's screen capped its transport to a CPU copy.
    ///
    /// Not a `GpuParameters` field, and that is the point: the ceiling belongs to the *binding*
    /// between one exporter and one screen (`--vnc-server ...,transport-cap=`, `
    /// --android-display-service ...,transport-cap=`), not to the GPU device, which has no opinion
    /// about how its frames leave. Set after construction because that is where a caller can read
    /// the binding -- `Gpu::new` takes the display backends but not the config they came from.
    dmabuf_import_capped: bool,
}

/// Default real time priority for the virtio-gpu worker thread.
///
/// The worker sits between a guest that is blocked on a fence and a GPU that has already finished:
/// every millisecond it spends waiting for a timeslice is a millisecond added to frame latency. It
/// is also, unlike a vcpu, guaranteed to block rather than spin -- its whole loop is
/// `WaitContext::wait()` plus blocking KGSL ioctls -- so a high priority costs nothing when there
/// is no work and cannot monopolize a CPU when there is.
#[cfg(any(target_os = "android", target_os = "linux"))]
const DEFAULT_GPU_RT_LEVEL: u16 = 97;

/// Promotes the calling thread to `SCHED_FIFO` for the virtio-gpu worker.
///
/// `CROSVM_GPU_RT_PRIO` overrides the level; `0`/`off` opts out entirely. Failure is not fatal --
/// without `CAP_SYS_NICE` or a high enough `RLIMIT_RTPRIO` the worker simply stays on CFS, which is
/// the pre-existing behaviour.
///
/// Call this *after* renderer and display init. The FIFO policy is inherited by threads created
/// afterwards, and init is what spawns the LibVNCServer event loop -- a CPU-hungry encode thread
/// that must not land on RT.
#[cfg(any(target_os = "android", target_os = "linux"))]
fn set_gpu_worker_rt_prio() {
    let prio = match std::env::var("CROSVM_GPU_RT_PRIO") {
        Ok(v) => {
            let v = v.trim().to_lowercase();
            if v == "off" || v == "false" {
                0
            } else {
                match v.parse::<u16>() {
                    Ok(p) if p <= 99 => p,
                    _ => {
                        warn!(
                            "invalid CROSVM_GPU_RT_PRIO={:?}, using default {}",
                            v, DEFAULT_GPU_RT_LEVEL
                        );
                        DEFAULT_GPU_RT_LEVEL
                    }
                }
            }
        }
        // Absent -> do not set real-time scheduling at all. RT is opt-in: the daemon only exports
        // CROSVM_GPU_RT_PRIO when the graphics tab's switch is on. (An explicit "off"/"0" also
        // disables it; an explicit level sets SCHED_FIFO at that level.)
        Err(_) => 0,
    };

    if prio == 0 {
        info!("v_gpu: real time scheduling disabled by CROSVM_GPU_RT_PRIO");
        return;
    }

    // RLIMIT_RTPRIO is per-process and only consulted for callers without CAP_SYS_NICE; raising it
    // is what lets an unprivileged crosvm reach `prio` at all. A failure here is not itself fatal,
    // so keep going and let sched_setscheduler render the verdict.
    if let Err(e) = set_rt_prio_limit(u64::from(prio)) {
        warn!("v_gpu: failed to raise RLIMIT_RTPRIO to {}: {}", prio, e);
    }

    match set_rt_fifo(i32::from(prio)) {
        Ok(()) => info!("v_gpu: running at SCHED_FIFO {}", prio),
        Err(e) => warn!(
            "v_gpu: failed to set SCHED_FIFO {} (staying on CFS): {}",
            prio, e
        ),
    }
}

impl Gpu {
    pub fn new(
        exit_evt_wrtube: SendTube,
        gpu_control_tube: Tube,
        resource_bridges: Vec<Tube>,
        display_backends: Vec<DisplayBackend>,
        gpu_parameters: &GpuParameters,
        rutabaga_server_descriptor: Option<SafeDescriptor>,
        event_devices: Vec<EventDevice>,
        base_features: u64,
        channels: &BTreeMap<String, PathBuf>,
        #[cfg(windows)] wndproc_thread: WindowProcedureThread,
        #[cfg(any(target_os = "android", target_os = "linux"))] gpu_cgroup_path: Option<&PathBuf>,
        external_scanout: Option<Arc<ExternalScanout>>,
    ) -> Gpu {
        let mut display_params = gpu_parameters.display_params.clone();
        // Zero configured displays is a real configuration, not an oversight: the picture comes
        // from somewhere else (crosvm's simplefb bridge) and this device is here to render. Keep
        // a nominal size for the internal bookkeeping below, but report no scanouts to the guest
        // (`num_scanouts` in build_config) so it never picks this device to display on.
        let num_scanouts = if display_params.is_empty() {
            0
        } else {
            VIRTIO_GPU_MAX_SCANOUTS as u32
        };
        if display_params.is_empty() {
            display_params.push(Default::default());
        }
        let (display_width, display_height) = display_params[0].get_virtual_display_size();

        let mut rutabaga_channels: Vec<RutabagaChannel> = Vec::new();
        for (channel_name, path) in channels {
            match &channel_name[..] {
                "" => rutabaga_channels.push(RutabagaChannel {
                    base_channel: path.clone(),
                    channel_type: RUTABAGA_CHANNEL_TYPE_WAYLAND,
                }),
                "mojo" => rutabaga_channels.push(RutabagaChannel {
                    base_channel: path.clone(),
                    channel_type: RUTABAGA_CHANNEL_TYPE_CAMERA,
                }),
                _ => error!("unknown rutabaga channel"),
            }
        }

        let rutabaga_channels_opt = Some(rutabaga_channels);
        let component = match gpu_parameters.mode {
            GpuMode::Mode2D => RutabagaComponentType::Rutabaga2D,
            #[cfg(feature = "virgl_renderer")]
            GpuMode::ModeVirglRenderer => RutabagaComponentType::VirglRenderer,
            #[cfg(feature = "gfxstream")]
            GpuMode::ModeGfxstream => RutabagaComponentType::Gfxstream,
        };

        // Only allow virglrenderer to fork its own render server when explicitly requested.
        // Caller can enforce its own restrictions (e.g. not allowed when sandboxed) and set the
        // allow flag appropriately.
        let use_render_server = rutabaga_server_descriptor.is_some()
            || gpu_parameters.allow_implicit_render_server_exec;

        let rutabaga_wsi = match gpu_parameters.wsi {
            Some(GpuWsi::Vulkan) => RutabagaWsi::VulkanSwapchain,
            _ => RutabagaWsi::Surfaceless,
        };

        let rutabaga_builder = RutabagaBuilder::new(component, gpu_parameters.capset_mask)
            .set_display_width(display_width)
            .set_display_height(display_height)
            .set_rutabaga_channels(rutabaga_channels_opt)
            .set_use_egl(gpu_parameters.renderer_use_egl)
            .set_use_gles(gpu_parameters.renderer_use_gles)
            .set_use_glx(gpu_parameters.renderer_use_glx)
            .set_use_surfaceless(gpu_parameters.renderer_use_surfaceless)
            .set_use_vulkan(gpu_parameters.use_vulkan.unwrap_or_default())
            .set_wsi(rutabaga_wsi)
            .set_use_external_blob(gpu_parameters.external_blob)
            .set_use_system_blob(gpu_parameters.system_blob)
            .set_use_render_server(use_render_server)
            .set_renderer_features(gpu_parameters.renderer_features.clone());

        #[cfg(windows)]
        let (gpu_display_wait_descriptor_ctrl_wr, gpu_display_wait_descriptor_ctrl_rd) =
            Tube::directional_pair().expect("failed to create wait descriptor control pair.");

        let mapper: Arc<Mutex<Option<Box<dyn SharedMemoryMapper>>>> = Arc::new(Mutex::new(None));
        Gpu {
            exit_evt_wrtube,
            gpu_control_tube: Some(gpu_control_tube),
            mapper,
            resource_bridges: Some(ResourceBridges::new(resource_bridges)),
            event_devices: Some(event_devices),
            worker_request_sender: None,
            worker_response_receiver: None,
            worker_suspend_evt: None,
            worker_state: WorkerState::Inactive,
            worker_thread: None,
            display_backends,
            display_params,
            num_scanouts,
            external_scanout,
            display_event: Arc::new(AtomicBool::new(false)),
            rutabaga_builder,
            pci_address: gpu_parameters.pci_address,
            pci_bar_size: gpu_parameters.pci_bar_size,
            external_blob: gpu_parameters.external_blob,
            fixed_blob_mapping: gpu_parameters.fixed_blob_mapping,
            rutabaga_component: component,
            #[cfg(windows)]
            wndproc_thread: Some(wndproc_thread),
            base_features,
            udmabuf: gpu_parameters.udmabuf,
            rutabaga_server_descriptor,
            #[cfg(windows)]
            gpu_display_wait_descriptor_ctrl_wr,
            #[cfg(windows)]
            gpu_display_wait_descriptor_ctrl_rd: Some(gpu_display_wait_descriptor_ctrl_rd),
            capset_mask: gpu_parameters.capset_mask,
            #[cfg(any(target_os = "android", target_os = "linux"))]
            gpu_cgroup_path: gpu_cgroup_path.cloned(),
            snapshot_scratch_directory: gpu_parameters.snapshot_scratch_path.clone(),
            dmabuf_import_capped: false,
        }
    }

    /// Caps this device's screen to the CPU transport, for a binding configured
    /// `transport-cap=cpu`.
    ///
    /// Must be called before the device is activated, which is when the display is opened and the
    /// cap applied; there is no way to change it afterwards, deliberately (see
    /// `GpuDisplay::cap_transport_to_cpu`). Ceilings only remove options from a negotiation whose
    /// floor is a CPU copy, so this cannot fail and there is nothing to report.
    pub fn cap_transport_to_cpu(&mut self) {
        self.dmabuf_import_capped = true;
    }

    /// Initializes the internal device state so that it can begin processing virtqueues.
    ///
    /// Only used by vhost-user GPU.
    pub fn initialize_frontend(
        &mut self,
        fence_state: Arc<Mutex<FenceState>>,
        fence_handler: RutabagaFenceHandler,
        mapper: Arc<Mutex<Option<Box<dyn SharedMemoryMapper>>>>,
    ) -> Option<Frontend> {
        let rutabaga_server_descriptor = self.rutabaga_server_descriptor.as_ref().map(|d| {
            to_rutabaga_descriptor(d.try_clone().expect("failed to clone server descriptor"))
        });
        let rutabaga = self
            .rutabaga_builder
            .clone()
            .set_fence_error_handler(create_fence_error_handler(fence_state.clone()))
            .build(fence_handler, rutabaga_server_descriptor)
            .map_err(|e| error!("failed to build rutabaga {}", e))
            .ok()?;

        let mut virtio_gpu = build(
            &self.display_backends,
            self.display_params.clone(),
            self.display_event.clone(),
            rutabaga,
            mapper,
            self.external_blob,
            self.fixed_blob_mapping,
            #[cfg(windows)]
            &mut self.wndproc_thread,
            self.udmabuf,
            #[cfg(windows)]
            self.gpu_display_wait_descriptor_ctrl_wr
                .try_clone()
                .expect("failed to clone wait context control channel"),
            self.snapshot_scratch_directory.clone(),
            self.dmabuf_import_capped,
        )?;

        for event_device in self.event_devices.take().expect("missing event_devices") {
            virtio_gpu
                .import_event_device(event_device)
                // We lost the `EventDevice`, so fail hard.
                .expect("failed to import event device");
        }

        Frontend::new(virtio_gpu, fence_state)
            .map_err(|e| error!("failed to create GPU frontend: {e}"))
            .ok()
    }

    // This is not invoked when running with vhost-user GPU.
    fn start_worker_thread(&mut self) {
        let suspend_evt = Event::new().unwrap();
        let suspend_evt_copy = suspend_evt
            .try_clone()
            .context("error cloning suspend event")
            .unwrap();

        let exit_evt_wrtube = self
            .exit_evt_wrtube
            .try_clone()
            .context("error cloning exit tube")
            .unwrap();

        let gpu_control_tube = self
            .gpu_control_tube
            .take()
            .context("gpu_control_tube is none")
            .unwrap();

        let resource_bridges = self
            .resource_bridges
            .take()
            .context("resource_bridges is none")
            .unwrap();

        let display_backends = self.display_backends.clone();
        let display_params = self.display_params.clone();
        let display_event = self.display_event.clone();
        let event_devices = self.event_devices.take().expect("missing event_devices");
        let external_blob = self.external_blob;
        let fixed_blob_mapping = self.fixed_blob_mapping;
        let udmabuf = self.udmabuf;
        let snapshot_scratch_directory = self.snapshot_scratch_directory.clone();
        let dmabuf_import_capped = self.dmabuf_import_capped;

        #[cfg(windows)]
        let mut wndproc_thread = self.wndproc_thread.take();

        #[cfg(windows)]
        let gpu_display_wait_descriptor_ctrl_wr = self
            .gpu_display_wait_descriptor_ctrl_wr
            .try_clone()
            .expect("failed to clone wait context ctrl channel");

        #[cfg(windows)]
        let gpu_display_wait_descriptor_ctrl_rd = self
            .gpu_display_wait_descriptor_ctrl_rd
            .take()
            .expect("failed to take gpu_display_wait_descriptor_ctrl_rd");

        #[cfg(any(target_os = "android", target_os = "linux"))]
        let gpu_cgroup_path = self.gpu_cgroup_path.clone();

        let mapper = Arc::clone(&self.mapper);

        let rutabaga_builder = self.rutabaga_builder.clone();
        let rutabaga_server_descriptor = self.rutabaga_server_descriptor.as_ref().map(|d| {
            to_rutabaga_descriptor(d.try_clone().expect("failed to clone server descriptor"))
        });

        let (init_finished_tx, init_finished_rx) = mpsc::channel();

        let (worker_request_sender, worker_request_receiver) = mpsc::channel();
        let (worker_response_sender, worker_response_receiver) = mpsc::channel();

        let external_scanout = self.external_scanout.clone();
        let worker_thread = WorkerThread::start("v_gpu", move |kill_evt| {
            #[cfg(any(target_os = "android", target_os = "linux"))]
            if let Some(cgroup_path) = gpu_cgroup_path {
                move_task_to_cgroup(cgroup_path, base::gettid())
                    .expect("Failed to move v_gpu into requested cgroup");
            }

            let mut worker = Worker::new(
                rutabaga_builder,
                rutabaga_server_descriptor,
                display_backends,
                display_params,
                display_event,
                mapper,
                event_devices,
                external_blob,
                fixed_blob_mapping,
                udmabuf,
                worker_request_receiver,
                worker_response_sender,
                exit_evt_wrtube,
                gpu_control_tube,
                resource_bridges,
                suspend_evt_copy,
                kill_evt,
                #[cfg(windows)]
                wndproc_thread,
                #[cfg(windows)]
                gpu_display_wait_descriptor_ctrl_rd,
                #[cfg(windows)]
                gpu_display_wait_descriptor_ctrl_wr,
                snapshot_scratch_directory,
                dmabuf_import_capped,
                external_scanout,
            )
            .expect("Failed to create virtio gpu worker thread");

            // Tell the parent thread that the init phase is complete.
            let _ = init_finished_tx.send(());

            // Promote to SCHED_FIFO only now that init is done. Renderer and display setup spawn
            // helper threads (notably the LibVNCServer event loop, which does full-framebuffer
            // encode) and those inherit the caller's policy -- they must not become RT. Threads
            // created later by the worker itself, i.e. the per-context KGSL fence sync threads, do
            // inherit it, which is what we want: they are the ones the guest is blocked on.
            #[cfg(any(target_os = "android", target_os = "linux"))]
            set_gpu_worker_rt_prio();

            worker.run()
        });

        self.worker_request_sender = Some(worker_request_sender);
        self.worker_response_receiver = Some(worker_response_receiver);
        self.worker_suspend_evt = Some(suspend_evt);
        self.worker_state = WorkerState::Inactive;
        self.worker_thread = Some(worker_thread);

        match init_finished_rx.recv() {
            Ok(()) => {}
            Err(mpsc::RecvError) => panic!("virtio-gpu worker thread init failed"),
        }
    }

    fn stop_worker_thread(&mut self) {
        self.worker_request_sender.take();
        self.worker_response_receiver.take();
        self.worker_suspend_evt.take();
        if let Some(worker_thread) = self.worker_thread.take() {
            worker_thread.stop();
        }
    }

    fn get_config(&self) -> virtio_gpu_config {
        let mut events_read = 0;

        if self.display_event.load(Ordering::Relaxed) {
            events_read |= VIRTIO_GPU_EVENT_DISPLAY;
        }

        let num_capsets = match self.capset_mask {
            0 => {
                match self.rutabaga_component {
                    RutabagaComponentType::Rutabaga2D => 0,
                    _ => {
                        #[allow(unused_mut)]
                        let mut num_capsets = 0;

                        // Three capsets for virgl_renderer
                        #[cfg(feature = "virgl_renderer")]
                        {
                            num_capsets += 3;
                        }

                        // One capset for gfxstream
                        #[cfg(feature = "gfxstream")]
                        {
                            num_capsets += 1;
                        }

                        num_capsets
                    }
                }
            }
            _ => self.capset_mask.count_ones(),
        };

        virtio_gpu_config {
            events_read: Le32::from(events_read),
            events_clear: Le32::from(0),
            num_scanouts: Le32::from(self.num_scanouts),
            num_capsets: Le32::from(num_capsets),
        }
    }

    /// Send a request to exit the process to VMM.
    pub fn send_exit_evt(&self) -> anyhow::Result<()> {
        self.exit_evt_wrtube
            .send::<VmEventType>(&VmEventType::Exit)
            .context("failed to send exit event")
    }
}

impl VirtioDevice for Gpu {
    fn keep_rds(&self) -> Vec<RawDescriptor> {
        let mut keep_rds = Vec::new();

        // To find the RawDescriptor associated with stdout and stderr on Windows is difficult.
        // Resource bridges are used only for Wayland displays. There is also no meaningful way
        // casting the underlying DMA buffer wrapped in File to a copyable RawDescriptor.
        // TODO(davidriley): Remove once virgl has another path to include
        // debugging logs.
        #[cfg(any(target_os = "android", target_os = "linux"))]
        if cfg!(debug_assertions) {
            keep_rds.push(libc::STDOUT_FILENO);
            keep_rds.push(libc::STDERR_FILENO);
        }

        if let Some(ref mapper) = *self.mapper.lock() {
            if let Some(descriptor) = mapper.as_raw_descriptor() {
                keep_rds.push(descriptor);
            }
        }

        if let Some(ref rutabaga_server_descriptor) = self.rutabaga_server_descriptor {
            keep_rds.push(rutabaga_server_descriptor.as_raw_descriptor());
        }

        keep_rds.push(self.exit_evt_wrtube.as_raw_descriptor());

        if let Some(gpu_control_tube) = &self.gpu_control_tube {
            keep_rds.push(gpu_control_tube.as_raw_descriptor());
        }

        if let Some(resource_bridges) = &self.resource_bridges {
            resource_bridges.append_raw_descriptors(&mut keep_rds);
        }

        for event_device in self.event_devices.iter().flatten() {
            keep_rds.push(event_device.as_raw_descriptor());
        }

        keep_rds
    }

    fn device_type(&self) -> DeviceType {
        DeviceType::Gpu
    }

    fn queue_max_sizes(&self) -> &[u16] {
        QUEUE_SIZES
    }

    fn features(&self) -> u64 {
        let mut virtio_gpu_features = 1 << VIRTIO_GPU_F_EDID;

        // If a non-2D component is specified, enable 3D features.  It is possible to run display
        // contexts without 3D backend (i.e, gfxstream / virglrender), so check for that too.
        if self.rutabaga_component != RutabagaComponentType::Rutabaga2D || self.capset_mask != 0 {
            virtio_gpu_features |= 1 << VIRTIO_GPU_F_VIRGL
                | 1 << VIRTIO_GPU_F_RESOURCE_UUID
                | 1 << VIRTIO_GPU_F_RESOURCE_BLOB
                | 1 << VIRTIO_GPU_F_CONTEXT_INIT
                | 1 << VIRTIO_GPU_F_EDID;

            if self.udmabuf {
                virtio_gpu_features |= 1 << VIRTIO_GPU_F_CREATE_GUEST_HANDLE;
            }

            // Virgl global cookies are mapped to private host tokens. Its
            // export/input alias contract is not implemented yet. Use the
            // same capset selection as renderer initialization, not just mode.
            if self.rutabaga_builder.default_component_type()
                != RutabagaComponentType::VirglRenderer
            {
                virtio_gpu_features |= 1 << VIRTIO_GPU_F_FENCE_PASSING;
            }
        }

        self.base_features | virtio_gpu_features
    }

    fn ack_features(&mut self, value: u64) {
        let _ = value;
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        copy_config(data, 0, self.get_config().as_bytes(), offset);
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        let mut cfg = self.get_config();
        copy_config(cfg.as_mut_bytes(), offset, data, 0);
        if (cfg.events_clear.to_native() & VIRTIO_GPU_EVENT_DISPLAY) != 0 {
            self.display_event.store(false, Ordering::Relaxed);
        }
    }

    fn on_device_sandboxed(&mut self) {
        // Unlike most Virtio devices which start their worker thread in activate(),
        // the Gpu's worker thread is started earlier here so that rutabaga and the
        // underlying render server have a chance to initialize before the guest OS
        // starts. This is needed because the Virtio GPU kernel module has a timeout
        // for some calls during initialization and some host GPU drivers have been
        // observed to be extremely slow to initialize on fresh GCE instances. The
        // entire worker thread is started here (as opposed to just initializing
        // rutabaga and the underlying render server) as OpenGL based renderers may
        // expect to be initialized on the same thread that later processes commands.
        self.start_worker_thread();
    }

    fn activate(
        &mut self,
        mem: GuestMemory,
        interrupt: Interrupt,
        mut queues: BTreeMap<usize, Queue>,
    ) -> anyhow::Result<()> {
        if queues.len() != QUEUE_SIZES.len() {
            return Err(anyhow!(
                "expected {} queues, got {}",
                QUEUE_SIZES.len(),
                queues.len()
            ));
        }

        let ctrl_queue = SharedQueueReader::new(queues.remove(&0).unwrap());
        let cursor_queue = SharedQueueReader::new(queues.remove(&1).unwrap());

        self.worker_request_sender
            .as_ref()
            .context("worker thread missing on activate?")?
            .send(WorkerRequest::Activate(WorkerActivateRequest {
                resources: GpuActivationResources {
                    mem,
                    interrupt,
                    ctrl_queue,
                    cursor_queue,
                },
            }))
            .map_err(|e| anyhow!("failed to send virtio gpu worker activate request: {:?}", e))?;

        self.worker_response_receiver
            .as_ref()
            .context("worker thread missing on activate?")?
            .recv()
            .inspect(|_| self.worker_state = WorkerState::Active)
            .inspect_err(|_| self.worker_state = WorkerState::Error)
            .context("failed to receive response for virtio gpu worker resume request")??;

        Ok(())
    }

    fn pci_address(&self) -> Option<PciAddress> {
        self.pci_address
    }

    fn get_shared_memory_region(&self) -> Option<SharedMemoryRegion> {
        Some(SharedMemoryRegion {
            id: VIRTIO_GPU_SHM_ID_HOST_VISIBLE,
            length: self.pci_bar_size,
        })
    }

    fn set_shared_memory_mapper(&mut self, mapper: Box<dyn SharedMemoryMapper>) {
        self.mapper.lock().replace(mapper);
    }

    fn expose_shmem_descriptors_with_viommu(&self) -> bool {
        // TODO(b/323368701): integrate with fixed_blob_mapping so this can always return true.
        !self.fixed_blob_mapping
    }

    fn get_shared_memory_prepare_type(&mut self) -> SharedMemoryPrepareType {
        if self.fixed_blob_mapping {
            let cache_type = if cfg!(feature = "noncoherent-dma") {
                MemCacheType::CacheNonCoherent
            } else {
                MemCacheType::CacheCoherent
            };
            if matches!(
                self.rutabaga_component,
                RutabagaComponentType::VirglRenderer
            ) {
                let arena_fd = env::var("CROSVM_DRM2KGSL_ARENA_FD")
                    .ok()
                    .and_then(|value| value.parse::<RawDescriptor>().ok());
                let arena_offset = env::var("CROSVM_DRM2KGSL_ARENA_FD_OFFSET")
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok());
                let arena_size = env::var("CROSVM_DRM2KGSL_ARENA_SIZE")
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok());
                if let (Some(fd), Some(offset), Some(size)) = (arena_fd, arena_offset, arena_size) {
                    if size > DRM2KGSL_BAR_BASE_GUARD && size <= self.pci_bar_size {
                        if let (Some(source_offset), Some(source_size)) = (
                            offset.checked_add(DRM2KGSL_BAR_BASE_GUARD),
                            size.checked_sub(DRM2KGSL_BAR_BASE_GUARD),
                        ) {
                            if let Ok(descriptor) = base::clone_descriptor(&base::Descriptor(fd)) {
                                // drm2kgsl sub-allocates blob zero from the arena suffix. The BAR
                                // is registered before VM start, so the renderer must never
                                // replace those pages after Gunyah installs the stage-2 mapping.
                                info!(
                                    "drm2kgsl: eagerly backing BAR suffix offset={:#x} size={:#x} from arena fd={} offset={:#x}",
                                    DRM2KGSL_BAR_BASE_GUARD,
                                    source_size,
                                    fd,
                                    source_offset,
                                );
                                return SharedMemoryPrepareType::SingleMappingEager(
                                    cache_type,
                                    VmMemorySource::Descriptor {
                                        descriptor,
                                        offset: source_offset,
                                        size: source_size,
                                    },
                                    DRM2KGSL_BAR_BASE_GUARD,
                                );
                            }
                        }
                    } else if size <= DRM2KGSL_BAR_BASE_GUARD {
                        error!(
                            "drm2kgsl BAR size {:#x} is not larger than the {:#x} base guard",
                            size, DRM2KGSL_BAR_BASE_GUARD,
                        );
                    } else if size > self.pci_bar_size {
                        error!(
                            "drm2kgsl BAR backing size {:#x} exceeds pci-bar-size {:#x}",
                            size, self.pci_bar_size
                        );
                    }
                }
            }
            SharedMemoryPrepareType::SingleMappingOnFirst(cache_type)
        } else {
            SharedMemoryPrepareType::DynamicPerMapping
        }
    }

    // Notes on sleep/wake/snapshot/restore functionality.
    //
    //   * Only 2d mode is supported so far.
    //   * We only snapshot the state relevant to the virtio-gpu 2d mode protocol (i.e. scanouts,
    //     resources, fences).
    //   * The GpuDisplay is recreated from scratch, we don't want to snapshot the state of a
    //     Wayland socket (for example).
    //   * No state about pending virtio requests needs to be snapshotted because the 2d backend
    //     completes them synchronously.
    fn virtio_sleep(&mut self) -> anyhow::Result<Option<BTreeMap<usize, Queue>>> {
        match self.worker_state {
            WorkerState::Error => {
                return Err(anyhow!(
                    "failed to sleep virtio gpu worker which is in error state"
                ));
            }
            WorkerState::Inactive => {
                return Ok(None);
            }
            _ => (),
        };

        if let (
            Some(worker_request_sender),
            Some(worker_response_receiver),
            Some(worker_suspend_evt),
        ) = (
            &self.worker_request_sender,
            &self.worker_response_receiver,
            &self.worker_suspend_evt,
        ) {
            worker_request_sender
                .send(WorkerRequest::Suspend)
                .map_err(|e| {
                    anyhow!(
                        "failed to send suspend request to virtio gpu worker: {:?}",
                        e
                    )
                })?;

            worker_suspend_evt
                .signal()
                .context("failed to signal virtio gpu worker suspend event")?;

            let response = worker_response_receiver
                .recv()
                .inspect(|_| self.worker_state = WorkerState::Inactive)
                .inspect_err(|_| self.worker_state = WorkerState::Error)
                .context("failed to receive response for virtio gpu worker suspend request")??;

            worker_suspend_evt
                .reset()
                .context("failed to reset virtio gpu worker suspend event")?;

            match response {
                WorkerResponse::Suspend(deactivation_resources) => Ok(deactivation_resources
                    .queues
                    .map(|q| q.into_iter().enumerate().collect())),
                _ => {
                    panic!("unexpected response from virtio gpu worker sleep request");
                }
            }
        } else {
            Err(anyhow!("virtio gpu worker not available for sleep"))
        }
    }

    fn virtio_wake(
        &mut self,
        queues_state: Option<(GuestMemory, Interrupt, BTreeMap<usize, Queue>)>,
    ) -> anyhow::Result<()> {
        match self.worker_state {
            WorkerState::Error => {
                return Err(anyhow!(
                    "failed to wake virtio gpu worker which is in error state"
                ));
            }
            WorkerState::Active => {
                return Ok(());
            }
            _ => (),
        };

        match queues_state {
            None => Ok(()),
            Some((mem, interrupt, queues)) => {
                // TODO(khei): activate is just what we want at the moment, but we should probably
                // move it into a "start workers" function to make it obvious that it isn't
                // strictly used for activate events.
                self.activate(mem, interrupt, queues)?;
                Ok(())
            }
        }
    }

    fn virtio_snapshot(&mut self) -> anyhow::Result<AnySnapshot> {
        match self.worker_state {
            WorkerState::Error => {
                return Err(anyhow!(
                    "failed to snapshot virtio gpu worker which is in error state"
                ));
            }
            WorkerState::Active => {
                return Err(anyhow!(
                    "failed to snapshot virtio gpu worker which is in active state"
                ));
            }
            _ => (),
        };

        if let (Some(worker_request_sender), Some(worker_response_receiver)) =
            (&self.worker_request_sender, &self.worker_response_receiver)
        {
            worker_request_sender
                .send(WorkerRequest::Snapshot)
                .map_err(|e| {
                    anyhow!(
                        "failed to send snapshot request to virtio gpu worker: {:?}",
                        e
                    )
                })?;

            match worker_response_receiver
                .recv()
                .inspect_err(|_| self.worker_state = WorkerState::Error)
                .context("failed to receive response for virtio gpu worker suspend request")??
            {
                WorkerResponse::Snapshot(snapshot) => Ok(AnySnapshot::to_any(snapshot)?),
                _ => {
                    panic!("unexpected response from virtio gpu worker sleep request");
                }
            }
        } else {
            Err(anyhow!("virtio gpu worker not available for snapshot"))
        }
    }

    fn virtio_restore(&mut self, data: AnySnapshot) -> anyhow::Result<()> {
        match self.worker_state {
            WorkerState::Error => {
                return Err(anyhow!(
                    "failed to restore virtio gpu worker which is in error state"
                ));
            }
            WorkerState::Active => {
                return Err(anyhow!(
                    "failed to restore virtio gpu worker which is in active state"
                ));
            }
            _ => (),
        };

        let snapshot: WorkerSnapshot = AnySnapshot::from_any(data)?;

        if let (Some(worker_request_sender), Some(worker_response_receiver)) =
            (&self.worker_request_sender, &self.worker_response_receiver)
        {
            worker_request_sender
                .send(WorkerRequest::Restore(snapshot))
                .map_err(|e| {
                    anyhow!(
                        "failed to send suspend request to virtio gpu worker: {:?}",
                        e
                    )
                })?;

            let response = worker_response_receiver
                .recv()
                .inspect_err(|_| self.worker_state = WorkerState::Error)
                .context("failed to receive response for virtio gpu worker suspend request")??;

            match response {
                WorkerResponse::Ok => Ok(()),
                _ => {
                    panic!("unexpected response from virtio gpu worker sleep request");
                }
            }
        } else {
            Err(anyhow!("virtio gpu worker not available for restore"))
        }
    }

    fn reset(&mut self) -> anyhow::Result<()> {
        // Do NOT tear the worker down here. on_device_sandboxed() starts it exactly once, so a
        // stopped worker is never restarted (start_worker_thread() consumes one-shot inputs it
        // can't reacquire), and the next activate() then fails with "worker thread missing on
        // activate?". That fires on every UEFI boot: the EDK2 virtio-gpu driver activates the
        // device for its boot display, then resets it on ExitBootServices, and the guest OS
        // re-activates it. Instead, deactivate the worker and drop the guest's resources while
        // keeping the thread + render server alive, so the OS's activate() succeeds and it starts
        // from a clean resource-id space. (Full teardown happens in Drop instead.)
        if self.worker_thread.is_none() {
            return Ok(());
        }
        let was_active = matches!(self.worker_state, WorkerState::Active);
        // If the worker is mid-processing, break it out of run_until_sleep_or_exit() without
        // killing it (the same signal virtio_sleep() uses to park it).
        if was_active {
            if let Some(suspend_evt) = &self.worker_suspend_evt {
                suspend_evt
                    .signal()
                    .context("failed to signal gpu worker suspend event for reset")?;
            }
        }
        if let (Some(sender), Some(receiver)) =
            (&self.worker_request_sender, &self.worker_response_receiver)
        {
            sender
                .send(WorkerRequest::Reset)
                .map_err(|e| anyhow!("failed to send gpu worker reset request: {:?}", e))?;
            receiver
                .recv()
                .context("failed to receive gpu worker reset response")??;
        }
        if was_active {
            if let Some(suspend_evt) = &self.worker_suspend_evt {
                suspend_evt
                    .reset()
                    .context("failed to reset gpu worker suspend event after reset")?;
            }
        }
        self.worker_state = WorkerState::Inactive;
        Ok(())
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        self.stop_worker_thread();
    }
}

/// This struct takes the ownership of resource bridges and tracks which ones should be processed.
struct ResourceBridges {
    resource_bridges: Vec<Tube>,
    should_process: Vec<bool>,
}

impl ResourceBridges {
    pub fn new(resource_bridges: Vec<Tube>) -> Self {
        #[cfg(windows)]
        assert!(
            resource_bridges.is_empty(),
            "resource bridges are not supported on Windows"
        );

        let mut resource_bridges = Self {
            resource_bridges,
            should_process: Default::default(),
        };
        resource_bridges.reset_should_process();
        resource_bridges
    }

    // Appends raw descriptors of all resource bridges to the given vector.
    pub fn append_raw_descriptors(&self, rds: &mut Vec<RawDescriptor>) {
        for bridge in &self.resource_bridges {
            rds.push(bridge.as_raw_descriptor());
        }
    }

    /// Adds all resource bridges to WaitContext.
    pub fn add_to_wait_context(&self, wait_ctx: &mut WaitContext<WorkerToken>) {
        for (index, bridge) in self.resource_bridges.iter().enumerate() {
            if let Err(e) = wait_ctx.add(bridge, WorkerToken::ResourceBridge { index }) {
                error!("failed to add resource bridge to WaitContext: {}", e);
            }
        }
    }

    /// Marks that the resource bridge at the given index should be processed when
    /// `process_resource_bridges()` is called.
    pub fn set_should_process(&mut self, index: usize) {
        self.should_process[index] = true;
    }

    /// Processes all resource bridges that have been marked as should be processed.  The markings
    /// will be cleared before returning. Faulty resource bridges will be removed from WaitContext.
    pub fn process_resource_bridges(
        &mut self,
        state: &mut Frontend,
        wait_ctx: &mut WaitContext<WorkerToken>,
    ) {
        for (bridge, &should_process) in self.resource_bridges.iter().zip(&self.should_process) {
            if should_process {
                if let Err(e) = state.process_resource_bridge(bridge) {
                    error!("Failed to process resource bridge: {:#}", e);
                    error!("Removing that resource bridge from the wait context.");
                    wait_ctx.delete(bridge).unwrap_or_else(|e| {
                        error!("Failed to remove faulty resource bridge: {:#}", e)
                    });
                }
            }
        }
        self.reset_should_process();
    }

    fn reset_should_process(&mut self) {
        self.should_process.clear();
        self.should_process
            .resize(self.resource_bridges.len(), false);
    }
}
