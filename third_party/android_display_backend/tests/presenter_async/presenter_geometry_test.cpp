// Real presenter: authentic storage, atomic geometry/buffer and mode lifetime.
#include "surface_buffer_presenter.h"
#include <cstdio>
using namespace android_display;
static int failures;
#define CHECK(c) do { if (!(c)) { ++failures; std::fprintf(stderr, "FAIL line %d: %s\n", __LINE__, #c); } } while (0)

int main() {
    auto& api = SurfaceControl::GetInstance();
    api.geometryAvailable = true;
    ANativeWindow window{3040, 1904}; // Parent stays in logical guest coordinates.
    AHardwareBuffer portrait{101, 0, 1904, 3040};
    AHardwareBuffer landscape{102, 0, 3040, 1904};
    auto presenter = SurfaceBufferPresenter::create(&window, "geometry", 17, false, true);
    ScanoutPresentationGeometry geometry{1904, 3040, 3040, 1904,
        ScanoutContentRotation::Clockwise270, 9, 1};
    CHECK(geometry.valid() && presenter->configureScanoutGeometry(geometry));
    CHECK(presenter->configureScanoutGeometry(geometry)); // Identical configuration is idempotent.
    const auto before = api.next;
    CHECK(!presenter->present(&landscape, -1, 1, 9)); // No false portrait storage label.
    CHECK(!presenter->present(&portrait)); // Old caller cannot bypass the negotiated tuple.
    CHECK(!presenter->present(&portrait, -1, 0, 9));
    CHECK(!presenter->present(&portrait, -1, 1, 8));
    portrait.layers = 2;
    CHECK(!presenter->present(&portrait, -1, 1, 9));
    portrait.layers = 1;
    CHECK(api.next == before && portrait.refs == 0 && api.callbacks.empty());
    auto first = presenter->present(&portrait, -1, 1, 9);
    CHECK(first && first->generation == 17); // Release identity stays attachment-scoped.
    CHECK(portrait.refs == 1 && !first->reusable());
    CHECK(api.geometries.size() == 1 && api.submissions.size() == 1);
    const auto firstGeometry = api.geometries.front();
    CHECK(firstGeometry.transaction == api.submissions.front().first);
    CHECK(firstGeometry.surface == api.submissions.front().second);
    CHECK(firstGeometry.source.left == 0 && firstGeometry.source.top == 0 &&
          firstGeometry.source.right == 1904 && firstGeometry.source.bottom == 3040);
    CHECK(firstGeometry.destination.left == 0 && firstGeometry.destination.top == 0 &&
          firstGeometry.destination.right == 3040 && firstGeometry.destination.bottom == 1904);
    CHECK(firstGeometry.transform == ANATIVEWINDOW_TRANSFORM_ROTATE_90);

    auto invalid = geometry;
    invalid.contentRotation = ScanoutContentRotation::Clockwise90;
    CHECK(!presenter->configureScanoutGeometry(invalid)); // Same generation cannot mutate orientation.
    invalid = geometry; invalid.resetGeneration = 10; invalid.modeGeneration = 2;
    CHECK(!presenter->configureScanoutGeometry(invalid)); // Reset requires a new owner/presenter.
    invalid = geometry; invalid.modeGeneration = 0;
    CHECK(!presenter->configureScanoutGeometry(invalid));
    invalid = geometry; invalid.storageWidth = 3040;
    CHECK(!presenter->configureScanoutGeometry(invalid));
    invalid = geometry; invalid.contentRotation = static_cast<ScanoutContentRotation>(4);
    CHECK(!presenter->configureScanoutGeometry(invalid));
    invalid = geometry; invalid.storageHeight = 8193;
    CHECK(!presenter->configureScanoutGeometry(invalid));
    invalid = geometry; invalid.logicalWidth = 0;
    CHECK(!presenter->configureScanoutGeometry(invalid));

    geometry.contentRotation = ScanoutContentRotation::Clockwise90;
    geometry.modeGeneration = 2;
    CHECK(presenter->configureScanoutGeometry(geometry));
    CHECK(!first->reusable() && portrait.refs == 1); // Mode transition cannot release the old front.
    CHECK(!presenter->present(&portrait, -1, 2, 9)); // Nor present the in-use allocation again.
    AHardwareBuffer next{103, 0, 1904, 3040};
    CHECK(!presenter->present(&next, -1, 1, 9)); // Old-generation retry never submits.
    auto second = presenter->present(&next, -1, 2, 9);
    CHECK(second && api.geometries.back().transform == ANATIVEWINDOW_TRANSFORM_ROTATE_270);
    CHECK(api.geometries.back().transaction == api.submissions.back().first);
    geometry.modeGeneration = 1;
    CHECK(!presenter->configureScanoutGeometry(geometry)); // Monotonic within the reset.

    geometry = {3040, 1904, 3040, 1904, ScanoutContentRotation::Identity, 9, 3};
    CHECK(presenter->configureScanoutGeometry(geometry));
    api.failNextCreate = true;
    CHECK(!presenter->present(&landscape, -1, 3, 9));
    CHECK(landscape.refs == 0); // No reservation leaked when allocation of the transaction fails.
    auto third = presenter->present(&landscape, -1, 3, 9);
    CHECK(third && api.geometries.back().transform == 0);
    CHECK(!first->reusable() && !second->reusable());
    {
        std::lock_guard lock(api.m);
        CHECK(api.log.empty()); // The actual executor is still blocked; admission did not wait.
        api.open = true;
        api.cv.notify_all();
    }
    CHECK(presenter->detach());
    CHECK(!first->reusable() && !second->reusable() && !third->reusable());
    api.releaseBuffer(&portrait); api.releaseBuffer(&next); api.releaseBuffer(&landscape);
    CHECK(first->reusable() && second->reusable() && third->reusable());
    CHECK(portrait.refs == 0 && next.refs == 0 && landscape.refs == 0);

    presenter = SurfaceBufferPresenter::create(&window, "half-turn", 18, false, true);
    geometry = {3040, 1904, 3040, 1904, ScanoutContentRotation::Clockwise180, 10, 1};
    CHECK(presenter->configureScanoutGeometry(geometry));
    auto halfTurn = presenter->present(&landscape, -1, 1, 10);
    CHECK(halfTurn && api.geometries.back().transform == ANATIVEWINDOW_TRANSFORM_ROTATE_180);
    CHECK(presenter->detach()); api.releaseBuffer(&landscape);

    presenter = SurfaceBufferPresenter::create(&window, "optional-api", 19, false, true);
    api.geometryAvailable = false;
    CHECK(!presenter->configureScanoutGeometry(geometry));
    auto legacy = presenter->present(&landscape); // Existing identity callers remain functional.
    CHECK(legacy != nullptr);
    CHECK(presenter->detach()); api.releaseBuffer(&landscape);
    api.geometryAvailable = true;
    ANativeWindow wrongParent{1904, 3040};
    presenter = SurfaceBufferPresenter::create(&wrongParent, "wrong-parent", 20, false, true);
    CHECK(!presenter->configureScanoutGeometry(geometry)); // Never stretch into storage-sized parent.
    CHECK(!presenter->present(&landscape, -1, 3, 9)); // No installed tuple, no generation bypass.
    CHECK(presenter->detach());
    std::printf("%s presenter storage/logical geometry (%d failures)\n", failures ? "FAIL" : "PASS", failures);
    return failures ? 1 : 0;
}
