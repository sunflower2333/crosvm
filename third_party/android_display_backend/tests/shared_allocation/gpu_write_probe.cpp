// Copyright 2026 The Android Open Source Project
// SPDX-License-Identifier: Apache-2.0
// Production Virgl/MSM/KGSL command-processor writes into an authentic AHB.
// This is a GPU-write/retirement proof, not a shader-render or display proof.
#define VIRGL_RENDERER_UNSTABLE_APIS
extern "C" {
#include "virglrenderer.h"
#include "virglrenderer_hw.h"
#include "drm/msm/msm_proto.h"
#include "drm/drm-uapi/msm_drm.h"
}
// Virgl's internal C overflow macros otherwise shadow libc++ chrono members.
#undef is_negative
#undef is_non_negative
#include "shared_ahb_allocation.h"
#include <atomic>
#include <cerrno>
#include <cstring>
#include <fcntl.h>
#include <inttypes.h>
#include <linux/dma-buf.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>
#include <vector>

namespace {
constexpr uint32_t kContext = 702;
constexpr uint32_t kControl = 80;
constexpr uint32_t kCommands = 81;
constexpr uint32_t kSentinel = 0x11223344;
unsigned assertions = 0;
std::atomic<uint64_t> retired{0};
std::atomic<bool> invalidFence{false};
bool check(bool ok, const char* what) {
    if (ok) { ++assertions; return true; }
    fprintf(stderr, "FAIL %s errno=%d\n", what, errno);
    return false;
}
void timedOut(int) {
    constexpr char message[] = "FAIL GPU-write watchdog; no reuse granted\n";
    (void)write(STDERR_FILENO, message, sizeof(message) - 1);
    _exit(124);
}
void fence(void*, uint32_t context, uint32_t ring, uint64_t value) {
    if (context != kContext || ring != 1) invalidFence.store(true);
    retired.store(value, std::memory_order_release);
}
void error(const char* message) { fprintf(stderr, "ALLOCATOR %s\n", message); }

struct Allocation {
    SharedAllocationDescription d{};
    void* holder = nullptr;
    android::base::unique_fd fd;
    uint32_t resource = 0;
    uint64_t nativeId = 0;
    struct stat identity{};
    ~Allocation() {
        if (resource) virgl_renderer_resource_unref(resource);
        android_display_destroy_shared_ahb(holder);
    }
    AHardwareBuffer* buffer() const { return crosvm_shared_ahb_get_buffer(holder); }
};

struct Probe {
    bool renderer = false, context = false, pending = false, control = false;
    msm_shmem* shmem = nullptr;
    uint32_t sequence = 0, queue = 0, submissions = 0;
    uint64_t va = 0, vaSize = 0;
    std::vector<std::unique_ptr<Allocation>> allocations;
    ~Probe() {
        // A failed retirement must never run our ordinary resource reuse path.
        // Process exit closes the isolated KGSL context and leaves kernel GPU
        // ownership cleanup to KGSL; no resource is returned to another writer.
        if (pending) {
            fprintf(stderr, "FAIL unretired GPU work; isolated process exit\n");
            fflush(nullptr);
            _exit(1);
        }
        if (context) virgl_renderer_context_destroy(kContext);
        allocations.clear();
        if (control) {
            if (shmem) virgl_renderer_resource_unmap(kControl);
            virgl_renderer_resource_unref(kControl);
        }
        if (renderer) virgl_renderer_cleanup(nullptr);
    }
    bool healthy() const {
        return shmem && __atomic_load_n(&shmem->async_error, __ATOMIC_ACQUIRE) == 0 &&
               __atomic_load_n(&shmem->global_faults, __ATOMIC_ACQUIRE) == 0 &&
               !invalidFence.load();
    }
    bool send(void* data, size_t size, uint32_t command) {
        auto* h = static_cast<vdrm_ccmd_req*>(data);
        h->cmd = command;
        h->len = size;
        h->seqno = ++sequence;
        h->rsp_off = 0;
        const int result = virgl_renderer_submit_cmd(data, kContext, size / 4);
        if (result) fprintf(stderr, "CCMD command=%u status=%d\n", command, result);
        return check(result == 0 && healthy(), "production native command and fault counters");
    }
    template<typename T> bool ioctlSimple(uint32_t command, T& payload) {
        std::vector<uint32_t> wire((sizeof(msm_ccmd_ioctl_simple_req) + sizeof(T)) / 4);
        auto* req = reinterpret_cast<msm_ccmd_ioctl_simple_req*>(wire.data());
        req->cmd = command;
        memcpy(req->payload, &payload, sizeof(payload));
        if (!send(wire.data(), wire.size() * 4, MSM_CCMD_IOCTL_SIMPLE)) return false;
        auto* rsp = reinterpret_cast<msm_ccmd_ioctl_simple_rsp*>(
                reinterpret_cast<uint8_t*>(shmem) + shmem->base.rsp_mem_offset);
        if (!check(rsp->ret == 0 && rsp->hdr.len == sizeof(*rsp) + sizeof(T),
                "native ioctl response")) return false;
        memcpy(&payload, rsp->payload, sizeof(payload));
        return true;
    }
    bool initialize() {
        virgl_renderer_callbacks callbacks{};
        callbacks.version = 3;
        callbacks.write_context_fence = fence;
        if (!check(virgl_renderer_init(nullptr, VIRGL_RENDERER_NO_VIRGL |
                VIRGL_RENDERER_DRM | VIRGL_RENDERER_ASYNC_FENCE_CB, &callbacks) == 0,
                "production Virgl initialization")) return false;
        renderer = true;
        uint32_t maxVersion = 0, capSize = 0;
        virgl_renderer_get_cap_set(VIRGL_RENDERER_CAPSET_DRM, &maxVersion, &capSize);
        if (!check(capSize == sizeof(virgl_renderer_capset_drm), "exact native capset size")) return false;
        virgl_renderer_capset_drm caps{};
        virgl_renderer_fill_caps(VIRGL_RENDERER_CAPSET_DRM, maxVersion, &caps);
        printf("CAPSET chip_id=%" PRIx64 " gpu_id=%u native_type=%u\n",
               caps.u.msm.chip_id, caps.u.msm.gpu_id, caps.context_type);
        // The packet helper below is the Adreno a5xx+ type7 format. Keep this
        // device proof bounded to the A830 already used by this workspace.
        if (!check(caps.context_type == VIRTGPU_DRM_CONTEXT_MSM &&
                   caps.u.msm.chip_id == 0x44050000, "supported A830 packet encoding")) return false;
        constexpr char name[] = "native-ahb-gpu-write-probe";
        if (!check(virgl_renderer_context_create_with_flags(kContext,
                VIRGL_RENDERER_CAPSET_DRM, sizeof(name) - 1, name) == 0,
                "native context creation")) return false;
        context = true;
        virgl_renderer_resource_create_blob_args args{};
        args.res_handle = kControl;
        args.ctx_id = kContext;
        args.blob_mem = VIRGL_RENDERER_BLOB_MEM_HOST3D;
        args.blob_flags = VIRGL_RENDERER_BLOB_FLAG_USE_MAPPABLE;
        args.size = 4096;
        if (!check(virgl_renderer_resource_create_blob(&args) == 0, "native control blob")) return false;
        control = true;
        void* mapped = nullptr;
        uint64_t size = 0;
        if (!check(virgl_renderer_resource_map(kControl, &mapped, &size) == 0 &&
                mapped && size == 4096, "native control mapping")) return false;
        shmem = static_cast<msm_shmem*>(mapped);
        if (!check(shmem->base.rsp_mem_offset >= sizeof(msm_shmem) &&
                shmem->base.rsp_mem_offset < size - 256 && healthy(),
                "valid native response region")) return false;
        drm_msm_param param{};
        param.pipe = MSM_PIPE_3D0;
        param.param = MSM_PARAM_VA_START;
        if (!ioctlSimple(DRM_IOCTL_MSM_GET_PARAM, param)) return false;
        va = param.value;
        param.param = MSM_PARAM_VA_SIZE;
        if (!ioctlSimple(DRM_IOCTL_MSM_GET_PARAM, param)) return false;
        vaSize = param.value;
        drm_msm_submitqueue q{};
        if (!ioctlSimple(DRM_IOCTL_MSM_SUBMITQUEUE_NEW, q)) return false;
        queue = q.id;
        printf("CONTEXT context=%u queue=%u va=%" PRIx64 " va_size=%" PRIx64 "\n",
               kContext, queue, va, vaSize);
        return check(queue && vaSize >= 32 * 1024 * 1024, "bounded context address range");
    }
    Allocation* allocate(uint32_t resource, uint32_t width, uint32_t height,
                         uint32_t fourcc, uint64_t iova) {
        auto a = std::make_unique<Allocation>();
        int raw = -1;
        a->holder = android_display_create_shared_ahb_v2(width, height, fourcc, &a->d, &raw, error);
        a->fd.reset(raw);
        if (!check(a->holder && raw >= 0 && fstat(raw, &a->identity) == 0,
                "authentic allocation and DMA identity")) return nullptr;
        if (!check(AHardwareBuffer_getId(a->buffer(), &a->nativeId) == 0 && a->nativeId,
                "native Android buffer identity")) return nullptr;
        android::base::unique_fd duplicate(fcntl(raw, F_DUPFD_CLOEXEC, 0));
        virgl_renderer_resource_import_blob_args import{};
        import.res_handle = resource;
        import.blob_mem = VIRGL_RENDERER_BLOB_MEM_HOST3D;
        import.fd_type = VIRGL_RENDERER_BLOB_FD_TYPE_DMABUF;
        import.fd = duplicate.get();
        import.size = a->d.allocation_size;
        if (!check(duplicate.get() >= 0 && virgl_renderer_resource_import_blob(&import) == 0,
                "production native DMA import")) return nullptr;
        (void)duplicate.release();
        a->resource = resource;
        virgl_renderer_native_resource_info info{};
        info.version = 1; info.size = sizeof(info);
        if (!check(virgl_renderer_ctx_attach_resource_checked(kContext, resource, &info) == 0 &&
                info.allocation_size == a->d.allocation_size &&
                info.dma_device == a->identity.st_dev && info.dma_inode == a->identity.st_ino,
                "checked KGSL identity")) return nullptr;
        msm_ccmd_gem_set_iova_req bind{};
        bind.iova = iova; bind.res_id = resource;
        if (!send(&bind, sizeof(bind), MSM_CCMD_GEM_SET_IOVA)) return nullptr;
        printf("BOUND resource=%u buffer_id=%" PRIu64 " native_buffer_id=%" PRIu64
               " device=%" PRIu64 " inode=%" PRIu64
               " width=%u height=%u stride=%u size=%" PRIu64 " iova=%" PRIx64 "\n",
               resource, a->d.buffer_id, a->nativeId, uint64_t(a->identity.st_dev), uint64_t(a->identity.st_ino),
               width, height, a->d.plane_stride, a->d.allocation_size, iova);
        Allocation* result = a.get();
        allocations.push_back(std::move(a));
        return result;
    }
    bool submit(Allocation& commands, Allocation& target, uint32_t bytes) {
        std::vector<uint32_t> wire((sizeof(msm_ccmd_gem_submit_req) +
                2 * sizeof(drm_msm_gem_submit_bo) + sizeof(drm_msm_gem_submit_cmd)) / 4);
        auto* req = reinterpret_cast<msm_ccmd_gem_submit_req*>(wire.data());
        req->flags = MSM_PIPE_3D0 | MSM_SUBMIT_FENCE_SN_IN | MSM_SUBMIT_NO_IMPLICIT;
        req->queue_id = queue;
        req->nr_bos = 2; req->nr_cmds = 1; req->fence = ++submissions;
        drm_msm_gem_submit_bo bos[2]{};
        bos[0].flags = MSM_SUBMIT_BO_READ; bos[0].handle = commands.resource;
        bos[1].flags = MSM_SUBMIT_BO_WRITE; bos[1].handle = target.resource;
        memcpy(req->payload, bos, sizeof(bos));
        drm_msm_gem_submit_cmd cmd{};
        cmd.type = MSM_SUBMIT_CMD_BUF; cmd.size = bytes;
        memcpy(req->payload + sizeof(bos), &cmd, sizeof(cmd));
        pending = true;
        if (!send(wire.data(), wire.size() * 4, MSM_CCMD_GEM_SUBMIT)) return false;
        if (!check(virgl_renderer_context_create_fence(kContext, 0, 1, submissions) == 0,
                "real GPU ring retirement primitive")) return false;
        for (unsigned wait = 0; wait != 5000 && retired.load(std::memory_order_acquire) != submissions; ++wait) {
            if (!healthy()) return check(false, "GPU fault while retiring");
            struct timespec delay{0, 1000000};
            nanosleep(&delay, nullptr);
        }
        if (!check(retired.load(std::memory_order_acquire) == submissions && healthy(),
                "exact GPU retirement and zero faults")) return false;
        pending = false;
        printf("GPU_RETIRED submit=%u callback=%" PRIu64 " ring=1 async_error=%u global_faults=%u\n",
               submissions, retired.load(), shmem->async_error, shmem->global_faults);
        return true;
    }
};

// Same type7 odd parity encoding as Mesa freedreno/common/freedreno_pm4.h.
uint32_t packet7(uint32_t opcode, uint32_t count) {
    const auto odd = [](uint32_t n) { return uint32_t(1 ^ (__builtin_popcount(n) & 1)); };
    return 0x70000000 | count | (odd(count) << 15) | (opcode << 16) | (odd(opcode) << 23);
}
uint32_t pattern(uint32_t x, uint32_t y, uint32_t pass) {
    return 0xff000000 | ((x * 17 + pass * 41) & 255) |
            (((y * 23 + pass * 53) & 255) << 8) | (((x ^ y ^ (pass * 71)) & 255) << 16);
}
bool fillCommands(Allocation& commands, const Allocation& target, uint64_t targetVa,
                  uint32_t pass, uint32_t& bytes) {
    const auto& d = target.d;
    bytes = (d.width + 3) * d.height * 4;
    if (!check(bytes <= commands.d.width * commands.d.height * 4 &&
            targetVa + d.plane_offset >= targetVa, "bounded command and destination ranges")) return false;
    void* mapped = nullptr;
    if (!check(AHardwareBuffer_lock(commands.buffer(), AHARDWAREBUFFER_USAGE_CPU_WRITE_OFTEN,
            -1, nullptr, &mapped) == 0 && mapped, "command writer admission")) return false;
    auto* out = static_cast<uint32_t*>(mapped);
    for (uint32_t y = 0; y < d.height; ++y) {
        const uint64_t address = targetVa + d.plane_offset + uint64_t(y) * d.plane_stride;
        *out++ = packet7(0x3d, d.width + 2); // CP_MEM_WRITE; no image source or blit.
        *out++ = address; *out++ = address >> 32;
        for (uint32_t x = 0; x < d.width; ++x) *out++ = pattern(x, y, pass);
    }
    return check(AHardwareBuffer_unlock(commands.buffer(), nullptr) == 0,
                 "publish command bytes before submit");
}
bool prepare(Allocation& target) {
    void* mapped = nullptr;
    if (!check(AHardwareBuffer_lock(target.buffer(), AHARDWAREBUFFER_USAGE_CPU_WRITE_OFTEN,
            -1, nullptr, &mapped) == 0 && mapped, "initial sentinel writer admission")) return false;
    for (uint32_t y = 0; y < target.d.height; ++y) {
        auto* row = reinterpret_cast<uint32_t*>(static_cast<uint8_t*>(mapped) + y * target.d.plane_stride);
        for (uint32_t x = 0; x < target.d.width; ++x) row[x] = kSentinel;
    }
    return check(AHardwareBuffer_unlock(target.buffer(), nullptr) == 0, "release initial sentinel writer");
}
bool pixels(const Allocation& target, const void* memory, uint32_t pass, uint64_t& hash) {
    hash = 14695981039346656037ull;
    for (uint32_t y = 0; y < target.d.height; ++y) {
        const auto* row = reinterpret_cast<const uint32_t*>(
                static_cast<const uint8_t*>(memory) + y * target.d.plane_stride);
        for (uint32_t x = 0; x < target.d.width; ++x) {
            const uint32_t value = row[x];
            if (value != pattern(x, y, pass)) {
                fprintf(stderr, "PIXEL_MISMATCH x=%u y=%u got=%08x expected=%08x\n",
                        x, y, value, pattern(x, y, pass));
                return false;
            }
            hash = (hash ^ value) * 1099511628211ull;
        }
    }
    return true;
}
bool verify(Allocation& target, uint32_t pass) {
    void* mapped = nullptr;
    if (!check(AHardwareBuffer_lock(target.buffer(), AHARDWAREBUFFER_USAGE_CPU_READ_OFTEN,
            -1, nullptr, &mapped) == 0 && mapped, "AHB read only after GPU retirement")) return false;
    uint64_t ahbHash = 0, dmaHash = 0;
    const bool ahbPixels = pixels(target, mapped, pass, ahbHash);
    const int unlocked = AHardwareBuffer_unlock(target.buffer(), nullptr);
    if (!check(ahbPixels && unlocked == 0, "every AHB pixel matches GPU payload")) return false;
    dma_buf_sync sync{DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ};
    if (!check(ioctl(target.fd.get(), DMA_BUF_IOCTL_SYNC, &sync) == 0,
               "DMA alias read cache admission")) return false;
    mapped = mmap(nullptr, target.d.allocation_size, PROT_READ, MAP_SHARED, target.fd.get(), 0);
    bool dmaPixels = mapped != MAP_FAILED && pixels(target,
            static_cast<const uint8_t*>(mapped) + target.d.plane_offset, pass, dmaHash);
    if (mapped != MAP_FAILED) munmap(mapped, target.d.allocation_size);
    sync.flags = DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ;
    const int synced = ioctl(target.fd.get(), DMA_BUF_IOCTL_SYNC, &sync);
    if (!check(dmaPixels && synced == 0 && dmaHash == ahbHash,
               "same GPU pixels visible through original DMA alias")) return false;
    uint32_t type = 0; int raw = -1;
    const int exported = virgl_renderer_resource_export_blob(target.resource, &type, &raw);
    android::base::unique_fd fd(raw);
    struct stat identity{};
    uint64_t bufferId = 0;
    if (!check(exported == 0 && type == VIRGL_RENDERER_BLOB_FD_TYPE_DMABUF &&
            fstat(fd.get(), &identity) == 0 && identity.st_dev == target.identity.st_dev &&
            identity.st_ino == target.identity.st_ino &&
            AHardwareBuffer_getId(target.buffer(), &bufferId) == 0 && bufferId == target.nativeId &&
            crosvm_shared_ahb_get_description(target.holder)->buffer_id == target.d.buffer_id,
            "post-submit native AHB and renderer export preserve identity")) return false;
    printf("GPU_PIXELS resource=%u buffer_id=%" PRIu64 " native_buffer_id=%" PRIu64
           " device=%" PRIu64 " inode=%" PRIu64
           " pass=%u pixels=%u ahb_hash=%016" PRIx64 " dma_hash=%016" PRIx64 "\n",
           target.resource, target.d.buffer_id, bufferId, uint64_t(identity.st_dev), uint64_t(identity.st_ino),
           pass, target.d.width * target.d.height, ahbHash, dmaHash);
    return true;
}
}

