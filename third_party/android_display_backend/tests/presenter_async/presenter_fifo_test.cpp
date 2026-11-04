// SPDX-License-Identifier: Apache-2.0
// Exercise the production presenter against the documented compositor queue
// contract. This is an API-boundary fixture, not Android runtime evidence.
#include "surface_buffer_presenter.h"
#include <cstdio>
#include <cstring>

int main(int argc, char** argv) {
    auto& api = SurfaceControl::GetInstance();
    api.open = true;
    const bool unavailable = argc > 1 && std::strcmp(argv[1], "unavailable") == 0;
    api.backPressureAvailable = !unavailable;
    ANativeWindow window;
    auto presenter = android_display::SurfaceBufferPresenter::create(&window, "scanout", 1);
    assert(presenter);
    AHardwareBuffer first{1, 0}, second{2, 0}, third{3, 0}, fourth{4, 0};
    // Transaction allocation failure must leave the next valid present able
    // to configure the layer, and must not take ownership of the buffer.
    api.failNextCreate = true;
    assert(!presenter->present(&first) && first.refs == 0);
    auto a = presenter->present(&first);
    auto b = presenter->present(&second);
    auto c = presenter->present(&third);
    assert(a && b && c);
    assert(!presenter->present(&fourth) && fourth.refs == 0);
    {
        std::unique_lock lock(api.m);
        assert(api.cv.wait_for(lock, std::chrono::seconds(2), [&] {
            return api.pending.size() == 3;
        }));
    }
    assert(!a->reusable() && !b->reusable() && !c->reusable());
    if (unavailable) {
        assert(api.latchFrame() == 3 && api.droppedFrames == 2);
        assert(api.pressureTransactions == 0);
    } else {
        if (api.latchFrame() != 1) {
            std::fputs("FAIL queued scanout buffer dropped before presentation\n", stderr);
            return 1;
        }
        assert(api.latchFrame() == 2 && api.latchFrame() == 3);
        assert(api.droppedFrames == 0 && api.pressureTransactions == 1);
    }
    // Latching and the queue option do not synthesize consumer release.
    assert(!a->reusable() && !b->reusable() && !c->reusable());
    api.releaseBuffer(&first);
    assert(a->reusable() && !b->reusable() && !c->reusable());
    auto d = presenter->present(&fourth);
    assert(d);
    {
        std::unique_lock lock(api.m);
        assert(api.cv.wait_for(lock, std::chrono::seconds(2), [&] {
            return api.pending.size() == 1;
        }));
    }
    assert(api.latchFrame() == 4);
    assert(api.pressureTransactions == (unavailable ? 0u : 1u));
    assert(presenter->requestDetach());
    assert(!b->reusable() && !c->reusable() && !d->reusable());
    api.releaseBuffer(&second);
    api.releaseBuffer(&third);
    api.releaseBuffer(&fourth);
    assert(b->reusable() && c->reusable() && d->reusable());
    std::puts(unavailable ? "PASS optional API absence retains existing behavior" :
        "PASS queued scanout FIFO, bounded admission, real release, and detach");
}
