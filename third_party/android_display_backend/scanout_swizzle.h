/* SPDX-License-Identifier: Apache-2.0 */
#pragma once

#include <cstddef>
#include <cstdint>

#if defined(__aarch64__)
#include <arm_neon.h>
#endif

// Convert BGRA/BGRX to RGBA/RGBX in a CPU-locked native-window row. Only
// complete visible pixels are touched; callers retain stride padding.
inline void swapScanoutRedBlueRow(uint8_t* row, size_t pixels) {
    size_t x = 0;
#if defined(__aarch64__)
    const uint8x16_t channels = {2, 1, 0, 3, 6, 5, 4, 7, 10, 9, 8, 11, 14, 13, 12, 15};
    for (; pixels - x >= 4; x += 4) {
        uint8_t* group = row + x * 4;
        vst1q_u8(group, vqtbl1q_u8(vld1q_u8(group), channels));
    }
#endif
    for (; x < pixels; ++x) {
        uint8_t* pixel = row + x * 4;
        const uint8_t red = pixel[2];
        pixel[2] = pixel[0];
        pixel[0] = red;
    }
}
