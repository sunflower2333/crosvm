// SPDX-License-Identifier: Apache-2.0
#include "native_blt.h"
#include <array>
#include <cstdint>
#include <cstdio>
#include <cstdlib>

static unsigned checks;
static void check(bool value, const char* label) {
    ++checks;
    if (!value) { std::fprintf(stderr, "FAIL %s\n", label); std::exit(1); }
}
int main() {
    NativeBltRegion r{1, 2, 4, 3, 3, 2};
    check(android_display::nativeBltRegionsValid(8, 7, 11, 9, &r, 1), "different extents with offsets");
    const NativeBltRegion valid = r;
    for (unsigned field = 0; field < 6; ++field) {
        r = valid;
        switch (field) {
        case 0: r.source_x = UINT32_MAX; break;
        case 1: r.source_y = UINT32_MAX; break;
        case 2: r.destination_x = UINT32_MAX; break;
        case 3: r.destination_y = UINT32_MAX; break;
        case 4: r.width = UINT32_MAX; break;
        case 5: r.height = UINT32_MAX; break;
        }
        check(!android_display::nativeBltRegionsValid(8, 7, 11, 9, &r, 1), "overflow/outside rejected");
    }
    r = valid; r.width = 0;
    check(!android_display::nativeBltRegionsValid(8, 7, 11, 9, &r, 1), "empty width");
    r = valid; r.height = 0;
    check(!android_display::nativeBltRegionsValid(8, 7, 11, 9, &r, 1), "empty height");
    check(!android_display::nativeBltRegionsValid(8, 7, 11, 9, nullptr, 1), "null pointer");
    check(!android_display::nativeBltRegionsValid(8, 7, 11, 9, &valid, 0), "no regions");
    check(!android_display::nativeBltRegionsValid(8, 7, 11, 9, &valid, 257), "bounded region count before access");
    check(!android_display::nativeBltRegionsValid(0, 7, 11, 9, &valid, 1), "zero source");
    check(!android_display::nativeBltRegionsValid(8, 7, 8193, 9, &valid, 1), "oversize destination");
    std::array<NativeBltRegion, 3> clips{{{1, 1, 3, 2, 3, 2}, {4, 3, 6, 4, 2, 2}, {2, 2, 4, 3, 2, 2}}};
    check(android_display::nativeBltRegionsValid(8, 7, 11, 9, clips.data(), clips.size()), "overlapping clips legal");
    check(android_display::nativeBltRegionsOverlap(clips[0], clips[2]), "overlap needs ordered writes");
    check(!android_display::nativeBltRegionsOverlap(clips[0], clips[1]), "adjacent distinct clips");
    std::array<uint32_t, 56> source{};
    std::array<uint32_t, 99> destination{};
    for (size_t i = 0; i < source.size(); ++i) source[i] = 0xff003100U + i;
    destination.fill(0x5a5a5a5a);
    for (const auto& c : clips)
        for (uint32_t y = 0; y < c.height; ++y)
            for (uint32_t x = 0; x < c.width; ++x)
                destination[(c.destination_y+y)*11+c.destination_x+x] = source[(c.source_y+y)*8+c.source_x+x];
    // Independent literal shape oracle: all clips translate source by (+2,+1).
    for (uint32_t y = 0; y < 9; ++y) for (uint32_t x = 0; x < 11; ++x) {
        const bool touched = (x>=3 && x<6 && y>=2 && y<4) || (x>=6 && x<8 && y>=4 && y<6) ||
                             (x>=4 && x<6 && y>=3 && y<5);
        const uint32_t expected = touched ? source[(y-1)*8+x-2] : 0x5a5a5a5a;
        check(destination[y*11+x] == expected, "translated clips preserve outside");
    }
    clips.back().destination_x = 11;
    check(!android_display::nativeBltRegionsValid(8, 7, 11, 9, clips.data(), clips.size()), "invalid final clip rejects full request");
    std::printf("native Blt geometry: %u checks PASS\n", checks);
}
