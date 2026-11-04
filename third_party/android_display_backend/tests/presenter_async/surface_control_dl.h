// Test double: records transaction operations; apply blocks on a gate.
#pragma once
#include <android/hardware_buffer.h>
#include <android/native_window.h>
#include <condition_variable>
#include <algorithm>
#include <cassert>
#include <mutex>
#include <string>
#include <vector>
#include <deque>
#include <map>
struct ASurfaceControl { int id; };
struct ASurfaceTransaction { int id; bool detach = false; AHardwareBuffer* buffer = nullptr; int surface = 0; bool pressureSet = false; bool pressure = false; };
struct ASurfaceTransactionStats { bool hasPreviousFence = true; int previousFence = -1;
    int presentFence = -1; uint64_t latchNs = 0; bool member = true; };
struct ARect { int32_t left, top, right, bottom; };
typedef void (*SurfaceTransactionOnComplete)(void*, ASurfaceTransactionStats*);
typedef void (*SurfaceTransactionOnBufferRelease)(void*, int);
class SurfaceControl {
public:
    bool IsSupported() const { return true; }
    static SurfaceControl& GetInstance() { static SurfaceControl s; return s; }
    bool HasPreviousReleaseFence() const { return true; }
    bool HasBufferReleaseCallback() const { return true; }
    bool HasBufferGeometry() const { return geometryAvailable; }
    bool HasPresentTiming() const { return timingAvailable; }
    bool PresentTiming(ASurfaceTransactionStats* stats, ASurfaceControl*, int* fd, uint64_t* latch) {
        *fd = -1; *latch = 0;
        if (!stats || !stats->member || !stats->latchNs) return false;
        *fd = stats->presentFence; *latch = stats->latchNs;
        return *fd >= 0;
    }
    bool HasBufferTransparency() const { return true; }
    bool HasFrameRate() const { return frameRateAvailable; }
    bool HasBackPressure() const { return backPressureAvailable; }
    void SetEnableBackPressure(ASurfaceTransaction* t, ASurfaceControl* s, bool enable) {
        assert(backPressureAvailable); t->surface=s->id; t->pressureSet=true; t->pressure=enable;
    }
    // Model the documented SurfaceFlinger boundary: without backpressure,
    // ready buffers for a layer may be consumed in one compositor cycle.
    int latchFrame() {
        std::lock_guard l(m);
        assert(!pending.empty());
        auto chosen=pending.front(); pending.pop_front();
        if (!pressure[chosen.second]) {
            while (!pending.empty() && pending.front().second==chosen.second) {
                chosen=pending.front(); pending.pop_front(); ++droppedFrames;
            }
        }
        presented.push_back(chosen.first->id);
        return chosen.first->id;
    }
    void ASurfaceTransaction_setFrameRateWithChangeStrategy(ASurfaceTransaction* t,
            ASurfaceControl* surface, float rate, int8_t compatibility, int8_t strategy) {
        std::lock_guard l(m);
        assert(frameRateAvailable);
        votes.push_back({t->id, surface->id, rate, compatibility, strategy});
    }
    ASurfaceControl* ASurfaceControl_createFromWindow(ANativeWindow* window, const char*) {
        std::lock_guard l(m);
        windows.push_back(window);
        ++liveSurfaces;
        return new ASurfaceControl{++nextSurface};
    }
    void ASurfaceControl_release(ASurfaceControl* s) {
        std::lock_guard l(m);
        --liveSurfaces;
        delete s;
        cv.notify_all();
    }
    ASurfaceTransaction* ASurfaceTransaction_create() {
        std::lock_guard l(m);
        if (failNextCreate) { failNextCreate = false; return nullptr; }
        return new ASurfaceTransaction{++next};
    }
    void ASurfaceTransaction_setBuffer(ASurfaceTransaction* t, ASurfaceControl*, AHardwareBuffer* b, int32_t) { if (!b) t->detach = true; }
    void ASurfaceTransaction_setOnComplete(ASurfaceTransaction* t, void* context, SurfaceTransactionOnComplete callback) {
        std::lock_guard l(m);
        completions.push_back({t->id, context, callback});
    }
    bool PreviousReleaseFence(ASurfaceTransactionStats* stats, ASurfaceControl*, int* fd) {
        *fd = stats->previousFence;
        return stats->hasPreviousFence;
    }
    void ASurfaceTransaction_reparent(ASurfaceTransaction*, ASurfaceControl*, ASurfaceControl*) {}
    void ASurfaceTransaction_setBufferWithRelease(ASurfaceTransaction* transaction, ASurfaceControl* surface, AHardwareBuffer* buffer, int fd, void* ctx,
                                                  SurfaceTransactionOnBufferRelease callback) {
        std::lock_guard l(m);
        transaction->buffer=buffer; transaction->surface=surface->id;
        submissions.push_back({transaction->id, surface->id});
        callbacks.push_back({buffer, ctx, callback});
        fds.push_back(fd);
    }
    // Only an explicit framework callback releases a test buffer. Applying a
    // detach or destroying a layer deliberately does not fabricate release.
    void releaseBuffer(AHardwareBuffer* buffer, int fd = -1) {
        Release callback;
        {
            std::lock_guard l(m);
            auto found = std::find_if(callbacks.begin(), callbacks.end(),
                    [buffer](const Release& pending) { return pending.buffer == buffer; });
            assert(found != callbacks.end());
            callback = *found;
            callbacks.erase(found);
        }
        callback.callback(callback.context, fd);
    }
    void completeTransaction(int transaction, int previousFence = -1, int presentFence = -1,
                             uint64_t latchNs = 0, bool member = true) {
        Completion callback;
        {
            std::lock_guard l(m);
            assert(std::find(log.begin(), log.end(), "delete:" + std::to_string(transaction)) != log.end());
            auto found = std::find_if(completions.begin(), completions.end(),
                    [transaction](const Completion& pending) { return pending.transaction == transaction; });
            assert(found != completions.end());
            callback = *found;
            completions.erase(found);
        }
        ASurfaceTransactionStats stats{true, previousFence, presentFence, latchNs, member};
        callback.callback(callback.context, &stats);
    }
    void ASurfaceTransaction_setGeometry(ASurfaceTransaction* t, ASurfaceControl* surface,
            const ARect* source, const ARect* destination, int32_t transform) {
        std::lock_guard l(m);
        geometries.push_back({t->id, surface->id, *source, *destination, transform});
    }
    void SetBufferOpaque(ASurfaceTransaction*, ASurfaceControl*) {}
    void ASurfaceTransaction_apply(ASurfaceTransaction* t) {
        std::unique_lock l(m);
        cv.wait(l, [this] { return open; });
        if(t->pressureSet) { pressure[t->surface]=t->pressure; ++pressureTransactions; }
        if(t->buffer) pending.push_back({t->buffer,t->surface});
        log.push_back((t->detach ? "apply-detach:" : "apply:") + std::to_string(t->id));
        cv.notify_all();
    }
    void ASurfaceTransaction_delete(ASurfaceTransaction* t) {
        std::lock_guard l(m);
        log.push_back("delete:" + std::to_string(t->id));
        delete t;
        cv.notify_all();
    }
    std::mutex m;
    std::condition_variable cv;
    bool open = false;
    bool backPressureAvailable=true;
    unsigned droppedFrames=0,pressureTransactions=0;
    std::map<int,bool> pressure;
    std::deque<std::pair<AHardwareBuffer*,int>> pending;
    std::vector<int> presented;
    bool failNextCreate = false;
    bool frameRateAvailable = true;
    bool geometryAvailable = false;
    bool timingAvailable = false;
    int next = 0;
    int nextSurface = 0;
    int liveSurfaces = 0;
    std::vector<std::string> log;
    struct Release { AHardwareBuffer* buffer; void* context; SurfaceTransactionOnBufferRelease callback; };
    std::vector<Release> callbacks;
    struct Completion { int transaction; void* context; SurfaceTransactionOnComplete callback; };
    std::vector<Completion> completions;
    std::vector<int> fds;
    std::vector<ANativeWindow*> windows;
    struct Vote { int transaction, surface; float rate; int8_t compatibility, strategy; };
    std::vector<Vote> votes;
    struct Geometry { int transaction, surface; ARect source, destination; int32_t transform; };
    std::vector<Geometry> geometries;
    std::vector<std::pair<int, int>> submissions;
};
