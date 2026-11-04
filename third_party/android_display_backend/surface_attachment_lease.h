// Copyright 2026 The Android Open Source Project
// SPDX-License-Identifier: Apache-2.0
#pragma once
#include <cstdint>

namespace android_display {
// Serialized by AndroidDisplaySurface::mSurfaceMutex. A lease identifies one
// app attachment, independently of the guest scanout/presenter lifetime.
class SurfaceAttachmentLease {
public:
    void attached(uint64_t generation, bool owned) {
        mAttached = true;
        mLease = owned ? generation : 0;
    }
    void clear() { mAttached = false; mLease = 0; }
    bool accepts(uint64_t lease) const { return mAttached && lease == mLease; }
    bool permitsAttach(bool owned) const { return owned || !mAttached || mLease == 0; }
    uint64_t current() const { return mLease; }
private:
    bool mAttached = false;
    uint64_t mLease = 0;
};
} // namespace android_display
