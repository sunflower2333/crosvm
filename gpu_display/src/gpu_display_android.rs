// Copyright 2024 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::ffi::c_char;
use std::ffi::c_int;
use std::ffi::c_void;
use std::ffi::CStr;
use std::ffi::CString;
use std::panic::catch_unwind;
use std::process::abort;
use std::ptr::NonNull;
use std::rc::Rc;
use std::slice;

use base::error;
use base::AsRawDescriptor;
use base::Event;
use base::FromRawDescriptor;
use base::RawDescriptor;
use base::SafeDescriptor;
use base::VolatileSlice;
use vm_control::gpu::DisplayParameters;

use crate::DisplayT;
use crate::FlipCompletionFence;
use crate::GpuDisplayError;
use crate::GpuDisplayFramebuffer;
use crate::GpuDisplayResult;
use crate::GpuDisplaySurface;
use crate::HostSharedAllocation;
use crate::ScanoutGeometry;
use crate::ScanoutProfile;
use crate::SemaphoreTimepoint;
use crate::SharedAllocationDescription;
use crate::SurfaceType;
use crate::SysDisplayT;
use crate::{DRM_FORMAT_ABGR8888, DRM_FORMAT_ARGB8888, DRM_FORMAT_RGB565, DRM_FORMAT_XBGR8888};

// Opaque blob
#[repr(C)]
pub(crate) struct AndroidDisplayContext {
    _data: [u8; 0],
    _marker: core::marker::PhantomData<(*mut u8, core::marker::PhantomPinned)>,
}

// Opaque blob
#[repr(C)]
pub(crate) struct AndroidDisplaySurface {
    _data: [u8; 0],
    _marker: core::marker::PhantomData<(*mut u8, core::marker::PhantomPinned)>,
}

// Should be the same as ANativeWindow_Buffer in android/native_window.h
// Note that this struct is part of NDK; guaranteed to be stable, so we use it directly across the
// FFI.
#[repr(C)]
pub(crate) struct ANativeWindow_Buffer {
    width: i32,
    height: i32,
    stride: i32, // in number of pixels, NOT bytes
    format: i32,
    bits: *mut u8,
    reserved: [u32; 6],
}

pub(crate) type ErrorCallback = unsafe extern "C" fn(message: *const c_char);

/// virtio-gpu's cursor plane is fixed at 64x64 (drivers/gpu/drm/virtio registers it with that
/// single size), and a smaller cursor image arrives in the top-left of it.
const CURSOR_PLANE_SIZE: u32 = 64;

/// Sentinel position meaning "the guest hid its pointer". A real cursor position is a framebuffer
/// coordinate, so u32::MAX can never collide with one.
const CURSOR_HIDDEN_POS: u32 = u32::MAX;