int main(int argc, char** argv) {
    if (argc != 2 || (strcmp(argv[1], "rgba") && strcmp(argv[1], "bgra"))) {
        fprintf(stderr, "usage: %s rgba|bgra\n", argv[0]); return 2;
    }
    signal(SIGALRM, timedOut); alarm(45); setvbuf(stdout, nullptr, _IONBF, 0);
    const uint32_t fourcc = !strcmp(argv[1], "bgra") ? 0x34325241 : 0x34324241;
    Probe probe;
    if (!probe.initialize()) return 1;
    auto* commands = probe.allocate(kCommands, 1024, 256, 0x34324241, probe.va + 0x100000);
    if (!commands) return 1;
    uint32_t resource = 82;
    for (const auto [width, height] : {std::pair{64u, 64u}, {513u, 257u}}) {
        const uint64_t targetVa = probe.va + uint64_t(resource - 80) * 0x400000;
        auto* target = probe.allocate(resource++, width, height, fourcc, targetVa);
        if (!target || !prepare(*target)) return 1;
        for (uint32_t pass = 1; pass != 3; ++pass) {
            uint32_t bytes = 0;
            if (!fillCommands(*commands, *target, targetVa, pass, bytes) ||
                    !probe.submit(*commands, *target, bytes) || !verify(*target, pass)) return 1;
        }
    }
    alarm(0);
    printf("RESULT passed=1 assertions=%u gpu_submissions=%u retired=%" PRIu64
           " target_allocations=2 command_allocations=1 gpu_image_blit_commands=0"
           " target_cpu_writes=initial_sentinel_only cpu_reads=verification_only"
           " guest_mapping=0 surfacecontrol=0 shader_rendering=0\n",
           assertions, probe.submissions, retired.load());
    return 0;
}
