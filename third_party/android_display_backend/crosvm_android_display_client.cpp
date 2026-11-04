/*
 * Copyright 2024 The Android Open Source Project
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *      http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

#include <aidl/android/crosvm/BnCrosvmAndroidDisplayService.h>
#include <aidl/android/crosvm/DisplayConfig.h>
#include <android-base/logging.h>
#include <android-base/result.h>
#include <android-base/scopeguard.h>
#include <android/binder_manager.h>
#include <android/binder_process.h>
#include <android/hardware_buffer.h>
#define ATRACE_TAG ATRACE_TAG_GRAPHICS
#include <cutils/trace.h>
#include <system/graphics.h> // for HAL_PIXEL_FORMAT_*
#include <system/window.h>
#include <vndk/window.h>
#define VK_NO_PROTOTYPES
#include <dlfcn.h>
#include <poll.h>
#include <sys/eventfd.h>
#include <sys/stat.h>
#include <unistd.h>
#include <vulkan/vulkan.h>
#include <vulkan/vulkan_android.h>

#include <algorithm>
#include <atomic>
#include <cerrno>
#include <condition_variable>
#include <cstdlib>
#include <cstring>
#include <iomanip>
#include <iterator>
#include <memory>
#include <mutex>
#include <unordered_map>
#include <utility>
#include <vector>

#if defined(__aarch64__)
#include <arm_neon.h>
#endif

#include "media_codec_dl.h"
#include "gpu_clock_floor.h"
#include "surface_control_dl.h"
#include "surface_buffer_presenter.h"
#include "surface_attachment_lease.h"
#include "scanout_profile.h"
#include "cursor_state.h"
#include "shared_ahb_allocation.h"
#include "shared_ahb_layout.h"
#include "native_blt.h"
#include "native_blt_transfer.h"

using aidl::android::crosvm::BnCrosvmAndroidDisplayService;
using aidl::android::view::Surface;

using android::base::Error;
using android::base::Result;

namespace {

// Process-wide observations at the actual operations below. These also include
// legacy display/encoder consumers; they are not per-buffer zero-copy claims.
struct DisplayPathCounters {
    std::atomic<uint64_t> imageCopyCommands{0}, imageBlitCommands{0}, blitReadMaps{0};
    std::atomic<uint64_t> savedBufferCopyBytes{0}, encoderUploadBytes{0};
    std::atomic<uint64_t> cursorBufferCopyBytes{0};
    std::atomic<uint64_t> directAccepted{0}, directReleased{0};
};
DisplayPathCounters gDisplayPathCounters;

void logDirectAhb(const char* event, uint64_t count, uint64_t bufferId,
                  uint64_t nativeBufferId, uint64_t generation, uint64_t sequence) {
    // Preserve the first two transactions of each new consumer generation so
    // static Activity refresh acceptance/release and copy counters are visible.
    if (count > 16 && count % 128 != 0 && sequence > 2) return;
    SharedAhbPagingCounters paging{};
    crosvm_shared_ahb_get_paging_counters(&paging);
    LOG(INFO) << "NATIVE-AHB-SURFACE event=" << event << " count=" << count
              << " buffer_id=" << bufferId << " native_buffer_id=" << nativeBufferId
              << " generation=" << generation
              << " sequence=" << sequence << " path=surfacecontrol"
              << " counters_scope=android_display_process"
              << " vk_image_copy_commands=" << gDisplayPathCounters.imageCopyCommands.load()
              << " vk_image_blit_commands=" << gDisplayPathCounters.imageBlitCommands.load()
              << " blit_read_maps=" << gDisplayPathCounters.blitReadMaps.load()
              << " saved_buffer_copy_bytes=" << gDisplayPathCounters.savedBufferCopyBytes.load()
              << " cursor_buffer_copy_bytes=" << gDisplayPathCounters.cursorBufferCopyBytes.load()
              << " encoder_upload_bytes=" << gDisplayPathCounters.encoderUploadBytes.load()
              << " explicit_paging_read_bytes=" << paging.read_bytes
              << " explicit_paging_write_bytes=" << paging.write_bytes
              << " explicit_paging_fill_bytes=" << paging.fill_bytes;
}

class ScopedTrace {
public:
    explicit ScopedTrace(const char* name) { ATRACE_BEGIN(name); }
    ~ScopedTrace() { ATRACE_END(); }

    ScopedTrace(const ScopedTrace&) = delete;
    ScopedTrace& operator=(const ScopedTrace&) = delete;
};

bool envFlagEnabled(const char* name, bool defaultValue) {
    const char* value = std::getenv(name);
    if (value == nullptr || *value == '\0') return defaultValue;
    if (std::strcmp(value, "1") == 0 || std::strcmp(value, "true") == 0 ||
        std::strcmp(value, "on") == 0) {
        return true;
    }
    if (std::strcmp(value, "0") == 0 || std::strcmp(value, "false") == 0 ||
        std::strcmp(value, "off") == 0) {
        return false;
    }
    LOG(WARNING) << "Ignoring invalid " << name << " value '" << value << "'";
    return defaultValue;
}

bool isSurfaceUnavailableStatus(int status) {
    // BufferQueue reports an abandoned producer as DEAD_OBJECT, whose status_t value is
    // -EPIPE. This is the normal result when the app process or its SurfaceView is recreated.
    // Leaving it out makes every guest flush tear down and rebuild the Vulkan bridge, then try
    // the same dead producer again, causing a high-CPU/GPU retry loop until crosvm exits.
    return status == -EPIPE || status == -ENODEV || status == -EINVAL;
}

enum class RuntimeFlipFailureStage {
    kNone,
    kDequeue,
    kTargetImport,
    kSubmit,
    kQueue,
};

RuntimeFlipFailureStage configuredRuntimeFlipFailureStage() {
    static const RuntimeFlipFailureStage stage = [] {
        const char* value = std::getenv("CROSVM_ANDROID_DISPLAY_FAIL_STAGE");
        if (value == nullptr || *value == '\0') return RuntimeFlipFailureStage::kNone;
        if (std::strcmp(value, "dequeue") == 0) return RuntimeFlipFailureStage::kDequeue;
        if (std::strcmp(value, "target_import") == 0) {
            return RuntimeFlipFailureStage::kTargetImport;
        }
        if (std::strcmp(value, "submit") == 0) return RuntimeFlipFailureStage::kSubmit;
        if (std::strcmp(value, "queue") == 0) return RuntimeFlipFailureStage::kQueue;
        LOG(WARNING) << "Ignoring invalid CROSVM_ANDROID_DISPLAY_FAIL_STAGE value '" << value
                     << "'";
        return RuntimeFlipFailureStage::kNone;
    }();
    return stage;
}

const char* runtimeFlipFailureStageName(RuntimeFlipFailureStage stage) {
    switch (stage) {
        case RuntimeFlipFailureStage::kDequeue:
            return "dequeue";
        case RuntimeFlipFailureStage::kTargetImport:
            return "target_import";
        case RuntimeFlipFailureStage::kSubmit:
            return "submit";
        case RuntimeFlipFailureStage::kQueue:
            return "queue";
        case RuntimeFlipFailureStage::kNone:
            return "none";
    }
    return "unknown";
}

bool injectRuntimeFlipFailure(RuntimeFlipFailureStage stage) {
    if (configuredRuntimeFlipFailureStage() != stage) return false;
    static std::atomic<bool> injected = false;
    if (injected.exchange(true, std::memory_order_relaxed)) return false;
    LOG(WARNING) << "Injecting one-shot Android display runtime flip failure at stage="
                 << runtimeFlipFailureStageName(stage);
    return true;
}

struct HwModuleMethods {
    int (*open)(const void* module, const char* id, void** device);
};

struct HwModule {
    uint32_t tag;
    uint16_t module_api_version;
    uint16_t hal_api_version;
    const char* id;
    const char* name;
    const char* author;
    HwModuleMethods* methods;
    void* dso;
#ifdef __LP64__
    uint64_t reserved[32 - 7];
#else
    uint32_t reserved[32 - 7];
#endif
};

struct HwDevice {
    uint32_t tag;
    uint32_t version;
    void* module;
#ifdef __LP64__
    uint64_t reserved[12];
#else
    uint32_t reserved[12];
#endif
    int (*close)(void* device);
};

struct HwvulkanDevice {
    HwDevice common;
    PFN_vkEnumerateInstanceExtensionProperties enumerate_instance_extensions;
    PFN_vkCreateInstance create_instance;
    PFN_vkGetInstanceProcAddr get_instance_proc_addr;
};

class VulkanDisplayBridge {
public:
    VulkanDisplayBridge()
          : mAsyncBlitEnabled(envFlagEnabled("CROSVM_ANDROID_DISPLAY_ASYNC_BLIT", true)) {
        mReady = initialize();
    }

    ~VulkanDisplayBridge() {
        if (!mNativeDestructionReady && mDevice && mDeviceWaitIdle) mDeviceWaitIdle(mDevice);
        destroyNativeStaging();
        for (auto& slot : mInFlightSlots) {
            if (slot.completionSemaphore && mDestroySemaphore) {
                mDestroySemaphore(mDevice, slot.completionSemaphore, nullptr);
            }
            if (slot.acquireSemaphore && mDestroySemaphore) {
                mDestroySemaphore(mDevice, slot.acquireSemaphore, nullptr);
            }
            if (slot.fence && mDestroyFence) mDestroyFence(mDevice, slot.fence, nullptr);
        }
        clearTargetCache();
        for (auto& [_, imported] : mImports) destroyImport(imported);
        mImports.clear();
        if (mDevice && mTimestampQueryPool && mDestroyQueryPool) {
            mDestroyQueryPool(mDevice, mTimestampQueryPool, nullptr);
        }
        if (mDevice && mCommandPool && mDestroyCommandPool) {
            mDestroyCommandPool(mDevice, mCommandPool, nullptr);
        }
        if (mDevice && mDestroyDevice) mDestroyDevice(mDevice, nullptr);
        if (mInstance && mDestroyInstance) mDestroyInstance(mInstance, nullptr);
        if (mHal && mHal->common.close) mHal->common.close(mHal);
        if (mLibrary) dlclose(mLibrary);
    }

    bool ready() const { return mReady; }
    bool hasCompletionFence() const { return mAsyncBlitEnabled; }

    bool prepareNativeDestruction() {
        // Only the dedicated native context uses this gate. A timeout/OOM
        // leaves every slot/import/target alive for retry or reset quarantine.
        for (auto& slot : mInFlightSlots) {
            if (!slot.inFlight) continue;
            const VkResult result = mWaitForFences(mDevice, 1, &slot.fence, VK_TRUE, 1000000000ULL);
            if (result == VK_ERROR_DEVICE_LOST) {
                mNativeDestructionReady = true;
                return true;
            }
            if (result != VK_SUCCESS) return false;
        }
        // All accepted submissions are tracked by slots. Avoid a second,
        // fallible idle call in the destructor after proving their completion.
        mNativeDestructionReady = true;
        return true;
    }

    // Used only by the dedicated Native Blt context, after its caller's
    // completion wait. Never discard imports on an uncertain GPU completion.
    Result<void> retireCompletedCopies() {
        for (auto& slot : mInFlightSlots) {
            if (auto result = reclaimSlot(slot, 1000000000ULL); !result.ok()) return result;
        }
        for (auto& [_, imported] : mImports) destroyImport(imported);
        mImports.clear();
        return {};
    }

    // Explicit ordinary backing transfer. Native AHB memory is never mapped.
    // The one owned staging allocation survives any uncertain completion.
    Result<void> transferNative(void* holder, const SharedAllocationDescription& native,
            const uint8_t* ordinarySource, uint8_t* ordinaryDestination,
            const NativeBltCpuLayout& cpu, const NativeBltRegion* regions,
            uint32_t count, bool upload, uint64_t packedSize) {
        auto createBuffer=deviceProc<PFN_vkCreateBuffer>("vkCreateBuffer");
        auto getRequirements=deviceProc<PFN_vkGetBufferMemoryRequirements>("vkGetBufferMemoryRequirements");
        auto bindBuffer=deviceProc<PFN_vkBindBufferMemory>("vkBindBufferMemory");
        auto mapMemory=deviceProc<PFN_vkMapMemory>("vkMapMemory");
        auto flushMemory=deviceProc<PFN_vkFlushMappedMemoryRanges>("vkFlushMappedMemoryRanges");
        auto invalidateMemory=deviceProc<PFN_vkInvalidateMappedMemoryRanges>("vkInvalidateMappedMemoryRanges");
        auto copyToImage=deviceProc<PFN_vkCmdCopyBufferToImage>("vkCmdCopyBufferToImage");
        auto copyToBuffer=deviceProc<PFN_vkCmdCopyImageToBuffer>("vkCmdCopyImageToBuffer");
        auto getMemoryProperties=instanceProc<PFN_vkGetPhysicalDeviceMemoryProperties>("vkGetPhysicalDeviceMemoryProperties");
        if (!createBuffer || !getRequirements || !bindBuffer || !mapMemory || !flushMemory ||
                !invalidateMemory || !copyToImage || !copyToBuffer || !getMemoryProperties ||
                !deviceProc<PFN_vkUnmapMemory>("vkUnmapMemory") ||
                !deviceProc<PFN_vkDestroyBuffer>("vkDestroyBuffer") || mInFlightSlots.empty())
            return Error() << "Native Blt staging entry points unavailable";
        if (auto retired=retireCompletedCopies(); !retired.ok()) return retired.error();
        destroyNativeStaging();

        VkImage image=VK_NULL_HANDLE;
        if (upload) {
            auto target=acquireTargetImage(crosvm_shared_ahb_get_buffer(holder),native.width,native.height);
            if (!target.ok()) return target.error();
            image=target.value()->image;
        } else {
            android::base::unique_fd fd(crosvm_shared_ahb_duplicate_dma_buf(holder));
            if (fd.get()<0) return Error() << "Native Blt readback DMA-BUF unavailable";
            const int64_t imported=importDmabuf(fd.get(),native.plane_offset,native.plane_stride,
                native.modifier,true,native.width,native.height,native.fourcc);
            if (!imported) return Error() << "Native Blt readback image import failed";
            image=mImports.at(imported).sourceImage;
        }
        const VkBufferCreateInfo bufferInfo{
            .sType=VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
            .size=packedSize,
            .usage=upload ? VK_BUFFER_USAGE_TRANSFER_SRC_BIT : VK_BUFFER_USAGE_TRANSFER_DST_BIT,
            .sharingMode=VK_SHARING_MODE_EXCLUSIVE,
        };
        if (createBuffer(mDevice,&bufferInfo,nullptr,&mNativeStagingBuffer)!=VK_SUCCESS)
            return Error() << "Native Blt staging buffer allocation failed";
        VkMemoryRequirements requirements{};
        getRequirements(mDevice,mNativeStagingBuffer,&requirements);
        if (requirements.size<packedSize || requirements.size>android_display::kNativeBltStagingLimit)
            return Error() << "Native Blt staging allocation exceeds bounded extent";
        VkPhysicalDeviceMemoryProperties properties{};
        getMemoryProperties(mPhysicalDevice,&properties);
        uint32_t memoryType=UINT32_MAX;
        for (uint32_t i=0;i<properties.memoryTypeCount;++i) {
            const auto flags=properties.memoryTypes[i].propertyFlags;
            if (!(requirements.memoryTypeBits&(1u<<i)) || !(flags&VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT)) continue;
            memoryType=i;
            if (flags&VK_MEMORY_PROPERTY_HOST_COHERENT_BIT) break;
        }
        if (memoryType==UINT32_MAX) return Error() << "Native Blt staging host memory unavailable";
        const VkMemoryAllocateInfo allocate{
            .sType=VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
            .allocationSize=requirements.size,
            .memoryTypeIndex=memoryType,
        };
        if (mAllocateMemory(mDevice,&allocate,nullptr,&mNativeStagingMemory)!=VK_SUCCESS ||
                bindBuffer(mDevice,mNativeStagingBuffer,mNativeStagingMemory,0)!=VK_SUCCESS ||
                mapMemory(mDevice,mNativeStagingMemory,0,VK_WHOLE_SIZE,0,&mNativeStagingAddress)!=VK_SUCCESS)
            return Error() << "Native Blt staging memory setup failed";
        const VkMappedMemoryRange range{
            .sType=VK_STRUCTURE_TYPE_MAPPED_MEMORY_RANGE,
            .memory=mNativeStagingMemory, .offset=0, .size=VK_WHOLE_SIZE,
        };
        if (upload) {
            android_display::nativeBltPackUpload(ordinarySource,cpu,native.fourcc,regions,count,
                static_cast<uint8_t*>(mNativeStagingAddress));
            if (flushMemory(mDevice,1,&range)!=VK_SUCCESS)
                return Error() << "Native Blt staging upload flush failed";
        }
        InFlightSlot& slot=mInFlightSlots[mNextInFlightSlot++%mInFlightSlots.size()];
        const VkCommandBuffer command=slot.commandBuffer;
        if (mResetCommandBuffer(command,0)!=VK_SUCCESS || mResetFences(mDevice,1,&slot.fence)!=VK_SUCCESS)
            return Error() << "Native Blt transfer reset failed";
        const VkCommandBufferBeginInfo begin{
            .sType=VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
            .flags=VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
        };
        if (mBeginCommandBuffer(command,&begin)!=VK_SUCCESS)
            return Error() << "Native Blt transfer recording failed";
        const VkImageLayout transferLayout=upload ? VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL : VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL;
        const VkAccessFlags transferAccess=upload ? VK_ACCESS_TRANSFER_WRITE_BIT : VK_ACCESS_TRANSFER_READ_BIT;
        const VkImageMemoryBarrier acquire{
            .sType=VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
            .srcAccessMask=VK_ACCESS_MEMORY_READ_BIT|VK_ACCESS_MEMORY_WRITE_BIT,
            .dstAccessMask=transferAccess,
            .oldLayout=VK_IMAGE_LAYOUT_GENERAL, .newLayout=transferLayout,
            .srcQueueFamilyIndex=VK_QUEUE_FAMILY_FOREIGN_EXT, .dstQueueFamilyIndex=mQueueFamilyIndex,
            .image=image, .subresourceRange=colorRange(),
        };
        VkBufferMemoryBarrier bufferBarrier{
            .sType=VK_STRUCTURE_TYPE_BUFFER_MEMORY_BARRIER,
            .srcAccessMask=upload ? VK_ACCESS_HOST_WRITE_BIT : 0u,
            .dstAccessMask=upload ? VK_ACCESS_TRANSFER_READ_BIT : VK_ACCESS_TRANSFER_WRITE_BIT,
            .srcQueueFamilyIndex=VK_QUEUE_FAMILY_IGNORED, .dstQueueFamilyIndex=VK_QUEUE_FAMILY_IGNORED,
            .buffer=mNativeStagingBuffer, .offset=0, .size=VK_WHOLE_SIZE,
        };
        mCmdPipelineBarrier(command,VK_PIPELINE_STAGE_ALL_COMMANDS_BIT|VK_PIPELINE_STAGE_HOST_BIT,
            VK_PIPELINE_STAGE_TRANSFER_BIT,0,0,nullptr,1,&bufferBarrier,1,&acquire);
        VkDeviceSize offset=0;
        for (uint32_t i=0;i<count;++i) {
            const auto& r=regions[i];
            bool overlap=false;
            for (uint32_t p=0;p<i;++p) overlap|=android_display::nativeBltRegionsOverlap(regions[p],r);
            if (upload && overlap) {
                const VkMemoryBarrier dependency{.sType=VK_STRUCTURE_TYPE_MEMORY_BARRIER,
                    .srcAccessMask=VK_ACCESS_TRANSFER_WRITE_BIT,.dstAccessMask=VK_ACCESS_TRANSFER_WRITE_BIT};
                mCmdPipelineBarrier(command,VK_PIPELINE_STAGE_TRANSFER_BIT,VK_PIPELINE_STAGE_TRANSFER_BIT,
                    0,1,&dependency,0,nullptr,0,nullptr);
            }
            const VkBufferImageCopy copy{
                .bufferOffset=offset, .bufferRowLength=0, .bufferImageHeight=0,
                .imageSubresource={VK_IMAGE_ASPECT_COLOR_BIT,0,0,1},
                .imageOffset={int32_t(upload?r.destination_x:r.source_x),int32_t(upload?r.destination_y:r.source_y),0},
                .imageExtent={r.width,r.height,1},
            };
            if (upload) copyToImage(command,mNativeStagingBuffer,image,transferLayout,1,&copy);
            else copyToBuffer(command,image,transferLayout,mNativeStagingBuffer,1,&copy);
            offset+=uint64_t(r.width)*r.height*4;
        }
        const VkImageMemoryBarrier release{
            .sType=VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
            .srcAccessMask=transferAccess, .dstAccessMask=VK_ACCESS_MEMORY_READ_BIT|VK_ACCESS_MEMORY_WRITE_BIT,
            .oldLayout=transferLayout, .newLayout=VK_IMAGE_LAYOUT_GENERAL,
            .srcQueueFamilyIndex=mQueueFamilyIndex, .dstQueueFamilyIndex=VK_QUEUE_FAMILY_FOREIGN_EXT,
            .image=image, .subresourceRange=colorRange(),
        };
        bufferBarrier.srcAccessMask=upload?VK_ACCESS_TRANSFER_READ_BIT:VK_ACCESS_TRANSFER_WRITE_BIT;
        bufferBarrier.dstAccessMask=upload?0u:VK_ACCESS_HOST_READ_BIT;
        mCmdPipelineBarrier(command,VK_PIPELINE_STAGE_TRANSFER_BIT,
            VK_PIPELINE_STAGE_ALL_COMMANDS_BIT|VK_PIPELINE_STAGE_HOST_BIT,0,0,nullptr,1,&bufferBarrier,1,&release);
        if (mEndCommandBuffer(command)!=VK_SUCCESS)
            return Error() << "Native Blt transfer end failed";
        const VkSubmitInfo submit{.sType=VK_STRUCTURE_TYPE_SUBMIT_INFO,
            .commandBufferCount=1,.pCommandBuffers=&command};
        // Conservatively retain command/image/staging objects on an unknown
        // submission failure as well as a wait failure. Only completion or
        // device loss permits the dedicated context destruction gate to free.
        slot.inFlight=true;
        const VkResult submitted=mQueueSubmit(mQueue,1,&submit,slot.fence);
        if (submitted!=VK_SUCCESS) {
            if (submitted==VK_ERROR_OUT_OF_HOST_MEMORY || submitted==VK_ERROR_OUT_OF_DEVICE_MEMORY)
                slot.inFlight=false; // Vulkan specifies these submissions had no effect.
            return Error() << "Native Blt transfer submit failed: " << submitted;
        }
        if (auto completed=reclaimSlot(slot,1000000000ULL); !completed.ok()) return completed.error();
        if (!upload) {
            if (invalidateMemory(mDevice,1,&range)!=VK_SUCCESS)
                return Error() << "Native Blt staging readback invalidate failed";
            android_display::nativeBltUnpackReadback(static_cast<const uint8_t*>(mNativeStagingAddress),
                native.fourcc,cpu,regions,count,ordinaryDestination);
        }
        return {};
    }

    int64_t importDmabuf(int fd, uint32_t offset, uint32_t stride, uint64_t modifier,
                         bool linearLayoutVerified, uint32_t width, uint32_t height,
                         uint32_t fourcc) {
        if (!mReady || fd < 0 || !width || !height ||
            static_cast<uint64_t>(stride) < static_cast<uint64_t>(width) * 4) {
            return 0;
        }
        if (!linearLayoutVerified) {
            LOG(WARNING) << "refusing direct image import without verified linear provenance";
            return 0;
        }
        if (modifier == kDrmFormatModInvalid) modifier = kDrmFormatModLinear;
        if (modifier != kDrmFormatModLinear) {
            LOG(WARNING) << "unsupported verified display modifier 0x" << std::hex << modifier;
            return 0;
        }

        VkFormat sourceFormat = vkFormatFromDrmFourcc(fourcc);
        if (sourceFormat == VK_FORMAT_UNDEFINED) {
            LOG(ERROR) << "unsupported display DRM fourcc 0x" << std::hex << fourcc;
            return 0;
        }

        ImportedImage imported{};
        imported.format = sourceFormat;
        imported.width = width;
        imported.height = height;
        if (!createSourceImage(fd, offset, stride, modifier, sourceFormat, imported)) {
            destroyImport(imported);
            return 0;
        }

        int64_t importId = mNextImportId++;
        mImports.emplace(importId, std::move(imported));
        return importId;
    }

    void release(int64_t importId) {
        auto it = mImports.find(importId);
        if (it == mImports.end()) return;
        if (mDeviceWaitIdle) mDeviceWaitIdle(mDevice);
        for (auto& slot : mInFlightSlots) reclaimSlotAfterDeviceIdle(slot);
        destroyImport(it->second);
        mImports.erase(it);
    }

    // Set when the async path can import an acquire fence as a GPU wait semaphore, letting the
    // caller skip the CPU poll on the dequeued release fence.
    bool canImportAcquireFence() const { return mAsyncBlitEnabled && mAsyncAcquireImportSupported; }

    Result<void> resetTargetsForSurfaceChange() {
        if (mTargetCache.empty()) return {};
        if (!mDeviceWaitIdle) return Error() << "Vulkan device idle wait is unavailable";

        const size_t targetCount = mTargetCache.size();
        VkResult result = mDeviceWaitIdle(mDevice);
        if (result != VK_SUCCESS) {
            return Error() << "Failed to drain display targets for surface change: " << result;
        }
        for (auto& slot : mInFlightSlots) reclaimSlotAfterDeviceIdle(slot);
        clearTargetCache();
        LOG(INFO) << "Android display cleared " << targetCount
                  << " cached targets for surface change";
        return {};
    }

    // acquireFenceFd is the release fence returned by ANativeWindow_dequeueBuffer. When
    // canImportAcquireFence() is true the caller passes it here for GPU-side waiting and blit()
    // takes ownership of it; otherwise the caller must already have waited and pass -1.
    Result<int> blit(int64_t importId, AHardwareBuffer* targetAhb, int acquireFenceFd = -1,
                     bool allowFaultInjection = false,
                     const NativeBltRegion* regions = nullptr, uint32_t regionCount = 0,
                     uint32_t targetWidth = 0, uint32_t targetHeight = 0) {
        ScopedTrace trace("crosvm_display.blit");
        ATRACE_INT64("crosvm_display.frame_id", static_cast<int64_t>(++mFrameSequence));
        // An early error must preserve the dequeued buffer's release dependency before flip()
        // cancels it with fence -1. Normal submission disables this guard after Vulkan consumes
        // the fd.
        auto acquireFenceGuard = android::base::make_scope_guard([&acquireFenceFd] {
            if (acquireFenceFd < 0) return;
            if (auto ret = pollFence(acquireFenceFd); !ret.ok()) {
                LOG(WARNING) << "Failed to drain display acquire fence on blit error: "
                             << ret.error();
            }
            acquireFenceFd = -1;
        });
        auto it = mImports.find(importId);
        if (it == mImports.end()) return Error() << "Unknown display import " << importId;
        ImportedImage& image = it->second;
        const bool preserveTarget = regions != nullptr;
        if (preserveTarget) {
            if (!android_display::nativeBltRegionsValid(image.width, image.height,
                    targetWidth, targetHeight, regions, regionCount))
                return Error() << "Invalid complete Native Blt region list";
        } else if (regionCount || targetWidth || targetHeight) {
            return Error() << "Native Blt regions missing";
        } else {
            targetWidth = image.width;
            targetHeight = image.height;
        }
        if (mInFlightSlots.empty()) return Error() << "No Vulkan display in-flight slots";

        if (allowFaultInjection &&
            injectRuntimeFlipFailure(RuntimeFlipFailureStage::kTargetImport)) {
            return Error() << "Injected display target import failure";
        }
        auto targetResult = acquireTargetImage(targetAhb, targetWidth, targetHeight);
        if (!targetResult.ok()) return targetResult.error();
        TargetImage& target = *targetResult.value();

        const size_t slotIndex = mNextInFlightSlot++ % mInFlightSlots.size();
        InFlightSlot& slot = mInFlightSlots[slotIndex];
        if (auto ret = reclaimSlot(slot, preserveTarget ? 1000000000ULL : UINT64_MAX); !ret.ok()) return ret.error();

        const bool injectSubmitFailure =
                allowFaultInjection && injectRuntimeFlipFailure(RuntimeFlipFailureStage::kSubmit);
        if (injectSubmitFailure && acquireFenceFd >= 0) {
            acquireFenceGuard.Disable();
            auto waitResult = pollFence(acquireFenceFd);
            acquireFenceFd = -1;
            if (!waitResult.ok()) return waitResult.error();
        }

        const bool recordGpuTimestamps = mTimestampQueryPool && ATRACE_ENABLED();
        const uint32_t timestampQueryBase = static_cast<uint32_t>(slotIndex * 2);
        VkCommandBuffer commandBuffer = slot.commandBuffer;
        {
            ScopedTrace recordTrace("crosvm_display.command_record");
            if (mResetCommandBuffer(commandBuffer, 0) != VK_SUCCESS) {
                evictTarget(target);
                return Error() << "Failed to reset display command buffer";
            }
            VkCommandBufferBeginInfo beginInfo = {
                    .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
                    .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
            };
            if (mBeginCommandBuffer(commandBuffer, &beginInfo) != VK_SUCCESS) {
                evictTarget(target);
                return Error() << "Failed to begin display command buffer";
            }
            if (recordGpuTimestamps) {
                mCmdResetQueryPool(commandBuffer, mTimestampQueryPool, timestampQueryBase, 2);
            }

            VkImageMemoryBarrier acquireBarriers[2] = {
                    {
                            .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                            .srcAccessMask = VK_ACCESS_MEMORY_WRITE_BIT,
                            .dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT,
                            .oldLayout = VK_IMAGE_LAYOUT_GENERAL,
                            .newLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
                            .srcQueueFamilyIndex = VK_QUEUE_FAMILY_FOREIGN_EXT,
                            .dstQueueFamilyIndex = mQueueFamilyIndex,
                            .image = image.sourceImage,
                            .subresourceRange = colorRange(),
                    },
                    {
                            .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                            .srcAccessMask = preserveTarget
                                    ? VK_ACCESS_MEMORY_READ_BIT | VK_ACCESS_MEMORY_WRITE_BIT : 0u,
                            .dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
                            .oldLayout = preserveTarget ? VK_IMAGE_LAYOUT_GENERAL : VK_IMAGE_LAYOUT_UNDEFINED,
                            .newLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                            .srcQueueFamilyIndex = VK_QUEUE_FAMILY_FOREIGN_EXT,
                            .dstQueueFamilyIndex = mQueueFamilyIndex,
                            .image = target.image,
                            .subresourceRange = colorRange(),
                    },
            };
            mCmdPipelineBarrier(commandBuffer, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT,
                                VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, nullptr, 0, nullptr, 2,
                                acquireBarriers);
            if (recordGpuTimestamps) {
                mCmdWriteTimestamp(commandBuffer, VK_PIPELINE_STAGE_TRANSFER_BIT,
                                   mTimestampQueryPool, timestampQueryBase);
            }

            const NativeBltRegion full{0, 0, 0, 0, image.width, image.height};
            const auto* copies = preserveTarget ? regions : &full;
            const uint32_t copyCount = preserveTarget ? regionCount : 1;
            for (uint32_t index = 0; index < copyCount; ++index) {
                const auto& r = copies[index];
                // Clipped rectangles may overlap. Separate commands still
                // require a dependency before a later write to the same texel.
                bool overlap = false;
                for (uint32_t prior = 0; prior < index; ++prior)
                    overlap |= android_display::nativeBltRegionsOverlap(copies[prior], r);
                if (overlap) {
                    VkMemoryBarrier barrier = {
                        .sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
                        .srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
                        .dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
                    };
                    mCmdPipelineBarrier(commandBuffer, VK_PIPELINE_STAGE_TRANSFER_BIT,
                            VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 1, &barrier, 0, nullptr, 0, nullptr);
                }
                const VkImageSubresourceLayers layers{VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1};
                if (image.format == target.format && mCmdCopyImage) {
                    const VkImageCopy copy = {
                        .srcSubresource = layers,
                        .srcOffset = {int32_t(r.source_x), int32_t(r.source_y), 0},
                        .dstSubresource = layers,
                        .dstOffset = {int32_t(r.destination_x), int32_t(r.destination_y), 0},
                        .extent = {r.width, r.height, 1},
                    };
                    mCmdCopyImage(commandBuffer, image.sourceImage,
                            VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, target.image,
                            VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, 1, &copy);
                    gDisplayPathCounters.imageCopyCommands.fetch_add(1, std::memory_order_relaxed);
                } else {
                    const VkImageBlit region = {
                        .srcSubresource = layers,
                        .srcOffsets = {{int32_t(r.source_x), int32_t(r.source_y), 0},
                                       {int32_t(r.source_x+r.width), int32_t(r.source_y+r.height), 1}},
                        .dstSubresource = layers,
                        .dstOffsets = {{int32_t(r.destination_x), int32_t(r.destination_y), 0},
                                       {int32_t(r.destination_x+r.width), int32_t(r.destination_y+r.height), 1}},
                    };
                    mCmdBlitImage(commandBuffer, image.sourceImage,
                            VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, target.image,
                            VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, 1, &region, VK_FILTER_NEAREST);
                    gDisplayPathCounters.imageBlitCommands.fetch_add(1, std::memory_order_relaxed);
                }
            }
            if (recordGpuTimestamps) {
                mCmdWriteTimestamp(commandBuffer, VK_PIPELINE_STAGE_TRANSFER_BIT,
                                   mTimestampQueryPool, timestampQueryBase + 1);
            }

            VkImageMemoryBarrier releaseBarriers[2] = {
                    {
                            .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                            .srcAccessMask = VK_ACCESS_TRANSFER_READ_BIT,
                            .dstAccessMask = VK_ACCESS_MEMORY_WRITE_BIT,
                            .oldLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
                            .newLayout = VK_IMAGE_LAYOUT_GENERAL,
                            .srcQueueFamilyIndex = mQueueFamilyIndex,
                            .dstQueueFamilyIndex = VK_QUEUE_FAMILY_FOREIGN_EXT,
                            .image = image.sourceImage,
                            .subresourceRange = colorRange(),
                    },
                    {
                            .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                            .srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
                            .dstAccessMask = VK_ACCESS_MEMORY_READ_BIT,
                            .oldLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                            .newLayout = VK_IMAGE_LAYOUT_GENERAL,
                            .srcQueueFamilyIndex = mQueueFamilyIndex,
                            .dstQueueFamilyIndex = VK_QUEUE_FAMILY_FOREIGN_EXT,
                            .image = target.image,
                            .subresourceRange = colorRange(),
                    },
            };
            mCmdPipelineBarrier(commandBuffer, VK_PIPELINE_STAGE_TRANSFER_BIT,
                                VK_PIPELINE_STAGE_ALL_COMMANDS_BIT, 0, 0, nullptr, 0, nullptr, 2,
                                releaseBarriers);

            if (mEndCommandBuffer(commandBuffer) != VK_SUCCESS) {
                evictTarget(target);
                return Error() << "Failed to end display command buffer";
            }
        }

        VkSemaphore signalSemaphore = VK_NULL_HANDLE;
        if (mAsyncBlitEnabled) {
            VkExportSemaphoreCreateInfo exportInfo = {
                    .sType = VK_STRUCTURE_TYPE_EXPORT_SEMAPHORE_CREATE_INFO,
                    .handleTypes = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT,
            };
            VkSemaphoreCreateInfo semaphoreInfo = {
                    .sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO,
                    .pNext = &exportInfo,
            };
            if (mCreateSemaphore(mDevice, &semaphoreInfo, nullptr, &slot.completionSemaphore) !=
                VK_SUCCESS) {
                evictTarget(target);
                return Error() << "Failed to create display completion semaphore";
            }
            signalSemaphore = slot.completionSemaphore;
            if (mResetFences(mDevice, 1, &slot.fence) != VK_SUCCESS) {
                evictTarget(target);
                mDestroySemaphore(mDevice, slot.completionSemaphore, nullptr);
                slot.completionSemaphore = VK_NULL_HANDLE;
                return Error() << "Failed to reset display in-flight fence";
            }
        }

        // Import the dequeued release fence as a GPU wait semaphore so the blit waits on target
        // availability without a CPU poll. Only attempted on the async path with SYNC_FD import
        // capability; otherwise the caller has already waited on the fence and passes -1.
        VkSemaphore waitSemaphore = VK_NULL_HANDLE;
        VkPipelineStageFlags waitStage = VK_PIPELINE_STAGE_TRANSFER_BIT;
        if (canImportAcquireFence() && acquireFenceFd >= 0) {
            VkSemaphoreCreateInfo waitSemaphoreInfo = {
                    .sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO,
            };
            if (mCreateSemaphore(mDevice, &waitSemaphoreInfo, nullptr, &slot.acquireSemaphore) !=
                VK_SUCCESS) {
                evictTarget(target);
                if (slot.completionSemaphore) {
                    mDestroySemaphore(mDevice, slot.completionSemaphore, nullptr);
                    slot.completionSemaphore = VK_NULL_HANDLE;
                }
                return Error() << "Failed to create display acquire semaphore";
            }
            VkImportSemaphoreFdInfoKHR importInfo = {
                    .sType = VK_STRUCTURE_TYPE_IMPORT_SEMAPHORE_FD_INFO_KHR,
                    .semaphore = slot.acquireSemaphore,
                    .flags = VK_SEMAPHORE_IMPORT_TEMPORARY_BIT,
                    .handleType = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT,
                    .fd = acquireFenceFd,
            };
            if (mImportSemaphoreFd(mDevice, &importInfo) != VK_SUCCESS) {
                // On success Vulkan owns the fd; on failure it stays ours. Fall back to a CPU wait
                // rather than dropping the acquire dependency, which would risk tearing.
                mDestroySemaphore(mDevice, slot.acquireSemaphore, nullptr);
                slot.acquireSemaphore = VK_NULL_HANDLE;
                acquireFenceGuard.Disable(); // pollFence consumes (closes) the fd
                if (auto ret = pollFence(acquireFenceFd); !ret.ok()) {
                    evictTarget(target);
                    if (slot.completionSemaphore) {
                        mDestroySemaphore(mDevice, slot.completionSemaphore, nullptr);
                        slot.completionSemaphore = VK_NULL_HANDLE;
                    }
                    return ret.error();
                }
            } else {
                waitSemaphore = slot.acquireSemaphore;
                acquireFenceGuard.Disable(); // vkImportSemaphoreFdKHR consumed the fd
            }
        }

        VkSubmitInfo submitInfo = {
                .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
                .waitSemaphoreCount = waitSemaphore == VK_NULL_HANDLE ? 0u : 1u,
                .pWaitSemaphores = waitSemaphore == VK_NULL_HANDLE ? nullptr : &waitSemaphore,
                .pWaitDstStageMask = waitSemaphore == VK_NULL_HANDLE ? nullptr : &waitStage,
                .commandBufferCount = 1,
                .pCommandBuffers = &commandBuffer,
                .signalSemaphoreCount = signalSemaphore == VK_NULL_HANDLE ? 0u : 1u,
                .pSignalSemaphores = signalSemaphore == VK_NULL_HANDLE ? nullptr : &signalSemaphore,
        };
        VkResult result;
        // The native copy context must retain every dependency if submission
        // has an uncertain outcome. A later close proves completion or loss.
        if (preserveTarget && mAsyncBlitEnabled) slot.inFlight = true;
        {
            ScopedTrace submitTrace("crosvm_display.queue_submit");
            result = injectSubmitFailure
                    ? VK_ERROR_UNKNOWN
                    : mQueueSubmit(mQueue, 1, &submitInfo,
                                   mAsyncBlitEnabled ? slot.fence : VK_NULL_HANDLE);
        }
        if (result != VK_SUCCESS) {
            if (preserveTarget && mAsyncBlitEnabled) {
                if (result != VK_ERROR_OUT_OF_HOST_MEMORY && result != VK_ERROR_OUT_OF_DEVICE_MEMORY)
                    return Error() << "Native Blt submit completion uncertain: " << result;
                slot.inFlight = false; // Vulkan defines these OOM submissions as having no effect.
            }
            evictTarget(target);
            if (slot.completionSemaphore) {
                mDestroySemaphore(mDevice, slot.completionSemaphore, nullptr);
                slot.completionSemaphore = VK_NULL_HANDLE;
            }
            if (slot.acquireSemaphore) {
                mDestroySemaphore(mDevice, slot.acquireSemaphore, nullptr);
                slot.acquireSemaphore = VK_NULL_HANDLE;
            }
            return Error() << "Failed to submit display blit: " << result;
        }

        if (!mAsyncBlitEnabled) {
            ScopedTrace waitTrace("crosvm_display.queue_wait_idle");
            result = mQueueWaitIdle(mQueue);
            if (result == VK_SUCCESS && recordGpuTimestamps) {
                collectGpuBlitDuration(timestampQueryBase);
            }
            // Target stays cached for reuse; only the blit completion is awaited here.
            if (result != VK_SUCCESS) return Error() << "Failed to wait for display queue";
            return -1;
        }

        slot.inFlight = true;
        slot.timestampPending = recordGpuTimestamps;
        VkSemaphoreGetFdInfoKHR fdInfo = {
                .sType = VK_STRUCTURE_TYPE_SEMAPHORE_GET_FD_INFO_KHR,
                .semaphore = slot.completionSemaphore,
                .handleType = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT,
        };
        int completionFd = -1;
        result = mGetSemaphoreFd(mDevice, &fdInfo, &completionFd);
        if (result != VK_SUCCESS || completionFd < 0) {
            LOG(WARNING) << "Failed to export display completion sync_fd; draining slot: "
                         << result;
            if (auto ret = reclaimSlot(slot, preserveTarget ? 1000000000ULL : UINT64_MAX); !ret.ok()) return ret.error();
            return -1;
        }
        return completionFd;
    }

private:
    void destroyNativeStaging() {
        if (mNativeStagingAddress) {
            deviceProc<PFN_vkUnmapMemory>("vkUnmapMemory")(mDevice,mNativeStagingMemory);
            mNativeStagingAddress=nullptr;
        }
        if (mNativeStagingBuffer) {
            deviceProc<PFN_vkDestroyBuffer>("vkDestroyBuffer")(mDevice,mNativeStagingBuffer,nullptr);
            mNativeStagingBuffer=VK_NULL_HANDLE;
        }
        if (mNativeStagingMemory) {
            mFreeMemory(mDevice,mNativeStagingMemory,nullptr);
            mNativeStagingMemory=VK_NULL_HANDLE;
        }
    }
    struct ImportedImage {
        VkImage sourceImage = VK_NULL_HANDLE;
        VkDeviceMemory sourceMemory = VK_NULL_HANDLE;
        VkFormat format = VK_FORMAT_UNDEFINED;
        uint32_t width = 0;
        uint32_t height = 0;
    };

    struct TargetImage {
        VkImage image = VK_NULL_HANDLE;
        VkDeviceMemory memory = VK_NULL_HANDLE;
        AHardwareBuffer* ahb = nullptr;
        VkFormat format = VK_FORMAT_UNDEFINED;
        uint32_t width = 0;
        uint32_t height = 0;
    };

    struct InFlightSlot {
        VkCommandBuffer commandBuffer = VK_NULL_HANDLE;
        VkFence fence = VK_NULL_HANDLE;
        VkSemaphore completionSemaphore = VK_NULL_HANDLE;
        VkSemaphore acquireSemaphore = VK_NULL_HANDLE;
        bool inFlight = false;
        bool timestampPending = false;
    };

    template <typename T>
    T instanceProc(const char* name) {
        return reinterpret_cast<T>(mHal->get_instance_proc_addr(mInstance, name));
    }

    template <typename T>
    T deviceProc(const char* name) {
        return reinterpret_cast<T>(mGetDeviceProcAddr(mDevice, name));
    }

    static VkImageSubresourceRange colorRange() {
        return {
                .aspectMask = VK_IMAGE_ASPECT_COLOR_BIT,
                .baseMipLevel = 0,
                .levelCount = 1,
                .baseArrayLayer = 0,
                .layerCount = 1,
        };
    }

    static constexpr uint32_t fourcc(char a, char b, char c, char d) {
        return static_cast<uint32_t>(a) | (static_cast<uint32_t>(b) << 8) |
                (static_cast<uint32_t>(c) << 16) | (static_cast<uint32_t>(d) << 24);
    }

    static VkFormat vkFormatFromDrmFourcc(uint32_t format) {
        switch (format) {
            case fourcc('X', 'R', '2', '4'):
            case fourcc('A', 'R', '2', '4'):
                return VK_FORMAT_B8G8R8A8_UNORM;
            case fourcc('X', 'B', '2', '4'):
            case fourcc('A', 'B', '2', '4'):
                return VK_FORMAT_R8G8B8A8_UNORM;
            default:
                return VK_FORMAT_UNDEFINED;
        }
    }

    static uint32_t firstMemoryType(uint32_t bits) {
        return bits ? static_cast<uint32_t>(__builtin_ctz(bits)) : UINT32_MAX;
    }

    static constexpr uint64_t kDrmFormatModLinear = 0;
    static constexpr uint64_t kDrmFormatModInvalid = 0x00ffffffffffffffULL;

    void initializeGpuTimestamps() {
        if (!mTimestampValidBits || !mGetPhysicalDeviceProperties || !mCreateQueryPool ||
            !mDestroyQueryPool || !mCmdResetQueryPool || !mCmdWriteTimestamp ||
            !mGetQueryPoolResults) {
            LOG(INFO) << "Android display GPU timestamps are unavailable";
            return;
        }

        VkPhysicalDeviceProperties properties{};
        mGetPhysicalDeviceProperties(mPhysicalDevice, &properties);
        mTimestampPeriodNs = properties.limits.timestampPeriod;
        if (mTimestampPeriodNs <= 0.0f) return;

        VkQueryPoolCreateInfo queryInfo = {
                .sType = VK_STRUCTURE_TYPE_QUERY_POOL_CREATE_INFO,
                .queryType = VK_QUERY_TYPE_TIMESTAMP,
                .queryCount = static_cast<uint32_t>(mInFlightSlots.size() * 2),
        };
        if (mCreateQueryPool(mDevice, &queryInfo, nullptr, &mTimestampQueryPool) != VK_SUCCESS) {
            mTimestampQueryPool = VK_NULL_HANDLE;
            LOG(WARNING) << "Failed to create Android display GPU timestamp query pool";
            return;
        }
        LOG(INFO) << "Android display GPU timestamps enabled: period=" << mTimestampPeriodNs
                  << " ns, validBits=" << mTimestampValidBits;
    }

    void collectGpuBlitDuration(uint32_t queryBase) {
        if (!mTimestampQueryPool) return;
        uint64_t timestamps[2]{};
        VkResult result =
                mGetQueryPoolResults(mDevice, mTimestampQueryPool, queryBase, 2, sizeof(timestamps),
                                     timestamps, sizeof(uint64_t), VK_QUERY_RESULT_64_BIT);
        if (result != VK_SUCCESS) {
            if (!mTimestampReadFailureLogged) {
                mTimestampReadFailureLogged = true;
                LOG(WARNING) << "Failed to read Android display GPU timestamps: " << result;
            }
            return;
        }

        uint64_t elapsedTicks = timestamps[1] - timestamps[0];
        if (mTimestampValidBits < 64) {
            const uint64_t mask = (uint64_t{1} << mTimestampValidBits) - 1;
            elapsedTicks &= mask;
        }
        const uint64_t durationNs =
                static_cast<uint64_t>(static_cast<double>(elapsedTicks) * mTimestampPeriodNs);
        ATRACE_INT64("crosvm_display.gpu_blit_ns", static_cast<int64_t>(durationNs));
    }

    Result<void> reclaimSlot(InFlightSlot& slot, uint64_t timeoutNs = UINT64_MAX) {
        if (!slot.inFlight) return {};
        VkResult result;
        {
            ScopedTrace waitTrace("crosvm_display.in_flight_wait");
            result = mWaitForFences(mDevice, 1, &slot.fence, VK_TRUE, timeoutNs);
        }
        if (result != VK_SUCCESS) {
            return Error() << "Failed to wait for display in-flight slot: " << result;
        }
        slot.inFlight = false;
        if (slot.timestampPending) {
            const size_t slotIndex = static_cast<size_t>(&slot - mInFlightSlots.data());
            collectGpuBlitDuration(static_cast<uint32_t>(slotIndex * 2));
            slot.timestampPending = false;
        }
        // Target images live in mTargetCache and outlive individual slots; do not destroy here.
        if (slot.completionSemaphore) {
            mDestroySemaphore(mDevice, slot.completionSemaphore, nullptr);
            slot.completionSemaphore = VK_NULL_HANDLE;
        }
        if (slot.acquireSemaphore) {
            mDestroySemaphore(mDevice, slot.acquireSemaphore, nullptr);
            slot.acquireSemaphore = VK_NULL_HANDLE;
        }
        return {};
    }

    void reclaimSlotAfterDeviceIdle(InFlightSlot& slot) {
        if (!slot.inFlight) return;
        slot.inFlight = false;
        if (slot.timestampPending) {
            const size_t slotIndex = static_cast<size_t>(&slot - mInFlightSlots.data());
            collectGpuBlitDuration(static_cast<uint32_t>(slotIndex * 2));
            slot.timestampPending = false;
        }
        // Target images live in mTargetCache and outlive individual slots; do not destroy here.
        if (slot.completionSemaphore) {
            mDestroySemaphore(mDevice, slot.completionSemaphore, nullptr);
            slot.completionSemaphore = VK_NULL_HANDLE;
        }
        if (slot.acquireSemaphore) {
            mDestroySemaphore(mDevice, slot.acquireSemaphore, nullptr);
            slot.acquireSemaphore = VK_NULL_HANDLE;
        }
    }

    bool modifierSupportsBlitSource(VkFormat format, uint64_t modifier) const {
        VkDrmFormatModifierPropertiesListEXT modifierList = {
                .sType = VK_STRUCTURE_TYPE_DRM_FORMAT_MODIFIER_PROPERTIES_LIST_EXT,
        };
        VkFormatProperties2 properties = {
                .sType = VK_STRUCTURE_TYPE_FORMAT_PROPERTIES_2,
                .pNext = &modifierList,
        };
        mGetPhysicalDeviceFormatProperties2(mPhysicalDevice, format, &properties);
        if (modifierList.drmFormatModifierCount == 0) return false;

        std::vector<VkDrmFormatModifierPropertiesEXT> modifiers(
                modifierList.drmFormatModifierCount);
        modifierList.pDrmFormatModifierProperties = modifiers.data();
        mGetPhysicalDeviceFormatProperties2(mPhysicalDevice, format, &properties);
        constexpr VkFormatFeatureFlags required =
                VK_FORMAT_FEATURE_TRANSFER_SRC_BIT | VK_FORMAT_FEATURE_BLIT_SRC_BIT;
        for (const auto& candidate : modifiers) {
            if (candidate.drmFormatModifier == modifier &&
                candidate.drmFormatModifierPlaneCount == 1 &&
                (candidate.drmFormatModifierTilingFeatures & required) == required) {
                return true;
            }
        }
        return false;
    }

    bool externalImageImportSupported(VkFormat format, VkImageTiling tiling,
                                      VkImageUsageFlags usage,
                                      VkExternalMemoryHandleTypeFlagBits handleType,
                                      const void* externalInfoNext = nullptr) const {
        VkPhysicalDeviceExternalImageFormatInfo externalInfo = {
                .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_IMAGE_FORMAT_INFO,
                .pNext = externalInfoNext,
                .handleType = handleType,
        };
        VkPhysicalDeviceImageFormatInfo2 imageInfo = {
                .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2,
                .pNext = &externalInfo,
                .format = format,
                .type = VK_IMAGE_TYPE_2D,
                .tiling = tiling,
                .usage = usage,
        };
        VkExternalImageFormatProperties externalProperties = {
                .sType = VK_STRUCTURE_TYPE_EXTERNAL_IMAGE_FORMAT_PROPERTIES,
        };
        VkImageFormatProperties2 imageProperties = {
                .sType = VK_STRUCTURE_TYPE_IMAGE_FORMAT_PROPERTIES_2,
                .pNext = &externalProperties,
        };
        if (mGetPhysicalDeviceImageFormatProperties2(mPhysicalDevice, &imageInfo,
                                                     &imageProperties) != VK_SUCCESS) {
            return false;
        }
        const auto& memoryProperties = externalProperties.externalMemoryProperties;
        return (memoryProperties.externalMemoryFeatures &
                VK_EXTERNAL_MEMORY_FEATURE_IMPORTABLE_BIT) != 0 &&
                (memoryProperties.compatibleHandleTypes & handleType) != 0;
    }

    bool asyncSemaphoreSupported(
            PFN_vkEnumerateDeviceExtensionProperties enumerateDeviceExtensionProperties) {
        if (!enumerateDeviceExtensionProperties || !mGetPhysicalDeviceExternalSemaphoreProperties) {
            return false;
        }
        uint32_t extensionCount = 0;
        if (enumerateDeviceExtensionProperties(mPhysicalDevice, nullptr, &extensionCount,
                                               nullptr) != VK_SUCCESS) {
            return false;
        }
        std::vector<VkExtensionProperties> extensions(extensionCount);
        if (enumerateDeviceExtensionProperties(mPhysicalDevice, nullptr, &extensionCount,
                                               extensions.data()) != VK_SUCCESS) {
            return false;
        }
        const auto hasExtension = [&extensions](const char* name) {
            return std::any_of(extensions.begin(), extensions.end(), [name](const auto& extension) {
                return std::strcmp(extension.extensionName, name) == 0;
            });
        };
        if (!hasExtension(VK_KHR_EXTERNAL_SEMAPHORE_EXTENSION_NAME) ||
            !hasExtension(VK_KHR_EXTERNAL_SEMAPHORE_FD_EXTENSION_NAME)) {
            return false;
        }

        VkPhysicalDeviceExternalSemaphoreInfo semaphoreInfo = {
                .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_SEMAPHORE_INFO,
                .handleType = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT,
        };
        VkExternalSemaphoreProperties semaphoreProperties = {
                .sType = VK_STRUCTURE_TYPE_EXTERNAL_SEMAPHORE_PROPERTIES,
        };
        mGetPhysicalDeviceExternalSemaphoreProperties(mPhysicalDevice, &semaphoreInfo,
                                                      &semaphoreProperties);
        // EXPORTABLE is required for the completion semaphore. IMPORTABLE is a separate capability
        // needed only for acquire-fence import; record it so the caller can gate that path.
        mSyncFdImportable = (semaphoreProperties.externalSemaphoreFeatures &
                             VK_EXTERNAL_SEMAPHORE_FEATURE_IMPORTABLE_BIT) != 0;
        return (semaphoreProperties.externalSemaphoreFeatures &
                VK_EXTERNAL_SEMAPHORE_FEATURE_EXPORTABLE_BIT) != 0;
    }

    bool initialize() {
        // The blit driver is any hwvulkan HAL that exposes the required extensions -- turnip, the
        // SoC's vendor driver, PanVK, ... -- so this is not turnip-specific. The chooser (the app's
        // GpuBlitProvider) names one via CROSVM_DISPLAY_VULKAN_LIBRARY; CROSVM_TURNIP_LIBRARY is the
        // former name, still honoured. There is deliberately NO hardcoded fallback: dlopen'ing a
        // fixed world-writable path (e.g. /data/local/tmp) would silently run untrusted native code,
        // so the driver must be named explicitly by whoever launched crosvm. With none named we do
        // not load anything and the caller drops to the CPU copy.
        const char* configuredPath = std::getenv("CROSVM_DISPLAY_VULKAN_LIBRARY");
        if (!configuredPath || !*configuredPath)
            configuredPath = std::getenv("CROSVM_TURNIP_LIBRARY");
        if (!configuredPath || !*configuredPath) {
            LOG(INFO) << "no display Vulkan blit driver configured; using CPU copy";
            return false;
        }
        mLibrary = dlopen(configuredPath, RTLD_NOW | RTLD_LOCAL);
        if (!mLibrary) {
            LOG(ERROR) << "failed to load display Vulkan blit driver from " << configuredPath << ": "
                       << dlerror();
            return false;
        }
        LOG(INFO) << "Android display loading Vulkan blit driver from " << configuredPath;

        auto* module = static_cast<HwModule*>(dlsym(mLibrary, "HMI"));
        if (!module || !module->methods || !module->methods->open ||
            module->methods->open(module, "vk0", reinterpret_cast<void**>(&mHal)) != 0 || !mHal ||
            !mHal->create_instance || !mHal->get_instance_proc_addr) {
            LOG(ERROR) << "invalid display Vulkan hwvulkan HMI";
            return false;
        }

        VkApplicationInfo appInfo = {
                .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
                .pApplicationName = "crosvm_android_display",
                .applicationVersion = 1,
                .pEngineName = "crosvm",
                .engineVersion = 1,
                .apiVersion = VK_API_VERSION_1_1,
        };
        VkInstanceCreateInfo instanceInfo = {
                .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                .pApplicationInfo = &appInfo,
        };
        if (mHal->create_instance(&instanceInfo, nullptr, &mInstance) != VK_SUCCESS) return false;

        auto enumeratePhysicalDevices =
                instanceProc<PFN_vkEnumeratePhysicalDevices>("vkEnumeratePhysicalDevices");
        auto enumerateDeviceExtensionProperties =
                instanceProc<PFN_vkEnumerateDeviceExtensionProperties>(
                        "vkEnumerateDeviceExtensionProperties");
        auto getQueueFamilyProperties = instanceProc<PFN_vkGetPhysicalDeviceQueueFamilyProperties>(
                "vkGetPhysicalDeviceQueueFamilyProperties");
        auto createDevice = instanceProc<PFN_vkCreateDevice>("vkCreateDevice");
        mGetPhysicalDeviceProperties =
                instanceProc<PFN_vkGetPhysicalDeviceProperties>("vkGetPhysicalDeviceProperties");
        mGetPhysicalDeviceFormatProperties2 =
                instanceProc<PFN_vkGetPhysicalDeviceFormatProperties2>(
                        "vkGetPhysicalDeviceFormatProperties2");
        mGetPhysicalDeviceImageFormatProperties2 =
                instanceProc<PFN_vkGetPhysicalDeviceImageFormatProperties2>(
                        "vkGetPhysicalDeviceImageFormatProperties2");
        mGetPhysicalDeviceExternalSemaphoreProperties =
                instanceProc<PFN_vkGetPhysicalDeviceExternalSemaphoreProperties>(
                        "vkGetPhysicalDeviceExternalSemaphoreProperties");
        mGetDeviceProcAddr = instanceProc<PFN_vkGetDeviceProcAddr>("vkGetDeviceProcAddr");
        mDestroyInstance = instanceProc<PFN_vkDestroyInstance>("vkDestroyInstance");
        if (!enumeratePhysicalDevices || !enumerateDeviceExtensionProperties ||
            !getQueueFamilyProperties || !createDevice || !mGetPhysicalDeviceFormatProperties2 ||
            !mGetPhysicalDeviceImageFormatProperties2 || !mGetDeviceProcAddr || !mDestroyInstance) {
            return false;
        }

        uint32_t physicalDeviceCount = 0;
        if (enumeratePhysicalDevices(mInstance, &physicalDeviceCount, nullptr) != VK_SUCCESS ||
            !physicalDeviceCount) {
            return false;
        }
        std::vector<VkPhysicalDevice> physicalDevices(physicalDeviceCount);
        if (enumeratePhysicalDevices(mInstance, &physicalDeviceCount, physicalDevices.data()) !=
            VK_SUCCESS) {
            return false;
        }
        mPhysicalDevice = physicalDevices[0];

        if (mAsyncBlitEnabled && !asyncSemaphoreSupported(enumerateDeviceExtensionProperties)) {
            LOG(WARNING) << "Android display async blit SYNC_FD capability is unavailable; "
                            "keeping synchronous queue waits";
            mAsyncBlitEnabled = false;
        }

        uint32_t queueFamilyCount = 0;
        getQueueFamilyProperties(mPhysicalDevice, &queueFamilyCount, nullptr);
        if (!queueFamilyCount) return false;
        std::vector<VkQueueFamilyProperties> queueFamilies(queueFamilyCount);
        getQueueFamilyProperties(mPhysicalDevice, &queueFamilyCount, queueFamilies.data());
        for (uint32_t i = 0; i < queueFamilyCount; ++i) {
            if (queueFamilies[i].queueFlags & (VK_QUEUE_GRAPHICS_BIT | VK_QUEUE_TRANSFER_BIT)) {
                mQueueFamilyIndex = i;
                mTimestampValidBits = queueFamilies[i].timestampValidBits;
                break;
            }
        }

        float priority = 1.0f;
        VkDeviceQueueCreateInfo queueInfo = {
                .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                .queueFamilyIndex = mQueueFamilyIndex,
                .queueCount = 1,
                .pQueuePriorities = &priority,
        };
        std::vector<const char*> extensions = {
                VK_EXT_EXTERNAL_MEMORY_DMA_BUF_EXTENSION_NAME,
                VK_EXT_IMAGE_DRM_FORMAT_MODIFIER_EXTENSION_NAME,
                VK_ANDROID_EXTERNAL_MEMORY_ANDROID_HARDWARE_BUFFER_EXTENSION_NAME,
                VK_KHR_EXTERNAL_MEMORY_FD_EXTENSION_NAME,
                VK_EXT_QUEUE_FAMILY_FOREIGN_EXTENSION_NAME,
        };
        if (mAsyncBlitEnabled) {
            extensions.push_back(VK_KHR_EXTERNAL_SEMAPHORE_EXTENSION_NAME);
            extensions.push_back(VK_KHR_EXTERNAL_SEMAPHORE_FD_EXTENSION_NAME);
        }
        VkDeviceCreateInfo deviceInfo = {
                .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                .queueCreateInfoCount = 1,
                .pQueueCreateInfos = &queueInfo,
                .enabledExtensionCount = static_cast<uint32_t>(extensions.size()),
                .ppEnabledExtensionNames = extensions.data(),
        };
        if (createDevice(mPhysicalDevice, &deviceInfo, nullptr, &mDevice) != VK_SUCCESS)
            return false;

#define LOAD_DEVICE_PROC(member, name) member = deviceProc<decltype(member)>(name)
        LOAD_DEVICE_PROC(mDestroyDevice, "vkDestroyDevice");
        LOAD_DEVICE_PROC(mDeviceWaitIdle, "vkDeviceWaitIdle");
        LOAD_DEVICE_PROC(mGetDeviceQueue, "vkGetDeviceQueue");
        LOAD_DEVICE_PROC(mCreateCommandPool, "vkCreateCommandPool");
        LOAD_DEVICE_PROC(mDestroyCommandPool, "vkDestroyCommandPool");
        LOAD_DEVICE_PROC(mAllocateCommandBuffers, "vkAllocateCommandBuffers");
        LOAD_DEVICE_PROC(mResetCommandBuffer, "vkResetCommandBuffer");
        LOAD_DEVICE_PROC(mBeginCommandBuffer, "vkBeginCommandBuffer");
        LOAD_DEVICE_PROC(mEndCommandBuffer, "vkEndCommandBuffer");
        LOAD_DEVICE_PROC(mQueueSubmit, "vkQueueSubmit");
        LOAD_DEVICE_PROC(mQueueWaitIdle, "vkQueueWaitIdle");
        LOAD_DEVICE_PROC(mCreateFence, "vkCreateFence");
        LOAD_DEVICE_PROC(mDestroyFence, "vkDestroyFence");
        LOAD_DEVICE_PROC(mWaitForFences, "vkWaitForFences");
        LOAD_DEVICE_PROC(mResetFences, "vkResetFences");
        LOAD_DEVICE_PROC(mCreateSemaphore, "vkCreateSemaphore");
        LOAD_DEVICE_PROC(mDestroySemaphore, "vkDestroySemaphore");
        LOAD_DEVICE_PROC(mGetSemaphoreFd, "vkGetSemaphoreFdKHR");
        LOAD_DEVICE_PROC(mImportSemaphoreFd, "vkImportSemaphoreFdKHR");
        LOAD_DEVICE_PROC(mCmdPipelineBarrier, "vkCmdPipelineBarrier");
        LOAD_DEVICE_PROC(mCmdCopyImage, "vkCmdCopyImage");
        LOAD_DEVICE_PROC(mCmdBlitImage, "vkCmdBlitImage");
        LOAD_DEVICE_PROC(mCreateQueryPool, "vkCreateQueryPool");
        LOAD_DEVICE_PROC(mDestroyQueryPool, "vkDestroyQueryPool");
        LOAD_DEVICE_PROC(mCmdResetQueryPool, "vkCmdResetQueryPool");
        LOAD_DEVICE_PROC(mCmdWriteTimestamp, "vkCmdWriteTimestamp");
        LOAD_DEVICE_PROC(mGetQueryPoolResults, "vkGetQueryPoolResults");
        LOAD_DEVICE_PROC(mCreateImage, "vkCreateImage");
        LOAD_DEVICE_PROC(mDestroyImage, "vkDestroyImage");
        LOAD_DEVICE_PROC(mGetImageMemoryRequirements2, "vkGetImageMemoryRequirements2");
        LOAD_DEVICE_PROC(mAllocateMemory, "vkAllocateMemory");
        LOAD_DEVICE_PROC(mFreeMemory, "vkFreeMemory");
        LOAD_DEVICE_PROC(mBindImageMemory, "vkBindImageMemory");
        LOAD_DEVICE_PROC(mGetMemoryFdProperties, "vkGetMemoryFdPropertiesKHR");
        LOAD_DEVICE_PROC(mGetAhbProperties, "vkGetAndroidHardwareBufferPropertiesANDROID");
#undef LOAD_DEVICE_PROC

        if (!mDestroyDevice || !mDeviceWaitIdle || !mGetDeviceQueue || !mCreateCommandPool ||
            !mDestroyCommandPool || !mAllocateCommandBuffers || !mResetCommandBuffer ||
            !mBeginCommandBuffer || !mEndCommandBuffer || !mQueueSubmit || !mQueueWaitIdle ||
            !mCmdPipelineBarrier || !mCmdCopyImage || !mCmdBlitImage || !mCreateImage ||
            !mDestroyImage ||
            !mGetImageMemoryRequirements2 || !mAllocateMemory || !mFreeMemory ||
            !mBindImageMemory || !mGetMemoryFdProperties || !mGetAhbProperties) {
            return false;
        }
        if (mAsyncBlitEnabled &&
            (!mCreateFence || !mDestroyFence || !mWaitForFences || !mResetFences ||
             !mCreateSemaphore || !mDestroySemaphore || !mGetSemaphoreFd)) {
            LOG(WARNING) << "Android display async blit Vulkan entry points are unavailable; "
                            "keeping synchronous queue waits";
            mAsyncBlitEnabled = false;
        }
        // Acquire-fence import is an independent capability on top of async blit: it needs the
        // import entry point and SYNC_FD IMPORTABLE support. Without it the async path still runs
        // but the dequeued release fence is waited on the CPU instead of the GPU.
        mAsyncAcquireImportSupported =
                mAsyncBlitEnabled && mImportSemaphoreFd != nullptr && mSyncFdImportable;
        if (mAsyncBlitEnabled) {
            LOG(INFO) << "Android display acquire-fence semaphore import "
                      << (mAsyncAcquireImportSupported ? "enabled (GPU wait)"
                                                       : "unavailable (CPU poll fallback)");
        }

        mGetDeviceQueue(mDevice, mQueueFamilyIndex, 0, &mQueue);
        VkCommandPoolCreateInfo poolInfo = {
                .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
                .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
                .queueFamilyIndex = mQueueFamilyIndex,
        };
        if (mCreateCommandPool(mDevice, &poolInfo, nullptr, &mCommandPool) != VK_SUCCESS) {
            return false;
        }
        VkCommandBufferAllocateInfo commandInfo = {
                .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
                .commandPool = mCommandPool,
                .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
                .commandBufferCount = mAsyncBlitEnabled ? kAsyncInFlightSlotCount : 1,
        };
        mInFlightSlots.resize(commandInfo.commandBufferCount);
        std::vector<VkCommandBuffer> commandBuffers(commandInfo.commandBufferCount);
        if (mAllocateCommandBuffers(mDevice, &commandInfo, commandBuffers.data()) != VK_SUCCESS) {
            return false;
        }
        for (size_t i = 0; i < mInFlightSlots.size(); ++i) {
            mInFlightSlots[i].commandBuffer = commandBuffers[i];
            if (!mAsyncBlitEnabled) continue;
            VkFenceCreateInfo fenceInfo = {
                    .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO,
                    .flags = VK_FENCE_CREATE_SIGNALED_BIT,
            };
            if (mCreateFence(mDevice, &fenceInfo, nullptr, &mInFlightSlots[i].fence) !=
                VK_SUCCESS) {
                return false;
            }
        }
        LOG(INFO) << "Android display async blit " << (mAsyncBlitEnabled ? "enabled" : "disabled")
                  << ", in-flight slots=" << mInFlightSlots.size();
        initializeGpuTimestamps();
        return true;
    }

    bool createSourceImage(int fd, uint32_t offset, uint32_t stride, uint64_t modifier,
                           VkFormat format, ImportedImage& imported) {
        if (!modifierSupportsBlitSource(format, modifier)) {
            LOG(ERROR) << "LINEAR display source does not support transfer/blit source usage";
            return false;
        }
        VkPhysicalDeviceImageDrmFormatModifierInfoEXT modifierQuery = {
                .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_DRM_FORMAT_MODIFIER_INFO_EXT,
                .drmFormatModifier = modifier,
                .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
        };
        if (!externalImageImportSupported(format, VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT,
                                          VK_IMAGE_USAGE_TRANSFER_SRC_BIT,
                                          VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
                                          &modifierQuery)) {
            LOG(ERROR) << "LINEAR DMA-BUF image import is unsupported for source format " << format;
            return false;
        }

        struct stat fdStat{};
        if (fstat(fd, &fdStat) != 0 || fdStat.st_size <= 0) {
            PLOG(ERROR) << "failed to determine display DMA-BUF size";
            return false;
        }
        const VkDeviceSize allocationSize = static_cast<VkDeviceSize>(fdStat.st_size);
        const VkDeviceSize imageSize = static_cast<VkDeviceSize>(stride) * imported.height;
        const VkDeviceSize imageEnd = static_cast<VkDeviceSize>(offset) + imageSize;
        if (imageSize / stride != imported.height || imageEnd < imageSize ||
            imageEnd > allocationSize) {
            LOG(ERROR) << "display DMA-BUF layout exceeds allocation: offset=" << offset
                       << " stride=" << stride << " height=" << imported.height
                       << " allocation=" << allocationSize;
            return false;
        }

        VkSubresourceLayout planeLayout = {
                .offset = offset,
                .size = 0,
                .rowPitch = stride,
                .arrayPitch = 0,
                .depthPitch = 0,
        };
        VkExternalMemoryImageCreateInfo externalInfo = {
                .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
                .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
        };
        VkImageDrmFormatModifierExplicitCreateInfoEXT modifierInfo = {
                .sType = VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT,
                .pNext = &externalInfo,
                .drmFormatModifier = modifier,
                .drmFormatModifierPlaneCount = 1,
                .pPlaneLayouts = &planeLayout,
        };
        // Adreno's 2D blitter reads the LINEAR source through the texture pipe, which fetches whole
        // rows at the image's PITCH. When the guest stride is padded (a width that is not 64px-
        // aligned -- e.g. 1400 -> stride 5632 = 1408px, 8px of row padding) turnip's a7xx A2D
        // texture fetch on a padded-pitch LINEAR source wedges the GPU, and because that blit shares
        // the Adreno with gfxstream's render thread the whole VM hangs. Describe the source image at
        // its natural unpadded width (stride/bpp) so the texture's WIDTH equals its PITCH/bpp; the
        // blit below still samples only the real [0,width) sub-rectangle, so this is a no-op for
        // unpadded widths (stride == width*bpp) and only widens the declared image for padded ones.
        const uint32_t kBytesPerPixel = 4; // every fourcc vkFormatFromDrmFourcc accepts is 32-bpp
        uint32_t sourceImageWidth = imported.width;
        if ((stride % kBytesPerPixel) == 0 && stride / kBytesPerPixel > sourceImageWidth) {
            sourceImageWidth = stride / kBytesPerPixel;
        }
        VkImageCreateInfo imageInfo = {
                .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
                .pNext = &modifierInfo,
                .imageType = VK_IMAGE_TYPE_2D,
                .format = format,
                .extent = {sourceImageWidth, imported.height, 1},
                .mipLevels = 1,
                .arrayLayers = 1,
                .samples = VK_SAMPLE_COUNT_1_BIT,
                .tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT,
                .usage = VK_IMAGE_USAGE_TRANSFER_SRC_BIT,
                .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
                .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
        };
        if (mCreateImage(mDevice, &imageInfo, nullptr, &imported.sourceImage) != VK_SUCCESS) {
            LOG(ERROR) << "failed to create explicit LINEAR display source image";
            return false;
        }

        VkMemoryDedicatedRequirements dedicatedRequirements = {
                .sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_REQUIREMENTS,
        };
        VkMemoryRequirements2 requirements = {
                .sType = VK_STRUCTURE_TYPE_MEMORY_REQUIREMENTS_2,
                .pNext = &dedicatedRequirements,
        };
        VkImageMemoryRequirementsInfo2 requirementsInfo = {
                .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_REQUIREMENTS_INFO_2,
                .image = imported.sourceImage,
        };
        mGetImageMemoryRequirements2(mDevice, &requirementsInfo, &requirements);
        if (requirements.memoryRequirements.size > allocationSize) {
            LOG(ERROR) << "display source image requirements exceed DMA-BUF allocation: required="
                       << requirements.memoryRequirements.size << " allocation=" << allocationSize;
            return false;
        }

        int importFd = dup(fd);
        if (importFd < 0) return false;
        VkMemoryFdPropertiesKHR fdProperties = {
                .sType = VK_STRUCTURE_TYPE_MEMORY_FD_PROPERTIES_KHR,
        };
        VkResult result =
                mGetMemoryFdProperties(mDevice, VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
                                       importFd, &fdProperties);
        uint32_t memoryType = firstMemoryType(requirements.memoryRequirements.memoryTypeBits &
                                              fdProperties.memoryTypeBits);
        if (result != VK_SUCCESS || memoryType == UINT32_MAX) {
            close(importFd);
            return false;
        }
        VkMemoryDedicatedAllocateInfo dedicatedInfo = {
                .sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
                .image = imported.sourceImage,
        };
        VkImportMemoryFdInfoKHR importInfo = {
                .sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR,
                .pNext = &dedicatedInfo,
                .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
                .fd = importFd,
        };
        VkMemoryAllocateInfo allocateInfo = {
                .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                .pNext = &importInfo,
                .allocationSize = allocationSize,
                .memoryTypeIndex = memoryType,
        };
        result = mAllocateMemory(mDevice, &allocateInfo, nullptr, &imported.sourceMemory);
        if (result != VK_SUCCESS) {
            close(importFd);
            return false;
        }
        return mBindImageMemory(mDevice, imported.sourceImage, imported.sourceMemory, 0) ==
                VK_SUCCESS;
    }

    bool createTargetImage(AHardwareBuffer* ahb, uint32_t width, uint32_t height,
                           TargetImage& target) {
        if (!ahb) return false;
        AHardwareBuffer_Desc desc{};
        AHardwareBuffer_describe(ahb, &desc);
        if (desc.width != width || desc.height != height) {
            LOG(ERROR) << "display target size mismatch: got " << desc.width << "x" << desc.height
                       << ", expected " << width << "x" << height;
            return false;
        }
        VkAndroidHardwareBufferFormatPropertiesANDROID formatProperties = {
                .sType = VK_STRUCTURE_TYPE_ANDROID_HARDWARE_BUFFER_FORMAT_PROPERTIES_ANDROID,
        };
        VkAndroidHardwareBufferPropertiesANDROID properties = {
                .sType = VK_STRUCTURE_TYPE_ANDROID_HARDWARE_BUFFER_PROPERTIES_ANDROID,
                .pNext = &formatProperties,
        };
        VkResult ahbResult;
        {
            ScopedTrace propertiesTrace("crosvm_display.ahb_properties");
            ahbResult = mGetAhbProperties(mDevice, ahb, &properties);
        }
        if (ahbResult != VK_SUCCESS || formatProperties.format == VK_FORMAT_UNDEFINED) {
            return false;
        }
        target.format = formatProperties.format;
        constexpr VkFormatFeatureFlags requiredTargetFeatures =
                VK_FORMAT_FEATURE_TRANSFER_DST_BIT | VK_FORMAT_FEATURE_BLIT_DST_BIT;
        bool targetImportSupported = false;
        {
            ScopedTrace capabilityTrace("crosvm_display.target_capability_query");
            targetImportSupported = externalImageImportSupported(
                    formatProperties.format, VK_IMAGE_TILING_OPTIMAL,
                    VK_IMAGE_USAGE_TRANSFER_DST_BIT,
                    VK_EXTERNAL_MEMORY_HANDLE_TYPE_ANDROID_HARDWARE_BUFFER_BIT_ANDROID);
        }
        if ((formatProperties.formatFeatures & requiredTargetFeatures) != requiredTargetFeatures ||
            !targetImportSupported) {
            LOG(ERROR) << "AHB target does not support transfer/blit destination import";
            return false;
        }

        VkExternalMemoryImageCreateInfo externalInfo = {
                .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
                .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_ANDROID_HARDWARE_BUFFER_BIT_ANDROID,
        };
        VkImageCreateInfo imageInfo = {
                .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
                .pNext = &externalInfo,
                .imageType = VK_IMAGE_TYPE_2D,
                .format = formatProperties.format,
                .extent = {width, height, 1},
                .mipLevels = 1,
                .arrayLayers = 1,
                .samples = VK_SAMPLE_COUNT_1_BIT,
                .tiling = VK_IMAGE_TILING_OPTIMAL,
                .usage = VK_IMAGE_USAGE_TRANSFER_DST_BIT,
                .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
                .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
        };
        {
            ScopedTrace imageTrace("crosvm_display.target_image_create");
            if (mCreateImage(mDevice, &imageInfo, nullptr, &target.image) != VK_SUCCESS) {
                return false;
            }
        }

        // The AHB external-memory rules require allocationSize and memoryTypeBits from
        // vkGetAndroidHardwareBufferPropertiesANDROID. Querying image memory requirements before
        // binding imported AHB memory is invalid.
        uint32_t memoryType = firstMemoryType(properties.memoryTypeBits);
        if (memoryType == UINT32_MAX) return false;
        VkMemoryDedicatedAllocateInfo dedicatedInfo = {
                .sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
                .image = target.image,
        };
        VkImportAndroidHardwareBufferInfoANDROID importInfo = {
                .sType = VK_STRUCTURE_TYPE_IMPORT_ANDROID_HARDWARE_BUFFER_INFO_ANDROID,
                .pNext = &dedicatedInfo,
                .buffer = ahb,
        };
        VkMemoryAllocateInfo allocateInfo = {
                .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                .pNext = &importInfo,
                .allocationSize = properties.allocationSize,
                .memoryTypeIndex = memoryType,
        };
        {
            ScopedTrace allocateTrace("crosvm_display.target_memory_allocate");
            if (mAllocateMemory(mDevice, &allocateInfo, nullptr, &target.memory) != VK_SUCCESS) {
                return false;
            }
        }
        {
            ScopedTrace bindTrace("crosvm_display.target_memory_bind");
            return mBindImageMemory(mDevice, target.image, target.memory, 0) == VK_SUCCESS;
        }
    }

    void destroyTarget(TargetImage& target) {
        ScopedTrace trace("crosvm_display.target_destroy");
        if (target.image && mDestroyImage) mDestroyImage(mDevice, target.image, nullptr);
        if (target.memory && mFreeMemory) mFreeMemory(mDevice, target.memory, nullptr);
        if (target.ahb) AHardwareBuffer_release(target.ahb);
        target = {};
    }

    void clearTargetCache() {
        for (auto& [_, target] : mTargetCache) destroyTarget(target);
        mTargetCache.clear();
    }

    // Destroy a cached target and remove it from the cache. Used on blit error paths so a target
    // that failed mid-record/submit is not left dangling in the cache. Safe to call with a target
    // that is not (or no longer) cached.
    void evictTarget(TargetImage& target) {
        AHardwareBuffer* ahb = target.ahb;
        destroyTarget(target);
        if (ahb) mTargetCache.erase(ahb);
    }

    // Returns a cached Vulkan import for the dequeued AHardwareBuffer, creating it on first use.
    // BufferQueue recycles a small fixed set of AHBs, so after warm-up this avoids the per-frame
    // vkCreateImage/vkAllocateMemory/vkImportAndroidHardwareBuffer/vkDestroy churn. The returned
    // pointer is stable across map growth (unordered_map guarantees reference stability). Cache
    // Entries are freed only after draining the device: on dimension/surface change, cache-cap
    // reset, or teardown. Therefore an in-flight blit can never reference a destroyed target.
    Result<TargetImage*> acquireTargetImage(AHardwareBuffer* ahb, uint32_t width, uint32_t height) {
        // A geometry change makes every cached AHB stale. Drain in-flight work, then rebuild.
        if (mTargetCacheWidth != width || mTargetCacheHeight != height) {
            if (mDeviceWaitIdle) mDeviceWaitIdle(mDevice);
            for (auto& slot : mInFlightSlots) reclaimSlotAfterDeviceIdle(slot);
            clearTargetCache();
            mTargetCacheWidth = width;
            mTargetCacheHeight = height;
        }

        auto it = mTargetCache.find(ahb);
        if (it != mTargetCache.end()) return &it->second;

        // Bound the cache: BufferQueue normally holds 3-4 buffers, but a surface churn could leak
        // stale AHB pointers. If we exceed the cap, drain and reset rather than grow unbounded.
        if (mTargetCache.size() >= kTargetCacheCap) {
            if (mDeviceWaitIdle) mDeviceWaitIdle(mDevice);
            for (auto& slot : mInFlightSlots) reclaimSlotAfterDeviceIdle(slot);
            clearTargetCache();
        }

        TargetImage target{};
        {
            ScopedTrace targetTrace("crosvm_display.target_import");
            if (!createTargetImage(ahb, width, height, target)) {
                destroyTarget(target);
                return Error() << "Failed to import display target AHardwareBuffer";
            }
        }
        AHardwareBuffer_acquire(ahb);
        target.ahb = ahb;
        target.width = width;
        target.height = height;
        auto [inserted, _] = mTargetCache.emplace(ahb, target);
        return &inserted->second;
    }

    void destroyImport(ImportedImage& imported) {
        if (imported.sourceImage && mDestroyImage) {
            mDestroyImage(mDevice, imported.sourceImage, nullptr);
        }
        if (imported.sourceMemory && mFreeMemory) {
            mFreeMemory(mDevice, imported.sourceMemory, nullptr);
        }
        imported = {};
    }

    // CPU-side wait on an acquire fence; consumes (closes) the fd. Used only as the fallback when
    // vkImportSemaphoreFdKHR fails after the caller already handed the fence to blit().
    static Result<void> pollFence(int fenceFd) {
        if (fenceFd < 0) return {};
        pollfd descriptor{.fd = fenceFd, .events = POLLIN, .revents = 0};
        int result;
        do {
            result = poll(&descriptor, 1, -1);
        } while (result < 0 && errno == EINTR);
        close(fenceFd);
        if (result <= 0) return Error() << "Failed to wait for display acquire fence";
        return {};
    }

    bool mReady = false;
    bool mNativeDestructionReady = false;
    VkBuffer mNativeStagingBuffer=VK_NULL_HANDLE;
    VkDeviceMemory mNativeStagingMemory=VK_NULL_HANDLE;
    void* mNativeStagingAddress=nullptr;
    // Three slots are not enough at the guest's 165 Hz cadence when a large
    // 3040x1904 blit overlaps a SurfaceFlinger dequeue. The fourth flush
    // waits for slot zero and feeds the guest's completion fence back into the
    // virtio queue, which produces the periodic stalls visible in UFO Test.
    // Keep enough slots to cover transient GPU work while retaining the
    // bounded queue and the existing drop-on-full behavior.
    static constexpr uint32_t kAsyncInFlightSlotCount = 6;
    static constexpr size_t kTargetCacheCap = 8;
    bool mAsyncBlitEnabled = false;
    bool mAsyncAcquireImportSupported = false;
    bool mSyncFdImportable = false;
    std::unordered_map<AHardwareBuffer*, TargetImage> mTargetCache;
    uint32_t mTargetCacheWidth = 0;
    uint32_t mTargetCacheHeight = 0;
    void* mLibrary = nullptr;
    HwvulkanDevice* mHal = nullptr;
    VkInstance mInstance = VK_NULL_HANDLE;
    VkPhysicalDevice mPhysicalDevice = VK_NULL_HANDLE;
    VkDevice mDevice = VK_NULL_HANDLE;
    VkQueue mQueue = VK_NULL_HANDLE;
    uint32_t mQueueFamilyIndex = 0;
    VkCommandPool mCommandPool = VK_NULL_HANDLE;
    std::vector<InFlightSlot> mInFlightSlots;
    size_t mNextInFlightSlot = 0;
    VkQueryPool mTimestampQueryPool = VK_NULL_HANDLE;
    float mTimestampPeriodNs = 0.0f;
    uint32_t mTimestampValidBits = 0;
    uint64_t mFrameSequence = 0;
    bool mTimestampReadFailureLogged = false;
    int64_t mNextImportId = 1;
    std::unordered_map<int64_t, ImportedImage> mImports;

    PFN_vkGetDeviceProcAddr mGetDeviceProcAddr = nullptr;
    PFN_vkGetPhysicalDeviceProperties mGetPhysicalDeviceProperties = nullptr;
    PFN_vkGetPhysicalDeviceFormatProperties2 mGetPhysicalDeviceFormatProperties2 = nullptr;
    PFN_vkGetPhysicalDeviceImageFormatProperties2 mGetPhysicalDeviceImageFormatProperties2 =
            nullptr;
    PFN_vkGetPhysicalDeviceExternalSemaphoreProperties
            mGetPhysicalDeviceExternalSemaphoreProperties = nullptr;
    PFN_vkDestroyInstance mDestroyInstance = nullptr;
    PFN_vkDestroyDevice mDestroyDevice = nullptr;
    PFN_vkDeviceWaitIdle mDeviceWaitIdle = nullptr;
    PFN_vkGetDeviceQueue mGetDeviceQueue = nullptr;
    PFN_vkCreateCommandPool mCreateCommandPool = nullptr;
    PFN_vkDestroyCommandPool mDestroyCommandPool = nullptr;
    PFN_vkAllocateCommandBuffers mAllocateCommandBuffers = nullptr;
    PFN_vkResetCommandBuffer mResetCommandBuffer = nullptr;
    PFN_vkBeginCommandBuffer mBeginCommandBuffer = nullptr;
    PFN_vkEndCommandBuffer mEndCommandBuffer = nullptr;
    PFN_vkQueueSubmit mQueueSubmit = nullptr;
    PFN_vkQueueWaitIdle mQueueWaitIdle = nullptr;
    PFN_vkCreateFence mCreateFence = nullptr;
    PFN_vkDestroyFence mDestroyFence = nullptr;
    PFN_vkWaitForFences mWaitForFences = nullptr;
    PFN_vkResetFences mResetFences = nullptr;
    PFN_vkCreateSemaphore mCreateSemaphore = nullptr;
    PFN_vkDestroySemaphore mDestroySemaphore = nullptr;
    PFN_vkGetSemaphoreFdKHR mGetSemaphoreFd = nullptr;
    PFN_vkImportSemaphoreFdKHR mImportSemaphoreFd = nullptr;
    PFN_vkCmdPipelineBarrier mCmdPipelineBarrier = nullptr;
    PFN_vkCmdCopyImage mCmdCopyImage = nullptr;
    PFN_vkCmdBlitImage mCmdBlitImage = nullptr;
    PFN_vkCreateQueryPool mCreateQueryPool = nullptr;
    PFN_vkDestroyQueryPool mDestroyQueryPool = nullptr;
    PFN_vkCmdResetQueryPool mCmdResetQueryPool = nullptr;
    PFN_vkCmdWriteTimestamp mCmdWriteTimestamp = nullptr;
    PFN_vkGetQueryPoolResults mGetQueryPoolResults = nullptr;
    PFN_vkCreateImage mCreateImage = nullptr;
    PFN_vkDestroyImage mDestroyImage = nullptr;
    PFN_vkGetImageMemoryRequirements2 mGetImageMemoryRequirements2 = nullptr;
    PFN_vkAllocateMemory mAllocateMemory = nullptr;
    PFN_vkFreeMemory mFreeMemory = nullptr;
    PFN_vkBindImageMemory mBindImageMemory = nullptr;
    PFN_vkGetMemoryFdPropertiesKHR mGetMemoryFdProperties = nullptr;
    PFN_vkGetAndroidHardwareBufferPropertiesANDROID mGetAhbProperties = nullptr;
};

// A Vulkan blit with nothing on the far side of it.
//
// VulkanDisplayBridge above never needed a screen. Its inputs are a dmabuf fd and an
// AHardwareBuffer*, and everything screen-shaped -- the binder service, the app's Surface, the
// BufferQueue those AHBs are dequeued from -- lives in AndroidDisplaySurface beside it, not in it.
// This class is what is left of the flip path when those are taken away: the same bridge, the same
// import, the same blit, into a buffer allocated here and readable by the CPU.
//
// That is the shape the VNC sink needs (plan §6 step 11). It cannot present an AHB -- it has to put
// pixels on a socket -- so its GPU half is "let the GPU do the copy and the channel-order
// conversion, then read the result back", and the readback is the point rather than a cost to be
// engineered away. Step 13 wants the same machinery pointed at a MediaCodec input buffer instead;
// `ensureTarget` is the only member that knows what the target is for.
class HeadlessBlitContext {
public:
    // The geometry is where the target starts, not a contract: `blit` re-allocates when the source
    // turns out to be a different size, which is how a guest resolution change is absorbed. Doing
    // it here as well means a screen that never resizes pays for its buffer at open rather than on
    // its first frame.
    HeadlessBlitContext(uint32_t width, uint32_t height) {
        if (mBridge.ready() && width && height) ensureTarget(width, height);
    }

    ~HeadlessBlitContext() {
        unmap();
        releaseTarget();
        // mBridge is destroyed after this body and drains the device first, so the copy of our AHB
        // reference it holds in its target cache outlives the one released just above -- which is
        // also what stops a freshly allocated target from being handed back the same pointer.
    }

    HeadlessBlitContext(const HeadlessBlitContext&) = delete;
    HeadlessBlitContext& operator=(const HeadlessBlitContext&) = delete;

    bool ready() const { return mBridge.ready(); }

    // `exchangeRedBlue` picks which byte order the blits from this import land in, and it is a
    // property of the import rather than of the blit because the only lever there is -- the source
    // image's declared format -- is fixed when the image is created. Two consumers of the same
    // guest scanout therefore need two imports of it: LibVNCServer wants B,G,R,X (exchange), the
    // H.264 encoder reads its input as the RGBA_8888 gralloc says it is (no exchange). See
    // blitSourceFourcc for the arithmetic.
    int64_t importDmabuf(int fd, uint32_t offset, uint32_t stride, uint64_t modifier,
                         bool linearLayoutVerified, uint32_t width, uint32_t height, uint32_t fourcc,
                         bool exchangeRedBlue) {
        return mBridge.importDmabuf(fd, offset, stride, modifier, linearLayoutVerified, width,
                                    height,
                                    exchangeRedBlue ? blitSourceFourcc(fourcc) : fourcc);
    }

    void release(int64_t importId) { mBridge.release(importId); }

    // Blit an import into a target somebody else owns, and hand back the completion fence.
    //
    // This is `blit` above with the two halves that are about *this* context's own buffer taken
    // out: no ensureTarget, no unmap, and no wait. The caller supplies the AHardwareBuffer and
    // decides what the fence is for -- the H.264 path passes it straight to queueBuffer, so the
    // codec waits on the GPU instead of this thread doing it. -1 means the bridge already drained
    // the queue and the target is ready now.
    //
    // The bridge's target cache is keyed by AHB pointer, so a foreign target costs one Vulkan
    // import on first sight and nothing afterwards, exactly like a dequeued display buffer.
    Result<int> blitInto(int64_t importId, AHardwareBuffer* ahb) {
        if (!ahb) return Error() << "no target AHardwareBuffer to blit into";
        return mBridge.blit(importId, ahb);
    }

    // Blit an import into the target and wait, with a bound, for the GPU to be done with it.
    //
    // Returning drops any CPU mapping first: AHardwareBuffer_lock is where the CPU's view of this
    // memory is invalidated, so a mapping taken before this blit would go on showing the frame
    // before it. The caller maps again afterwards.
    Result<void> blit(int64_t importId, uint32_t width, uint32_t height, int timeoutMs) {
        unmap();
        if (!ensureTarget(width, height)) {
            return Error() << "no CPU-readable blit target for " << width << "x" << height;
        }
        auto fence = mBridge.blit(importId, mAhb);
        if (!fence.ok()) return fence.error();
        int fenceFd = *fence;
        // -1 is the bridge saying it already drained the queue itself, on either of the two paths
        // that end there: async blit disabled, or the completion sync_fd could not be exported.
        if (fenceFd < 0) return {};

        pollfd descriptor{.fd = fenceFd, .events = POLLIN, .revents = 0};
        int ret;
        do {
            ret = poll(&descriptor, 1, timeoutMs);
        } while (ret < 0 && errno == EINTR);
        const int pollErrno = errno;
        close(fenceFd);
        // Bounded, and a timeout is a hard error rather than "read it anyway". This is a readback,
        // not a present: handing the CPU a target the GPU is still writing produces a torn frame
        // with nothing anywhere to say so, which is exactly the class of failure that looks like a
        // picture. The caller's answer to an error is the CPU copy, which is always available.
        if (ret == 0) {
            return Error() << "blit completion fence unsignalled after " << timeoutMs << "ms";
        }
        if (ret < 0) return Error() << "poll on blit completion fence failed: " << pollErrno;
        return {};
    }

    // Map the target for CPU reading. The mapping stays valid until the next `blit` or until this
    // context is destroyed, which is what lets a consumer keep referring to the last frame -- the
    // VNC sink's cursor-only updates re-read the pixels the pointer used to cover.
    Result<void> map(const uint8_t** outPixels, uint32_t* outStrideBytes, uint32_t* outWidth,
                     uint32_t* outHeight, uint32_t* outSize) {
        if (!mAhb) return Error() << "no blit target to map";
        if (!mMapped) {
            void* address = nullptr;
            const int status = AHardwareBuffer_lock(mAhb, AHARDWAREBUFFER_USAGE_CPU_READ_OFTEN,
                                                    /* fence= */ -1, /* rect= */ nullptr, &address);
            if (status != 0 || address == nullptr) {
                return Error() << "AHardwareBuffer_lock for CPU read failed: " << status;
            }
            mAddress = address;
            mMapped = true;
            gDisplayPathCounters.blitReadMaps.fetch_add(1, std::memory_order_relaxed);
        }
        *outPixels = static_cast<const uint8_t*>(mAddress);
        *outStrideBytes = mStrideBytes;
        *outWidth = mWidth;
        *outHeight = mHeight;
        *outSize = mStrideBytes * mHeight;
        return {};
    }

    void unmap() {
        if (!mMapped) return;
        AHardwareBuffer_unlock(mAhb, /* fence= */ nullptr);
        mMapped = false;
        mAddress = nullptr;
    }