extern "C" {
    fn android_display_query_scanout_profile(ctx: *mut AndroidDisplayContext, surface: *mut c_void,
        profile: *mut ScanoutProfile) -> bool;
    fn android_display_reset_scanout_geometry(ctx: *mut AndroidDisplayContext,
        old_reset: u64, new_reset: u64) -> bool;
    fn android_display_configure_scanout_profile(ctx: *mut AndroidDisplayContext, surface: *mut c_void,
        geometry: *const ScanoutGeometry, endpoint: u64, profile: u64) -> bool;
    fn android_display_get_timing(context: *mut AndroidDisplayContext,
                                  sample: *mut crate::DisplayTimingSample) -> bool;
    /// Constructs an AndroidDisplayContext for this backend. This awlays returns a valid (ex:
    /// non-null) handle to the context. The `name` parameter is from crosvm commandline and the
    /// client of crosvm will use it to locate and communicate to the AndroidDisplayContext. For
    /// example, this can be a path to UNIX domain socket where a RPC binder server listens on.
    /// `error_callback` is a function pointer to an error reporting function, and will be used by
    /// this and other functions below when something goes wrong. The returned context should be
    /// destroyed by calling `destroy_android_display_context` if this backend is no longer in use.
    fn create_android_display_context(
        name: *const c_char,
        error_callback: ErrorCallback,
    ) -> *mut AndroidDisplayContext;

    /// Destroys the AndroidDisplayContext created from `create_android_display_context`.
    fn destroy_android_display_context(self_: *mut AndroidDisplayContext);

    /// Creates an Android Surface (which is also called as Window) of given size. If the surface
    /// can't be created for whatever reason, null pointer is returned, in which case we shouldn't
    /// proceed further.
    fn create_android_surface(
        ctx: *mut AndroidDisplayContext,
        width: u32,
        height: u32,
        for_cursor: bool,
    ) -> *mut AndroidDisplaySurface;

    /// Detaches the guest's direct presenter. The service keeps the app-owned
    /// native window available for a new guest surface or external producer.
    fn destroy_android_surface(
        ctx: *mut AndroidDisplayContext,
        surface: *mut AndroidDisplaySurface,
    );

    /// Queues detach without blocking on SurfaceFlinger or a busy surface
    /// mutex. Admission never substitutes for the exported release eventfd.
    fn request_android_surface_detach(
        ctx: *mut AndroidDisplayContext,
        surface: *mut AndroidDisplaySurface,
    ) -> bool;

    /// Obtains one buffer from the given Android Surface. The information about the buffer (buffer
    /// address, size, stride, etc) is reported via the `ANativeWindow_Buffer` struct. It shouldn't
    /// be null. The size of the buffer is guaranteed to be bigger than (width * stride * 4) bytes.
    /// This function locks the buffer for the client, which means the caller has the exclusive
    /// access to the buffer until it is returned back to Android display stack (surfaceflinger) by
    /// calling `post_android_surface_buffer`. This function may fail (in which case false is
    /// returned), then the caller shouldn't try to read `out_buffer` or use the buffer in any way.
    fn get_android_surface_buffer(
        ctx: *mut AndroidDisplayContext,
        surface: *mut AndroidDisplaySurface,
        out_buffer: *mut ANativeWindow_Buffer,
    ) -> bool;

    fn set_android_surface_position(ctx: *mut AndroidDisplayContext, x: u32, y: u32);

    /// Posts the buffer obtained from `get_android_surface_buffer` to the Android display system
    /// so that it can be displayed on the screen. Once this is called, the caller shouldn't use
    /// the buffer any more.
    fn post_android_surface_buffer(
        ctx: *mut AndroidDisplayContext,
        surface: *mut AndroidDisplaySurface,
    );

    fn set_android_surface_buffer_format(
        ctx: *mut AndroidDisplayContext,
        surface: *mut AndroidDisplaySurface,
        fourcc: u32,
    );

    /// Supplies the guest's requested scanout refresh rate to the Android native window. The
    /// native surface may not be attached yet, so the bridge stores this until its first
    /// producer connection and advertises it to SurfaceFlinger before dequeuing buffers.
    fn set_android_surface_frame_rate(
        ctx: *mut AndroidDisplayContext,
        surface: *mut AndroidDisplaySurface,
        refresh_rate: u32,
    );

    fn android_display_import_dmabuf(
        ctx: *mut AndroidDisplayContext,
        surface: *mut AndroidDisplaySurface,
        fd: RawDescriptor,
        offset: u32,
        stride: u32,
        modifier: u64,
        linear_layout_verified: bool,
        width: u32,
        height: u32,
        fourcc: u32,
    ) -> i64;

    fn android_display_release_import(ctx: *mut AndroidDisplayContext, raw_handle: i64);

    fn android_display_is_vulkan_blit_available(ctx: *mut AndroidDisplayContext) -> bool;

    /// Whether the app has a native window attached to the scanout surface, i.e. whether a frame
    /// posted now can reach a screen. The native side reads the current state under its surface
    /// lock and never waits for one to arrive: this is asked once per frame from a timer thread.
    fn android_display_has_consumer(ctx: *mut AndroidDisplayContext) -> bool;

    /// Returns the generation of the current native scanout Surface/BufferQueue. A replacement
    /// Surface increments this value so crosvm can retry imports that failed against the old one.
    fn android_display_consumer_generation(ctx: *mut AndroidDisplayContext) -> u64;
    fn android_display_set_native_refresh_event(ctx: *mut AndroidDisplayContext, fd: c_int) -> bool;
    fn android_display_native_refresh_generation(ctx: *mut AndroidDisplayContext) -> u64;

    /// `out_completion_fence_fd` is written by the callee and is mandatory: the C side rejects a
    /// null pointer outright, and on the async-blit path it hands back an owned sync_file fd that
    /// this side must close. Keep the arity in step with the definition in
    /// `crosvm_android_display_client.cpp` -- these symbols are not declared in any shared header,
    /// so this extern block is the only "declaration" the compiler ever sees and a mismatch links
    /// silently.
    fn android_display_flip_to(
        ctx: *mut AndroidDisplayContext,
        surface: *mut AndroidDisplaySurface,
        raw_handle: i64,
        out_completion_fence_fd: *mut c_int,
    ) -> bool;

    fn android_display_create_shared_ahb_v2(
        width: u32,
        height: u32,
        fourcc: u32,
        description: *mut SharedAllocationDescription,
        dma_fd: *mut RawDescriptor,
        error_callback: ErrorCallback,
    ) -> *mut c_void;

    fn android_display_destroy_shared_ahb(holder: *mut c_void);

    fn android_display_shared_ahb_read_bytes(
        holder: *mut c_void,
        offset: u64,
        destination: *mut u8,
        length: u64,
    ) -> c_int;
    fn android_display_shared_ahb_write_bytes(
        holder: *mut c_void,
        offset: u64,
        source: *const u8,
        length: u64,
    ) -> c_int;
    fn android_display_shared_ahb_fill_bytes(
        holder: *mut c_void,
        offset: u64,
        length: u64,
        pattern: u32,
    ) -> c_int;

    fn android_display_refresh_shared_ahb(
        ctx: *mut AndroidDisplayContext,
        surface: *mut AndroidDisplaySurface,
        holder: u64,
        acquire_fence_fd: c_int,
        out_release_fence_fd: *mut c_int,
        expected_generation: u64,
    ) -> bool;
    fn android_display_configure_scanout_geometry(
        ctx: *mut AndroidDisplayContext, surface: *mut c_void,
        geometry: *const ScanoutGeometry,
    ) -> bool;
    fn android_display_present_shared_ahb_geometry(
        ctx: *mut AndroidDisplayContext, surface: *mut c_void, holder: u64,
        acquire_fence_fd: c_int, out_release_fence_fd: *mut c_int,
        attachment_generation: u64, mode_generation: u64, reset_generation: u64,
    ) -> bool;
}

unsafe extern "C" fn error_callback(message: *const c_char) {
    catch_unwind(|| {
        error!(
            "{}",
            // SAFETY: message is null terminated
            unsafe { CStr::from_ptr(message) }.to_string_lossy()
        )
    })
    .unwrap_or_else(|_| abort())
}

struct AndroidDisplayContextWrapper(NonNull<AndroidDisplayContext>);

struct AndroidSharedAllocation {
    holder: NonNull<c_void>,
    description: SharedAllocationDescription,
    dma_buf: SafeDescriptor,
}

