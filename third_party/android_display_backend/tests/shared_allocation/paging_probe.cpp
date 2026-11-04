// Copyright 2026 The Android Open Source Project
// SPDX-License-Identifier: Apache-2.0
#include "shared_ahb_allocation.h"
#include <algorithm>
#include <cerrno>
#include <cstring>
#include <inttypes.h>
#include <signal.h>
#include <stdio.h>
#include <unistd.h>
#include <vector>

namespace {
unsigned checks = 0;
void require(bool condition, const char* detail) {
    if (!condition) { fprintf(stderr, "FAIL %s\n", detail); _exit(1); }
    ++checks;
}
void timedOut(int) {
    constexpr char message[] = "FAIL shared-ahb paging watchdog\n";
    (void)write(STDERR_FILENO, message, sizeof(message) - 1);
    _exit(124);
}
void error(const char* message) { fprintf(stderr, "%s\n", message); }
uint8_t pattern(uint64_t position) { return uint8_t((position * 73) ^ (position >> 7) ^ 0xa6); }
}

int main(int argc, char** argv) {
    if (argc != 2 || (strcmp(argv[1], "rgba") && strcmp(argv[1], "bgra"))) return 2;
    const uint32_t fourcc = !strcmp(argv[1], "bgra") ? 0x34325241 : 0x34324241;
    signal(SIGALRM, timedOut);
    alarm(45);
    setvbuf(stdout, nullptr, _IONBF, 0);
    uint64_t expectedRead = 0, expectedWrite = 0, expectedFill = 0;
    for (const auto [width, height] : {std::pair{64u, 64u}, {513u, 257u}}) {
        SharedAllocationDescription d{};
        int dma = -1;
        void* holder = crosvm_shared_ahb_create_v2(width, height, fourcc, &d, &dma, error);
        require(holder && dma >= 0, "actual production holder");
        require(d.allocation_size > 4 && !(d.allocation_size & 3), "allocation extent");
        std::vector<uint8_t> data(65536), actual(65536);
        // Preserve every byte, including row/end padding, using full and final
        // partial chunks; deliberately use an unaligned offset in the next pass.
        for (uint64_t offset = 0; offset < d.allocation_size;) {
            const uint64_t length = std::min<uint64_t>(data.size(), d.allocation_size - offset);
            for (uint64_t pos = 0; pos < length; ++pos) data[pos] = pattern(offset + pos);
            require(crosvm_shared_ahb_write_bytes(holder, offset, data.data(), length) == 0,
                    "paging write and sync END");
            expectedWrite += length;
            offset += length;
        }
        for (uint64_t offset = 1; offset < d.allocation_size;) {
            const uint64_t length = std::min<uint64_t>(actual.size(), d.allocation_size - offset);
            require(crosvm_shared_ahb_read_bytes(holder, offset, actual.data(), length) == 0,
                    "unaligned paging read and sync END");
            expectedRead += length;
            for (uint64_t pos = 0; pos < length; ++pos)
                require(actual[pos] == pattern(offset + pos), "full backing roundtrip including padding");
            offset += length;
        }
        void* native = nullptr;
        AHardwareBuffer* buffer = crosvm_shared_ahb_get_buffer(holder);
        require(AHardwareBuffer_lock(buffer, AHARDWAREBUFFER_USAGE_CPU_READ_OFTEN,
                                     -1, nullptr, &native) == 0 && native,
                "native AHB reader after paging write");
        for (uint32_t y = 0; y < height; ++y) for (uint32_t x = 0; x < width * 4; ++x)
            require(static_cast<const uint8_t*>(native)[uint64_t(y) * d.plane_stride + x] ==
                    pattern(d.plane_offset + uint64_t(y) * d.plane_stride + x),
                    "native AHB sees same backing write");
        require(AHardwareBuffer_unlock(buffer, nullptr) == 0, "native reader retired");

        constexpr uint32_t fillPattern = 0xf13ca572;
        for (uint64_t offset = 0; offset < d.allocation_size;) {
            const uint64_t length = std::min<uint64_t>(65536, d.allocation_size - offset);
            require(crosvm_shared_ahb_fill_bytes(holder, offset, length, fillPattern) == 0,
                    "paging fill and sync END");
            expectedFill += length;
            offset += length;
        }
        for (uint64_t offset = 0; offset < d.allocation_size;) {
            const uint64_t length = std::min<uint64_t>(actual.size(), d.allocation_size - offset);
            require(crosvm_shared_ahb_read_bytes(holder, offset, actual.data(), length) == 0,
                    "filled backing read");
            expectedRead += length;
            for (uint64_t pos = 0; pos < length; ++pos)
                require(actual[pos] == uint8_t(fillPattern >> (8 * ((offset + pos) & 3))),
                        "fill little endian bytes including padding");
            offset += length;
        }
        const auto rejectRead = [&](uint64_t offset, uint64_t length) {
            require(crosvm_shared_ahb_read_bytes(holder, offset, actual.data(), length) == -EINVAL,
                    "invalid read rejected");
        };
        rejectRead(d.allocation_size, 1);
        rejectRead(UINT64_MAX, 8);
        rejectRead(0, 65537);
        rejectRead(0, 0);
        require(crosvm_shared_ahb_write_bytes(holder, d.allocation_size - 1, data.data(), 2) == -EINVAL,
                "crossing write rejected");
        require(crosvm_shared_ahb_write_bytes(holder, 0, nullptr, 1) == -EINVAL,
                "null write rejected");
        require(crosvm_shared_ahb_fill_bytes(holder, 1, 4, 0) == -EINVAL,
                "unaligned fill offset rejected");
        require(crosvm_shared_ahb_fill_bytes(holder, 0, 3, 0) == -EINVAL,
                "unaligned fill length rejected");
        require(crosvm_shared_ahb_read_bytes(nullptr, 0, actual.data(), 1) == -EINVAL,
                "null holder rejected");
        require(crosvm_shared_ahb_read_bytes(holder, 0, actual.data(), 4) == 0,
                "read after rejections");
        expectedRead += 4;
        require(actual[0] == 0x72 && actual[1] == 0xa5 && actual[2] == 0x3c && actual[3] == 0xf1,
                "rejected operations did not modify first pixel");
        printf("PAGING buffer_id=%" PRIu64 " size=%" PRIu64 " stride=%u width=%u height=%u fourcc=%08x\n",
               d.buffer_id, d.allocation_size, d.plane_stride, width, height, fourcc);
        close(dma);
        crosvm_shared_ahb_destroy(holder);
    }
    SharedAhbPagingCounters counters{};
    crosvm_shared_ahb_get_paging_counters(&counters);
    require(counters.read_bytes == expectedRead && counters.write_bytes == expectedWrite &&
            counters.fill_bytes == expectedFill, "actual copy and fill byte counters");
    alarm(0);
    printf("RESULT passed=1 assertions=%u paging_read_bytes=%" PRIu64
           " paging_write_bytes=%" PRIu64 " paging_fill_bytes=%" PRIu64
           " gpu_submit=0 guest_paging=0 present=0\n", checks,
           counters.read_bytes, counters.write_bytes, counters.fill_bytes);
    return 0;
}