private:
    static constexpr uint32_t fourccOf(char a, char b, char c, char d) {
        return static_cast<uint32_t>(a) | (static_cast<uint32_t>(b) << 8) |
                (static_cast<uint32_t>(c) << 16) | (static_cast<uint32_t>(d) << 24);
    }

    // The fourcc the SOURCE image is declared with, which is not the fourcc the guest declared.
    //
    // The only lever vkCmdBlitImage offers over channel order is the pair of formats it is given:
    // it reads texels through the source format and writes them through the destination format, so
    // naming a channel differently on one side moves it on the other. The destination here is
    // AHARDWAREBUFFER_FORMAT_R8G8B8A8_UNORM -- R,G,B,A in memory, and the only 32-bit colour format
    // in the NDK's AHardwareBuffer enum at all, so it is not a choice. The consumer at the far end
    // is LibVNCServer, whose serverFormat is red<<16 green<<8 blue<<0 little-endian: B,G,R,X in
    // memory, the VNC sink's declared target order. Those two disagree by exactly a red/blue
    // exchange.
    //
    // The CPU path pays for that exchange at its producer-to-sink copy boundary. Here it costs
    // nothing at all: declare the source with its red and blue names exchanged and the GPU performs
    // the swap as part of the copy it was already doing. A guest AR24 scanout is B,G,R,A in memory;
    // declared as AB24 the blit samples R := B_true and
    // B := R_true, and writing that through an R8G8B8A8 destination puts B,G,R,A back in memory --
    // which is what VNC reads. Exactly, not approximately: identical extents with VK_FILTER_NEAREST
    // between two 8-bit UNORM formats is a texel copy.
    //
    // This is a deliberate misdeclaration in the one place §4.4 warns is silently fatal, so it is a
    // named function with the arithmetic written out rather than a swapped constant at a call site.
    // Backwards, it does not fail: it returns the whole picture with red and blue exchanged, with
    // nothing in any log.
    static uint32_t blitSourceFourcc(uint32_t guestFourcc) {
        switch (guestFourcc) {
            case fourccOf('A', 'R', '2', '4'):
                return fourccOf('A', 'B', '2', '4');
            case fourccOf('A', 'B', '2', '4'):
                return fourccOf('A', 'R', '2', '4');
            case fourccOf('X', 'R', '2', '4'):
                return fourccOf('X', 'B', '2', '4');
            case fourccOf('X', 'B', '2', '4'):
                return fourccOf('X', 'R', '2', '4');
            default:
                // Not a format the bridge accepts either way; hand it through so the refusal names
                // the fourcc the guest actually declared.
                return guestFourcc;
        }
    }

    bool ensureTarget(uint32_t width, uint32_t height) {
        if (mAhb && mWidth == width && mHeight == height) return true;
        unmap();
        releaseTarget();
        if (!width || !height) return false;

        AHardwareBuffer_Desc request = {
                .width = width,
                .height = height,
                .layers = 1,
                .format = AHARDWAREBUFFER_FORMAT_R8G8B8A8_UNORM,
                // GPU_COLOR_OUTPUT is what makes gralloc give back something Vulkan will import as
                // a blit destination; CPU_READ_OFTEN is what this buffer exists for.
                .usage = AHARDWAREBUFFER_USAGE_CPU_READ_OFTEN |
                        AHARDWAREBUFFER_USAGE_GPU_COLOR_OUTPUT,
        };
        AHardwareBuffer* ahb = nullptr;
        const int status = AHardwareBuffer_allocate(&request, &ahb);
        if (status != 0 || ahb == nullptr) {
            LOG(ERROR) << "failed to allocate " << width << "x" << height
                       << " CPU-readable blit target: " << status;
            return false;
        }
        AHardwareBuffer_Desc actual{};
        AHardwareBuffer_describe(ahb, &actual);
        mAhb = ahb;
        mWidth = width;
        mHeight = height;
        // AHardwareBuffer_describe reports the stride in PIXELS. gralloc is free to pad it past the
        // width, and does; the consumer is told the real number rather than being assumed packed.
        mStrideBytes = actual.stride * 4;
        LOG(INFO) << "headless blit target " << width << "x" << height
                  << " stride=" << actual.stride << "px";
        return true;
    }

    void releaseTarget() {
        if (!mAhb) return;
        AHardwareBuffer_release(mAhb);
        mAhb = nullptr;
        mWidth = 0;
        mHeight = 0;
        mStrideBytes = 0;
    }

    VulkanDisplayBridge mBridge;
    AHardwareBuffer* mAhb = nullptr;
    uint32_t mWidth = 0;
    uint32_t mHeight = 0;
    uint32_t mStrideBytes = 0;
    bool mMapped = false;
    void* mAddress = nullptr;
};