// The holder is allocated and destroyed by the Android display library. It is
// a process-local token and never exposed to a guest or another process.
unsafe impl Send for AndroidSharedAllocation {}

impl Drop for AndroidSharedAllocation {
    fn drop(&mut self) {
        // SAFETY: holder came from android_display_create_shared_ahb_v2 and is
        // kept alive until every display import has been released.
        unsafe { android_display_destroy_shared_ahb(self.holder.as_ptr()) };
    }
}

impl HostSharedAllocation for AndroidSharedAllocation {
    fn description(&self) -> SharedAllocationDescription {
        self.description
    }

    fn duplicate_descriptor(&self) -> anyhow::Result<SafeDescriptor> {
        self.dma_buf
            .try_clone()
            .map_err(|e| anyhow::anyhow!("duplicate native AHB DMA-BUF: {e}"))
    }

    fn native_handle(&self) -> *mut c_void {
        self.holder.as_ptr()
    }

    fn read_bytes(&self, offset: u64, destination: &mut [u8]) -> anyhow::Result<()> {
        // SAFETY: holder remains owned by self, and the writable slice is live
        // for this synchronous call. The caller owns paging admission.
        let result = unsafe {
            android_display_shared_ahb_read_bytes(
                self.holder.as_ptr(),
                offset,
                destination.as_mut_ptr(),
                destination.len() as u64,
            )
        };
        anyhow::ensure!(
            result == 0,
            "native AHB paging read failed: errno={}",
            -result
        );
        Ok(())
    }

    fn write_bytes(&self, offset: u64, source: &[u8]) -> anyhow::Result<()> {
        // SAFETY: holder and readable slice remain live for the synchronous
        // call. The caller owns exclusive paging admission.
        let result = unsafe {
            android_display_shared_ahb_write_bytes(
                self.holder.as_ptr(),
                offset,
                source.as_ptr(),
                source.len() as u64,
            )
        };
        anyhow::ensure!(
            result == 0,
            "native AHB paging write failed: errno={}",
            -result
        );
        Ok(())
    }

    fn fill_bytes(&self, offset: u64, length: u64, pattern: u32) -> anyhow::Result<()> {
        // SAFETY: self retains the native holder and the C++ implementation
        // validates the extent. The caller owns exclusive paging admission.
        let result = unsafe {
            android_display_shared_ahb_fill_bytes(self.holder.as_ptr(), offset, length, pattern)
        };
        anyhow::ensure!(
            result == 0,
            "native AHB paging fill failed: errno={}",
            -result
        );
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum AndroidImport {
    Vulkan(i64),
    SharedAhb(u64),
}

impl Drop for AndroidDisplayContextWrapper {
    fn drop(&mut self) {
        // SAFETY: this object is constructed from create_android_display_context
        unsafe { destroy_android_display_context(self.0.as_ptr()) };
    }
}

impl Default for ANativeWindow_Buffer {
    fn default() -> Self {
        Self {
            width: 0,
            height: 0,
            stride: 0,
            format: 0,
            bits: std::ptr::null_mut(),
            reserved: [0u32; 6],
        }
    }
}

// Values are the stable HAL_PIXEL_FORMAT_* assignments from
// system/core/include/system/graphics.h.  The Android buffer reports the
// actual producer format; treating every buffer as RGBA8888 swaps red and blue
// for the XR24/AR24 buffers used by the boot logo and the Windows cursor.
fn android_buffer_format(format: i32) -> (u32, u32) {
    match format {
        // HAL_PIXEL_FORMAT_RGBA_8888: bytes R, G, B, A (DRM AB24).
        1 => (DRM_FORMAT_ABGR8888, 4),
        // HAL_PIXEL_FORMAT_RGBX_8888: bytes R, G, B, X (DRM XB24).
        2 => (DRM_FORMAT_XBGR8888, 4),
        // HAL_PIXEL_FORMAT_RGB_565: packed 16-bit RGB (DRM RG16).
        4 => (DRM_FORMAT_RGB565, 2),
        // HAL_PIXEL_FORMAT_BGRA_8888: bytes B, G, R, A (DRM AR24).
        5 => (DRM_FORMAT_ARGB8888, 4),
        // The native window normally returns one of the formats above. Keep
        // the historical RGBA interpretation for an unknown value so older
        // bridges remain usable while the producer is being upgraded.
        _ => (DRM_FORMAT_ABGR8888, 4),
    }
}

impl From<ANativeWindow_Buffer> for GpuDisplayFramebuffer<'_> {
    fn from(anb: ANativeWindow_Buffer) -> Self {
        let (fourcc, bytes_per_pixel) = android_buffer_format(anb.format);
        let stride_bytes = bytes_per_pixel * u32::try_from(anb.stride).unwrap();
        let buffer_size = stride_bytes * u32::try_from(anb.height).unwrap();
        let buffer =
            // SAFETY: get_android_surface_buffer guarantees that bits points to a valid buffer and
            // the buffer remains available until post_android_surface_buffer is called.
            unsafe { slice::from_raw_parts_mut(anb.bits, buffer_size.try_into().unwrap()) };
        Self::new(
            VolatileSlice::new(buffer),
            stride_bytes,
            bytes_per_pixel,
            fourcc,
        )
    }
}

struct AndroidSurface {
    context: Rc<AndroidDisplayContextWrapper>,
    refresh_generation: Rc<Cell<u64>>,
    surface: NonNull<AndroidDisplaySurface>,
    surface_id: u32,
    surfaces: Rc<RefCell<BTreeMap<u32, NonNull<AndroidDisplaySurface>>>>,
    reset_detach_requested: bool,
    imports: Rc<RefCell<BTreeMap<u32, AndroidImport>>>,
    /// Completion fence of the most recent async flip, parked here until the caller collects it
    /// with `take_flip_completion_fence()`. A pending fence blocks the next flip so an exceptional
    /// caller cannot silently drop the release signal and reuse the source allocation early.
    pending_flip_fence: Option<FlipCompletionFence>,
}

impl Drop for AndroidSurface {
    fn drop(&mut self) {
        let last_user = {
            let mut surfaces = self.surfaces.borrow_mut();
            surfaces.remove(&self.surface_id);
            !surfaces.values().any(|surface| *surface == self.surface)
        };
        if last_user && !self.reset_detach_requested {
            // SAFETY: the context outlives this wrapper and owns the native
            // surface. This submits a real detach, never a release signal.
            // Aliased wrappers must not detach a still-live guest surface.
            unsafe { destroy_android_surface(self.context.0.as_ptr(), self.surface.as_ptr()) };
        }
    }
}

impl GpuDisplaySurface for AndroidSurface {
    fn request_reset_detach(&mut self) -> bool {
        if self.reset_detach_requested {
            return true;
        }
        // SAFETY: both opaque objects remain owned until reset succeeds.
        self.reset_detach_requested = unsafe {
            request_android_surface_detach(self.context.0.as_ptr(), self.surface.as_ptr())
        };
        self.reset_detach_requested
    }

