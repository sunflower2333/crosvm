// Copyright 2026 The Android Open Source Project
// SPDX-License-Identifier: Apache-2.0
#include "shared_ahb_allocation.h"
#include <cerrno>

#if defined(__ANDROID_APEX__)
// libui's authoritative gralloc mapper is not part of the com.android.virt
// APEX contract. Keep its ordinary display paths usable and reject the opt-in
// Native AHB API explicitly. The platform build links the real allocator.
extern "C" void* crosvm_shared_ahb_create_v2(
        uint32_t, uint32_t, uint32_t, SharedAllocationDescription* description, int* dma_fd,
        void (*error_callback)(const char*)) {
    if (description) *description = {};
    if (dma_fd) *dma_fd = -1;
    if (error_callback) error_callback("shared-ahb: authoritative mapper requires platform crosvm");
    return nullptr;
}
extern "C" void* crosvm_shared_ahb_create(
        uint32_t width, uint32_t height, SharedAllocationDescription* description, int* dma_fd,
        void (*error_callback)(const char*)) {
    return crosvm_shared_ahb_create_v2(width, height, 0, description, dma_fd, error_callback);
}
extern "C" void crosvm_shared_ahb_destroy(void*) {}
extern "C" AHardwareBuffer* crosvm_shared_ahb_get_buffer(void*) { return nullptr; }
extern "C" int crosvm_shared_ahb_duplicate_dma_buf(void*) { return -1; }
extern "C" const SharedAllocationDescription* crosvm_shared_ahb_get_description(void*) {
    return nullptr;
}
extern "C" int crosvm_shared_ahb_read_bytes(void*, uint64_t, void*, uint64_t) { return -ENOTSUP; }
extern "C" int crosvm_shared_ahb_write_bytes(void*, uint64_t, const void*, uint64_t) { return -ENOTSUP; }
extern "C" int crosvm_shared_ahb_fill_bytes(void*, uint64_t, uint64_t, uint32_t) { return -ENOTSUP; }
extern "C" void crosvm_shared_ahb_get_paging_counters(SharedAhbPagingCounters* counters) {
    if (counters) *counters = {};
}
#endif

// The allocator is linked into the display client. This process-local opaque
// holder ABI keeps authentic AHardwareBuffer ownership in one implementation;
// no gralloc handle, pointer, or fd is accepted from the guest.
extern "C" void* android_display_create_shared_ahb(
        uint32_t width, uint32_t height, SharedAllocationDescription* description, int* dma_fd,
        void (*error_callback)(const char*)) {
    return crosvm_shared_ahb_create(width, height, description, dma_fd, error_callback);
}

extern "C" void* android_display_create_shared_ahb_v2(
        uint32_t width, uint32_t height, uint32_t fourcc,
        SharedAllocationDescription* description, int* dma_fd,
        void (*error_callback)(const char*)) {
    if (!description || !dma_fd) return nullptr;
    *description = {};
    *dma_fd = -1;
    return crosvm_shared_ahb_create_v2(width, height, fourcc, description, dma_fd, error_callback);
}

extern "C" void android_display_destroy_shared_ahb(void* holder) {
    if (holder) crosvm_shared_ahb_destroy(holder);
}

extern "C" int android_display_shared_ahb_read_bytes(
        void* holder, uint64_t offset, void* destination, uint64_t length) {
    return crosvm_shared_ahb_read_bytes(holder, offset, destination, length);
}
extern "C" int android_display_shared_ahb_write_bytes(
        void* holder, uint64_t offset, const void* source, uint64_t length) {
    return crosvm_shared_ahb_write_bytes(holder, offset, source, length);
}
extern "C" int android_display_shared_ahb_fill_bytes(
        void* holder, uint64_t offset, uint64_t length, uint32_t pattern) {
    return crosvm_shared_ahb_fill_bytes(holder, offset, length, pattern);
}
