// Copyright 2026 The Android Open Source Project
// SPDX-License-Identifier: Apache-2.0
// Bounded actual allocation/import ownership probe. No GPU commands or guest map.
#define VIRGL_RENDERER_UNSTABLE_APIS
extern "C" {
#include "virglrenderer.h"
#include "virglrenderer_hw.h"
}
#include "shared_ahb_allocation.h"
#ifndef __user
#define __user
#endif
#include "drm/drm2kgsl/msm_kgsl.h"
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <signal.h>
#include <stdio.h>
#include <sys/stat.h>
#include <sys/ioctl.h>
#include <unistd.h>
#include <cstring>
#include <vector>

using Description = SharedAllocationDescription;

namespace {
constexpr uint32_t kRgbaFourcc = 0x34324241;
unsigned assertions = 0;
bool check(bool result, const char* name) {
    if (result) { ++assertions; return true; }
    fprintf(stderr, "FAIL %s\n", name);
    return false;
}
void timedOut(int) {
    constexpr char message[] = "FAIL shared-ahb import watchdog\n";
    (void)write(STDERR_FILENO, message, sizeof(message) - 1);
    _exit(124);
}
void error(const char* message) { fprintf(stderr, "ALLOCATOR %s\n", message); }
void fence(void*, uint32_t, uint32_t, uint64_t) {}
struct Renderer {
    bool initialized = false;
    ~Renderer() { if (initialized) virgl_renderer_cleanup(nullptr); }
};
struct Context {
    uint32_t id = 0;
    ~Context() { if (id) virgl_renderer_context_destroy(id); }
};
struct Allocation {
    Description description{};
    void* holder = nullptr;
    android::base::unique_fd fd;
    uint32_t resource = 0;
    ~Allocation() {
        if (resource) virgl_renderer_resource_unref(resource);
        android_display_destroy_shared_ahb(holder);
    }
};
virgl_renderer_native_resource_info request() {
    virgl_renderer_native_resource_info result{};
    result.version = 1;
    result.size = sizeof(result);
    return result;
}
bool sameFd(int fd, const struct stat& expected) {
    struct stat actual{};
    return fd >= 0 && fstat(fd, &actual) == 0 &&
        actual.st_dev == expected.st_dev && actual.st_ino == expected.st_ino;
}
bool import(Allocation& allocation, uint32_t resource) {
    android::base::unique_fd transferred(fcntl(allocation.fd.get(), F_DUPFD_CLOEXEC, 0));
    if (!check(transferred.get() >= 0, "duplicate import fd")) return false;
    virgl_renderer_resource_import_blob_args args{};
    args.res_handle = resource;
    args.blob_mem = VIRGL_RENDERER_BLOB_MEM_HOST3D;
    args.fd_type = VIRGL_RENDERER_BLOB_FD_TYPE_DMABUF;
    args.fd = transferred.get();
    args.size = allocation.description.allocation_size;
    const int ret = virgl_renderer_resource_import_blob(&args);
    if (!check(ret == 0, "actual Virgl import")) {
        fprintf(stderr, "IMPORT ret=%d\n", ret);
        return false;
    }
    (void)transferred.release(); // Exact ownership transfer only on success.
    allocation.resource = resource;
    return true;
}

// Diagnose the same real exporter before/after the native renderer's VBO
// reservation. This uses official ioctls, no GPU submission or alternate BO.
bool directImport(const Allocation& allocation, const char* stage, uint64_t flags) {
    android::base::unique_fd device(open("/dev/kgsl-3d0", O_RDWR | O_CLOEXEC));
    if (!check(device.get() >= 0, "open direct KGSL diagnostic fd")) return false;
    kgsl_gpuobj_import_dma_buf dma{}; dma.fd = allocation.fd.get();
    kgsl_gpuobj_import request{};
    request.priv = reinterpret_cast<uintptr_t>(&dma);
    request.priv_len = sizeof(dma);
    request.flags = flags;
    request.type = KGSL_USER_MEM_TYPE_DMABUF;
    const int ret = ioctl(device.get(), IOCTL_KGSL_GPUOBJ_IMPORT, &request);
    const int error = ret ? errno : 0;
    printf("DIRECT stage=%s ret=%d errno=%d requested_flags=%" PRIx64
           " returned_flags=%" PRIx64 " id=%u request_size=%zu private_size=%zu\n",
           stage, ret, error, flags, static_cast<uint64_t>(request.flags), request.id,
           sizeof(request), sizeof(dma));
    if (ret) return true; // Diagnostic observation; checked production attach still must pass.
    kgsl_gpuobj_info info{}; info.id = request.id;
    const int infoRet = ioctl(device.get(), IOCTL_KGSL_GPUOBJ_INFO, &info);
    printf("DIRECT_INFO stage=%s ret=%d errno=%d size=%" PRIu64 " flags=%" PRIx64
           " gpuaddr=%" PRIx64 "\n", stage, infoRet, infoRet ? errno : 0,
           static_cast<uint64_t>(info.size), static_cast<uint64_t>(info.flags),
           static_cast<uint64_t>(info.gpuaddr));
    kgsl_gpuobj_free release{}; release.id = request.id;
    return check(ioctl(device.get(), IOCTL_KGSL_GPUOBJ_FREE, &release) == 0,
                 "direct diagnostic object cleanup");
}
}