// The guest's hardware cursor, as one frame's worth of overlay.
//
// The classic VNC consumer blends this into its own outgoing framebuffer (blend_cursor, in
// crosvm's vnc_server_bridge.c) and hands a copy to LibVNCServer as an RFB cursor so a client can
// draw it itself. Neither of those reaches an H.264 decoder: a video stream is pixels and nothing
// else, so for this consumer the pointer has to be IN the encoded picture or it is not there at
// all. Same bytes, same straight-alpha convention, blended a second time into a different canvas.
struct H264CursorOverlay {
    const uint8_t* pixels = nullptr; // B,G,R,A with a meaningful alpha byte
    int width = 0;
    int height = 0;
    int x = 0; // top-left of the image, hotspot already applied by the guest; may be negative
    int y = 0;
    bool visible = false;
};

// One H.264 encoder, fed through its own input Surface.
//
// Plan §6 step 13. The picture the VNC sink already has -- the same frame, at the same instant,
// off the same bus offer -- goes into a MediaCodec input buffer instead of onto an RFB socket, and
// the compressed result leaves by a side channel. RFB is untouched; a legacy client keeps being
// served by the LibVNCServer consumer from the same offer, which is what the bus was split for.
//
// The input Surface is the whole reason this is worth doing. AMediaCodec_createInputSurface hands
// back an ANativeWindow whose buffers are gralloc buffers, so the frame can be delivered to the
// encoder the same way it is delivered to the app's display: dequeue a buffer, put the picture in
// it, queue it. When the producer is on the GPU transport that "put the picture in it" is the
// step-11 Vulkan blit with a different target, and the guest's pixels never touch the CPU on their
// way into the codec. Plan §7 listed that as an unverified premise -- "createTargetImage takes any
// AHardwareBuffer, mechanically it should work" -- and this class is where it is either true or
// reported false.
//
// The CPU route beside it is not a second design. It exists because the offer does not always come
// with a GPU source (a simplefb producer that fell back, a resource that failed to import), and
// because a dequeued codec buffer has to be CPU-writable anyway for the cursor to be blended into
// it. One dequeue/queue path, two ways to fill the buffer.
class H264EncoderSession {
public:
    static std::unique_ptr<H264EncoderSession> Open(uint32_t width, uint32_t height,
                                                    int32_t bitrateBps, int32_t frameRate,
                                                    int32_t iFrameIntervalSecs) {
        auto& media = MediaCodecLib::GetInstance();
        if (!media.IsSupported()) {
            LOG(ERROR) << "H.264 side channel: no media NDK on this device";
            return nullptr;
        }
        if (!width || !height) {
            LOG(ERROR) << "H.264 side channel: refusing a " << width << "x" << height << " screen";
            return nullptr;
        }

        // MediaCodec is a binder client, and the buffers it hands back arrive on incoming binder
        // transactions -- so this process needs threads to receive them on. The display path
        // starts the pool before registering its service; a VNC-only crosvm never takes that path,
        // and without this the codec comes up and then never returns a buffer. Idempotent, so the
        // two callers do not have to know about each other.
        ABinderProcess_setThreadPoolMaxThreadCount(4);
        ABinderProcess_startThreadPool();

        auto session = std::unique_ptr<H264EncoderSession>(new H264EncoderSession(width, height));

        AMediaFormat* format = media.AMediaFormat_new();
        if (!format) {
            LOG(ERROR) << "H.264 side channel: AMediaFormat_new failed";
            return nullptr;
        }
        auto formatGuard = android::base::make_scope_guard(
                [&] { media.AMediaFormat_delete(format); });
        media.AMediaFormat_setString(format, kFormatKeyMime, kMimeTypeAvc);
        media.AMediaFormat_setInt32(format, kFormatKeyWidth, static_cast<int32_t>(width));
        media.AMediaFormat_setInt32(format, kFormatKeyHeight, static_cast<int32_t>(height));
        media.AMediaFormat_setInt32(format, kFormatKeyColorFormat, kColorFormatSurface);
        media.AMediaFormat_setInt32(format, kFormatKeyBitRate, bitrateBps);
        media.AMediaFormat_setInt32(format, kFormatKeyFrameRate, frameRate);
        media.AMediaFormat_setInt32(format, kFormatKeyIFrameInterval, iFrameIntervalSecs);

        session->mCodec = media.AMediaCodec_createEncoderByType(kMimeTypeAvc);
        if (!session->mCodec) {
            LOG(ERROR) << "H.264 side channel: no encoder for " << kMimeTypeAvc;
            return nullptr;
        }
        media_status_t status = media.AMediaCodec_configure(session->mCodec, format,
                                                            /* surface= */ nullptr,
                                                            /* crypto= */ nullptr,
                                                            AMEDIACODEC_CONFIGURE_FLAG_ENCODE);
        if (status != AMEDIA_OK) {
            LOG(ERROR) << "H.264 side channel: configure(" << width << "x" << height << " @"
                       << bitrateBps << "bps) failed: " << status;
            return nullptr;
        }
        status = media.AMediaCodec_createInputSurface(session->mCodec, &session->mWindow);
        if (status != AMEDIA_OK || session->mWindow == nullptr) {
            LOG(ERROR) << "H.264 side channel: createInputSurface failed: " << status;
            return nullptr;
        }
        status = media.AMediaCodec_start(session->mCodec);
        if (status != AMEDIA_OK) {
            LOG(ERROR) << "H.264 side channel: start failed: " << status;
            return nullptr;
        }
        if (auto ready = session->prepareWindow(); !ready.ok()) {
            LOG(ERROR) << "H.264 side channel: " << ready.error().message();
            return nullptr;
        }

        std::string codecName = "?";
        if (media.AMediaCodec_getName && media.AMediaCodec_releaseName) {
            char* name = nullptr;
            if (media.AMediaCodec_getName(session->mCodec, &name) == AMEDIA_OK && name) {
                codecName = name;
                media.AMediaCodec_releaseName(session->mCodec, name);
            }
        }
        LOG(INFO) << "H.264 side channel: encoder \"" << codecName << "\" up for " << width << "x"
                  << height << " @ " << bitrateBps << " bps, " << frameRate << " fps, IDR every "
                  << iFrameIntervalSecs << "s";
        return session;
    }

