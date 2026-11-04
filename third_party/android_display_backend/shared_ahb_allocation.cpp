// Copyright 2026 The Android Open Source Project
// SPDX-License-Identifier: Apache-2.0
#include "shared_ahb_allocation.h"
#include "shared_ahb_layout.h"
#include <android-base/scopeguard.h>
#include <ui/GraphicBufferMapper.h>
#include <vndk/hardware_buffer.h>
#include <linux/dma-buf.h>
#include <fcntl.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>
#include <cstring>
#include <atomic>
#include <cerrno>
#include <vector>

namespace android_display {
using android::base::Error;
using android::base::Result;
using android::base::unique_fd;

namespace {
std::atomic<uint64_t> pagingReadBytes{0}, pagingWriteBytes{0}, pagingFillBytes{0};
constexpr uint32_t kPagingRead = 1, kPagingWrite = 2, kPagingFill = 3;

int syncPagingDma(int fd, uint64_t flags) {
    dma_buf_sync sync{.flags = flags};
    int result;
    do { result = ioctl(fd, DMA_BUF_IOCTL_SYNC, &sync); } while (result < 0 && errno == EINTR);
    return result == 0 ? 0 : -errno;
}

uint32_t pattern(uint32_t x, uint32_t y, uint32_t seed) {
    uint32_t value = x * 0x9e3779b9u + y * 0x85ebca6bu + seed;
    value ^= value >> 16; value *= 0x7feb352du; value ^= value >> 15;
    return (value & 0x00ffffffu) | 0xff000000u;
}
bool writeAhb(AHardwareBuffer* buffer, const SharedAhbLayoutFacts& facts, uint32_t seed) {
    void* pixels = nullptr;
    if (AHardwareBuffer_lock(buffer, AHARDWAREBUFFER_USAGE_CPU_WRITE_OFTEN,
                             -1, nullptr, &pixels) != 0) return false;
    if (!pixels) { AHardwareBuffer_unlock(buffer, nullptr); return false; }
    auto* data = static_cast<uint32_t*>(pixels);
    for (uint32_t y = 0; y < facts.height; ++y)
        for (uint32_t x = 0; x < facts.width; ++x)
            data[uint64_t(y) * facts.native_stride + x] = pattern(x, y, seed);
    // nullptr requests synchronous CPU completion under the AHB API contract.
    // This allocation-time proof is unrelated to GPU PRESENT's async fence.
    return AHardwareBuffer_unlock(buffer, nullptr) == 0;
}
bool matchesDmaBuf(int fd, const SharedAhbLayoutFacts& facts, uint32_t seed) {
    // Metadata/data FD positions are not assumed. Require a real DMA-BUF and
    // exact capacity, then verify every visible byte through the mapper layout.
    const off_t length = lseek(fd, 0, SEEK_END);
    if (length < 0 || static_cast<uint64_t>(length) != facts.allocation_size) return false;
    dma_buf_sync sync{.flags = DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ};
    if (ioctl(fd, DMA_BUF_IOCTL_SYNC, &sync) != 0) return false;
    auto finish = android::base::make_scope_guard([&] {
        sync.flags = DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ;
        (void)ioctl(fd, DMA_BUF_IOCTL_SYNC, &sync);
    });
    void* mapping = mmap(nullptr, facts.allocation_size, PROT_READ, MAP_SHARED, fd, 0);
    if (mapping == MAP_FAILED) return false;
    auto unmap = android::base::make_scope_guard([&] { munmap(mapping, facts.allocation_size); });
    const auto* bytes = static_cast<const uint8_t*>(mapping) + facts.offset;
    for (uint32_t y = 0; y < facts.height; ++y) for (uint32_t x = 0; x < facts.width; ++x) {
        uint32_t actual;
        memcpy(&actual, bytes + uint64_t(y) * facts.stride + uint64_t(x) * 4, 4);
        if (actual != pattern(x, y, seed)) return false;
    }
    sync.flags = DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ;
    const bool synced = ioctl(fd, DMA_BUF_IOCTL_SYNC, &sync) == 0;
    finish.Disable();
    return synced;
}
bool verifyReverseAlias(int fd, AHardwareBuffer* buffer, const SharedAhbLayoutFacts& facts) {
    constexpr uint32_t seed = 0x74291b36;
    dma_buf_sync sync{.flags = DMA_BUF_SYNC_START | DMA_BUF_SYNC_WRITE};
    if (ioctl(fd, DMA_BUF_IOCTL_SYNC, &sync) != 0) return false;
    void* mapping = mmap(nullptr, facts.allocation_size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (mapping == MAP_FAILED) {
        sync.flags = DMA_BUF_SYNC_END | DMA_BUF_SYNC_WRITE;
        (void)ioctl(fd, DMA_BUF_IOCTL_SYNC, &sync);
        return false;
    }
    // All exported bytes, including offset/row/end padding, are initialized
    // before any guest could map this object. The alias has already been proven.
    memset(mapping, 0, facts.allocation_size);
    auto* bytes = static_cast<uint8_t*>(mapping) + facts.offset;
    for (uint32_t y = 0; y < facts.height; ++y) for (uint32_t x = 0; x < facts.width; ++x) {
        const uint32_t value = pattern(x, y, seed);
        memcpy(bytes + uint64_t(y) * facts.stride + uint64_t(x) * 4, &value, 4);
    }
    munmap(mapping, facts.allocation_size);
    sync.flags = DMA_BUF_SYNC_END | DMA_BUF_SYNC_WRITE;
    if (ioctl(fd, DMA_BUF_IOCTL_SYNC, &sync) != 0) return false;
    void* pixels = nullptr;
    if (AHardwareBuffer_lock(buffer, AHARDWAREBUFFER_USAGE_CPU_READ_OFTEN,
                             -1, nullptr, &pixels) != 0) return false;
    if (!pixels) { AHardwareBuffer_unlock(buffer, nullptr); return false; }
    const auto* data = static_cast<const uint32_t*>(pixels);
    bool matches = true;
    for (uint32_t y = 0; y < facts.height && matches; ++y)
        for (uint32_t x = 0; x < facts.width; ++x)
            if (data[uint64_t(y) * facts.native_stride + x] != pattern(x, y, seed)) {
                matches = false; break;
            }
    return AHardwareBuffer_unlock(buffer, nullptr) == 0 && matches;
}
} // namespace

Result<std::shared_ptr<SharedAhbAllocation>> SharedAhbAllocation::allocate(
        uint32_t width, uint32_t height, uint32_t fourcc) {
    if (!width || !height || width > 8192 || height > 8192)
        return Error() << "shared-ahb: invalid dimensions";
    auto allocation = std::shared_ptr<SharedAhbAllocation>(new SharedAhbAllocation);
    AHardwareBuffer_Desc request{};
    request.width = width; request.height = height; request.layers = 1;
    if (fourcc == kSharedAhbRgbaFourcc) {
        request.format = AHARDWAREBUFFER_FORMAT_R8G8B8A8_UNORM;
    } else if (fourcc == kSharedAhbBgraFourcc) {
        request.format = AHARDWAREBUFFER_FORMAT_B8G8R8A8_UNORM;
    } else {
        return Error() << "shared-ahb: unsupported requested fourcc";
    }
    request.usage = AHARDWAREBUFFER_USAGE_GPU_COLOR_OUTPUT | AHARDWAREBUFFER_USAGE_GPU_SAMPLED_IMAGE |
            AHARDWAREBUFFER_USAGE_COMPOSER_OVERLAY | AHARDWAREBUFFER_USAGE_CPU_READ_OFTEN |
            AHARDWAREBUFFER_USAGE_CPU_WRITE_OFTEN;
    AHardwareBuffer* raw = nullptr;
    if (AHardwareBuffer_allocate(&request, &raw) != 0 || !raw)
        return Error() << "shared-ahb: gralloc allocation failed";
    allocation->mBuffer = {raw, AHardwareBuffer_release};
    AHardwareBuffer_Desc actual{};
    AHardwareBuffer_describe(raw, &actual);
    if (actual.width != width || actual.height != height || actual.layers != 1 ||
            actual.format != request.format || (actual.usage & request.usage) != request.usage)
        return Error() << "shared-ahb: gralloc description mismatch";
    const native_handle_t* handle = AHardwareBuffer_getNativeHandle(raw);
    if (!handle || handle->numFds <= 0 || handle->numFds > 16)
        return Error() << "shared-ahb: invalid native fd inventory";
    auto& mapper = android::GraphicBufferMapper::get();
    SharedAhbLayoutFacts facts{};
    facts.width = width; facts.height = height; facts.native_stride = actual.stride;
    std::vector<android::ui::PlaneLayout> planes;
    if (mapper.getBufferId(handle, &facts.buffer_id) != 0 ||
            mapper.getAllocationSize(handle, &facts.allocation_size) != 0 ||
            mapper.getPixelFormatFourCC(handle, &facts.fourcc) != 0 ||
            mapper.getPixelFormatModifier(handle, &facts.modifier) != 0 ||
            mapper.getPlaneLayouts(handle, &planes) != 0)
        return Error() << "shared-ahb: authoritative mapper metadata unavailable";
    facts.planes = planes.size();
    if (planes.size() != 1) return Error() << "shared-ahb: requires exactly one authoritative plane";
    const auto& plane = planes[0];
    facts.offset = plane.offsetInBytes; facts.stride = plane.strideInBytes;
    facts.sample_bits = plane.sampleIncrementInBits; facts.plane_width = plane.widthInSamples;
    facts.plane_height = plane.heightInSamples; facts.plane_size = plane.totalSizeInBytes;
    facts.subsample_x = plane.horizontalSubsampling; facts.subsample_y = plane.verticalSubsampling;
    if (!validSharedAhbLayout(facts, fourcc))
        return Error() << "shared-ahb: unsupported or inconsistent authoritative linear layout";
    // Two changing full-image patterns must alias through the same owned fd.
    // Duplicate fds for the SAME kernel object are allowed, distinct matching
    // objects are ambiguous and rejected. No native-handle data is rewritten.
    constexpr uint32_t seeds[] = {0x147ac39e, 0xb891503d};
    for (uint32_t seed : seeds) {
        if (!writeAhb(raw, facts, seed)) return Error() << "shared-ahb: CPU proof producer failed";
        unique_fd matched;
        uint64_t device = 0, inode = 0;
        for (int index = 0; index < handle->numFds; ++index) {
            const int candidate = handle->data[index];
            if (!matchesDmaBuf(candidate, facts, seed)) continue;
            struct stat identity{};
            if (fstat(candidate, &identity) != 0 || !identity.st_ino)
                return Error() << "shared-ahb: DMA object identity unavailable";
            if (matched.get() >= 0 && (device != identity.st_dev || inode != identity.st_ino))
                return Error() << "shared-ahb: ambiguous distinct matching DMA objects";
            if (matched.get() < 0) {
                matched.reset(fcntl(candidate, F_DUPFD_CLOEXEC, 0));
                if (matched.get() < 0) return Error() << "shared-ahb: DMA descriptor duplication failed";
                device = identity.st_dev; inode = identity.st_ino;
            }
        }
        if (matched.get() < 0) return Error() << "shared-ahb: no authoritative layout/FD alias";
        if (allocation->mDmaBuf.get() >= 0 &&
                (allocation->mDevice != device || allocation->mInode != inode))
            return Error() << "shared-ahb: backing identity changed during proof";
        allocation->mDevice = device; allocation->mInode = inode;
        allocation->mDmaBuf = std::move(matched);
    }
    if (!verifyReverseAlias(allocation->mDmaBuf.get(), raw, facts))
        return Error() << "shared-ahb: reverse DMA-to-AHB alias proof failed";
    uint64_t finalBufferId = 0, finalSize = 0, finalModifier = 0;
    uint32_t finalFourcc = 0;
    std::vector<android::ui::PlaneLayout> finalPlanes;
    if (mapper.getBufferId(handle, &finalBufferId) != 0 || finalBufferId != facts.buffer_id ||
            mapper.getAllocationSize(handle, &finalSize) != 0 || finalSize != facts.allocation_size ||
            mapper.getPixelFormatModifier(handle, &finalModifier) != 0 || finalModifier != facts.modifier ||
            mapper.getPixelFormatFourCC(handle, &finalFourcc) != 0 || finalFourcc != facts.fourcc ||
            mapper.getPlaneLayouts(handle, &finalPlanes) != 0 || finalPlanes != planes)
        return Error() << "shared-ahb: authoritative metadata changed during proof";
    allocation->mDescription = {.version = 1, .size = sizeof(SharedAhbDescription),
        .buffer_id = facts.buffer_id, .allocation_size = facts.allocation_size,
        .modifier = facts.modifier, .plane_offset = static_cast<uint64_t>(facts.offset),
        .plane_stride = static_cast<uint32_t>(facts.stride), .width = width, .height = height,
        .fourcc = facts.fourcc, .plane_count = 1, .layout_flags = kSharedAhbVerifiedLinearAlias};
    return allocation;
}

unique_fd SharedAhbAllocation::duplicateDmaBuf() const {
    return unique_fd(fcntl(mDmaBuf.get(), F_DUPFD_CLOEXEC, 0));
}
bool SharedAhbAllocation::isSameBacking(int fd) const {
    struct stat identity{};
    return fd >= 0 && fstat(fd, &identity) == 0 &&
            static_cast<uint64_t>(identity.st_dev) == mDevice &&
            static_cast<uint64_t>(identity.st_ino) == mInode;
}

int SharedAhbAllocation::readBytes(uint64_t offset, void* destination, uint64_t length) const {
    return pageBytes(kPagingRead, offset, length, destination, nullptr, 0);
}
int SharedAhbAllocation::writeBytes(uint64_t offset, const void* source, uint64_t length) const {
    return pageBytes(kPagingWrite, offset, length, nullptr, source, 0);
}
int SharedAhbAllocation::fillBytes(uint64_t offset, uint64_t length, uint32_t pattern) const {
    return pageBytes(kPagingFill, offset, length, nullptr, nullptr, pattern);
}

int SharedAhbAllocation::pageBytes(uint32_t operation, uint64_t offset, uint64_t length,
                                 void* destination, const void* source, uint32_t pattern) const {
    if (!length || length > 65536 || offset > mDescription.allocation_size ||
            length > mDescription.allocation_size - offset ||
            (operation == kPagingRead && !destination) ||
            (operation == kPagingWrite && !source) ||
            (operation == kPagingFill && ((offset | length) & 3)) ||
            operation < kPagingRead || operation > kPagingFill) return -EINVAL;
    const long pageSize = sysconf(_SC_PAGESIZE);
    if (pageSize <= 0 || (pageSize & (pageSize - 1))) return -EINVAL;
    const uint64_t alignedOffset = offset & ~(uint64_t(pageSize) - 1);
    const size_t delta = static_cast<size_t>(offset - alignedOffset);
    const size_t mappingSize = delta + static_cast<size_t>(length);
    const uint64_t access = operation == kPagingRead ? DMA_BUF_SYNC_READ : DMA_BUF_SYNC_WRITE;
    std::lock_guard<std::mutex> lock(mPagingMutex);
    int result = syncPagingDma(mDmaBuf.get(), DMA_BUF_SYNC_START | access);
    if (result) return result;
    auto endAccess = android::base::make_scope_guard([&] {
        (void)syncPagingDma(mDmaBuf.get(), DMA_BUF_SYNC_END | access);
    });
    void* mapping = mmap(nullptr, mappingSize,
                         operation == kPagingRead ? PROT_READ : PROT_READ | PROT_WRITE,
                         MAP_SHARED, mDmaBuf.get(), static_cast<off_t>(alignedOffset));
    if (mapping == MAP_FAILED) return -errno;
    auto unmap = android::base::make_scope_guard([&] { munmap(mapping, mappingSize); });
    auto* bytes = static_cast<uint8_t*>(mapping) + delta;
    if (operation == kPagingRead) {
        memcpy(destination, bytes, length);
        pagingReadBytes.fetch_add(length, std::memory_order_relaxed);
    } else if (operation == kPagingWrite) {
        memcpy(bytes, source, length);
        pagingWriteBytes.fetch_add(length, std::memory_order_relaxed);
    } else {
        // The wire fill pattern is little endian, independently of host byte order.
        const uint8_t value[] = {uint8_t(pattern), uint8_t(pattern >> 8),
                                 uint8_t(pattern >> 16), uint8_t(pattern >> 24)};
        for (uint64_t pos = 0; pos < length; pos += 4) memcpy(bytes + pos, value, 4);
        pagingFillBytes.fetch_add(length, std::memory_order_relaxed);
    }
    result = syncPagingDma(mDmaBuf.get(), DMA_BUF_SYNC_END | access);
    endAccess.Disable();
    return result;
}
} // namespace android_display

namespace {
using Holder = std::shared_ptr<android_display::SharedAhbAllocation>;
}

extern "C" void* crosvm_shared_ahb_create(uint32_t width, uint32_t height,
                                       SharedAllocationDescription* description, int* dma_fd,
                                       void (*error_callback)(const char*)) {
    return crosvm_shared_ahb_create_v2(width, height, android_display::kSharedAhbRgbaFourcc,
                                     description, dma_fd, error_callback);
}

extern "C" void* crosvm_shared_ahb_create_v2(uint32_t width, uint32_t height, uint32_t fourcc,
                                           SharedAllocationDescription* description,
                                           int* dma_fd,
                                           void (*error_callback)(const char*)) {
    if (!description || !dma_fd) return nullptr;
    *description = {};
    *dma_fd = -1;
    auto result = android_display::SharedAhbAllocation::allocate(width, height, fourcc);
    if (!result.ok()) {
        if (error_callback) error_callback(result.error().message().c_str());
        return nullptr;
    }
    auto fd = (*result)->duplicateDmaBuf();
    if (fd.get() < 0) {
        if (error_callback) error_callback("shared-ahb: DMA descriptor duplication failed");
        return nullptr;
    }
    auto* holder = new Holder(*result);
    const auto& native = (*holder)->description();
    description->version = native.version;
    description->size = native.size;
    description->buffer_id = native.buffer_id;
    description->allocation_size = native.allocation_size;
    description->modifier = native.modifier;
    description->plane_offset = native.plane_offset;
    description->plane_stride = native.plane_stride;
    description->width = native.width;
    description->height = native.height;
    description->fourcc = native.fourcc;
    description->plane_count = native.plane_count;
    description->layout_flags = native.layout_flags;
    *dma_fd = fd.release();
    return holder;
}

extern "C" void crosvm_shared_ahb_destroy(void* holder) {
    delete static_cast<Holder*>(holder);
}

extern "C" int crosvm_shared_ahb_duplicate_dma_buf(void* holder) {
    return holder ? static_cast<Holder*>(holder)->get()->duplicateDmaBuf().release() : -1;
}

extern "C" AHardwareBuffer* crosvm_shared_ahb_get_buffer(void* holder) {
    if (!holder) return nullptr;
    auto* allocation = static_cast<Holder*>(holder);
    return allocation->get() ? allocation->get()->buffer() : nullptr;
}

extern "C" const SharedAllocationDescription* crosvm_shared_ahb_get_description(void* holder) {
    if (!holder) return nullptr;
    auto* allocation = static_cast<Holder*>(holder);
    if (!allocation->get()) return nullptr;
    // The C++ and C descriptors are byte-identical by construction. Keep a
    // process-local copy so callers never see a reinterpret-cast object.
    thread_local SharedAllocationDescription description;
    const auto& native = allocation->get()->description();
    description = {.version = native.version,
                   .size = native.size,
                   .buffer_id = native.buffer_id,
                   .allocation_size = native.allocation_size,
                   .modifier = native.modifier,
                   .plane_offset = native.plane_offset,
                   .plane_stride = native.plane_stride,
                   .width = native.width,
                   .height = native.height,
                   .fourcc = native.fourcc,
                   .plane_count = native.plane_count,
                   .layout_flags = native.layout_flags};
    return &description;
}

extern "C" int crosvm_shared_ahb_read_bytes(
        void* holder, uint64_t offset, void* destination, uint64_t length) {
    if (!holder || !static_cast<Holder*>(holder)->get()) return -EINVAL;
    return (*static_cast<Holder*>(holder))->readBytes(offset, destination, length);
}
extern "C" int crosvm_shared_ahb_write_bytes(
        void* holder, uint64_t offset, const void* source, uint64_t length) {
    if (!holder || !static_cast<Holder*>(holder)->get()) return -EINVAL;
    return (*static_cast<Holder*>(holder))->writeBytes(offset, source, length);
}
extern "C" int crosvm_shared_ahb_fill_bytes(
        void* holder, uint64_t offset, uint64_t length, uint32_t pattern) {
    if (!holder || !static_cast<Holder*>(holder)->get()) return -EINVAL;
    return (*static_cast<Holder*>(holder))->fillBytes(offset, length, pattern);
}
extern "C" void crosvm_shared_ahb_get_paging_counters(SharedAhbPagingCounters* counters) {
    if (!counters) return;
    *counters = {android_display::pagingReadBytes.load(std::memory_order_relaxed),
                 android_display::pagingWriteBytes.load(std::memory_order_relaxed),
                 android_display::pagingFillBytes.load(std::memory_order_relaxed)};
}
