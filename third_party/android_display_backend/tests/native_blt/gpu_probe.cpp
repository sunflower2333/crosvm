// Copyright 2026 The Android Open Source Project
// SPDX-License-Identifier: Apache-2.0
#include "native_blt.h"
#include "native_blt_transfer.h"
#include "shared_ahb_allocation.h"
#include "consumer_release.h"
#include <android/hardware_buffer.h>
#include <poll.h>
#include <signal.h>
#include <unistd.h>
#include <cerrno>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <vector>

// A missing implementation must produce an explicit failing hardware oracle,
// including while building the RED probe before the transfer path exists.
#pragma weak android_display_native_blt_upload
#pragma weak android_display_native_blt_readback

namespace {
constexpr uint32_t rgba = 0x34324241, bgra = 0x34325241, xrgb = 0x34325258, sentinel = 0x971d59b3;
uint64_t checks = 0;
void check(bool ok, const char* message) {
    ++checks;
    if (!ok) { std::fprintf(stderr, "FAIL %s checks=%llu\n", message, (unsigned long long)checks); std::exit(1); }
}
void timeout(int) { constexpr char text[] = "FAIL Native Blt watchdog\n"; (void)write(2, text, sizeof(text)-1); _exit(124); }
void error(const char* message) { std::fprintf(stderr, "allocator: %s\n", message); }
uint32_t encoded(uint32_t canonical, uint32_t format) {
    if (format == xrgb) return canonical | 0xff000000;
    return format == bgra ? canonical : (canonical & 0xff00ff00) |
        ((canonical & 0x00ff0000) >> 16) | ((canonical & 0x000000ff) << 16);
}
uint32_t pattern(uint32_t x, uint32_t y) {
    return 0xff000000 | (((x*31+y*7+13)&255)<<16) | (((x*11+y*29+71)&255)<<8) | ((x*17+y*19+137)&255);
}
struct Allocation {
    void* holder = nullptr;
    SharedAllocationDescription description{};
    Allocation(uint32_t w, uint32_t h, uint32_t format) {
        int fd = -1;
        holder = crosvm_shared_ahb_create_v2(w, h, format, &description, &fd, error);
        check(holder && fd >= 0, "authentic AHB allocation"); close(fd);
        std::printf("AHB id=%llu extent=%ux%u stride=%u fourcc=%08x\n",
            (unsigned long long)description.buffer_id, w, h, description.plane_stride, format);
    }
    ~Allocation() { crosvm_shared_ahb_destroy(holder); }
    AHardwareBuffer* buffer() const { return crosvm_shared_ahb_get_buffer(holder); }
    void initialize(bool source, uint32_t salt = 0) {
        void* data = nullptr;
        check(AHardwareBuffer_lock(buffer(), AHARDWAREBUFFER_USAGE_CPU_WRITE_OFTEN, -1, nullptr, &data) == 0 && data,
            "oracle CPU initialize lock");
        AHardwareBuffer_Desc desc{}; AHardwareBuffer_describe(buffer(), &desc);
        auto* pixels = static_cast<uint32_t*>(data);
        for (uint32_t y=0; y<desc.height; ++y) for (uint32_t x=0; x<desc.stride; ++x)
            pixels[uint64_t(y)*desc.stride+x] = x<desc.width
                ? encoded(source ? (pattern(x,y)^salt) : sentinel, description.fourcc) : 0xa55aa55a;
        check(AHardwareBuffer_unlock(buffer(), nullptr) == 0, "oracle CPU initialize unlock");
    }
    void verify(const std::vector<uint32_t>& expected, const char* label) {
        void* data = nullptr;
        check(AHardwareBuffer_lock(buffer(), AHARDWAREBUFFER_USAGE_CPU_READ_OFTEN, -1, nullptr, &data) == 0 && data,
            "oracle CPU read after GPU completion");
        AHardwareBuffer_Desc desc{}; AHardwareBuffer_describe(buffer(), &desc);
        const auto* pixels = static_cast<const uint32_t*>(data);
        uint64_t paddingChanges = 0;
        for (uint32_t y=0; y<desc.height; ++y) for (uint32_t x=0; x<desc.stride; ++x) {
            if (x >= desc.width) { paddingChanges += pixels[uint64_t(y)*desc.stride+x] != 0xa55aa55a; continue; }
            const uint32_t actual = encoded(pixels[uint64_t(y)*desc.stride+x], description.fourcc);
            const uint32_t wanted = expected[uint64_t(y)*desc.width+x];
            if (actual != wanted) std::fprintf(stderr, "PIXEL %s x=%u y=%u actual=%08x expected=%08x\n", label,x,y,actual,wanted);
            check(actual == wanted, label);
        }
        check(AHardwareBuffer_unlock(buffer(), nullptr) == 0, "oracle CPU read unlock");
        std::printf("PIXELS %s count=%zu padding_changes=%llu\n", label, expected.size(), (unsigned long long)paddingChanges);
    }
};
bool synchronousNativeCopy=false;
void waitComplete(int fd) {
    if (fd < 0) { std::puts(synchronousNativeCopy ? "COMPLETE synchronous_native_wait=1" : "COMPLETE synchronous_export_fallback=1"); return; }
    pollfd p{fd, POLLIN, 0}; int result;
    do { result = poll(&p, 1, 2000); } while (result < 0 && errno == EINTR);
    const bool completed = result == 1 && (p.revents & POLLIN) && !(p.revents & (POLLERR|POLLNVAL)) &&
        android_display::consumerFenceStatus(fd) == 1;
    close(fd);
    check(completed, "real successful GPU completion fence");
    std::puts("COMPLETE sync_fd_signalled=1");
}
int transferProbe(bool large, bool skipUpload, bool skipReadback, bool fullOnly, bool xrgbOnly) {
    check(android_display_native_blt_upload && android_display_native_blt_readback,
          "ordinary/native GPU transfer entry points");
    void* context=android_display_native_blt_create(); check(context, "Vulkan transfer context");
    unsigned cases=0;
    for (uint32_t ordinaryFormat : {bgra,rgba,xrgb}) for (uint32_t nativeFormat : {bgra,rgba}) {
        if ((ordinaryFormat==xrgb) != xrgbOnly) continue;
        const uint32_t sw=large?3040:61, sh=large?1904:43;
        const uint32_t nw=large?3040:73, nh=large?1904:49;
        NativeBltCpuLayout inputLayout{sw,sh,(sw+19)*4,ordinaryFormat,uint64_t((sw+19)*4)*sh};
        // Unaligned ordinary bytes with poisoned row padding and end guards.
        std::vector<uint8_t> input(inputLayout.size+30,0x87);
        auto* inputBytes=input.data()+13;
        for (uint32_t y=0;y<sh;++y) for (uint32_t x=0;x<sw;++x) {
            auto pixel=encoded(pattern(x,y),ordinaryFormat);
            if (ordinaryFormat==xrgb) pixel=(pixel&0x00ffffff)|(((x+y)&255)<<24);
            std::memcpy(inputBytes+uint64_t(y)*inputLayout.stride+x*4,&pixel,4);
        }
        const auto untouchedInput=input;
        Allocation native(nw,nh,nativeFormat); native.initialize(false);
        std::vector<uint32_t> nativeExpected(uint64_t(nw)*nh,sentinel);
        NativeBltRegion clips[]={{1,2,7,8,23,17},{4,5,10,11,13,11},{40,29,49,35,17,12}};
        NativeBltRegion invalid[]={clips[0],{0,0,nw,0,1,1}};
        uint64_t transferred=999;
        check(android_display_native_blt_upload(context,native.holder,inputBytes,&inputLayout,invalid,2,&transferred)==-EINVAL && !transferred,
              "late invalid upload rejects complete request");
        native.verify(nativeExpected,"rejected-upload-unchanged");
        const uint32_t ow=large?3040:89, oh=large?1904:53;
        NativeBltCpuLayout outputLayout{ow,oh,(ow+13)*4,ordinaryFormat==xrgb?xrgb:(ordinaryFormat==bgra?rgba:bgra),uint64_t((ow+13)*4)*oh};
        for (unsigned pass=fullOnly?2u:0u;pass<(large?3u:2u);++pass) {
            if (pass==1) clips[0]={5,7,7,8,23,17};
            const NativeBltRegion full{0,0,0,0,sw,sh};
            const NativeBltRegion* selected=pass==2?&full:clips;
            const uint32_t count=pass==2?1:3;
            uint64_t wantedBytes=0;
            for (uint32_t i=0;i<count;++i) {
                const auto& r=selected[i]; wantedBytes+=uint64_t(r.width)*r.height*4;
                for(uint32_t y=0;y<r.height;++y) for(uint32_t x=0;x<r.width;++x)
                    nativeExpected[uint64_t(r.destination_y+y)*nw+r.destination_x+x]=pattern(r.source_x+x,r.source_y+y);
            }
            if (!skipUpload) {
                check(android_display_native_blt_upload(context,native.holder,inputBytes,&inputLayout,selected,count,&transferred)==0 && transferred==wantedBytes,
                      "GPU upload completed with exact byte accounting");
            }
            native.verify(nativeExpected,"ordinary-upload-destination-and-outside");
            check(input==untouchedInput,"ordinary upload source and padding preserved");
            // Independent readback input: recycled upload staging must never
            // satisfy this oracle if vkCmdCopyImageToBuffer is missing.
            const uint32_t salt=(0x005a37c1u+pass*0x00110307u)|(xrgbOnly?0x8a000000u:0u);
            native.initialize(true,salt);
            for(uint32_t y=0;y<nh;++y) for(uint32_t x=0;x<nw;++x)
                nativeExpected[uint64_t(y)*nw+x]=pattern(x,y)^salt;
            std::vector<uint8_t> output(outputLayout.size+30,0x59), expected=output;
            NativeBltRegion readClips[]={{0,0,3,2,21,15},{7,8,9,10,23,17},{49,35,57,39,17,12}};
            const auto* reads=pass==2?&full:readClips;
            NativeBltRegion badRead[]={readClips[0],{0,0,ow,0,1,1}};
            check(android_display_native_blt_readback(context,native.holder,output.data()+13,&outputLayout,badRead,2,&transferred)==-EINVAL && !transferred && output==expected,
                  "late invalid readback preserves all ordinary bytes");
            uint64_t readBytes=0;
            for (uint32_t i=0;i<count;++i) {
                const auto& r=reads[i]; readBytes+=uint64_t(r.width)*r.height*4;
                for(uint32_t y=0;y<r.height;++y) for(uint32_t x=0;x<r.width;++x) {
                    const uint32_t pixel=encoded(nativeExpected[uint64_t(r.source_y+y)*nw+r.source_x+x],outputLayout.fourcc);
                    std::memcpy(expected.data()+13+uint64_t(r.destination_y+y)*outputLayout.stride+uint64_t(r.destination_x+x)*4,&pixel,4);
                }
            }
            if (!skipReadback) {
                check(android_display_native_blt_readback(context,native.holder,output.data()+13,&outputLayout,reads,count,&transferred)==0 && transferred==readBytes,
                      "GPU readback completed with exact byte accounting");
            }
            for(size_t i=0;i<output.size();++i) {
                if(output[i]!=expected[i]) std::fprintf(stderr,"BYTE readback offset=%zu actual=%02x expected=%02x\n",i,output[i],expected[i]);
                check(output[i]==expected[i],"ordinary-readback-destination-and-outside");
            }
            native.verify(nativeExpected,"native-readback-source-preserved");
        }
        ++cases;
    }
    check(android_display_native_blt_destroy(context)==0,"drained transfer context destruction");
    std::printf("TRANSFER_RESULT passed=1 cases=%u checks=%llu skip_upload=%d skip_readback=%d native_cpu_copy=0 integrated_kmd=0\n",
        cases,(unsigned long long)checks,skipUpload,skipReadback);
    return 0;
}
}
int nativeCopy(void* context,void* source,void* destination,const NativeBltRegion* regions,uint32_t count,int* fd) {
    if (!synchronousNativeCopy) return android_display_native_blt_copy(context,source,destination,regions,count,fd);
    *fd=-1;
    return android_display_native_blt_copy_wait(context,source,destination,regions,count);
}
int main(int argc, char** argv) {
    const bool skip = argc == 2 && !std::strcmp(argv[1], "--skip-copy");
    synchronousNativeCopy=argc==2 && !std::strcmp(argv[1],"--sync-large");
    const bool large = synchronousNativeCopy || (argc == 2 && !std::strcmp(argv[1], "--large"));
    const bool transfer=argc==2 && !std::strcmp(argv[1],"--transfer");
    const bool transferLarge=argc==2 && !std::strcmp(argv[1],"--transfer-large");
    const bool transferFull=argc==2 && !std::strcmp(argv[1],"--transfer-full");
    const bool transferXrgb=argc==2 && !std::strcmp(argv[1],"--transfer-xrgb");
    const bool skipUpload=argc==2 && !std::strcmp(argv[1],"--skip-upload");
    const bool skipReadback=argc==2 && !std::strcmp(argv[1],"--skip-readback");
    if (argc > 2 || (argc == 2 && !skip && !large && !transfer && !transferLarge && !transferFull && !transferXrgb && !skipUpload && !skipReadback)) return 2;
    setvbuf(stdout, nullptr, _IONBF, 0); signal(SIGALRM, timeout); alarm(45);
    if (transfer || transferLarge || transferFull || transferXrgb || skipUpload || skipReadback)
        return transferProbe(transferLarge||transferFull,skipUpload,skipReadback,transferFull,transferXrgb);
    void* context = android_display_native_blt_create(); check(context, "Vulkan native copy context with SYNC_FD");
    unsigned cases = 0;
    for (uint32_t sourceFormat : {bgra, rgba}) for (uint32_t destinationFormat : {bgra, rgba}) {
        const uint32_t sw = large ? 3040 : 61, sh = large ? 1904 : 43;
        const uint32_t dw = large ? 3040 : 73, dh = large ? 1904 : 49;
        Allocation source(sw, sh, sourceFormat), destination(dw, dh, destinationFormat);
        source.initialize(true); destination.initialize(false);
        std::vector<uint32_t> sourceExpected(uint64_t(sw)*sh), expected(uint64_t(dw)*dh, sentinel);
        for (uint32_t y=0; y<sh; ++y) for (uint32_t x=0; x<sw; ++x) sourceExpected[uint64_t(y)*sw+x]=pattern(x,y);
        NativeBltRegion clips[] = {{1,2,7,8,23,17}, {4,5,10,11,13,11}, {40,29,49,35,17,12}};
        int completion = 123;
        check(nativeCopy(context, source.holder, source.holder, clips, 3, &completion) == -EINVAL && completion == -1,
            "same allocation rejected");
        NativeBltRegion invalid[] = {clips[0], {0,0,dw,0,1,1}};
        check(nativeCopy(context, source.holder, destination.holder, invalid, 2, &completion) == -EINVAL && completion == -1,
            "invalid last rectangle rejects entire copy");
        destination.verify(expected, "rejected-copy-unchanged");
        for (unsigned pass=0; pass<2; ++pass) {
            // The second call overwrites one region from a different source
            // location, exercising preservation across release/reacquisition.
            if (pass) clips[0] = {5,7,7,8,23,17};
            for (const auto& r: clips) for (uint32_t y=0; y<r.height; ++y) for (uint32_t x=0; x<r.width; ++x)
                expected[uint64_t(r.destination_y+y)*dw+r.destination_x+x] = pattern(r.source_x+x,r.source_y+y);
            if (!skip) {
                check(nativeCopy(context, source.holder, destination.holder, clips, 3, &completion) == 0,
                    "GPU clipped copy accepted"); waitComplete(completion);
            }
            destination.verify(expected, "clipped-destination-and-outside");
            source.verify(sourceExpected, "source-preserved");
        }
        if (large) {
            const NativeBltRegion full{0,0,0,0,sw,sh};
            check(nativeCopy(context, source.holder, destination.holder, &full, 1, &completion) == 0, "full-size GPU copy");
            waitComplete(completion); destination.verify(sourceExpected, "full-size-destination");
            source.verify(sourceExpected, "full-size-source-preserved");
        }
        ++cases;
    }
    check(android_display_native_blt_destroy(context) == 0, "drained context destruction"); alarm(0);
    std::printf("RESULT passed=1 cases=%u checks=%llu skip_copy=%d native_cpu_copy=0 oracle_cpu_access=1 integrated_kmd=0\n",
        cases, (unsigned long long)checks, skip);
}