    ~H264EncoderSession() {
        auto& media = MediaCodecLib::GetInstance();
        if (mCodec) {
            if (mWindow) media.AMediaCodec_signalEndOfInputStream(mCodec);
            media.AMediaCodec_stop(mCodec);
            media.AMediaCodec_delete(mCodec);
        }
        if (mWindow) {
            if (mConnectedApi) native_window_api_disconnect(mWindow, mConnectedApi);
            ANativeWindow_release(mWindow);
        }
    }

    H264EncoderSession(const H264EncoderSession&) = delete;
    H264EncoderSession& operator=(const H264EncoderSession&) = delete;

    uint32_t width() const { return mWidth; }
    uint32_t height() const { return mHeight; }

    // Ask the encoder for an IDR at the next opportunity.
    //
    // Called when a side-channel client connects, which is the case the i-frame interval cannot
    // cover: a decoder that joins between two IDRs has no reference frame and shows nothing until
    // the next one, up to a whole interval later. Cheap enough that there is no reason to be
    // clever about it -- the encoder coalesces repeats itself.
    void requestSyncFrame() {
        auto& media = MediaCodecLib::GetInstance();
        AMediaFormat* params = media.AMediaFormat_new();
        if (!params) return;
        media.AMediaFormat_setInt32(params, kFormatKeyRequestSync, 0);
        media.AMediaCodec_setParameters(mCodec, params);
        media.AMediaFormat_delete(params);
    }

    // Feed one picture into the codec's input surface.
    //
    // `importId` non-zero selects the GPU route: `blitCtx` blits that import straight into the
    // dequeued gralloc buffer. Zero selects the CPU route, which uploads `pixels` (B,G,R,X, packed
    // to `width`) into it. Either way the cursor is blended afterwards, in place, on the CPU.
    Result<void> encodeFrame(HeadlessBlitContext* blitCtx, int64_t importId, const uint8_t* pixels,
                             uint32_t pixelsSize, uint32_t width, uint32_t height,
                             const H264CursorOverlay& cursor, int64_t ptsUs) {
        if (width != mWidth || height != mHeight) {
            return Error() << "encoder is " << mWidth << "x" << mHeight << " but the frame is "
                           << width << "x" << height;
        }
        if (auto ready = prepareWindow(); !ready.ok()) return ready;

        // The timestamp has to be set before the buffer is queued: it travels with the queue, not
        // with the dequeue. A codec input surface reads it as the frame's presentation time.
        ANativeWindow_setBuffersTimestamp(mWindow, ptsUs * 1000);

        ANativeWindowBuffer* buffer = nullptr;
        int fenceFd = -1;
        int result = ANativeWindow_dequeueBuffer(mWindow, &buffer, &fenceFd);
        if (result == -ETIMEDOUT || result == -EWOULDBLOCK) {
            // Every input buffer is still with the encoder. Drop the frame rather than stall: this
            // runs on the guest's own flush path (VncSurface::flip_to), so waiting here would put
            // a video encoder into the guest's vblank loop -- the exact coupling step 11 measured
            // its way out of. The next flush carries a newer picture anyway.
            if (fenceFd >= 0) close(fenceFd);
            ++mDroppedFrames;
            return {};
        }
        if (result != 0 || buffer == nullptr) {
            if (fenceFd >= 0) close(fenceFd);
            return Error() << "dequeue from the codec input surface failed: " << result;
        }

        AHardwareBuffer* ahb = ANativeWindowBuffer_getHardwareBuffer(buffer);
        if (ahb == nullptr) {
            if (fenceFd >= 0) close(fenceFd);
            ANativeWindow_cancelBuffer(mWindow, buffer, -1);
            return Error() << "the codec input buffer has no AHardwareBuffer";
        }

        // The GPU route hands the acquire fence to the blit and gets a completion fence back; the
        // CPU route has to wait for the acquire itself before it may touch the memory.
        int queueFenceFd = -1;
        Result<void> filled;
        if (importId != 0 && blitCtx != nullptr) {
            filled = fillByBlit(blitCtx, importId, ahb, fenceFd, &queueFenceFd);
            fenceFd = -1; // fillByBlit owns it on every path
        } else {
            filled = waitFence(fenceFd);
            fenceFd = -1; // waitFence closes it
            if (filled.ok()) filled = fillByUpload(ahb, pixels, pixelsSize);
        }
        if (!filled.ok()) {
            if (queueFenceFd >= 0) close(queueFenceFd);
            ANativeWindow_cancelBuffer(mWindow, buffer, -1);
            return filled;
        }

        if (cursor.visible && cursor.pixels && cursor.width > 0 && cursor.height > 0) {
            // The blend reads and writes the target, so whatever is still writing it has to be
            // finished. On the CPU route that fence was already waited on and this is a no-op.
            if (auto ret = waitFence(queueFenceFd); !ret.ok()) {
                queueFenceFd = -1;
                ANativeWindow_cancelBuffer(mWindow, buffer, -1);
                return ret;
            }
            queueFenceFd = -1;
            if (auto ret = blendCursor(ahb, cursor); !ret.ok()) {
                ANativeWindow_cancelBuffer(mWindow, buffer, -1);
                return ret;
            }
        }

        result = ANativeWindow_queueBuffer(mWindow, buffer, queueFenceFd);
        if (result != 0) {
            // queueBuffer consumes the fence even when it fails, so it is not ours to close.
            ANativeWindow_cancelBuffer(mWindow, buffer, -1);
            return Error() << "queue to the codec input surface failed: " << result;
        }
        ++mQueuedFrames;
        return {};
    }

    // Drain one compressed buffer.
    //
    // Returns the number of bytes written to `out`, 0 when nothing was ready before the timeout,
    // and a negative number on error. `kOutputTooSmall` means the buffer exists but did not fit;
    // *outSize then holds what it needs and the encoded frame has NOT been dropped -- it is still
    // owned by the codec, so a caller that grows and asks again gets the same one.
    static constexpr int32_t kOutputTooSmall = -1;
    static constexpr int32_t kOutputFailed = -2;
    int32_t pollOutput(uint8_t* out, uint32_t cap, uint32_t* outSize, uint32_t* outFlags,
                       int64_t* outPtsUs, int64_t timeoutUs) {
        auto& media = MediaCodecLib::GetInstance();
        AMediaCodecBufferInfo info{};
        ssize_t index = media.AMediaCodec_dequeueOutputBuffer(mCodec, &info, timeoutUs);
        if (index == AMEDIACODEC_INFO_TRY_AGAIN_LATER ||
            index == AMEDIACODEC_INFO_OUTPUT_BUFFERS_CHANGED) {
            return 0;
        }
        if (index == AMEDIACODEC_INFO_OUTPUT_FORMAT_CHANGED) {
            captureCodecConfig();
            return 0;
        }
        if (index < 0) return kOutputFailed;

        size_t bufferSize = 0;
        uint8_t* data = media.AMediaCodec_getOutputBuffer(mCodec, static_cast<size_t>(index),
                                                          &bufferSize);
        if (data == nullptr) {
            media.AMediaCodec_releaseOutputBuffer(mCodec, static_cast<size_t>(index), false);
            return kOutputFailed;
        }
        const uint32_t size = static_cast<uint32_t>(info.size < 0 ? 0 : info.size);
        if (outSize) *outSize = size;
        if (outFlags) *outFlags = info.flags;
        if (outPtsUs) *outPtsUs = info.presentationTimeUs;
        if (size > cap) {
            // Left undrained on purpose: releasing it here would lose the frame, and the caller's
            // answer to "it did not fit" is to come back with a bigger buffer.
            return kOutputTooSmall;
        }
        memcpy(out, data + info.offset, size);
        if ((info.flags & AMEDIACODEC_BUFFER_FLAG_CODEC_CONFIG) != 0) {
            std::lock_guard lk(mCodecConfigMutex);
            mCodecConfig.assign(out, out + size);
        }
        media.AMediaCodec_releaseOutputBuffer(mCodec, static_cast<size_t>(index), false);
        return static_cast<int32_t>(size);
    }

