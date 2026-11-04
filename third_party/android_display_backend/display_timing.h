// Copyright 2026 The Android Open Source Project
// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <algorithm>
#include <array>
#include <atomic>
#include <cstdint>
#include <linux/sync_file.h>
#include <mutex>
#include <sys/ioctl.h>
#include <time.h>
#include <unistd.h>

namespace android_display {
// Stable C ABI, mirrored by gpu_display and the private virtio protocol.
// PRESENT identifies transaction timing, not proof of a particular buffer's
// panel scanout. Display binding and physical period are deliberately separate.
struct DisplayTimingSample {
    uint32_t version = 1, size = 80, flags = 0, reserved = 0;
    uint64_t attachmentEpoch = 0, modeEpoch = 0, sequence = 0;
    uint64_t hostNowNs = 0, presentNs = 0, latchNs = 0;
    uint64_t periodNs = 0, uncertaintyNs = 0;
};
static_assert(sizeof(DisplayTimingSample) == 80);
constexpr uint32_t kTimingPresent = 1;
constexpr uint32_t kTimingDisplayBound = 2;
constexpr uint32_t kTimingPeriodKnown = 4;

inline uint64_t displayMonotonicNs() {
    timespec ts{};
    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0 || ts.tv_sec < 0) return 0;
    return static_cast<uint64_t>(ts.tv_sec) * 1000000000ULL + ts.tv_nsec;
}

enum class PresentFenceStatus { Pending, Invalid, Signalled };

struct DisplayTimingDiagnostics {
    uint64_t completedCallbacks = 0, validSignals = 0, duplicatePresents = 0;
    uint64_t outOfOrder = 0, expired = 0, invalid = 0, admissionDrops = 0;
    uint64_t invalidatedCallbacks = 0, unavailableStats = 0, malformedCallbacks = 0;
    uint64_t latchAfterArrival = 0, invalidFence = 0, presentBeforeLatch = 0;
    uint64_t presentAfterObservation = 0, stalePresent = 0, invalidClockOrder = 0;
    uint64_t oldSequence = 0, presentRegression = 0, sortedBatches = 0;
    uint64_t clockErrors = 0, signalsAfterSnapshotStart = 0, promotedSamples = 0;
    uint64_t pendingFences = 0, outstandingCallbacks = 0;
    uint64_t lastEnqueuedNs = 0, lastLatchNs = 0, lastPresentNs = 0;
    uint64_t maxEnqueueToLatchNs = 0, maxLatchToPresentNs = 0;
};

// No poll/wait and no heap allocation. Kernel dma_fence documents timestamp_ns
// in CLOCK_MONOTONIC. Like AOSP Fence::getSignalTime, choose the latest merged
// component. Unusually large/errored/empty sync files are unsupported evidence.
inline PresentFenceStatus readPresentFence(int fd, uint64_t* timestampNs) {
    if (fd < 0 || !timestampNs) return PresentFenceStatus::Invalid;
    std::array<sync_fence_info, 16> fences{};
    sync_file_info info{};
    info.num_fences = fences.size();
    info.sync_fence_info = reinterpret_cast<uintptr_t>(fences.data());
    if (ioctl(fd, SYNC_IOC_FILE_INFO, &info) != 0 || info.status < 0 ||
        !info.num_fences || info.num_fences > fences.size()) return PresentFenceStatus::Invalid;
    if (info.status == 0) return PresentFenceStatus::Pending;
    if (info.status != 1) return PresentFenceStatus::Invalid;
    uint64_t latest = 0;
    for (uint32_t i = 0; i < info.num_fences; ++i) {
        if (fences[i].status != 1 || !fences[i].timestamp_ns) return PresentFenceStatus::Invalid;
        latest = std::max(latest, static_cast<uint64_t>(fences[i].timestamp_ns));
    }
    *timestampNs = latest;
    return PresentFenceStatus::Signalled;
}

// One state per exact attachment/mode. It owns only present-fence descriptors,
// never AHBs or SurfaceControls. Outstanding callbacks hold this state, and are
// harmless after invalidate(). Sampling and fd ownership stay bounded even if
// the compositor, consumer, or guest stops making progress.
class DisplayTimingState {
public:
    using FenceReader = PresentFenceStatus (*)(int, uint64_t*);
    static constexpr uint64_t kMaxAgeNs = 250000000;
    DisplayTimingState(uint64_t attachment, uint64_t mode)
        : mAttachment(attachment), mMode(mode) {}
    ~DisplayTimingState() { invalidate(); }

