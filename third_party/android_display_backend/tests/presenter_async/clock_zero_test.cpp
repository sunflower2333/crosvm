// SPDX-License-Identifier: Apache-2.0
// Regression derived from fifo_peer28's independent actual-header reproduction.
#include "native_fifo_trace.h"
#include <cassert>
#include <cstring>
#include <iostream>
#include <string>
#include <vector>
using namespace android_display;
static uint64_t ns = 1000000000;
static unsigned clocks = 0, arms = 0, opens = 0, writes = 0;
static std::vector<std::string> lines;
static uint64_t now() { ++clocks; return ns; }
static bool armed() { ++arms; return true; }
static int openMarker() { ++opens; return 8; }
static ssize_t writeMarker(int fd, const void* data, size_t count) {
    assert(fd == 8); ++writes;
    lines.emplace_back(static_cast<const char*>(data), count);
    return count;
}
static int pid() { return 17; }
static long tid() { return 19; }
static NativeFifoTraceIo io{now, armed, openMarker, writeMarker, pid, tid};
int main(int argc, char** argv) {
    assert(argc == 2);
    NativeFifoTraceState state(true, io);
    if (!strcmp(argv[1], "unarmed-begin")) {
        ns = 0;
        assert(state.begin({}).id == 0 && state.enabled());
        assert(clocks == 1 && arms == 0 && opens == 0 && writes == 0);
        ns = 1000000000;
        assert(state.begin({}).id == 1 && state.enabled());
        assert(arms == 1 && opens == 1 && writes == 2);
    } else {
        auto first = state.begin({});
        assert(first.id == 1 && lines.size() == 2);
        ns = 0;
        if (!strcmp(argv[1], "armed-begin")) assert(state.begin({}).id == 0);
        else {
            assert(!strcmp(argv[1], "armed-event"));
            state.event("operation", first);
        }
        assert(!state.enabled() && lines.size() == 3);
        assert(lines.back().find(" st=limit ") != std::string::npos);
        assert(lines.back().find(" ns=0 ") != std::string::npos);
        assert(lines.back().find(" v=3 ") != std::string::npos);
        assert(arms == 1 && opens == 1 && writes == 3);
        const unsigned clockCount = clocks;
        ns = 1000000100;
        assert(state.begin({}).id == 0);
        state.event("operation", first);
        assert(!state.enabled() && lines.size() == 3 && clocks == clockCount);
        assert(arms == 1 && opens == 1 && writes == 3);
    }
    std::cout << "PASS " << argv[1] << '\n';
}
