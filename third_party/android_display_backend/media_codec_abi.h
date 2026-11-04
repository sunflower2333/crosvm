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

#include <stddef.h>
#include <stdint.h>
#include <sys/types.h>

// The slice of the media NDK this backend uses, declared here rather than included.
//
// Same shape and same reason as surface_control_abi.h beside it: libmediandk is resolved with
// dlopen at run time, so the build needs the ABI and not the library. It buys two things. The
// module keeps its current dependency list -- adding libmediandk to Android.bp would pull the
// whole media stack into a tree that is deliberately a minimal manifest -- and "this phone has no
// H.264 encoder to talk to" stays a run-time answer the caller can fall back from, which is the
// same way the Vulkan blit driver and ASurfaceControl are already treated here.
//
// Every declaration below is copied from the public NDK headers (NdkMediaCodec.h,
// NdkMediaFormat.h, NdkMediaError.h). Nothing in this module may include those headers as well:
// the types would then be defined twice.

extern "C" {

typedef struct AMediaCodec AMediaCodec;
typedef struct AMediaFormat AMediaFormat;
typedef struct AMediaCrypto AMediaCrypto;
typedef struct ANativeWindow ANativeWindow;

typedef int32_t media_status_t;
constexpr media_status_t AMEDIA_OK = 0;

// NdkMediaCodec.h: what a dequeued output buffer covers and what it is.
typedef struct AMediaCodecBufferInfo {
    int32_t offset;
    int32_t size;
    int64_t presentationTimeUs;
    uint32_t flags;
} AMediaCodecBufferInfo;

// Negative returns from AMediaCodec_dequeueOutputBuffer. TRY_AGAIN_LATER is the ordinary "nothing
// ready yet"; the other two are notifications, not errors, and both leave nothing to release.
constexpr ssize_t AMEDIACODEC_INFO_TRY_AGAIN_LATER = -1;
constexpr ssize_t AMEDIACODEC_INFO_OUTPUT_FORMAT_CHANGED = -2;
constexpr ssize_t AMEDIACODEC_INFO_OUTPUT_BUFFERS_CHANGED = -3;

// AMediaCodecBufferInfo::flags.
constexpr uint32_t AMEDIACODEC_BUFFER_FLAG_KEY_FRAME = 1;
constexpr uint32_t AMEDIACODEC_BUFFER_FLAG_CODEC_CONFIG = 2;
constexpr uint32_t AMEDIACODEC_BUFFER_FLAG_END_OF_STREAM = 4;

// AMediaCodec_configure flags.
constexpr uint32_t AMEDIACODEC_CONFIGURE_FLAG_ENCODE = 1;

// MediaFormat keys. Spelled out rather than taken from the AMEDIAFORMAT_KEY_* symbols, which are
// exported *data* -- resolving them through dlsym means dereferencing a const char* const, and
// getting that indirection wrong yields a key that is silently ignored rather than a link error.
constexpr const char* kFormatKeyMime = "mime";
constexpr const char* kFormatKeyWidth = "width";
constexpr const char* kFormatKeyHeight = "height";
constexpr const char* kFormatKeyColorFormat = "color-format";
constexpr const char* kFormatKeyBitRate = "bitrate";
constexpr const char* kFormatKeyFrameRate = "frame-rate";
constexpr const char* kFormatKeyIFrameInterval = "i-frame-interval";
// MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME. Any value; the presence of the key is the request.
constexpr const char* kFormatKeyRequestSync = "request-sync";
// The out-of-band codec-specific data an AVC encoder reports once its output format settles:
// csd-0 is the SPS and csd-1 the PPS, each already an Annex-B NAL unit with its start code.
constexpr const char* kFormatKeyCsd0 = "csd-0";
constexpr const char* kFormatKeyCsd1 = "csd-1";

constexpr const char* kMimeTypeAvc = "video/avc";

// MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface: the input comes from a Surface, not from
// buffers the caller fills. This is what makes AMediaCodec_createInputSurface legal.
constexpr int32_t kColorFormatSurface = 0x7F000789;

using pAMediaCodec_createEncoderByType = AMediaCodec* (*)(const char* mime_type);
using pAMediaCodec_configure = media_status_t (*)(AMediaCodec*, const AMediaFormat*,
                                                  ANativeWindow* surface, AMediaCrypto* crypto,
                                                  uint32_t flags);
using pAMediaCodec_createInputSurface = media_status_t (*)(AMediaCodec*, ANativeWindow** surface);
using pAMediaCodec_start = media_status_t (*)(AMediaCodec*);
using pAMediaCodec_stop = media_status_t (*)(AMediaCodec*);
using pAMediaCodec_delete = media_status_t (*)(AMediaCodec*);
using pAMediaCodec_signalEndOfInputStream = media_status_t (*)(AMediaCodec*);
using pAMediaCodec_dequeueOutputBuffer = ssize_t (*)(AMediaCodec*, AMediaCodecBufferInfo* info,
                                                     int64_t timeoutUs);
using pAMediaCodec_getOutputBuffer = uint8_t* (*)(AMediaCodec*, size_t idx, size_t* out_size);
using pAMediaCodec_releaseOutputBuffer = media_status_t (*)(AMediaCodec*, size_t idx, bool render);
using pAMediaCodec_getOutputFormat = AMediaFormat* (*)(AMediaCodec*);
using pAMediaCodec_setParameters = media_status_t (*)(AMediaCodec*, const AMediaFormat* params);
using pAMediaCodec_getName = media_status_t (*)(AMediaCodec*, char** out_name);
using pAMediaCodec_releaseName = void (*)(AMediaCodec*, char* name);

using pAMediaFormat_new = AMediaFormat* (*)(void);
using pAMediaFormat_delete = media_status_t (*)(AMediaFormat*);
using pAMediaFormat_setString = void (*)(AMediaFormat*, const char* name, const char* value);
using pAMediaFormat_setInt32 = void (*)(AMediaFormat*, const char* name, int32_t value);
using pAMediaFormat_getBuffer = bool (*)(AMediaFormat*, const char* name, void** data,
                                         size_t* size);
using pAMediaFormat_toString = const char* (*)(AMediaFormat*);

} // extern "C"
