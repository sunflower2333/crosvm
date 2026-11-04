// Copyright 2026 The Android Open Source Project
// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <atomic>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <fcntl.h>
#include <mutex>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

namespace android_display {

// Diagnostic identities only. Never participates in buffer ownership, fencing,
// transaction order, admission or guest completion. Copied across async apply.
struct NativeFifoTraceContext {
    uint64_t id = 0, guestSequence = 0, fence = 0, operationSequence = 0;
    uint64_t generation = 0, ticket = 0, bufferId = 0, nativeBufferId = 0;
    uint32_t command = 0, resource = 0, context = 0, descriptor = 0;
    uint32_t flags = 0, ring = 0;
};

struct NativeFifoTraceIo {
    uint64_t (*now)();
    bool (*armed)();
    int (*openMarker)();
    ssize_t (*writeMarker)(int, const void*, size_t);
    int (*pid)();
    long (*tid)();
};

inline uint64_t nativeFifoTraceNow() {
    timespec ts{};
    if (clock_gettime(CLOCK_MONOTONIC, &ts) || ts.tv_sec < 0) return 0;
    return uint64_t(ts.tv_sec) * 1000000000ULL + ts.tv_nsec;
}
inline bool nativeFifoTraceArmed() {
    return access("/data/local/tmp/crosvm-native-fifo-trace.arm", F_OK) == 0;
}
inline int nativeFifoTraceOpen() {
    int fd = open("/sys/kernel/tracing/trace_marker", O_WRONLY | O_CLOEXEC | O_NONBLOCK);
    return fd >= 0 ? fd : open("/sys/kernel/debug/tracing/trace_marker",
                              O_WRONLY | O_CLOEXEC | O_NONBLOCK);
}

// One finite, explicitly armed window per process: at most4096 packets,
// 65536 events plus one terminal record, and10 monotonic seconds. Enabled
// polling of the arm file is limited to once per100ms. Unset/!=1: one cached
// flag branch; no per-frame clock, arm-file read, formatting or marker write.
class NativeFifoTraceState {
public:
    NativeFifoTraceState(bool enabled, NativeFifoTraceIo io,
                         uint64_t packetLimit = 4096, uint64_t eventLimit = 65536,
                         uint64_t durationNs = 10000000000ULL)
        : mEnabled(enabled), mIo(io), mPacketLimit(packetLimit),
          mEventLimit(eventLimit), mDurationNs(durationNs) {}

    bool enabled() const { return mEnabled && !mStopped.load(std::memory_order_relaxed); }

    NativeFifoTraceContext begin(NativeFifoTraceContext context) {
        if (!enabled()) return {};
        std::lock_guard lock(mMutex);
        if (mStopped.load(std::memory_order_relaxed)) return {};
        const uint64_t now = mIo.now();
        if (!now) {
            if (mArmed) stopLocked(now, 3);
            return {};
        }
        if (!mArmed) {
            if (now < mNextArmPoll) return {};
            mNextArmPoll = now > UINT64_MAX - 100000000ULL ? UINT64_MAX : now + 100000000ULL;
            if (!mIo.armed()) return {};
            mFd = mIo.openMarker();
            if (mFd < 0) { mStopped.store(true, std::memory_order_relaxed); return {}; }
            mPid = mIo.pid();
            mStartNs = now;
            mArmed = true;
            writeLocked("armed", {}, now, 0);
        }
        if (now < mStartNs || now - mStartNs >= mDurationNs) {
            stopLocked(now, 3); return {};
        }
        if (mPackets >= mPacketLimit) { stopLocked(now, 1); return {}; }
        if (mEvents >= mEventLimit) { stopLocked(now, 2); return {}; }
        context.id = ++mPackets;
        writeLocked("packet", context, now, 0);
        return context;
    }

