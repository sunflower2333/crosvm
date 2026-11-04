// SPDX-License-Identifier: Apache-2.0
#include "cursor_state.h"

#include <atomic>
#include <cassert>
#include <cstdio>
#include <fcntl.h>
#include <signal.h>
#include <thread>

using android_display::CursorImageCache;
using android_display::CursorPositionChannel;
using android_display::CursorTransformHint;

struct Pipe {
    int readFd, writeFd;
    Pipe() {
        int fds[2];
        assert(pipe(fds) == 0);
        readFd = fds[0]; writeFd = fds[1];
        assert(fcntl(readFd, F_SETFL, O_NONBLOCK) == 0);
    }
    ~Pipe() { close(readFd); close(writeFd); }
    std::array<uint32_t, 2> readPair() {
        std::array<uint32_t, 2> pair;
        assert(read(readFd, pair.data(), sizeof(pair)) == sizeof(pair));
        return pair;
    }
    void expect(uint32_t x, uint32_t y) {
        auto pair = readPair(); assert(pair[0] == x && pair[1] == y);
    }
    void expectEmpty() {
        uint64_t pair;
        assert(read(readFd, &pair, sizeof(pair)) == -1 && errno == EAGAIN);
    }
};

static void imageLifecycle() {
    CursorImageCache image;
    assert(!image.valid() && !image.publish() && image.begin() == nullptr);
    assert(!image.configure(0, 3));
    assert(image.configure(3, 3));
    auto* pending = image.begin();
    assert(pending && image.begin() == nullptr);
    for (int i = 0; i < 9; ++i) pending[i] = i + 1;
    assert(!image.valid()); // Unfinished producer contents cannot be replayed.
    assert(image.publish() && image.valid());
    const uint32_t expected[4][9] = {
        {1,2,3,4,5,6,7,8,9}, {7,4,1,8,5,2,9,6,3},
        {9,8,7,6,5,4,3,2,1}, {3,6,9,2,5,8,1,4,7}
    };
    const auto revision = image.revision();
    for (unsigned rotation = 0; rotation != 4; ++rotation) {
        uint32_t target[15];
        std::fill(std::begin(target), std::end(target), 0xfeedface);
        assert(image.copyTo(target, 5, rotation));
        for (int y = 0; y != 3; ++y) {
            for (int x = 0; x != 3; ++x) assert(target[y*5+x] == expected[rotation][y*3+x]);
            assert(target[y*5+3] == 0xfeedface && target[y*5+4] == 0xfeedface);
        }
        assert(image.revision() == revision); // Reattach/rotation replay is not a new producer frame.
    }
    assert(image.configure(3, 3) && image.valid());
    pending = image.begin();
    pending[0] = 99;
    uint32_t target[9];
    assert(image.copyTo(target, 3, 0));
    assert(target[0] == 1 && target[8] == 9); // Binder replay during a write sees the old COMPLETE image.
    assert(!image.configure(2, 2)); // Resizing cannot invalidate an active writer's pointer.
    for (int i = 0; i != 9; ++i) pending[i] = 99 - i;
    assert(image.publish());
    assert(image.copyTo(target, 3, 0) && target[0] == 99 && target[8] == 91);
    assert(image.revision() == revision + 1 && !image.publish());
    assert(!image.copyTo(target, 2, 0) && !image.copyTo(target, 3, 4));
    assert(image.configure(2, 3) && !image.valid());
    pending = image.begin();
    for (int i = 0; i != 6; ++i) pending[i] = i;
    assert(image.publish() && image.copyTo(target, 2, 0));
    assert(!image.copyTo(target, 2, 1)); // Quarter-turn pre-rotation remains square-only.
}

static void positionLifecycle() {
    CursorPositionChannel channel;
    Pipe first, second;
    assert(channel.attach(first.writeFd) == 0);
    first.expectEmpty(); // Unknown state is not a fabricated visible cursor at 0,0.
    assert(channel.update(123, 456) == 0);
    first.expect(123, 456);
    assert(channel.attach(second.writeFd) == 0);
    second.expect(123, 456); first.expectEmpty();
    assert(channel.update(UINT32_MAX, UINT32_MAX) == 0);
    second.expect(UINT32_MAX, UINT32_MAX);
    assert(channel.attach(first.writeFd) == 0);
    first.expect(UINT32_MAX, UINT32_MAX); // Hide survives reattach exactly.
    assert(channel.update(static_cast<uint32_t>(-12), 34) == 0);
    first.expect(static_cast<uint32_t>(-12), 34); // The channel does not reinterpret signed bits.
    assert(channel.attach(-1) == EBADF); // Failed attach retains the previous stream.
    assert(channel.update(7, 8) == 0); first.expect(7, 8);

    CursorPositionChannel background;
    assert(background.update(15, 16) == 0); // Movement before app attach is retained.
    assert(background.attach(second.writeFd) == 0); second.expect(15, 16);
    close(second.readFd); second.readFd = -1;
    assert(background.update(17, 18) == EPIPE);
    assert(background.attach(first.writeFd) == 0); first.expect(17, 18);
}

