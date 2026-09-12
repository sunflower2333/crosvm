// Compile the exact production contracts without the VM/device dependency
// graph for bounded host-side failure/identity regression tests.
#[path = "../src/shared_allocation.rs"]
mod shared_allocation;
#[path = "../src/native_allocation.rs"]
mod native_allocation;

#[test]
fn actual_ledgers_keep_failed_mapping_alias_and_unmap_ownership_together() {
    use native_allocation::NativeAllocationMapLedger;
    use shared_allocation::{DynamicMappingWindow, ExternalMappingLedger};
    let window = DynamicMappingWindow { bar_size: 128 << 20, reserved_prefix: 8 << 20, alignment: 16384 };
    let mut native = NativeAllocationMapLedger::new(3);
    let mut mapper = ExternalMappingLedger::default();
    let a = native.reserve(17, 81, 91).unwrap();
    let b = native.reserve(17, 82, 92).unwrap();
    assert!(native.imported(a, 32768));
    assert!(native.imported(b, 16384));
    assert!(native.begin_mapping(a, 17, window, 8 << 20));
    let receipt = mapper.reserve(window, 8 << 20, 32768).unwrap();
    mapper.quarantine(receipt); // SHARE reply lost: actual ledger never reports live.
    assert!(native.mapping_failed(a, Some(receipt)));
    assert!(native.begin_mapping(b, 17, window, (8 << 20) + 16384));
    assert!(mapper.reserve(window, (8 << 20) + 16384, 16384).is_none());
    assert!(native.mapping_failed(b, None));
    assert!(!native.acknowledge_mapping(a, 17, receipt, mapper.is_live(receipt)));
    assert!(!native.retire_unmapped(a));
    assert_eq!(native.begin_unmap(a), Some(receipt));
    assert!(mapper.begin_remove(receipt));
    assert!(!native.can_retire_unmapped(a)); // Actual unshare still failed.
    assert!(mapper.reserve(window, 8 << 20, 16384).is_none());
    assert!(mapper.complete_remove(receipt)); // Later actual backend success.
    assert!(native.unmapped(a, receipt));
    assert!(native.retire_unmapped(a));
    assert!(native.begin_mapping(b, 17, window, 8 << 20));
    let fresh = mapper.reserve(window, 8 << 20, 16384).unwrap();
    assert_ne!(fresh.mapping_generation, receipt.mapping_generation);
    assert!(mapper.commit(fresh));
    assert!(native.mapped(b, fresh));
    assert!(!mapper.begin_remove(receipt));
    assert!(!native.acknowledge_mapping(a, 17, receipt, mapper.is_live(receipt)));
    assert!(native.acknowledge_mapping(b, 17, fresh, mapper.is_live(fresh)));
}