    void event(const char* stage, const NativeFifoTraceContext& context, uint64_t value = 0) {
        if (!context.id || !enabled()) return;
        std::lock_guard lock(mMutex);
        if (mStopped.load(std::memory_order_relaxed)) return;
        const uint64_t now = mIo.now();
        if (!now || now < mStartNs || now - mStartNs >= mDurationNs) {
            stopLocked(now, 3); return;
        }
        if (mEvents >= mEventLimit) { stopLocked(now, 2); return; }
        writeLocked(stage, context, now, value);
    }

private:
    void stopLocked(uint64_t now, uint64_t reason) {
        if (mStopped.exchange(true, std::memory_order_relaxed)) return;
        // Explicit truncation reason:1 packets,2 events,3 duration/clock.
        writeLocked("limit", {}, now, reason);
    }
    void writeLocked(const char* stage, const NativeFifoTraceContext& c,
                     uint64_t now, uint64_t value) {
        // Proper atrace instant framing gives TraceProcessor a thread slice.
        // Existing plain NAHB/NAHBC markers are not parsed as slices.
        char line[768];
        const int n = snprintf(line, sizeof(line),
            "I|%d|NFIFO1 st=%s e=%llu id=%llu ns=%llu tid=%ld cmd=%u r=%u "
            "gs=%llu op=%llu f=%llu ctx=%u ring=%u fl=%u d=%u g=%llu s=%llu "
            "bid=%llu nbid=%llu v=%llu lost=%llu\n", mPid, stage,
            (unsigned long long)++mEvents, (unsigned long long)c.id,
            (unsigned long long)now, mIo.tid(), c.command, c.resource,
            (unsigned long long)c.guestSequence, (unsigned long long)c.operationSequence,
            (unsigned long long)c.fence, c.context, c.ring, c.flags, c.descriptor,
            (unsigned long long)c.generation, (unsigned long long)c.ticket,
            (unsigned long long)c.bufferId, (unsigned long long)c.nativeBufferId,
            (unsigned long long)value, (unsigned long long)mLost);
        // No truncated marker, retry, wait or log fallback. A short/failed
        // write is accounted in the next marker; it never changes rendering.
        if (n <= 0 || size_t(n) >= sizeof(line) ||
                mIo.writeMarker(mFd, line, size_t(n)) != n) ++mLost;
    }
    const bool mEnabled;
    const NativeFifoTraceIo mIo;
    const uint64_t mPacketLimit, mEventLimit, mDurationNs;
    std::atomic<bool> mStopped{false};
    std::mutex mMutex;
    bool mArmed = false;
    int mFd = -1, mPid = 0;
    uint64_t mNextArmPoll = 0, mStartNs = 0, mPackets = 0, mEvents = 0, mLost = 0;
};

#ifdef NATIVE_FIFO_TRACE_TESTING
inline NativeFifoTraceState* nativeFifoTraceTestState = nullptr;
#endif
inline NativeFifoTraceState& nativeFifoTraceState() {
#ifdef NATIVE_FIFO_TRACE_TESTING
    if (nativeFifoTraceTestState) return *nativeFifoTraceTestState;
#endif
    static auto* state = new NativeFifoTraceState([] {
        const char* value = getenv("CROSVM_NATIVE_FIFO_TRACE");
        return value && value[0] == '1' && value[1] == '\0';
    }(), {nativeFifoTraceNow, nativeFifoTraceArmed, nativeFifoTraceOpen,
          write, getpid, [] { return syscall(SYS_gettid); }});
    return *state;
}
inline thread_local NativeFifoTraceContext nativeFifoTraceCurrent;

inline uint64_t nativeFifoTraceBegin(uint32_t command, uint32_t resource, uint64_t guestSequence,
        uint64_t fence, uint32_t context, uint32_t ring, uint32_t flags, uint32_t descriptor) {
    // An accidental nested scope cannot steal the outer packet's identity.
    if (nativeFifoTraceCurrent.id) return 0;
    NativeFifoTraceContext trace{};
    trace.command = command; trace.resource = resource; trace.guestSequence = guestSequence;
    trace.fence = fence; trace.context = context; trace.ring = ring;
    trace.flags = flags; trace.descriptor = descriptor;
    nativeFifoTraceCurrent = nativeFifoTraceState().begin(trace);
    return nativeFifoTraceCurrent.id;
}
inline void nativeFifoTraceEnd(uint64_t id) {
    if (!id || nativeFifoTraceCurrent.id != id) return;
    nativeFifoTraceState().event("packet_end", nativeFifoTraceCurrent);
    nativeFifoTraceCurrent = {};
}
inline void nativeFifoTraceOperation(uint64_t sequence) {
    if (!nativeFifoTraceCurrent.id) return;
    nativeFifoTraceCurrent.operationSequence = sequence;
    nativeFifoTraceState().event("operation", nativeFifoTraceCurrent);
}
inline void nativeFifoTraceBuffer(uint64_t bufferId, uint64_t nativeBufferId) {
    if (!nativeFifoTraceCurrent.id) return;
    nativeFifoTraceCurrent.bufferId = bufferId;
    nativeFifoTraceCurrent.nativeBufferId = nativeBufferId;
    nativeFifoTraceState().event("bridge", nativeFifoTraceCurrent);
}
inline NativeFifoTraceContext nativeFifoTraceTicket(uint64_t generation, uint64_t sequence) {
    if (!nativeFifoTraceCurrent.id) return {};
    auto trace = nativeFifoTraceCurrent;
    trace.generation = generation; trace.ticket = sequence;
    nativeFifoTraceState().event("admit", trace);
    return trace;
}
inline void nativeFifoTraceEvent(const char* stage, const NativeFifoTraceContext& trace,
                                 uint64_t value = 0) {
    if (trace.id) nativeFifoTraceState().event(stage, trace, value);
}
} // namespace android_display
