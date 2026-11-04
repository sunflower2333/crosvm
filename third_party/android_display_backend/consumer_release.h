// Copyright 2026 The Android Open Source Project
// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <linux/sync_file.h>
#include <poll.h>
#include <sys/eventfd.h>
#include <sys/ioctl.h>
#include <fcntl.h>
#include <unistd.h>

#include <algorithm>
#include <atomic>
#include <cerrno>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <memory>
#include <mutex>
#include <functional>
#include <thread>
#include <vector>

namespace android_display {

// CROSVM_NATIVE_AHB_TRACE=1: one trace_marker line per ticket transition, so a
// host trace shows whether a release never arrived or was lost after arrival.
inline void traceRelease(const char* event, uint64_t generation, uint64_t sequence, int value) {
    static const int fd = [] {
        const char* on = getenv("CROSVM_NATIVE_AHB_TRACE");
        if (!on || on[0] != '1') return -1;
        int marker = open("/sys/kernel/tracing/trace_marker", O_WRONLY | O_CLOEXEC);
        return marker >= 0 ? marker : open("/sys/kernel/debug/tracing/trace_marker", O_WRONLY | O_CLOEXEC);
    }();
    if (fd < 0) return;
    char line[128];
    const int length = snprintf(line, sizeof(line), "NAHBC %s g=%llu s=%llu v=%d", event,
                                (unsigned long long)generation, (unsigned long long)sequence, value);
    if (length > 0) (void)!write(fd, line, std::min<size_t>(length, sizeof(line) - 1));
}

enum class ReleaseStatus { CallbackPending, FencePending, Released, Quarantined };

// Only a real, successful sync_file signal authorizes reuse. poll(POLLIN) also
// accepts ordinary readable fds and error fences, so is insufficient by itself.
inline int consumerFenceStatus(int fd) {
    sync_file_info info{};
    int result;
    do { result = ioctl(fd, SYNC_IOC_FILE_INFO, &info); } while (result < 0 && errno == EINTR);
    if (result < 0 || info.status < 0) return -1;
    return info.status > 0 ? 1 : 0;
}

class ConsumerRelease {
public:
    const uint64_t generation;
    const uint64_t sequence;
    ReleaseStatus status() const { return mStatus.load(std::memory_order_acquire); }
    bool reusable() const { return status() == ReleaseStatus::Released; }
    int callbackCount() const { return mCallbackCount.load(std::memory_order_acquire); }
    int callbackFence() const { return mCallbackFence.load(std::memory_order_acquire); }

    // Installs a one-shot notification used by the direct-display FFI to
    // export a waitable release fd. The callback is run at most once, and is
    // also run immediately when the ticket was already released.
    void setReleasedCallback(std::function<void()> callback) {
        bool invoke = false;
        {
            std::lock_guard lock(mMutex);
            if (status() == ReleaseStatus::Released) {
                invoke = true;
            } else if (mOnReleased) {
                return;
            } else {
                mOnReleased = std::move(callback);
            }
        }
        if (invoke && callback) callback();
        traceRelease("installed", generation, sequence, invoke ? 1 : 0);
    }

    // Takes ownership of fd, even on failure. Called only by a genuine release
    // callback (or the completion of the transaction replacing this buffer).
    void receiveFence(int fd) {
        std::lock_guard lock(mMutex);
        traceRelease("callback", generation, sequence, fd < 0 ? fd : static_cast<int>(status()) * 1000);
        mCallbackCount.fetch_add(1, std::memory_order_release);
        if (status() != ReleaseStatus::CallbackPending) {
            if (fd >= 0) close(fd);
            return;
        }
        mCallbackFence.store(fd, std::memory_order_release);
        if (fd == -1 && mProducerFence < 0) {
            mOwner.reset();
            mStatus.store(ReleaseStatus::Released, std::memory_order_release);
            traceRelease("released", generation, sequence, mOnReleased ? 1 : 0);
            if (mOnReleased) {
                auto callback = std::move(mOnReleased);
                callback();
            }
        } else if (fd >= -1) {
            mFence = fd;
            mStatus.store(ReleaseStatus::FencePending, std::memory_order_release);
            // SurfaceFlinger often delivers a fence that has already signalled.
            // Release it now; otherwise the monitor must wait on this fence.
            advanceLocked();
            if (status() == ReleaseStatus::FencePending) wakeMonitor();
        } else {
            if (mProducerFence >= 0) close(mProducerFence);
            mProducerFence = -1;
            mStatus.store(ReleaseStatus::Quarantined, std::memory_order_release);
            traceRelease("quarantine", generation, sequence, fd);
        }
    }

    // Only for the legacy previous-release-fence path. The API forbids using
    // OnComplete to release buffers registered with setBufferWithRelease.
    // Missing transaction stats cannot establish a release.
    void receiveDetachFence(int fd) {
        if (fd == -2) return;
        receiveFence(fd);
    }

private:
    friend class ConsumerReleaseMonitor;
    ConsumerRelease(std::shared_ptr<void> owner, uint64_t gen, uint64_t seq, int producerFence,
                    int (*fenceStatus)(int), int wakeFd)
        : generation(gen), sequence(seq), mIdentity(owner.get()), mOwner(std::move(owner)),
          mProducerFence(producerFence), mFenceStatus(fenceStatus), mWakeFd(wakeFd) {}

    void advance() {
        std::lock_guard lock(mMutex);
        advanceLocked();
    }

    // Duplicates the fences this ticket still waits on, for the monitor to
    // poll without holding either lock. Only the fence's own status, read
    // afterwards, decides release; poll merely wakes the monitor.
    void appendPendingFences(std::vector<int>& fds) {
        std::lock_guard lock(mMutex);
        if (status() != ReleaseStatus::FencePending) return;
        for (int fence : {mProducerFence, mFence}) {
            if (fence < 0) continue;
            const int copy = fcntl(fence, F_DUPFD_CLOEXEC, 0);
            if (copy >= 0) fds.push_back(copy);
        }
    }

