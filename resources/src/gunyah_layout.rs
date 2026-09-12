// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Checked address arithmetic shared by the Gunyah architecture and hypervisor
//! integrations.
//!
//! Gunyah describes one contiguous IPA layout to the resource manager.  The
//! PCI high-MMIO aperture is part of that layout, even though it is not a
//! guest-memory region.  Keeping the calculation here prevents the allocator
//! and the FDT producer from silently choosing different windows.

use std::cmp;

/// The first address at which a Gunyah 64-bit PCI aperture may start.
pub const GUNYAH_HIGH_MMIO_MIN_BASE: u64 = 1u64 << 32;

/// Largest BAR currently used by the native-context/gfxstream paths.
pub const GUNYAH_DEFAULT_BAR_ALIGNMENT: u64 = 4u64 << 30;

/// Space kept above the aligned BAR slot for the remaining 64-bit BARs.
pub const GUNYAH_HIGH_MMIO_SLACK: u64 = 1u64 << 29;

/// Granularity required by the Gunyah `size-max` property.
pub const GUNYAH_SIZE_MAX_GRANULARITY: u64 = 1u64 << 30;

/// Do not advertise a layout smaller than this to the resource manager.
pub const GUNYAH_SIZE_MAX_MINIMUM: u64 = 4u64 << 30;

/// The checked high-MMIO layout derived from the end of the guest-memory
/// regions that existed before dynamic PCI BAR aliases were installed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GunyahMmioLayout {
    /// End of the guest-memory layout before the platform MMIO gap.
    pub guest_memory_end: u64,
    /// Exclusive end of the platform MMIO region.
    pub platform_mmio_end: u64,
    /// Start of the high-MMIO allocator range.
    pub high_mmio_base: u64,
    /// The first address after the high-MMIO allocator range.
    pub high_mmio_top: u64,
    /// First BAR-aligned address in the high-MMIO range.
    pub aligned_bar_base: u64,
}

impl GunyahMmioLayout {
    /// Size of the high-MMIO allocator range.
    pub fn high_mmio_size(self) -> u64 {
        self.high_mmio_top - self.high_mmio_base
    }
}

/// Align `value` upwards without allowing the addition used for alignment to
/// wrap.  `alignment` must be a non-zero power of two.
pub fn checked_align_up(value: u64, alignment: u64) -> Option<u64> {
    if alignment == 0 || !alignment.is_power_of_two() {
        return None;
    }
    value
        .checked_add(alignment - 1)
        .map(|aligned| aligned & !(alignment - 1))
}

/// Compute the Gunyah high-MMIO aperture.
///
/// `guest_memory_end` must include every statically declared pool, framebuffer,
/// SWIOTLB region, and other occupied range that lies below the PCI aperture.
/// Dynamic BAR aliases are installed later and therefore must not be fed back
/// into this calculation.  When the natural platform-MMIO end is below 4 GiB,
/// the allocator starts at 4 GiB so the advertised aperture never straddles
/// the 32-bit boundary.
pub fn compute_gunyah_mmio_layout(
    guest_memory_end: u64,
    platform_mmio_size: u64,
    bar_alignment: u64,
) -> Option<GunyahMmioLayout> {
    let platform_mmio_end = guest_memory_end.checked_add(platform_mmio_size)?;
    let high_mmio_base = cmp::max(platform_mmio_end, GUNYAH_HIGH_MMIO_MIN_BASE);
    let aligned_bar_base = checked_align_up(high_mmio_base, bar_alignment)?;
    let high_mmio_top = aligned_bar_base
        .checked_add(bar_alignment)?
        .checked_add(GUNYAH_HIGH_MMIO_SLACK)?;
    if high_mmio_top <= high_mmio_base {
        return None;
    }
    Some(GunyahMmioLayout {
        guest_memory_end,
        platform_mmio_end,
        high_mmio_base,
        high_mmio_top,
        aligned_bar_base,
    })
}

