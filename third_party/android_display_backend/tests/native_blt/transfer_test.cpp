// SPDX-License-Identifier: Apache-2.0
#include <cstdio>
#if !__has_include("native_blt_transfer.h")
int main() { std::puts("FAIL ordinary/native GPU transfer contract is missing"); return 1; }
#else
#include "native_blt_transfer.h"
#include <array>
#include <cstring>
#include <cstdlib>
#include <vector>
unsigned checks;
void check(bool ok, const char* label) {
    ++checks;
    if (!ok) { std::fprintf(stderr, "FAIL %s\n", label); std::exit(1); }
}
int main() {
    using namespace android_display;
    // The unused X byte cannot masquerade as alpha, and XR24 has the same
    // channel order as AR24. Test distinct R/B and every possible X value.
    for (uint32_t native : {0x34324241U,0x34325241U}) {
        NativeBltCpuLayout xrgb{256,1,1032,0x34325258,1032};
        NativeBltRegion clip{0,0,0,0,256,1}; uint64_t size=0;
        check(nativeBltTransferValid(256,1,xrgb,&clip,1,true,&size) && size==1024,"XRGB upload admitted");
        check(nativeBltTransferValid(256,1,xrgb,&clip,1,false,&size) && size==1024,"XRGB readback admitted");
        std::array<uint8_t,1032> source{}, result; result.fill(0x5a);
        std::array<uint8_t,1024> packed{};
        for (unsigned x=0;x<256;++x) {
            source[x*4]=0x23;source[x*4+1]=0x67;source[x*4+2]=0xb1;source[x*4+3]=x;
        }
        nativeBltPackUpload(source.data(),xrgb,native,&clip,1,packed.data());
        for (unsigned x=0;x<256;++x) {
            check(packed[x*4]==(native==0x34324241U?0xb1:0x23) && packed[x*4+1]==0x67 &&
                packed[x*4+2]==(native==0x34324241U?0x23:0xb1) && packed[x*4+3]==0xff,
                "XRGB upload channel order and opaque alpha");
            packed[x*4+3]=x; // Readback ignores arbitrary source alpha too.
        }
        nativeBltUnpackReadback(packed.data(),native,xrgb,&clip,1,result.data());
        for (unsigned x=0;x<256;++x)
            check(result[x*4]==0x23 && result[x*4+1]==0x67 && result[x*4+2]==0xb1 && result[x*4+3]==0xff,
                "XRGB readback channel order and unused byte");
        for (unsigned i=1024;i<1032;++i) check(result[i]==0x5a,"XRGB preserves padding");
    }
    NativeBltCpuLayout cpu{7, 5, 40, 0x34325241, 188};
    NativeBltRegion r[] = {{1, 1, 3, 2, 3, 2}, {2, 2, 4, 3, 2, 1}};
    uint64_t bytes = 999;
    check(nativeBltTransferValid(13, 9, cpu, r, 2, true, &bytes) && bytes == 32, "translated upload and padded source extent");
    std::array<uint8_t, 188> input{}, output;
    output.fill(0x59);
    for (unsigned y=0; y<5; ++y) for (unsigned x=0; x<7; ++x) {
        uint32_t pixel=0x80000000 | (x<<16) | (y<<8) | (x+y+31);
        std::memcpy(input.data()+y*40+x*4, &pixel, 4);
    }
    std::array<uint8_t, 32> packed{};
    nativeBltPackUpload(input.data(), cpu, 0x34324241, r, 2, packed.data());
    // Build the independent oracle from coordinates, not the row-copy helper.
    const unsigned xs[] = {1,2,3,1,2,3,2,3}, ys[] = {1,1,1,2,2,2,2,2};
    for (unsigned i=0; i<8; ++i) {
        uint32_t value; std::memcpy(&value, packed.data()+i*4, 4);
        check(value == (0x80000000 | ((xs[i]+ys[i]+31)<<16) | (ys[i]<<8) | xs[i]), "upload channel conversion and row offsets");
    }
    NativeBltRegion readback[] = {{3,2,1,1,3,2}, {4,3,2,2,2,1}};
    check(nativeBltTransferValid(13, 9, cpu, readback, 2, false, &bytes) && bytes==32, "translated readback");
    nativeBltUnpackReadback(packed.data(), 0x34324241, cpu, readback, 2, output.data());
    for (unsigned i=0; i<output.size(); ++i) {
        unsigned y=i/40, x=(i%40)/4;
        const bool copied=y>=1 && y<=2 && x>=1 && x<=3;
        check(output[i] == (copied ? input[i] : 0x59), "readback preserves every outside and padding byte");
    }
    auto bad=cpu;
    bad.size=187; check(!nativeBltTransferValid(13,9,bad,r,2,true,&bytes), "one byte short rejects before write");
    bad=cpu; bad.stride=27; check(!nativeBltTransferValid(13,9,bad,r,2,true,&bytes), "short stride");
    bad=cpu; bad.stride=29; check(!nativeBltTransferValid(13,9,bad,r,2,true,&bytes), "unaligned stride");
    bad=cpu; bad.fourcc=0; check(!nativeBltTransferValid(13,9,bad,r,2,true,&bytes), "unknown format");
    bad=cpu; bad.width=0; check(!nativeBltTransferValid(13,9,bad,r,2,true,&bytes), "zero extent");
    bad=cpu; bad.height=8193; check(!nativeBltTransferValid(13,9,bad,r,2,true,&bytes), "oversized extent");
    auto late=std::array<NativeBltRegion,2>{r[0],{0,0,13,0,1,1}};
    check(!nativeBltTransferValid(13,9,cpu,late.data(),2,true,&bytes) && bytes==0, "late invalid region clears byte count");
    check(!nativeBltTransferValid(13,9,cpu,nullptr,1,true,&bytes), "missing regions");
    check(!nativeBltTransferValid(13,9,cpu,r,0,true,&bytes), "zero regions");
    check(!nativeBltTransferValid(13,9,cpu,r,257,true,&bytes), "excessive regions");
    check(!nativeBltTransferValid(13,9,cpu,r,2,true,nullptr), "missing byte counter");
    NativeBltCpuLayout large{4096,4096,16384,0x34324241,67108864};
    NativeBltRegion full[]={{0,0,0,0,4096,4096},{0,0,0,0,1,1}};
    check(nativeBltTransferValid(4096,4096,large,full,1,true,&bytes) && bytes==67108864, "bounded staging limit accepted");
    check(!nativeBltTransferValid(4096,4096,large,full,2,true,&bytes) && bytes==0, "staging limit includes overlapping regions");
    std::printf("native Blt transfer layout: %u checks PASS\n",checks);
}
#endif
