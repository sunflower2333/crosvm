/* SPDX-License-Identifier: Apache-2.0 */
#pragma once

#include <algorithm>
#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <fstream>
#include <functional>
#include <mutex>
#include <sstream>
#include <string>
#include <thread>
#include <utility>
#include <vector>

// A GPU clock floor held only while the Vulkan display path is presenting frames.
//
// Why: the guest compositor's present waits on the host display blit (the RESOURCE_FLUSH fence is
// deferred to the blit's completion), so VM graphics are latency-bound rather than load-bound. The
// GPU sits idle for most of each frame waiting on that pipeline, and msm-adreno-tz reads the idle
// time as headroom: measured 2026-09-17 on an SM8750 (Adreno 830) with a Windows guest running
// TestUFO, it held the GPU at 160-222 MHz (of 1100) for the entire session at ~40% busy. At 160 MHz
// the display blit alone took 3.56 ms of GPU time (0.34 ms at 342 MHz) and the guest's DWM slipped
// to every third vblank on a third of its frames.
//
//   guest 120 Hz: 160 MHz 48.8 DWM presents/s; 222 54.8; 342 57.2; 443 56.6; 525 56.9; 1100 56.6
//   guest 165 Hz: 342 MHz 56.3 presents/s;     443 74.3; 525 74.7; 1100 73.6
//
// The knee moves with the guest's vblank: 342 MHz holds two vblanks at 120 Hz but not at 165 Hz,
// where 443 MHz and above lock DWM to two vblanks (77 fps on the Android layer). Declaring
// GameManager.setGameState(MODE_GAMEPLAY_UNINTERRUPTIBLE) did not move the clock on this device.
//
// Zero-copy (native AHB) presentation at 165 Hz, 2026-09-24, guest TestUFO, 8 s windows:
//   160 MHz ~100 DWM presents/s; 443 139; 660 146; 1100 147. The GPU's share of a frame is small,
//   but each DWM buffer must return from SurfaceControl within two frames, so every ms counts.
//
// The floor is the lowest advertised frequency at or above a share of max_freq (660 MHz above), is
// applied on the first presented frame, and the previous min_freq is restored after the display
// has been idle for a while or when the display context is destroyed. A min_freq that somebody else
// raised in the meantime is left alone; one lowered below the floor while frames flow is put back.
namespace gpu_clock_floor {

struct Config {
    bool enabled = true;
    std::string devfreqDir = "/sys/class/kgsl/kgsl-3d0/devfreq";
    // The same floor drives a cpufreq policy with these pointed at its scaling_* files.
    std::string minFile = "min_freq";
    std::string maxFile = "max_freq";
    std::string availableFile = "available_frequencies";
    std::string label = "GPU clock floor";
    uint64_t explicitHz = 0;          // 0: choose from available_frequencies
    unsigned autoPercentOfMax = 60;   // lowest advertised frequency >= this share of max_freq
    std::chrono::milliseconds idleRestore{2000};
    // While engaged, how often min_freq is re-read. Platform services reset it on app switches
    // without the display ever going idle, which would otherwise drop the floor until it does.
    std::chrono::milliseconds recheck{1000};
    std::function<void(const std::string&)> log;
};

// CROSVM_ANDROID_DISPLAY_GPU_MIN_FREQ: unset or "auto" chooses from the table, "0"/"off" disables,
// a decimal number is an explicit floor in Hz.
inline Config configFromValue(const char* value, Config config = {}) {
    if (value == nullptr || *value == '\0' || std::strcmp(value, "auto") == 0) return config;
    if (std::strcmp(value, "0") == 0 || std::strcmp(value, "off") == 0 ||
        std::strcmp(value, "false") == 0) {
        config.enabled = false;
        return config;
    }
    char* end = nullptr;
    const unsigned long long hz = std::strtoull(value, &end, 10);
    if (end == value || *end != '\0' || hz == 0) {
        config.enabled = false;  // an unparseable request must not guess a clock
        return config;
    }
    config.explicitHz = hz;
    return config;
}

inline bool readUint(const std::string& path, uint64_t& out) {
    std::ifstream in(path);
    unsigned long long value = 0;
    if (!(in >> value)) return false;
    out = value;
    return true;
}

inline std::vector<uint64_t> readList(const std::string& path) {
    std::ifstream in(path);
    std::vector<uint64_t> values;
    unsigned long long value = 0;
    while (in >> value) values.push_back(value);
    return values;
}

inline bool writeUint(const std::string& path, uint64_t value) {
    std::ofstream out(path);
    if (!out) return false;
    out << value;
    out.flush();
    return static_cast<bool>(out);
}

inline uint64_t chooseFloorHz(const std::vector<uint64_t>& available, uint64_t maxHz,
                              unsigned percentOfMax) {
    if (maxHz == 0) return 0;
    // A policy that advertises no table (some cpufreq drivers) can still be floored at its max.
    if (available.empty()) return percentOfMax >= 100 ? maxHz : 0;
    const uint64_t threshold = maxHz / 100 * percentOfMax + maxHz % 100 * percentOfMax / 100;
    uint64_t best = 0;
    for (uint64_t hz : available) {
        if (hz >= threshold && hz <= maxHz && (best == 0 || hz < best)) best = hz;
    }
    return best;
}

// One floor per cpufreq policy, each pinned at its current scaling_max_freq while frames flow.
// Measured 2026-09-25 at 165 Hz: the guest's frame chain (Edge -> DWM -> flip) crosses several
// vCPU and crosvm threads per frame, and letting the host governor drop them to 1.0-1.4 GHz
// between frames cost 10 fps on the Android layer (151 -> 164 fps with the floor). The platform
// caps scaling_max_freq itself, so the floor never exceeds what the power HAL allows.
// CROSVM_ANDROID_DISPLAY_CPU_MIN_FREQ: unset or "auto" enables it, "0"/"off" disables it.
inline std::vector<Config> cpuPolicyConfigs(
        const char* value, const std::string& root = "/sys/devices/system/cpu/cpufreq") {
    std::vector<Config> configs;
    if (value != nullptr && (std::strcmp(value, "0") == 0 || std::strcmp(value, "off") == 0 ||
                             std::strcmp(value, "false") == 0))
        return configs;
    for (int policy = 0; policy < 16; ++policy) {
        const std::string dir = root + "/policy" + std::to_string(policy);
        uint64_t probe = 0;
        if (!readUint(dir + "/scaling_min_freq", probe)) continue;
        Config config;
        config.devfreqDir = dir;
        config.minFile = "scaling_min_freq";
        config.maxFile = "scaling_max_freq";
        config.availableFile = "scaling_available_frequencies";
        config.autoPercentOfMax = 100;
        config.label = "CPU policy" + std::to_string(policy) + " clock floor";
        configs.push_back(std::move(config));
    }
    return configs;
}

class Floor {
public:
    explicit Floor(Config config) : mConfig(std::move(config)) {
        mDisabled.store(!mConfig.enabled, std::memory_order_relaxed);
    }