    // The stream's SPS and PPS, as Annex-B, or nothing if the encoder has not produced them yet.
    //
    // Kept so a client that connects mid-stream can be handed them before its first IDR. The
    // encoder emits them exactly once, in the first output buffer, which is fine for the client
    // that was already connected and useless for every one after it.
    size_t codecConfig(uint8_t* out, uint32_t cap) {
        std::lock_guard lk(mCodecConfigMutex);
        if (mCodecConfig.empty() || mCodecConfig.size() > cap) return mCodecConfig.size();
        memcpy(out, mCodecConfig.data(), mCodecConfig.size());
        return mCodecConfig.size();
    }

    uint64_t droppedFrames() const { return mDroppedFrames; }
    uint64_t queuedFrames() const { return mQueuedFrames; }

private:
    H264EncoderSession(uint32_t width, uint32_t height) : mWidth(width), mHeight(height) {}

    // What the input surface's buffers have to be for both fill routes to work.
    //
    // RGBA_8888 because it is what an encoder input surface is fed everywhere else in Android
    // (SurfaceFlinger's own screen recording path) and the only 32-bit colour format Vulkan will
    // import an AHardwareBuffer as. GPU_COLOR_OUTPUT is what makes gralloc give back something the
    // blit can be a destination of; the CPU bits are for the cursor blend and the upload route.
    // Asking for CPU access also tends to force a linear layout, which is what the Vulkan import
    // wants anyway -- a compressed one would be refused.
    Result<void> prepareWindow() {
        if (mWindowReady) return {};
        if (mWindow == nullptr) return Error() << "no codec input surface";
        // CPU is the honest declaration -- this producer writes with the CPU on one route and with
        // a Vulkan blit on the other, and neither is EGL. EGL is tried second only because it is
        // what every other producer of a codec input surface in Android connects as, so a
        // BufferQueue that turns CPU down is not out of the question and the failure would
        // otherwise be a whole rung reported as unavailable.
        mConnectedApi = NATIVE_WINDOW_API_CPU;
        int connected = native_window_api_connect(mWindow, mConnectedApi);
        if (connected != 0) {
            LOG(WARNING) << "codec input surface refused a CPU producer (" << connected
                         << "); trying EGL";
            mConnectedApi = NATIVE_WINDOW_API_EGL;
            connected = native_window_api_connect(mWindow, mConnectedApi);
        }
        if (connected != 0) {
            mConnectedApi = 0;
            return Error() << "failed to connect to the codec input surface: " << connected;
        }
        if (int result = ANativeWindow_setBuffersGeometry(mWindow, static_cast<int32_t>(mWidth),
                                                          static_cast<int32_t>(mHeight),
                                                          HAL_PIXEL_FORMAT_RGBA_8888);
            result != 0) {
            native_window_api_disconnect(mWindow, mConnectedApi);
            mConnectedApi = 0;
            return Error() << "failed to set codec input geometry: " << result;
        }
        constexpr uint64_t kUsage = AHARDWAREBUFFER_USAGE_GPU_COLOR_OUTPUT |
                AHARDWAREBUFFER_USAGE_CPU_WRITE_OFTEN | AHARDWAREBUFFER_USAGE_CPU_READ_OFTEN;
        if (int result = ANativeWindow_setUsage(mWindow, kUsage); result != 0) {
            native_window_api_disconnect(mWindow, mConnectedApi);
            mConnectedApi = 0;
            return Error() << "failed to set codec input usage: " << result;
        }
        // Never block the producer. Same reason as the display sink's GPU path: the caller is on
        // the guest's flush path and a full codec queue must cost a dropped frame, not a stalled
        // guest.
        if (int result = mWindow->perform(mWindow, NATIVE_WINDOW_SET_DEQUEUE_TIMEOUT, (int64_t)0);
            result != 0) {
            LOG(WARNING) << "H.264 side channel: zero dequeue timeout refused: " << result;
        }
        mWindowReady = true;
        return {};
    }

    // Waits for a fence and closes it. -1 is "nothing to wait for" and succeeds.
    static Result<void> waitFence(int fenceFd) {
        if (fenceFd < 0) return {};
        pollfd descriptor{.fd = fenceFd, .events = POLLIN, .revents = 0};
        int ret;
        do {
            ret = poll(&descriptor, 1, kFenceTimeoutMs);
        } while (ret < 0 && errno == EINTR);
        const int pollErrno = errno;
        close(fenceFd);
        if (ret == 0) return Error() << "codec buffer fence unsignalled after " << kFenceTimeoutMs
                                     << "ms";
        if (ret < 0) return Error() << "poll on a codec buffer fence failed: " << pollErrno;
        return {};
    }

    // The GPU route: the guest's own dmabuf, blitted into the encoder's buffer by the same Vulkan
    // bridge that blits into the app's display buffer. This is the §7 premise; if it is going to
    // fail it fails here, and the message says so.
    Result<void> fillByBlit(HeadlessBlitContext* blitCtx, int64_t importId, AHardwareBuffer* ahb,
                            int acquireFenceFd, int* outCompletionFd) {
        // The bridge's blit does not take an acquire fence on this entry point, so the wait is
        // ours. It is a display buffer coming back from the encoder rather than from a compositor,
        // so in practice it is already signalled.
        if (auto ret = waitFence(acquireFenceFd); !ret.ok()) return ret;
        auto fence = blitCtx->blitInto(importId, ahb);
        if (!fence.ok()) {
            return Error() << "Vulkan blit into the MediaCodec input buffer failed: "
                           << fence.error().message();
        }
        *outCompletionFd = *fence;
        return {};
    }

    // The CPU route: the picture the sink already has, copied in with red and blue exchanged.
    //
    // `pixels` is the VNC sink's B,G,R,X framebuffer and the MediaCodec input buffer is declared
    // RGBA_8888. Convert while uploading, matching the producer-to-sink edge rule used by the
    // crosvm CPU display path.
    Result<void> fillByUpload(AHardwareBuffer* ahb, const uint8_t* pixels, uint32_t pixelsSize) {
        if (pixels == nullptr) return Error() << "no pixels to upload and no GPU source either";
        AHardwareBuffer_Desc desc{};
        AHardwareBuffer_describe(ahb, &desc);
        void* address = nullptr;
        const int status = AHardwareBuffer_lock(ahb, AHARDWAREBUFFER_USAGE_CPU_WRITE_OFTEN,
                                                /* fence= */ -1, /* rect= */ nullptr, &address);
        if (status != 0 || address == nullptr) {
            return Error() << "AHardwareBuffer_lock on a MediaCodec input buffer failed: "
                           << status;
        }
        const size_t dstStride = static_cast<size_t>(desc.stride) * 4;
        const size_t srcStride = static_cast<size_t>(mWidth) * 4;
        uint8_t* dstBase = static_cast<uint8_t*>(address);
        for (uint32_t y = 0; y < mHeight; y++) {
            const size_t srcOff = static_cast<size_t>(y) * srcStride;
            if (srcOff + srcStride > pixelsSize) break;
            copyBgrxRowToRgba(dstBase + static_cast<size_t>(y) * dstStride, pixels + srcOff,
                              mWidth);
        }
        AHardwareBuffer_unlock(ahb, /* fence= */ nullptr);
        return {};
    }

    static void copyBgrxRowToRgba(uint8_t* dst, const uint8_t* src, uint32_t pixelCount) {
        gDisplayPathCounters.encoderUploadBytes.fetch_add(uint64_t(pixelCount) * 4,
                                                          std::memory_order_relaxed);
        uint32_t x = 0;
#if defined(__aarch64__)
        for (; x + 16 <= pixelCount; x += 16, dst += 64, src += 64) {
            uint8x16x4_t channels = vld4q_u8(src);
            uint8x16_t tmp = channels.val[0];
            channels.val[0] = channels.val[2];
            channels.val[2] = tmp;
            vst4q_u8(dst, channels);
        }
#endif
        for (; x < pixelCount; x++, dst += 4, src += 4) {
            dst[0] = src[2];
            dst[1] = src[1];
            dst[2] = src[0];
            dst[3] = src[3];
        }
    }

    // Alpha-blend the guest's pointer into the encoded picture, clipped to the screen.
    //
    // Straight (non-premultiplied) alpha, matching blend_cursor on the classic path exactly, so
    // the two renderings of the same pointer agree. The source is B,G,R,A and the target is
    // R,G,B,A, so the channel exchange happens here too.
    Result<void> blendCursor(AHardwareBuffer* ahb, const H264CursorOverlay& cursor) {
        const int screenW = static_cast<int>(mWidth);
        const int screenH = static_cast<int>(mHeight);
        const int x0 = cursor.x < 0 ? 0 : cursor.x;
        const int y0 = cursor.y < 0 ? 0 : cursor.y;
        const int x1 = std::min(cursor.x + cursor.width, screenW);
        const int y1 = std::min(cursor.y + cursor.height, screenH);
        if (x1 <= x0 || y1 <= y0) return {};

        AHardwareBuffer_Desc desc{};
        AHardwareBuffer_describe(ahb, &desc);
        ARect rect{.left = x0, .top = y0, .right = x1, .bottom = y1};
        void* address = nullptr;
        const int status = AHardwareBuffer_lock(
                ahb, AHARDWAREBUFFER_USAGE_CPU_WRITE_OFTEN | AHARDWAREBUFFER_USAGE_CPU_READ_OFTEN,
                /* fence= */ -1, &rect, &address);
        if (status != 0 || address == nullptr) {
            return Error() << "AHardwareBuffer_lock for the cursor blend failed: " << status;
        }
        const size_t dstStride = static_cast<size_t>(desc.stride) * 4;
        uint8_t* dstBase = static_cast<uint8_t*>(address);
        for (int y = y0; y < y1; y++) {
            const uint8_t* src =
                    cursor.pixels + (((y - cursor.y) * cursor.width) + (x0 - cursor.x)) * 4;
            uint8_t* dst = dstBase + static_cast<size_t>(y) * dstStride +
                    static_cast<size_t>(x0) * 4;
            for (int x = x0; x < x1; x++, src += 4, dst += 4) {
                const uint32_t a = src[3];
                if (a == 0) continue;
                if (a == 255) {
                    dst[0] = src[2];
                    dst[1] = src[1];
                    dst[2] = src[0];
                    continue;
                }
                dst[0] = static_cast<uint8_t>((src[2] * a + dst[0] * (255 - a)) / 255);
                dst[1] = static_cast<uint8_t>((src[1] * a + dst[1] * (255 - a)) / 255);
                dst[2] = static_cast<uint8_t>((src[0] * a + dst[2] * (255 - a)) / 255);
            }
        }
        AHardwareBuffer_unlock(ahb, /* fence= */ nullptr);
        return {};
    }

    // SPS and PPS off the settled output format, concatenated in that order.
    //
    // Belt and braces with the CODEC_CONFIG output buffer pollOutput also caches: which of the two
    // an encoder produces is not something every component agrees on, and both arrive before the
    // first coded picture, so whichever lands first is the one a late client is handed.
    void captureCodecConfig() {
        auto& media = MediaCodecLib::GetInstance();
        AMediaFormat* format = media.AMediaCodec_getOutputFormat(mCodec);
        if (!format) return;
        auto formatGuard =
                android::base::make_scope_guard([&] { media.AMediaFormat_delete(format); });
        std::vector<uint8_t> config;
        for (const char* key : {kFormatKeyCsd0, kFormatKeyCsd1}) {
            void* data = nullptr;
            size_t size = 0;
            if (media.AMediaFormat_getBuffer(format, key, &data, &size) && data && size) {
                const uint8_t* bytes = static_cast<const uint8_t*>(data);
                config.insert(config.end(), bytes, bytes + size);
            }
        }
        if (config.empty()) return;
        std::lock_guard lk(mCodecConfigMutex);
        if (mCodecConfig.empty()) {
            LOG(INFO) << "H.264 side channel: codec config is " << config.size()
                      << " bytes of Annex-B";
            mCodecConfig = std::move(config);
        }
    }

    // Three vsyncs of a 120 Hz panel, the figure every other bounded fence wait in this file uses.
    static constexpr int kFenceTimeoutMs = 25;

    const uint32_t mWidth;
    const uint32_t mHeight;
    AMediaCodec* mCodec = nullptr;
    ANativeWindow* mWindow = nullptr;
    bool mWindowReady = false;
    /// Which producer API the input surface is connected as, or 0 for not connected.
    int mConnectedApi = 0;
    uint64_t mDroppedFrames = 0;
    uint64_t mQueuedFrames = 0;
    std::mutex mCodecConfigMutex;
    std::vector<uint8_t> mCodecConfig;
};

class SinkANativeWindow_Buffer {
public:
    Result<void> configure(uint32_t width, uint32_t height, int format) {
        // kFormat is RGBA_8888 (see AndroidDisplaySurface::kFormat for why we no longer use BGRA).
        if (format != HAL_PIXEL_FORMAT_RGBA_8888) {
            return Error() << "Pixel format " << format << " is not RGBA_8888.";
        }

        mBufferBits.resize(width * height * 4);
        mBuffer = ANativeWindow_Buffer{
                .width = static_cast<int32_t>(width),
                .height = static_cast<int32_t>(height),
                .stride = static_cast<int32_t>(width),
                .format = format,
                .bits = mBufferBits.data(),
        };
        return {};
    }

    operator ANativeWindow_Buffer&() { return mBuffer; }

private:
    ANativeWindow_Buffer mBuffer;
    std::vector<uint8_t> mBufferBits;
};

// The acceptance instrument for a byte-identical refactor of the display pipeline (see the plan's
// §6 step 4 and §9): the three CPU copy sites that feed this sink are being consolidated behind one
// function, and "the sink receives the same bytes" is not an acceptance condition until something
// measures it. This sits deliberately below everything that refactor touches, so the same number is
// taken in both binaries and the two frame sequences can simply be compared.
//
// The hash covers exactly the visible pixels, row by row: the padding a gralloc stride leaves at
// the end of each row is displayed by nobody and initialised by nothing, so hashing it would let
// two identical frames disagree. Off unless CROSVM_DISPLAY_HASH_FRAMES=1, read once.
static bool frameHashEnabled() {
    static const bool enabled = envFlagEnabled("CROSVM_DISPLAY_HASH_FRAMES", false);
    return enabled;
}

static uint64_t fnv1a64VisiblePixels(const ANativeWindow_Buffer& buf) {
    uint64_t hash = 0xcbf29ce484222325ULL;
    const uint8_t* base = static_cast<const uint8_t*>(buf.bits);
    if (base == nullptr || buf.width <= 0 || buf.height <= 0) return hash;
    const size_t rowBytes = static_cast<size_t>(buf.width) * 4;
    for (int32_t y = 0; y < buf.height; y++) {
        const uint8_t* px = base + static_cast<size_t>(y) * static_cast<size_t>(buf.stride) * 4;
        for (size_t i = 0; i < rowBytes; i++) {
            hash = (hash ^ px[i]) * 0x100000001b3ULL;
        }
    }
    return hash;
}

static void logFrameHash(const std::string& surfaceName, const ANativeWindow_Buffer& buf) {
    LOG(INFO) << "FRAMEHASH surface=" << surfaceName << " " << buf.width << "x" << buf.height
              << " fnv1a64=0x" << std::hex << std::setw(16) << std::setfill('0')
              << fnv1a64VisiblePixels(buf) << std::dec;
}

// A direct SurfaceControl submission must return a waitable release object to
// the guest-side producer. The eventfd is signalled only after the consumer's
// release fence and the producer acquire fence are both complete. The shared
// state survives the caller dropping the holder, because the release callback
// is owned by ConsumerRelease until the framework has actually released it.
struct DirectReleaseSignal {
    android::base::unique_fd fd{eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK)};

    void signal() {
        if (fd.get() < 0) return;
        const uint64_t one = 1;
        ssize_t result;
        do {
            result = write(fd.get(), &one, sizeof(one));
        } while (result < 0 && errno == EINTR);
        if (result < 0 && errno != EAGAIN) PLOG(WARNING) << "direct AHB release eventfd write";
    }
};

// Wrapper which contains the latest available Surface/ANativeWindow from the DisplayService, if
// available. A Surface/ANativeWindow may not always be available if, for example, the VmLauncherApp
// on the other end of the DisplayService is not in the foreground / is paused.
class AndroidDisplaySurface {
public:
    AndroidDisplaySurface(const std::string& name) : mName(name), mPreRotate(name == "cursor") {}

    Result<int64_t> setNativeSurface(const Surface& incoming, bool owned = false) {
        if (!incoming.get()) return Error() << "Missing native Surface";
        int64_t lease = 0;
        {
            std::lock_guard lk(mSurfaceMutex);
            if (!mAttachmentLease.permitsAttach(owned))
                return Error() << "Legacy attach cannot replace an owned Surface";
            // clear/attach can each advance the generation. Never issue a
            // wrapped or non-positive token over signed AIDL long.
            if (mNativeSurfaceGeneration > static_cast<uint64_t>(INT64_MAX) - 2)
                return Error() << "Surface attachment generation exhausted";
            clearNativeSurfaceLocked(true);
            // NativeWindow's constructor acquires its own reference. Borrow
            // from the in-only parcel instead of stealing and echoing it back.
            mNativeSurface = std::make_unique<Surface>(incoming.get());
            mNativeSurfaceNeedsConfiguring = true;
            ++mNativeSurfaceGeneration;
            mAttachmentLease.attached(mNativeSurfaceGeneration, owned);
            refreshScanoutProfileLocked();
            // Wake for every real main-surface attach. eventfd coalesces bursts;
            // no timer, pixel copy, or retained-buffer replay is involved.
            signalNativeRefreshLocked();
            lease = static_cast<int64_t>(mAttachmentLease.current());
            LOG(INFO) << "display surface " << mName << " native window attached generation="
                      << mNativeSurfaceGeneration << " lease=" << lease;
            Surface* surface = mNativeSurface.get();
            if (!surface) {
                return Error() << "Failed to get Surface";
            }

            ANativeWindow* anw = surface->get();
            auto& sc = SurfaceControl::GetInstance();
            if (sc.IsSupported()) {
                mSurfaceControl = sc.ASurfaceControl_createFromWindow(anw, mName.c_str());
            }
            // A new app Activity has no saved-frame flag and the guest need not
            // resend an unchanged shape. Replay the last complete cursor here.
            if (mPreRotate) {
                if (auto ret = drawCursorFrameLocked(); !ret.ok()) {
                    LOG(WARNING) << "cursor replay on attach failed: " << ret.error();
                }
            }
        }

        mNativeSurfaceReady.notify_one();
        return lease;
    }

    void removeSurface(uint64_t lease = 0) {
        {
            std::lock_guard lk(mSurfaceMutex);
            if (!mAttachmentLease.accepts(lease)) {
                LOG(INFO) << "display surface " << mName << " ignored stale detach lease="
                          << lease << " current=" << mAttachmentLease.current();
                return;
            }
            clearNativeSurfaceLocked(true);
        }
        mNativeSurfaceReady.notify_one();
    }

    // The guest can disable/recreate its scanout while the Activity keeps the
    // same Surface attached (including handover to the simplefb producer).
    // End the direct layer's read, but preserve that app-owned Surface and
    // producer connection for the next guest/external frame. Detach only
    // submits removal; ConsumerRelease still owns the real completion.
    void releaseGuestSurface() {
        std::lock_guard lk(mSurfaceMutex);
        if (!android_display::SurfaceBufferPresenter::detachAndReset(mDirectPresenter)) {
            LOG(ERROR) << "display surface " << mName
                       << " failed to detach guest AHB presenter; retaining it";
            return;
        }
        mDirectPresenterGeneration = 0;
    }

    bool requestGuestSurfaceDetach() {
        std::unique_lock lk(mSurfaceMutex, std::try_to_lock);
        if (!lk.owns_lock()) return false;
        if (!mDirectPresenter) return true;
        // Never wait on a Binder cursor/window lock or a SurfaceFlinger apply
        // from the reset caller. Retain this presenter on failed admission.
        if (!mDirectPresenter->requestDetach()) return false;
        mDirectPresenter.reset(); // Already detached; destructor cannot apply.
        mDirectPresenterGeneration = 0;
        return true;
    }

    Surface* getSurface() {
        std::unique_lock lk(mSurfaceMutex);
        return mNativeSurface.get();
    }

    // Whether a native window is attached right now. This is the non-blocking read of the same
    // field that waitForNativeSurface() parks on, and the distinction is the whole point: the
    // caller is crosvm's simplefb bridge asking once per frame from its 30 fps timer thread, and
    // waiting there would hold that thread -- input dispatch included -- until the user came back
    // to the display view.
    bool hasNativeSurface() {
        std::lock_guard lk(mSurfaceMutex);
        return mNativeSurface != nullptr;
    }

    uint64_t nativeSurfaceGeneration() {
        std::lock_guard lk(mSurfaceMutex);
        return mNativeSurfaceGeneration;
    }

    bool setNativeRefreshEvent(int fd) {
        android::base::unique_fd duplicate;
        if (fd >= 0) {
            duplicate.reset(fcntl(fd, F_DUPFD_CLOEXEC, 0));
            if (duplicate.get() < 0) return false;
            const int flags = fcntl(duplicate.get(), F_GETFL);
            if (flags < 0 || fcntl(duplicate.get(), F_SETFL, flags | O_NONBLOCK) < 0) return false;
        }
        std::lock_guard lk(mSurfaceMutex);
        mNativeRefreshEvent = std::move(duplicate);
        if (mNativeSurface) signalNativeRefreshLocked();
        return true;
    }

    uint64_t nativeRefreshGeneration() {
        std::lock_guard lk(mSurfaceMutex);
        return mNativeSurface && mDirectPresentedGeneration != mNativeSurfaceGeneration
                ? mNativeSurfaceGeneration : 0;
    }

    bool displayTiming(android_display::DisplayTimingSample* sample) {
        if (!sample) return false;
        *sample = {};
        std::unique_lock lk(mSurfaceMutex, std::try_to_lock);
        if (!lk.owns_lock() || !mNativeSurface || !mDisplayTiming) return false;
        *sample = mDisplayTiming->snapshot(android_display::displayMonotonicNs());
        return sample->hostNowNs != 0;
    }

    Result<void> prepareLogicalSurface(uint32_t width, uint32_t height) {
        {
            std::lock_guard lk(mSurfaceMutex);
            if (mScanoutGeometry) {
                // A wrapper can be recreated after scanout disable/suspend
                // without clearing or changing its committed content geometry.
                if (!mPreRotate && mRequestedSurfaceDimensions &&
                        mRequestedSurfaceDimensions->width == width &&
                        mRequestedSurfaceDimensions->height == height &&
                        mScanoutGeometry->logicalWidth == width &&
                        mScanoutGeometry->logicalHeight == height)
                    return {};
                return Error() << "Logical wrapper cannot replace committed native geometry";
            }
        }
        return configure(width, height);
    }

    Result<void> configure(uint32_t width, uint32_t height) {
        std::unique_lock lk(mSurfaceMutex);

        if (mCpuProducerLocked)
            return Error() << "Cannot reconfigure an active CPU producer";
        if (mScanoutGeometry)
            return Error() << "Native geometry requires a generation-bound configure";

        LOG(INFO) << "display surface " << mName << " configure " << width << "x" << height;
        if (!mRequestedSurfaceDimensions || mRequestedSurfaceDimensions->width != width ||
            mRequestedSurfaceDimensions->height != height) resetDisplayTimingLocked();
        mRequestedSurfaceDimensions = Rect{
                .width = width,
                .height = height,
        };
        refreshScanoutProfileLocked();
        // The buffer geometry must follow: if a native window is already attached, its buffers
        // were sized for the previous dimensions and the next lock() has to re-apply them, or
        // frames of the new size get scaled into stale-geometry buffers (squashed display).
        mNativeSurfaceNeedsConfiguring = true;

        if (auto ret = mSinkBuffer.configure(width, height, kFormat); !ret.ok()) {
            return Error() << "Failed to configure sink buffer: " << ret.error();
        }
        if (mPreRotate) {
            if (!mCursorImage.configure(width, height))
                return Error() << "Failed to configure cursor image cache";
        }
        return {};
    }

    Result<void> configureScanoutGeometry(const android_display::ScanoutPresentationGeometry& geometry,
            uint64_t endpointGeneration = 0, uint64_t profileGeneration = 0) {
        std::lock_guard lk(mSurfaceMutex);
        refreshScanoutProfileLocked();
        const bool boundProfile = endpointGeneration || profileGeneration;
        if ((mBoundProfileGeneration && !boundProfile) ||
                (boundProfile && !mScanoutProfile.matches(geometry, endpointGeneration, profileGeneration)))
            return Error() << "Native scanout preferred profile changed";
        if (boundProfile && mScanoutGeometry &&
                (endpointGeneration != mBoundEndpointGeneration || profileGeneration != mBoundProfileGeneration) &&
                geometry.resetGeneration == mScanoutGeometry->resetGeneration &&
                geometry.modeGeneration <= mScanoutGeometry->modeGeneration)
            return Error() << "A new preferred profile requires a newer mode";
        // This first profile keeps app, input and cursor coordinates logical.
        // It does not resize the app's parent to the rotated AHB storage size.
        if (mPreRotate || !geometry.valid() || mCpuProducerLocked || !mNativeSurface ||
                (mNativeResetFloor && geometry.resetGeneration != mNativeResetFloor) ||
                !mRequestedSurfaceDimensions ||
                mRequestedSurfaceDimensions->width != geometry.logicalWidth ||
                mRequestedSurfaceDimensions->height != geometry.logicalHeight ||
                !SurfaceControl::GetInstance().HasBufferGeometry())
            return Error() << "Native scanout geometry prerequisites unavailable";
        auto* window = mNativeSurface->get();
        if (!window || ANativeWindow_getWidth(window) != static_cast<int32_t>(geometry.logicalWidth) ||
                ANativeWindow_getHeight(window) != static_cast<int32_t>(geometry.logicalHeight))
            return Error() << "Native scanout parent must retain logical dimensions";
        if (mScanoutGeometry) {
            const auto& old = *mScanoutGeometry;
            if (geometry.resetGeneration < old.resetGeneration ||
                    (geometry.resetGeneration == old.resetGeneration &&
                     geometry.modeGeneration < old.modeGeneration))
                return Error() << "Stale native scanout geometry";
            if (geometry.resetGeneration == old.resetGeneration &&
                    geometry.modeGeneration == old.modeGeneration) {
                if (!(geometry == old)) return Error() << "Native mode tuple changed";
                return {};
            }
        }
        // All checks precede mutation. Reset retires the old layer but never
        // signals its outstanding buffers. Only real consumer release does.
        if (mDirectPresenter && mScanoutGeometry &&
                geometry.resetGeneration != mScanoutGeometry->resetGeneration) {
            if (!mDirectPresenter->requestDetach())
                return Error() << "Native scanout reset detach unavailable";
            mDirectPresenter.reset();
        } else if (mDirectPresenter && !mDirectPresenter->configureScanoutGeometry(geometry)) {
            return Error() << "Native presenter rejected scanout geometry";
        }
        mScanoutGeometry = geometry;
        mNativeScanoutOnly = true;
        mBoundEndpointGeneration = endpointGeneration;
        mBoundProfileGeneration = profileGeneration;
        resetDisplayTimingLocked();
        mDirectPresentedGeneration = 0;
        signalNativeRefreshLocked();
        return {};
    }