    fn framebuffer(&mut self) -> Option<GpuDisplayFramebuffer> {
        let mut anb = ANativeWindow_Buffer::default();
        // SAFETY: context and surface are opaque handles and buf is used as the out parameter to
        // hold the return values.
        let success = unsafe {
            get_android_surface_buffer(
                self.context.0.as_ptr(),
                self.surface.as_ptr(),
                &mut anb as *mut ANativeWindow_Buffer,
            )
        };
        if success {
            Some(anb.into())
        } else {
            None
        }
    }

    fn flip(&mut self) {
        // SAFETY: context and surface are opaque handles.
        unsafe { post_android_surface_buffer(self.context.0.as_ptr(), self.surface.as_ptr()) }
    }

    fn set_position(&mut self, x: i32, y: i32) {
        // The image origin goes negative when the pointer is within the hotspot of the left or top
        // edge, but the app owns this FFI and it takes u32 -- and u32::MAX is the hide sentinel,
        // which -1 would land on exactly, making the pointer vanish near the edge. Clamp instead:
        // the cursor stops up to a hotspot short of the corner rather than disappearing into it.
        // Drawing it properly clipped would mean re-rendering the plane, which is the app's side.
        // SAFETY: context is an opaque handle.
        unsafe {
            set_android_surface_position(self.context.0.as_ptr(), x.max(0) as u32, y.max(0) as u32)
        };
    }

    fn set_buffer_fourcc(&mut self, fourcc: u32) {
        // SAFETY: context and surface are live opaque handles owned by this surface.
        unsafe {
            set_android_surface_buffer_format(
                self.context.0.as_ptr(),
                self.surface.as_ptr(),
                fourcc,
            )
        };
    }

    /// Hiding rides the existing position pipe rather than a new FFI entry point: the native side
    /// forwards whatever it is given straight to the app, so a coordinate the guest can never
    /// produce carries the message with no change to the C bridge or the AIDL.
    fn set_cursor_visible(&mut self, visible: bool) {
        if visible {
            return; // the next real position makes it visible again
        }
        // SAFETY: context is an opaque handle.
        unsafe {
            set_android_surface_position(
                self.context.0.as_ptr(),
                CURSOR_HIDDEN_POS,
                CURSOR_HIDDEN_POS,
            )
        };
    }

    fn flip_to(
        &mut self,
        import_id: u32,
        _acquire_timepoint: Option<SemaphoreTimepoint>,
        _release_timepoint: Option<SemaphoreTimepoint>,
        _extra_info: Option<crate::FlipToExtraInfo>,
    ) -> anyhow::Result<sync::Waitable> {
        self.flip_import(import_id, None)
    }

    fn configure_scanout_geometry(&mut self, geometry: ScanoutGeometry) -> anyhow::Result<()> {
        anyhow::ensure!(geometry.valid(), "invalid native scanout tuple");
        // SAFETY: opaque handles remain owned and the ABI borrows this tuple.
        let accepted = unsafe { android_display_configure_scanout_geometry(
            self.context.0.as_ptr(), self.surface.as_ptr().cast(), &geometry) };
        anyhow::ensure!(accepted, "native parent unavailable or tuple rejected");
        Ok(())
    }

    fn query_scanout_profile(&mut self) -> anyhow::Result<ScanoutProfile> {
        let mut profile = ScanoutProfile::empty();
        // SAFETY: live context/surface handles and writable fixed-size output.
        let accepted = unsafe { android_display_query_scanout_profile(
            self.context.0.as_ptr(), self.surface.as_ptr().cast(), &mut profile) };
        anyhow::ensure!(accepted && profile.valid(), "native profile unavailable");
        Ok(profile)
    }
    fn configure_scanout_profile(&mut self, geometry: ScanoutGeometry, endpoint: u64, profile: u64) -> anyhow::Result<()> {
        // SAFETY: live handles; the native mutex compares both epochs atomically.
        let accepted = unsafe { android_display_configure_scanout_profile(
            self.context.0.as_ptr(), self.surface.as_ptr().cast(), &geometry, endpoint, profile) };
        anyhow::ensure!(accepted, "native profile configure rejected");
        Ok(())
    }

    fn flip_to_geometry(&mut self, import_id: u32, geometry: ScanoutGeometry) -> anyhow::Result<sync::Waitable> {
        anyhow::ensure!(geometry.valid(), "invalid native scanout tuple");
        self.flip_import(import_id, Some(geometry))
    }

