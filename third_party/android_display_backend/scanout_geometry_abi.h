// Copyright 2026 The Android Open Source Project
// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <stdbool.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

struct AndroidDisplayContext;

// Host-local C ABI, not a guest pointer or a capability advertisement.
// content_rotation is the producer's clockwise quarter turns (0..3).
// Dimensions describe the actual allocation and the unchanged logical parent.
// Both generations are nonzero and monotonically increase within their scope:
// mode within reset, reset within this display-context lifetime. Equal IDs are
// immutable. A new reset may restart mode numbering, but cannot reuse old IDs.
struct AndroidDisplayScanoutGeometry {
    uint32_t version, size;
    uint32_t storage_width, storage_height;
    uint32_t logical_width, logical_height;
    uint32_t content_rotation, reserved;
    uint64_t reset_generation, mode_generation;
    uint64_t reserved_tail[2];
};

// A current host preference, not guest producer readiness or HWC proof.
// Unavailable profiles clear flags/extents/rotation; their epochs can persist.
struct AndroidDisplayScanoutProfile {
    uint32_t version, size, flags, content_rotation;
    uint32_t storage_width, storage_height, logical_width, logical_height;
    uint64_t endpoint_generation, profile_generation;
    uint64_t reserved_tail[2];
};

bool android_display_query_scanout_profile(struct AndroidDisplayContext* ctx,
        void* surface, struct AndroidDisplayScanoutProfile* profile);
// Internal VMM transition after real reader/renderer drain, not a guest command.
// Clears committed geometry, preserves endpoint/profile and native-only policy.
bool android_display_reset_scanout_geometry(struct AndroidDisplayContext* ctx,
        uint64_t old_reset, uint64_t new_reset);
bool android_display_configure_scanout_profile(struct AndroidDisplayContext* ctx,
        void* surface, const struct AndroidDisplayScanoutGeometry* geometry,
        uint64_t endpoint_generation, uint64_t profile_generation);

// An activated profile remains native-only for this display-context lifetime.
// Rollback uses a newer identity tuple; legacy calls cannot clear the profile.
// The surface handle is opaque: its C++ implementation has internal linkage.
bool android_display_configure_scanout_geometry(struct AndroidDisplayContext* ctx,
        void* surface,
        const struct AndroidDisplayScanoutGeometry* geometry);

// attachment_generation is mandatory for refresh, zero for ordinary presents.
// Mode/reset IDs belong to the resource binding, not the latest display mode.
bool android_display_present_shared_ahb_geometry(struct AndroidDisplayContext* ctx,
        void* surface, uint64_t holder, int acquire_fence_fd,
        int* out_release_fence_fd, uint64_t attachment_generation,
        uint64_t mode_generation, uint64_t reset_generation);

#ifdef __cplusplus
}
static_assert(sizeof(AndroidDisplayScanoutGeometry) == 64);
static_assert(sizeof(AndroidDisplayScanoutProfile) == 64);
#else
_Static_assert(sizeof(struct AndroidDisplayScanoutGeometry) == 64, "scanout geometry ABI");
_Static_assert(sizeof(struct AndroidDisplayScanoutProfile) == 64, "scanout profile ABI");
#endif