    Floor(const Floor&) = delete;
    Floor& operator=(const Floor&) = delete;

    ~Floor() {
        {
            std::lock_guard<std::mutex> lock(mMutex);
            mStop = true;
        }
        mCv.notify_all();
        if (mThread.joinable()) mThread.join();
        std::lock_guard<std::mutex> lock(mMutex);
        restoreLocked("display teardown");
    }

    // Called once per presented frame, from the presenting thread. Lock-free while engaged.
    void onFrame() {
        if (mDisabled.load(std::memory_order_relaxed)) return;
        const int64_t now = nowNs();
        mLastFrameNs.store(now, std::memory_order_relaxed);
        if (mEngaged.load(std::memory_order_relaxed)) return;
        std::lock_guard<std::mutex> lock(mMutex);
        if (mStop || mEngaged.load(std::memory_order_relaxed) || now < mRetryAfterNs) return;
        if (!engageLocked()) {
            // Not needed (already at or above the floor) or failed: look again after an idle
            // period rather than on every frame.
            mRetryAfterNs = now + idleNs();
            return;
        }
        if (!mThread.joinable()) mThread = std::thread([this] { worker(); });
        mCv.notify_all();
    }

    bool engaged() const { return mEngaged.load(std::memory_order_relaxed); }
    bool disabled() const { return mDisabled.load(std::memory_order_relaxed); }

private:
    static int64_t nowNs() {
        return std::chrono::duration_cast<std::chrono::nanoseconds>(
                       std::chrono::steady_clock::now().time_since_epoch())
                .count();
    }

    int64_t idleNs() const {
        return std::chrono::duration_cast<std::chrono::nanoseconds>(mConfig.idleRestore).count();
    }

    std::string minPath() const { return mConfig.devfreqDir + "/" + mConfig.minFile; }