    void wakeMonitor() const {
        if (mWakeFd < 0) return;
        const uint64_t one = 1;
        ssize_t result;
        do { result = write(mWakeFd, &one, sizeof(one)); } while (result < 0 && errno == EINTR);
    }

    void advanceLocked() {
        if (status() != ReleaseStatus::FencePending) return;
        bool pending = false;
        bool failed = false;
        // A consumer can discard a buffer without waiting for its acquire fence.
        // Reuse must independently wait for our producer writes as well.
        for (int* fence : {&mProducerFence, &mFence}) {
            if (*fence < 0) continue;
            const int result = mFenceStatus(*fence);
            if (result == 0) { pending = true; continue; }
            close(*fence);
            *fence = -1;
            failed |= result < 0;
        }
        if (failed) {
            if (mProducerFence >= 0) close(mProducerFence);
            if (mFence >= 0) close(mFence);
            mProducerFence = mFence = -1;
            mStatus.store(ReleaseStatus::Quarantined, std::memory_order_release);
            traceRelease("quarantine", generation, sequence, 1);
        } else if (!pending) {
            mOwner.reset();
            mStatus.store(ReleaseStatus::Released, std::memory_order_release);
            traceRelease("released", generation, sequence, mOnReleased ? 1 : 0);
            if (mOnReleased) {
                auto callback = std::move(mOnReleased);
                callback();
            }
        }
    }

    std::atomic<ReleaseStatus> mStatus{ReleaseStatus::CallbackPending};
    std::atomic<int> mCallbackCount{0};
    std::atomic<int> mCallbackFence{-3};
    void* const mIdentity;
    std::mutex mMutex;
    std::shared_ptr<void> mOwner;
    int mFence = -1;
    int mProducerFence = -1;
    int (*const mFenceStatus)(int);
    const int mWakeFd;
    std::function<void()> mOnReleased;
};

// One process-lifetime worker retains even detached generations and missing/error
// callbacks. It never captures a presenter or AndroidDisplaySurface pointer.
// The bounded registry fails closed instead of leaking unbounded new generations.
class ConsumerReleaseMonitor {
public:
    static ConsumerReleaseMonitor& get() {
        static auto* monitor = new ConsumerReleaseMonitor(consumerFenceStatus);
        return *monitor;
    }

    std::shared_ptr<ConsumerRelease> track(std::shared_ptr<void> owner,
                                         uint64_t generation, uint64_t sequence,
                                         int producerFence = -1) {
        if (producerFence < -1) return nullptr;
        std::lock_guard lock(mMutex);
        sweepLocked();
        if (mPending.size() >= 64) return nullptr;
        // One host-owned AHB object must never be resubmitted by a new Surface
        // generation while an older consumer still owns it.
        for (const auto& pending : mPending) {
            if (pending->mIdentity == owner.get()) return nullptr;
        }
        const int producer = producerFence < 0 ? -1 : fcntl(producerFence, F_DUPFD_CLOEXEC, 0);
        if (producerFence >= 0 && producer < 0) return nullptr;
        auto ticket = std::shared_ptr<ConsumerRelease>(new ConsumerRelease(
                std::move(owner), generation, sequence, producer, mFenceStatus, mWakeFd));
        mPending.push_back(ticket);
        traceRelease("track", generation, sequence, static_cast<int>(mPending.size()));
        ticket->wakeMonitor();
        return ticket;
    }

#ifdef CONSUMER_RELEASE_TESTING
    // Test-only constructor substitutes kernel fence observations, never the
    // production callback/ownership state machine. Android builds omit this API.
    static ConsumerReleaseMonitor& testMonitor(int (*fenceStatus)(int)) {
        return *new ConsumerReleaseMonitor(fenceStatus);
    }
#endif

private:
    // A release gates the guest's next render into this buffer, so it is
    // observed when the fence signals rather than on a timer: a 5 ms poll cost
    // most of a 165 Hz frame. The timeout only bounds a missed wakeup.
    explicit ConsumerReleaseMonitor(int (*fenceStatus)(int))
        : mFenceStatus(fenceStatus), mWakeFd(eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK)) {
        std::thread([this] {
            std::vector<int> fences;
            std::vector<pollfd> fds;
            for (;;) {
                bool idle;
                {
                    std::lock_guard lock(mMutex);
                    sweepLocked();
                    idle = mPending.empty();
                    for (auto& ticket : mPending) ticket->appendPendingFences(fences);
                }
                fds.clear();
                fds.push_back({mWakeFd, POLLIN, 0});
                for (int fence : fences) fds.push_back({fence, POLLIN, 0});
                const int timeout = mWakeFd < 0 ? 1 : idle ? -1 : 100;
                int result;
                do { result = poll(fds.data(), fds.size(), timeout); } while (result < 0 && errno == EINTR);
                for (int fence : fences) close(fence);
                fences.clear();
                uint64_t count;
                if (mWakeFd >= 0 && (fds[0].revents & POLLIN) && read(mWakeFd, &count, sizeof(count)) < 0) {
                    // EAGAIN: another wakeup already drained the counter.
                }
            }
        }).detach();
    }

    void sweepLocked() {
        for (auto& ticket : mPending) ticket->advance();
        mPending.erase(std::remove_if(mPending.begin(), mPending.end(),
                                     [](const auto& t) { return t->reusable(); }), mPending.end());
    }
    int (*const mFenceStatus)(int);
    const int mWakeFd;
    std::mutex mMutex;
    std::vector<std::shared_ptr<ConsumerRelease>> mPending;
};

} // namespace android_display
