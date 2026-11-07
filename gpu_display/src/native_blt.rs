// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license.

//! Explicit GPU copies. The caller must retire producers and every Android
//! reader before granting exclusive access to either native allocation.
//! These APIs complete the GPU work before returning success; they never map
//! native memory on the CPU. Ordinary slices are explicit upload/readback.

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NativeBltRegion {
    pub source_x: u32,
    pub source_y: u32,
    pub destination_x: u32,
    pub destination_y: u32,
    pub width: u32,
    pub height: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NativeBltCpuLayout {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub fourcc: u32,
    pub size: u64,
}

const _: () = assert!(std::mem::size_of::<NativeBltRegion>() == 24);
const _: () = assert!(std::mem::size_of::<NativeBltCpuLayout>() == 24);

pub fn native_blt_bytes(
    sw: u32,
    sh: u32,
    dw: u32,
    dh: u32,
    regions: &[NativeBltRegion],
) -> Option<u64> {
    if [sw, sh, dw, dh].iter().any(|n| *n == 0 || *n > 8192)
        || regions.is_empty()
        || regions.len() > 256
    {
        return None;
    }
    let mut bytes = 0u64;
    for r in regions {
        if r.width == 0
            || r.height == 0
            || r.source_x >= sw
            || r.source_y >= sh
            || r.destination_x >= dw
            || r.destination_y >= dh
            || r.width > sw - r.source_x
            || r.height > sh - r.source_y
            || r.width > dw - r.destination_x
            || r.height > dh - r.destination_y
        {
            return None;
        }
        bytes = bytes.checked_add(u64::from(r.width) * u64::from(r.height) * 4)?;
    }
    Some(bytes)
}

impl NativeBltCpuLayout {
    pub fn valid(&self, length: usize) -> bool {
        self.width > 0
            && self.height > 0
            && self.width <= 8192
            && self.height <= 8192
            && self.stride % 4 == 0
            && u64::from(self.stride) >= u64::from(self.width) * 4
            && [u32::from_le_bytes(*b"AB24"), u32::from_le_bytes(*b"AR24"),
                u32::from_le_bytes(*b"XR24")].contains(&self.fourcc)
            && self.size <= 256 * 1024 * 1024
            && self.size <= length as u64
            && u64::from(self.stride) * u64::from(self.height - 1) + u64::from(self.width) * 4
                <= self.size
    }
}

#[cfg(feature = "android_display")]
pub(crate) use engine::NativeBltEngine;

#[cfg(feature = "android_display")]
mod engine {
    use super::*;
    use crate::HostSharedAllocation;
    use std::ffi::c_void;
    use std::ptr::NonNull;

    extern "C" {
        fn android_display_native_blt_create() -> *mut c_void;
        fn android_display_native_blt_destroy(context: *mut c_void) -> i32;
        fn android_display_native_blt_copy_wait(
            context: *mut c_void,
            source: *mut c_void,
            destination: *mut c_void,
            regions: *const NativeBltRegion,
            count: u32,
        ) -> i32;
        fn android_display_native_blt_upload(
            context: *mut c_void,
            destination: *mut c_void,
            source: *const u8,
            layout: *const NativeBltCpuLayout,
            regions: *const NativeBltRegion,
            count: u32,
            completed: *mut u64,
        ) -> i32;
        fn android_display_native_blt_readback(
            context: *mut c_void,
            source: *mut c_void,
            destination: *mut u8,
            layout: *const NativeBltCpuLayout,
            regions: *const NativeBltRegion,
            count: u32,
            completed: *mut u64,
        ) -> i32;
    }

    // Not Send/Sync: the GPU worker serializes both this context and ownership
    // admission. A failed submission permanently disables this activation's
    // engine. Native code retains all imported storage on uncertain completion.
    #[derive(Default)]
    pub(crate) struct NativeBltEngine {
        context: Option<NonNull<c_void>>,
        poisoned: bool,
        _thread: std::marker::PhantomData<std::rc::Rc<()>>,
    }

    type Result<T> = std::io::Result<T>;
    fn invalid() -> std::io::Error {
        std::io::Error::from_raw_os_error(22)
    }
    fn failed() -> std::io::Error {
        std::io::Error::other("native GPU copy failed; resources require quarantine")
    }

    impl NativeBltEngine {
        fn context(&mut self) -> Result<NonNull<c_void>> {
            if self.poisoned {
                return Err(failed());
            }
            if self.context.is_none() {
                // SAFETY: context ownership transfers to this engine, serialized
                // on the GPU worker. No borrowed allocation is passed here.
                self.context = NonNull::new(unsafe { android_display_native_blt_create() });
            }
            self.context
                .ok_or_else(|| std::io::Error::other("native GPU copy unavailable"))
        }

        pub(crate) fn copy(
            &mut self,
            source: &dyn HostSharedAllocation,
            destination: &dyn HostSharedAllocation,
            regions: &[NativeBltRegion],
        ) -> Result<u64> {
            let s = source.description();
            let d = destination.description();
            if !s.valid(s.width, s.height)
                || !d.valid(d.width, d.height)
                || s.buffer_id == d.buffer_id
                || source.native_handle().is_null()
                || destination.native_handle().is_null()
            {
                return Err(invalid());
            }
            let bytes = native_blt_bytes(s.width, s.height, d.width, d.height, regions)
                .ok_or_else(invalid)?;
            let context = self.context()?;
            // SAFETY: authentic holders remain borrowed through the bounded GPU
            // wait. C++ owns additional AHB/DMA import references if it fails.
            let result = unsafe {
                android_display_native_blt_copy_wait(
                    context.as_ptr(),
                    source.native_handle(),
                    destination.native_handle(),
                    regions.as_ptr(),
                    regions.len() as u32,
                )
            };
            if result != 0 {
                self.poisoned = true;
                return Err(failed());
            }
            Ok(bytes)
        }

        fn transfer_bytes(
            allocation: &dyn HostSharedAllocation,
            length: usize,
            layout: NativeBltCpuLayout,
            regions: &[NativeBltRegion],
            upload: bool,
        ) -> Result<u64> {
            let d = allocation.description();
            if !d.valid(d.width, d.height)
                || allocation.native_handle().is_null()
                || !layout.valid(length)
            {
                return Err(invalid());
            }
            let bytes = if upload {
                native_blt_bytes(layout.width, layout.height, d.width, d.height, regions)
            } else {
                native_blt_bytes(d.width, d.height, layout.width, layout.height, regions)
            }
            .ok_or_else(invalid)?;
            if bytes > 64 * 1024 * 1024 {
                return Err(invalid());
            }
            Ok(bytes)
        }

        fn complete(&mut self, result: i32, completed: u64, expected: u64) -> Result<u64> {
            if result != 0 || completed != expected {
                self.poisoned = true;
                return Err(failed());
            }
            Ok(completed)
        }

        pub(crate) fn upload(
            &mut self,
            destination: &dyn HostSharedAllocation,
            source: &[u8],
            layout: NativeBltCpuLayout,
            regions: &[NativeBltRegion],
        ) -> Result<u64> {
            let expected = Self::transfer_bytes(destination, source.len(), layout, regions, true)?;
            let context = self.context()?;
            let mut completed = 0;
            // SAFETY: validated exact slice/layout/region bounds. Native code
            // copies the ordinary bytes into owned staging before submission.
            let result = unsafe {
                android_display_native_blt_upload(
                    context.as_ptr(),
                    destination.native_handle(),
                    source.as_ptr(),
                    &layout,
                    regions.as_ptr(),
                    regions.len() as u32,
                    &mut completed,
                )
            };
            self.complete(result, completed, expected)
        }

        pub(crate) fn readback(
            &mut self,
            source: &dyn HostSharedAllocation,
            destination: &mut [u8],
            layout: NativeBltCpuLayout,
            regions: &[NativeBltRegion],
        ) -> Result<u64> {
            let expected = Self::transfer_bytes(source, destination.len(), layout, regions, false)?;
            let context = self.context()?;
            let mut completed = 0;
            // SAFETY: validated writable slice; C++ writes it only after GPU
            // completion and cache invalidation and retains no slice pointer.
            let result = unsafe {
                android_display_native_blt_readback(
                    context.as_ptr(),
                    source.native_handle(),
                    destination.as_mut_ptr(),
                    &layout,
                    regions.as_ptr(),
                    regions.len() as u32,
                    &mut completed,
                )
            };
            self.complete(result, completed, expected)
        }
    }

    impl Drop for NativeBltEngine {
        fn drop(&mut self) {
            if let Some(context) = self.context.take() {
                // SAFETY: only this owner can destroy the context. -EBUSY
                // intentionally retains the entire native object and its GPU
                // dependencies for the remaining process lifetime.
                let status = unsafe { android_display_native_blt_destroy(context.as_ptr()) };
                if status != 0 {
                    eprintln!("native Blt GPU context quarantined on close: {status}");
                }
            }
        }
    }
}