    void log(const std::string& message) const {
        if (mConfig.log) mConfig.log(message);
    }

    void disable(const std::string& why) {
        mDisabled.store(true, std::memory_order_relaxed);
        log(mConfig.label + " disabled: " + why);
    }

    bool engageLocked() {
        uint64_t current = 0;
        uint64_t maxHz = 0;
        if (!readUint(minPath(), current) ||
            !readUint(mConfig.devfreqDir + "/" + mConfig.maxFile, maxHz)) {
            disable("no readable " + mConfig.minFile + "/" + mConfig.maxFile + " under " +
                    mConfig.devfreqDir);
            return false;
        }
        uint64_t target = mConfig.explicitHz;
        if (target == 0) {
            target = chooseFloorHz(readList(mConfig.devfreqDir + "/" + mConfig.availableFile),
                                   maxHz, mConfig.autoPercentOfMax);
            if (target == 0) {
                disable("no advertised frequency at or above " +
                        std::to_string(mConfig.autoPercentOfMax) + "% of " + mConfig.maxFile);
                return false;
            }
        }
        if (target > maxHz) target = maxHz;  // max_freq may be thermally capped right now
        if (current >= target) return false;
        if (!writeUint(minPath(), target)) {
            disable("cannot write " + minPath());
            return false;
        }
        uint64_t applied = 0;
        if (!readUint(minPath(), applied) || applied < target) {
            // The governor refused or rounded the request down; nothing of ours to restore.
            if (applied != current) writeUint(minPath(), current);
            disable("min_freq did not accept " + std::to_string(target));
            return false;
        }
        mOriginalHz = current;
        mAppliedHz = applied;
        mEngaged.store(true, std::memory_order_relaxed);
        std::ostringstream message;
        message << mConfig.label << " engaged: " << mConfig.minFile << " " << current << " -> "
                << applied;
        log(message.str());
        return true;
    }

    void restoreLocked(const char* why) {
        if (!mEngaged.load(std::memory_order_relaxed)) return;
        mEngaged.store(false, std::memory_order_relaxed);
        uint64_t now = 0;
        if (!readUint(minPath(), now)) {
            log(mConfig.label + " restore skipped (" + why + "): " + mConfig.minFile +
                " unreadable");
            return;
        }
        if (now != mAppliedHz) {
            log(mConfig.label + " restore skipped (" + why + "): " + mConfig.minFile +
                " is now " + std::to_string(now) + ", changed by someone else");
            return;
        }
        const bool ok = writeUint(minPath(), mOriginalHz);
        log(mConfig.label + (ok ? " restored" : " restore FAILED") + " (" + why + "): " +
            mConfig.minFile + " " + std::to_string(mAppliedHz) + " -> " +
            std::to_string(mOriginalHz));
    }

    // Somebody lowered min_freq below the floor while frames are still flowing: put it back. A
    // raised value is theirs and is left alone (and is then not restored either, see
    // restoreLocked).
    void reassertLocked() {
        uint64_t now = 0;
        if (!readUint(minPath(), now) || now >= mAppliedHz) return;
        const bool ok = writeUint(minPath(), mAppliedHz);
        log(mConfig.label + (ok ? " re-applied" : " re-apply FAILED") + ": " + mConfig.minFile +
            " had been lowered to " + std::to_string(now));
    }

    void worker() {
        const int64_t recheckNs =
                std::chrono::duration_cast<std::chrono::nanoseconds>(mConfig.recheck).count();
        std::unique_lock<std::mutex> lock(mMutex);
        int64_t nextRecheck = nowNs() + recheckNs;
        while (!mStop) {
            if (!mEngaged.load(std::memory_order_relaxed)) {
                mCv.wait(lock);
                nextRecheck = nowNs() + recheckNs;
                continue;
            }
            const int64_t deadline = mLastFrameNs.load(std::memory_order_relaxed) + idleNs();
            const int64_t now = nowNs();
            if (now >= deadline) {
                restoreLocked("display idle");
                continue;
            }
            if (now >= nextRecheck) {
                reassertLocked();
                nextRecheck = now + recheckNs;
            }
            mCv.wait_for(lock, std::chrono::nanoseconds(std::min(deadline, nextRecheck) - now));
        }
    }

