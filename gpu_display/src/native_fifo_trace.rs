// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Opt-in host diagnostic scope. The C++ state owns enablement and bounds.
use std::marker::PhantomData;
use std::rc::Rc;

#[cfg(any(all(target_os = "android", feature = "android_display"), test))]
extern "C" {
    fn android_display_native_fifo_trace_begin(command: u32, resource: u32, guest_sequence: u64,
        fence: u64, context: u32, ring: u32, flags: u32, descriptor: u32) -> u64;
    fn android_display_native_fifo_trace_operation(sequence: u64);
    fn android_display_native_fifo_trace_end(id: u64);
}

// C++ keeps the current packet in TLS; a scope must stay on its creating thread.
pub struct NativeFifoTraceScope {
    id: u64,
    _thread: PhantomData<Rc<()>>,
}
impl NativeFifoTraceScope {
    #[allow(clippy::too_many_arguments)]
    pub fn begin(command: u32, resource: u32, guest_sequence: u64, fence: u64,
                 context: u32, ring: u32, flags: u32, descriptor: u32) -> Self {
        #[cfg(any(all(target_os = "android", feature = "android_display"), test))]
        let id = unsafe { android_display_native_fifo_trace_begin(command, resource, guest_sequence,
            fence, context, ring, flags, descriptor) };
        #[cfg(not(any(all(target_os = "android", feature = "android_display"), test)))]
        let id = {
            let _ = (command, resource, guest_sequence, fence, context, ring, flags, descriptor);
            0
        };
        Self { id, _thread: PhantomData }
    }
}
impl Drop for NativeFifoTraceScope {
    fn drop(&mut self) {
        if self.id == 0 { return; }
        #[cfg(any(all(target_os = "android", feature = "android_display"), test))]
        unsafe { android_display_native_fifo_trace_end(self.id); }
    }
}
pub fn native_fifo_trace_operation(sequence: u64) {
    #[cfg(any(all(target_os = "android", feature = "android_display"), test))]
    unsafe { android_display_native_fifo_trace_operation(sequence); }
    #[cfg(not(any(all(target_os = "android", feature = "android_display"), test)))]
    let _ = sequence;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    static CALLS: Mutex<Vec<Vec<u64>>> = Mutex::new(Vec::new());
    #[no_mangle]
    extern "C" fn android_display_native_fifo_trace_begin(c: u32, r: u32, gs: u64, f: u64,
        ctx: u32, ring: u32, flags: u32, desc: u32) -> u64 {
        CALLS.lock().unwrap().push(vec![c as u64,r as u64,gs,f,ctx as u64,ring as u64,flags as u64,desc as u64]);
        if c == 0 { 0 } else { 91 }
    }
    #[no_mangle]
    extern "C" fn android_display_native_fifo_trace_end(id: u64) {
        CALLS.lock().unwrap().push(vec![id]);
    }
    #[no_mangle]
    extern "C" fn android_display_native_fifo_trace_operation(seq: u64) {
        CALLS.lock().unwrap().push(vec![0,seq]);
    }
    fn early_return() {
        let _scope = NativeFifoTraceScope::begin(3,4,u64::MAX,6,7,8,9,10);
        native_fifo_trace_operation(123);
    }
    #[test]
    fn real_raii_forwards_all_fields_and_ends_only_nonzero_scopes() {
        early_return();
        { let _disabled = NativeFifoTraceScope::begin(0,1,2,3,4,5,6,7); }
        assert_eq!(*CALLS.lock().unwrap(), vec![
            vec![3,4,u64::MAX,6,7,8,9,10], vec![0,123], vec![91], vec![0,1,2,3,4,5,6,7]]);
    }
}
