// Copyright 2026 DroidVM contributors
// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <array>
#include <cerrno>
#include <condition_variable>
#include <cstdint>
#include <cstring>
#include <fcntl.h>
#include <limits>
#include <memory>
#include <mutex>
#include <optional>
#include <poll.h>
#include <thread>
#include <unistd.h>
#include <vector>

namespace android_display {

// Android native-window transform bits: identity=0, ROT_180=3, ROT_90=4,
// ROT_270=7. A root Surface hint is authoritative for the app-owned cursor,
// whose parent applies translation/scaling but no additional rotation.
class CursorTransformHint {
public:
    static bool supported(int hint) {
        return hint == 0 || hint == 3 || hint == 4 || hint == 7;
    }
    bool setAuthoritative(int hint) {
        if (!supported(hint)) return false;
        mRootHint = hint;
        return true;
    }
    int resolve(int queuedHint) const {
        return mRootHint.value_or(supported(queuedHint) ? queuedHint : 0);
    }
private:
    std::optional<int> mRootHint;
};

// Last complete cursor image, in guest orientation. The caller serializes
// configure/begin/publish/replay using its surface mutex. The producer may write
// the staging pixels outside that mutex: replay only reads the other buffer.
class CursorImageCache {
public:
    bool configure(uint32_t width, uint32_t height) {
        if (!width || !height || width > std::numeric_limits<size_t>::max() / height / 4)
            return false;
        if (mWidth == width && mHeight == height) return true;
        if (mWriting) return false;
        mStaging.resize(static_cast<size_t>(width) * height);
        mComplete.resize(static_cast<size_t>(width) * height);
        mWidth = width;
        mHeight = height;
        mValid = false;
        return true;
    }
    uint32_t* begin() {
        if (mWriting || mStaging.empty()) return nullptr;
        mWriting = true;
        return mStaging.data();
    }
    bool publish() {
        if (!mWriting) return false;
        mStaging.swap(mComplete); // Only descriptors swap; no pixel copy.
        mWriting = false;
        mValid = true;
        ++mRevision;
        return true;
    }
    bool valid() const { return mValid; }
    uint64_t revision() const { return mRevision; }
    uint32_t width() const { return mWidth; }
    uint32_t height() const { return mHeight; }
    const uint32_t* pixels() const { return mValid ? mComplete.data() : nullptr; }

    // Destination dimensions are checked by the ANativeWindow caller before
    // this copy. Rotation changes only the tiny cursor image, never scanout.
    bool copyTo(uint32_t* dst, uint32_t stride, unsigned quarterTurns) const {
        if (!mValid || !dst || stride < mWidth || quarterTurns > 3 ||
            (quarterTurns && mWidth != mHeight)) return false;
        for (uint32_t y = 0; y < mHeight; ++y) {
            if (!quarterTurns) {
                memcpy(dst + static_cast<size_t>(y) * stride,
                       mComplete.data() + static_cast<size_t>(y) * mWidth, mWidth * 4);
                continue;
            }
            for (uint32_t x = 0; x < mWidth; ++x) {
                uint32_t dx, dy;
                if (quarterTurns == 1) { dx = mWidth - 1 - y; dy = x; }
                else if (quarterTurns == 2) { dx = mWidth - 1 - x; dy = mHeight - 1 - y; }
                else { dx = y; dy = mHeight - 1 - x; }
                dst[static_cast<size_t>(dy) * stride + dx] =
                        mComplete[static_cast<size_t>(y) * mWidth + x];
            }
        }
        return true;
    }
private:
    uint32_t mWidth = 0, mHeight = 0;
    uint64_t mRevision = 0;
    bool mWriting = false, mValid = false;
    std::vector<uint32_t> mStaging, mComplete;
};

// A Binder reattach and the GPU worker must not race close(fd)/write(fd), and
// neither may wait for the app to drain a stale or stalled position pipe.
// Preserve the exact wire pair, including the existing MAX,MAX hide marker;
// absence of an event is distinct from either a hidden cursor or position 0,0.
class CursorPositionChannel {
public:
    CursorPositionChannel() : mWriter([this] { retryLoop(); }) {}
    CursorPositionChannel(const CursorPositionChannel&) = delete;
    CursorPositionChannel& operator=(const CursorPositionChannel&) = delete;
    ~CursorPositionChannel() {
        {
            std::lock_guard lock(mMutex);
            mStop = true;
        }
        mReady.notify_one();
        mWriter.join();
    }
    int attach(int borrowedFd) {
        int replacement = fcntl(borrowedFd, F_DUPFD_CLOEXEC, 0);
        if (replacement < 0) return errno;
        const int flags = fcntl(replacement, F_GETFL);
        if (flags < 0 || fcntl(replacement, F_SETFL, flags | O_NONBLOCK) < 0) {
            const int error = errno;
            close(replacement);
            return error;
        }
        auto owner = std::make_shared<PipeWriter>(replacement);
        std::lock_guard lock(mMutex);
        mFd = std::move(owner);
        return writeLocked();
    }
    int update(uint32_t x, uint32_t y) {
        std::lock_guard lock(mMutex);
        mPosition = std::array<uint32_t, 2>{x, y};
        return writeLocked();
    }
private:
    struct PipeWriter {
        explicit PipeWriter(int value) : fd(value) {}
        ~PipeWriter() { close(fd); }
        const int fd;
    };
    int writeLocked() {
        mPending = false;
        if (!mFd || !mPosition) return 0;
        ssize_t count;
        do { count = write(mFd->fd, mPosition->data(), sizeof(*mPosition)); }
        while (count < 0 && errno == EINTR);
        if (count < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) {
            // Atomic 8-byte pipe records are all-or-nothing. Coalesce only the
            // not-yet-written position; the final event must be retried even if
            // the guest sends no further cursor movement (including hide).
            mPending = true;
            mReady.notify_one();
            return 0;
        }
        return count == static_cast<ssize_t>(sizeof(*mPosition)) ? 0 : (count < 0 ? errno : EIO);
    }
    void retryLoop() {
        std::unique_lock lock(mMutex);
        while (!mStop) {
            mReady.wait(lock, [this] { return mStop || mPending; });
            if (mStop) break;
            // Retain this exact pipe across unlock/poll. Reattach can replace
            // it immediately without closing/reusing a descriptor under poll.
            const auto pipe = mFd;
            lock.unlock();
            pollfd descriptor{pipe->fd, POLLOUT, 0};
            // Only a saturated consumer wakes periodically. Bound shutdown or
            // old-pipe replacement to 25ms; the normal idle worker sleeps on
            // the condition variable and GPU/Binder callers never poll.
            const int result = poll(&descriptor, 1, 25);
            lock.lock();
            if (mStop) break;
            if (pipe != mFd || !mPending) continue;
            if (result > 0) (void)writeLocked();
        }
    }
    std::mutex mMutex;
    std::condition_variable mReady;
    std::optional<std::array<uint32_t, 2>> mPosition;
    std::shared_ptr<PipeWriter> mFd;
    bool mPending = false;
    bool mStop = false;
    std::thread mWriter;
};
} // namespace android_display