    Result<void> resetScanoutGeometry(uint64_t oldReset, uint64_t newReset) {
        std::lock_guard lk(mSurfaceMutex);
        // Internal host reset only, after actual reader/renderer drain. Never
        // detach, signal a release, or clear endpoint/profile observations here.
        if (mPreRotate || !oldReset || oldReset == UINT64_MAX || newReset != oldReset + 1 ||
                mCpuProducerLocked || mDirectPresenter ||
                (mNativeResetFloor && mNativeResetFloor != oldReset) ||
                (mScanoutGeometry && mScanoutGeometry->resetGeneration != oldReset))
            return Error() << "Native scanout reset is not drained or is stale";
        mScanoutGeometry.reset();
        mBoundEndpointGeneration = mBoundProfileGeneration = 0;
        mNativeResetFloor = newReset;
        mDirectPresentedGeneration = 0;
        resetDisplayTimingLocked();
        // mNativeScanoutOnly is sticky: mode0 can use direct AHB, never CPU/blit.
        return {};
    }

    AndroidDisplayScanoutProfile scanoutProfile() {
        std::lock_guard lk(mSurfaceMutex);
        refreshScanoutProfileLocked();
        return mScanoutProfile.snapshot();
    }

    Result<void> setMainTransformHint(int32_t hint, uint64_t lease, uint64_t root, uint64_t serial) {
        std::lock_guard lk(mSurfaceMutex);
        if (mPreRotate || !lease || !mAttachmentLease.accepts(lease))
            return Error() << "Stale main transform observation";
        refreshScanoutProfileLocked();
        const auto previous = mScanoutProfile.snapshot();
        if (!mScanoutProfile.rootHint(root, serial, hint))
            return Error() << "Stale main root transform serial";
        const auto profile = mScanoutProfile.snapshot();
        if (previous.profile_generation != profile.profile_generation ||
                previous.endpoint_generation != profile.endpoint_generation || previous.flags != profile.flags) {
            signalNativeRefreshLocked();
            LOG(INFO) << "NATIVE-SCANOUT-PROFILE lease=" << lease << " root=" << root
                      << " serial=" << serial << " hint=" << hint
                      << " endpoint=" << profile.endpoint_generation
                      << " profile=" << profile.profile_generation << " flags=" << profile.flags
                      << " storage=" << profile.storage_width << "x" << profile.storage_height
                      << " logical=" << profile.logical_width << "x" << profile.logical_height
                      << " rotation=" << profile.content_rotation;
        }
        return {};
    }

    void setFrameRate(uint32_t refreshRate) {
        std::lock_guard lk(mSurfaceMutex);
        if (mRequestedFrameRate == refreshRate) return;
        resetDisplayTimingLocked();
        mRequestedFrameRate = refreshRate;
        mFrameRateConfigured = false;
        if (mDirectPresenter) (void)mDirectPresenter->setFrameRate(refreshRate);
    }

    Result<void> setCursorTransformHint(int hint, uint64_t lease = 0) {
        std::lock_guard lk(mSurfaceMutex);
        // A delayed hint RPC belongs to its original Activity/Surface, just
        // like detach. It cannot rotate a newly attached cursor producer.
        if (!mAttachmentLease.accepts(lease)) return {};
        if (!mPreRotate || !mCursorTransformHint.setAuthoritative(hint)) {
            return Error() << "Invalid cursor transform hint " << hint;
        }
        // The root Surface notification is independent of queueBuffer. This
        // redraw is necessary even when a same-size rotation leaves the
        // ANativeWindow's cached hint and guest cursor image unchanged.
        return drawCursorFrameLocked();
    }

    void waitForNativeSurface() {
        std::unique_lock lk(mSurfaceMutex);
        mNativeSurfaceReady.wait(lk, [this] { return mNativeSurface != nullptr; });
    }

    Result<void> lock(ANativeWindow_Buffer* out_buffer) {
        std::unique_lock lk(mSurfaceMutex);
        if (mNativeScanoutOnly || mScanoutGeometry || mCpuProducerLocked)
            return Error() << "CPU producer cannot use native scanout geometry or overlap a lock";
        mLastBuffer = {};

        // Always retain cursor updates, including while Native Display is
        // closed. Binder replay reads only the previous complete buffer while
        // the GPU worker writes this staging buffer outside mSurfaceMutex.
        if (mPreRotate) {
            auto* bits = mCursorImage.begin();
            if (!bits) return Error() << "Cursor image is unconfigured or already locked";
            *out_buffer = ANativeWindow_Buffer{
                    .width = static_cast<int32_t>(mCursorImage.width()),
                    .height = static_cast<int32_t>(mCursorImage.height()),
                    .stride = static_cast<int32_t>(mCursorImage.width()),
                    .format = kFormat,
                    .bits = bits,
            };
            return {};
        }

        Surface* surface = mNativeSurface.get();
        if (surface == nullptr) {
            // Surface not currently available but not necessarily an error
            // if, for example, the VmLauncherApp is not in the foreground.
            *out_buffer = mSinkBuffer;
            mCpuProducerLocked = true;
            return {};
        }

        ANativeWindow* anw = surface->get();
        if (anw == nullptr) {
            return Error() << "Failed to get ANativeWindow";
        }

        if (auto ret = prepareNativeWindowLocked(anw, false); !ret.ok()) {
            return ret;
        }

        if (auto ret = setBuffersTransformLocked(anw, 0); !ret.ok()) {
            return ret;
        }

        if (ANativeWindow_lock(anw, out_buffer, nullptr) != 0) {
            return Error() << "Failed to lock window";
        }
        // Rust writes after releasing mSurfaceMutex. Binder may detach or
        // replace mNativeSurface before post, so pin this exact producer.
        mCpuLockedSurface = std::make_unique<Surface>(anw);
        mLastBuffer = *out_buffer;
        mCpuProducerLocked = true;
        return {};
    }

    Result<int> flip(VulkanDisplayBridge& bridge, int64_t importId) {
        ScopedTrace trace("crosvm_display.flip");
        std::unique_lock lk(mSurfaceMutex);
        if (mNativeScanoutOnly || mScanoutGeometry || mCpuProducerLocked)
            return Error() << "Blit producer cannot use native scanout geometry";

        Surface* surface = mNativeSurface.get();
        if (surface == nullptr) {
            // The display service can temporarily lose its Surface while the launcher is
            // backgrounded. Keep the source import alive and drop the frame in that interval.
            return -1;
        }
        if (mVulkanTargetGeneration != mNativeSurfaceGeneration) {
            if (auto ret = bridge.resetTargetsForSurfaceChange(); !ret.ok()) {
                return Error() << ret.error();
            }
            mVulkanTargetGeneration = mNativeSurfaceGeneration;
        }
        ANativeWindow* anw = surface->get();
        if (anw == nullptr) return Error() << "Failed to get ANativeWindow";
        {
            ScopedTrace prepareTrace("crosvm_display.prepare_window");
            if (auto ret = prepareNativeWindowLocked(anw, true); !ret.ok()) {
                return Error() << ret.error();
            }
        }

        ANativeWindowBuffer* buffer = nullptr;
        int fenceFd = -1;
        int result;
        const bool allowFaultInjection = mName == "scanout";
        if (allowFaultInjection && injectRuntimeFlipFailure(RuntimeFlipFailureStage::kDequeue)) {
            return Error() << "Injected display dequeue failure";
        }
        {
            ScopedTrace dequeueTrace("crosvm_display.dequeue_buffer");
            result = ANativeWindow_dequeueBuffer(anw, &buffer, &fenceFd);
        }
        if (result == -ETIMEDOUT || result == -EWOULDBLOCK) {
            // Every buffer is queued to SurfaceFlinger and none has been released yet (the
            // producer runs with a zero dequeue timeout, see prepareNativeWindowLocked). Drop
            // this frame instead of stalling: nothing has touched the guest's source dmabuf, so
            // returning without a completion fence lets the guest reuse it right away, and the
            // next flush will present a newer frame.
            if (fenceFd >= 0) close(fenceFd);
            const uint64_t dropped = ++mDroppedFlips;
            if ((dropped & 63) == 1) {
                LOG(INFO) << "display surface " << mName << " dropped " << dropped
                          << " flip(s) so far: no free display buffer (panel-paced)";
            }
            return -1;
        }
        if (result != 0) {
            if (fenceFd >= 0) close(fenceFd);
            if (clearIfSurfaceUnavailableLocked("dequeueBuffer", result)) return -1;
            return Error() << "Failed to dequeue display buffer: " << result;
        }
        if (buffer == nullptr) {
            if (fenceFd >= 0) close(fenceFd);
            return Error() << "Failed to dequeue display buffer: null buffer";
        }

        // When the bridge can import the acquire fence as a GPU wait semaphore, hand the fence to
        // blit() (which takes ownership) instead of blocking the display thread on a CPU poll.
        int acquireFenceFd = -1;
        if (bridge.canImportAcquireFence()) {
            acquireFenceFd = fenceFd;
            fenceFd = -1;
        } else if (auto ret = waitForFence(fenceFd); !ret.ok()) {
            fenceFd = -1; // waitForFence closed it
            ANativeWindow_cancelBuffer(anw, buffer, -1);
            return Error() << ret.error();
        } else {
            fenceFd = -1; // waitForFence closed it
        }

        AHardwareBuffer* targetAhb = ANativeWindowBuffer_getHardwareBuffer(buffer);
        if (targetAhb == nullptr) {
            if (acquireFenceFd >= 0) {
                if (auto ret = waitForFence(acquireFenceFd); !ret.ok()) {
                    ANativeWindow_cancelBuffer(anw, buffer, -1);
                    return ret.error();
                }
            }
            ANativeWindow_cancelBuffer(anw, buffer, -1);
            return Error() << "Failed to get display target AHardwareBuffer";
        }
        auto completion = bridge.blit(importId, targetAhb, acquireFenceFd, allowFaultInjection);
        if (!completion.ok()) {
            // blit() owns acquireFenceFd on every return path, so do not close it here.
            ANativeWindow_cancelBuffer(anw, buffer, -1);
            return Error() << "Failed to blit into dequeued display buffer: " << completion.error();
        }
        int targetCompletionFd = *completion;
        int sourceCompletionFd = -1;
        if (targetCompletionFd >= 0) {
            sourceCompletionFd = dup(targetCompletionFd);
            if (sourceCompletionFd < 0) {
                PLOG(WARNING) << "Failed to duplicate display completion sync_fd; draining";
                if (auto ret = waitForFence(targetCompletionFd); !ret.ok()) {
                    ANativeWindow_cancelBuffer(anw, buffer, -1);
                    return ret.error();
                }
                targetCompletionFd = -1;
            }
        }
        const bool injectQueueFailure =
                allowFaultInjection && injectRuntimeFlipFailure(RuntimeFlipFailureStage::kQueue);
        bool queueCalled = false;
        if (injectQueueFailure) {
            result = -EIO;
        } else {
            ScopedTrace queueTrace("crosvm_display.queue_buffer");
            result = ANativeWindow_queueBuffer(anw, buffer, targetCompletionFd);
            queueCalled = true;
        }
        if (result != 0) {
            // The duplicate guarantees the target is no longer being written before it is
            // cancelled back to BufferQueue.
            if (sourceCompletionFd >= 0) {
                auto waitResult = waitForFence(sourceCompletionFd);
                sourceCompletionFd = -1;
                if (!waitResult.ok()) {
                    if (!queueCalled && targetCompletionFd >= 0) close(targetCompletionFd);
                    ANativeWindow_cancelBuffer(anw, buffer, -1);
                    return waitResult.error();
                }
            }
            // A real queueBuffer call consumes the completion fd even on error. The injected
            // failure skips that call, so close our still-owned fd after its duplicate signals.
            if (!queueCalled && targetCompletionFd >= 0) close(targetCompletionFd);
            ANativeWindow_cancelBuffer(anw, buffer, -1);
            if (!injectQueueFailure && clearIfSurfaceUnavailableLocked("queueBuffer", result)) {
                return -1;
            }
            return Error() << (injectQueueFailure ? "Injected display queue failure: "
                                                  : "Failed to queue display buffer: ")
                           << result;
        }
        const uint64_t presented = ++mPresentedFlips;
        if ((presented & 255) == 0) {
            LOG(INFO) << "display surface " << mName << " presented " << presented
                      << " flip(s), dropped=" << mDroppedFlips;
        }
        return sourceCompletionFd;
    }

    Result<void> unlockAndPost() {
        std::unique_lock lk(mSurfaceMutex);

        if (mPreRotate) {
            if (!mCursorImage.publish()) return Error() << "Cursor image was not locked";
            if (frameHashEnabled()) {
                ANativeWindow_Buffer image{
                        .width = static_cast<int32_t>(mCursorImage.width()),
                        .height = static_cast<int32_t>(mCursorImage.height()),
                        .stride = static_cast<int32_t>(mCursorImage.width()),
                        .format = kFormat,
                        .bits = const_cast<uint32_t*>(mCursorImage.pixels()),
                };
                logFrameHash(mName, image);
            }
            return drawCursorFrameLocked();
        }

        if (!mCpuProducerLocked) return Error() << "CPU image was not locked";
        mCpuProducerLocked = false;
        auto surface = std::move(mCpuLockedSurface);
        const int disconnectApi = std::exchange(mCpuDeferredDisconnectApi, 0);
        const auto buffer = std::exchange(mLastBuffer, ANativeWindow_Buffer{});
        if (!surface) {
            // The frame was written into the owned sink while detached. An
            // Activity attached meanwhile does not own this lock.
            return {};
        }

        ANativeWindow* anw = surface->get();
        if (anw == nullptr) {
            return Error() << "Failed to get ANativeWindow";
        }

        // The CPU edge has already copied into the RGBA layout declared by this sink, so this hash
        // is also exactly what SurfaceFlinger receives. See frameHashEnabled().
        if (frameHashEnabled()) logFrameHash(mName, buffer);

        // The descriptor is borrowed only between lock and unlockAndPost.
        // Android owns the pixels after this call, including on a post error.
        const int result = ANativeWindow_unlockAndPost(anw);
        // A detached producer must remain connected through the CPU write
        // and unlock, even when the consumer has abandoned its window.
        if (disconnectApi) {
            const int status = native_window_api_disconnect(anw, disconnectApi);
            if (status != 0) LOG(WARNING) << "Retired CPU window disconnect failed: " << status;
        }
        if (result != 0) {
            return Error() << "Failed to unlock and post window";
        }
        return {};
    }

    // Presents an already allocated host AHB without touching BufferQueue or
    // running a Vulkan blit. The returned release signal is the only point at
    // which the producer may write the same AHB again.
    Result<void> presentShared(AHardwareBuffer* ahb, int acquireFenceFd,
                               const SharedAllocationDescription* expected,
                               const std::shared_ptr<DirectReleaseSignal>& releaseSignal,
                               uint64_t expectedGeneration = 0,
                               uint64_t expectedModeGeneration = 0,
                               uint64_t expectedResetGeneration = 0) {
        std::lock_guard lk(mSurfaceMutex);
        // Checked under the same mutex as attach and actual submission: a
        // late refresh must never publish into a different Activity.
        if (expectedGeneration && expectedGeneration != mNativeSurfaceGeneration)
            return Error() << "Native refresh attachment changed";
        if (mBoundProfileGeneration) {
            refreshScanoutProfileLocked();
            if (!mScanoutGeometry || !mScanoutProfile.matches(*mScanoutGeometry,
                    mBoundEndpointGeneration, mBoundProfileGeneration))
                return Error() << "Native scanout preferred profile revoked";
        }
        if (mScanoutGeometry) {
            if (expectedModeGeneration != mScanoutGeometry->modeGeneration ||
                    expectedResetGeneration != mScanoutGeometry->resetGeneration)
                return Error() << "Native scanout mode or reset changed";
        } else if (expectedModeGeneration || expectedResetGeneration) {
            return Error() << "Native scanout geometry is not configured";
        }
        if (!ahb || !releaseSignal || releaseSignal->fd.get() < 0) {
            return Error() << "Invalid direct AHB presentation arguments";
        }
        AHardwareBuffer_Desc desc{};
        AHardwareBuffer_describe(ahb, &desc);
        uint64_t nativeBufferId = 0;
        // Optional diagnostic identity must not change presentation admission.
        (void)AHardwareBuffer_getId(ahb, &nativeBufferId);
        // Mapper allocation IDs and GraphicBuffer/AHB IDs are distinct namespaces.
        const uint64_t bufferId = expected ? expected->buffer_id : 0;
        android_display::nativeFifoTraceBuffer(bufferId, nativeBufferId);
        if (expected) {
            const uint32_t expectedFormat = expected->fourcc == android_display::kSharedAhbRgbaFourcc
                    ? AHARDWAREBUFFER_FORMAT_R8G8B8A8_UNORM
                    : expected->fourcc == android_display::kSharedAhbBgraFourcc
                    ? AHARDWAREBUFFER_FORMAT_B8G8R8A8_UNORM : 0;
            if (!expectedFormat || desc.width != expected->width || desc.height != expected->height ||
                desc.layers != 1 || desc.format != expectedFormat ||
                uint64_t(desc.stride) * 4 != expected->plane_stride) {
                return Error() << "Direct AHB description does not match the guest scanout";
            }
        }
        if (mScanoutGeometry &&
            (desc.width != mScanoutGeometry->storageWidth ||
             desc.height != mScanoutGeometry->storageHeight || desc.layers != 1)) {
            return Error() << "Direct AHB dimensions do not match native storage";
        }
        if (!mScanoutGeometry && mRequestedSurfaceDimensions &&
            (mRequestedSurfaceDimensions->width != desc.width ||
             mRequestedSurfaceDimensions->height != desc.height)) {
            return Error() << "Direct AHB dimensions do not match the configured surface";
        }
        if (!mNativeSurface) return Error() << "Native display surface is unavailable";
        if (!mDirectPresenter || mDirectPresenterGeneration != mNativeSurfaceGeneration) {
            mDirectPresenter.reset();
            mDirectPresenter = android_display::SurfaceBufferPresenter::create(
                    mNativeSurface->get(), mName.c_str(), mNativeSurfaceGeneration,
                    /* forcePreviousFence = */ false, /* opaque = */ true);
            mDirectPresenterGeneration = mNativeSurfaceGeneration;
            resetDisplayTimingLocked();
        }
        if (!mDirectPresenter) {
            return Error() << "SurfaceControl consumer-release APIs unavailable";
        }
        if (mScanoutGeometry && !mDirectPresenter->configureScanoutGeometry(*mScanoutGeometry))
            return Error() << "Direct presenter cannot apply native scanout geometry";
        // Each newly owned main layer needs its own vote. The presenter caches
        // submitted values, so steady-state frames add no rate transactions.
        (void)mDirectPresenter->setFrameRate(mRequestedFrameRate);
        auto ticket = mDirectPresenter->present(ahb, acquireFenceFd,
                expectedModeGeneration, expectedResetGeneration);
        if (!ticket) return Error() << "Direct AHB busy, invalid, or release registry full";
        mDirectPresentedGeneration = mNativeSurfaceGeneration;
        const uint64_t generation = ticket->generation, sequence = ticket->sequence;
        if (mDisplayTiming && sequence % 128 == 0) {
            (void)mDisplayTiming->snapshot(android_display::displayMonotonicNs());
            const auto timing = mDisplayTiming->diagnostics();
            LOG(INFO) << "DISPLAY-TIMING attachment=" << generation << " mode=" << mDisplayTimingEpoch
                      << " callbacks=" << timing.completedCallbacks << " signalled=" << timing.validSignals
                      << " duplicate_present=" << timing.duplicatePresents << " out_of_order=" << timing.outOfOrder
                      << " expired=" << timing.expired << " invalid=" << timing.invalid
                      << " unavailable_stats=" << timing.unavailableStats
                      << " invalidated_callback=" << timing.invalidatedCallbacks
                      << " malformed_callback=" << timing.malformedCallbacks
                      << " latch_after_arrival=" << timing.latchAfterArrival
                      << " invalid_fence=" << timing.invalidFence
                      << " present_before_latch=" << timing.presentBeforeLatch
                      << " present_after_observation=" << timing.presentAfterObservation
                      << " stale_present=" << timing.stalePresent
                      << " invalid_clock_order=" << timing.invalidClockOrder
                      << " clock_error=" << timing.clockErrors
                      << " old_sequence=" << timing.oldSequence
                      << " present_regression=" << timing.presentRegression
                      << " sorted_batch=" << timing.sortedBatches
                      << " signal_after_snapshot_start=" << timing.signalsAfterSnapshotStart
                      << " promoted=" << timing.promotedSamples
                      << " pending_fence=" << timing.pendingFences
                      << " awaiting_callback=" << timing.outstandingCallbacks
                      << " admission_drop=" << timing.admissionDrops << " enqueued_ns=" << timing.lastEnqueuedNs
                      << " latch_ns=" << timing.lastLatchNs << " present_ns=" << timing.lastPresentNs
                      << " max_enqueue_latch_ns=" << timing.maxEnqueueToLatchNs
                      << " max_latch_present_ns=" << timing.maxLatchToPresentNs;
        }
        logDirectAhb("accepted", gDisplayPathCounters.directAccepted.fetch_add(1) + 1,
                     bufferId, nativeBufferId, generation, sequence);
        android_display::traceRelease("accepted", generation, sequence, static_cast<int>(bufferId));
        ticket->setReleasedCallback([releaseSignal, bufferId, nativeBufferId, generation, sequence] {
            logDirectAhb("released", gDisplayPathCounters.directReleased.fetch_add(1) + 1,
                         bufferId, nativeBufferId, generation, sequence);
            releaseSignal->signal();
        });
        return {};
    }

    // Retained for the old C++ users. It still goes through release tracking,
    // unlike the historical raw setBuffer transaction.
    Result<void> setBuffer(AHardwareBuffer* ahb) {
        auto signal = std::make_shared<DirectReleaseSignal>();
        return presentShared(ahb, -1, nullptr, signal);
    }

    // Preserve the Binder ABI, but never snapshot a borrowed main buffer.
    // During lock the producer writes outside mSurfaceMutex; after post the
    // consumer owns its pixels. Neither interval admits a Binder-side read.
    // Native AHB frames likewise need a fresh guest read reservation to replay.
    Result<void> saveFrame() {
        std::unique_lock lk(mSurfaceMutex);
        if (mPreRotate) return {}; // The cursor cache already owns the complete image.
        return Error() << "Main frame snapshot unavailable; a fresh producer presentation is required";
    }

    // Only the separate, owned cursor cache may be replayed.
    Result<void> drawSavedFrame() {
        std::unique_lock lk(mSurfaceMutex);
        if (mPreRotate) return drawCursorFrameLocked();
        return Error() << "Main frame snapshot unavailable; a fresh producer presentation is required";
    }

    const std::string& name() const { return mName; }

private:
    void refreshScanoutProfileLocked() {
        const auto previous = mScanoutProfile.snapshot();
        mScanoutProfile.nativeSurface(mNativeSurfaceGeneration,
                !mPreRotate && mNativeSurface && mAttachmentLease.current() != 0);
        auto* window = mNativeSurface ? mNativeSurface->get() : nullptr;
        auto& api = SurfaceControl::GetInstance();
        mScanoutProfile.observe(mRequestedSurfaceDimensions ? mRequestedSurfaceDimensions->width : 0,
                mRequestedSurfaceDimensions ? mRequestedSurfaceDimensions->height : 0,
                window ? ANativeWindow_getWidth(window) : 0,
                window ? ANativeWindow_getHeight(window) : 0,
                !mCpuProducerLocked && api.HasBufferGeometry() && api.HasPreviousReleaseFence() &&
                        api.HasBufferTransparency());
        const auto current = mScanoutProfile.snapshot();
        if (previous.profile_generation != current.profile_generation ||
                previous.endpoint_generation != current.endpoint_generation || previous.flags != current.flags)
            signalNativeRefreshLocked();
    }

    void signalNativeRefreshLocked() {
        if (mPreRotate || mNativeRefreshEvent.get() < 0) return;
        const uint64_t one = 1;
        const auto result = TEMP_FAILURE_RETRY(write(mNativeRefreshEvent.get(), &one, sizeof(one)));
        if (result < 0 && errno != EAGAIN) PLOG(WARNING) << "native refresh eventfd write";
    }

    void clearNativeSurfaceLocked(bool disconnect) {
        if (mDisplayTiming) mDisplayTiming->invalidate();
        mDisplayTiming.reset();
        const bool hadSurface = mNativeSurface != nullptr;
        mAttachmentLease.clear();
        mAppliedBuffersTransform = 0;
        mCursorFramePosted = false;
        const bool retiringCpuWindow = mCpuLockedSurface && mNativeSurface &&
                mCpuLockedSurface->get() == mNativeSurface->get();
        if (retiringCpuWindow && disconnect && mConnectedNativeWindowApi)
            mCpuDeferredDisconnectApi = mConnectedNativeWindowApi;
        if (disconnect && !retiringCpuWindow) {
            disconnectNativeWindowLocked();
        } else {
            mConnectedNativeWindowApi = 0;
            mGpuUsageConfigured = false;
            mZeroDequeueTimeout = false;
            mFrameRateConfigured = false;
        }

        auto& sc = SurfaceControl::GetInstance();
        // Detach the direct SurfaceControl layer before dropping the native
        // window. Pending tickets remain owned by ConsumerRelease and keep
        // their AHardwareBuffer references until genuine consumer release.
        if (!android_display::SurfaceBufferPresenter::detachAndReset(mDirectPresenter)) {
            LOG(ERROR) << "display surface " << mName
                       << " failed to detach direct AHB presenter; retaining it quarantined";
            (void)mDirectPresenter.release();
        }
        mDirectPresenterGeneration = 0;
        if (mSurfaceControl) {
            if (sc.IsSupported()) sc.ASurfaceControl_release(mSurfaceControl);
            mSurfaceControl = nullptr;
        }
        mNativeSurface = nullptr;
        mNativeSurfaceNeedsConfiguring = true;
        mGpuUsageConfigured = false;
        mFrameRateConfigured = false;
        if (!mCpuProducerLocked) mLastBuffer = {};
        if (hadSurface) ++mNativeSurfaceGeneration;
        refreshScanoutProfileLocked();
    }

    void resetDisplayTimingLocked() {
        if (mDisplayTiming) mDisplayTiming->invalidate();
        mDisplayTiming.reset();
        // Explicit experimental gate; absent/old app/host pairs retain their
        // existing presentation cost and never advertise synchronization.
        static const bool enabled = envFlagEnabled("CROSVM_DISPLAY_TIMING", false);
        if (enabled && !mPreRotate && mNativeSurface && mDirectPresenter &&
            mDisplayTimingEpoch < UINT64_MAX && SurfaceControl::GetInstance().HasPresentTiming()) {
            mDisplayTiming = std::make_shared<android_display::DisplayTimingState>(
                    mNativeSurfaceGeneration, ++mDisplayTimingEpoch);
        }
        if (mDirectPresenter) mDirectPresenter->setTimingState(mDisplayTiming);
    }

    bool clearIfSurfaceUnavailableLocked(const char* operation, int status) {
        if (!isSurfaceUnavailableStatus(status)) return false;
        LOG(WARNING) << "display surface " << mName << " became unavailable during " << operation
                     << " (" << status
                     << "); keeping Vulkan path active and dropping frames until a replacement is "
                        "attached";
        // The producer is already disconnected or abandoned. Avoid another native-window call
        // while releasing our stale Surface reference.
        clearNativeSurfaceLocked(false);
        return true;
    }

    void disconnectNativeWindowLocked() {
        if (mConnectedNativeWindowApi == 0) return;
        Surface* surface = mNativeSurface.get();
        ANativeWindow* anw = surface ? surface->get() : nullptr;
        if (anw) {
            int result = native_window_api_disconnect(anw, mConnectedNativeWindowApi);
            if (result != 0) {
                LOG(WARNING) << "display surface " << mName
                             << " failed to disconnect native window API "
                             << mConnectedNativeWindowApi << ": " << result;
            }
        }
        mConnectedNativeWindowApi = 0;
        mGpuUsageConfigured = false;
        mZeroDequeueTimeout = false;
        mFrameRateConfigured = false;
    }

    static Result<void> waitForFence(int fenceFd) {
        ScopedTrace trace("crosvm_display.target_fence_wait");
        if (fenceFd < 0) return {};
        pollfd descriptor{
                .fd = fenceFd,
                .events = POLLIN,
                .revents = 0,
        };
        int result;
        do {
            result = poll(&descriptor, 1, -1);
        } while (result < 0 && errno == EINTR);
        close(fenceFd);
        if (result <= 0) return Error() << "Failed to wait for display acquire fence";
        return {};
    }

