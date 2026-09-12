// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license.

//! Range and identity contracts for independently shared host allocations.
//! No fd mapping into a pre-backed region is an external-memory acceptance.
use std::collections::BTreeMap;

pub const UNAVAILABLE_MAPPER: u64 = 1 << 0;
pub const UNAVAILABLE_NO_SUFFIX: u64 = 1 << 1;
pub const UNAVAILABLE_TRIPLE_CAPACITY: u64 = 1 << 2;
pub const UNAVAILABLE_NATIVE_SURFACE: u64 = 1 << 3;
pub const UNAVAILABLE_ALLOCATOR: u64 = 1 << 4;
pub const UNAVAILABLE_RENDERER_IMPORT: u64 = 1 << 5;
pub const UNAVAILABLE_PRODUCER_BRIDGE: u64 = 1 << 6;
pub const UNAVAILABLE_CONSUMER_BRIDGE: u64 = 1 << 7;
pub const UNAVAILABLE_GUEST_IDENTITY: u64 = 1 << 8;

/// Independent mapping/allocator primitives exist, but no direct PRESENT
/// command may be admitted until the entire producer/consumer contract lands.
pub fn discovery_unavailable(window: Option<DynamicMappingWindow>, generation: u64,
    width: u32, height: u32) -> (u64, u64) {
    let mut reasons = UNAVAILABLE_ALLOCATOR | UNAVAILABLE_RENDERER_IMPORT
        | UNAVAILABLE_PRODUCER_BRIDGE | UNAVAILABLE_CONSUMER_BRIDGE | UNAVAILABLE_GUEST_IDENTITY;
    if generation == 0 { reasons |= UNAVAILABLE_NATIVE_SURFACE; }
    let Some(window) = window else { return (reasons | UNAVAILABLE_MAPPER, 0); };
    if window.available_bytes() == 0 { reasons |= UNAVAILABLE_NO_SUFFIX; }
    let minimum = (width != 0 && height != 0 && window.alignment.is_power_of_two())
        .then(|| u64::from(width).checked_mul(u64::from(height)))
        .flatten().and_then(|pixels| pixels.checked_mul(4))
        .and_then(|bytes| bytes.checked_add(window.alignment - 1))
        .map(|bytes| bytes & !(window.alignment - 1))
        .and_then(|bytes| bytes.checked_mul(3));
    if minimum.map_or(true, |bytes| bytes > window.available_bytes()) {
        reasons |= UNAVAILABLE_TRIPLE_CAPACITY;
    }
    (reasons, minimum.unwrap_or(0))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DynamicMappingWindow {
    pub bar_size: u64,
    /// All bytes before this offset are reserved, including an eager BAR guard.
    pub reserved_prefix: u64,
    pub alignment: u64,
}

impl DynamicMappingWindow {
    pub fn available_bytes(self) -> u64 {
        self.bar_size.saturating_sub(self.reserved_prefix)
    }

    pub fn permits(self, offset: u64, size: u64) -> bool {
        self.alignment.is_power_of_two()
            && size != 0
            && offset % self.alignment == 0
            && size % self.alignment == 0
            && offset >= self.reserved_prefix
            && offset
                .checked_add(size)
                .is_some_and(|end| end <= self.bar_size)
    }
}

/// Mapping identity is monotonic for the mapper's lifetime, not a Surface
/// generation, resource id, BAR offset or kernel descriptor number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExternalMappingReceipt {
    pub offset: u64,
    pub size: u64,
    pub mapping_generation: u64,
}

/// Attached to an error only after a real backend registration was attempted.
/// The range and retained descriptor must remain quarantined even if the IPC
/// reply was lost. Callers can distinguish this from a preflight rejection.
#[derive(Clone, Copy, Debug)]
pub struct ExternalMappingFailure {
    pub receipt: ExternalMappingReceipt,
}

impl std::fmt::Display for ExternalMappingFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "uncertain external SHARE/ACCEPT at {:#x}, size {:#x}, generation {}",
            self.receipt.offset, self.receipt.size, self.receipt.mapping_generation)
    }
}

impl std::error::Error for ExternalMappingFailure {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Pending,
    Live,
    Retiring,
    Quarantined,
}

pub fn ranges_overlap(a: u64, a_size: u64, b: u64, b_size: u64) -> bool {
    match (a.checked_add(a_size), b.checked_add(b_size)) {
        (Some(a_end), Some(b_end)) => a < b_end && b < a_end,
        _ => true,
    }
}

