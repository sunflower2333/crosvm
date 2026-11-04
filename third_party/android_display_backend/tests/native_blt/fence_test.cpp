// SPDX-License-Identifier: Apache-2.0
#include <linux/sync_file.h>
#include <sys/ioctl.h>
#include <poll.h>
#include <cerrno>
#include <cstdio>
int syncStatus=1, ioctlResult=0, closed=0, failed=0;
int fakeIoctl(int, unsigned long, sync_file_info* info) { info->status=syncStatus; return ioctlResult; }
int fakePoll(pollfd* p, unsigned long, int) { p->revents=POLLIN; return 1; }
int fakeClose(int) { ++closed; return 0; }
void check(bool ok, const char*) { failed += !ok; }
#define ioctl fakeIoctl
#define poll fakePoll
#define close fakeClose
namespace android_display {
// INSERT_REAL_FENCE_STATUS
}
// INSERT_REAL_ORACLE_WAIT
int main() {
    for (int status: {1,0,-EIO}) {
        syncStatus=status; closed=failed=0; waitComplete(41);
        if (closed!=1 || failed != (status==1 ? 0 : 1)) {
            std::printf("FAIL readable sync_file status=%d accepted=%d\n",status,failed==0); return 1;
        }
    }
    ioctlResult=-1; syncStatus=1; failed=closed=0; waitComplete(41);
    if (closed!=1 || failed!=1) { std::puts("FAIL ordinary readable fd accepted"); return 1; }
    std::puts("native Blt oracle: successful/error/active/non-sync fences PASS");
}
