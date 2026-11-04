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

#include "media_codec_dl.h"

#include <android-base/logging.h>
#include <dlfcn.h>

#define LOAD_REQUIRED(lib, func)                                        \
    do {                                                                \
        func = reinterpret_cast<p##func>(dlsym(lib, #func));            \
        if (!func) {                                                    \
            LOG(INFO) << "libmediandk has no " #func "; no hw encoder"; \
            return false;                                               \
        }                                                               \
    } while (0)

#define LOAD_OPTIONAL(lib, func) func = reinterpret_cast<p##func>(dlsym(lib, #func))

MediaCodecLib& MediaCodecLib::GetInstance() {
    static android::base::NoDestructor<MediaCodecLib> instance;
    return *instance;
}

MediaCodecLib::MediaCodecLib() {
    is_supported_ = LoadFunctions();
}

bool MediaCodecLib::IsSupported() const {
    return is_supported_;
}

bool MediaCodecLib::LoadFunctions() {
    void* lib = dlopen("libmediandk.so", RTLD_NOW);
    if (lib == nullptr) {
        LOG(INFO) << "libmediandk.so not present: " << dlerror();
        return false;
    }

    LOAD_REQUIRED(lib, AMediaCodec_createEncoderByType);
    LOAD_REQUIRED(lib, AMediaCodec_configure);
    LOAD_REQUIRED(lib, AMediaCodec_createInputSurface);
    LOAD_REQUIRED(lib, AMediaCodec_start);
    LOAD_REQUIRED(lib, AMediaCodec_stop);
    LOAD_REQUIRED(lib, AMediaCodec_delete);
    LOAD_REQUIRED(lib, AMediaCodec_signalEndOfInputStream);
    LOAD_REQUIRED(lib, AMediaCodec_dequeueOutputBuffer);
    LOAD_REQUIRED(lib, AMediaCodec_getOutputBuffer);
    LOAD_REQUIRED(lib, AMediaCodec_releaseOutputBuffer);
    LOAD_REQUIRED(lib, AMediaCodec_getOutputFormat);
    LOAD_REQUIRED(lib, AMediaCodec_setParameters);
    LOAD_REQUIRED(lib, AMediaFormat_new);
    LOAD_REQUIRED(lib, AMediaFormat_delete);
    LOAD_REQUIRED(lib, AMediaFormat_setString);
    LOAD_REQUIRED(lib, AMediaFormat_setInt32);
    LOAD_REQUIRED(lib, AMediaFormat_getBuffer);
    LOAD_REQUIRED(lib, AMediaFormat_toString);

    LOAD_OPTIONAL(lib, AMediaCodec_getName);
    LOAD_OPTIONAL(lib, AMediaCodec_releaseName);

    return true;
}
