// Copyright 2026 The Android Open Source Project
// SPDX-License-Identifier: Apache-2.0
#pragma once

#include "consumer_release.h"
#include "display_timing.h"
#include "native_fifo_trace.h"
#include "surface_control_dl.h"
#include "scanout_presentation_geometry.h"

#include <android/hardware_buffer.h>
#include <android/native_window.h>
#include <fcntl.h>
#include <map>
#include <deque>
#include <future>
#include <optional>

namespace android_display {

// The public API guarantees ordering for transactions applied on the same
// thread. A mutex between Binder detach and GPU present threads is not that
// contract, so all our apply calls go through one process-lifetime executor.
class SurfaceTransactionExecutor {
public:
    static void apply(ASurfaceTransaction* transaction) {
        static auto* executor = instance();
        std::packaged_task<void()> task([transaction] {
            SurfaceControl::GetInstance().ASurfaceTransaction_apply(transaction);
        });
        auto completion = task.get_future();
        {
            std::lock_guard lock(executor->mMutex);
            executor->mTasks.push_back(std::move(task));
        }
        executor->mChanged.notify_one();
        completion.wait();
    }
    // Same order as apply(), without waiting: the executor applies and then
    // deletes the transaction. A present's caller needs neither the apply's
    // completion (it returns nothing) nor its latency, which SurfaceFlinger
    // occasionally stretches to several milliseconds -- the guest's flip is
    // waiting on this answer.
    static void applyAsync(ASurfaceTransaction* transaction,
                           std::shared_ptr<ASurfaceControl> retainedSurface = {},
                           NativeFifoTraceContext trace = {}) {
        static auto* executor = instance();
        std::packaged_task<void()> task([transaction, trace, retainedSurface = std::move(retainedSurface)]() mutable {
            auto& api = SurfaceControl::GetInstance();
            nativeFifoTraceEvent("apply_begin", trace, reinterpret_cast<uintptr_t>(transaction));
            api.ASurfaceTransaction_apply(transaction);
            nativeFifoTraceEvent("apply_end", trace, reinterpret_cast<uintptr_t>(transaction));
            api.ASurfaceTransaction_delete(transaction);
            retainedSurface.reset();
        });
        {
            std::lock_guard lock(executor->mMutex);
            executor->mTasks.push_back(std::move(task));
            nativeFifoTraceEvent("enqueue", trace, reinterpret_cast<uintptr_t>(transaction));
            // Record notification intent while the queue lock still prevents
            // dequeue. The unchanged notify_one below may wake a sleeping worker.
            nativeFifoTraceEvent("notify", trace, reinterpret_cast<uintptr_t>(transaction));
        }
        executor->mChanged.notify_one();
    }
private:
    static SurfaceTransactionExecutor* instance() {
        static auto* executor = new SurfaceTransactionExecutor;
        return executor;
    }
    SurfaceTransactionExecutor() {
        std::thread([this] {
            for (;;) {
                std::packaged_task<void()> task;
                {
                    std::unique_lock lock(mMutex);
                    mChanged.wait(lock, [this] { return !mTasks.empty(); });
                    task = std::move(mTasks.front());
                    mTasks.pop_front();
                }
                task();
            }
        }).detach();
    }
    std::mutex mMutex;
    std::condition_variable mChanged;
    std::deque<std::packaged_task<void()>> mTasks;
};

// Release prerequisite only: this does not allocate guest-shared storage or
// advertise zero-copy. A caller must obey the returned ticket before writing.
class SurfaceBufferPresenter {
public:
    static std::unique_ptr<SurfaceBufferPresenter> create(ANativeWindow* window,
            const char* name, uint64_t generation, bool forcePreviousFence = false, bool opaque = false) {
        auto& api = SurfaceControl::GetInstance();
        if (!window || !api.HasPreviousReleaseFence()) return nullptr;
        if (opaque && !api.HasBufferTransparency()) return nullptr;
        auto* surface = api.ASurfaceControl_createFromWindow(window, name);
        if (!surface) return nullptr;
        return std::unique_ptr<SurfaceBufferPresenter>(new SurfaceBufferPresenter(
                surface, generation, !forcePreviousFence && api.HasBufferReleaseCallback(),
                ANativeWindow_getWidth(window), ANativeWindow_getHeight(window), opaque));
    }

    ~SurfaceBufferPresenter() { detach(); }
    bool usesBufferReleaseCallback() const { return mBufferCallback; }

