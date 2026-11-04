// SPDX-License-Identifier: Apache-2.0
#include "native_blt_transfer.h"
#include <array>
#include <cassert>
#include <cstdio>
#include <cstring>
using VkResult=int;
constexpr int VK_SUCCESS=0, VK_TIMEOUT=2, VK_ERROR_OUT_OF_HOST_MEMORY=-1,
    VK_ERROR_OUT_OF_DEVICE_MEMORY=-2, VK_ERROR_DEVICE_LOST=-4, VK_ERROR_UNKNOWN=-13;
struct Error {
    template<class T> Error& operator<<(const T&) { return *this; }
};
template<class T> struct Result {
    bool success=true;
    Result()=default;
    Result(Error) : success(false) {}
    bool ok() const { return success; }
    Error error() const { return {}; }
};
struct Slot { bool inFlight=false; int fence=7; };
int submitResult,waitResult,invalidateResult;
unsigned submitted,waited,invalidated;
int mQueue=1,mDevice=2;
int mQueueSubmit(int,unsigned,const int*,int) { ++submitted; return submitResult; }
int invalidateMemory(int,unsigned,const int*) { ++invalidated; return invalidateResult; }
Result<void> reclaimSlot(Slot& slot,uint64_t timeout) {
    assert(timeout==1000000000ULL); ++waited;
    if(waitResult!=VK_SUCCESS) return Error{};
    slot.inFlight=false; return {};
}
struct Native { uint32_t fourcc=0x34324241; };
Result<void> finish(Slot& slot,void* mNativeStagingAddress,uint8_t* ordinaryDestination,
        bool upload,const NativeBltCpuLayout& cpu,const NativeBltRegion* regions,uint32_t count) {
    int submit=0,range=0; Native native;
// INSERT_REAL_COMPLETION
}
int main() {
    std::array<uint8_t,16> output,staging{0x11,0x22,0x33,0x44,0x55,0x66,0x77,0x88};
    NativeBltCpuLayout cpu{4,1,16,0x34325241,16};
    NativeBltRegion region{0,0,1,0,2,1};
    unsigned cases=0;
    for(int submitCode : {VK_SUCCESS,VK_ERROR_OUT_OF_HOST_MEMORY,VK_ERROR_OUT_OF_DEVICE_MEMORY,VK_ERROR_DEVICE_LOST,VK_ERROR_UNKNOWN})
    for(int waitCode : {VK_SUCCESS,VK_TIMEOUT,VK_ERROR_OUT_OF_HOST_MEMORY,VK_ERROR_OUT_OF_DEVICE_MEMORY,VK_ERROR_DEVICE_LOST})
    for(int invalidateCode : {VK_SUCCESS,VK_ERROR_OUT_OF_HOST_MEMORY}) {
        Slot slot; output.fill(0x59); submitted=waited=invalidated=0;
        submitResult=submitCode; waitResult=waitCode; invalidateResult=invalidateCode;
        auto r=finish(slot,staging.data(),output.data(),false,cpu,&region,1);
        const bool completed=submitCode==VK_SUCCESS && waitCode==VK_SUCCESS;
        const bool success=completed && invalidateCode==VK_SUCCESS;
        assert(r.ok()==success && submitted==1);
        assert(waited==unsigned(submitCode==VK_SUCCESS));
        assert(invalidated==unsigned(completed));
        const bool submitNoEffect=submitCode==VK_ERROR_OUT_OF_HOST_MEMORY || submitCode==VK_ERROR_OUT_OF_DEVICE_MEMORY;
        assert(slot.inFlight==(!submitNoEffect && !completed));
        const std::array<uint8_t,16> expected{0x59,0x59,0x59,0x59,0x33,0x22,0x11,0x44,0x77,0x66,0x55,0x88,0x59,0x59,0x59,0x59};
        if(success) assert(output==expected);
        else for(auto value:output) assert(value==0x59);
        ++cases;
    }
    Slot slot; output.fill(0x59); submitResult=waitResult=VK_SUCCESS;
    invalidateResult=VK_ERROR_OUT_OF_HOST_MEMORY; invalidated=0;
    assert(finish(slot,staging.data(),nullptr,true,cpu,&region,1).ok() && !slot.inFlight && !invalidated);
    std::printf("native Blt actual completion: %u error combinations and upload PASS\n",cases);
}