    bool reserveCallback() {
        std::lock_guard lock(mMutex);
        size_t count = mCallbacks;
        for (const auto& pending : mPending) count += pending.fd >= 0;
        if (!mActive || count >= mPending.size()) return false;
        // Late callbacks can outlive many mode/attachment states. Bound
        // those retained contexts across states as well as within this one.
        if (sLiveCallbacks.fetch_add(1, std::memory_order_relaxed) >= 4) {
            sLiveCallbacks.fetch_sub(1, std::memory_order_relaxed);
            return false;
        }
        ++mCallbacks;
        return true;
    }

    void noteAdmissionDrop() {
        std::lock_guard lock(mMutex);
        increment(mDiagnostics.admissionDrops);
    }

    void invalidate() {
        std::lock_guard lock(mMutex);
        mActive = false;
        for (auto& pending : mPending) closePending(pending);
        mLatest = {};
    }

    // Takes fd ownership in every case. Callback arrival is only an expiry
    // bound; it must never be published as a presentation timestamp.
    void receive(uint64_t sequence, uint64_t latchNs, int fd, uint64_t arrivalNs, uint64_t enqueuedNs = 0) {
        std::lock_guard lock(mMutex);
        if (mCallbacks) {
            --mCallbacks;
            sLiveCallbacks.fetch_sub(1, std::memory_order_relaxed);
        }
        increment(mDiagnostics.completedCallbacks);
        uint64_t* invalidReason = nullptr;
        if (!mActive) invalidReason = &mDiagnostics.invalidatedCallbacks;
        else if (fd < 0 || !latchNs) invalidReason = &mDiagnostics.unavailableStats;
        else if (!sequence || !arrivalNs) invalidReason = &mDiagnostics.malformedCallbacks;
        else if (latchNs > arrivalNs) invalidReason = &mDiagnostics.latchAfterArrival;
        if (invalidReason) {
            reject(*invalidReason);
            if (fd >= 0) close(fd);
            return;
        }
        for (auto& pending : mPending) {
            if (pending.fd >= 0 && arrivalNs >= pending.arrivalNs &&
                arrivalNs - pending.arrivalNs > kMaxAgeNs) {
                increment(mDiagnostics.expired);
                closePending(pending);
            }
            if (pending.fd < 0) {
                pending = {fd, sequence, latchNs, arrivalNs, enqueuedNs};
                return;
            }
        }
        increment(mDiagnostics.admissionDrops);
        close(fd); // At most four retained fds; timing cannot block presentation.
    }

