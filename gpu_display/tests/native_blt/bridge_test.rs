// Compile the production bridge against a deterministic C ABI and allocation fixture.
use std::ffi::c_void;
#[derive(Clone, Copy)]
pub struct SharedAllocationDescription {
    pub width: u32,
    pub height: u32,
    pub buffer_id: u64,
}
impl SharedAllocationDescription {
    pub fn valid(&self, w: u32, h: u32) -> bool {
        w > 0
            && h > 0
            && w <= 8192
            && h <= 8192
            && w == self.width
            && h == self.height
            && self.buffer_id != 0
    }
}
pub trait HostSharedAllocation {
    fn description(&self) -> SharedAllocationDescription;
    fn native_handle(&self) -> *mut c_void;
}
#[path = "../../src/native_blt.rs"]
mod native_blt;
use native_blt::*;
use std::cell::RefCell;
#[derive(Default)]
struct Calls {
    create: u32,
    copy: u32,
    upload: u32,
    readback: u32,
    close: u32,
    status: i32,
    short: bool,
    unavailable: bool,
}
thread_local! { static CALLS:RefCell<Calls>=RefCell::new(Calls::default()); }
#[no_mangle]
extern "C" fn android_display_native_blt_create() -> *mut c_void {
    CALLS.with(|c| {
        let mut c = c.borrow_mut();
        c.create += 1;
        if c.unavailable {
            std::ptr::null_mut()
        } else {
            1usize as *mut c_void
        }
    })
}
#[no_mangle]
extern "C" fn android_display_native_blt_destroy(_: *mut c_void) -> i32 {
    CALLS.with(|c| {
        c.borrow_mut().close += 1;
        0
    })
}
#[no_mangle]
unsafe extern "C" fn android_display_native_blt_copy_wait(
    _: *mut c_void,
    s: *mut c_void,
    d: *mut c_void,
    r: *const NativeBltRegion,
    n: u32,
) -> i32 {
    assert_eq!((s as usize, d as usize), (17, 29));
    assert_eq!(n, 1);
    assert_eq!((*r).source_x, 2);
    CALLS.with(|c| {
        let mut c = c.borrow_mut();
        c.copy += 1;
        c.status
    })
}
unsafe fn transfer(upload: bool, r: *const NativeBltRegion, n: u32, out: *mut u64) -> i32 {
    CALLS.with(|c| {
        let mut c = c.borrow_mut();
        if upload {
            c.upload += 1
        } else {
            c.readback += 1
        };
        *out = if c.status != 0 {
            0
        } else {
            std::slice::from_raw_parts(r, n as usize)
                .iter()
                .map(|r| u64::from(r.width) * u64::from(r.height) * 4)
                .sum::<u64>()
                - u64::from(c.short)
        };
        c.status
    })
}
#[no_mangle]
unsafe extern "C" fn android_display_native_blt_upload(
    _: *mut c_void,
    _: *mut c_void,
    _: *const u8,
    l: *const NativeBltCpuLayout,
    r: *const NativeBltRegion,
    n: u32,
    out: *mut u64,
) -> i32 {
    assert_eq!((*l).stride, 80);
    transfer(true, r, n, out)
}
#[no_mangle]
unsafe extern "C" fn android_display_native_blt_readback(
    _: *mut c_void,
    _: *mut c_void,
    _: *mut u8,
    l: *const NativeBltCpuLayout,
    r: *const NativeBltRegion,
    n: u32,
    out: *mut u64,
) -> i32 {
    assert_eq!((*l).fourcc, u32::from_le_bytes(*b"AR24"));
    transfer(false, r, n, out)
}
struct Allocation(u64);
impl HostSharedAllocation for Allocation {
    fn description(&self) -> SharedAllocationDescription {
        SharedAllocationDescription {
            width: 16,
            height: 12,
            buffer_id: self.0,
        }
    }
    fn native_handle(&self) -> *mut c_void {
        self.0 as usize as *mut c_void
    }
}
fn region() -> NativeBltRegion {
    NativeBltRegion {
        source_x: 2,
        source_y: 3,
        destination_x: 4,
        destination_y: 5,
        width: 7,
        height: 6,
    }
}
fn layout() -> NativeBltCpuLayout {
    NativeBltCpuLayout {
        width: 16,
        height: 12,
        stride: 80,
        fourcc: u32::from_le_bytes(*b"AR24"),
        size: 960,
    }
}
#[test]
fn abi_sizes() {
    assert_eq!(std::mem::size_of::<NativeBltRegion>(), 24);
    assert_eq!(std::mem::size_of::<NativeBltCpuLayout>(), 24);
}
#[test]
fn late_invalid_and_alias_do_not_create_context() {
    let mut e = NativeBltEngine::default();
    let mut bad = region();
    bad.width = u32::MAX;
    assert!(e
        .copy(&Allocation(17), &Allocation(29), &[region(), bad])
        .is_err());
    assert!(e
        .copy(&Allocation(17), &Allocation(17), &[region()])
        .is_err());
    CALLS.with(|c| assert_eq!(c.borrow().create, 0));
}
#[test]
fn copy_routes_real_handles_and_reuses_context() {
    let mut e = NativeBltEngine::default();
    for _ in 0..2 {
        assert_eq!(
            e.copy(&Allocation(17), &Allocation(29), &[region()])
                .unwrap(),
            168
        );
    }
    drop(e);
    CALLS.with(|c| {
        let c = c.borrow();
        assert_eq!((c.create, c.copy, c.close), (1, 2, 1));
    });
}
#[test]
fn copy_failure_prevents_retry_and_transfer() {
    let mut e = NativeBltEngine::default();
    CALLS.with(|c| c.borrow_mut().status = -5);
    assert!(e
        .copy(&Allocation(17), &Allocation(29), &[region()])
        .is_err());
    CALLS.with(|c| c.borrow_mut().status = 0);
    assert!(e
        .copy(&Allocation(17), &Allocation(29), &[region()])
        .is_err());
    assert!(e
        .upload(&Allocation(29), &vec![0; 960], layout(), &[region()])
        .is_err());
    CALLS.with(|c| assert_eq!((c.borrow().copy, c.borrow().upload), (1, 0)));
}
#[test]
fn malformed_layout_never_reaches_ffi() {
    let mut e = NativeBltEngine::default();
    for field in 0..6 {
        let mut l = layout();
        match field {
            0 => l.width = 8193,
            1 => l.stride = 63,
            2 => l.fourcc = 0,
            3 => l.size = 0,
            4 => l.size = 961,
            _ => l.height = 0,
        };
        assert!(e
            .upload(&Allocation(29), &vec![0; 960], l, &[region()])
            .is_err());
    }
    CALLS.with(|c| assert_eq!(c.borrow().create, 0));
}
#[test]
fn complete_transfer_byte_counts_and_pitch() {
    let mut e = NativeBltEngine::default();
    let mut bytes = vec![0; 960];
    assert_eq!(
        e.upload(&Allocation(29), &bytes, layout(), &[region()])
            .unwrap(),
        168
    );
    assert_eq!(
        e.readback(&Allocation(17), &mut bytes, layout(), &[region()])
            .unwrap(),
        168
    );
    CALLS.with(|c| assert_eq!((c.borrow().upload, c.borrow().readback), (1, 1)));
}
#[test]
fn short_success_is_poisoned() {
    let mut e = NativeBltEngine::default();
    CALLS.with(|c| c.borrow_mut().short = true);
    assert!(e
        .readback(&Allocation(17), &mut vec![0; 960], layout(), &[region()])
        .is_err());
    CALLS.with(|c| c.borrow_mut().short = false);
    assert!(e
        .readback(&Allocation(17), &mut vec![0; 960], layout(), &[region()])
        .is_err());
    CALLS.with(|c| assert_eq!(c.borrow().readback, 1));
}
#[test]
fn bounded_packed_staging() {
    let r = NativeBltRegion {
        source_x: 0,
        source_y: 0,
        destination_x: 0,
        destination_y: 0,
        width: 8192,
        height: 8192,
    };
    assert_eq!(
        native_blt_bytes(8192, 8192, 8192, 8192, &[r]),
        Some(268435456)
    );
    assert!(native_blt_bytes(8192, 8192, 8192, 8192, &vec![r; 257]).is_none());
}
