// Copyright 2026 The Android Open Source Project
// SPDX-License-Identifier: Apache-2.0
#pragma once
#include "native_blt.h"
#include <cstring>

// Ordinary guest backing is copied only through an explicit staging buffer.
// This descriptor describes host-owned bytes, never a mapped native AHB.
struct NativeBltCpuLayout {
    uint32_t width, height, stride, fourcc;
    uint64_t size;
};
static_assert(sizeof(NativeBltCpuLayout) == 24);

namespace android_display {
constexpr uint64_t kNativeBltStagingLimit = 64ULL * 1024 * 1024;
inline bool nativeBltTransferValid(uint32_t nw, uint32_t nh, const NativeBltCpuLayout& cpu,
        const NativeBltRegion* regions, uint32_t count, bool upload, uint64_t* packedSize) {
    if (!packedSize) return false;
    *packedSize = 0;
    if (!cpu.width || !cpu.height || cpu.width>8192 || cpu.height>8192 ||
            (cpu.fourcc!=0x34324241 && cpu.fourcc!=0x34325241 && cpu.fourcc!=0x34325258) || cpu.stride%4 ||
            uint64_t(cpu.stride)<uint64_t(cpu.width)*4 || cpu.size>256ULL*1024*1024 ||
            uint64_t(cpu.stride)*(cpu.height-1)+uint64_t(cpu.width)*4>cpu.size ||
            !nativeBltRegionsValid(upload ? cpu.width : nw, upload ? cpu.height : nh,
                upload ? nw : cpu.width, upload ? nh : cpu.height, regions, count)) return false;
    uint64_t total=0;
    for (uint32_t i=0; i<count; ++i) {
        total+=uint64_t(regions[i].width)*regions[i].height*4;
        if (total>kNativeBltStagingLimit) return false;
    }
    *packedSize=total;
    return true;
}
inline uint32_t nativeBltColorFourcc(uint32_t fourcc) {
    return fourcc==0x34325258 ? 0x34325241 : fourcc;
}
inline void nativeBltConvertRow(uint8_t* destination, const uint8_t* source, uint32_t pixels, bool swap, bool opaque) {
    if (!swap && !opaque) { std::memcpy(destination, source, uint64_t(pixels)*4); return; }
    for (uint32_t x=0; x<pixels; ++x) {
        destination[x*4]=source[x*4+(swap?2:0)]; destination[x*4+1]=source[x*4+1];
        destination[x*4+2]=source[x*4+(swap?0:2)]; destination[x*4+3]=opaque?0xff:source[x*4+3];
    }
}
// Call only after complete nativeBltTransferValid admission and with distinct
// staging/ordinary storage. Regions are packed in order, including overlaps.
inline void nativeBltPackUpload(const uint8_t* source, const NativeBltCpuLayout& cpu,
        uint32_t nativeFourcc, const NativeBltRegion* regions, uint32_t count, uint8_t* packed) {
    for (uint32_t i=0; i<count; ++i) {
        const auto& r=regions[i];
        for (uint32_t y=0; y<r.height; ++y) {
            nativeBltConvertRow(packed, source+uint64_t(r.source_y+y)*cpu.stride+uint64_t(r.source_x)*4,
                               r.width, nativeBltColorFourcc(cpu.fourcc)!=nativeFourcc, cpu.fourcc==0x34325258);
            packed+=uint64_t(r.width)*4;
        }
    }
}
inline void nativeBltUnpackReadback(const uint8_t* packed, uint32_t nativeFourcc,
        const NativeBltCpuLayout& cpu, const NativeBltRegion* regions, uint32_t count, uint8_t* destination) {
    for (uint32_t i=0; i<count; ++i) {
        const auto& r=regions[i];
        for (uint32_t y=0; y<r.height; ++y) {
            nativeBltConvertRow(destination+uint64_t(r.destination_y+y)*cpu.stride+uint64_t(r.destination_x)*4,
                               packed, r.width, nativeBltColorFourcc(cpu.fourcc)!=nativeFourcc, cpu.fourcc==0x34325258);
            packed+=uint64_t(r.width)*4;
        }
    }
}
} // namespace android_display

extern "C" {
// Exclusive native ownership and ordinary backing lifetime are caller-held.
// These calls return success only after bounded GPU completion, including
// FOREIGN release and (for readback) host cache invalidation. Failure poisons
// the context when completion is uncertain; no failed readback writes bytes.
// completed_bytes counts explicit transfer bytes, never native paging bytes.
int android_display_native_blt_upload(void* context, void* destination,
        const uint8_t* source, const NativeBltCpuLayout* layout,
        const NativeBltRegion* regions, uint32_t count, uint64_t* completed_bytes);
int android_display_native_blt_readback(void* context, void* source,
        uint8_t* destination, const NativeBltCpuLayout* layout,
        const NativeBltRegion* regions, uint32_t count, uint64_t* completed_bytes);
}
