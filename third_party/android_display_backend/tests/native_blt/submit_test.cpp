#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <vector>
using VkResult=int;
constexpr int VK_SUCCESS=0,VK_TRUE=1,VK_TIMEOUT=2,VK_ERROR_UNKNOWN=-13,VK_ERROR_DEVICE_LOST=-4;
constexpr int VK_ERROR_OUT_OF_HOST_MEMORY=-1,VK_ERROR_OUT_OF_DEVICE_MEMORY=-2,VK_NULL_HANDLE=0;
struct ScopedTrace {explicit ScopedTrace(const char*){}};
struct Error{template<class T>Error& operator<<(T){return *this;}operator int(){return -1;}};
struct Bridge{
 struct Slot{bool inFlight=false;uint64_t fence=7;int completionSemaphore=19,acquireSemaphore=23;};
 std::vector<Slot> mInFlightSlots{Slot{}};
 bool mNativeDestructionReady=false,mAsyncBlitEnabled=true;
 int mDevice=1,mQueue=1,submitted=VK_SUCCESS,waited=VK_TIMEOUT,destroyed=0,evicted=0;
 int mQueueSubmit(int,unsigned,void*,uint64_t){return submitted;}
 int mWaitForFences(int,unsigned,uint64_t*,int,uint64_t){return waited;}
 void mDestroySemaphore(int,int,void*){++destroyed;}
 void evictTarget(int){++evicted;}
 // INSERT_DRAIN
 int submit(bool preserveTarget){
  auto& slot=mInFlightSlots[0];int target=5,submitInfo=0;bool injectSubmitFailure=false;
  (void)preserveTarget;
  // INSERT_SUBMIT
  return 0;
 }
};
void check(bool v,const char* s){if(!v){std::printf("FAIL %s\n",s);std::exit(1);}}
int main(){for(int result:{VK_ERROR_UNKNOWN,VK_ERROR_DEVICE_LOST}){Bridge b;b.submitted=result;check(b.submit(true)!=0,"submission failed");check(b.evicted==0&&b.destroyed==0&&b.mInFlightSlots[0].inFlight,"uncertain submit retains all GPU dependencies");check(!b.prepareNativeDestruction(),"timeout close retains context");b.waited=VK_ERROR_DEVICE_LOST;check(b.prepareNativeDestruction(),"loss proof allows close");}for(int result:{VK_ERROR_OUT_OF_HOST_MEMORY,VK_ERROR_OUT_OF_DEVICE_MEMORY}){Bridge b;b.submitted=result;check(b.submit(true)!=0,"OOM rejected");check(!b.mInFlightSlots[0].inFlight&&b.evicted==1&&b.destroyed==2,"OOM no-effect cleanup");}Bridge ok;check(ok.submit(true)==0&&ok.mInFlightSlots[0].inFlight,"success tracked before completion");Bridge old;old.submitted=VK_ERROR_UNKNOWN;check(old.submit(false)!=0&&old.evicted==1&&old.destroyed==2,"legacy display policy unchanged");std::puts("actual native submit dependencies: PASS");}
