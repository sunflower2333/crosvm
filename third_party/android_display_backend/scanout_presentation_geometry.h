// Copyright 2026 The Android Open Source Project
// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <cstdint>
#include "scanout_geometry_abi.h"

namespace android_display {

// Internal host contract, not a wire ABI or a guest capability advertisement.
// The producer has already rotated logical pixels clockwise into real storage.
// The SurfaceControl buffer transform reverses this within the logical parent;
// its composition with the actual display transform must be checked separately.
enum class ScanoutContentRotation : uint32_t {
    Identity = 0,
    Clockwise90 = 1,
    Clockwise180 = 2,
    Clockwise270 = 3,
};

struct ScanoutPresentationGeometry {
    uint32_t storageWidth = 0, storageHeight = 0;
    uint32_t logicalWidth = 0, logicalHeight = 0;
    ScanoutContentRotation contentRotation = ScanoutContentRotation::Identity;
    uint64_t resetGeneration = 0, modeGeneration = 0;

    bool valid() const {
        const auto rotation = static_cast<uint32_t>(contentRotation);
        if (!resetGeneration || !modeGeneration || rotation > 3 ||
                !storageWidth || !storageHeight || !logicalWidth || !logicalHeight ||
                storageWidth > 8192 || storageHeight > 8192 ||
                logicalWidth > 8192 || logicalHeight > 8192) return false;
        return (rotation & 1)
                ? storageWidth == logicalHeight && storageHeight == logicalWidth
                : storageWidth == logicalWidth && storageHeight == logicalHeight;
    }

    bool operator==(const ScanoutPresentationGeometry& other) const {
        return storageWidth == other.storageWidth && storageHeight == other.storageHeight &&
                logicalWidth == other.logicalWidth && logicalHeight == other.logicalHeight &&
                contentRotation == other.contentRotation &&
                resetGeneration == other.resetGeneration && modeGeneration == other.modeGeneration;
    }
};

inline bool decodeScanoutGeometry(const AndroidDisplayScanoutGeometry& input,
                                 ScanoutPresentationGeometry* output) {
    if (!output || input.version != 1 || input.size != sizeof(input) || input.reserved ||
            input.reserved_tail[0] || input.reserved_tail[1] || input.content_rotation > 3)
        return false;
    const ScanoutPresentationGeometry decoded{
        input.storage_width, input.storage_height, input.logical_width, input.logical_height,
        static_cast<ScanoutContentRotation>(input.content_rotation),
        input.reset_generation, input.mode_generation,
    };
    if (!decoded.valid()) return false;
    *output = decoded;
    return true;
}

} // namespace android_display
