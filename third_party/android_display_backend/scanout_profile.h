// Copyright 2026 The Android Open Source Project
// SPDX-License-Identifier: Apache-2.0
#pragma once
#include "scanout_presentation_geometry.h"
#include <climits>
#include <limits>

namespace android_display {
// Owned by the main surface mutex. No cached ANativeWindow hint is consulted.
class ScanoutProfileState {
public:
    static constexpr uint32_t kUsable = 7;

    void nativeSurface(uint64_t generation, bool attached) {
        if (mNativeGeneration == generation && mAttached == attached) return;
        mNativeGeneration = generation;
        mAttached = attached;
        mRoot = mSerial = 0;
        mHint = INT_MIN;
        advance(mEndpoint);
        advance(mProfile);
    }

    bool rootHint(uint64_t root, uint64_t serial, int32_t hint) {
        if (!mAttached || !root || !serial || root < mRoot || serial < mSerial) return false;
        if (serial == mSerial) return root == mRoot && hint == mHint;
        const bool changed = root != mRoot || hint != mHint;
        if (root != mRoot) advance(mEndpoint);
        mRoot = root;
        mSerial = serial;
        mHint = hint; // Unsupported values explicitly revoke, never become identity.
        if (changed) advance(mProfile);
        return true;
    }

    void observe(uint32_t width, uint32_t height, int32_t parentWidth,
                 int32_t parentHeight, bool mechanism) {
        if (mWidth == width && mHeight == height && mParentWidth == parentWidth &&
                mParentHeight == parentHeight && mMechanism == mechanism) return;
        mWidth = width; mHeight = height;
        mParentWidth = parentWidth; mParentHeight = parentHeight;
        mMechanism = mechanism;
        advance(mProfile);
    }

    AndroidDisplayScanoutProfile snapshot() const {
        AndroidDisplayScanoutProfile p{1, 64, 0, 0, 0, 0, 0, 0, mEndpoint, mProfile, {0, 0}};
        if (mExhausted || !mAttached || !mRoot || !mMechanism ||
                !mWidth || !mHeight || mWidth > 8192 || mHeight > 8192 ||
                mParentWidth != static_cast<int32_t>(mWidth) ||
                mParentHeight != static_cast<int32_t>(mHeight)) return p;
        switch (mHint) {
            case 0: p.content_rotation = 0; break;
            case 4: p.content_rotation = 1; break;
            case 3: p.content_rotation = 2; break;
            case 7: p.content_rotation = 3; break;
            default: return p;
        }
        p.flags = kUsable;
        p.logical_width = mWidth; p.logical_height = mHeight;
        p.storage_width = p.content_rotation & 1 ? mHeight : mWidth;
        p.storage_height = p.content_rotation & 1 ? mWidth : mHeight;
        return p;
    }

    bool matches(const ScanoutPresentationGeometry& g, uint64_t endpoint, uint64_t profile) const {
        const auto p = snapshot();
        return p.flags == kUsable && endpoint && profile &&
                p.endpoint_generation == endpoint && p.profile_generation == profile &&
                p.storage_width == g.storageWidth && p.storage_height == g.storageHeight &&
                p.logical_width == g.logicalWidth && p.logical_height == g.logicalHeight &&
                p.content_rotation == static_cast<uint32_t>(g.contentRotation);
    }

private:
    void advance(uint64_t& value) {
        if (value == std::numeric_limits<uint64_t>::max()) mExhausted = true;
        else ++value;
    }
    uint64_t mNativeGeneration = 0, mEndpoint = 0, mProfile = 0, mRoot = 0, mSerial = 0;
    int32_t mHint = INT_MIN, mParentWidth = 0, mParentHeight = 0;
    uint32_t mWidth = 0, mHeight = 0;
    bool mAttached = false, mMechanism = false, mExhausted = false;
};
} // namespace android_display