pub fn after_reserved_prefix(start: u64, size: u64, destination: u64) -> bool {
    start
        .checked_add(size)
        .is_some_and(|end| destination >= end)
}

#[derive(Default)]
pub struct ExternalMappingLedger {
    last_generation: u64,
    entries: BTreeMap<u64, (ExternalMappingReceipt, State)>,
}

impl ExternalMappingLedger {
    pub fn overlaps(&self, offset: u64, size: u64) -> bool {
        self.entries
            .values()
            .any(|(r, _)| ranges_overlap(offset, size, r.offset, r.size))
    }

    pub fn reserve(
        &mut self,
        window: DynamicMappingWindow,
        offset: u64,
        size: u64,
    ) -> Option<ExternalMappingReceipt> {
        if !window.permits(offset, size) || self.overlaps(offset, size) {
            return None;
        }
        let generation = self.last_generation.checked_add(1)?;
        let receipt = ExternalMappingReceipt {
            offset,
            size,
            mapping_generation: generation,
        };
        self.entries.insert(offset, (receipt, State::Pending));
        self.last_generation = generation;
        Some(receipt)
    }

    pub fn commit(&mut self, receipt: ExternalMappingReceipt) -> bool {
        match self.entries.get_mut(&receipt.offset) {
            Some((stored, state)) if *stored == receipt && *state == State::Pending => {
                *state = State::Live;
                true
            }
            _ => false,
        }
    }

    /// A failed IPC may have lost its reply after SHARE succeeded. Keep the
    /// range unavailable; a timeout is never proof of no mapping.
    pub fn quarantine(&mut self, receipt: ExternalMappingReceipt) {
        if let Some((stored, state)) = self.entries.get_mut(&receipt.offset) {
            if *stored == receipt {
                *state = State::Quarantined;
            }
        }
    }

    pub fn is_live(&self, receipt: ExternalMappingReceipt) -> bool {
        self.entries.get(&receipt.offset) == Some(&(receipt, State::Live))
    }

    /// Revoke mapping visibility before release. A failed guest RELEASE or
    /// host unshare (including EUCLEAN) never restores a writable identity.
    /// The same receipt remains valid only for cleanup retries.
    pub fn begin_remove(&mut self, receipt: ExternalMappingReceipt) -> bool {
        match self.entries.get_mut(&receipt.offset) {
            Some((stored, state))
                if *stored == receipt && matches!(*state,
                    State::Live | State::Retiring | State::Quarantined) =>
            {
                *state = State::Retiring;
                true
            }
            _ => false,
        }
    }

