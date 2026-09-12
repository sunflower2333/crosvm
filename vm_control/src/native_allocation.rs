// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license.

//! Native allocation identity across real descriptor mapping/acceptance.
//! This is host bookkeeping, not a substitute for owning the AHB, renderer
//! object, descriptor and real mapper receipt. The caller retains all those
//! owners until it confirms unmap AND renderer retirement, then removes the
//! identity. No operation in this module grants a render or present lease.
use std::collections::BTreeMap;
use crate::shared_allocation::{DynamicMappingWindow, ExternalMappingReceipt};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeAllocationIdentity {
    pub surface_generation: u64,
    pub token: u64,
    pub context_id: u32,
    pub resource_id: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Reserved,
    Imported,
    Mapping(u64),
    Mapped(ExternalMappingReceipt),
    Acknowledged(ExternalMappingReceipt),
    Unmapping(ExternalMappingReceipt),
    UnmappedTerminal,
    Quarantined(Option<ExternalMappingReceipt>),
}

struct Entry {
    identity: NativeAllocationIdentity,
    allocation_size: u64,
    phase: Phase,
}

pub struct NativeAllocationMapLedger {
    last_token: u64,
    maximum_allocations: usize,
    entries: BTreeMap<u32, Entry>,
}

impl NativeAllocationMapLedger {
    pub fn contains_resource(&self, resource_id: u32) -> bool {
        self.entries.contains_key(&resource_id)
    }
    pub fn new(maximum_allocations: usize) -> Self {
        Self { last_token: 0, maximum_allocations, entries: BTreeMap::new() }
    }

    /// Reserve before creating/importing a native object. Tokens are burned
    /// even if creation fails; resource IDs are released only after cleanup.
    pub fn reserve(&mut self, surface_generation: u64, context_id: u32, resource_id: u32)
        -> Option<NativeAllocationIdentity> {
        if surface_generation == 0 || context_id == 0 || resource_id == 0 ||
            self.entries.len() >= self.maximum_allocations || self.entries.contains_key(&resource_id) {
            return None;
        }
        let token = self.last_token.checked_add(1)?;
        let identity = NativeAllocationIdentity { surface_generation, token, context_id, resource_id };
        self.entries.insert(resource_id, Entry { identity, allocation_size: 0, phase: Phase::Reserved });
        self.last_token = token;
        Some(identity)
    }

    fn entry(&self, identity: NativeAllocationIdentity) -> Option<&Entry> {
        self.entries.get(&identity.resource_id).filter(|entry| entry.identity == identity)
    }

    fn entry_mut(&mut self, identity: NativeAllocationIdentity) -> Option<&mut Entry> {
        self.entries.get_mut(&identity.resource_id).filter(|entry| entry.identity == identity)
    }

    /// Called only after authentic allocation metadata and checked native
    /// GPU import agree on exact bytes. Failed imports stay reserved for cleanup.
    pub fn imported(&mut self, identity: NativeAllocationIdentity, allocation_size: u64) -> bool {
        let Some(entry) = self.entry_mut(identity) else { return false; };
        if allocation_size == 0 || entry.phase != Phase::Reserved { return false; }
        entry.allocation_size = allocation_size;
        entry.phase = Phase::Imported;
        true
    }

    pub fn begin_mapping(&mut self, identity: NativeAllocationIdentity, current_generation: u64,
        window: DynamicMappingWindow, offset: u64) -> bool {
        if identity.surface_generation != current_generation { return false; }
        let Some(entry) = self.entry_mut(identity) else { return false; };
        if entry.phase != Phase::Imported || !window.permits(offset, entry.allocation_size) {
            return false;
        }
        entry.phase = Phase::Mapping(offset);
        true
    }

    /// A synchronous mapper receipt follows actual backend SHARE/ACCEPT. It is
    /// not manufactured from the request or from a guest acknowledgement.
    pub fn mapped(&mut self, identity: NativeAllocationIdentity, receipt: ExternalMappingReceipt) -> bool {
        let Some(entry) = self.entry_mut(identity) else { return false; };
        if entry.phase != Phase::Mapping(receipt.offset) || receipt.mapping_generation == 0 ||
            receipt.size != entry.allocation_size { return false; }
        entry.phase = Phase::Mapped(receipt);
        true
    }

