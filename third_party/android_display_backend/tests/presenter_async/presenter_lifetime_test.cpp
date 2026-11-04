// The production presenter submits removal; only a framework release callback
// can authorize reuse, including across guest scanout destruction/recreation.
#include "surface_buffer_presenter.h"
#include <cstdio>
using namespace android_display;
static int failures;
#define CHECK(c) do { if (!(c)) { ++failures; std::fprintf(stderr, "FAIL line %d: %s\n", __LINE__, #c); } } while (0)

struct ReleaseEvent {
    int fd = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);
    ~ReleaseEvent() { close(fd); }
    void observe(const std::shared_ptr<ConsumerRelease>& ticket) {
        CHECK(fd >= 0 && ticket != nullptr);
        ticket->setReleasedCallback([descriptor = fd] {
            const uint64_t one = 1;
            CHECK(write(descriptor, &one, sizeof(one)) == sizeof(one));
        });
    }
    bool ready() const {
        pollfd descriptor{fd, POLLIN, 0};
        return poll(&descriptor, 1, 0) == 1 && (descriptor.revents & POLLIN);
    }
};

static void queuedPresentDetach(bool previousFence) {
    auto& api = SurfaceControl::GetInstance();
    ANativeWindow appWindow;
    AHardwareBuffer a{10, 0}, b{11, 0};
    ReleaseEvent releasedA, releasedB;
    auto presenter = SurfaceBufferPresenter::create(&appWindow, "queued-reset", 3, previousFence, true);
    int first;
    {
        std::lock_guard lock(api.m);
        api.open = false;
        api.log.clear();
        first = api.next + 1;
    }
    auto ta = presenter->present(&a);
    auto tb = presenter->present(&b);
    releasedA.observe(ta);
    releasedB.observe(tb);
    CHECK(presenter->requestDetach());
    presenter.reset();
    CHECK(!releasedA.ready() && !releasedB.ready() && a.refs == 1 && b.refs == 1);
    {
        std::unique_lock lock(api.m);
        CHECK(api.log.empty() && api.liveSurfaces == 1);
        api.open = true;
        api.cv.notify_all();
        CHECK(api.cv.wait_for(lock, std::chrono::seconds(2), [&] {
            return api.log.size() == 6 && (previousFence || api.liveSurfaces == 0);
        }));
        const std::vector<std::string> expected{
            "apply:" + std::to_string(first), "delete:" + std::to_string(first),
            "apply:" + std::to_string(first + 1), "delete:" + std::to_string(first + 1),
            "apply-detach:" + std::to_string(first + 2), "delete:" + std::to_string(first + 2)};
        CHECK(api.log == expected);
        // Legacy OnComplete contexts retain the layer until both previous
        // callbacks arrive, even though the presenter is already gone.
        CHECK(api.liveSurfaces == (previousFence ? 1 : 0));
    }
    CHECK(!releasedA.ready() && !releasedB.ready());
    if (previousFence) api.completeTransaction(first + 1); else api.releaseBuffer(&a);
    CHECK(releasedA.ready() && ta->reusable() && a.refs == 0);
    CHECK(!releasedB.ready() && !tb->reusable() && b.refs == 1);
    if (previousFence) {
        CHECK(api.liveSurfaces == 1);
        api.completeTransaction(first + 2);
    } else {
        api.releaseBuffer(&b);
    }
    CHECK(releasedB.ready() && tb->reusable() && b.refs == 0);
    CHECK(api.liveSurfaces == 0 && api.callbacks.empty() && api.completions.empty());
}