    Result<void> prepareNativeWindowLocked(ANativeWindow* anw, bool gpu) {
        const int requestedApi = gpu ? NATIVE_WINDOW_API_EGL : NATIVE_WINDOW_API_CPU;
        if (mConnectedNativeWindowApi != 0 && mConnectedNativeWindowApi != requestedApi) {
            int result = native_window_api_disconnect(anw, mConnectedNativeWindowApi);
            if (result != 0) {
                return Error() << "Failed to switch native window producer API: " << result;
            }
            mConnectedNativeWindowApi = 0;
            mGpuUsageConfigured = false;
            mNativeSurfaceNeedsConfiguring = true;
        }
        if (mConnectedNativeWindowApi == 0) {
            int result = native_window_api_connect(anw, requestedApi);
            if (result != 0) return Error() << "Failed to connect native window: " << result;
            mConnectedNativeWindowApi = requestedApi;
        }

        // SurfaceFlinger otherwise assigns this SurfaceView the default UI vote, which may be
        // substantially below the guest mode.  AT_LEAST is the correct contract for an
        // interactive desktop: request the guest rate, but let the display scheduler select the
        // fastest physical mode it actually supports (for example 120 Hz for a 165 Hz guest).
        // This is a scheduling hint only; it does not throttle production or force a non-seamless
        // panel switch.
        if (gpu && !mFrameRateConfigured && mRequestedFrameRate > 0) {
            const int result = native_window_set_frame_rate(
                    anw, static_cast<float>(mRequestedFrameRate),
                    ANATIVEWINDOW_FRAME_RATE_COMPATIBILITY_AT_LEAST,
                    ANATIVEWINDOW_CHANGE_FRAME_RATE_ONLY_IF_SEAMLESS);
            if (result == 0) {
                LOG(INFO) << "display surface " << mName << " frame-rate vote "
                          << mRequestedFrameRate << " Hz (native-window, at-least, seamless)";
            } else if ((result == -ENOSYS || result == -ENOTSUP || result == -EINVAL) &&
                       applyFrameRateTransactionLocked()) {
                LOG(INFO) << "display surface " << mName << " frame-rate vote "
                          << mRequestedFrameRate
                          << " Hz (surface-control transaction, at-least, seamless)";
            } else {
                LOG(WARNING) << "display surface " << mName << " set frame-rate vote "
                             << mRequestedFrameRate << " Hz failed: " << result;
            }
            // A permanent vendor refusal should not turn into one warning per guest frame. A new
            // Surface or a changed guest mode clears this and retries against the new window.
            mFrameRateConfigured = true;
        }

        // The GPU (blit) producer must never block in dequeueBuffer. With the default timeout
        // (-1) a flip stalls until SurfaceFlinger releases a buffer at its next vsync, and since
        // the guest's RESOURCE_FLUSH fence is deferred to this blit's completion, the whole guest
        // compositor ends up paced by the phone panel: on a 60 Hz ColorOS panel venus/drm2kgsl
        // ran at 70-800 fps with the display attached and 4000-9000 fps without it. A timeout of
        // A short timeout keeps the guest from blocking behind a full BufferQueue while allowing
        // a just-released panel buffer to be reused.  Zero caused avoidable periodic drops when
        // SurfaceFlinger released a buffer a few hundred microseconds after the dequeue call.
        // The CPU-copy producer keeps the blocking default so ANativeWindow_lock behaves as before.
        const bool wantZeroTimeout = gpu;
        if (mZeroDequeueTimeout != wantZeroTimeout) {
            // NATIVE_WINDOW_SET_DEQUEUE_TIMEOUT is expressed in nanoseconds.  Bound the wait to
            // 2ms, below one 165Hz refresh interval, so it cannot pace the guest to the panel.
            const int64_t timeout = wantZeroTimeout ? 2'000'000 : -1;
            int result = anw->perform(anw, NATIVE_WINDOW_SET_DEQUEUE_TIMEOUT, timeout);
            if (result != 0) {
                LOG(WARNING) << "display surface " << mName << " set dequeue timeout " << timeout
                             << " failed: " << result;
            } else {
                mZeroDequeueTimeout = wantZeroTimeout;
            }
        }

        if (!mRequestedSurfaceDimensions) {
            return Error() << "Surface dimension is not configured yet!";
        }
        const auto& dims = *mRequestedSurfaceDimensions;
        if (mNativeSurfaceNeedsConfiguring) {
            LOG(INFO) << "display surface " << mName << " set buffer geometry " << dims.width << "x"
                      << dims.height;
            if (ANativeWindow_setBuffersGeometry(anw, dims.width, dims.height, kFormat) != 0) {
                return Error() << "Failed to set buffer geometry.";
            }
            mNativeSurfaceNeedsConfiguring = false;
        }

        if (gpu && !mGpuUsageConfigured) {
            constexpr uint64_t kGpuUsage = AHARDWAREBUFFER_USAGE_GPU_SAMPLED_IMAGE |
                    AHARDWAREBUFFER_USAGE_GPU_FRAMEBUFFER | AHARDWAREBUFFER_USAGE_COMPOSER_OVERLAY;
            if (ANativeWindow_setUsage(anw, kGpuUsage) != 0) {
                return Error() << "Failed to set GPU buffer usage";
            }
            mGpuUsageConfigured = true;
        } else if (!gpu) {
            mGpuUsageConfigured = false;
        }
        return {};
    }

    // The hardware cursor is pre-rotated by the window's transform hint, the way libvulkan's
    // swapchain handles preTransform: the content is rotated by the hint and the buffer transform
    // is set to its inverse, so the composer's net transform is identity. The panel is portrait
    // and the display landscape, so without this the cursor layer carries ROT_90; the display
    // engine rotates only UBWC buffers, and a CPU-written cursor is linear. That sent the cursor
    // -- and with it the full-screen scanout -- to GPU composition on every pointer move.
    // Square surfaces only, so a rotation never changes the buffer geometry.
    int preRotationHintLocked(ANativeWindow* anw) {
        if (!mPreRotate || !mRequestedSurfaceDimensions ||
            mRequestedSurfaceDimensions->width != mRequestedSurfaceDimensions->height) {
            return 0;
        }
        int hint = 0;
        (void)anw->query(anw, NATIVE_WINDOW_TRANSFORM_HINT, &hint);
        return mCursorTransformHint.resolve(hint);
    }

    static int inverseRotation(int rotation) {
        switch (rotation) {
            case NATIVE_WINDOW_TRANSFORM_ROT_90:
                return NATIVE_WINDOW_TRANSFORM_ROT_270;
            case NATIVE_WINDOW_TRANSFORM_ROT_270:
                return NATIVE_WINDOW_TRANSFORM_ROT_90;
            default:
                return rotation;
        }
    }

    Result<void> setBuffersTransformLocked(ANativeWindow* anw, int transform) {
        if (mAppliedBuffersTransform == transform) return {};
        if (int result = native_window_set_buffers_transform(anw, transform); result != 0) {
            return Error() << "Failed to set buffers transform " << transform << ": " << result;
        }
        mAppliedBuffersTransform = transform;
        return {};
    }

    Result<void> drawCursorFrameLocked() {
        if (!mNativeSurface || !mCursorImage.valid()) return {};
        ANativeWindow* anw = mNativeSurface->get();
        if (!anw) return Error() << "Cursor Surface not ready";
        if (auto ret = prepareNativeWindowLocked(anw, false); !ret.ok()) return ret;
        const int hint = preRotationHintLocked(anw);
        if (mCursorFramePosted && mCursorPostedRevision == mCursorImage.revision() &&
            mAppliedBuffersTransform == inverseRotation(hint)) return {};
        if (auto ret = setBuffersTransformLocked(anw, inverseRotation(hint)); !ret.ok()) {
            return ret;
        }
        ANativeWindow_Buffer buf;
        if (ANativeWindow_lock(anw, &buf, nullptr) != 0) {
            return Error() << "Failed to lock window";
        }
        if (buf.width != static_cast<int32_t>(mCursorImage.width()) ||
            buf.height != static_cast<int32_t>(mCursorImage.height())) {
            ANativeWindow_unlockAndPost(anw);
            return Error() << "pre-rotation dimension mismatch";
        }
        const unsigned turns = hint == NATIVE_WINDOW_TRANSFORM_ROT_90 ? 1 :
                hint == NATIVE_WINDOW_TRANSFORM_ROT_180 ? 2 :
                hint == NATIVE_WINDOW_TRANSFORM_ROT_270 ? 3 : 0;
        if (!mCursorImage.copyTo(static_cast<uint32_t*>(buf.bits), buf.stride, turns)) {
            ANativeWindow_unlockAndPost(anw);
            return Error() << "Invalid cursor buffer geometry";
        }
        gDisplayPathCounters.cursorBufferCopyBytes.fetch_add(
                static_cast<uint64_t>(buf.width) * buf.height * 4, std::memory_order_relaxed);
        if (ANativeWindow_unlockAndPost(anw) != 0) {
            return Error() << "Failed to unlock and post window";
        }
        mCursorFramePosted = true;
        mCursorPostedRevision = mCursorImage.revision();
        return {};
    }

    bool applyFrameRateTransactionLocked() {
        auto& sc = SurfaceControl::GetInstance();
        if (!sc.HasFrameRate() || !mSurfaceControl) return false;
        auto transaction = sc.ASurfaceTransaction_create();
        if (!transaction) return false;
        sc.ASurfaceTransaction_setFrameRateWithChangeStrategy(
                transaction, mSurfaceControl, static_cast<float>(mRequestedFrameRate),
                ANATIVEWINDOW_FRAME_RATE_COMPATIBILITY_AT_LEAST,
                ANATIVEWINDOW_CHANGE_FRAME_RATE_ONLY_IF_SEAMLESS);
        sc.ASurfaceTransaction_apply(transaction);
        sc.ASurfaceTransaction_delete(transaction);
        return true;
    }

    // Keep the Android buffer RGBA_8888 (not BGRA_8888) so it imports as a regular GL_TEXTURE_2D.
    // A BGRA buffer can become GL_TEXTURE_EXTERNAL_OES on some Adreno devices, whose Skia
    // RenderEngine then aborts in makeImage ("Unable to generate SkImage"), crashing
    // surfaceflinger. crosvm now declares this RGBA layout as the CPU sink format and performs any
    // required R/B exchange while copying into the buffer; no post-process is needed here. RGBA
    // also preserves the alpha channel used for cursor blending.
    static constexpr const int kFormat = HAL_PIXEL_FORMAT_RGBA_8888;

    std::string mName;
    // Whether this surface pre-rotates its CPU frames; see preRotationHintLocked().
    const bool mPreRotate;
    android_display::CursorTransformHint mCursorTransformHint;
    android_display::CursorImageCache mCursorImage;
    uint64_t mCursorPostedRevision = 0;
    bool mCursorFramePosted = false;
    int mAppliedBuffersTransform = 0;

    std::mutex mSurfaceMutex;
    android_display::SurfaceAttachmentLease mAttachmentLease;
    std::unique_ptr<Surface> mNativeSurface;
    std::unique_ptr<android_display::SurfaceBufferPresenter> mDirectPresenter;
    uint64_t mDirectPresenterGeneration = 0;
    uint64_t mDirectPresentedGeneration = 0;
    uint64_t mDisplayTimingEpoch = 0;
    std::shared_ptr<android_display::DisplayTimingState> mDisplayTiming;
    android::base::unique_fd mNativeRefreshEvent;
    ASurfaceControl* mSurfaceControl = nullptr;
    std::condition_variable mNativeSurfaceReady;
    bool mNativeSurfaceNeedsConfiguring = true;
    int mConnectedNativeWindowApi = 0;
    bool mGpuUsageConfigured = false;
    uint32_t mRequestedFrameRate = 0;
    bool mFrameRateConfigured = false;
    // Whether the connected producer currently runs with a zero dequeue timeout (GPU path).
    bool mZeroDequeueTimeout = false;
    // Flips dropped because no display buffer was free at the time (see flip()).
    uint64_t mDroppedFlips = 0;
    // Successful BufferQueue submissions. Both counters are protected by mSurfaceMutex.
    uint64_t mPresentedFlips = 0;
    uint64_t mNativeSurfaceGeneration = 0;
    uint64_t mVulkanTargetGeneration = 0;

    // Buffer which crosvm uses when in background. This is just to not fail crosvm even when
    // Android-side Surface doesn't exist. The content drawn here is never displayed on the physical
    // screen.
    SinkANativeWindow_Buffer mSinkBuffer;

    // Borrowed CPU descriptor used only for optional hashing before post.
    ANativeWindow_Buffer mLastBuffer{};
    bool mCpuProducerLocked = false;
    // A Binder detach may retire the current window while Rust still writes
    // its locked pixels. Release/disconnect only after that lock is posted.
    std::unique_ptr<Surface> mCpuLockedSurface;
    int mCpuDeferredDisconnectApi = 0;
    std::optional<android_display::ScanoutPresentationGeometry> mScanoutGeometry;
    bool mNativeScanoutOnly = false;
    uint64_t mNativeResetFloor = 0;
    android_display::ScanoutProfileState mScanoutProfile;
    uint64_t mBoundEndpointGeneration = 0, mBoundProfileGeneration = 0;

    struct Rect {
        uint32_t width = 0;
        uint32_t height = 0;
    };
    std::optional<Rect> mRequestedSurfaceDimensions;

public:
    // Current scanout size (0x0 until the first configure()). Read by getDisplayConfig so the app
    // can follow guest resolution changes; configure() is the single writer for every cause.
    Rect dimensions() {
        std::lock_guard lk(mSurfaceMutex);
        return mRequestedSurfaceDimensions.value_or(Rect{});
    }
};

class DisplayService : public BnCrosvmAndroidDisplayService {
public:
    DisplayService() = default;
    virtual ~DisplayService() = default;

    ndk::ScopedAStatus setSurface(Surface* surface, bool forCursor) override {
        if (auto ret = getSurface(forCursor).setNativeSurface(*surface); !ret.ok()) {
            return ndk::ScopedAStatus::fromServiceSpecificErrorWithMessage(
                    EINVAL, ret.error().message().c_str());
        }
        return ::ndk::ScopedAStatus::ok();
    }

    ndk::ScopedAStatus setSurfaceWithLease(const Surface& surface, bool forCursor,
                                          int64_t* outLease) override {
        *outLease = 0;
        auto ret = getSurface(forCursor).setNativeSurface(surface, true);
        if (!ret.ok()) return ndk::ScopedAStatus::fromServiceSpecificErrorWithMessage(
                EINVAL, ret.error().message().c_str());
        *outLease = *ret;
        return ndk::ScopedAStatus::ok();
    }

    ndk::ScopedAStatus removeSurfaceWithLease(bool forCursor, int64_t lease) override {
        if (lease <= 0) return ndk::ScopedAStatus::fromServiceSpecificError(EINVAL);
        getSurface(forCursor).removeSurface(static_cast<uint64_t>(lease));
        return ndk::ScopedAStatus::ok();
    }

    ndk::ScopedAStatus setCursorTransformHintWithLease(int32_t hint, int64_t lease) override {
        if (lease <= 0) return ndk::ScopedAStatus::fromServiceSpecificError(EINVAL);
        if (auto ret = mCursor.setCursorTransformHint(hint, static_cast<uint64_t>(lease)); !ret.ok()) {
            return ndk::ScopedAStatus::fromServiceSpecificErrorWithMessage(
                    EINVAL, ret.error().message().c_str());
        }
        return ndk::ScopedAStatus::ok();
    }

    ndk::ScopedAStatus setMainTransformHintWithLease(int32_t hint, int64_t lease,
            int64_t rootGeneration, int64_t serial) override {
        if (lease <= 0 || rootGeneration <= 0 || serial <= 0)
            return ndk::ScopedAStatus::fromServiceSpecificError(EINVAL);
        auto ret = getSurface(false).setMainTransformHint(hint, static_cast<uint64_t>(lease),
                static_cast<uint64_t>(rootGeneration), static_cast<uint64_t>(serial));
        if (!ret.ok()) return ndk::ScopedAStatus::fromServiceSpecificErrorWithMessage(
                EINVAL, ret.error().message().c_str());
        return ndk::ScopedAStatus::ok();
    }

    ndk::ScopedAStatus removeSurface(bool forCursor) override {
        getSurface(forCursor).removeSurface();
        return ::ndk::ScopedAStatus::ok();
    }

    ndk::ScopedAStatus setCursorStream(const ndk::ScopedFileDescriptor& in_stream) {
        if (int error = mCursorPosition.attach(in_stream.get()); error != 0) {
            return ndk::ScopedAStatus::fromServiceSpecificErrorWithMessage(error,
                    "Failed to attach/replay cursor position stream");
        }
        return ::ndk::ScopedAStatus::ok();
    }

    void setCursorPosition(uint32_t x, uint32_t y) {
        if (int error = mCursorPosition.update(x, y); error != 0 && error != EPIPE) {
            LOG(WARNING) << "cursor position stream write failed: " << strerror(error);
        }
    }

    ndk::ScopedAStatus setCursorTransformHint(int32_t hint) override {
        if (auto ret = mCursor.setCursorTransformHint(hint); !ret.ok()) {
            return ndk::ScopedAStatus::fromServiceSpecificErrorWithMessage(
                    EINVAL, ret.error().message().c_str());
        }
        return ndk::ScopedAStatus::ok();
    }

    ndk::ScopedAStatus saveFrameForSurface(bool forCursor) override {
        if (auto ret = getSurface(forCursor).saveFrame(); !ret.ok()) {
            std::string msg = std::format("Failed to save frame: {}", ret.error().message());
            return ::ndk::ScopedAStatus(
                    AStatus_fromServiceSpecificErrorWithMessage(-1, msg.c_str()));
        }
        return ::ndk::ScopedAStatus::ok();
    }

    ndk::ScopedAStatus drawSavedFrameForSurface(bool forCursor) override {
        if (auto ret = getSurface(forCursor).drawSavedFrame(); !ret.ok()) {
            std::string msg = std::format("Failed to draw saved frame: {}", ret.error().message());
            return ::ndk::ScopedAStatus(
                    AStatus_fromServiceSpecificErrorWithMessage(-1, msg.c_str()));
        }
        return ::ndk::ScopedAStatus::ok();
    }

    ndk::ScopedAStatus getDisplayConfig(
            ::aidl::android::crosvm::DisplayConfig* _aidl_return) override {
        auto dims = mScanout.dimensions();
        _aidl_return->width = static_cast<int32_t>(dims.width);
        _aidl_return->height = static_cast<int32_t>(dims.height);
        // dpi/refreshRate aren't tracked here; the app only consumes width/height for letterboxing.
        _aidl_return->dpi = 0;
        _aidl_return->refreshRate = 0;
        return ::ndk::ScopedAStatus::ok();
    }

    AndroidDisplaySurface& getSurface(bool forCursor) {
        if (forCursor) {
            return mCursor;
        } else {
            return mScanout;
        }
    }

private:
    AndroidDisplaySurface mScanout{"scanout"};
    AndroidDisplaySurface mCursor{"cursor"};
    android_display::CursorPositionChannel mCursorPosition;
};

} // namespace

typedef void (*ErrorCallback)(const char* message);

struct AndroidDisplayContext {
    enum class DisplayPath {
        kUnknown,
        kVulkanBlit,
        kCpu,
    };

    std::shared_ptr<DisplayService> disp_service;
    ErrorCallback error_callback;
    DisplayPath display_path = DisplayPath::kUnknown;
    std::unique_ptr<VulkanDisplayBridge> vulkan_display;
    // The display blit is latency-coupled to the guest present fence. Keep the
    // Adreno governor from dropping below the measured floor while frames flow,
    // then restore the previous policy after the display goes idle.
    gpu_clock_floor::Floor gpu_floor{[] {
        auto config = gpu_clock_floor::configFromValue(
                std::getenv("CROSVM_ANDROID_DISPLAY_GPU_MIN_FREQ"));
        config.log = [](const std::string& message) { LOG(INFO) << "Android display " << message; };
        return config;
    }()};
    // The guest's frame chain crosses vCPU and crosvm threads several times per frame; the host
    // CPU governor dropping between frames costs whole vblanks. See cpuPolicyConfigs().
    std::vector<std::unique_ptr<gpu_clock_floor::Floor>> cpu_floors = [] {
        std::vector<std::unique_ptr<gpu_clock_floor::Floor>> floors;
        for (auto& config : gpu_clock_floor::cpuPolicyConfigs(
                     std::getenv("CROSVM_ANDROID_DISPLAY_CPU_MIN_FREQ"))) {
            config.log = [](const std::string& message) {
                LOG(INFO) << "Android display " << message;
            };
            floors.push_back(std::make_unique<gpu_clock_floor::Floor>(std::move(config)));
        }
        return floors;
    }();
    // Keeps Qualcomm core_ctl from pausing cores while the VM display exists. See CoreCtlHold.
    gpu_clock_floor::CoreCtlHold core_ctl_hold{
            std::getenv("CROSVM_ANDROID_DISPLAY_CORE_CTL"),
            [](const std::string& message) { LOG(INFO) << "Android display " << message; }};

    void onPresentedFrame() {
        gpu_floor.onFrame();
        for (auto& floor : cpu_floors) floor->onFrame();
        core_ctl_hold.reassert();
    }

    AndroidDisplayContext(const char* service_name, ErrorCallback cb) : error_callback(cb) {
        if (!envFlagEnabled("CROSVM_ANDROID_DISPLAY_VULKAN_BLIT", true)) {
            display_path = DisplayPath::kCpu;
            LOG(INFO) << "Android display path forced to CPU by feature flag";
        } else {
            LOG(INFO) << "Android display path is unprobed";
        }
        auto disp_service = ::ndk::SharedRefBase::make<DisplayService>();

        // Register the DisplayService directly to the service manager under `service_name`
        // (passed from crosvm's --android-display-service argument). This works because crosvm
        // here is launched in a privileged (root) context with SELinux permissive, so it is
        // allowed to register a service. The app side looks the service up by the same name.
        //
        // The binder thread pool MUST be started before addService so that incoming calls
        // (e.g. setSurface) from the app are serviced.
        ABinderProcess_setThreadPoolMaxThreadCount(4);
        ABinderProcess_startThreadPool();

        binder_status_t status =
                AServiceManager_addService(disp_service->asBinder().get(), service_name);
        if (status != STATUS_OK) {
            errorf("Failed to register '%s' to service manager: status=%d", service_name, status);
            return;
        }

        this->disp_service = disp_service;
    }

    ~AndroidDisplayContext() = default;

    VulkanDisplayBridge* getVulkanDisplay() {
        if (display_path == DisplayPath::kCpu) return nullptr;
        if (display_path == DisplayPath::kUnknown) {
            vulkan_display = std::make_unique<VulkanDisplayBridge>();
            if (!vulkan_display->ready()) {
                display_path = DisplayPath::kCpu;
                LOG(WARNING) << "Android display M1 probe failed; using CPU for this process";
                return nullptr;
            }
            display_path = DisplayPath::kVulkanBlit;
            LOG(INFO) << "Android display M1 probe succeeded; using Vulkan blit";
        }
        return vulkan_display.get();
    }

    bool vulkanDisplayActive() const { return display_path == DisplayPath::kVulkanBlit; }

    void fallbackToCpu(const char* reason) {
        if (display_path == DisplayPath::kCpu) return;
        // A dequeue/fence/import failure can be transient while SurfaceFlinger replaces a
        // BufferQueue or the VM changes scanout resources.  Permanently selecting CPU here
        // turns one missed frame into a process-lifetime readback path and makes recovery
        // impossible.  Drop all Vulkan state, then let the next frame reprobe the bridge.
        display_path = DisplayPath::kUnknown;
        LOG(WARNING) << "Android display Vulkan path reset after transient failure: " << reason;
        // Rust resource states release imports lazily as those resources are revisited. Destroy
        // the bridge now so inactive source imports, cached AHB targets, and in-flight sync
        // objects cannot survive into the retry.
        vulkan_display.reset();
    }

    void errorf(const char* format, ...) {
        char buffer[1024];

        va_list vararg;
        va_start(vararg, format);
        vsnprintf(buffer, sizeof(buffer), format, vararg);
        va_end(vararg);

        error_callback(buffer);
    }
};

extern "C" uint64_t android_display_native_fifo_trace_begin(uint32_t command, uint32_t resource,
        uint64_t guestSequence, uint64_t fence, uint32_t context, uint32_t ring,
        uint32_t flags, uint32_t descriptor) {
    return android_display::nativeFifoTraceBegin(command, resource, guestSequence, fence,
                                                context, ring, flags, descriptor);
}
extern "C" void android_display_native_fifo_trace_operation(uint64_t sequence) {
    android_display::nativeFifoTraceOperation(sequence);
}
extern "C" void android_display_native_fifo_trace_end(uint64_t id) {
    android_display::nativeFifoTraceEnd(id);
}

extern "C" struct AndroidDisplayContext* create_android_display_context(
        const char* name, ErrorCallback error_callback) {
    return new AndroidDisplayContext(name, error_callback);
}

extern "C" void destroy_android_display_context(struct AndroidDisplayContext* ctx) {
    delete ctx;
}

extern "C" AndroidDisplaySurface* create_android_surface(struct AndroidDisplayContext* ctx,
                                                         uint32_t width, uint32_t height,
                                                         bool forCursor) {
    if (!ctx) return nullptr;
    if (ctx->disp_service == nullptr) {
        ctx->errorf("Display service was not created");
        return nullptr;
    }

    AndroidDisplaySurface& surface = ctx->disp_service->getSurface(forCursor);
    if (auto ret = surface.prepareLogicalSurface(width, height); !ret.ok()) {
        ctx->errorf("Failed to configure surface %s: %s", surface.name().c_str(),
                    ret.error().message().c_str());
        return nullptr;
    }

    // DisplayService owns this wrapper for its entire lifetime; the Rust
    // surface keeps the context alive. Guest release retires only its direct
    // presenter, while app attachment/removal owns the native window.
    return &surface;
}

extern "C" void destroy_android_surface(struct AndroidDisplayContext* ctx,
                                          AndroidDisplaySurface* surface) {
    if (!ctx || !surface) return;
    surface->releaseGuestSurface();
}

extern "C" bool request_android_surface_detach(struct AndroidDisplayContext* ctx,
                                                AndroidDisplaySurface* surface) {
    if (!ctx || !surface) return false;
    return surface->requestGuestSurfaceDetach();
}

extern "C" bool get_android_surface_buffer(struct AndroidDisplayContext* ctx,
                                           AndroidDisplaySurface* surface,
                                           ANativeWindow_Buffer* out_buffer) {
    if (out_buffer == nullptr) {
        ctx->errorf("out_buffer is null");
        return false;
    }

    if (surface == nullptr) {
        ctx->errorf("Invalid AndroidDisplaySurface provided");
        return false;
    }

    auto ret = surface->lock(out_buffer);
    if (!ret.ok()) {
        ctx->errorf("Failed to lock surface %s: %s", surface->name().c_str(),
                    ret.error().message().c_str());
        return false;
    }

    return true;
}

extern "C" void set_android_surface_position(struct AndroidDisplayContext* ctx, uint32_t x,
                                             uint32_t y) {
    if (ctx->disp_service == nullptr) {
        ctx->errorf("Display service was not created");
        return;
    }
    ctx->disp_service->setCursorPosition(x, y);
}

extern "C" void post_android_surface_buffer(struct AndroidDisplayContext* ctx,
                                            AndroidDisplaySurface* surface) {
    if (surface == nullptr) {
        ctx->errorf("Invalid AndroidDisplaySurface provided");
        return;
    }

    auto ret = surface->unlockAndPost();
    if (!ret.ok()) {
        ctx->errorf("Failed to unlock and post for surface %s: %s", surface->name().c_str(),
                    ret.error().message().c_str());
    }
    return;
}

extern "C" void set_android_surface_buffer_format(struct AndroidDisplayContext* ctx,
                                                  AndroidDisplaySurface* surface, uint32_t fourcc) {
    if (ctx == nullptr || surface == nullptr) {
        return;
    }
    // The app-side ANativeWindow is configured RGBA_8888 only (see configure()); until buffer
    // reallocation plumbing exists app-side, the guest's scanout fourcc is diagnostic only.
    static std::atomic<uint32_t> lastFourcc{0};
    if (lastFourcc.exchange(fourcc, std::memory_order_relaxed) != fourcc) {
        LOG(INFO) << "CROSVM_DISPLAY_FOURCC 0x" << std::hex << fourcc << std::dec
                  << " for surface " << surface->name() << " (format switch not implemented)";
    }
}

