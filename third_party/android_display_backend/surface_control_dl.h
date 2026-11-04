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

#pragma once

#include <android-base/no_destructor.h>

#include <new>

#include "surface_control_abi.h"

class SurfaceControl {
public:
    static SurfaceControl& GetInstance();

    bool IsSupported() const;
    // Optional on older Android releases. Their absence never disables the
    // existing NativeWindow/Vulkan-blit path.
    bool HasPreviousReleaseFence() const;
    bool HasBufferReleaseCallback() const;
    bool HasBufferGeometry() const;
    bool HasPresentTiming() const;
    bool PresentTiming(ASurfaceTransactionStats*, ASurfaceControl*, int* fd, uint64_t* latchNs);
    bool HasFrameRate() const {
        return is_supported_ && ASurfaceTransaction_setFrameRateWithChangeStrategy_;
    }

    ASurfaceControl* ASurfaceControl_createFromWindow(ANativeWindow* window,
                                                      const char* debug_name);
    void ASurfaceControl_release(ASurfaceControl* surface_control);
    ASurfaceTransaction* ASurfaceTransaction_create();
    void ASurfaceTransaction_setBuffer(ASurfaceTransaction* transaction,
                                       ASurfaceControl* surface_control, AHardwareBuffer* buffer,
                                       int32_t acquire_fence_fd);
    void ASurfaceTransaction_setOnComplete(ASurfaceTransaction*, void*, SurfaceTransactionOnComplete);
    bool PreviousReleaseFence(ASurfaceTransactionStats*, ASurfaceControl*, int* fd);
    void ASurfaceTransaction_reparent(ASurfaceTransaction*, ASurfaceControl*, ASurfaceControl*);
    void ASurfaceTransaction_setBufferWithRelease(ASurfaceTransaction*, ASurfaceControl*,
                                                  AHardwareBuffer*, int, void*,
                                                  SurfaceTransactionOnBufferRelease);
    void ASurfaceTransaction_setGeometry(ASurfaceTransaction*, ASurfaceControl*, const ARect*,
                                         const ARect*, int32_t);
    bool HasBufferTransparency() const {
        return is_supported_ && ASurfaceTransaction_setBufferTransparency_;
    }
    void SetBufferOpaque(ASurfaceTransaction*, ASurfaceControl*);
    bool HasBackPressure() const {
        return is_supported_ && ASurfaceTransaction_setEnableBackPressure_;
    }
    void SetEnableBackPressure(ASurfaceTransaction*, ASurfaceControl*, bool);
    void ASurfaceTransaction_setFrameRateWithChangeStrategy(
            ASurfaceTransaction* transaction, ASurfaceControl* surface_control, float frame_rate,
            int8_t compatibility, int8_t change_frame_rate_strategy);
    void ASurfaceTransaction_apply(ASurfaceTransaction* transaction);
    void ASurfaceTransaction_delete(ASurfaceTransaction* transaction);

private:
    friend class android::base::NoDestructor<SurfaceControl>;

    SurfaceControl();
    ~SurfaceControl() = delete;

    bool LoadFunctions();

    bool is_supported_ = false;

    pASurfaceControl_createFromWindow ASurfaceControl_createFromWindow_ = nullptr;
    pASurfaceControl_release ASurfaceControl_release_ = nullptr;
    pASurfaceTransaction_create ASurfaceTransaction_create_ = nullptr;
    pASurfaceTransaction_setBuffer ASurfaceTransaction_setBuffer_ = nullptr;
    pASurfaceTransaction_setOnComplete ASurfaceTransaction_setOnComplete_ = nullptr;
    pASurfaceTransactionStats_getPresentFenceFd ASurfaceTransactionStats_getPresentFenceFd_ = nullptr;
    pASurfaceTransactionStats_getLatchTime ASurfaceTransactionStats_getLatchTime_ = nullptr;
    pASurfaceTransactionStats_getASurfaceControls ASurfaceTransactionStats_getASurfaceControls_ = nullptr;
    pASurfaceTransactionStats_releaseASurfaceControls
            ASurfaceTransactionStats_releaseASurfaceControls_ = nullptr;
    pASurfaceTransactionStats_getPreviousReleaseFenceFd
            ASurfaceTransactionStats_getPreviousReleaseFenceFd_ = nullptr;
    pASurfaceTransaction_reparent ASurfaceTransaction_reparent_ = nullptr;
    pASurfaceTransaction_setBufferWithRelease ASurfaceTransaction_setBufferWithRelease_ = nullptr;
    pASurfaceTransaction_setGeometry ASurfaceTransaction_setGeometry_ = nullptr;
    pASurfaceTransaction_setBufferTransparency ASurfaceTransaction_setBufferTransparency_ = nullptr;
    pASurfaceTransaction_setEnableBackPressure ASurfaceTransaction_setEnableBackPressure_ = nullptr;
    pASurfaceTransaction_setFrameRateWithChangeStrategy
            ASurfaceTransaction_setFrameRateWithChangeStrategy_ = nullptr;
    pASurfaceTransaction_apply ASurfaceTransaction_apply_ = nullptr;
    pASurfaceTransaction_delete ASurfaceTransaction_delete_ = nullptr;
};