    using ClockReader = uint64_t (*)();
    DisplayTimingSample snapshot(uint64_t nowNs, FenceReader reader = readPresentFence,
                                 ClockReader clock = displayMonotonicNs) {
        std::lock_guard lock(mMutex);
        DisplayTimingSample result{};
        result.attachmentEpoch = mAttachment;
        result.modeEpoch = mMode;
        if (!mActive || !nowNs) return result;
        // The caller's time precedes this lock and the ioctls. A pending fence
        // can legitimately signal during either interval. Bracket inspection
        // with the same monotonic clock and validate against its end, never a
        // tolerance or a timestamp substituted for the actual fence signal.
        const uint64_t queryStartNs = nowNs;
        const uint64_t inspectionStartNs = clock();
        if (!inspectionStartNs || inspectionStartNs < queryStartNs) {
            increment(mDiagnostics.clockErrors);
            return result; // Keep bounded pending fds for a later valid clock.
        }
        struct Observed { Pending* pending; PresentFenceStatus status; uint64_t timestamp; };
        std::array<Observed, 4> observed{};
        size_t observedCount = 0;
        for (auto& pending : mPending) {
            if (pending.fd < 0) continue;
            if (inspectionStartNs < pending.arrivalNs) {
                reject(mDiagnostics.invalidClockOrder);
                closePending(pending);
                continue;
            }
            if (inspectionStartNs - pending.arrivalNs > kMaxAgeNs) {
                increment(mDiagnostics.expired);
                closePending(pending);
                continue;
            }
            uint64_t timestamp = 0;
            const auto status = reader(pending.fd, &timestamp);
            observed[observedCount++] = {&pending, status, timestamp};
        }
        const uint64_t inspectionEndNs = clock();
        if (!inspectionEndNs || inspectionEndNs < inspectionStartNs) {
            increment(mDiagnostics.clockErrors);
            return result; // No publication or fd retirement on a bad bracket.
        }
        nowNs = inspectionEndNs;
        struct Ready { uint64_t sequence, latchNs, enqueuedNs, timestamp; };
        std::array<Ready, 4> ready{};
        size_t readyCount = 0;
        for (size_t i = 0; i < observedCount; ++i) {
            auto& pending = *observed[i].pending;
            const auto status = observed[i].status;
            const uint64_t timestamp = observed[i].timestamp;
            // This is observation retention age, not GPU fence latency. The
            // collector may have been idle since the callback arrived.
            if (nowNs - pending.arrivalNs > kMaxAgeNs) {
                increment(mDiagnostics.expired);
                closePending(pending);
                continue;
            }
            if (status == PresentFenceStatus::Pending) continue;
            if (status != PresentFenceStatus::Signalled) reject(mDiagnostics.invalidFence);
            else if (timestamp < pending.latchNs) reject(mDiagnostics.presentBeforeLatch);
            else if (timestamp > nowNs) reject(mDiagnostics.presentAfterObservation);
            else if (nowNs - timestamp > kMaxAgeNs) reject(mDiagnostics.stalePresent);
            else {
                increment(mDiagnostics.validSignals);
                if (timestamp > queryStartNs) increment(mDiagnostics.signalsAfterSnapshotStart);
                ready[readyCount++] = {pending.sequence, pending.latchNs, pending.enqueuedNs, timestamp};
            }
            closePending(pending);
        }
        // Slots are reused, so their index is not producer order. Sort this
        // bounded ready batch before applying monotonic sample publication.
        const auto bySequence = [](const Ready& a, const Ready& b) { return a.sequence < b.sequence; };
        if (!std::is_sorted(ready.begin(), ready.begin() + readyCount, bySequence))
            increment(mDiagnostics.sortedBatches);
        std::sort(ready.begin(), ready.begin() + readyCount, bySequence);
        for (size_t i = 0; i < readyCount; ++i) {
            const auto& item = ready[i];
            if (item.timestamp == mLatest.presentNs) increment(mDiagnostics.duplicatePresents);
            if (item.sequence <= mLatest.sequence) {
                increment(mDiagnostics.oldSequence);
                increment(mDiagnostics.outOfOrder);
            } else if (item.timestamp < mLatest.presentNs) {
                increment(mDiagnostics.presentRegression);
                increment(mDiagnostics.outOfOrder);
            }
            if (item.enqueuedNs && item.enqueuedNs <= item.latchNs) {
                mDiagnostics.maxEnqueueToLatchNs = std::max(mDiagnostics.maxEnqueueToLatchNs,
                        item.latchNs - item.enqueuedNs);
            }
            mDiagnostics.maxLatchToPresentNs = std::max(mDiagnostics.maxLatchToPresentNs,
                    item.timestamp - item.latchNs);
            if (item.sequence > mLatest.sequence && item.timestamp > mLatest.presentNs) {
                increment(mDiagnostics.promotedSamples);
                mLatest.sequence = item.sequence;
                mLatest.presentNs = item.timestamp;
                mLatest.latchNs = item.latchNs;
                mLatest.flags = kTimingPresent;
                // Unknown accuracy/binding must not permit phase locking.
                mLatest.uncertaintyNs = UINT64_MAX;
                mDiagnostics.lastEnqueuedNs = item.enqueuedNs;
                mDiagnostics.lastLatchNs = item.latchNs;
                mDiagnostics.lastPresentNs = item.timestamp;
            }
        }
        if (mLatest.presentNs && nowNs >= mLatest.presentNs &&
            nowNs - mLatest.presentNs <= kMaxAgeNs) {
            result.flags = mLatest.flags;
            result.sequence = mLatest.sequence;
            result.presentNs = mLatest.presentNs;
            result.latchNs = mLatest.latchNs;
            result.uncertaintyNs = mLatest.uncertaintyNs;
        }
        result.hostNowNs = nowNs;
        return result;
    }

    DisplayTimingDiagnostics diagnostics() {
        std::lock_guard lock(mMutex);
        auto result = mDiagnostics;
        result.outstandingCallbacks = mCallbacks;
        for (const auto& pending : mPending) result.pendingFences += pending.fd >= 0;
        return result;
    }

private:
    static void increment(uint64_t& counter) { if (counter != UINT64_MAX) ++counter; }
    void reject(uint64_t& reason) { increment(mDiagnostics.invalid); increment(reason); }
    struct Pending { int fd = -1; uint64_t sequence = 0, latchNs = 0, arrivalNs = 0, enqueuedNs = 0; };
    static void closePending(Pending& pending) {
        if (pending.fd >= 0) close(pending.fd);
        pending = {};
    }
    std::mutex mMutex;
    const uint64_t mAttachment, mMode;
    bool mActive = true;
    size_t mCallbacks = 0;
    inline static std::atomic<size_t> sLiveCallbacks{0};
    std::array<Pending, 4> mPending{};
    DisplayTimingSample mLatest{};
    DisplayTimingDiagnostics mDiagnostics{};
};
} // namespace android_display
