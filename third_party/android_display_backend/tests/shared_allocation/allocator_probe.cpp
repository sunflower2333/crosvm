// Copyright 2026 The Android Open Source Project
// SPDX-License-Identifier: Apache-2.0
#include "shared_ahb_allocation.h"
#include <fcntl.h>
#include <inttypes.h>
#include <signal.h>
#include <stdio.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>
#include <cstring>
#include <vector>

namespace {
constexpr uint32_t kRgbaFourcc = 0x34324241;
void timedOut(int) {
    constexpr char message[] = "FAIL shared-ahb allocation watchdog\n";
    (void)write(STDERR_FILENO, message, sizeof(message) - 1);
    _exit(124);
}
}

int main(int argc, char** argv) {
    if (argc > 3 || (argc >= 2 && strcmp(argv[1], "rgba") && strcmp(argv[1], "bgra")) ||
            (argc == 3 && strcmp(argv[2], "triple"))) {
        fprintf(stderr, "usage: %s [rgba|bgra [triple]]\n", argv[0]);
        return 2;
    }
    const uint32_t requestedFourcc = argc >= 2 && !strcmp(argv[1], "bgra")
            ? 0x34325241 : kRgbaFourcc;
    signal(SIGALRM, timedOut);
    alarm(45);
    setvbuf(stdout, nullptr, _IONBF, 0);
    unsigned assertions = 0;
    std::vector<std::shared_ptr<android_display::SharedAhbAllocation>> retained;
    const std::vector<std::pair<uint32_t, uint32_t>> dimensions = argc == 3
            ? std::vector<std::pair<uint32_t, uint32_t>>(3, {3040u, 1904u})
            : std::vector<std::pair<uint32_t, uint32_t>>{{64u, 64u}, {513u, 257u}, {1920u, 1080u}};
    uint64_t totalSize = 0;
    for (const auto [width, height] : dimensions) {
        auto result = android_display::SharedAhbAllocation::allocate(
                width, height, requestedFourcc);
        if (!result.ok()) {
            fprintf(stderr, "FAIL %ux%u: %s\n", width, height, result.error().message().c_str());
            return 1;
        }
        auto allocation = *result;
        const auto& d = allocation->description();
        totalSize += d.allocation_size;
        printf("ALLOCATION buffer_id=%" PRIu64 " size=%" PRIu64 " modifier=%" PRIu64
               " offset=%" PRIu64 " stride=%u width=%u height=%u fourcc=%08x flags=%u\n",
               d.buffer_id, d.allocation_size, d.modifier, d.plane_offset, d.plane_stride,
               d.width, d.height, d.fourcc, d.layout_flags);
        auto duplicate = allocation->duplicateDmaBuf();
        struct stat identity{};
        if (duplicate.get() < 0 || fstat(duplicate.get(), &identity)) return 1;
        printf("DMA_IDENTITY buffer_id=%" PRIu64 " device=%" PRIu64 " inode=%" PRIu64 "\n",
               d.buffer_id, uint64_t(identity.st_dev), uint64_t(identity.st_ino));
        void* mapping = mmap(nullptr, d.allocation_size, PROT_READ | PROT_WRITE,
                             MAP_SHARED, duplicate.get(), 0);
        if (mapping == MAP_FAILED) return 1;
        FILE* smaps = fopen("/proc/self/smaps", "re");
        if (!smaps) { munmap(mapping, d.allocation_size); return 1; }
        bool selected = false, foundFlags = false;
        char line[1024];
        while (fgets(line, sizeof(line), smaps)) {
            uintptr_t begin = 0, end = 0;
            if (sscanf(line, "%" SCNxPTR "-%" SCNxPTR, &begin, &end) == 2)
                selected = reinterpret_cast<uintptr_t>(mapping) >= begin &&
                        reinterpret_cast<uintptr_t>(mapping) < end;
            if (selected && !strncmp(line, "VmFlags:", 8)) {
                printf("DMABUF_VMA buffer_id=%" PRIu64 " %s", d.buffer_id, line);
                foundFlags = true;
                break;
            }
        }
        fclose(smaps);
        munmap(mapping, d.allocation_size);
        if (!foundFlags) return 1;
        ++assertions;
        android::base::unique_fd unrelated(open("/dev/null", O_RDONLY | O_CLOEXEC));
        if (duplicate.get() < 0 || !allocation->isSameBacking(duplicate.get()) ||
                allocation->isSameBacking(unrelated.get()) || allocation->isSameBacking(-1)) {
            fprintf(stderr, "FAIL duplicate or unrelated descriptor identity\n");
            return 1;
        }
        assertions += 4;
        for (const auto& previous : retained) {
            if (previous->isSameBacking(duplicate.get()) ||
                    previous->description().buffer_id == d.buffer_id) {
                fprintf(stderr, "FAIL simultaneous allocations alias\n");
                return 1;
            }
            assertions += 2;
        }
        retained.push_back(std::move(allocation));
    }
    if (android_display::SharedAhbAllocation::allocate(
                0, 64, requestedFourcc).ok() ||
            android_display::SharedAhbAllocation::allocate(
                64, 8193, requestedFourcc).ok()) return 1;
    assertions += 2;
    alarm(0);
    printf("RESULT passed=1 assertions=%u allocations=%zu total_bytes=%" PRIu64
           " guest_mapping=0 zero_copy_enabled=0\n", assertions, retained.size(), totalSize);
    return 0;
}
