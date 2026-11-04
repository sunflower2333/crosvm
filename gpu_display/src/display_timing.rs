// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license.

/// Matched 80-byte Android C ABI. This is transaction presentation timing,
/// not proof of a guest buffer's scanout. Unknown period/binding stay explicit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct DisplayTimingSample {
    pub version: u32,
    pub size: u32,
    pub flags: u32,
    pub reserved: u32,
    pub attachment_epoch: u64,
    pub mode_epoch: u64,
    pub sequence: u64,
    pub host_now_ns: u64,
    pub present_ns: u64,
    pub latch_ns: u64,
    pub period_ns: u64,
    pub uncertainty_ns: u64,
}

impl DisplayTimingSample {
    pub fn valid(&self) -> bool {
        self.version == 1 && self.size == 80 && self.reserved == 0 && self.flags & !7 == 0
            && self.attachment_epoch != 0 && self.mode_epoch != 0 && self.host_now_ns != 0
            && (self.flags & 1 == 0 || (self.sequence != 0 && self.latch_ns != 0
                && self.present_ns >= self.latch_ns && self.present_ns <= self.host_now_ns
                && self.host_now_ns - self.present_ns <= 250_000_000))
            && (self.flags & 4 == 0 || (self.flags & 2 != 0
                && (1_000_000..=100_000_000).contains(&self.period_ns)))
            && (self.flags & 4 != 0 || self.period_ns == 0)
    }
}

const _: () = assert!(std::mem::size_of::<DisplayTimingSample>() == 80);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actual_present_does_not_invent_period_or_binding() {
        let valid = DisplayTimingSample { version: 1, size: 80, flags: 1,
            attachment_epoch: 9, mode_epoch: 2, sequence: 4, host_now_ns: 900,
            present_ns: 850, latch_ns: 800, uncertainty_ns: u64::MAX, ..Default::default() };
        assert!(valid.valid());
        for bad in [DisplayTimingSample { version: 0, ..valid },
            DisplayTimingSample { size: 79, ..valid },
            DisplayTimingSample { flags: 9, ..valid },
            DisplayTimingSample { present_ns: 799, ..valid },
            DisplayTimingSample { present_ns: 901, ..valid },
            DisplayTimingSample { host_now_ns: 250_000_851, ..valid },
            DisplayTimingSample { period_ns: 6_060_606, ..valid },
            DisplayTimingSample { flags: 5, period_ns: 6_060_606, ..valid }] {
            assert!(!bad.valid());
        }
        assert!(DisplayTimingSample { flags: 7, period_ns: 6_060_606, ..valid }.valid());
    }
}