int main() {
    auto& api = SurfaceControl::GetInstance();
    api.open = true;
    ANativeWindow appWindow;
    AHardwareBuffer a{1, 0}, b{2, 0}, c{3, 0};
    ReleaseEvent releasedA, releasedB, releasedC, releasedBAgain;
    auto presenter = SurfaceBufferPresenter::create(&appWindow, "guest", 1, false, true);
    auto ta = presenter->present(&a);
    releasedA.observe(ta);
    auto tb = presenter->present(&b);
    releasedB.observe(tb);
    CHECK(!releasedA.ready() && !releasedB.ready());
    api.releaseBuffer(&a); // Genuine replacement release for the old front.
    CHECK(releasedA.ready() && ta->reusable() && a.refs == 0);
    CHECK(!releasedB.ready() && !tb->reusable() && b.refs == 1);

    // Guest unbind retires its direct layer. The app window remains reusable,
    // but the last AHB is still owned while its release callback is delayed.
    CHECK(SurfaceBufferPresenter::detachAndReset(presenter));
    CHECK(!presenter && api.liveSurfaces == 0);
    CHECK(!releasedB.ready() && !tb->reusable() && b.refs == 1);
    CHECK(SurfaceBufferPresenter::detachAndReset(presenter));
    presenter = SurfaceBufferPresenter::create(&appWindow, "replacement", 1, false, true);
    CHECK(presenter != nullptr && api.windows.size() == 2);
    CHECK(api.windows[0] == &appWindow && api.windows[1] == &appWindow);
    CHECK(presenter->present(&b) == nullptr); // Old reader survives recreation.
    auto tc = presenter->present(&c);
    releasedC.observe(tc);
    api.releaseBuffer(&b);
    CHECK(releasedB.ready() && tb->reusable() && b.refs == 0);
    auto tbAgain = presenter->present(&b);
    releasedBAgain.observe(tbAgain);
    CHECK(tbAgain != nullptr && !releasedBAgain.ready());

    // Detach admission failure retains the presenter and every reader.
    api.failNextCreate = true;
    CHECK(!SurfaceBufferPresenter::detachAndReset(presenter));
    CHECK(presenter != nullptr && api.liveSurfaces == 1);
    CHECK(!releasedC.ready() && !releasedBAgain.ready());
    CHECK(SurfaceBufferPresenter::detachAndReset(presenter));
    CHECK(!presenter && api.liveSurfaces == 0);
    CHECK(!releasedC.ready() && !releasedBAgain.ready());
    api.releaseBuffer(&c);
    api.releaseBuffer(&b);
    CHECK(releasedC.ready() && releasedBAgain.ready());
    CHECK(c.refs == 0 && b.refs == 0 && api.callbacks.empty());

    // Reset detach must return while SurfaceFlinger apply is blocked. The
    // executor retains the layer until its ordered removal actually applies.
    AHardwareBuffer d{4, 0};
    ReleaseEvent releasedD;
    presenter = SurfaceBufferPresenter::create(&appWindow, "reset", 2, false, true);
    {
        std::lock_guard lock(api.m);
        api.open = false;
    }
    auto td = presenter->present(&d);
    releasedD.observe(td);
    api.failNextCreate = true;
    CHECK(!presenter->requestDetach());
    CHECK(!releasedD.ready() && d.refs == 1);
    auto start = std::chrono::steady_clock::now();
    CHECK(presenter->requestDetach());
    CHECK(presenter->requestDetach()); // Idempotent; no additional transaction.
    presenter.reset();
    CHECK(std::chrono::steady_clock::now() - start < std::chrono::milliseconds(500));
    CHECK(!releasedD.ready() && d.refs == 1);
    {
        std::unique_lock lock(api.m);
        CHECK(api.liveSurfaces == 1);
        api.open = true;
        api.cv.notify_all();
        CHECK(api.cv.wait_for(lock, std::chrono::seconds(2), [&] { return api.liveSurfaces == 0; }));
    }
    CHECK(!releasedD.ready() && !td->reusable());
    api.releaseBuffer(&d);
    CHECK(releasedD.ready() && td->reusable() && d.refs == 0);
    queuedPresentDetach(false);
    queuedPresentDetach(true);
    std::printf("%s presenter lifetime (%d failures)\n", failures ? "FAIL" : "PASS", failures);
    return failures ? 1 : 0;
}
