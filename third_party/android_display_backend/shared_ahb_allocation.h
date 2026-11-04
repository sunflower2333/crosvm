// Copyright 2026 The Android Open Source Project
// SPDX-License-Identifier: Apache-2.0
#pragma once
#include <android/hardware_buffer.h>
#include <android-base/result.h>
#include <android-base/unique_fd.h>
#include <cstdint>
#include <memory>
#include <mutex>

// C ABI descriptor shared with gpu_display's Rust FFI. Keep this explicitly
// laid out and independent from C++ names; no pointer or fd enters the ABI.
extern "C" {
struct SharedAllocationDescription {
    uint32_t version;
    uint32_t size;
    uint64_t buffer_id;
    uint64_t allocation_size;
    uint64_t modifier;
    uint64_t plane_offset;
    uint32_t plane_stride;
    uint32_t width;
    uint32_t height;
    uint32_t fourcc;
    uint32_t plane_count;
    uint32_t layout_flags;
};
struct SharedAhbPagingCounters {
    uint64_t read_bytes;
    uint64_t write_bytes;
    uint64_t fill_bytes;
};
}
static_assert(sizeof(SharedAllocationDescription) == 64);

namespace android_display {

// Native/FFI descriptor, all host-endian. The virtio protocol serializes every
// field explicitly; pointers, fd numbers and native-handle ints are never wire data.
struct SharedAhbDescription {
    uint32_t version = 1;
    uint32_t size = sizeof(SharedAhbDescription);
    uint64_t buffer_id = 0;
    uint64_t allocation_size = 0;
    uint64_t modifier = 0;
    uint64_t plane_offset = 0;
    uint32_t plane_stride = 0;
    uint32_t width = 0;
    uint32_t height = 0;
    uint32_t fourcc = 0;
    uint32_t plane_count = 0;
    uint32_t layout_flags = 0;
};
static_assert(sizeof(SharedAhbDescription) == 64);
constexpr uint32_t kSharedAhbVerifiedLinearAlias = 1;

// Owns the authentic Android allocation and its independently proven DMA-BUF
// alias. No gralloc handle is constructed or changed. Initial support is strict
// RGBA8 or BGRA8/linear/single-plane; CPU usage permits allocation-time proof and
// explicit VidMm paging, never a per-frame staging copy. Unrecognized/ambiguous
// layouts fail closed.
class SharedAhbAllocation {
public:
    static android::base::Result<std::shared_ptr<SharedAhbAllocation>> allocate(
            uint32_t width, uint32_t height, uint32_t fourcc);
    const SharedAhbDescription& description() const { return mDescription; }
    AHardwareBuffer* buffer() const { return mBuffer.get(); }
    android::base::unique_fd duplicateDmaBuf() const;
    bool isSameBacking(int fd) const;
    // Synchronous explicit paging only. The caller must own the allocation
    // exclusively after GPU retirement and every Android consumer release.
    // Includes allocation padding. Each nonzero operation is at most 64 KiB.
    // Returns 0 or negative errno; a failed write/fill may have changed bytes
    // before synchronization failed, so it must not publish writer reuse.
    int readBytes(uint64_t offset, void* destination, uint64_t length) const;
    int writeBytes(uint64_t offset, const void* source, uint64_t length) const;
    int fillBytes(uint64_t offset, uint64_t length, uint32_t pattern) const;

private:
    SharedAhbAllocation() = default;
    SharedAhbDescription mDescription;
    std::shared_ptr<AHardwareBuffer> mBuffer;
    android::base::unique_fd mDmaBuf;
    uint64_t mDevice = 0, mInode = 0;
    mutable std::mutex mPagingMutex;
    int pageBytes(uint32_t operation, uint64_t offset, uint64_t length,
                  void* destination, const void* source, uint32_t pattern) const;
};
} // namespace android_display

extern "C" {
// Preserve the original RGBA-only call shape. Explicit format requests use a
// distinct symbol so a stale library cannot interpret the fourcc as a pointer.
void* crosvm_shared_ahb_create(uint32_t width, uint32_t height,
                               SharedAllocationDescription* description, int* dma_fd,
                               void (*error_callback)(const char*));
void* crosvm_shared_ahb_create_v2(uint32_t width, uint32_t height, uint32_t fourcc,
                               SharedAllocationDescription* description, int* dma_fd,
                               void (*error_callback)(const char*));
void* android_display_create_shared_ahb(uint32_t width, uint32_t height,
                                      SharedAllocationDescription* description, int* dma_fd,
                                      void (*error_callback)(const char*));
void* android_display_create_shared_ahb_v2(uint32_t width, uint32_t height, uint32_t fourcc,
                                         SharedAllocationDescription* description, int* dma_fd,
                                         void (*error_callback)(const char*));
void android_display_destroy_shared_ahb(void* holder);
void crosvm_shared_ahb_destroy(void* holder);
AHardwareBuffer* crosvm_shared_ahb_get_buffer(void* holder);
int crosvm_shared_ahb_duplicate_dma_buf(void* holder);
const SharedAllocationDescription* crosvm_shared_ahb_get_description(void* holder);
int crosvm_shared_ahb_read_bytes(void* holder, uint64_t offset, void* destination, uint64_t length);
int crosvm_shared_ahb_write_bytes(void* holder, uint64_t offset, const void* source, uint64_t length);
int crosvm_shared_ahb_fill_bytes(void* holder, uint64_t offset, uint64_t length, uint32_t pattern);
void crosvm_shared_ahb_get_paging_counters(SharedAhbPagingCounters* counters);
int android_display_shared_ahb_read_bytes(void* holder, uint64_t offset, void* destination, uint64_t length);
int android_display_shared_ahb_write_bytes(void* holder, uint64_t offset, const void* source, uint64_t length);
int android_display_shared_ahb_fill_bytes(void* holder, uint64_t offset, uint64_t length, uint32_t pattern);
}
