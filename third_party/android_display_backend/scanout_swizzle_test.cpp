/* SPDX-License-Identifier: Apache-2.0 */
#include "scanout_swizzle.h"

#include <cstdio>
#include <vector>

int main() {
    size_t cases = 0;
    // Cover unaligned addresses, vector tails and padded multi-row surfaces.
    for (size_t width = 0; width <= 257; ++width) {
        for (size_t align = 0; align < 16; ++align) {
            for (size_t padding = 0; padding <= 7; ++padding) {
                const size_t stride = width * 4 + padding;
                std::vector<uint8_t> original(32 + align + stride * 3);
                for (size_t i = 0; i < original.size(); ++i)
                    original[i] = static_cast<uint8_t>(i * 73 + (i >> 2));
                auto expected = original;
                auto actual = original;
                for (size_t y = 0; y < 3; ++y) {
                    const size_t start = 16 + align + y * stride;
                    for (size_t x = 0; x < width; ++x) {
                        expected[start + x * 4] = original[start + x * 4 + 2];
                        expected[start + x * 4 + 2] = original[start + x * 4];
                    }
                    swapScanoutRedBlueRow(actual.data() + start, width);
                }
                if (actual != expected) {
                    std::fprintf(stderr, "FAIL width=%zu alignment=%zu padding=%zu\n", width, align, padding);
                    return 1;
                }
                for (size_t y = 0; y < 3; ++y)
                    swapScanoutRedBlueRow(actual.data() + 16 + align + y * stride, width);
                if (actual != original) return 2;
                ++cases;
            }
        }
    }
    std::printf("PASS %zu cases: channels, alpha, padding, guards and roundtrip\n", cases);
    return 0;
}