/// Compute the `gunyah-vm-config/memory/size-max` value needed to cover both
/// guest memory and the high-MMIO aperture.
pub fn compute_gunyah_size_max(
    layout_base: u64,
    guest_memory_end: u64,
    high_mmio_top: u64,
) -> Option<u64> {
    let covered_top = cmp::max(guest_memory_end, high_mmio_top);
    let span = covered_top.checked_sub(layout_base)?;
    let rounded = checked_align_up(span, GUNYAH_SIZE_MAX_GRANULARITY)?;
    let size_max = cmp::max(rounded, GUNYAH_SIZE_MAX_MINIMUM);
    layout_base.checked_add(size_max)?;
    Some(size_max)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLATFORM_MMIO: u64 = 0x800000;
    const GIB: u64 = 1 << 30;
    const MIB: u64 = 1 << 20;

    #[test]
    fn low_memory_is_forced_above_32_bit_boundary() {
        let layout = compute_gunyah_mmio_layout(3 * GIB, PLATFORM_MMIO, 8 * MIB).unwrap();
        assert_eq!(layout.platform_mmio_end, 3 * GIB + PLATFORM_MMIO);
        assert_eq!(layout.high_mmio_base, 4 * GIB);
        assert_eq!(layout.aligned_bar_base, 4 * GIB);
        assert_eq!(
            layout.high_mmio_top,
            4 * GIB + 8 * MIB + GUNYAH_HIGH_MMIO_SLACK
        );
    }

    #[test]
    fn four_gib_bar_gets_a_four_gib_aligned_slot() {
        let layout = compute_gunyah_mmio_layout(4 * GIB, PLATFORM_MMIO, 4 * GIB).unwrap();
        assert_eq!(layout.high_mmio_base, 4 * GIB + PLATFORM_MMIO);
        assert_eq!(layout.aligned_bar_base, 8 * GIB);
        assert_eq!(layout.high_mmio_top, 12 * GIB + GUNYAH_HIGH_MMIO_SLACK);
    }

    #[test]
    fn extra_pool_and_framebuffer_space_is_part_of_the_input_end() {
        let base_end = 2 * GIB;
        let occupied_end = base_end + 256 * MIB + 16 * MIB;
        let layout = compute_gunyah_mmio_layout(occupied_end, PLATFORM_MMIO, 4 * GIB).unwrap();
        assert_eq!(layout.guest_memory_end, occupied_end);
        assert!(layout.high_mmio_base >= occupied_end + PLATFORM_MMIO);
    }

    #[test]
    fn size_max_covers_window_and_rounds_to_gib() {
        let layout = compute_gunyah_mmio_layout(3 * GIB, PLATFORM_MMIO, 8 * MIB).unwrap();
        let size_max =
            compute_gunyah_size_max(2 * GIB, layout.guest_memory_end, layout.high_mmio_top)
                .unwrap();
        assert_eq!(size_max % GUNYAH_SIZE_MAX_GRANULARITY, 0);
        assert!(2 * GIB + size_max >= layout.high_mmio_top);
        assert!(size_max >= GUNYAH_SIZE_MAX_MINIMUM);
    }

    #[test]
    fn arithmetic_overflow_is_rejected() {
        assert!(compute_gunyah_mmio_layout(u64::MAX, PLATFORM_MMIO, 8 * MIB).is_none());
        assert!(compute_gunyah_mmio_layout(3 * GIB, PLATFORM_MMIO, 3 * MIB).is_none());
        assert!(compute_gunyah_size_max(u64::MAX - 1, u64::MAX, u64::MAX).is_none());
        assert!(checked_align_up(u64::MAX, 2).is_none());
    }

    #[test]
    fn pstore_reservation_does_not_move_the_aperture_back_below_four_gib() {
        let layout = compute_gunyah_mmio_layout(3 * GIB, PLATFORM_MMIO, 4 * MIB).unwrap();
        let pstore_end = layout.high_mmio_base + 2 * MIB;
        assert!(pstore_end <= layout.high_mmio_top);
        assert!(layout.high_mmio_base >= GUNYAH_HIGH_MMIO_MIN_BASE);
    }
}
