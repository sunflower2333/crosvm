// Copyright 2026 The Android Open Source Project
// SPDX-License-Identifier: Apache-2.0
#pragma once
#include <cstdint>

// Host-endian C ABI. No pointers or native handles appear in virtio wire data.
struct NativeBltRegion {
    uint32_t source_x, source_y, destination_x, destination_y, width, height;
};
static_assert(sizeof(NativeBltRegion) == 24);

namespace android_display {
constexpr uint32_t kNativeBltMaxRegions = 256;
inline bool nativeBltRegionsValid(uint32_t sw, uint32_t sh, uint32_t dw, uint32_t dh,
                                  const NativeBltRegion* regions, uint32_t count) {
    if (!sw || !sh || !dw || !dh || sw > 8192 || sh > 8192 || dw > 8192 || dh > 8192 ||
            !regions || !count || count > kNativeBltMaxRegions) return false;
    for (uint32_t i = 0; i < count; ++i) {
        const auto& r = regions[i];
        // Subtraction after validating origins avoids integer wrap, including
        // an invalid last region after otherwise valid prefixes.
        if (!r.width || !r.height || r.source_x >= sw || r.source_y >= sh ||
                r.destination_x >= dw || r.destination_y >= dh ||
                r.width > sw-r.source_x || r.height > sh-r.source_y ||
                r.width > dw-r.destination_x || r.height > dh-r.destination_y) return false;
    }
    return true;
}
inline bool nativeBltRegionsOverlap(const NativeBltRegion& a, const NativeBltRegion& b) {
    return uint64_t(a.destination_x) < uint64_t(b.destination_x)+b.width &&
           uint64_t(b.destination_x) < uint64_t(a.destination_x)+a.width &&
           uint64_t(a.destination_y) < uint64_t(b.destination_y)+b.height &&
           uint64_t(b.destination_y) < uint64_t(a.destination_y)+a.height;
}
} // namespace android_display

extern "C" {
// Caller serializes this context and holds exclusive source/destination access
// after GPU producers and Android consumers have released both allocations.
void* android_display_native_blt_create();
// Returns -EBUSY with the entire context retained if GPU completion is still
// uncertain. Caller must retry or quarantine it through reset, never free it.
int android_display_native_blt_destroy(void* context);
// Holds imports until completion. Returns 0 and an owned SYNC_FD on success
// (-1 only after proven synchronous completion), negative errno otherwise.
// A submission error poisons the context: an error
// never grants reuse of potentially accessed allocations. No CPU-copy fallback.
int android_display_native_blt_copy(void* context, void* source, void* destination,
                                    const NativeBltRegion* regions, uint32_t count,
                                    int* completion_fd);
// Bounded synchronous variant for the serialized virtio command worker.
// Success proves the actual Vulkan fence completed; errors require quarantine.
int android_display_native_blt_copy_wait(void* context, void* source, void* destination,
                                        const NativeBltRegion* regions, uint32_t count);
}
