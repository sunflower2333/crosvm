#pragma once
#include <cstdint>
struct ANativeWindow { int32_t width = 64, height = 32; };
constexpr int32_t ANATIVEWINDOW_TRANSFORM_ROTATE_90 = 4;
constexpr int32_t ANATIVEWINDOW_TRANSFORM_ROTATE_180 = 3;
constexpr int32_t ANATIVEWINDOW_TRANSFORM_ROTATE_270 = 7;
constexpr int8_t ANATIVEWINDOW_FRAME_RATE_COMPATIBILITY_AT_LEAST = 2;
constexpr int8_t ANATIVEWINDOW_CHANGE_FRAME_RATE_ONLY_IF_SEAMLESS = 0;
inline int32_t ANativeWindow_getWidth(ANativeWindow* w) { return w->width; }
inline int32_t ANativeWindow_getHeight(ANativeWindow* w) { return w->height; }
