// Copyright 2024 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Stub implementation of the native interface of libcrosvm_android_display_client
//!
//! This implementation is used to enable the gpu display backend for Android to be compiled
//! without libcrosvm_android_display_client available. It is only used for testing purposes and
//! not functional at runtime.

use std::ffi::c_char;
use std::ffi::c_int;

use crate::gpu_display_android::ANativeWindow_Buffer;
use crate::gpu_display_android::AndroidDisplayContext;
use crate::gpu_display_android::AndroidDisplaySurface;
use crate::gpu_display_android::ErrorCallback;

#[cfg(test)]
thread_local! {
    pub(crate) static SURFACE_LIFETIME_CALLS: std::cell::RefCell<Vec<(&'static str, usize)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    pub(crate) static GEOMETRY_CONFIGURE: std::cell::Cell<Option<crate::ScanoutGeometry>> =
        const { std::cell::Cell::new(None) };
    pub(crate) static GEOMETRY_PRESENT: std::cell::Cell<Option<(u64, i32, u64, u64, u64)>> =
        const { std::cell::Cell::new(None) };
    pub(crate) static PROFILE_CONFIGURE: std::cell::Cell<Option<(u64, u64)>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
struct SurfaceLifetimeContext {
    scanout: u8,
    cursor: u8,
}

#[no_mangle]
extern "C" fn android_display_create_shared_ahb_v2(
    _width: u32,
    _height: u32,
    _fourcc: u32,
    _description: *mut crate::shared_allocation::SharedAllocationDescription,
    _dma_fd: *mut base::RawDescriptor,
    _error_callback: ErrorCallback,
) -> *mut std::ffi::c_void {
    std::ptr::null_mut()
}

#[no_mangle]
extern "C" fn android_display_destroy_shared_ahb(_holder: *mut std::ffi::c_void) {}

#[no_mangle]
extern "C" fn android_display_shared_ahb_read_bytes(
    _holder: *mut std::ffi::c_void,
    _offset: u64,
    _destination: *mut u8,
    _length: u64,
) -> c_int {
    -libc::ENOTSUP
}

#[no_mangle]
extern "C" fn android_display_shared_ahb_write_bytes(
    _holder: *mut std::ffi::c_void,
    _offset: u64,
    _source: *const u8,
    _length: u64,
) -> c_int {
    -libc::ENOTSUP
}

#[no_mangle]
extern "C" fn android_display_shared_ahb_fill_bytes(
    _holder: *mut std::ffi::c_void,
    _offset: u64,
    _length: u64,
    _pattern: u32,
) -> c_int {
    -libc::ENOTSUP
}

#[no_mangle]
extern "C" fn android_display_refresh_shared_ahb(
    _ctx: *mut AndroidDisplayContext,
    _surface: *mut AndroidDisplaySurface,
    _holder: u64,
    _acquire_fence_fd: c_int,
    out_release_fence_fd: *mut c_int,
    _expected_generation: u64,
) -> bool {
    if !out_release_fence_fd.is_null() {
        // SAFETY: caller supplies a writable out parameter, matching the real
        // Android ABI. The stub cannot present a native buffer.
        unsafe { *out_release_fence_fd = -1 };
    }
    false
}

#[no_mangle]
extern "C" fn android_display_configure_scanout_geometry(
    _ctx: *mut AndroidDisplayContext, _surface: *mut std::ffi::c_void,
    _geometry: *const crate::ScanoutGeometry,
) -> bool {
    #[cfg(test)]
    if !_geometry.is_null() {
        // SAFETY: the real Rust caller borrows the live ABI tuple for this call.
        GEOMETRY_CONFIGURE.with(|call| call.set(Some(unsafe { *_geometry })));
    }
    false
}

#[no_mangle]
extern "C" fn android_display_reset_scanout_geometry(_ctx: *mut AndroidDisplayContext,
    _old_reset: u64, _new_reset: u64) -> bool {
    false
}

#[no_mangle]
extern "C" fn android_display_query_scanout_profile(_ctx: *mut AndroidDisplayContext,
    _surface: *mut std::ffi::c_void, profile: *mut crate::ScanoutProfile) -> bool {
    if !profile.is_null() {
        // SAFETY: the ABI caller provides a writable fixed-size structure.
        unsafe { *profile = crate::ScanoutProfile::empty(); }
    }
    false
}
#[no_mangle]
extern "C" fn android_display_configure_scanout_profile(_ctx: *mut AndroidDisplayContext,
    _surface: *mut std::ffi::c_void, _geometry: *const crate::ScanoutGeometry,
    _endpoint: u64, _profile: u64) -> bool {
    #[cfg(test)]
    PROFILE_CONFIGURE.with(|call| call.set(Some((_endpoint, _profile))));
    android_display_configure_scanout_geometry(_ctx, _surface, _geometry)
}

#[no_mangle]
extern "C" fn android_display_present_shared_ahb_geometry(
    ctx: *mut AndroidDisplayContext, surface: *mut std::ffi::c_void, holder: u64,
    acquire_fence_fd: c_int, out_release_fence_fd: *mut c_int,
    attachment_generation: u64, _mode_generation: u64, _reset_generation: u64,
) -> bool {
    #[cfg(test)]
    GEOMETRY_PRESENT.with(|call| call.set(Some((holder, acquire_fence_fd,
        attachment_generation, _mode_generation, _reset_generation))));
    android_display_refresh_shared_ahb(ctx, surface.cast(), holder, acquire_fence_fd,
        out_release_fence_fd, attachment_generation)
}

#[no_mangle]
extern "C" fn create_android_display_context(
    _name: *const c_char,
    _error_callback: ErrorCallback,
) -> *mut AndroidDisplayContext {
    #[cfg(test)]
    {
        let context = Box::into_raw(Box::new(SurfaceLifetimeContext { scanout: 0, cursor: 0 }));
        SURFACE_LIFETIME_CALLS.with(|calls| calls.borrow_mut().push(("context-create", context as usize)));
        return context.cast();
    }
    #[cfg(not(test))]
    unimplemented!();
}

#[no_mangle]
extern "C" fn destroy_android_display_context(_ctx: *mut AndroidDisplayContext) {
    #[cfg(test)]
    {
        SURFACE_LIFETIME_CALLS.with(|calls| calls.borrow_mut().push(("context-destroy", _ctx as usize)));
        // SAFETY: only the test create function supplies this opaque context.
        drop(unsafe { Box::from_raw(_ctx.cast::<SurfaceLifetimeContext>()) });
    }
    #[cfg(not(test))]
    unimplemented!();
}

#[no_mangle]
extern "C" fn android_display_get_timing(
    _context: *mut AndroidDisplayContext, _sample: *mut crate::DisplayTimingSample,
) -> bool {
    false
}

#[no_mangle]
extern "C" fn create_android_surface(
    _ctx: *mut AndroidDisplayContext,
    _width: u32,
    _height: u32,
    _for_cursor: bool,
) -> *mut AndroidDisplaySurface {
    #[cfg(test)]
    {
        // The real service reuses one scanout and one cursor object, even
        // after their guest-side wrappers are released.
        let context = unsafe { &mut *_ctx.cast::<SurfaceLifetimeContext>() };
        let surface: *mut u8 = if _for_cursor { &mut context.cursor } else { &mut context.scanout };
        SURFACE_LIFETIME_CALLS.with(|calls| calls.borrow_mut().push(("surface-create", surface as usize)));
        return surface.cast();
    }
    #[cfg(not(test))]
    unimplemented!();
}

#[no_mangle]
extern "C" fn destroy_android_surface(
    _ctx: *mut AndroidDisplayContext,
    _surface: *mut AndroidDisplaySurface,
) {
    #[cfg(test)]
    SURFACE_LIFETIME_CALLS.with(|calls| calls.borrow_mut().push(("surface-destroy", _surface as usize)));
    #[cfg(not(test))]
    unimplemented!();
}

#[no_mangle]
extern "C" fn request_android_surface_detach(
    _ctx: *mut AndroidDisplayContext,
    _surface: *mut AndroidDisplaySurface,
) -> bool {
    #[cfg(test)]
    SURFACE_LIFETIME_CALLS.with(|calls| calls.borrow_mut().push(("surface-detach-request", _surface as usize)));
    true
}

#[no_mangle]
extern "C" fn set_android_surface_frame_rate(
    _ctx: *mut AndroidDisplayContext,
    _surface: *mut AndroidDisplaySurface,
    _refresh_rate: u32,
) {}

#[no_mangle]
extern "C" fn set_android_surface_position(_ctx: *mut AndroidDisplayContext, _x: u32, _y: u32) {
    unimplemented!();
}

#[no_mangle]
extern "C" fn get_android_surface_buffer(
    _ctx: *mut AndroidDisplayContext,
    _surface: *mut AndroidDisplaySurface,
    _out_buffer: *mut ANativeWindow_Buffer,
) -> u32 {
    unimplemented!();
}

#[no_mangle]
extern "C" fn post_android_surface_buffer(
    _ctx: *mut AndroidDisplayContext,
    _surface: *mut AndroidDisplaySurface,
) {
    unimplemented!();
}

#[no_mangle]
extern "C" fn set_android_surface_buffer_format(
    _ctx: *mut AndroidDisplayContext,
    _surface: *mut AndroidDisplaySurface,
    _fourcc: u32,
) {
    unimplemented!();
}

#[no_mangle]
extern "C" fn android_display_import_dmabuf(
    _ctx: *mut AndroidDisplayContext,
    _surface: *mut AndroidDisplaySurface,
    _fd: base::RawDescriptor,
    _offset: u32,
    _stride: u32,
    _modifier: u64,
    _linear_layout_verified: bool,
    _width: u32,
    _height: u32,
    _fourcc: u32,
) -> i64 {
    0
}

#[no_mangle]
extern "C" fn android_display_release_import(_ctx: *mut AndroidDisplayContext, _raw_handle: i64) {}

#[no_mangle]
extern "C" fn android_display_is_vulkan_blit_available(_ctx: *mut AndroidDisplayContext) -> bool {
    false
}

// Nothing can be attached to a display that does not exist, so `false` is the honest answer here
// as well as the one that keeps this stub free of a panic on a per-frame path.
#[no_mangle]
extern "C" fn android_display_has_consumer(_ctx: *mut AndroidDisplayContext) -> bool {
    false
}

#[no_mangle]
extern "C" fn android_display_consumer_generation(_ctx: *mut AndroidDisplayContext) -> u64 {
    0
}

#[no_mangle]
extern "C" fn android_display_set_native_refresh_event(_ctx: *mut AndroidDisplayContext, _fd: c_int) -> bool {
    true
}

#[no_mangle]
extern "C" fn android_display_native_refresh_generation(_ctx: *mut AndroidDisplayContext) -> u64 {
    0
}

// The headless blit context (the VNC sink's GPU half) lives in the same native library, so its
// symbols need standing in for on the same builds. `create` returning null is the whole answer: it
// is what the real one returns on a machine with no Vulkan blit driver named, and the sink treats
// that as "no GPU half" rather than as an error, so nothing below is ever reached.
#[no_mangle]
extern "C" fn android_blit_ctx_create(_width: u32, _height: u32) -> *mut std::ffi::c_void {
    std::ptr::null_mut()
}

#[no_mangle]
extern "C" fn android_blit_ctx_destroy(_ctx: *mut std::ffi::c_void) {}

#[no_mangle]
extern "C" fn android_blit_ctx_import_dmabuf(
    _ctx: *mut std::ffi::c_void,
    _fd: base::RawDescriptor,
    _offset: u32,
    _stride: u32,
    _modifier: u64,
    _linear_layout_verified: bool,
    _width: u32,
    _height: u32,
    _fourcc: u32,
) -> i64 {
    0
}

#[no_mangle]
extern "C" fn android_blit_ctx_release_import(_ctx: *mut std::ffi::c_void, _import_id: i64) {}

#[no_mangle]
extern "C" fn android_blit_ctx_blit(
    _ctx: *mut std::ffi::c_void,
    _import_id: i64,
    _width: u32,
    _height: u32,
    _timeout_ms: c_int,
) -> bool {
    false
}

#[no_mangle]
extern "C" fn android_blit_ctx_map(
    _ctx: *mut std::ffi::c_void,
    _out_pixels: *mut *const u8,
    _out_stride_bytes: *mut u32,
    _out_width: *mut u32,
    _out_height: *mut u32,
    _out_size: *mut u32,
) -> bool {
    false
}

#[no_mangle]
extern "C" fn android_blit_ctx_unmap(_ctx: *mut std::ffi::c_void) {}

#[no_mangle]
extern "C" fn android_display_flip_to(
    _ctx: *mut AndroidDisplayContext,
    _surface: *mut AndroidDisplaySurface,
    _raw_handle: i64,
    out_completion_fence_fd: *mut c_int,
) -> bool {
    if !out_completion_fence_fd.is_null() {
        // SAFETY: checked non-null, and the caller passes a pointer to its own c_int.
        unsafe { *out_completion_fence_fd = -1 };
    }
    false
}

// The non-Android test backend cannot execute a GPU copy. Never report a
// successful no-op to callers expecting an actual GPU completion.
#[no_mangle]
extern "C" fn android_display_native_blt_create() -> *mut std::ffi::c_void { std::ptr::null_mut() }
#[no_mangle]
extern "C" fn android_display_native_blt_destroy(_context: *mut std::ffi::c_void) -> c_int { 0 }
#[no_mangle]
extern "C" fn android_display_native_blt_copy_wait(_context: *mut std::ffi::c_void,
    _source: *mut std::ffi::c_void, _destination: *mut std::ffi::c_void,
    _regions: *const crate::NativeBltRegion, _count: u32) -> c_int { -libc::ENOTSUP }
#[no_mangle]
unsafe extern "C" fn android_display_native_blt_upload(_context: *mut std::ffi::c_void,
    _destination: *mut std::ffi::c_void, _source: *const u8, _layout: *const crate::NativeBltCpuLayout,
    _regions: *const crate::NativeBltRegion, _count: u32, completed: *mut u64) -> c_int {
    if !completed.is_null() { *completed = 0; }
    -libc::ENOTSUP
}
#[no_mangle]
unsafe extern "C" fn android_display_native_blt_readback(_context: *mut std::ffi::c_void,
    _source: *mut std::ffi::c_void, _destination: *mut u8, _layout: *const crate::NativeBltCpuLayout,
    _regions: *const crate::NativeBltRegion, _count: u32, completed: *mut u64) -> c_int {
    if !completed.is_null() { *completed = 0; }
    -libc::ENOTSUP
}