    // A configured tuple is immutable within a mode generation. The owner
    // must negotiate/bind that generation to each resource presentation before
    // using this entry point. Existing unconfigured callers remain identity.
    // Retiring a mode does not release its buffers; their real callbacks do.
    bool configureScanoutGeometry(const ScanoutPresentationGeometry& geometry) {
        std::lock_guard lock(mMutex);
        if (!mSurface || !geometry.valid() || !SurfaceControl::GetInstance().HasBufferGeometry() ||
                mWidth != static_cast<int32_t>(geometry.logicalWidth) ||
                mHeight != static_cast<int32_t>(geometry.logicalHeight)) return false;
        if (mGeometry) {
            if (geometry.resetGeneration != mGeometry->resetGeneration ||
                    geometry.modeGeneration < mGeometry->modeGeneration) return false;
            if (geometry.modeGeneration == mGeometry->modeGeneration)
                return geometry == *mGeometry;
        }
        mGeometry = geometry;
        return true;
    }

    void setTimingState(std::shared_ptr<DisplayTimingState> state) {
        std::lock_guard lock(mMutex);
        if (mTiming) mTiming->invalidate();
        mTiming = std::move(state);
    }

    // The direct AHB producer bypasses ANativeWindow's BufferQueue, so its
    // refresh preference must target this owned layer. Queue only changes,
    // including zero to clear a prior vote; a recreated layer starts at zero.
    // Keep the hint separate from setBuffer: vendor rejection of a preference
    // must not invalidate a buffer transaction or change its release contract.
    bool setFrameRate(uint32_t refreshRate) {
        std::lock_guard lock(mMutex);
        if (!mSurface) return false;
        if (mFrameRate == refreshRate) return true;
        auto& api = SurfaceControl::GetInstance();
        if (!api.HasFrameRate()) return false;
        auto* transaction = api.ASurfaceTransaction_create();
        if (!transaction) return false; // A later present may retry allocation.
        api.ASurfaceTransaction_setFrameRateWithChangeStrategy(
                transaction, mSurface.get(), static_cast<float>(refreshRate),
                ANATIVEWINDOW_FRAME_RATE_COMPATIBILITY_AT_LEAST,
                ANATIVEWINDOW_CHANGE_FRAME_RATE_ONLY_IF_SEAMLESS);
        SurfaceTransactionExecutor::applyAsync(transaction, mSurface);
        mFrameRate = refreshRate; // Submitted hint, not proof of panel selection.
        return true;
    }

    // Retiring a guest scanout removes its layer without destroying the
    // app-owned ANativeWindow. Tickets remain held by the release monitor.
    // Failed transaction creation retains the presenter for a later retry.
    static bool detachAndReset(std::unique_ptr<SurfaceBufferPresenter>& presenter) {
        if (!presenter) return true;
        if (!presenter->detach()) return false;
        presenter.reset();
        return true;
    }