    fn take_flip_completion_fence(&mut self) -> Option<FlipCompletionFence> {
        self.pending_flip_fence.take()
    }
}

impl AndroidSurface {
    fn flip_import(&mut self, import_id: u32, geometry: Option<ScanoutGeometry>) -> anyhow::Result<sync::Waitable> {
        if self.pending_flip_fence.is_some() {
            return Err(anyhow::anyhow!(
                "previous Android display completion fence was not collected"
            ));
        }
        let import = self
            .imports
            .borrow()
            .get(&import_id)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("invalid Android display import id {}", import_id))?;
        // On the synchronous blit path the bridge waits for the Turnip queue before handing the AHB
        // to SurfaceControl and reports -1 here. The async path instead exports a sync_file for the
        // blit's completion and transfers it to us, so this fd must be owned on every success path
        // or the process leaks one per presented frame.
        let mut completion_fence_fd: c_int = -1;
        if geometry.is_some() && !matches!(import, AndroidImport::SharedAhb(_)) {
            return Err(anyhow::anyhow!("native geometry requires authentic AHB storage"));
        }
        let success = unsafe {
            match import {
                AndroidImport::Vulkan(raw_handle) => android_display_flip_to(
                    self.context.0.as_ptr(),
                    self.surface.as_ptr(),
                    raw_handle,
                    &mut completion_fence_fd,
                ),
                AndroidImport::SharedAhb(holder) if geometry.is_some() => {
                    let geometry = geometry.unwrap();
                    android_display_present_shared_ahb_geometry(self.context.0.as_ptr(),
                        self.surface.as_ptr().cast(), holder, -1, &mut completion_fence_fd,
                        self.refresh_generation.get(), geometry.mode_generation, geometry.reset_generation)
                }
                AndroidImport::SharedAhb(holder) => android_display_refresh_shared_ahb(
                    self.context.0.as_ptr(),
                    self.surface.as_ptr(),
                    holder,
                    -1,
                    &mut completion_fence_fd,
                    self.refresh_generation.get(),
                ),
            }
        };
        // Adopt the fd before the error check: `success` says whether the flip happened, not
        // whether an fd came back, and leaking it on the failure path would be the same bug.
        let completion_fence = if completion_fence_fd >= 0 {
            // SAFETY: the bridge transfers ownership of this fd to us, and it is only written when
            // the export succeeded.
            let fd = unsafe { SafeDescriptor::from_raw_descriptor(completion_fence_fd) };
            Some(match import {
                AndroidImport::Vulkan(_) => FlipCompletionFence::SyncFile(fd),
                AndroidImport::SharedAhb(_) => FlipCompletionFence::AndroidReleaseEvent(fd),
            })
        } else {
            None
        };
        if !success {
            return Err(anyhow::anyhow!(
                "Android display direct/native presentation failed"
            ));
        }
        // Park the fence for take_flip_completion_fence(). The caller (virtio_gpu.rs
        // flush_resource) collects it and defers the RESOURCE_FLUSH virtio fence until it
        // signals, which is what finally orders the guest's reuse of the source dmabuf against
        // the async blit reading it (the bridge's own reclaimSlot() only bounds the AHB slots,
        // not the source). The guard above makes an uncollected fence a hard admission failure,
        // never an implicit close-and-reuse.
        self.pending_flip_fence = completion_fence;
        Ok(sync::Waitable::signaled())
    }

}

pub struct DisplayAndroid {
    native_blt: crate::native_blt::NativeBltEngine,
    context: Rc<AndroidDisplayContextWrapper>,
    refresh_generation: Rc<Cell<u64>>,
    imports: Rc<RefCell<BTreeMap<u32, AndroidImport>>>,
    surfaces: Rc<RefCell<BTreeMap<u32, NonNull<AndroidDisplaySurface>>>>,
    /// Real main-surface attachment notifications, coalesced by eventfd.
    event: Event,
}

#[cfg(all(test, feature = "android_display_stub"))]
mod surface_lifetime_tests {
    use super::*;
    use crate::gpu_display_android_stub::SURFACE_LIFETIME_CALLS;

    #[test]
    fn native_window_formats_preserve_channel_order_and_bpp() {
        assert_eq!(android_buffer_format(1), (DRM_FORMAT_ABGR8888, 4));
        assert_eq!(android_buffer_format(2), (DRM_FORMAT_XBGR8888, 4));
        assert_eq!(android_buffer_format(4), (DRM_FORMAT_RGB565, 2));
        assert_eq!(android_buffer_format(5), (DRM_FORMAT_ARGB8888, 4));
        assert_eq!(android_buffer_format(0), (DRM_FORMAT_ABGR8888, 4));
    }

    fn take_calls() -> Vec<(&'static str, usize)> {
        SURFACE_LIFETIME_CALLS.with(|calls| std::mem::take(&mut *calls.borrow_mut()))
    }

