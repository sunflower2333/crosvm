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

#include <stdint.h>
#include <stddef.h>

extern "C" {

typedef struct ASurfaceControl ASurfaceControl;
typedef struct ASurfaceTransaction ASurfaceTransaction;
typedef struct ANativeWindow ANativeWindow;
typedef struct AHardwareBuffer AHardwareBuffer;
typedef struct ASurfaceTransactionStats ASurfaceTransactionStats;
typedef struct ARect ARect;

using SurfaceTransactionOnComplete = void (*)(void*, ASurfaceTransactionStats*);
using SurfaceTransactionOnBufferRelease = void (*)(void*, int);

using pASurfaceControl_createFromWindow = ASurfaceControl* (*)(ANativeWindow * window,
                                                               const char* debug_name);

using pASurfaceControl_release = void (*)(ASurfaceControl* surface_control);

using pASurfaceTransaction_create = ASurfaceTransaction* (*)(void);

using pASurfaceTransaction_setBuffer = void (*)(ASurfaceTransaction* transaction,
                                                ASurfaceControl* surface_control,
                                                AHardwareBuffer* buffer, int32_t acquire_fence_fd);

using pASurfaceTransaction_setOnComplete = void (*)(ASurfaceTransaction*, void*,
                                                    SurfaceTransactionOnComplete);
using pASurfaceTransactionStats_getASurfaceControls = void (*)(ASurfaceTransactionStats*,
                                                               ASurfaceControl***, size_t*);
using pASurfaceTransactionStats_releaseASurfaceControls = void (*)(ASurfaceControl**);
using pASurfaceTransactionStats_getPreviousReleaseFenceFd = int (*)(ASurfaceTransactionStats*,
                                                                    ASurfaceControl*);
using pASurfaceTransactionStats_getPresentFenceFd = int (*)(ASurfaceTransactionStats*);
using pASurfaceTransactionStats_getLatchTime = int64_t (*)(ASurfaceTransactionStats*);
using pASurfaceTransaction_reparent = void (*)(ASurfaceTransaction*, ASurfaceControl*,
                                               ASurfaceControl*);
using pASurfaceTransaction_setBufferWithRelease = void (*)(ASurfaceTransaction*, ASurfaceControl*,
                                                           AHardwareBuffer*, int, void*,
                                                           SurfaceTransactionOnBufferRelease);
using pASurfaceTransaction_setGeometry = void (*)(ASurfaceTransaction*, ASurfaceControl*,
                                                  const ARect*, const ARect*, int32_t);
enum class SurfaceBufferTransparency : int8_t { Opaque = 2 };
using pASurfaceTransaction_setBufferTransparency = void (*)(ASurfaceTransaction*, ASurfaceControl*,
                                                            SurfaceBufferTransparency);
using pASurfaceTransaction_setEnableBackPressure = void (*)(ASurfaceTransaction*, ASurfaceControl*,
                                                            bool);

using pASurfaceTransaction_setFrameRateWithChangeStrategy = void (*)(
        ASurfaceTransaction* transaction, ASurfaceControl* surface_control, float frame_rate,
        int8_t compatibility, int8_t change_frame_rate_strategy);

using pASurfaceTransaction_apply = void (*)(ASurfaceTransaction* transaction);

using pASurfaceTransaction_delete = void (*)(ASurfaceTransaction* transaction);

} // extern "C"