    /// Called only after the backend confirms guest release AND unshare.
    /// Failed unmap must leave the entry untouched so the same receipt retries.
    pub fn complete_remove(&mut self, receipt: ExternalMappingReceipt) -> bool {
        if self.entries.get(&receipt.offset) != Some(&(receipt, State::Retiring)) {
            return false;
        }
        self.entries.remove(&receipt.offset);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const MIB: u64 = 1 << 20;
    fn window() -> DynamicMappingWindow {
        DynamicMappingWindow {
            bar_size: 64 * MIB,
            reserved_prefix: 8 * MIB,
            alignment: 16384,
        }
    }
    #[test]
    fn discovery_explains_current_capacity_without_advertising_unimplemented_path() {
        let current = DynamicMappingWindow { bar_size: 8*MIB, reserved_prefix: 8*MIB, alignment: 4096 };
        let (why, minimum) = discovery_unavailable(Some(current), 7, 1920, 1080);
        assert_ne!(why & UNAVAILABLE_NO_SUFFIX, 0);
        assert_ne!(why & UNAVAILABLE_TRIPLE_CAPACITY, 0);
        assert_eq!(minimum, 24_883_200);
        let (large, _) = discovery_unavailable(Some(window()), 7, 1920, 1080);
        assert_eq!(large & (UNAVAILABLE_NO_SUFFIX | UNAVAILABLE_TRIPLE_CAPACITY), 0);
        assert_ne!(large & UNAVAILABLE_GUEST_IDENTITY, 0);
        assert_ne!(large & UNAVAILABLE_PRODUCER_BRIDGE, 0);
        assert_ne!(large & UNAVAILABLE_CONSUMER_BRIDGE, 0);
        assert_ne!(discovery_unavailable(None, 0, 0, 0).0 & UNAVAILABLE_MAPPER, 0);
        assert_ne!(discovery_unavailable(None, 0, 0, 0).0 & UNAVAILABLE_NATIVE_SURFACE, 0);
        assert_eq!(discovery_unavailable(Some(window()), 7, u32::MAX, u32::MAX).1, 0);
    }
    #[test]
    fn current_eight_mib_has_no_suffix() {
        let w = DynamicMappingWindow {
            bar_size: 8 * MIB,
            reserved_prefix: 8 * MIB,
            alignment: 4096,
        };
        assert_eq!(w.available_bytes(), 0);
        assert!(!w.permits(2 * MIB, 4096));
        assert!(!w.permits(8 * MIB, 4096));
        assert!(!w.permits(8 * MIB, 0));
    }
    #[test]
    fn exact_boundaries_alignment_and_overflow() {
        let w = window();
        assert!(w.permits(8 * MIB, 56 * MIB));
        assert!(w.permits(64 * MIB - 16384, 16384));
        for (offset, size) in [
            (8 * MIB - 16384, 16384),
            (64 * MIB, 16384),
            (8 * MIB, 0),
            (8 * MIB + 1, 16384),
            (8 * MIB, 4096),
            (u64::MAX - 16383, 16384),
            (8 * MIB, u64::MAX),
        ] {
            assert!(!w.permits(offset, size));
        }
        assert!(!DynamicMappingWindow { alignment: 0, ..w }.permits(8 * MIB, MIB));
        assert!(!DynamicMappingWindow { alignment: 3, ..w }.permits(8 * MIB, MIB));
        assert!(!after_reserved_prefix(u64::MAX - 4095, 4096, u64::MAX));
        assert!(after_reserved_prefix(2 * MIB, 6 * MIB, 8 * MIB));
        assert!(!after_reserved_prefix(2 * MIB, 6 * MIB, 2 * MIB));
        assert!(!after_reserved_prefix(2 * MIB, 6 * MIB, 0));
    }
    #[test]
    fn pending_live_and_quarantined_ranges_all_block_overlap() {
        let mut ledger = ExternalMappingLedger::default();
        let r = ledger.reserve(window(), 10 * MIB, 2 * MIB).unwrap();
        assert!(!ledger.is_live(r));
        for (start, size) in [
            (10 * MIB, MIB),
            (9 * MIB, 2 * MIB),
            (11 * MIB, 2 * MIB),
            (9 * MIB, 4 * MIB),
        ] {
            assert!(ledger.reserve(window(), start, size).is_none());
        }
        assert!(ledger.reserve(window(), 8 * MIB, 2 * MIB).is_some());
        assert!(ledger.reserve(window(), 12 * MIB, MIB).is_some());
        assert!(ledger.commit(r));
        assert!(!ledger.commit(r));
        ledger.quarantine(r);
        assert!(!ledger.is_live(r));
        assert!(!ledger.complete_remove(r));
        assert!(ledger.reserve(window(), 10 * MIB, 2 * MIB).is_none());
    }
    #[test]
    fn failed_unmap_retry_and_offset_reuse_have_distinct_identity() {
        let mut ledger = ExternalMappingLedger::default();
        let old = ledger.reserve(window(), 8 * MIB, MIB).unwrap();
        assert!(ledger.commit(old));
        assert!(ledger.begin_remove(old));
        // Failed backend unmap: identity is cleanup-only, even if guest ACCEPT
        // could not be restored. An ACK cannot accidentally re-enable writes.
        assert!(!ledger.is_live(old));
        assert!(ledger.begin_remove(old));
        assert!(ledger.reserve(window(), 8 * MIB, MIB).is_none());
        assert!(!ledger.complete_remove(ExternalMappingReceipt {
            size: 2 * MIB,
            ..old
        }));
        assert!(ledger.complete_remove(old));
        let fresh = ledger.reserve(window(), 8 * MIB, MIB).unwrap();
        assert!(ledger.commit(fresh));
        assert_ne!(old.mapping_generation, fresh.mapping_generation);
        assert!(!ledger.is_live(old));
        assert!(!ledger.complete_remove(old));
        assert!(!ledger.begin_remove(old));
        assert!(ledger.is_live(fresh));
    }
    #[test]
    fn generation_exhaustion_never_reuses_token() {
        let mut ledger = ExternalMappingLedger {
            last_generation: u64::MAX,
            ..Default::default()
        };
        assert!(ledger.reserve(window(), 8 * MIB, MIB).is_none());
        assert!(!ledger.overlaps(8 * MIB, MIB));
    }
}