extern "C" void set_android_surface_frame_rate(struct AndroidDisplayContext* ctx,
                                                AndroidDisplaySurface* surface,
                                                uint32_t refreshRate) {
    if (ctx == nullptr || surface == nullptr) return;
    surface->setFrameRate(refreshRate);
}

extern "C" int64_t android_display_import_dmabuf(struct AndroidDisplayContext* ctx,
                                                  AndroidDisplaySurface* surface, int fd,
                                                 uint32_t offset, uint32_t stride,
                                                 uint64_t modifier, bool linearLayoutVerified,
                                                 uint32_t width, uint32_t height, uint32_t fourcc) {
    if (!ctx || !surface) return 0;
    static bool loggedImportLayout = false;
    if (!loggedImportLayout) {
        loggedImportLayout = true;
        LOG(INFO) << "CROSVM_DISPLAY_IMPORT fd=" << fd << " offset=0x" << std::hex << offset
                  << " stride=0x" << stride << " modifier=0x" << modifier << " fourcc=0x" << fourcc
                  << std::dec << " linear_verified=" << linearLayoutVerified << " size=" << width
                  << "x" << height;
    }
    VulkanDisplayBridge* bridge = ctx->getVulkanDisplay();
    if (!bridge) return 0;
    int64_t importId = bridge->importDmabuf(fd, offset, stride, modifier, linearLayoutVerified,
                                            width, height, fourcc);
    if (!importId) {
        ctx->errorf("Failed to import display dmabuf (%ux%u stride=%u fourcc=0x%x)", width, height,
                    stride, fourcc);
        return 0;
    }
    return importId;
}

extern "C" bool android_display_is_vulkan_blit_available(struct AndroidDisplayContext* ctx) {
    return ctx && ctx->getVulkanDisplay() != nullptr;
}

// Whether a frame posted now can reach a screen: the app attached a Surface to the scanout and has
// not taken it back. Leaving the display view destroys the SurfaceView, and the removeSurface(false)
// that follows drops the native window here; the same clearing happens when a producer is abandoned
// under us (clearIfSurfaceUnavailableLocked). While there is none, lock() hands out the sink buffer
// and unlockAndPost() returns without posting, so the frame is built and thrown away.
//
// Only the scanout counts. The cursor surface is a separate SurfaceView on its own lifecycle and may
// never be attached at all (the app's display view does not always carry a cursor overlay), so it
// answers a different question and would make this one wrong in both directions.
//
// A caller is entitled to skip building the frame on a false, so this must never block or wait: the
// answer is the current state, and a consumer that arrives a moment later is seen by the next call.
extern "C" bool android_display_has_consumer(struct AndroidDisplayContext* ctx) {
    if (ctx == nullptr || ctx->disp_service == nullptr) return false;
    return ctx->disp_service->getSurface(/* forCursor= */ false).hasNativeSurface();
}

extern "C" uint64_t android_display_consumer_generation(struct AndroidDisplayContext* ctx) {
    if (ctx == nullptr || ctx->disp_service == nullptr) return 0;
    return ctx->disp_service->getSurface(/* forCursor= */ false).nativeSurfaceGeneration();
}

extern "C" bool android_display_set_native_refresh_event(AndroidDisplayContext* ctx, int fd) {
    return ctx && ctx->disp_service &&
           ctx->disp_service->getSurface(false).setNativeRefreshEvent(fd);
}

extern "C" uint64_t android_display_native_refresh_generation(AndroidDisplayContext* ctx) {
    return ctx && ctx->disp_service
            ? ctx->disp_service->getSurface(false).nativeRefreshGeneration() : 0;
}

extern "C" bool android_display_get_timing(AndroidDisplayContext* ctx,
                                            android_display::DisplayTimingSample* sample) {
    if (!sample) return false;
    *sample = {};
    return ctx && ctx->disp_service && ctx->disp_service->getSurface(false).displayTiming(sample);
}

// Direct host-AHB presentation. `holder` is the process-local opaque token
// returned by crosvm_shared_ahb_create; it is never a guest pointer and is
// retained by the Rust HostSharedAllocation until the returned release fd has
// signalled. The output fd is an eventfd that becomes readable only after the
// SurfaceFlinger consumer release is proven.
static bool presentSharedAhbWithGeometry(
        struct AndroidDisplayContext* ctx, AndroidDisplaySurface* surface, uint64_t holder,
        int acquireFenceFd, int* outReleaseFenceFd, uint64_t expectedGeneration,
        uint64_t expectedModeGeneration, uint64_t expectedResetGeneration) {
    if (outReleaseFenceFd) *outReleaseFenceFd = -1;
    if (!ctx || !surface || !holder || !outReleaseFenceFd) return false;

    auto* holderPtr = reinterpret_cast<void*>(static_cast<uintptr_t>(holder));
    const auto* description = crosvm_shared_ahb_get_description(holderPtr);
    AHardwareBuffer* buffer = crosvm_shared_ahb_get_buffer(holderPtr);
    if (!description || !buffer || description->version != 1 || description->size != 64 ||
        description->buffer_id == 0 || description->plane_count != 1 ||
        description->layout_flags != android_display::kSharedAhbVerifiedLinearAlias ||
        description->modifier != 0 ||
        (description->fourcc != android_display::kSharedAhbRgbaFourcc &&
         description->fourcc != android_display::kSharedAhbBgraFourcc)) {
        ctx->errorf("Direct AHB holder failed authenticated description validation");
        return false;
    }
    auto releaseSignal = std::make_shared<DirectReleaseSignal>();
    if (releaseSignal->fd.get() < 0) {
        PLOG(ERROR) << "Failed to allocate direct AHB release eventfd";
        return false;
    }
    // Duplicate the producer-visible eventfd before submitting the transaction.
    // Once SurfaceControl accepts the buffer, its release callback owns the
    // ticket and there is no cancellation API. Returning failure after that
    // point would leave the caller reusing a buffer without a fence.
    const int exported = fcntl(releaseSignal->fd.get(), F_DUPFD_CLOEXEC, 0);
    if (exported < 0) {
        PLOG(ERROR) << "Failed to duplicate direct AHB release eventfd";
        return false;
    }
    auto result = surface->presentShared(buffer, acquireFenceFd, description, releaseSignal,
            expectedGeneration, expectedModeGeneration, expectedResetGeneration);
    if (!result.ok()) {
        close(exported);
        ctx->errorf("Failed to present direct AHB on surface %s: %s", surface->name().c_str(),
                    result.error().message().c_str());
        return false;
    }
    *outReleaseFenceFd = exported;
    // The zero-copy path is as latency-coupled as the blit: each DWM buffer
    // must come back from SurfaceControl within two frames.
    ctx->onPresentedFrame();
    return true;
}

extern "C" bool android_display_configure_scanout_geometry(
        AndroidDisplayContext* ctx, void* surfaceHandle,
        const AndroidDisplayScanoutGeometry* input) {
    auto* surface = static_cast<AndroidDisplaySurface*>(surfaceHandle);
    android_display::ScanoutPresentationGeometry geometry;
    if (!ctx || !surface || !input || !android_display::decodeScanoutGeometry(*input, &geometry))
        return false;
    auto result = surface->configureScanoutGeometry(geometry);
    if (!result.ok()) {
        ctx->errorf("Failed to configure native scanout geometry: %s", result.error().message().c_str());
        return false;
    }
    return true;
}

extern "C" bool android_display_reset_scanout_geometry(
        AndroidDisplayContext* ctx, uint64_t oldReset, uint64_t newReset) {
    return ctx && ctx->disp_service &&
            ctx->disp_service->getSurface(false).resetScanoutGeometry(oldReset, newReset).ok();
}

extern "C" bool android_display_query_scanout_profile(
        AndroidDisplayContext* ctx, void* surfaceHandle, AndroidDisplayScanoutProfile* output) {
    if (!output) return false;
    *output = {};
    auto* surface = static_cast<AndroidDisplaySurface*>(surfaceHandle);
    if (!ctx || !surface) return false;
    *output = surface->scanoutProfile();
    return true;
}

extern "C" bool android_display_configure_scanout_profile(
        AndroidDisplayContext* ctx, void* surfaceHandle, const AndroidDisplayScanoutGeometry* input,
        uint64_t endpointGeneration, uint64_t profileGeneration) {
    auto* surface = static_cast<AndroidDisplaySurface*>(surfaceHandle);
    android_display::ScanoutPresentationGeometry geometry;
    if (!ctx || !surface || !input || !endpointGeneration || !profileGeneration ||
            !android_display::decodeScanoutGeometry(*input, &geometry)) return false;
    auto result = surface->configureScanoutGeometry(geometry, endpointGeneration, profileGeneration);
    if (!result.ok()) {
        ctx->errorf("Failed to configure preferred scanout profile: %s", result.error().message().c_str());
        return false;
    }
    return true;
}

extern "C" bool android_display_present_shared_ahb_geometry(
        AndroidDisplayContext* ctx, void* surfaceHandle, uint64_t holder,
        int acquireFenceFd, int* outReleaseFenceFd, uint64_t attachmentGeneration,
        uint64_t modeGeneration, uint64_t resetGeneration) {
    auto* surface = static_cast<AndroidDisplaySurface*>(surfaceHandle);
    if (!modeGeneration || !resetGeneration) {
        if (outReleaseFenceFd) *outReleaseFenceFd = -1;
        return false;
    }
    return presentSharedAhbWithGeometry(ctx, surface, holder, acquireFenceFd,
            outReleaseFenceFd, attachmentGeneration, modeGeneration, resetGeneration);
}

extern "C" bool android_display_refresh_shared_ahb(
        AndroidDisplayContext* ctx, AndroidDisplaySurface* surface, uint64_t holder,
        int acquireFenceFd, int* outReleaseFenceFd, uint64_t expectedGeneration) {
    return presentSharedAhbWithGeometry(ctx, surface, holder, acquireFenceFd,
            outReleaseFenceFd, expectedGeneration, 0, 0);
}

extern "C" bool android_display_present_shared_ahb(
        AndroidDisplayContext* ctx, AndroidDisplaySurface* surface, uint64_t holder,
        int acquireFenceFd, int* outReleaseFenceFd) {
    return android_display_refresh_shared_ahb(ctx, surface, holder, acquireFenceFd,
                                             outReleaseFenceFd, 0);
}

extern "C" void android_display_release_import(struct AndroidDisplayContext* ctx,
                                               int64_t rawHandle) {
    if (!ctx || !ctx->vulkan_display || !rawHandle) return;
    ctx->vulkan_display->release(rawHandle);
}

extern "C" bool android_display_flip_to(struct AndroidDisplayContext* ctx,
                                        AndroidDisplaySurface* surface, int64_t rawHandle,
                                        int* outCompletionFenceFd) {
    if (outCompletionFenceFd) *outCompletionFenceFd = -1;
    if (!ctx || !surface || !rawHandle || !outCompletionFenceFd || !ctx->vulkan_display ||
        !ctx->vulkanDisplayActive()) {
        return false;
    }
    auto ret = surface->flip(*ctx->vulkan_display, rawHandle);
    if (!ret.ok()) {
        ctx->errorf("Failed to flip imported buffer for surface %s: %s", surface->name().c_str(),
                    ret.error().message().c_str());
        ctx->fallbackToCpu("M1 runtime flip failed");
        return false;
    }
    *outCompletionFenceFd = *ret;
    ctx->onPresentedFrame();
    return true;
}

// ---------------------------------------------------------------------------------------------
// The headless blit: the same Vulkan machinery, with a buffer we can read instead of a screen.
//
// Separate entry points rather than a flag on the display context above, because there is no
// display: no service name, no binder registration, no Surface, no cursor. A caller that wants this
// wants only the blit, and giving it the display context would mean it had to be told which half of
// that object was real.
// ---------------------------------------------------------------------------------------------

struct AndroidBlitContext {
    HeadlessBlitContext impl;
    AndroidBlitContext(uint32_t width, uint32_t height) : impl(width, height) {}
};

// Creates a blit context, or returns null if this process has no Vulkan blit driver to load. Null
// is a normal answer, not an error: it is what a machine with no CROSVM_DISPLAY_VULKAN_LIBRARY
// named says, and every caller of this already has a CPU path to fall to.
extern "C" struct AndroidBlitContext* android_blit_ctx_create(uint32_t width, uint32_t height) {
    auto ctx = std::make_unique<AndroidBlitContext>(width, height);
    if (!ctx->impl.ready()) {
        LOG(INFO) << "headless blit context unavailable; caller stays on the CPU copy";
        return nullptr;
    }
    LOG(INFO) << "headless blit context ready for " << width << "x" << height;
    return ctx.release();
}

extern "C" void android_blit_ctx_destroy(struct AndroidBlitContext* ctx) {
    delete ctx;
}

// Imports a guest dmabuf as a blit source. Returns 0 if it cannot be imported. `fourcc` is the
// GUEST's declaration; what the source image is actually created with is HeadlessBlitContext's
// business (see blitSourceFourcc).
//
// `exchange_red_blue` says which byte order blits from this import must land in: true for the
// CPU pipeline's canonical B,G,R,X (what LibVNCServer serves), false for the R,G,B,A an
// RGBA_8888 gralloc buffer claims to hold (what a video encoder reads). One dmabuf can be
// imported both ways at once; each import is one VkImage over the same guest pages.
extern "C" int64_t android_blit_ctx_import_dmabuf(struct AndroidBlitContext* ctx, int fd,
                                                  uint32_t offset, uint32_t stride,
                                                  uint64_t modifier, bool linearLayoutVerified,
                                                  uint32_t width, uint32_t height, uint32_t fourcc,
                                                  bool exchange_red_blue) {
    if (!ctx) return 0;
    return ctx->impl.importDmabuf(fd, offset, stride, modifier, linearLayoutVerified, width, height,
                                  fourcc, exchange_red_blue);
}

extern "C" void android_blit_ctx_release_import(struct AndroidBlitContext* ctx, int64_t importId) {
    if (!ctx || !importId) return;
    ctx->impl.release(importId);
}

// Blits an import into the context's target and waits, with a bound, for the GPU to finish. False
// means the frame did not happen and the caller should fall back.
extern "C" bool android_blit_ctx_blit(struct AndroidBlitContext* ctx, int64_t importId,
                                      uint32_t width, uint32_t height, int timeout_ms) {
    if (!ctx || !importId) return false;
    auto ret = ctx->impl.blit(importId, width, height, timeout_ms);
    if (!ret.ok()) {
        LOG(WARNING) << "headless blit failed: " << ret.error().message();
        return false;
    }
    return true;
}

// Maps the target for CPU reading. The mapping is valid until the next android_blit_ctx_blit or
// android_blit_ctx_unmap on this context, or until it is destroyed. Out params are only written on
// success.
extern "C" bool android_blit_ctx_map(struct AndroidBlitContext* ctx, const uint8_t** out_pixels,
                                     uint32_t* out_stride_bytes, uint32_t* out_width,
                                     uint32_t* out_height, uint32_t* out_size) {
    if (!ctx || !out_pixels || !out_stride_bytes || !out_width || !out_height || !out_size) {
        return false;
    }
    auto ret = ctx->impl.map(out_pixels, out_stride_bytes, out_width, out_height, out_size);
    if (!ret.ok()) {
        LOG(WARNING) << "headless blit map failed: " << ret.error().message();
        return false;
    }
    return true;
}

extern "C" void android_blit_ctx_unmap(struct AndroidBlitContext* ctx) {
    if (!ctx) return;
    ctx->impl.unmap();
}

// ---------------------------------------------------------------------------------------------
// The H.264 side channel's encoder half (plan §6 step 13).
//
// Deliberately shaped like the blit entry points above rather than folded into them: a caller can
// have a blit context and no encoder (every VNC screen today) or, in principle, an encoder and no
// blit context (a CPU-transport producer whose frames are uploaded). The one place the two meet is
// android_h264_enc_encode_frame, which takes the blit context as an argument -- so the coupling is
// per frame and visible in the signature, instead of a field somebody has to remember to set.
//
// Everything above the socket is here; nothing about the socket is. Which port the stream leaves
// by, who is listening on it and what the framing looks like are crosvm's, and this side never
// learns any of it.
// ---------------------------------------------------------------------------------------------

struct AndroidH264Encoder {
    std::unique_ptr<H264EncoderSession> impl;
};

// Brings up a hardware H.264 encoder for a screen of this size, or returns null.
//
// Null is an ordinary answer with several ordinary causes -- no media NDK, no AVC encoder, a
// geometry the component refuses -- and the caller's response to all of them is the same: serve
// RFB and nothing else. The reason is logged here, once, because here is where it is known.
extern "C" struct AndroidH264Encoder* android_h264_enc_create(uint32_t width, uint32_t height,
                                                              int32_t bitrate_bps,
                                                              int32_t frame_rate,
                                                              int32_t iframe_interval_secs) {
    auto session = H264EncoderSession::Open(width, height, bitrate_bps, frame_rate,
                                            iframe_interval_secs);
    if (!session) return nullptr;
    auto ctx = std::make_unique<AndroidH264Encoder>();
    ctx->impl = std::move(session);
    return ctx.release();
}

extern "C" void android_h264_enc_destroy(struct AndroidH264Encoder* ctx) {
    delete ctx;
}

// Asks for an IDR at the next opportunity. Called when a side-channel client connects.
extern "C" void android_h264_enc_request_sync_frame(struct AndroidH264Encoder* ctx) {
    if (!ctx) return;
    ctx->impl->requestSyncFrame();
}

// Feeds one picture. `import_id` non-zero (with `blit_ctx`) takes the GPU route -- the guest's own
// dmabuf blitted into the codec's input buffer -- and zero takes the CPU upload of `pixels`.
//
// False means the frame did not reach the encoder. `out_error` receives a NUL-terminated reason
// when it did not, so the caller can report the first failure verbatim rather than "hw encode did
// not work": on the GPU route that message IS the §7 verdict.
extern "C" bool android_h264_enc_encode_frame(struct AndroidH264Encoder* ctx,
                                              struct AndroidBlitContext* blit_ctx,
                                              int64_t import_id, const uint8_t* pixels,
                                              uint32_t pixels_size, uint32_t width, uint32_t height,
                                              const uint8_t* cursor_bgra, int32_t cursor_w,
                                              int32_t cursor_h, int32_t cursor_x, int32_t cursor_y,
                                              bool cursor_visible, int64_t pts_us, char* out_error,
                                              uint32_t error_cap) {
    if (out_error && error_cap) out_error[0] = '\0';
    if (!ctx) return false;
    H264CursorOverlay cursor{
            .pixels = cursor_bgra,
            .width = cursor_w,
            .height = cursor_h,
            .x = cursor_x,
            .y = cursor_y,
            .visible = cursor_visible,
    };
    auto ret = ctx->impl->encodeFrame(blit_ctx ? &blit_ctx->impl : nullptr, import_id, pixels,
                                      pixels_size, width, height, cursor, pts_us);
    if (ret.ok()) return true;
    if (out_error && error_cap) {
        const std::string message = ret.error().message();
        const size_t copied = std::min<size_t>(message.size(), error_cap - 1);
        memcpy(out_error, message.data(), copied);
        out_error[copied] = '\0';
    }
    return false;
}

// Drains one compressed buffer, blocking for at most `timeout_us`.
//
// Returns bytes written, 0 for "nothing was ready", -1 for "it did not fit" (with *out_size set to
// what it needs; the frame is still queued), and -2 for a codec error.
extern "C" int32_t android_h264_enc_poll_output(struct AndroidH264Encoder* ctx, uint8_t* out,
                                                uint32_t cap, uint32_t* out_size,
                                                uint32_t* out_flags, int64_t* out_pts_us,
                                                int64_t timeout_us) {
    if (!ctx || !out) return H264EncoderSession::kOutputFailed;
    return ctx->impl->pollOutput(out, cap, out_size, out_flags, out_pts_us, timeout_us);
}

// Copies the cached SPS+PPS, and returns how many bytes they are. A return greater than `cap`
// means nothing was written; zero means the encoder has not emitted them yet.
extern "C" uint32_t android_h264_enc_codec_config(struct AndroidH264Encoder* ctx, uint8_t* out,
                                                  uint32_t cap) {
    if (!ctx || !out) return 0;
    return static_cast<uint32_t>(ctx->impl->codecConfig(out, cap));
}

// How many frames were handed to the encoder, and how many were dropped because every input
// buffer was still with it. Reported rather than logged per frame: the ratio is the only honest
// way to read the stream's frame rate against the producer's offer rate.
extern "C" void android_h264_enc_frame_counts(struct AndroidH264Encoder* ctx, uint64_t* out_queued,
                                              uint64_t* out_dropped) {
    if (!ctx) return;
    if (out_queued) *out_queued = ctx->impl->queuedFrames();
    if (out_dropped) *out_dropped = ctx->impl->droppedFrames();
}

namespace {
struct NativeBltContext {
    VulkanDisplayBridge bridge;
    bool poisoned = false;
};
}

extern "C" void* android_display_native_blt_create() {
    auto context = std::make_unique<NativeBltContext>();
    if (!context->bridge.ready() || !context->bridge.hasCompletionFence()) return nullptr;
    return context.release();
}

extern "C" int android_display_native_blt_destroy(void* opaque) {
    auto* context = static_cast<NativeBltContext*>(opaque);
    if (!context) return 0;
    if (!context->bridge.prepareNativeDestruction()) return -EBUSY;
    delete context;
    return 0;
}

extern "C" int android_display_native_blt_copy(void* opaque, void* source, void* destination,
        const NativeBltRegion* regions, uint32_t count, int* completionFd) {
    if (completionFd) *completionFd = -1;
    if (!opaque || !source || !destination || !completionFd || source == destination) return -EINVAL;
    auto* context = static_cast<NativeBltContext*>(opaque);
    if (context->poisoned) return -EIO;
    // The getter uses one thread-local descriptor. Snapshot each identity
    // before the next call overwrites that storage.
    const auto* sourceDescription = crosvm_shared_ahb_get_description(source);
    if (!sourceDescription) return -EINVAL;
    const SharedAllocationDescription sourceValue = *sourceDescription;
    const auto* destinationDescription = crosvm_shared_ahb_get_description(destination);
    if (!destinationDescription) return -EINVAL;
    const SharedAllocationDescription destinationValue = *destinationDescription;
    const auto* src = &sourceValue;
    const auto* dst = &destinationValue;
    auto validDescription = [](const SharedAllocationDescription* d) {
        return d && d->version == 1 && d->size == sizeof(*d) && d->buffer_id &&
            d->plane_count == 1 && d->modifier == 0 &&
            d->layout_flags == android_display::kSharedAhbVerifiedLinearAlias &&
            (d->fourcc == android_display::kSharedAhbRgbaFourcc ||
             d->fourcc == android_display::kSharedAhbBgraFourcc) &&
            d->plane_offset <= UINT32_MAX;
    };
    if (!validDescription(src) || !validDescription(dst) || src->buffer_id == dst->buffer_id ||
            !android_display::nativeBltRegionsValid(src->width, src->height,
                    dst->width, dst->height, regions, count)) return -EINVAL;
    android::base::unique_fd sourceFd(crosvm_shared_ahb_duplicate_dma_buf(source));
    android::base::unique_fd destinationFd(crosvm_shared_ahb_duplicate_dma_buf(destination));
    struct stat sourceIdentity{}, destinationIdentity{};
    if (sourceFd.get() < 0 || destinationFd.get() < 0 ||
            fstat(sourceFd.get(), &sourceIdentity) || fstat(destinationFd.get(), &destinationIdentity)) return -EBADF;
    if (sourceIdentity.st_dev == destinationIdentity.st_dev &&
            sourceIdentity.st_ino == destinationIdentity.st_ino) return -EINVAL;
    // Reject the complete request before imports or writes. A new call cannot
    // destroy the previous source import while its GPU copy is still pending.
    if (auto retired = context->bridge.retireCompletedCopies(); !retired.ok()) {
        LOG(ERROR) << "Native Blt prior completion: " << retired.error();
        context->poisoned = true;
        return -EIO;
    }
    const int64_t imported = context->bridge.importDmabuf(sourceFd.get(), src->plane_offset,
            src->plane_stride, src->modifier, true, src->width, src->height, src->fourcc);
    if (!imported) return -ENOTSUP;
    auto result = context->bridge.blit(imported, crosvm_shared_ahb_get_buffer(destination), -1,
            false, regions, count, dst->width, dst->height);
    if (!result.ok()) {
        LOG(ERROR) << "Native Blt GPU copy: " << result.error();
        context->poisoned = true;
        return -EIO;
    }
    // A negative fd means the bridge synchronously proved slot completion
    // after a failed export; it is never an unproven successful submission.
    *completionFd = *result;
    return 0;
}

extern "C" int android_display_native_blt_copy_wait(void* opaque, void* source, void* destination,
        const NativeBltRegion* regions, uint32_t count) {
    int completionFd = -1;
    const int status = android_display_native_blt_copy(opaque, source, destination, regions, count, &completionFd);
    android::base::unique_fd completion(completionFd);
    if (status != 0) return status;
    auto* context = static_cast<NativeBltContext*>(opaque);
    if (auto result = context->bridge.retireCompletedCopies(); !result.ok()) {
        LOG(ERROR) << "Native Blt completion: " << result.error();
        context->poisoned = true;
        return -EIO;
    }
    return 0;
}

static int nativeBltTransfer(void* opaque, void* holder, const uint8_t* source, uint8_t* destination,
        const NativeBltCpuLayout* layout, const NativeBltRegion* regions, uint32_t count,
        bool upload, uint64_t* completedBytes) {
    if (completedBytes) *completedBytes=0;
    if (!opaque || !holder || !layout || !completedBytes || !regions || !count ||
            count>android_display::kNativeBltMaxRegions || (upload ? !source : !destination)) return -EINVAL;
    auto* context=static_cast<NativeBltContext*>(opaque);
    if (context->poisoned) return -EIO;
    const auto* description=crosvm_shared_ahb_get_description(holder);
    if (!description) return -EINVAL;
    const SharedAllocationDescription native=*description;
    const NativeBltCpuLayout cpu=*layout;
    // Snapshot the complete control input before any output bytes are written.
    NativeBltRegion copies[android_display::kNativeBltMaxRegions];
    std::memcpy(copies,regions,count*sizeof(copies[0]));
    uint64_t packedSize=0;
    if (native.version!=1 || native.size!=sizeof(native) || !native.buffer_id ||
            native.plane_count!=1 || native.modifier ||
            native.layout_flags!=android_display::kSharedAhbVerifiedLinearAlias ||
            (native.fourcc!=android_display::kSharedAhbRgbaFourcc && native.fourcc!=android_display::kSharedAhbBgraFourcc) ||
            native.plane_offset>UINT32_MAX || native.plane_stride%4 ||
            uint64_t(native.plane_stride)<uint64_t(native.width)*4 ||
            native.allocation_size>android_display::kSharedAhbMaxAllocation ||
            native.plane_offset>native.allocation_size ||
            !android_display::nativeBltTransferValid(native.width,native.height,cpu,copies,count,upload,&packedSize) ||
            uint64_t(native.plane_stride)*(native.height-1)+uint64_t(native.width)*4>
                native.allocation_size-native.plane_offset) return -EINVAL;
    auto result=context->bridge.transferNative(holder,native,source,destination,cpu,copies,count,upload,packedSize);
    if (!result.ok()) {
        LOG(ERROR) << "Native Blt ordinary transfer: " << result.error();
        context->poisoned=true;
        return -EIO;
    }
    *completedBytes=packedSize;
    return 0;
}

extern "C" int android_display_native_blt_upload(void* context, void* destination,
        const uint8_t* source, const NativeBltCpuLayout* layout,
        const NativeBltRegion* regions, uint32_t count, uint64_t* completedBytes) {
    return nativeBltTransfer(context,destination,source,nullptr,layout,regions,count,true,completedBytes);
}

extern "C" int android_display_native_blt_readback(void* context, void* source,
        uint8_t* destination, const NativeBltCpuLayout* layout,
        const NativeBltRegion* regions, uint32_t count, uint64_t* completedBytes) {
    return nativeBltTransfer(context,source,nullptr,destination,layout,regions,count,false,completedBytes);
}
