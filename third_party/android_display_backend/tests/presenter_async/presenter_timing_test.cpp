#include "surface_buffer_presenter.h"
#include <cassert>
#include <cstdio>
using namespace android_display;
static void awaitTransactions(unsigned count) {
    auto& api = SurfaceControl::GetInstance();
    std::unique_lock lock(api.m);
    assert(api.cv.wait_for(lock, std::chrono::seconds(2), [&] { return api.log.size() >= count * 2; }));
}
int main() {
    auto& api = SurfaceControl::GetInstance();
    api.open = true; api.timingAvailable = true;
    ANativeWindow window;
    AHardwareBuffer a{101, 0}, b{102, 0};
    auto timing = std::make_shared<DisplayTimingState>(5, 6);
    auto presenter = SurfaceBufferPresenter::create(&window, "timing", 5);
    presenter->setTimingState(timing);
    auto ta = presenter->present(&a);
    assert(ta && !ta->reusable());
    awaitTransactions(1);
    int fd = eventfd(1, EFD_CLOEXEC);
    api.completeTransaction(1, -1, fd, displayMonotonicNs() - 1000);
    assert(fcntl(fd, F_GETFD) == -1 && errno == EBADF);
    assert(!ta->reusable() && a.refs == 1); // OnComplete cannot release API36 buffer.
    assert(timing->diagnostics().completedCallbacks == 1 && timing->diagnostics().invalid == 1);
    auto tb = presenter->present(&b);
    assert(tb && presenter->requestDetach());
    presenter.reset();
    awaitTransactions(3);
    assert(api.liveSurfaces == 1); // Only the pending OnComplete retains the surface.
    api.completeTransaction(2); // Unsupported present fence after invalidation.
    assert(api.liveSurfaces == 0 && !ta->reusable() && !tb->reusable());
    assert(!timing->snapshot(displayMonotonicNs()).hostNowNs);
    api.releaseBuffer(&a); api.releaseBuffer(&b);
    assert(ta->reusable() && tb->reusable());

    auto old = std::make_shared<DisplayTimingState>(7, 8);
    presenter = SurfaceBufferPresenter::create(&window, "previous-fence", 7, true);
    presenter->setTimingState(old);
    ta = presenter->present(&a);
    tb = presenter->present(&b);
    awaitTransactions(5);
    api.completeTransaction(4);
    assert(!ta->reusable() && !tb->reusable());
    fd = eventfd(1, EFD_CLOEXEC);
    api.completeTransaction(5, -1, fd, displayMonotonicNs() - 1000);
    assert(ta->reusable() && !tb->reusable()); // Existing previous-release contract preserved.
    assert(SurfaceBufferPresenter::detachAndReset(presenter));
    awaitTransactions(6);
    api.completeTransaction(6);
    assert(tb->reusable() && api.liveSurfaces == 0 && api.completions.empty());
    assert(old->diagnostics().completedCallbacks == 2); // Detach is never a timing frame.
    std::puts("PASS timing completion: API36/legacy release independence, late callback, detached surface lifetime");
}