int main(int argc, char** argv) {
    if (argc > 2 || (argc == 2 && strcmp(argv[1], "rgba") && strcmp(argv[1], "bgra"))) {
        fprintf(stderr, "usage: %s [rgba|bgra]\n", argv[0]);
        return 2;
    }
    const uint32_t requestedFourcc = argc == 2 && !strcmp(argv[1], "bgra")
            ? 0x34325241 : kRgbaFourcc;
    signal(SIGALRM, timedOut);
    alarm(45);
    setvbuf(stdout, nullptr, _IONBF, 0);
    Description invalid{};
    int invalidFd = -1;
    void* invalidHolder = android_display_create_shared_ahb_v2(
            0, 64, requestedFourcc, &invalid, &invalidFd, nullptr);
    if (!check(!invalidHolder && invalidFd == -1, "invalid allocator dimensions rejected")) {
        android_display_destroy_shared_ahb(invalidHolder);
        if (invalidFd >= 0) close(invalidFd);
        return 1;
    }
    Renderer renderer;
    auto first = std::make_unique<Allocation>();
    int firstFd = -1;
    first->holder = android_display_create_shared_ahb_v2(
            64, 64, requestedFourcc,
            &first->description, &firstFd, error);
    first->fd.reset(firstFd);
    if (!check(first->holder && firstFd >= 0, "allocate identical backing before native VBO")) return 1;
    if (!directImport(*first, "before-native-init", 0)) return 1;
    virgl_renderer_callbacks callbacks{};
    callbacks.version = 3;
    callbacks.write_context_fence = fence;
    if (!check(virgl_renderer_init(nullptr, VIRGL_RENDERER_NO_VIRGL |
            VIRGL_RENDERER_DRM | VIRGL_RENDERER_ASYNC_FENCE_CB, &callbacks) == 0,
            "native Virgl renderer initialization")) return 1;
    renderer.initialized = true;
    if (!directImport(*first, "after-native-init", 0) ||
            !directImport(*first, "after-native-init-iocoherent", KGSL_MEMFLAGS_IOCOHERENT)) return 1;
    Context context;
    constexpr char name[] = "shared-ahb-import-probe";
    if (!check(virgl_renderer_context_create_with_flags(701, VIRGL_RENDERER_CAPSET_DRM,
            sizeof(name) - 1, name) == 0, "actual DRM context creation")) return 1;
    context.id = 701;
    std::vector<std::unique_ptr<Allocation>> retained;
    for (const auto [width, height] : {std::pair{64u, 64u}, {513u, 257u}, {1920u, 1080u}}) {
        auto allocation = first ? std::move(first) : std::make_unique<Allocation>();
        int raw = allocation->fd.get();
        if (!allocation->holder) {
            allocation->holder = android_display_create_shared_ahb_v2(
                    width, height, requestedFourcc,
                    &allocation->description, &raw, error);
            allocation->fd.reset(raw);
        }
        if (!check(allocation->holder && raw >= 0, "actual optional-helper AHB allocation")) return 1;
        const auto& d = allocation->description;
        struct stat identity{};
        if (!check(fstat(raw, &identity) == 0 && identity.st_ino != 0 &&
                lseek(raw, 0, SEEK_END) == static_cast<off_t>(d.allocation_size),
                "AHB source identity and exact size")) return 1;
        printf("ALLOCATION buffer_id=%" PRIu64 " size=%" PRIu64 " modifier=%" PRIu64
               " offset=%" PRIu64 " stride=%u width=%u height=%u fourcc=%08x"
               " device=%" PRIu64 " inode=%" PRIu64 "\n",
               d.buffer_id, d.allocation_size, d.modifier, d.plane_offset, d.plane_stride,
               d.width, d.height, d.fourcc, uint64_t(identity.st_dev), uint64_t(identity.st_ino));
        const uint32_t resource = 71 + retained.size();
        if (!import(*allocation, resource)) return 1;
        uint32_t mapInfo = 0xffffffff;
        const int noCache = virgl_renderer_resource_get_map_info(resource, &mapInfo);
        printf("UNCLASSIFIED resource=%u ret=%d map_info=%08x\n", resource, noCache, mapInfo);
        // The public getter rejects internal NONE instead of returning it as
        // usable cache information. Its output must remain untouched on error.
        if (!check(noCache == -EINVAL && mapInfo == 0xffffffff,
                "import alone does not guess map cache")) return 1;
        auto bad = request(); bad.version = 2;
        const auto original = bad;
        if (!check(virgl_renderer_ctx_attach_resource_checked(context.id, resource, &bad) != 0 &&
                memcmp(&bad, &original, sizeof(bad)) == 0,
                "invalid attachment version rejected without output mutation")) return 1;
        auto attached = request();
        const int ret = virgl_renderer_ctx_attach_resource_checked(context.id, resource, &attached);
        printf("ATTACH ret=%d resource=%u buffer_id=%" PRIu64 " size=%" PRIu64
               " width=%u height=%u stride=%u kernel_flags=%" PRIx64 " map_info=%u\n",
               ret, resource, d.buffer_id, d.allocation_size, width, height, d.plane_stride,
               attached.kernel_flags, attached.map_info);
        if (!check(ret == 0, "actual checked KGSL attachment")) return 1;
        if (!check(attached.version == 1 && attached.size == sizeof(attached) &&
                attached.ctx_id == context.id && attached.res_id == resource &&
                attached.allocation_size == d.allocation_size &&
                attached.dma_device == identity.st_dev && attached.dma_inode == identity.st_ino &&
                attached.reserved == 0, "kernel attachment matches exact allocation identity")) return 1;
        if (!check(attached.map_info >= VIRGL_RENDERER_MAP_CACHE_CACHED &&
                attached.map_info <= VIRGL_RENDERER_MAP_CACHE_WC &&
                virgl_renderer_resource_get_map_info(resource, &mapInfo) == 0 &&
                mapInfo == attached.map_info, "kernel cache classification published")) return 1;
        auto repeated = request();
        if (!check(virgl_renderer_ctx_attach_resource_checked(context.id, resource, &repeated) == 0 &&
                memcmp(&repeated, &attached, sizeof(attached)) == 0,
                "repeated attach preserves one allocation identity")) return 1;
        uint32_t type = 0;
        int exported = -1;
        const int exportRet = virgl_renderer_resource_export_blob(resource, &type, &exported);
        android::base::unique_fd exportedFd(exported);
        if (!check(exportRet == 0 && type == VIRGL_RENDERER_BLOB_FD_TYPE_DMABUF &&
                sameFd(exportedFd.get(), identity), "Virgl export preserves DMA backing identity")) return 1;
        virgl_renderer_resource_unref(resource);
        allocation->resource = 0;
        auto stale = request();
        if (!check(virgl_renderer_ctx_attach_resource_checked(context.id, resource, &stale) != 0,
                "released resource cannot attach")) return 1;
        if (!check(sameFd(raw, identity) && sameFd(exportedFd.get(), identity),
                "independent AHB and exported fd survive renderer resource release")) return 1;
        // Reuse only after confirmed synchronous cleanup, and without any GPU submit.
        if (!import(*allocation, resource)) return 1;
        auto recreated = request();
        if (!check(virgl_renderer_ctx_attach_resource_checked(context.id, resource, &recreated) == 0 &&
                memcmp(&recreated, &attached, sizeof(attached)) == 0,
                "resource ID reuse imports original backing after cleanup")) return 1;
        for (const auto& previous : retained) {
            if (!check(!sameFd(previous->fd.get(), identity) &&
                    previous->description.buffer_id != d.buffer_id,
                    "live AHB allocations remain distinct")) return 1;
        }
        retained.push_back(std::move(allocation));
    }
    virgl_renderer_context_destroy(context.id);
    context.id = 0;
    for (const auto& allocation : retained) {
        uint32_t type = 0; int raw = -1;
        const int ret = virgl_renderer_resource_export_blob(allocation->resource, &type, &raw);
        android::base::unique_fd fd(raw);
        struct stat identity{};
        if (!check(fstat(allocation->fd.get(), &identity) == 0 && ret == 0 &&
                sameFd(fd.get(), identity), "global backing survives context cleanup")) return 1;
    }
    retained.clear(); // Destroy all resource owners before renderer teardown.
    alarm(0);
    printf("RESULT passed=1 assertions=%u allocations=3 gpu_submissions=0 guest_mapping=0"
           " zero_copy_enabled=0\n", assertions);
    return 0;
}
