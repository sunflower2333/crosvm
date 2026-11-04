// SPDX-License-Identifier: Apache-2.0
#include "surface_attachment_lease.h"
#include <cassert>
#include <cstdio>

using android_display::SurfaceAttachmentLease;
struct SurfaceModel {
    SurfaceAttachmentLease owner;
    uint64_t generation = 0;
    int detached = 0, released = 0, hint = 0;
    uint64_t attach(bool owned = true) {
        // Replace/detach queues removal, never manufactures consumer release.
        if (generation) ++detached;
        owner.clear();
        owner.attached(++generation, owned);
        return owner.current();
    }
    bool detach(uint64_t token) {
        if (!owner.accepts(token)) return false;
        owner.clear();
        ++detached;
        return true;
    }
    bool setHint(uint64_t token, int value) {
        if (!owner.accepts(token)) return false;
        hint = value;
        return true;
    }
};
int main() {
    SurfaceModel main, cursor;
    const auto oldMain = main.attach(), oldCursor = cursor.attach();
    assert(cursor.setHint(oldCursor, 4));
    // New Activity surfaces attach BEFORE old SurfaceDestroyed/OnDestroy.
    const auto newMain = main.attach(), newCursor = cursor.attach();
    assert(newMain != oldMain && newCursor != oldCursor);
    const int mainDetaches = main.detached, cursorDetaches = cursor.detached;
    assert(!main.detach(oldMain) && !cursor.detach(oldCursor));
    assert(!main.detach(oldMain) && !cursor.detach(oldCursor)); // late shutdown
    assert(!cursor.setHint(oldCursor, 7)); // delayed background Binder RPC
    assert(cursor.hint == 4);
    assert(main.owner.accepts(newMain) && cursor.owner.accepts(newCursor));
    assert(!main.owner.permitsAttach(false) && !cursor.owner.permitsAttach(false));
    assert(main.owner.permitsAttach(true) && cursor.owner.permitsAttach(true));
    assert(main.detached == mainDetaches && cursor.detached == cursorDetaches);
    assert(!main.detach(0) && !cursor.setHint(0, 3)); // legacy callback cannot win
    assert(cursor.setHint(newCursor, 7) && cursor.hint == 7);
    assert(main.released == 0 && cursor.released == 0);
    // Guest presenter reset does not revoke a still-attached app's lease.
    ++main.detached;
    assert(main.owner.accepts(newMain));
    assert(main.detach(newMain) && cursor.detach(newCursor));
    assert(!main.detach(newMain) && !cursor.setHint(newCursor, 4));
    assert(main.released == 0 && cursor.released == 0);
    const auto replacement = main.attach();
    assert(replacement != newMain && !main.detach(newMain));
    assert(main.owner.accepts(replacement));
    SurfaceModel legacy;
    assert(legacy.owner.permitsAttach(false));
    assert(legacy.attach(false) == 0 && legacy.detach(0));
    const auto owned = legacy.attach();
    assert(!legacy.detach(0) && legacy.owner.accepts(owned));
    puts("PASS competing Activity attach/detach/hint, duplicate teardown, legacy isolation and release independence");
}
