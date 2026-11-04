// SPDX-License-Identifier: Apache-2.0
#include "native_blt.h"
#include <cstdio>
#include <cstdlib>
#include <cerrno>
#include <vector>
using VkResult=int;
constexpr int VK_SUCCESS=0, VK_TRUE=1, VK_TIMEOUT=2, VK_ERROR_DEVICE_LOST=-4;
constexpr int VK_ERROR_OUT_OF_HOST_MEMORY=-1, VK_ERROR_OUT_OF_DEVICE_MEMORY=-2;
unsigned released=0, unsafeReleased=0, waits=0;
struct Bridge {
    struct Slot { bool inFlight=true; uint64_t fence=1; };
    std::vector<Slot> mInFlightSlots{Slot{}};
    bool mNativeDestructionReady=false;
    int mDevice=1;
    int waitResult=VK_TIMEOUT;
    ~Bridge() { ++released; unsafeReleased += !mNativeDestructionReady; }
    VkResult mWaitForFences(int, unsigned, uint64_t*, int, uint64_t timeout) {
        if (timeout != 1000000000ULL) std::abort();
        ++waits; return waitResult;
    }
    // INSERT_REAL_DRAIN
};
struct NativeBltContext { Bridge bridge; };
// INSERT_REAL_DESTROY
int main() {
    auto* context = new NativeBltContext;
    for (int result : {VK_TIMEOUT, VK_ERROR_OUT_OF_HOST_MEMORY, VK_ERROR_OUT_OF_DEVICE_MEMORY}) {
        context->bridge.waitResult=result;
        if (android_display_native_blt_destroy(context) != -EBUSY || released ||
                context->bridge.mNativeDestructionReady) {
            std::puts("FAIL native close released pending GPU dependencies"); return 1;
        }
    }
    context->bridge.waitResult=VK_SUCCESS;
    if (android_display_native_blt_destroy(context) || released != 1 || unsafeReleased) return 1;
    auto* lost = new NativeBltContext; lost->bridge.waitResult=VK_ERROR_DEVICE_LOST;
    if (android_display_native_blt_destroy(lost) || released!=2 || unsafeReleased) return 1;
    auto* idle = new NativeBltContext; idle->bridge.mInFlightSlots.clear();
    unsigned oldWaits=waits;
    if (android_display_native_blt_destroy(idle) || released!=3 || unsafeReleased || waits!=oldWaits) return 1;
    std::puts("native Blt close: timeout/OOM retained, success/lost/idle drained PASS");
}
