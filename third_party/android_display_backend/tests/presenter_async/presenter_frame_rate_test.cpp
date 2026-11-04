// Actual presenter: vote identity/state, optional API and ordered async lifetime.
#include "surface_buffer_presenter.h"
#include <cstdio>
using namespace android_display;
static int failures;
#define CHECK(c) do { if (!(c)) { ++failures; std::fprintf(stderr, "FAIL line %d: %s\n", __LINE__, #c); } } while (0)

int main() {
    auto& api = SurfaceControl::GetInstance();
    ANativeWindow window;
    AHardwareBuffer a{41, 0}, b{42, 0};
    auto presenter = SurfaceBufferPresenter::create(&window, "main", 1, false, true);
    CHECK(presenter != nullptr);
    CHECK(presenter->setFrameRate(0) && api.next == 0); // New layer already has no vote.
    auto start = std::chrono::steady_clock::now();
    CHECK(presenter->setFrameRate(165));
    for (int i = 0; i < 100; ++i) CHECK(presenter->setFrameRate(165));
    CHECK(api.next == 1 && api.votes.size() == 1);
    auto ticket = presenter->present(&a);
    CHECK(ticket != nullptr);
    CHECK(api.votes[0].surface == api.submissions[0].second);
    CHECK(api.votes[0].transaction != api.submissions[0].first);
    CHECK(api.votes[0].rate == 165 && api.votes[0].compatibility == 2 && api.votes[0].strategy == 0);
    CHECK(presenter->setFrameRate(144));
    CHECK(presenter->setFrameRate(0));
    CHECK(presenter->setFrameRate(0));
    CHECK(api.votes.size() == 3 && api.votes[1].rate == 144 && api.votes[2].rate == 0);
    CHECK(presenter->requestDetach());
    CHECK(!presenter->setFrameRate(165));
    presenter.reset();
    CHECK(std::chrono::steady_clock::now() - start < std::chrono::milliseconds(500));
    CHECK(!ticket->reusable() && a.refs == 1);
    {
        std::unique_lock lock(api.m);
        CHECK(api.log.empty() && api.liveSurfaces == 1);
        api.open = true;
        api.cv.notify_all();
        CHECK(api.cv.wait_for(lock, std::chrono::seconds(2), [&] {
            return api.log.size() == 10 && api.liveSurfaces == 0;
        }));
        const std::vector<std::string> expected{
            "apply:1", "delete:1", "apply:2", "delete:2", "apply:3", "delete:3",
            "apply:4", "delete:4", "apply-detach:5", "delete:5"};
        CHECK(api.log == expected);
    }
    CHECK(!ticket->reusable() && a.refs == 1); // Vote and detach cannot invent release.
    api.releaseBuffer(&a);
    CHECK(ticket->reusable() && a.refs == 0);

    presenter = SurfaceBufferPresenter::create(&window, "reattached", 2, false, true);
    CHECK(presenter->setFrameRate(165));
    CHECK(api.votes.size() == 4 && api.votes.back().surface != api.votes.front().surface);
    api.failNextCreate = true;
    CHECK(!presenter->setFrameRate(120));
    CHECK(api.votes.size() == 4);
    CHECK(presenter->setFrameRate(120)); // Allocation failure must leave the vote retryable.
    CHECK(presenter->setFrameRate(120));
    CHECK(api.votes.size() == 5 && api.votes.back().rate == 120);
    CHECK(SurfaceBufferPresenter::detachAndReset(presenter));

    api.frameRateAvailable = false;
    presenter = SurfaceBufferPresenter::create(&window, "older-api", 3, false, true);
    CHECK(presenter != nullptr);
    const auto previousTransactions = api.next;
    CHECK(!presenter->setFrameRate(165));
    CHECK(!presenter->setFrameRate(165));
    CHECK(api.next == previousTransactions && api.votes.size() == 5);
    auto noVoteTicket = presenter->present(&b);
    CHECK(noVoteTicket != nullptr); // Missing scheduling hint does not disable scanout.
    CHECK(SurfaceBufferPresenter::detachAndReset(presenter));
    CHECK(!noVoteTicket->reusable());
    api.releaseBuffer(&b);
    CHECK(noVoteTicket->reusable() && b.refs == 0);
    CHECK(api.liveSurfaces == 0 && api.callbacks.empty());
    std::printf("%s presenter frame rate (%d failures)\n", failures ? "FAIL" : "PASS", failures);
    return failures ? 1 : 0;
}
