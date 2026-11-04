/*
 * Copyright 2025 The Android Open Source Project
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *      http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

#include "surface_control_dl.h"

#include <dlfcn.h>
#include <cerrno>

#define LOAD_FUNCTION(lib, func)                                \
    do {                                                        \
        func##_ = reinterpret_cast<p##func>(dlsym(lib, #func)); \
        if (!func##_) {                                         \
            return false;                                       \
        }                                                       \
    } while (0)

SurfaceControl& SurfaceControl::GetInstance() {
    static android::base::NoDestructor<SurfaceControl> instance;
    return *instance;
}

SurfaceControl::SurfaceControl() {
    is_supported_ = LoadFunctions();
}

bool SurfaceControl::IsSupported() const {
    return is_supported_;
}

bool SurfaceControl::HasPreviousReleaseFence() const {
    return IsSupported() && ASurfaceTransaction_setOnComplete_ &&
            ASurfaceTransactionStats_getASurfaceControls_ &&
            ASurfaceTransactionStats_releaseASurfaceControls_ &&
            ASurfaceTransactionStats_getPreviousReleaseFenceFd_ && ASurfaceTransaction_reparent_;
}

bool SurfaceControl::HasBufferReleaseCallback() const {
    return HasPreviousReleaseFence() && ASurfaceTransaction_setBufferWithRelease_;
}

bool SurfaceControl::HasBufferGeometry() const {
    return IsSupported() && ASurfaceTransaction_setGeometry_;
}

bool SurfaceControl::HasPresentTiming() const {
    return IsSupported() && ASurfaceTransaction_setOnComplete_ &&
           ASurfaceTransactionStats_getASurfaceControls_ &&
           ASurfaceTransactionStats_releaseASurfaceControls_ &&
           ASurfaceTransactionStats_getPresentFenceFd_ && ASurfaceTransactionStats_getLatchTime_;
}

bool SurfaceControl::PresentTiming(ASurfaceTransactionStats* stats, ASurfaceControl* surface,
                                  int* fd, uint64_t* latchNs) {
    if (!fd || !latchNs) return false;
    *fd = -1;
    *latchNs = 0;
    if (!stats || !surface || !HasPresentTiming()) return false;
    ASurfaceControl** controls = nullptr;
    size_t count = 0;
    ASurfaceTransactionStats_getASurfaceControls_(stats, &controls, &count);
    bool found = false;
    if (controls) {
        for (size_t i = 0; i < count; ++i) found |= controls[i] == surface;
        ASurfaceTransactionStats_releaseASurfaceControls_(controls);
    }
    if (!found) return false;
    const int64_t latch = ASurfaceTransactionStats_getLatchTime_(stats);
    if (latch <= 0) return false;
    *fd = ASurfaceTransactionStats_getPresentFenceFd_(stats);
    if (*fd < 0) return false; // -1 means unsupported, never already presented.
    *latchNs = static_cast<uint64_t>(latch);
    return true;
}

bool SurfaceControl::LoadFunctions() {
    void* lib = dlopen("libandroid.so", RTLD_NOW);
    if (lib == nullptr) {
        return false;
    }

    LOAD_FUNCTION(lib, ASurfaceControl_createFromWindow);
    LOAD_FUNCTION(lib, ASurfaceControl_release);
    LOAD_FUNCTION(lib, ASurfaceTransaction_create);
    LOAD_FUNCTION(lib, ASurfaceTransaction_setBuffer);
    LOAD_FUNCTION(lib, ASurfaceTransaction_apply);
    LOAD_FUNCTION(lib, ASurfaceTransaction_delete);

#define LOAD_OPTIONAL(func) func##_ = reinterpret_cast<p##func>(dlsym(lib, #func))
    LOAD_OPTIONAL(ASurfaceTransaction_setFrameRateWithChangeStrategy);
    LOAD_OPTIONAL(ASurfaceTransaction_setOnComplete);
    LOAD_OPTIONAL(ASurfaceTransactionStats_getPresentFenceFd);
    LOAD_OPTIONAL(ASurfaceTransactionStats_getLatchTime);
    LOAD_OPTIONAL(ASurfaceTransactionStats_getASurfaceControls);
    LOAD_OPTIONAL(ASurfaceTransactionStats_releaseASurfaceControls);
    LOAD_OPTIONAL(ASurfaceTransactionStats_getPreviousReleaseFenceFd);
    LOAD_OPTIONAL(ASurfaceTransaction_reparent);
    LOAD_OPTIONAL(ASurfaceTransaction_setBufferWithRelease);
    LOAD_OPTIONAL(ASurfaceTransaction_setGeometry);
    LOAD_OPTIONAL(ASurfaceTransaction_setBufferTransparency);
    LOAD_OPTIONAL(ASurfaceTransaction_setEnableBackPressure);
#undef LOAD_OPTIONAL

    return true;
}

ASurfaceControl* SurfaceControl::ASurfaceControl_createFromWindow(ANativeWindow* window,
                                                                  const char* debug_name) {
    return ASurfaceControl_createFromWindow_(window, debug_name);
}

void SurfaceControl::ASurfaceControl_release(ASurfaceControl* surface_control) {
    ASurfaceControl_release_(surface_control);
}

ASurfaceTransaction* SurfaceControl::ASurfaceTransaction_create() {
    return ASurfaceTransaction_create_();
}

void SurfaceControl::ASurfaceTransaction_setBuffer(ASurfaceTransaction* transaction,
                                                   ASurfaceControl* surface_control,
                                                   AHardwareBuffer* buffer,
                                                   int32_t acquire_fence_fd) {
    ASurfaceTransaction_setBuffer_(transaction, surface_control, buffer, acquire_fence_fd);
}

void SurfaceControl::ASurfaceTransaction_setFrameRateWithChangeStrategy(
        ASurfaceTransaction* transaction, ASurfaceControl* surface_control, float frame_rate,
        int8_t compatibility, int8_t change_frame_rate_strategy) {
    if (HasFrameRate()) {
        ASurfaceTransaction_setFrameRateWithChangeStrategy_(
                transaction, surface_control, frame_rate, compatibility, change_frame_rate_strategy);
    }
}

void SurfaceControl::ASurfaceTransaction_setOnComplete(ASurfaceTransaction* transaction,
                                                       void* context,
                                                       SurfaceTransactionOnComplete callback) {
    if (ASurfaceTransaction_setOnComplete_) ASurfaceTransaction_setOnComplete_(transaction, context,
                                                                                 callback);
}

bool SurfaceControl::PreviousReleaseFence(ASurfaceTransactionStats* stats,
                                          ASurfaceControl* surface, int* fd) {
    if (!stats || !surface || !fd || !HasPreviousReleaseFence()) return false;
    ASurfaceControl** controls = nullptr;
    size_t size = 0;
    ASurfaceTransactionStats_getASurfaceControls_(stats, &controls, &size);
    bool found = false;
    for (size_t i = 0; i < size; ++i) found |= controls[i] == surface;
    if (controls) ASurfaceTransactionStats_releaseASurfaceControls_(controls);
    if (!found) return false;
    errno = 0;
    *fd = ASurfaceTransactionStats_getPreviousReleaseFenceFd_(stats, surface);
    return *fd >= 0 || (*fd == -1 && errno == 0);
}

void SurfaceControl::ASurfaceTransaction_reparent(ASurfaceTransaction* transaction,
                                                  ASurfaceControl* surface,
                                                  ASurfaceControl* parent) {
    if (ASurfaceTransaction_reparent_) ASurfaceTransaction_reparent_(transaction, surface, parent);
}

void SurfaceControl::ASurfaceTransaction_setBufferWithRelease(
        ASurfaceTransaction* transaction, ASurfaceControl* surface, AHardwareBuffer* buffer,
        int acquireFd, void* context, SurfaceTransactionOnBufferRelease callback) {
    if (ASurfaceTransaction_setBufferWithRelease_) {
        ASurfaceTransaction_setBufferWithRelease_(transaction, surface, buffer, acquireFd, context,
                                                  callback);
    }
}

void SurfaceControl::ASurfaceTransaction_setGeometry(ASurfaceTransaction* transaction,
                                                     ASurfaceControl* surface, const ARect* source,
                                                     const ARect* destination, int32_t transform) {
    if (ASurfaceTransaction_setGeometry_) {
        ASurfaceTransaction_setGeometry_(transaction, surface, source, destination, transform);
    }
}

void SurfaceControl::SetBufferOpaque(ASurfaceTransaction* transaction, ASurfaceControl* surface) {
    if (ASurfaceTransaction_setBufferTransparency_) {
        ASurfaceTransaction_setBufferTransparency_(transaction, surface,
                                                   SurfaceBufferTransparency::Opaque);
    }
}

void SurfaceControl::SetEnableBackPressure(ASurfaceTransaction* transaction, ASurfaceControl* surface,
                                           bool enable) {
    if (HasBackPressure()) ASurfaceTransaction_setEnableBackPressure_(transaction, surface, enable);
}

void SurfaceControl::ASurfaceTransaction_apply(ASurfaceTransaction* transaction) {
    ASurfaceTransaction_apply_(transaction);
}

void SurfaceControl::ASurfaceTransaction_delete(ASurfaceTransaction* transaction) {
    ASurfaceTransaction_delete_(transaction);
}