    // Borrows acquireFd; duplicates it only after all admission checks. The
    // framework owns that duplicate, including when transactions are deleted.
    std::shared_ptr<ConsumerRelease> present(AHardwareBuffer* buffer, int acquireFd = -1,
            uint64_t expectedModeGeneration = 0, uint64_t expectedResetGeneration = 0) {
        std::lock_guard lock(mMutex);
        if (!mSurface || !buffer || acquireFd < -1) return nullptr;
        for (auto it = mBuffers.begin(); it != mBuffers.end();) {
            if (it->second->reusable()) it = mBuffers.erase(it); else ++it;
        }
        if (mBuffers.count(buffer) || mBuffers.size() >= 3) return nullptr;
        AHardwareBuffer_Desc desc{};
        AHardwareBuffer_describe(buffer, &desc);
        if (!(desc.usage & AHARDWAREBUFFER_USAGE_GPU_SAMPLED_IMAGE)) return nullptr;
        auto& api = SurfaceControl::GetInstance();
        if (mGeometry) {
            if (expectedModeGeneration != mGeometry->modeGeneration ||
                    expectedResetGeneration != mGeometry->resetGeneration ||
                    desc.width != mGeometry->storageWidth ||
                    desc.height != mGeometry->storageHeight || desc.layers != 1 ||
                    !api.HasBufferGeometry()) return nullptr;
        } else if (expectedModeGeneration || expectedResetGeneration) {
            return nullptr;
        }
        auto* transaction = api.ASurfaceTransaction_create();
        if (!transaction) return nullptr;
        const int ownedAcquire = acquireFd < 0 ? -1 : fcntl(acquireFd, F_DUPFD_CLOEXEC, 0);
        if (acquireFd >= 0 && ownedAcquire < 0) {
            api.ASurfaceTransaction_delete(transaction);
            return nullptr;
        }
        AHardwareBuffer_acquire(buffer);
        std::shared_ptr<void> owner(buffer, [](void* p) {
            AHardwareBuffer_release(static_cast<AHardwareBuffer*>(p));
        });
        auto ticket = ConsumerReleaseMonitor::get().track(owner, mGeneration, ++mSequence,
                                                         acquireFd);
        if (!ticket) {
            if (ownedAcquire >= 0) close(ownedAcquire);
            api.ASurfaceTransaction_delete(transaction);
            return nullptr;
        }
        mBuffers.emplace(buffer, ticket);
        const auto trace = nativeFifoTraceTicket(mGeneration, mSequence);
        // Native scanout bypasses BufferQueue's server-side backpressure.
        // Otherwise two ready buffers can be consumed in one SF commit, losing
        // a guest frame even when both producer and panel run at 165 Hz.
        // Configure each new layer in its first valid buffer transaction. The
        // existing three-ticket bound and real consumer fences still govern
        // admission/reuse; this flag never establishes buffer release.
        if (!mBackPressureEnabled && api.HasBackPressure()) {
            api.SetEnableBackPressure(transaction, mSurface.get(), true);
            mBackPressureEnabled = true;
        }
        // A completed scanout is an opaque primary, including XRGB sources
        // whose unused alpha bits must never make the Windows desktop vanish.
        if (mOpaque) api.SetBufferOpaque(transaction, mSurface.get());
        if (api.HasBufferGeometry() && mWidth > 0 && mHeight > 0) {
            const ARect source{0, 0, static_cast<int32_t>(desc.width), static_cast<int32_t>(desc.height)};
            const ARect destination{0, 0, mWidth, mHeight};
            int32_t transform = 0;
            if (mGeometry) {
                switch (mGeometry->contentRotation) {
                    case ScanoutContentRotation::Identity: break;
                    case ScanoutContentRotation::Clockwise90:
                        transform = ANATIVEWINDOW_TRANSFORM_ROTATE_270; break;
                    case ScanoutContentRotation::Clockwise180:
                        transform = ANATIVEWINDOW_TRANSFORM_ROTATE_180; break;
                    case ScanoutContentRotation::Clockwise270:
                        transform = ANATIVEWINDOW_TRANSFORM_ROTATE_90; break;
                }
            }
            api.ASurfaceTransaction_setGeometry(transaction, mSurface.get(), &source, &destination, transform);
        }
        if (mBufferCallback) {
            // The release callback must not own the SurfaceControl. A detached
            // layer is offscreen, so SurfaceFlinger releases its last buffer
            // only when the layer is destroyed; a callback holding the layer
            // alive until that release would pin the buffer forever (the guest
            // then waits on it: res 5 held after a Native Display reattach).
            auto* callback = new Callback{nullptr, ticket};
            api.ASurfaceTransaction_setBufferWithRelease(transaction, mSurface.get(), buffer,
                    ownedAcquire, callback, onBufferRelease);
        } else {
            api.ASurfaceTransaction_setBuffer(transaction, mSurface.get(), buffer, ownedAcquire);
        }
        setCompletionCallback(transaction, mBufferCallback ? nullptr : mCurrent, true);
        mCurrent = ticket;
        SurfaceTransactionExecutor::applyAsync(transaction, {}, trace);
        return ticket;
    }

    bool detach() {
        std::lock_guard lock(mMutex);
        if (!mSurface) return true;
        auto& api = SurfaceControl::GetInstance();
        auto* transaction = api.ASurfaceTransaction_create();
        if (!transaction) return false; // Outstanding tickets remain retained.
        // With setBufferWithRelease the per-buffer callback owns release, even
        // on detach. The API forbids releasing that buffer via OnComplete.
        if (!mBufferCallback && mCurrent) setCompletionCallback(transaction, mCurrent, false);
        // A released C++ handle alone may leave the layer on display. Remove
        // the buffer and parent in the same transaction, then await release.
        api.ASurfaceTransaction_setBuffer(transaction, mSurface.get(), nullptr, -1);
        api.ASurfaceTransaction_reparent(transaction, mSurface.get(), nullptr);
        SurfaceTransactionExecutor::apply(transaction);
        api.ASurfaceTransaction_delete(transaction);
        mCurrent.reset();
        if (mTiming) mTiming->invalidate();
        mSurface.reset();
        return true;
    }