    /// Only preflight rejection is safely retryable. A typed uncertain backend
    /// failure retains the exact receipt and keeps allocation/resource quota.
    pub fn mapping_failed(&mut self, identity: NativeAllocationIdentity,
        uncertain: Option<ExternalMappingReceipt>) -> bool {
        let Some(entry) = self.entry_mut(identity) else { return false; };
        let Phase::Mapping(offset) = entry.phase else { return false; };
        entry.phase = match uncertain {
            None => Phase::Imported,
            Some(receipt) if receipt.offset == offset && receipt.size == entry.allocation_size &&
                receipt.mapping_generation != 0 => Phase::Quarantined(Some(receipt)),
            // A mismatching backend identity can never authorize cleanup of
            // someone else's mapping. Retain ownership without a usable receipt.
            Some(_) => Phase::Quarantined(None),
        };
        true
    }

    pub fn mapping_receipt(&self, identity: NativeAllocationIdentity) -> Option<ExternalMappingReceipt> {
        match self.entry(identity)?.phase {
            Phase::Mapped(receipt) | Phase::Acknowledged(receipt) |
            Phase::Unmapping(receipt) | Phase::Quarantined(Some(receipt)) => Some(receipt),
            _ => None,
        }
    }

    /// `mapper_live` must be the actual mapper's external_mapping_live(receipt)
    /// result. Matching guest fields alone never create or validate a mapping.
    pub fn acknowledge_mapping(&mut self, identity: NativeAllocationIdentity,
        current_generation: u64, receipt: ExternalMappingReceipt, mapper_live: bool) -> bool {
        if identity.surface_generation != current_generation { return false; }
        let Some(entry) = self.entry_mut(identity) else { return false; };
        if !matches!(entry.phase, Phase::Mapped(stored) | Phase::Acknowledged(stored)
            if stored == receipt) { return false; }
        if !mapper_live {
            entry.phase = Phase::Quarantined(Some(receipt));
            return false;
        }
        entry.phase = Phase::Acknowledged(receipt);
        true
    }

    /// Revoke ACK eligibility before the backend attempts RELEASE/unshare.
    /// Failure leaves cleanup-only state, never a restored writable identity.
    pub fn begin_unmap(&mut self, identity: NativeAllocationIdentity) -> Option<ExternalMappingReceipt> {
        let entry = self.entry_mut(identity)?;
        let receipt = match entry.phase {
            Phase::Mapped(receipt) | Phase::Acknowledged(receipt) |
            Phase::Unmapping(receipt) | Phase::Quarantined(Some(receipt)) => receipt,
            _ => return None,
        };
        entry.phase = Phase::Unmapping(receipt);
        Some(receipt)
    }

    pub fn unmapped(&mut self, identity: NativeAllocationIdentity, receipt: ExternalMappingReceipt) -> bool {
        let Some(entry) = self.entry_mut(identity) else { return false; };
        if entry.phase != Phase::Unmapping(receipt) { return false; }
        // v1 ACK has no guest-visible mapping nonce. Remapping the same token
        // at the same offset would let an old ACK match the new mapping, so
        // successful UNMAP is terminal: destroy/reallocate obtains a fresh token.
        entry.phase = Phase::UnmappedTerminal;
        true
    }

    /// The caller must first confirm renderer cleanup. This merely permits
    /// removing its host identity once no possible external mapping remains.
    pub fn retire_unmapped(&mut self, identity: NativeAllocationIdentity) -> bool {
        if !self.can_retire_unmapped(identity) {
            return false;
        }
        self.entries.remove(&identity.resource_id);
        true
    }

