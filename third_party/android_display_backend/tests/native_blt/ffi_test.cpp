// SPDX-License-Identifier: Apache-2.0
#include "native_blt.h"
#include <cerrno>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <fcntl.h>
#include <iostream>
#include <memory>
#include <sys/stat.h>
#include <unistd.h>
#define LOG(level) std::cerr
struct SharedAllocationDescription {
    uint32_t version, size;
    uint64_t buffer_id, allocation_size, modifier, plane_offset;
    uint32_t plane_stride, width, height, fourcc, plane_count, layout_flags;
};
struct FakeAllocation {
    SharedAllocationDescription value;
    int fd;
    const SharedAllocationDescription& description() const { return value; }
};
using Holder = std::shared_ptr<FakeAllocation>;
// INSERT_REAL_DESCRIPTION_HELPER
namespace android { namespace base {
struct unique_fd {
    explicit unique_fd(int value) : value(value) {}
    ~unique_fd() { if (value >= 0) close(value); }
    int get() const { return value; }
    int value;
};
}}
namespace android_display {
constexpr uint32_t kSharedAhbVerifiedLinearAlias = 1;
constexpr uint32_t kSharedAhbRgbaFourcc = 0x34324241, kSharedAhbBgraFourcc = 0x34325241;
}
int crosvm_shared_ahb_duplicate_dma_buf(void* h) { return dup(static_cast<Holder*>(h)->get()->fd); }
void* crosvm_shared_ahb_get_buffer(void* h) { return h; }
struct Result {
    bool success = true;
    bool ok() const { return success; }
    const char* error() const { return "injected failure"; }
    int operator*() const { return 41; }
};
struct FakeBridge {
    bool retireOk = true, importOk = true, copyOk = true;
    unsigned retires = 0, imports = 0, copies = 0;
    uint32_t sourceWidth = 0, sourceHeight = 0, targetWidth = 0, targetHeight = 0;
    Result retireCompletedCopies() { ++retires; return {retireOk}; }
    int64_t importDmabuf(int, uint32_t, uint32_t, uint64_t, bool, uint32_t w, uint32_t h, uint32_t) {
        ++imports; sourceWidth=w; sourceHeight=h; return importOk ? 11 : 0;
    }
    Result blit(int64_t, void*, int, bool, const NativeBltRegion*, uint32_t, uint32_t w, uint32_t h) {
        ++copies; targetWidth=w; targetHeight=h; return {copyOk};
    }
};
struct NativeBltContext { FakeBridge bridge; bool poisoned = false; };
// INSERT_REAL_COPY_FFI
unsigned checks;
void check(bool ok, const char* label) { ++checks; if (!ok) { std::fprintf(stderr,"FAIL %s\n", label); std::exit(1); } }
int main() {
    int sourceFd=open("/dev/null",O_RDONLY), targetFd=open("/dev/zero",O_RDONLY);
    check(sourceFd>=0 && targetFd>=0, "distinct inode fixtures");
    Holder source = std::make_shared<FakeAllocation>(FakeAllocation{{1,64,17,65536,0,0,256,61,43,0x34325241,1,1}, sourceFd});
    Holder target = std::make_shared<FakeAllocation>(FakeAllocation{{1,64,29,65536,0,0,512,73,49,0x34324241,1,1}, targetFd});
    NativeBltContext ctx;
    const NativeBltRegion valid{1,2,7,8,23,17}; int fd=999;
    check(android_display_native_blt_copy(&ctx,&source,&target,&valid,1,&fd)==0, "distinct holders survive thread-local description reuse");
    check(fd==41 && ctx.bridge.imports==1 && ctx.bridge.copies==1, "copy admitted exactly once");
    check(ctx.bridge.sourceWidth==61 && ctx.bridge.sourceHeight==43 && ctx.bridge.targetWidth==73 && ctx.bridge.targetHeight==49,
        "separate source and target descriptions retained");
    const NativeBltRegion invalid[] = {valid,{0,0,73,0,1,1}};
    check(android_display_native_blt_copy(&ctx,&source,&target,invalid,2,&fd)==-EINVAL && fd==-1 && ctx.bridge.imports==1,
        "late invalid rectangle does not import or mutate");
    Holder alias=std::make_shared<FakeAllocation>(*target); alias->fd=sourceFd;
    check(android_display_native_blt_copy(&ctx,&source,&alias,&valid,1,&fd)==-EINVAL && ctx.bridge.imports==1, "different holders sharing same backing rejected");
    check(android_display_native_blt_copy(&ctx,&source,&source,&valid,1,&fd)==-EINVAL, "same holder rejected");
    ctx.bridge.copyOk=false;
    check(android_display_native_blt_copy(&ctx,&source,&target,&valid,1,&fd)==-EIO && ctx.poisoned && fd==-1, "copy error poisons and clears completion");
    const unsigned old=ctx.bridge.imports;
    check(android_display_native_blt_copy(&ctx,&source,&target,&valid,1,&fd)==-EIO && ctx.bridge.imports==old, "poisoned context cannot resubmit");
    NativeBltContext pending; pending.bridge.retireOk=false;
    check(android_display_native_blt_copy(&pending,&source,&target,&valid,1,&fd)==-EIO && pending.poisoned && !pending.bridge.imports,
        "unretired prior imports retained");
    close(sourceFd); close(targetFd);
    std::printf("native Blt FFI: %u checks PASS\n",checks);
}
