// SPDX-License-Identifier: Apache-2.0
#define NATIVE_FIFO_TRACE_TESTING
#include "surface_buffer_presenter.h"
#include <algorithm>
#include <atomic>
#include <cstring>
#include <iostream>
#include <sstream>
#include <string>
#include <vector>
using namespace android_display;
namespace {
struct Boundary {
    std::atomic<uint64_t> ns{1000000000};
    std::atomic<int> clocks{0}, arms{0}, opens{0}, writes{0}, pids{0}, tids{0};
    bool arm = true, failOpen = false, shortWrite = false;
    std::mutex mutex;
    std::vector<std::string> lines;
} *boundary;
uint64_t now() { ++boundary->clocks; return boundary->ns.load(); }
bool armed() { ++boundary->arms; return boundary->arm; }
int openMarker() { ++boundary->opens; return boundary->failOpen ? -1 : 8; }
ssize_t writeMarker(int fd, const void* data, size_t bytes) {
    assert(fd == 8); ++boundary->writes;
    std::lock_guard lock(boundary->mutex);
    boundary->lines.emplace_back(static_cast<const char*>(data), bytes);
    if (boundary->shortWrite) { boundary->shortWrite = false; return bytes - 1; }
    return bytes;
}
int pid() { ++boundary->pids; return 17; }
long tid() { ++boundary->tids; return syscall(SYS_gettid); }
NativeFifoTraceIo io{now, armed, openMarker, writeMarker, pid, tid};
std::string field(const std::string& line, const std::string& key) {
    const auto at = line.find(" " + key + "=");
    assert(at != std::string::npos);
    const auto start = at + key.size() + 2;
    return line.substr(start, line.find_first_of(" \n", start) - start);
}
void barrier() {
    auto& api = SurfaceControl::GetInstance();
    auto* tx = api.ASurfaceTransaction_create();
    SurfaceTransactionExecutor::apply(tx);
    api.ASurfaceTransaction_delete(tx);
}
void chain(bool enabled) {
    Boundary b; boundary = &b;
    NativeFifoTraceState state(enabled, io); nativeFifoTraceTestState = &state;
    auto& api = SurfaceControl::GetInstance(); api.open = true;
    ANativeWindow window;
    auto presenter = SurfaceBufferPresenter::create(&window, "trace", 73);
    AHardwareBuffer buffers[]{{1,0},{2,0},{3,0},{4,0}};
    std::vector<std::shared_ptr<ConsumerRelease>> tickets;
    for (int i = 0; i < 4; ++i) {
        const auto id = nativeFifoTraceBegin(0xd213, i+20, 100+i, 200+i, 9, 2, 1, 40+i);
        if (enabled) assert(id == uint64_t(i+1)); else assert(id == 0);
        assert(nativeFifoTraceBegin(99,99,99,99,99,99,99,99) == 0);
        nativeFifoTraceOperation(300+i); nativeFifoTraceBuffer(400+i,500+i);
        tickets.push_back(presenter->present(&buffers[i]));
        nativeFifoTraceEnd(id);
        assert(nativeFifoTraceCurrent.id == 0);
    }
    assert(tickets[0] && tickets[1] && tickets[2] && !tickets[3]);
    assert(buffers[3].refs == 0);
    barrier();
    if (enabled) {
        const std::vector<std::string> expected{"packet","operation","bridge","admit","enqueue","notify","apply_begin","apply_end"};
        std::lock_guard lock(b.mutex);
        for (const auto& line : b.lines) {
            assert(line.rfind("I|17|NFIFO1 ",0) == 0 && line.back() == '\n');
            assert(std::count(line.begin(),line.end(),'\n') == 1 && line.size() < 768);
        }
        std::string applyTid;
        for (unsigned id = 1; id <= 4; ++id) {
            std::vector<std::string> stages;
            for (const auto& line : b.lines) {
                if (field(line,"id") != std::to_string(id)) continue;
                const auto stage = field(line,"st");
                assert(field(line,"r") == std::to_string(id+19));
                assert(field(line,"gs") == std::to_string(id+99));
                assert(field(line,"f") == std::to_string(id+199));
                assert(field(line,"ctx") == "9" && field(line,"ring") == "2");
                assert(field(line,"fl") == "1" && field(line,"d") == std::to_string(id+39));
                if (stage == "packet_end") continue;
                stages.push_back(stage);
                if (stage != "packet") assert(field(line,"op") == std::to_string(id+299));
                if (stage == "admit" || stage == "enqueue" || stage == "notify" || stage.find("apply_") == 0) {
                    assert(field(line,"g") == "73" && field(line,"s") == std::to_string(id));
                    assert(field(line,"bid") == std::to_string(id+399));
                    assert(field(line,"nbid") == std::to_string(id+499));
                }
                if (stage.find("apply_") == 0) {
                    assert(field(line,"v") != "0");
                    if (applyTid.empty()) applyTid = field(line,"tid");
                    assert(field(line,"tid") == applyTid);
                    assert(applyTid != std::to_string(syscall(SYS_gettid)));
                }
            }
            assert(stages == (id <= 3 ? expected : std::vector<std::string>{"packet","operation","bridge"}));
        }
    } else {
        assert(b.clocks == 0 && b.arms == 0 && b.opens == 0 && b.writes == 0 && b.pids == 0 && b.tids == 0);
    }
    presenter->requestDetach(); barrier();
    for (int i = 0; i < 3; ++i) api.releaseBuffer(&buffers[i]);
    presenter.reset(); nativeFifoTraceTestState = nullptr;
}
void limits() {
    Boundary b; boundary = &b;
    {
        NativeFifoTraceState state(true, io, 1, 20);
        auto c = state.begin({}); assert(c.id == 1);
        assert(state.begin({}).id == 0 && !state.enabled());
        for (int i=0;i<20;++i) state.event("unused",c);
        assert(b.lines.size() == 3 && field(b.lines.back(),"v") == "1");
    }
    b.lines.clear();
    {
        NativeFifoTraceState state(true, io, 20, 2);
        auto c = state.begin({}); state.event("overflow",c); state.event("again",c);
        assert(b.lines.size() == 3 && field(b.lines.back(),"st") == "limit" && field(b.lines.back(),"v") == "2");
    }
    b.lines.clear();
    {
        NativeFifoTraceState state(true, io, 20, 20, 100);
        auto c = state.begin({}); b.ns += 100; state.event("expired",c);
        assert(b.lines.size() == 3 && field(b.lines.back(),"v") == "3");
    }
    b.lines.clear(); b.arms = 0; b.arm = false;
    {
        NativeFifoTraceState state(true, io);
        assert(state.begin({}).id == 0 && b.arms == 1);
        b.arm = true; b.ns += 50000000;
        assert(state.begin({}).id == 0 && b.arms == 1);
        b.ns += 50000000;
        assert(state.begin({}).id == 1 && b.arms == 2);
    }
    b.lines.clear(); b.shortWrite = true;
    {
        NativeFifoTraceState state(true, io);
        NativeFifoTraceContext c;
        c.guestSequence=c.fence=c.operationSequence=c.generation=c.ticket=c.bufferId=c.nativeBufferId=UINT64_MAX;
        c.command=c.resource=c.context=c.descriptor=c.flags=c.ring=UINT32_MAX;
        c = state.begin(c); state.event("apply_begin",c,UINT64_MAX);
        assert(b.lines.size() == 3 && field(b.lines[1],"lost") == "1");
        assert(field(b.lines[2],"v") == std::to_string(UINT64_MAX));
        assert(field(b.lines[2],"nbid") == std::to_string(UINT64_MAX));
        assert(b.lines[2].size() < 768 && b.lines[2].back() == '\n');
    }
    b.lines.clear(); b.failOpen = true;
    {
        NativeFifoTraceState state(true, io);
        assert(state.begin({}).id == 0 && !state.enabled());
        const int clocks = b.clocks; state.begin({});
        assert(b.lines.empty() && b.clocks == clocks);
    }
}
}
int main(int argc, char** argv) {
    assert(argc == 2);
    if (!strcmp(argv[1],"chain")) chain(true);
    else if (!strcmp(argv[1],"disabled")) chain(false);
    else { assert(!strcmp(argv[1],"limits")); limits(); }
    std::cout << "PASS " << argv[1] << '\n';
}