    pub fn can_retire_unmapped(&self, identity: NativeAllocationIdentity) -> bool {
        self.entry(identity).is_some_and(|entry|
            matches!(entry.phase, Phase::Reserved | Phase::Imported | Phase::UnmappedTerminal))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const OFFSET: u64 = 8 << 20;
    const SIZE: u64 = 16384;
    fn window() -> DynamicMappingWindow {
        DynamicMappingWindow { bar_size: 64 << 20, reserved_prefix: OFFSET, alignment: SIZE }
    }
    fn receipt(generation: u64) -> ExternalMappingReceipt {
        ExternalMappingReceipt { offset: OFFSET, size: SIZE, mapping_generation: generation }
    }
    fn imported(ledger: &mut NativeAllocationMapLedger) -> NativeAllocationIdentity {
        let identity = ledger.reserve(7, 81, 91).unwrap();
        assert!(ledger.imported(identity, SIZE));
        identity
    }
    #[test]
    fn failed_allocation_burns_token_and_old_identity_cannot_clean_reuse() {
        let mut ledger = NativeAllocationMapLedger::new(1);
        let failed = ledger.reserve(7, 81, 91).unwrap();
        assert!(ledger.reserve(7, 81, 92).is_none());
        assert!(ledger.retire_unmapped(failed));
        let fresh = imported(&mut ledger);
        assert_ne!(failed.token, fresh.token);
        assert!(!ledger.imported(failed, SIZE));
        assert!(!ledger.retire_unmapped(failed));
        ledger.last_token = u64::MAX;
        assert!(ledger.retire_unmapped(fresh));
        assert!(ledger.reserve(7, 81, 91).is_none());
    }
    #[test]
    fn ack_requires_completed_real_receipt_and_exact_identity() {
        let mut ledger = NativeAllocationMapLedger::new(3);
        let id = imported(&mut ledger);
        assert!(!ledger.acknowledge_mapping(id, 7, receipt(1), true));
        assert!(!ledger.begin_mapping(id, 8, window(), OFFSET));
        assert!(!ledger.begin_mapping(id, 7, window(), OFFSET-1));
        assert!(ledger.begin_mapping(id, 7, window(), OFFSET));
        assert!(!ledger.acknowledge_mapping(id, 7, receipt(1), true));
        assert!(ledger.mapped(id, receipt(1)));
        assert!(!ledger.acknowledge_mapping(id, 7, receipt(2), true));
        assert!(!ledger.acknowledge_mapping(NativeAllocationIdentity { context_id: 82, ..id }, 7, receipt(1), true));
        assert!(!ledger.acknowledge_mapping(id, 8, receipt(1), true));
        assert!(ledger.acknowledge_mapping(id, 7, receipt(1), true));
        assert!(ledger.acknowledge_mapping(id, 7, receipt(1), true));
    }
    #[test]
    fn failed_unmap_is_cleanup_only_and_success_requires_new_allocation_token() {
        let mut ledger = NativeAllocationMapLedger::new(3);
        let id = imported(&mut ledger);
        assert!(ledger.begin_mapping(id, 7, window(), OFFSET));
        assert!(ledger.mapped(id, receipt(1)));
        assert_eq!(ledger.begin_unmap(id), Some(receipt(1)));
        assert!(!ledger.acknowledge_mapping(id, 7, receipt(1), true));
        assert!(!ledger.retire_unmapped(id));
        assert_eq!(ledger.begin_unmap(id), Some(receipt(1))); // RELEASE failed; same receipt retries.
        assert!(!ledger.unmapped(id, receipt(2)));
        assert!(ledger.unmapped(id, receipt(1)));
        assert!(!ledger.begin_mapping(id, 7, window(), OFFSET));
        assert!(ledger.retire_unmapped(id));
        let fresh = imported(&mut ledger);
        assert!(ledger.begin_mapping(fresh, 7, window(), OFFSET));
        assert!(ledger.mapped(fresh, receipt(2)));
        assert!(!ledger.acknowledge_mapping(id, 7, receipt(1), true));
        assert!(ledger.acknowledge_mapping(fresh, 7, receipt(2), true));
    }
    #[test]
    fn uncertain_share_retains_quota_and_never_accepts_ack() {
        let mut ledger = NativeAllocationMapLedger::new(1);
        let id = imported(&mut ledger);
        assert!(ledger.begin_mapping(id, 7, window(), OFFSET));
        assert!(ledger.mapping_failed(id, None)); // Rejected before backend attempt.
        assert!(ledger.begin_mapping(id, 7, window(), OFFSET));
        assert!(ledger.mapping_failed(id, Some(receipt(1)))); // Lost SHARE reply.
        assert!(!ledger.retire_unmapped(id));
        assert!(ledger.reserve(7, 81, 92).is_none());
        assert!(!ledger.acknowledge_mapping(id, 7, receipt(1), true));
        assert_eq!(ledger.begin_unmap(id), Some(receipt(1)));
        assert!(!ledger.retire_unmapped(id)); // No known backend ID: cannot confirm unmap.
    }
    #[test]
    fn vanished_mapper_and_mismatched_receipts_never_authorize_cleanup() {
        let mut ledger = NativeAllocationMapLedger::new(2);
        let id = imported(&mut ledger);
        assert!(ledger.begin_mapping(id, 7, window(), OFFSET));
        assert!(ledger.mapped(id, receipt(1)));
        assert!(!ledger.acknowledge_mapping(id, 7, receipt(1), false));
        assert!(!ledger.acknowledge_mapping(id, 7, receipt(1), true));
        let other = ledger.reserve(7, 81, 92).unwrap();
        assert!(ledger.imported(other, SIZE));
        assert!(ledger.begin_mapping(other, 7, window(), OFFSET+SIZE));
        assert!(ledger.mapping_failed(other, Some(receipt(1))));
        assert!(ledger.mapping_receipt(other).is_none());
        assert!(ledger.begin_unmap(other).is_none());
        assert!(!ledger.retire_unmapped(other));
    }
}
