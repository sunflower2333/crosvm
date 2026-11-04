#include <cerrno>
#include <cstdint>
#include <cstdlib>
#include <fcntl.h>
#include <iostream>
#include <unistd.h>
#include "native_blt.h"
#define LOG(level) std::cerr
namespace android::base {struct unique_fd {int value;explicit unique_fd(int f):value(f){}~unique_fd(){if(value>=0)close(value);}};}
struct Result{bool good;bool ok(){return good;}const char* error(){return "injected completion failure";}};
struct Bridge{bool good=true;unsigned waits=0;Result retireCompletedCopies(){++waits;return {good};}};
struct NativeBltContext{Bridge bridge;bool poisoned=false;};
int status=0,lastFd=-1;
extern "C" int android_display_native_blt_copy(void*,void*,void*,const NativeBltRegion*,uint32_t,int* out){*out=status? -1:open("/dev/null",O_RDONLY);lastFd=*out;return status;}
// INSERT_REAL_SYNC
void check(bool v){if(!v)std::abort();}
int main(){NativeBltContext c;check(android_display_native_blt_copy_wait(&c,nullptr,nullptr,nullptr,0)==0);check(c.bridge.waits==1&&!c.poisoned&&fcntl(lastFd,F_GETFD)==-1);c.bridge.good=false;check(android_display_native_blt_copy_wait(&c,nullptr,nullptr,nullptr,0)==-EIO);check(c.bridge.waits==2&&c.poisoned&&fcntl(lastFd,F_GETFD)==-1);status=-EINVAL;check(android_display_native_blt_copy_wait(&c,nullptr,nullptr,nullptr,0)==-EINVAL&&c.bridge.waits==2);std::cout<<"native Blt sync completion and owned fd: PASS\n";}