    const Config mConfig;
    std::atomic<bool> mDisabled{false};
    std::atomic<bool> mEngaged{false};
    std::atomic<int64_t> mLastFrameNs{0};
    std::mutex mMutex;
    std::condition_variable mCv;
    std::thread mThread;
    bool mStop = false;
    int64_t mRetryAfterNs = 0;
    uint64_t mOriginalHz = 0;
    uint64_t mAppliedHz = 0;
};

// Qualcomm core_ctl pauses cores of a cluster down to min_cpus whenever its load estimate is
// low -- which is exactly an idle guest. A vCPU thread woken onto a paused core does not run
// until core_ctl unpauses it: measured 27 ms wakeup->run for SCHED_RR vCPUs, so the guest's
// one-shot vblank timer missed 3.8 whole 165 Hz periods per second on an idle desktop
// (160.4 Hz delivered). With min_cpus raised to max_cpus: max 3.5 ms, 165.02 Hz, zero misses.
// Held for the whole display lifetime, not per frame, because the guest needs on-time vblanks
// while nothing is being presented too. Platform services may rewrite min_cpus, so reassert()
// re-applies it at most once per second. CROSVM_ANDROID_DISPLAY_CORE_CTL=off disables it.
class CoreCtlHold {
public:
    using Log = std::function<void(const std::string&)>;

    CoreCtlHold(const char* value, Log log) : mLog(std::move(log)) {
        if (value && (!strcmp(value, "off") || !strcmp(value, "0"))) {
            log_("core_ctl hold disabled by CROSVM_ANDROID_DISPLAY_CORE_CTL");
            return;
        }
        for (int cpu = 0;; ++cpu) {
            const std::string cpuDir = "/sys/devices/system/cpu/cpu" + std::to_string(cpu);
            std::ifstream probe(cpuDir + "/uevent");
            if (!probe) break;  // past the last CPU
            const std::string dir = cpuDir + "/core_ctl/";
            uint64_t enable = 0, minCpus = 0, maxCpus = 0;
            // core_ctl exists only on the first CPU of each cluster.
            if (!readUint(dir + "enable", enable) || !enable) continue;
            if (!readUint(dir + "min_cpus", minCpus) || !readUint(dir + "max_cpus", maxCpus) ||
                minCpus >= maxCpus) {
                continue;
            }
            Held held{dir + "min_cpus", minCpus, maxCpus};
            if (!writeUint(held.path, held.applied)) {
                log_("core_ctl hold FAILED: " + held.path + " " + std::to_string(minCpus) + " -> " +
                     std::to_string(maxCpus));
                continue;
            }
            log_("core_ctl hold engaged: " + held.path + " " + std::to_string(minCpus) + " -> " +
                 std::to_string(maxCpus));
            mHeld.push_back(std::move(held));
        }
    }

    ~CoreCtlHold() {
        std::lock_guard lock(mMutex);
        for (const auto& held : mHeld) {
            uint64_t current = 0;
            // Someone else changed it since our last reassert: theirs now, leave it.
            if (!readUint(held.path, current) || current != held.applied) continue;
            const bool ok = writeUint(held.path, held.original);
            log_(std::string("core_ctl hold ") + (ok ? "restored: " : "restore FAILED: ") +
                 held.path + " " + std::to_string(held.applied) + " -> " +
                 std::to_string(held.original));
        }
    }

    CoreCtlHold(const CoreCtlHold&) = delete;
    CoreCtlHold& operator=(const CoreCtlHold&) = delete;

    // Cheap enough to call per presented frame: one clock read unless a second has passed.
    void reassert() {
        if (mHeld.empty()) return;
        const auto now = std::chrono::steady_clock::now();
        std::unique_lock lock(mMutex, std::try_to_lock);
        if (!lock.owns_lock() || now - mLastCheck < std::chrono::seconds(1)) return;
        mLastCheck = now;
        for (const auto& held : mHeld) {
            uint64_t current = 0;
            if (!readUint(held.path, current) || current >= held.applied) continue;
            const bool ok = writeUint(held.path, held.applied);
            log_(std::string("core_ctl hold ") + (ok ? "reasserted: " : "reassert FAILED: ") +
                 held.path + " " + std::to_string(current) + " -> " + std::to_string(held.applied));
        }
    }

private:
    struct Held {
        std::string path;
        uint64_t original;
        uint64_t applied;
    };

    void log_(const std::string& message) const {
        if (mLog) mLog(message);
    }

    Log mLog;
    std::mutex mMutex;
    std::vector<Held> mHeld;
    std::chrono::steady_clock::time_point mLastCheck{};
};

}  // namespace gpu_clock_floor
