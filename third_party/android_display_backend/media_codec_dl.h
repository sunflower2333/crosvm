/*
 * Copyright 2026 The Android Open Source Project
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

#include "media_codec_abi.h"

// libmediandk, resolved once at run time.
//
// Deliberately the same shape as SurfaceControl next door, including the all-or-nothing load:
// a partial resolve is treated as no media NDK at all, so no call site has to ask whether the one
// entry point it wants happens to exist. IsSupported() is the whole question, asked once.
class MediaCodecLib {
public:
    static MediaCodecLib& GetInstance();

    bool IsSupported() const;

    pAMediaCodec_createEncoderByType AMediaCodec_createEncoderByType = nullptr;
    pAMediaCodec_configure AMediaCodec_configure = nullptr;
    pAMediaCodec_createInputSurface AMediaCodec_createInputSurface = nullptr;
    pAMediaCodec_start AMediaCodec_start = nullptr;
    pAMediaCodec_stop AMediaCodec_stop = nullptr;
    pAMediaCodec_delete AMediaCodec_delete = nullptr;
    pAMediaCodec_signalEndOfInputStream AMediaCodec_signalEndOfInputStream = nullptr;
    pAMediaCodec_dequeueOutputBuffer AMediaCodec_dequeueOutputBuffer = nullptr;
    pAMediaCodec_getOutputBuffer AMediaCodec_getOutputBuffer = nullptr;
    pAMediaCodec_releaseOutputBuffer AMediaCodec_releaseOutputBuffer = nullptr;
    pAMediaCodec_getOutputFormat AMediaCodec_getOutputFormat = nullptr;
    pAMediaCodec_setParameters AMediaCodec_setParameters = nullptr;
    // API 28. Optional: it names the component in a log line and nothing depends on it, so a
    // device without it still encodes.
    pAMediaCodec_getName AMediaCodec_getName = nullptr;
    pAMediaCodec_releaseName AMediaCodec_releaseName = nullptr;

    pAMediaFormat_new AMediaFormat_new = nullptr;
    pAMediaFormat_delete AMediaFormat_delete = nullptr;
    pAMediaFormat_setString AMediaFormat_setString = nullptr;
    pAMediaFormat_setInt32 AMediaFormat_setInt32 = nullptr;
    pAMediaFormat_getBuffer AMediaFormat_getBuffer = nullptr;
    pAMediaFormat_toString AMediaFormat_toString = nullptr;

private:
    friend class android::base::NoDestructor<MediaCodecLib>;

    MediaCodecLib();
    ~MediaCodecLib() = delete;

    bool LoadFunctions();

    bool is_supported_ = false;
};
