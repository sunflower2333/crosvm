#include "surface_buffer_presenter.h"
#include "surface_attachment_lease.h"
#include <atomic>
#include <cassert>
#include <condition_variable>
#include <fcntl.h>
#include <optional>
#include <sstream>
#include <sys/resource.h>

#define LOG(x) std::ostringstream()
#define PLOG(x) std::ostringstream()
struct Failure { template<class T> Failure& operator<<(const T&) { return *this; } };
static Failure Error() { return {}; }
template<class T=void> struct Result {
    bool good=true;
    Result() = default;
    Result(Failure) : good(false) {}
    template<class U> Result(U) {}
    bool ok() const { return good; }
    int error() const { return 1; }
};
namespace android::base {
struct unique_fd {
    int value=-1;
    unique_fd() = default;
    explicit unique_fd(int fd) : value(fd) {}
    unique_fd& operator=(unique_fd&& other) { reset(other.value); other.value=-1; return *this; }
    ~unique_fd() { reset(); }
    int get() const { return value; }
    void reset(int fd=-1) { if(value>=0) close(value); value=fd; }
};
}
struct Surface { ANativeWindow* window; explicit Surface(ANativeWindow* w):window(w){} ANativeWindow* get() const {return window;} };
struct Rect { uint32_t width,height; };
struct SharedAllocationDescription { uint64_t buffer_id; uint32_t fourcc,width,height,plane_stride; };
constexpr int AHARDWAREBUFFER_FORMAT_R8G8B8A8_UNORM=1, AHARDWAREBUFFER_FORMAT_B8G8R8A8_UNORM=5;
namespace android_display { constexpr uint32_t kSharedAhbRgbaFourcc=1,kSharedAhbBgraFourcc=2; }
static int AHardwareBuffer_getId(AHardwareBuffer* b,uint64_t* id) { *id=b->id; return 0; }
static void logDirectAhb(const char*,uint64_t,uint64_t,uint64_t,uint64_t,uint64_t) {}
struct { std::atomic<uint64_t> directAccepted{},directReleased{}; } gDisplayPathCounters;
struct DirectReleaseSignal {
    android::base::unique_fd fd{eventfd(0,EFD_CLOEXEC|EFD_NONBLOCK)};
    void signal() { const uint64_t one=1; assert(write(fd.get(),&one,8)==8); }
};
class AndroidDisplaySurface {
public:
    std::string mName="scanout";
    bool mPreRotate=false;
    std::mutex mSurfaceMutex;
    android_display::SurfaceAttachmentLease mAttachmentLease;
    uint64_t mNativeSurfaceGeneration=0,mDirectPresenterGeneration=0,mDirectPresentedGeneration=0;
    uint32_t mRequestedFrameRate=60;
    std::unique_ptr<Surface> mNativeSurface;
    bool mNativeSurfaceNeedsConfiguring=false;
    ASurfaceControl* mSurfaceControl=nullptr;
    std::condition_variable mNativeSurfaceReady;
    std::unique_ptr<android_display::SurfaceBufferPresenter> mDirectPresenter;
    std::optional<Rect> mRequestedSurfaceDimensions;
    android::base::unique_fd mNativeRefreshEvent;
    Result<void> drawCursorFrameLocked() { return {}; }
    void clearNativeSurfaceLocked(bool) {
        mAttachmentLease.clear();
        assert(android_display::SurfaceBufferPresenter::detachAndReset(mDirectPresenter));
        if(mSurfaceControl) SurfaceControl::GetInstance().ASurfaceControl_release(mSurfaceControl);
        mSurfaceControl=nullptr;
        if(mNativeSurface) ++mNativeSurfaceGeneration;
        mNativeSurface.reset();
    }
    ~AndroidDisplaySurface() { clearNativeSurfaceLocked(true); }
    // INSERT_PRODUCTION
};
static uint64_t drain(int fd) { uint64_t n=0; auto ret=read(fd,&n,8); assert(ret==8); return n; }
int main() {
    rlimit limit{}; setrlimit(RLIMIT_CORE,&limit);
    auto& api=SurfaceControl::GetInstance(); api.open=true;
    ANativeWindow window;
    Surface incoming(&window);
    android::base::unique_fd event{eventfd(0,EFD_CLOEXEC|EFD_NONBLOCK)};
    AndroidDisplaySurface surface;
    assert(surface.setNativeRefreshEvent(event.get()));
    assert(surface.setNativeSurface(incoming,true).ok());
    assert(drain(event.get())==1);
    assert(surface.nativeRefreshGeneration()==1);
    AHardwareBuffer buffer{19,0};
    auto first=std::make_shared<DirectReleaseSignal>();
    assert(surface.presentShared(&buffer,-1,nullptr,first,1).ok());
    assert(surface.nativeRefreshGeneration()==0);
    // A new Activity queues detach, but does not itself release its AHB.
    assert(surface.setNativeSurface(incoming,true).ok());
    assert(surface.nativeRefreshGeneration()==3 && drain(event.get())==1);
    pollfd released{first->fd.get(),POLLIN,0}; assert(poll(&released,1,0)==0);
    auto next=std::make_shared<DirectReleaseSignal>();
    const auto accepted=gDisplayPathCounters.directAccepted.load();
    AHardwareBuffer staleBuffer{20,0};
    assert(!surface.presentShared(&staleBuffer,-1,nullptr,next,1).ok());
    assert(gDisplayPathCounters.directAccepted==accepted);
    api.releaseBuffer(&buffer);
    assert(poll(&released,1,0)==1); drain(first->fd.get());
    assert(surface.presentShared(&buffer,-1,nullptr,next,3).ok());
    assert(surface.nativeRefreshGeneration()==0);
    assert(surface.setNativeSurface(incoming,true).ok());
    assert(surface.setNativeSurface(incoming,true).ok());
    assert(drain(event.get())==2 && surface.nativeRefreshGeneration()==7);
    api.releaseBuffer(&buffer);
    // Registration after attach observes the current need without polling.
    assert(surface.setNativeRefreshEvent(-1));
    assert(surface.setNativeRefreshEvent(event.get())); assert(drain(event.get())==1);
    assert(!surface.setNativeRefreshEvent(123456));
    assert(surface.setNativeSurface(incoming,true).ok()); assert(drain(event.get())==1);
    AndroidDisplaySurface cursor; cursor.mPreRotate=true;
    assert(cursor.setNativeRefreshEvent(event.get())); assert(cursor.setNativeSurface(incoming,true).ok());
    pollfd notified{event.get(),POLLIN,0}; assert(poll(&notified,1,0)==0);
}
