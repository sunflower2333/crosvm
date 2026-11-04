// Copyright 2026 The Android Open Source Project
// SPDX-License-Identifier: Apache-2.0
#pragma once
#include <cstdint>
#include <limits>

namespace android_display {
constexpr uint32_t kSharedAhbRgbaFourcc = 0x34324241; // DRM_FORMAT_ABGR8888
constexpr uint32_t kSharedAhbBgraFourcc = 0x34325241; // DRM_FORMAT_ARGB8888
constexpr uint64_t kSharedAhbMaxAllocation = 256ull * 1024 * 1024;

// Facts returned by the official mapper, not reconstructed vendor metadata.
// Signed members preserve malformed negative results until validation.
struct SharedAhbLayoutFacts {
    uint64_t buffer_id = 0, allocation_size = 0, modifier = 0;
    uint32_t width = 0, height = 0, native_stride = 0, fourcc = 0, planes = 0;
    int64_t offset = 0, stride = 0, sample_bits = 0, plane_width = 0,
            plane_height = 0, plane_size = 0, subsample_x = 0, subsample_y = 0;
};
inline bool validSharedAhbLayout(const SharedAhbLayoutFacts& f, uint32_t expectedFourcc) {
    if (!f.buffer_id || !f.width || !f.height || f.width > 8192 || f.height > 8192 ||
            (expectedFourcc != kSharedAhbRgbaFourcc && expectedFourcc != kSharedAhbBgraFourcc) ||
            f.fourcc != expectedFourcc || f.modifier != 0 || f.planes != 1 ||
            !f.allocation_size || f.allocation_size > kSharedAhbMaxAllocation ||
            f.offset < 0 || f.stride <= 0 || f.stride > std::numeric_limits<uint32_t>::max() ||
            f.sample_bits != 32 || f.subsample_x != 1 || f.subsample_y != 1 ||
            f.plane_width < f.width || f.plane_height < f.height || f.plane_size <= 0 ||
            f.stride % 4 || static_cast<uint64_t>(f.stride) != uint64_t(f.native_stride) * 4 ||
            static_cast<uint64_t>(f.stride) < uint64_t(f.width) * 4) return false;
    const uint64_t offset = static_cast<uint64_t>(f.offset);
    const uint64_t planeSize = static_cast<uint64_t>(f.plane_size);
    if (offset > f.allocation_size || planeSize > f.allocation_size - offset) return false;
    // Validate without unchecked add/multiply. Last-row padding need not be
    // visible, but all actual visible bytes must belong to the reported plane.
    const uint64_t row = uint64_t(f.width) * 4;
    return row <= planeSize && uint64_t(f.height - 1) <=
            (planeSize - row) / static_cast<uint64_t>(f.stride);
}
} // namespace android_display