    #[test]
    fn geometry_ffi_preserves_tuple_and_distinct_generation_arguments() {
        use crate::gpu_display_android_stub::{GEOMETRY_CONFIGURE, GEOMETRY_PRESENT, PROFILE_CONFIGURE};
        struct Allocation;
        impl HostSharedAllocation for Allocation {
            fn description(&self) -> SharedAllocationDescription {
                SharedAllocationDescription {
                    version: 1, size: 64, buffer_id: 99, allocation_size: 32,
                    plane_stride: 8, width: 2, height: 4,
                    fourcc: u32::from_le_bytes(*b"AB24"), plane_count: 1,
                    layout_flags: 1, ..Default::default()
                }
            }
            fn native_handle(&self) -> *mut c_void { 99usize as *mut c_void }
            fn duplicate_descriptor(&self) -> anyhow::Result<SafeDescriptor> {
                anyhow::bail!("fixture has no DMA fd")
            }
        }
        GEOMETRY_CONFIGURE.with(|call| call.set(None));
        GEOMETRY_PRESENT.with(|call| call.set(None));
        let mut display = crate::GpuDisplay::open_android("geometry-ffi").unwrap();
        let params = DisplayParameters::default_with_mode(vm_control::gpu::DisplayMode::Windowed(4, 2));
        let surface = display.create_surface(None, Some(0), &params, SurfaceType::Scanout).unwrap();
        let mut geometry = ScanoutGeometry::identity(4, 2, 71, 23);
        geometry.storage_width = 2;
        geometry.storage_height = 4;
        geometry.content_rotation = 3;
        // The stub deliberately refuses configuration/presentation: this tests
        // production ABI argument routing and error propagation, not hardware.
        assert!(display.configure_scanout_geometry(surface, geometry).is_err());
        assert_eq!(GEOMETRY_CONFIGURE.with(|call| call.get()), Some(geometry));
        assert!(display.query_scanout_profile(surface).is_err());
        assert!(display.configure_scanout_profile(surface, geometry, 83, 97).is_err());
        assert_eq!(PROFILE_CONFIGURE.with(|call| call.get()), Some((83, 97)));
        assert_eq!(GEOMETRY_CONFIGURE.with(|call| call.get()), Some(geometry));
        let allocation = Allocation;
        let import = display.import_resource(surface,
            crate::DisplayExternalResourceImport::SharedAhb { allocation: &allocation }).unwrap();
        display.set_native_refresh_generation(47);
        assert!(display.flip_to_geometry(surface, import, geometry).is_err());
        assert_eq!(GEOMETRY_PRESENT.with(|call| call.get()), Some((99, -1, 47, 23, 71)));
        assert!(display.take_flip_completion_fence(surface).is_none());
        display.release_import(import, surface);
    }

    #[test]
    fn release_detaches_guest_surface_and_reuses_app_owned_surface() {
        take_calls();
        let mut display = crate::GpuDisplay::open_android("surface-lifetime").unwrap();
        let params = DisplayParameters::default();
        let scanout = display.create_surface(None, Some(0), &params, SurfaceType::Scanout).unwrap();
        let cursor = display.create_surface(Some(scanout), None, &params, SurfaceType::Cursor).unwrap();
        let created = take_calls();
        assert_eq!(created.iter().map(|c| c.0).collect::<Vec<_>>(),
                   vec!["context-create", "surface-create", "surface-create"]);
        display.release_surface(scanout);
        assert_eq!(take_calls(), vec![("surface-destroy", created[1].1)]);
        // simplefb/modeset can create a replacement wrapper without a new
        // Activity attachment. The native service's surface is still alive.
        let replacement = display.create_surface(None, Some(0), &params, SurfaceType::Scanout).unwrap();
        assert_eq!(take_calls(), vec![("surface-create", created[1].1)]);
        display.release_surface(cursor);
        assert_eq!(take_calls(), vec![("surface-destroy", created[2].1)]);
        display.release_surface(replacement);
        assert_eq!(take_calls(), vec![("surface-destroy", created[1].1)]);
        drop(display);
        assert_eq!(take_calls(), vec![("context-destroy", created[0].1)]);
    }

    #[test]
    fn alias_surface_releases_only_after_last_guest_wrapper() {
        take_calls();
        let mut display = crate::GpuDisplay::open_android("surface-alias").unwrap();
        let params = DisplayParameters::default();
        let first = display.create_surface(None, Some(0), &params, SurfaceType::Scanout).unwrap();
        let second = display.create_surface(None, Some(1), &params, SurfaceType::Scanout).unwrap();
        let created = take_calls();
        assert_eq!(created[1].1, created[2].1);
        display.release_surface(first);
        assert!(take_calls().is_empty());
        drop(display);
        assert_eq!(take_calls(), vec![("surface-destroy", created[2].1),
                                     ("context-destroy", created[0].1)]);
        assert_ne!(first, second);
    }

    #[test]
    fn reset_request_retains_wrappers_and_avoids_synchronous_drop_detach() {
        take_calls();
        let mut display = crate::GpuDisplay::open_android("surface-reset").unwrap();
        let params = DisplayParameters::default();
        let surface = display.create_surface(None, Some(0), &params, SurfaceType::Scanout).unwrap();
        let created = take_calls();
        assert!(display.request_reset_detach());
        assert_eq!(take_calls(), vec![("surface-detach-request", created[1].1)]);
        assert!(display.request_reset_detach());
        assert!(take_calls().is_empty());
        display.release_surface(surface);
        // No second synchronous C++ destroy call after accepted async detach.
        assert!(take_calls().is_empty());
        drop(display);
        assert_eq!(take_calls(), vec![("context-destroy", created[0].1)]);
    }
}

impl DisplayAndroid {
    pub fn new(name: &str) -> GpuDisplayResult<DisplayAndroid> {
        let name = CString::new(name).unwrap();
        let context = NonNull::new(
            // SAFETY: service_name is not leaked outside of this function
            unsafe { create_android_display_context(name.as_ptr(), error_callback) },
        )
        .ok_or(GpuDisplayError::Unsupported)?;
        let context = AndroidDisplayContextWrapper(context);
        let event = Event::new().map_err(|_| GpuDisplayError::CreateEvent)?;
        // SAFETY: the bridge duplicates the fd and owns its copy until unregister.
        if !unsafe { android_display_set_native_refresh_event(context.0.as_ptr(), event.as_raw_descriptor()) } {
            return Err(GpuDisplayError::CreateEvent);
        }
        Ok(DisplayAndroid {
            native_blt: Default::default(),
            context: context.into(),
            refresh_generation: Rc::new(Cell::new(0)),
            imports: Rc::new(RefCell::new(BTreeMap::new())),
            surfaces: Rc::new(RefCell::new(BTreeMap::new())),
            event,
        })
    }
}

