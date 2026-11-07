// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use base::Error;
use base::RawDescriptor;
use base::Result;
use super::gunyah_sys::bindings::gh_fn_ioeventfd_arg;
use super::gunyah_sys::bindings::GH_IOEVENTFD_DATAMATCH;

use crate::Datamatch;
use crate::IoEventAddress;

/// Build the same matching key for ADD_FUNCTION and REMOVE_FUNCTION. The kernel compares
/// addr, len, flags and (when DATAMATCH is set) data; removing by address alone is insufficient.
pub(crate) fn args(
    fd: RawDescriptor,
    addr: IoEventAddress,
    datamatch: Datamatch,
) -> Result<gh_fn_ioeventfd_arg> {
    let addr = match addr {
        IoEventAddress::Mmio(addr) => addr,
        IoEventAddress::Pio(_) => return Err(Error::new(libc::EINVAL)),
    };
    let (len, value) = match datamatch {
        Datamatch::AnyLength => (0, None),
        Datamatch::U8(value) => (1, value.map(u64::from)),
        Datamatch::U16(value) => (2, value.map(u64::from)),
        Datamatch::U32(value) => (4, value.map(u64::from)),
        Datamatch::U64(value) => (8, value),
    };
    Ok(gh_fn_ioeventfd_arg {
        fd,
        addr,
        len,
        datamatch: value.unwrap_or(0),
        // The generated UAPI constant is already a mask (1), not a bit number.
        flags: if value.is_some() { GH_IOEVENTFD_DATAMATCH } else { 0 },
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_kernel_matching_key_for_all_widths() {
        let cases = [
            (Datamatch::AnyLength, 0, 0, 0),
            (Datamatch::U8(None), 1, 0, 0),
            (Datamatch::U16(None), 2, 0, 0),
            (Datamatch::U32(None), 4, 0, 0),
            (Datamatch::U64(None), 8, 0, 0),
            (Datamatch::U8(Some(0)), 1, 0, 1),
            (Datamatch::U16(Some(0x1234)), 2, 0x1234, 1),
            (Datamatch::U32(Some(0x89abcdef)), 4, 0x89abcdef, 1),
            (Datamatch::U64(Some(u64::MAX)), 8, u64::MAX, 1),
        ];
        for (datamatch, len, value, flags) in cases {
            let add = args(7, IoEventAddress::Mmio(0x12340000), datamatch).unwrap();
            let remove = args(9, IoEventAddress::Mmio(0x12340000), datamatch).unwrap();
            assert_eq!((add.addr, add.len, add.datamatch, add.flags),
                       (0x12340000, len, value, flags));
            // Mirrors gunyah_ioevent_compare(): FD is not part of the matching key.
            assert_eq!((remove.addr, remove.len, remove.datamatch, remove.flags),
                       (add.addr, add.len, add.datamatch, add.flags));
            assert_eq!(add.fd, 7);
            assert_eq!(remove.fd, 9);
        }
    }

    #[test]
    fn rejects_port_io_without_panicking() {
        assert_eq!(args(7, IoEventAddress::Pio(0x80), Datamatch::AnyLength)
            .unwrap_err().errno(), libc::EINVAL);
    }
}