static void concurrentReattach() {
    CursorPositionChannel channel;
    Pipe first, second;
    constexpr uint32_t mask = 0x5a5a5a5a;
    assert(channel.update(0, mask) == 0);
    assert(channel.attach(first.writeFd) == 0);
    std::thread producer([&] {
        for (uint32_t i = 1; i <= 100; ++i) assert(channel.update(i, i ^ mask) == 0);
    });
    std::thread binder([&] {
        for (int i = 0; i != 100; ++i)
            assert(channel.attach((i & 1) ? first.writeFd : second.writeFd) == 0);
    });
    producer.join(); binder.join();
    int records = 0;
    for (int fd : {first.readFd, second.readFd}) {
        std::array<uint32_t, 2> pair;
        ssize_t count;
        while ((count = read(fd, pair.data(), sizeof(pair))) > 0) {
            assert(count == sizeof(pair) && pair[1] == (pair[0] ^ mask));
            ++records;
        }
        assert(count == -1 && errno == EAGAIN);
    }
    assert(records == 201);
    assert(channel.attach(second.writeFd) == 0); second.expect(100, 100 ^ mask);
}

static void sameSizeRotationWithQueueOnlyHint() {
    CursorImageCache image;
    CursorTransformHint hint;
    assert(image.configure(3, 3));
    auto pixels = image.begin();
    for (int i = 0; i < 9; ++i) pixels[i] = i + 1;
    assert(image.publish());

    // Model Surface's producer-side cache: root rotation changes independently,
    // but querying the native window keeps returning the old hint until a
    // buffer has been queued. No resolution or guest image change occurs.
    int rootHint = 4, producerHint = 4, postedHint = -1;
    uint64_t postedRevision = 0;
    int posts = 0;
    uint32_t target[9]{};
    auto redraw = [&] {
        const int selected = hint.resolve(producerHint);
        if (postedRevision == image.revision() && postedHint == selected) return false;
        const unsigned turns = selected == 4 ? 1 : selected == 3 ? 2 : selected == 7 ? 3 : 0;
        assert(image.copyTo(target, 3, turns));
        postedHint = selected;
        postedRevision = image.revision();
        producerHint = rootHint; // queueBuffer reply is the only cache update.
        ++posts;
        return true;
    };
    assert(redraw() && posts == 1 && target[0] == 7);
    rootHint = 7;
    assert(!redraw() && producerHint == 4); // Existing cached-query skip reproduces.
    assert(hint.setAuthoritative(rootHint));
    assert(redraw() && posts == 2 && target[0] == 3 && postedHint == rootHint);
    assert(!redraw() && posts == 2); // No probe frames or retry loop.
    rootHint = 4;
    assert(producerHint == 7 && hint.setAuthoritative(rootHint));
    assert(redraw() && posts == 3 && target[0] == 7 && postedHint == rootHint);
    assert(!hint.setAuthoritative(-1) && !hint.setAuthoritative(1));
    assert(!hint.setAuthoritative(5) && !hint.setAuthoritative(90));
    assert(hint.resolve(7) == 4 && !redraw());

    // A new Activity can supply the hint before producer connect or before the
    // first complete cursor shape. Reattach need not learn it through probes.
    CursorTransformHint detached;
    assert(detached.setAuthoritative(7) && detached.resolve(0) == 7);
    assert(detached.setAuthoritative(0) && detached.resolve(7) == 0);
    assert(detached.setAuthoritative(3) && detached.resolve(4) == 3);
}

int main() {
    signal(SIGPIPE, SIG_IGN); // Match the Rust host's handling of a closed app pipe.
    imageLifecycle();
    positionLifecycle();
    concurrentReattach();
    sameSizeRotationWithQueueOnlyHint();
    puts("PASS cursor lifecycle, same-size rotation with stale queued hint, position/hide replay and concurrent reattach");
}
