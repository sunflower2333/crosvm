// A present must not wait for SurfaceFlinger: the guest's flip waits on it.
// Transactions still apply in order, each deleted exactly once after apply.
#include "surface_buffer_presenter.h"
#include <chrono>
#include <cstdio>
using namespace android_display;
static int failures;
#define CHECK(c) do { if (!(c)) { ++failures; std::fprintf(stderr, "FAIL line %d: %s\n", __LINE__, #c); } } while (0)
int main() {
    auto& api = SurfaceControl::GetInstance();
    ANativeWindow window;
    auto presenter = SurfaceBufferPresenter::create(&window, "test", 1, false, true);
    CHECK(presenter != nullptr);
    AHardwareBuffer a{1, 0}, b{2, 0};
    // apply is gated shut: both presents must still return with a ticket.
    auto start = std::chrono::steady_clock::now();
    auto ta = presenter->present(&a);
    auto tb = presenter->present(&b);
    CHECK(ta != nullptr && tb != nullptr);
    CHECK(std::chrono::steady_clock::now() - start < std::chrono::milliseconds(500));
    {
        std::lock_guard l(api.m);
        CHECK(api.log.empty());   // nothing applied or deleted before the gate opens
    }
    // A detach applies synchronously and after every earlier present.
    std::thread opener([&] {
        std::this_thread::sleep_for(std::chrono::milliseconds(50));
        std::lock_guard l(api.m);
        api.open = true;
        api.cv.notify_all();
    });
    CHECK(presenter->detach());
    opener.join();
    std::vector<std::string> expected{"apply:1", "delete:1", "apply:2", "delete:2", "apply-detach:3", "delete:3"};
    {
        std::lock_guard l(api.m);
        CHECK(api.log == expected);
        for (auto& e : api.log) std::fprintf(stderr, "%s ", e.c_str());
        std::fprintf(stderr, "\n");
    }
    // The presenter keeps the buffers referenced until their release.
    CHECK(a.refs == 1 && b.refs == 1);
    std::printf("%s presenter async apply (%d failures)\n", failures ? "FAIL" : "PASS", failures);
    return failures ? 1 : 0;
}