impl Drop for DisplayAndroid {
    fn drop(&mut self) {
        let context = self.context.0.as_ptr();
        // SAFETY: unregister before the event and context owners disappear.
        unsafe { android_display_set_native_refresh_event(context, -1) };
        let imports = std::mem::take(&mut *self.imports.borrow_mut());
        for import in imports.into_values() {
            if let AndroidImport::Vulkan(raw_handle) = import {
                // SAFETY: every Vulkan handle was created by the matching native import call and
                // is released before the context is dropped. Native AHB holders are owned by the
                // allocation object and require no release-import call.
                unsafe { android_display_release_import(context, raw_handle) };
            }
        }
    }
}

impl DisplayT for DisplayAndroid {
    fn native_blt_copy(&mut self, source: &dyn HostSharedAllocation,
        destination: &dyn HostSharedAllocation, regions: &[crate::NativeBltRegion]) -> anyhow::Result<u64> {
        Ok(self.native_blt.copy(source,destination,regions)?)
    }
    fn native_blt_upload(&mut self, destination: &dyn HostSharedAllocation, source: &[u8],
        layout: crate::NativeBltCpuLayout, regions: &[crate::NativeBltRegion]) -> anyhow::Result<u64> {
        Ok(self.native_blt.upload(destination,source,layout,regions)?)
    }
    fn native_blt_readback(&mut self, source: &dyn HostSharedAllocation, destination: &mut [u8],
        layout: crate::NativeBltCpuLayout, regions: &[crate::NativeBltRegion]) -> anyhow::Result<u64> {
        Ok(self.native_blt.readback(source,destination,layout,regions)?)
    }

    fn flush(&self) {
        // The GPU worker reads current generation after draining the event.
        // An attach racing this drain either appears in that read or leaves
        // the descriptor readable for the next iteration.
        if let Err(error) = self.event.reset() {
            error!("failed to drain native refresh event: {}", error);
        }
    }

    fn native_refresh_generation(&self) -> u64 {
        // SAFETY: context is live; C++ snapshots under its attachment mutex.
        unsafe { android_display_native_refresh_generation(self.context.0.as_ptr()) }
    }

    fn reset_native_scanout(&self, old_reset: u64, new_reset: u64) -> anyhow::Result<()> {
        // SAFETY: live context, scalar reset-domain tokens; no borrowed buffer.
        anyhow::ensure!(unsafe { android_display_reset_scanout_geometry(
            self.context.0.as_ptr(), old_reset, new_reset) }, "native bridge reset refused");
        Ok(())
    }

    fn display_timing(&self) -> Option<crate::DisplayTimingSample> {
        let mut sample = crate::DisplayTimingSample::default();
        // SAFETY: context remains owned and sample is writable for its exact C ABI size.
        if unsafe { android_display_get_timing(self.context.0.as_ptr(), &mut sample) } {
            Some(sample)
        } else {
            None
        }
    }

    fn set_native_refresh_generation(&self, generation: u64) {
        self.refresh_generation.set(generation);
    }

    fn is_dmabuf_import_supported(&mut self) -> bool {
        // SAFETY: context is a live opaque handle owned by this DisplayAndroid.
        unsafe { android_display_is_vulkan_blit_available(self.context.0.as_ptr()) }
    }

    /// The app already tells this side when nobody is watching: leaving the display view destroys
    /// the SurfaceView, and the `removeSurface(forCursor=false)` that follows clears the native
    /// window behind the scanout. That knowledge stopped at the C bridge -- the trait default
    /// answered `true` regardless -- so the simplefb bridge went on reading a whole framebuffer out
    /// of guest memory 30 times a second and copying it into a sink buffer that is never displayed.
    ///
    /// Answering honestly is only useful because the consumer of the answer is
    /// `simplefb_display_loop`, and it is only safe there for the same reason: a frame may be
    /// skipped on a `false` where the producer is a timer that will offer the next one anyway, so a
    /// consumer that comes back is picked up on the following tick. A producer driven by the guest
    /// (virtio-gpu's flush) never gets a second chance at a dropped frame and must not gate on this.
    fn has_consumer(&self) -> bool {
        // SAFETY: context is a live opaque handle owned by this DisplayAndroid.
        unsafe { android_display_has_consumer(self.context.0.as_ptr()) }
    }

    fn consumer_generation(&self) -> u64 {
        // SAFETY: context is a live opaque handle owned by this DisplayAndroid.
        unsafe { android_display_consumer_generation(self.context.0.as_ptr()) }
    }

