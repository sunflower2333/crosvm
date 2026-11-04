#pragma once
#include <cstdint>
struct AHardwareBuffer { int id; int refs; uint32_t width = 64, height = 32, layers = 1, format = 5, stride = 0; };
struct AHardwareBuffer_Desc { uint32_t width, height, layers, format; uint64_t usage; uint32_t stride, rfu0; uint64_t rfu1; };
#define AHARDWAREBUFFER_USAGE_GPU_SAMPLED_IMAGE (1ULL << 8)
inline void AHardwareBuffer_describe(const AHardwareBuffer* b, AHardwareBuffer_Desc* d) {
    *d = {}; d->width = b->width; d->height = b->height; d->layers = b->layers;
    d->usage = AHARDWAREBUFFER_USAGE_GPU_SAMPLED_IMAGE;
    d->format = b->format; d->stride = b->stride ? b->stride : b->width;
}
inline void AHardwareBuffer_acquire(AHardwareBuffer* b) { ++b->refs; }
inline void AHardwareBuffer_release(AHardwareBuffer* b) { --b->refs; }