    // Reset cannot wait for Binder/SurfaceFlinger while the guest transport
    // waits for its reply. Queue a real detach and keep its layer alive on the
    // executor until apply completes. Success means only that removal was
    // queued; ConsumerRelease remains the sole buffer-reuse authority.
    bool requestDetach() {
        std::unique_lock lock(mMutex, std::try_to_lock);
        if (!lock.owns_lock()) return false;
        if (!mSurface) return true;
        auto& api = SurfaceControl::GetInstance();
        auto* transaction = api.ASurfaceTransaction_create();
        if (!transaction) return false;
        if (!mBufferCallback && mCurrent) setCompletionCallback(transaction, mCurrent, false);
        api.ASurfaceTransaction_setBuffer(transaction, mSurface.get(), nullptr, -1);
        api.ASurfaceTransaction_reparent(transaction, mSurface.get(), nullptr);
        SurfaceTransactionExecutor::applyAsync(transaction, std::move(mSurface));
        mCurrent.reset();
        if (mTiming) mTiming->invalidate();
        return true;
    }

private:
    struct Callback {
        std::shared_ptr<ASurfaceControl> surface;
        std::shared_ptr<ConsumerRelease> ticket;
    };
    struct CompletionCallback {
        std::shared_ptr<ASurfaceControl> surface;
        std::shared_ptr<ConsumerRelease> previous;
        std::shared_ptr<DisplayTimingState> timing;
        uint64_t sequence;
        uint64_t enqueuedNs;
    };
    SurfaceBufferPresenter(ASurfaceControl* surface, uint64_t generation, bool bufferCallback,
                           int32_t width, int32_t height, bool opaque)
        : mSurface(surface, [](ASurfaceControl* p) {
            SurfaceControl::GetInstance().ASurfaceControl_release(p);
        }), mGeneration(generation), mBufferCallback(bufferCallback), mWidth(width), mHeight(height),
          mOpaque(opaque) {}

    static void onBufferRelease(void* opaque, int fence) {
        std::unique_ptr<Callback> callback(static_cast<Callback*>(opaque));
        callback->ticket->receiveFence(fence);
    }

    static void onComplete(void* opaque, ASurfaceTransactionStats* stats) {
        std::unique_ptr<CompletionCallback> callback(static_cast<CompletionCallback*>(opaque));
        auto& api = SurfaceControl::GetInstance();
        if (callback->previous) {
            int fence = -2;
            if (!api.PreviousReleaseFence(stats, callback->surface.get(), &fence)) {
                if (fence >= 0) close(fence);
                fence = -2; // Missing stats can never establish release.
            }
            callback->previous->receiveDetachFence(fence);
        }
        if (callback->timing) {
            int fence = -1;
            uint64_t latchNs = 0;
            (void)api.PresentTiming(stats, callback->surface.get(), &fence, &latchNs);
            callback->timing->receive(callback->sequence, latchNs, fence, displayMonotonicNs(), callback->enqueuedNs);
            // Bounded non-waiting status inspection lets the next callback
            // reuse timing capacity. Pending fences remain capped at four.
            (void)callback->timing->snapshot(displayMonotonicNs());
        }
    }

    void setCompletionCallback(ASurfaceTransaction* transaction,
                               const std::shared_ptr<ConsumerRelease>& previous, bool presented) {
        auto timing = presented && SurfaceControl::GetInstance().HasPresentTiming() ? mTiming : nullptr;
        if (timing && !timing->reserveCallback()) {
            // All slots may contain fences that signalled after their
            // OnComplete. Drain at most four status queries before declining
            // this observation; presentation itself must never wait.
            (void)timing->snapshot(displayMonotonicNs());
            if (!timing->reserveCallback()) {
                timing->noteAdmissionDrop();
                timing.reset();
            }
        }
        if (!previous && !timing) return;
        const uint64_t enqueuedNs = timing ? displayMonotonicNs() : 0;
        auto* callback = new CompletionCallback{mSurface, previous, std::move(timing), mSequence, enqueuedNs};
        SurfaceControl::GetInstance().ASurfaceTransaction_setOnComplete(transaction, callback,
                                                                       onComplete);
    }

    std::mutex mMutex;
    std::shared_ptr<ASurfaceControl> mSurface;
    const uint64_t mGeneration;
    const bool mBufferCallback;
    const int32_t mWidth;
    const int32_t mHeight;
    const bool mOpaque;
    bool mBackPressureEnabled = false;
    uint32_t mFrameRate = 0;
    std::optional<ScanoutPresentationGeometry> mGeometry;
    uint64_t mSequence = 0;
    std::shared_ptr<ConsumerRelease> mCurrent;
    std::shared_ptr<DisplayTimingState> mTiming;
    std::map<AHardwareBuffer*, std::shared_ptr<ConsumerRelease>> mBuffers;
};
} // namespace android_display