    fn create_surface(
        &mut self,
        parent_surface_id: Option<u32>,
        surface_id: u32,
        _scanout_id: Option<u32>,
        display_params: &DisplayParameters,
        surf_type: SurfaceType,
    ) -> GpuDisplayResult<Box<dyn GpuDisplaySurface>> {
        // A parented surface is virtio-gpu's cursor. Its scanout carries no display_params of its
        // own, so crosvm passes DisplayParameters::default() -- sizing the cursor surface from
        // that would configure it at the default DISPLAY resolution instead of the cursor plane's
        // 64x64, and the Android side would allocate a full-screen buffer per pointer image.
        let (requested_width, requested_height) = if parent_surface_id.is_some() {
            (CURSOR_PLANE_SIZE, CURSOR_PLANE_SIZE)
        } else {
            display_params.get_virtual_display_size()
        };
        // SAFETY: context is an opaque handle.
        let surface = NonNull::new(unsafe {
            create_android_surface(
                self.context.0.as_ptr(),
                requested_width,
                requested_height,
                parent_surface_id.is_some(),
            )
        })
        .ok_or(GpuDisplayError::CreateSurface)?;
        if parent_surface_id.is_none()
            && matches!(surf_type, SurfaceType::Scanout)
            && display_params.refresh_rate > 0
        {
            // Apply this on the Android side when the SurfaceView's native window is connected.
            unsafe {
                set_android_surface_frame_rate(
                    self.context.0.as_ptr(),
                    surface.as_ptr(),
                    display_params.refresh_rate,
                )
            };
        }
        self.surfaces.borrow_mut().insert(surface_id, surface);

        Ok(Box::new(AndroidSurface {
            context: self.context.clone(),
            refresh_generation: self.refresh_generation.clone(),
            surface,
            surface_id,
            surfaces: self.surfaces.clone(),
            reset_detach_requested: false,
            imports: self.imports.clone(),
            pending_flip_fence: None,
        }))
    }

    fn import_resource(
        &mut self,
        import_id: u32,
        surface_id: u32,
        external_display_resource: crate::DisplayExternalResourceImport,
    ) -> anyhow::Result<()> {
        let surface = self
            .surfaces
            .borrow()
            .get(&surface_id)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("invalid Android display surface {}", surface_id))?;
        let import = match external_display_resource {
            crate::DisplayExternalResourceImport::Dmabuf {
                descriptor,
                offset,
                stride,
                modifiers,
                linear_layout_verified,
                width,
                height,
                fourcc,
            } => {
                let raw_handle = unsafe {
                    android_display_import_dmabuf(
                        self.context.0.as_ptr(),
                        surface.as_ptr(),
                        descriptor.as_raw_descriptor(),
                        offset,
                        stride,
                        modifiers,
                        linear_layout_verified,
                        width,
                        height,
                        fourcc,
                    )
                };
                if raw_handle == 0 {
                    return Err(anyhow::anyhow!("Turnip DMA-BUF import failed"));
                }
                AndroidImport::Vulkan(raw_handle)
            }
            crate::DisplayExternalResourceImport::SharedAhb { allocation } => {
                let description = allocation.description();
                let holder = allocation.native_handle();
                if holder.is_null() || !description.valid(description.width, description.height) {
                    return Err(anyhow::anyhow!(
                        "native shared allocation has no authenticated Android holder"
                    ));
                }
                AndroidImport::SharedAhb(holder as u64)
            }
            _ => {
                return Err(anyhow::anyhow!(
                    "Android display only supports DMA-BUF or direct AHB imports"
                ))
            }
        };
        self.imports.borrow_mut().insert(import_id, import);
        Ok(())
    }

    fn release_import(&mut self, import_id: u32, _surface_id: u32) {
        // The common display wrapper owns the import id counter; remove the native
        // object by id and let the shared map keep surface flips coherent.
        if let Some((_, import)) = self.imports.borrow_mut().remove_entry(&import_id) {
            if let AndroidImport::Vulkan(raw_handle) = import {
                unsafe { android_display_release_import(self.context.0.as_ptr(), raw_handle) };
            }
        }
    }

    fn allocate_host_shared_allocation(
        &mut self,
        width: u32,
        height: u32,
        fourcc: u32,
    ) -> anyhow::Result<Box<dyn HostSharedAllocation>> {
        let mut description = SharedAllocationDescription::default();
        let mut raw_fd: RawDescriptor = -1;
        let holder = unsafe {
            android_display_create_shared_ahb_v2(
                width,
                height,
                fourcc,
                &mut description,
                &mut raw_fd,
                error_callback,
            )
        };
        let holder = match NonNull::new(holder) {
            Some(holder) => holder,
            None => {
                // The bridge owns any descriptor it writes, even if allocation fails to return
                // its opaque holder. Adopt it immediately so malformed native failures cannot
                // leak one fd per attempt.
                if raw_fd >= 0 {
                    // SAFETY: a non-negative descriptor is transferred by the bridge and is
                    // therefore owned by this caller on every return path.
                    drop(unsafe { SafeDescriptor::from_raw_descriptor(raw_fd) });
                }
                return Err(anyhow::anyhow!("Android gralloc AHB allocation failed"));
            }
        };
        if raw_fd < 0 || description.fourcc != fourcc || !description.valid(width, height) {
            if raw_fd >= 0 {
                // SAFETY: the descriptor was transferred by the bridge; the holder has a
                // separate internal duplicate and does not own this exported fd.
                drop(unsafe { SafeDescriptor::from_raw_descriptor(raw_fd) });
            }
            unsafe { android_display_destroy_shared_ahb(holder.as_ptr()) };
            return Err(anyhow::anyhow!(
                "Android shared AHB returned invalid descriptor or DMA-BUF"
            ));
        }
        // SAFETY: ownership of the descriptor is transferred by the FFI on
        // successful allocation, exactly once.
        let dma_buf = unsafe { SafeDescriptor::from_raw_descriptor(raw_fd) };
        Ok(Box::new(AndroidSharedAllocation {
            holder,
            description,
            dma_buf,
        }))
    }
}

impl SysDisplayT for DisplayAndroid {}

impl AsRawDescriptor for DisplayAndroid {
    fn as_raw_descriptor(&self) -> RawDescriptor {
        self.event.as_raw_descriptor()
    }
}
