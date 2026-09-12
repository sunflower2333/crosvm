// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Exercise the actual frontend decoder/encoder, callback and split used rings.
//! The 2D component provides deterministic fence callbacks without a real GPU.

use std::mem::size_of_val;
use std::sync::atomic::AtomicBool;

use vm_memory::GuestAddress;
use zerocopy::IntoBytes;

use super::*;
use crate::virtio::create_descriptor_chain;
use crate::virtio::DescriptorType;
use crate::virtio::QueueConfig;
#[cfg(target_os = "android")]
use crate::virtio::resource_bridge::ResourceInfo;

type Activation = Arc<Mutex<Option<FenceHandlerActivationResources<SharedQueueReader>>>>;

#[cfg(target_os = "android")]
#[test]
#[ignore = "requires the matched Virgl library and real Android EGL"]
fn real_virgl_signaled_fence_crosses_resource_bridge_without_cookie_zero() {
    let mut harness = Harness::with_component(false, RutabagaComponentType::VirglRenderer);
    assert!(harness.frontend.virtio_gpu.uses_virgl_global_fences());
    let (client, server) = Tube::pair().unwrap();

    // Cookie zero is unknown and must never become an implicit completed sync.
    client
        .send(&ResourceRequest::GetFence { seqno: 0 })
        .unwrap();
    harness.frontend.process_resource_bridge(&server).unwrap();
    assert!(matches!(
        client.recv::<ResourceResponse>().unwrap(),
        ResourceResponse::Invalid
    ));

    client.send(&ResourceRequest::GetSignaledFence).unwrap();
    harness.frontend.process_resource_bridge(&server).unwrap();
    let ResourceResponse::Resource(ResourceInfo::Fence { handle }) =
        client.recv::<ResourceResponse>().unwrap()
    else {
        panic!("explicit signaled export failed");
    };
    assert!(flip_fence_signaled(&handle).unwrap());
    let duplicate = handle.try_clone().unwrap();
    drop(handle);
    drop(harness);
    assert!(flip_fence_signaled(&duplicate).unwrap());
}

struct Harness {
    frontend: Frontend,
    mem: GuestMemory,
    queues: [SharedQueueReader; 2],
    submitted: [u16; 2],
    callback: RutabagaFenceHandler,
    observed: Arc<Mutex<Vec<RutabagaFence>>>,
    activation: Activation,
}

impl Harness {
    fn new(inline: bool) -> Self {
        Self::with_component(inline, RutabagaComponentType::Rutabaga2D)
    }

    fn with_contexts(inline: bool) -> Self {
        Self::with_component(inline, RutabagaComponentType::CrossDomain)
    }

    fn with_component(inline: bool, component: RutabagaComponentType) -> Self {
        Self::with_behavior(inline, component, false)
    }

    fn with_behavior(inline: bool, component: RutabagaComponentType, fail_inline: bool) -> Self {
        let mem = GuestMemory::new(&[(GuestAddress(0), 0x10000)]).unwrap();
        let queues = [0x1000, 0x3000].map(|base| {
            let mut config = QueueConfig::new(8, 0);
            config.set_desc_table(GuestAddress(base));
            config.set_avail_ring(GuestAddress(base + 0x200));
            config.set_used_ring(GuestAddress(base + 0x300));
            config.set_ready(true);
            SharedQueueReader::new(
                config
                    .activate(&mem, Event::new().unwrap(), Interrupt::new_for_test())
                    .unwrap(),
            )
        });
        let state = Arc::new(Mutex::new(FenceState::default()));
        let activation = Arc::new(Mutex::new(Some(FenceHandlerActivationResources {
            mem: mem.clone(),
            ctrl_queue: queues[0].clone(),
            cursor_queue: Some(queues[1].clone()),
        })));
        let callback = create_fence_handler(activation.clone(), state.clone());
        let callback_copy = callback.clone();
        let error_callback = create_fence_error_handler(state.clone());
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_copy = observed.clone();
        let renderer_callback = RutabagaFenceHandler::new(move |fence| {
            observed_copy.lock().push(fence);
            if fail_inline {
                error_callback.call(RutabagaFenceError {
                    ctx_id: fence.ctx_id,
                    ring_idx: u32::from(fence.ring_idx),
                    fence_id: fence.fence_id,
                    error: -libc::EIO,
                });
            }
            if inline {
                callback_copy.call(fence);
            }
        });
        let mut builder = RutabagaBuilder::new(component, 0);
        if component == RutabagaComponentType::VirglRenderer {
            builder = builder
                .set_use_egl(true)
                .set_use_gles(true)
                .set_use_surfaceless(true);
        }
        let rutabaga = builder.build(renderer_callback, None).unwrap();
        let gpu = VirtioGpu::new(
            GpuDisplay::open_stub().unwrap(),
            Vec::new(),
            Arc::new(AtomicBool::new(false)),
            rutabaga,
            Arc::new(Mutex::new(None)),
            false,
            false,
            false,
            None,
        )
        .unwrap();
        let mut frontend = Frontend::new(gpu, state.clone()).unwrap();
        // Select the Virgl frontend protocol for deterministic components too.
        // Only the ignored Android case above uses the actual Virgl renderer.
        frontend.maps_global_fences = true;
        state.lock().uses_global_tokens = true;
        Self {
            frontend,
            mem,
            queues,
            submitted: [0, 0],
            callback,
            observed,
            activation,
        }
    }

    fn submit(&mut self, command: &[u8], queue: usize, response: bool) {
        let base = if queue == 0 { 0x1000 } else { 0x3000 };
        // Up to four simultaneous requests per queue; do not rewrite memory
        // still referenced by a parked teardown/renderer descriptor.
        let head = (self.submitted[queue] % 4) * 2;
        let payload = base + 0x800 + u64::from(head / 2) * 0x400;
        let mut descriptors = vec![(DescriptorType::Readable, command.len() as u32)];
        if response {
            descriptors.push((DescriptorType::Writable, 512));
        }
        drop(
            create_descriptor_chain(
                &self.mem,
                GuestAddress(base + u64::from(head) * 16),
                GuestAddress(payload),
                descriptors,
                0,
            )
            .unwrap(),
        );
        if response {
            // The helper writes relative next=1; the actual queue indexes the
            // whole descriptor table, so link to this head's response slot.
            self.mem
                .write_obj_at_addr(
                    Le16::from(head + 1),
                    GuestAddress(base + u64::from(head) * 16 + 14),
                )
                .unwrap();
        }
        self.mem
            .write_all_at_addr(command, GuestAddress(payload))
            .unwrap();
        let slot = self.submitted[queue] % 8;
        self.mem
            .write_obj_at_addr(
                Le16::from(head),
                GuestAddress(base + 0x204 + u64::from(slot) * 2),
            )
            .unwrap();
        self.submitted[queue] += 1;
        self.mem
            .write_obj_at_addr(
                Le16::from(self.submitted[queue]),
                GuestAddress(base + 0x202),
            )
            .unwrap();
        let signal = if queue == 0 {
            self.frontend.process_queue(&self.mem, &self.queues[queue])
        } else {
            self.frontend
                .process_cursor_queue(&self.mem, &self.queues[queue])
        };
        if signal {
            self.queues[queue].signal_used();
        }
    }

    fn used(&self, queue: usize) -> u16 {
        let addr = if queue == 0 { 0x1302 } else { 0x3302 };
        self.mem
            .read_obj_from_addr::<Le16>(GuestAddress(addr))
            .unwrap()
            .to_native()
    }

    fn response(&self, queue: usize, command_len: usize) -> virtio_gpu_ctrl_hdr {
        let base = if queue == 0 { 0x1800 } else { 0x3800 };
        let base = base + u64::from((self.submitted[queue] - 1) % 4) * 0x400;
        self.mem
            .read_obj_from_addr(GuestAddress(base + command_len as u64))
            .unwrap()
    }
}

fn context_request(ctx_id: u32, cookie: u64, ring_idx: u8) -> virtio_gpu_ctrl_hdr {
    virtio_gpu_ctrl_hdr {
        flags: (VIRTIO_GPU_FLAG_FENCE | VIRTIO_GPU_FLAG_INFO_RING_IDX).into(),
        ctx_id: ctx_id.into(),
        ring_idx,
        ..request(cookie)
    }
}

fn create_context(test: &mut Harness, ctx_id: u32) {
    let command = virtio_gpu_ctx_create {
        hdr: virtio_gpu_ctrl_hdr {
            type_: VIRTIO_GPU_CMD_CTX_CREATE.into(),
            ctx_id: ctx_id.into(),
            ..Default::default()
        },
        context_init: RUTABAGA_CAPSET_CROSS_DOMAIN.into(),
        ..Default::default()
    };
    test.submit(command.as_bytes(), 0, true);
    assert_eq!(
        test.response(0, size_of_val(&command)).type_.to_native(),
        VIRTIO_GPU_RESP_OK_NODATA
    );
}

#[test]
fn recreated_context_waits_for_new_token_and_rejects_old_or_wrong_callback() {
    let mut test = Harness::with_contexts(false);
    create_context(&mut test, 7);
    test.submit(context_request(7, 100, 0).as_bytes(), 0, true);
    let old = *test.observed.lock().last().unwrap();
    test.callback.call(old);
    assert_eq!(test.used(0), 2);
    let destroy = virtio_gpu_ctrl_hdr {
        type_: VIRTIO_GPU_CMD_CTX_DESTROY.into(),
        ..context_request(7, u64::MAX, 0)
    };
    test.submit(destroy.as_bytes(), 0, true);
    assert_eq!(test.used(0), 3);
    assert_eq!(
        test.response(0, size_of_val(&destroy)).type_.to_native(),
        VIRTIO_GPU_RESP_OK_NODATA
    );
    assert_eq!(
        test.response(0, size_of_val(&destroy)).fence_id.to_native(),
        u64::MAX
    );
    assert_eq!(test.observed.lock().len(), 1); // no fence created after teardown
    create_context(&mut test, 7);
    test.submit(context_request(7, 1, 0).as_bytes(), 0, true);
    let new = *test.observed.lock().last().unwrap();
    assert!(new.fence_id > old.fence_id);
    assert_eq!(test.used(0), 4);
    test.callback.call(old);
    test.callback.call(RutabagaFence { ctx_id: 8, ..new });
    test.callback.call(RutabagaFence { ring_idx: 1, ..new });
    assert_eq!(test.used(0), 4);
    test.callback.call(new);
    assert_eq!(test.used(0), 5);
    assert_eq!(
        test.response(0, size_of_val(&destroy)).fence_id.to_native(),
        1
    );
}

#[test]
fn renderer_failure_retains_tokens_and_descriptors_despite_later_success() {
    let mut test = Harness::with_contexts(false);
    create_context(&mut test, 7);
    test.submit(context_request(7, 100, 0).as_bytes(), 0, true);
    test.submit(context_request(7, 101, 0).as_bytes(), 1, true);
    let first = test.observed.lock()[0];
    let later = test.observed.lock()[1];
    let error = RutabagaFenceError {
        ctx_id: first.ctx_id,
        ring_idx: u32::from(first.ring_idx),
        fence_id: first.fence_id,
        error: -libc::EIO,
    };
    let handler = create_fence_error_handler(test.frontend.fence_state.clone());
    for wrong in [
        RutabagaFenceError { ctx_id: 8, ..error },
        RutabagaFenceError {
            ring_idx: 256,
            ..error
        },
        RutabagaFenceError {
            fence_id: u64::MAX,
            ..error
        },
        RutabagaFenceError { error: 0, ..error },
    ] {
        handler.call(wrong);
        assert!(test.frontend.fence_state.lock().renderer_error.is_none());
    }
    handler.call(error);
    test.callback.call(later);
    test.callback.call(first);
    assert_eq!(test.used(0), 1);
    assert_eq!(test.used(1), 0);
    let state = test.frontend.fence_state.lock();
    assert_eq!(state.renderer_error, Some(error));
    assert!(state
        .context_tokens
        .contains(7, first.ring_idx, first.fence_id));
    assert!(state
        .context_tokens
        .contains(7, later.ring_idx, later.fence_id));
    assert!(!state
        .queue
        .completed_renderer
        .contains_key(&VirtioGpuRing::ContextSpecific {
            ctx_id: 7,
            ring_idx: first.ring_idx,
        }));
    assert!(state.snapshot().is_err());
    drop(state);
    assert!(test.frontend.check_quiescent().is_err());
    assert!(test
        .frontend
        .process_context_retirement(&test.mem, &test.queues[0], None)
        .is_err());
}

#[test]
fn inline_renderer_failure_wakes_without_teardown_and_stops_new_commands() {
    let mut test = Harness::with_behavior(true, RutabagaComponentType::CrossDomain, true);
    create_context(&mut test, 7);
    test.submit(context_request(7, 10, 0).as_bytes(), 0, true);
    assert_eq!(test.used(0), 1);
    assert!(!test.frontend.fence_state.lock().waiting_for_context_destroy);
    assert!(matches!(
        test.frontend
            .context_retirement_event
            .wait_timeout(Duration::ZERO),
        Ok(base::EventWaitResult::Signaled)
    ));
    assert!(!test.frontend.fence_state.lock().queue.is_empty());
    let new_context = virtio_gpu_ctx_create {
        hdr: virtio_gpu_ctrl_hdr {
            type_: VIRTIO_GPU_CMD_CTX_CREATE.into(),
            ctx_id: 8.into(),
            ..Default::default()
        },
        ..Default::default()
    };
    test.submit(new_context.as_bytes(), 0, true);
    assert_eq!(test.used(0), 1);
    assert!(test.frontend.virtio_gpu.destroy_context(8).is_err());
}

#[test]
fn inactive_renderer_failure_survives_callbacks_and_rejects_restore() {
    let mut test = Harness::with_contexts(false);
    let snapshot = test.frontend.fence_state.lock().snapshot().unwrap();
    create_context(&mut test, 7);
    test.submit(context_request(7, 10, 0).as_bytes(), 0, false);
    let fence = test.observed.lock()[0];
    test.activation.lock().take();
    create_fence_error_handler(test.frontend.fence_state.clone()).call(RutabagaFenceError {
        ctx_id: fence.ctx_id,
        ring_idx: u32::from(fence.ring_idx),
        fence_id: fence.fence_id,
        error: -libc::ENOENT,
    });
    test.callback.call(fence);
    assert_eq!(test.used(0), 1);
    assert!(matches!(
        test.frontend
            .context_retirement_event
            .wait_timeout(Duration::ZERO),
        Ok(base::EventWaitResult::Signaled)
    ));
    assert!(test.frontend.fence_state.lock().restore(snapshot).is_err());
}

#[test]
fn teardown_waits_before_backend_destroy_then_resumes_queued_recreate() {
    let mut test = Harness::with_contexts(false);
    create_context(&mut test, 7);
    test.submit(context_request(7, 100, 0).as_bytes(), 0, false);
    let old = *test.observed.lock().last().unwrap();
    let destroy = virtio_gpu_ctrl_hdr {
        type_: VIRTIO_GPU_CMD_CTX_DESTROY.into(),
        ..context_request(7, 2, 0)
    };
    test.submit(destroy.as_bytes(), 1, false);
    assert!(test.frontend.deferred_context_destroy.is_some());
    assert_eq!([test.used(0), test.used(1)], [1, 0]);
    // Actual backend rejects duplicate creation, proving it is still alive.
    assert!(test
        .frontend
        .virtio_gpu
        .create_context(7, RUTABAGA_CAPSET_CROSS_DOMAIN, None)
        .is_err());
    let recreate = virtio_gpu_ctx_create {
        hdr: virtio_gpu_ctrl_hdr {
            type_: VIRTIO_GPU_CMD_CTX_CREATE.into(),
            ctx_id: 7.into(),
            ..Default::default()
        },
        context_init: RUTABAGA_CAPSET_CROSS_DOMAIN.into(),
        ..Default::default()
    };
    test.submit(recreate.as_bytes(), 0, true);
    assert_eq!(test.used(0), 1);
    test.callback.call(old);
    assert!(matches!(
        test.frontend
            .context_retirement_event
            .wait_timeout(Duration::ZERO)
            .unwrap(),
        base::EventWaitResult::Signaled
    ));
    test.frontend
        .process_context_retirement(&test.mem, &test.queues[0], Some(&test.queues[1]))
        .unwrap();
    assert!(test.frontend.deferred_context_destroy.is_none());
    assert_eq!([test.used(0), test.used(1)], [3, 1]);
    assert_eq!(
        test.response(0, size_of_val(&recreate)).type_.to_native(),
        VIRTIO_GPU_RESP_OK_NODATA
    );
    assert_eq!(test.observed.lock().len(), 1);
    test.submit(context_request(7, 0, 0).as_bytes(), 0, false);
    assert_eq!(test.used(0), 3);
    test.callback.call(old);
    assert_eq!(test.used(0), 3);
    test.callback.call(*test.observed.lock().last().unwrap());
    assert_eq!(test.used(0), 4);
}

#[test]
fn context_teardown_timeout_keeps_backend_and_descriptors_owned() {
    let mut test = Harness::with_contexts(false);
    create_context(&mut test, 9);
    test.submit(context_request(9, 11, 0).as_bytes(), 0, true);
    let destroy = virtio_gpu_ctrl_hdr {
        type_: VIRTIO_GPU_CMD_CTX_DESTROY.into(),
        ctx_id: 9.into(),
        ..Default::default()
    };
    test.submit(destroy.as_bytes(), 1, true);
    test.frontend
        .deferred_context_destroy
        .as_mut()
        .unwrap()
        .deadline = std::time::Instant::now();
    assert!(test
        .frontend
        .process_context_retirement(&test.mem, &test.queues[0], Some(&test.queues[1]))
        .is_err());
    assert!(test.frontend.display_failed);
    test.callback.call(*test.observed.lock().last().unwrap());
    assert_eq!([test.used(0), test.used(1)], [1, 0]);
    assert!(test.frontend.deferred_context_destroy.is_some());
    assert!(test
        .frontend
        .virtio_gpu
        .create_context(9, RUTABAGA_CAPSET_CROSS_DOMAIN, None)
        .is_err());
}

#[test]
fn inline_context_fences_keep_full_cookies_and_zero_response_ownership() {
    let mut test = Harness::with_contexts(true);
    create_context(&mut test, 4);
    for cookie in [u64::MAX, 0, 7, 7, 1] {
        test.submit(context_request(4, cookie, 0).as_bytes(), 0, true);
        assert_eq!(
            test.response(0, size_of::<virtio_gpu_ctrl_hdr>())
                .fence_id
                .to_native(),
            cookie
        );
        assert!(test.frontend.fence_state.lock().queue.is_empty());
    }
    test.submit(context_request(4, 2, 0).as_bytes(), 1, false);
    assert_eq!(test.used(1), 1);
    assert!(test.frontend.fence_state.lock().context_tokens.is_empty());
}

#[test]
fn failed_context_fence_is_cancelled_and_snapshot_keeps_allocator_high_water() {
    let mut test = Harness::with_contexts(false);
    test.submit(context_request(999, 0, 0).as_bytes(), 0, true);
    assert_eq!(test.used(0), 1);
    let mut state = test.frontend.fence_state.lock();
    assert!(state.context_tokens.is_empty());
    assert!(!state.context_tokens.complete(999, 0, 1));
    let old = state.snapshot().unwrap();
    let token = state.context_tokens.reserve(9, 0).unwrap();
    state.context_tokens.cancel(token);
    state.restore(old).unwrap();
    assert!(state.context_tokens.reserve(9, 0).unwrap() > token);
}

#[test]
fn suspended_context_callback_retains_completion_until_same_activation_resumes() {
    let mut test = Harness::with_contexts(false);
    create_context(&mut test, 9);
    test.submit(context_request(9, 11, 0).as_bytes(), 0, false);
    assert!(test.frontend.check_quiescent().is_err());
    let activation = test.activation.lock().take();
    test.callback.call(*test.observed.lock().last().unwrap());
    assert_eq!(test.used(0), 1);
    assert!(test.frontend.fence_state.lock().context_tokens.is_empty());
    assert!(test.frontend.check_quiescent().is_err());
    *test.activation.lock() = activation;
    return_fenced_descriptors(
        test.frontend.fence_state.lock().queue.drain_ready(),
        &test.queues[0],
        Some(&test.queues[1]),
    );
    assert_eq!(test.used(0), 2);
    assert!(test.frontend.check_quiescent().is_ok());
}

#[test]
fn inline_completion_before_queued_teardown_wakes_without_another_guest_kick() {
    let mut test = Harness::with_contexts(true);
    create_context(&mut test, 7);
    let activation = test.activation.lock().take();
    // A different source queue prevents process_queue's local drain from
    // releasing this completed descriptor before the following destroy.
    let command = context_request(7, 11, 0);
    let chain = create_descriptor_chain(
        &test.mem,
        GuestAddress(0x5000),
        GuestAddress(0x6000),
        vec![(DescriptorType::Readable, size_of_val(&command) as u32)],
        0,
    )
    .unwrap();
    test.mem
        .write_all_at_addr(command.as_bytes(), GuestAddress(0x6000))
        .unwrap();
    assert!(test
        .frontend
        .process_descriptor(&test.mem, chain, GpuQueue::Cursor)
        .is_none());
    *test.activation.lock() = activation;
    let destroy = virtio_gpu_ctrl_hdr {
        type_: VIRTIO_GPU_CMD_CTX_DESTROY.into(),
        ctx_id: 7.into(),
        ..Default::default()
    };
    test.submit(destroy.as_bytes(), 1, false);
    assert_eq!(test.used(1), 1); // completed old descriptor, not destroy
    assert!(test.frontend.deferred_context_destroy.is_some());
    assert!(matches!(
        test.frontend
            .context_retirement_event
            .wait_timeout(Duration::ZERO)
            .unwrap(),
        base::EventWaitResult::Signaled
    ));
    test.frontend
        .process_context_retirement(&test.mem, &test.queues[0], Some(&test.queues[1]))
        .unwrap();
    assert_eq!(test.used(1), 2);
    assert!(test.frontend.check_quiescent().is_ok());
}

#[test]
fn context_token_exhaustion_rejects_create_before_side_effects() {
    let mut test = Harness::with_contexts(true);
    assert!(test
        .frontend
        .fence_state
        .lock()
        .context_tokens
        .restore_last_issued(u64::MAX));
    let create = virtio_gpu_ctx_create {
        hdr: virtio_gpu_ctrl_hdr {
            type_: VIRTIO_GPU_CMD_CTX_CREATE.into(),
            ..context_request(7, 1, 0)
        },
        context_init: RUTABAGA_CAPSET_CROSS_DOMAIN.into(),
        ..Default::default()
    };
    test.submit(create.as_bytes(), 0, true);
    assert_eq!(
        test.response(0, size_of_val(&create)).type_.to_native(),
        VIRTIO_GPU_RESP_ERR_UNSPEC
    );
    assert!(test.observed.lock().is_empty());
    assert!(test
        .frontend
        .virtio_gpu
        .create_context(7, RUTABAGA_CAPSET_CROSS_DOMAIN, None)
        .is_ok());
    test.frontend.virtio_gpu.destroy_context(7).unwrap();
}

#[test]
fn context_snapshot_format_and_watermarks_are_validated_before_allocator_changes() {
    let mut state = FenceState {
        uses_global_tokens: true,
        ..Default::default()
    };
    let legacy = FenceStateSnapshot {
        completed_fences: BTreeMap::new(),
        global_tokens_last_issued: Some(80),
        context_tokens_last_issued: None,
    };
    assert!(state.restore(legacy).is_err());
    let invalid = FenceStateSnapshot {
        completed_fences: BTreeMap::from([(
            VirtioGpuRing::ContextSpecific {
                ctx_id: 1,
                ring_idx: 0,
            },
            9,
        )]),
        global_tokens_last_issued: Some(80),
        context_tokens_last_issued: Some(8),
    };
    assert!(state.restore(invalid).is_err());
    assert_eq!(state.global_tokens.last_issued(), 0);
    assert_eq!(state.context_tokens.last_issued(), 0);
    let valid = FenceStateSnapshot {
        completed_fences: BTreeMap::from([(
            VirtioGpuRing::ContextSpecific {
                ctx_id: 1,
                ring_idx: 0,
            },
            9,
        )]),
        global_tokens_last_issued: Some(80),
        context_tokens_last_issued: Some(9),
    };
    state.restore(valid).unwrap();
    assert_eq!(state.context_tokens.reserve(1, 0), Some(10));
    assert!(state.snapshot().is_err());
    assert!(!state.context_tokens.complete(1, 0, 9));
}

fn request(cookie: u64) -> virtio_gpu_ctrl_hdr {
    virtio_gpu_ctrl_hdr {
        type_: VIRTIO_GPU_CMD_GET_DISPLAY_INFO.into(),
        flags: VIRTIO_GPU_FLAG_FENCE.into(),
        fence_id: cookie.into(),
        ..Default::default()
    }
}

fn display_readers(test: &mut Harness, count: usize) -> Vec<Event> {
    let events: Vec<_> = (0..count).map(|_| Event::new().unwrap()).collect();
    test.frontend.virtio_gpu.inject_flip_fences_for_test(
        events
            .iter()
            .map(|event| SafeDescriptor::try_from(event as &dyn AsRawDescriptor).unwrap())
            .collect(),
    );
    events
}

#[test]
fn every_display_reader_must_finish_before_either_used_ring_advances() {
    let mut test = Harness::new(true);
    let _events = display_readers(&mut test, 3);
    test.submit(request(u64::MAX).as_bytes(), 0, true);
    // The renderer callback completes inline, but this later descriptor must
    // still wait behind all readers of the earlier global flush.
    test.submit(request(0).as_bytes(), 1, true);
    assert_eq!([test.used(0), test.used(1)], [0, 0]);
    assert_eq!(test.frontend.pending_flip_fences.len(), 3);
    assert_eq!(test.observed.lock().len(), 1);
    let first_fd = test.frontend.pending_flip_fences[0]
        .fence
        .as_raw_descriptor();
    let wait = WaitContext::new().unwrap();
    test.frontend.register_flip_fences(&wait).unwrap();
    test.frontend
        .complete_flip_fences_with(&test.queues[0], &test.queues[1], &wait, |fd| {
            Ok(fd.as_raw_descriptor() != first_fd)
        })
        .unwrap();
    assert_eq!([test.used(0), test.used(1)], [0, 0]);
    assert_eq!(test.frontend.pending_flip_fences.len(), 1);
    test.frontend
        .complete_flip_fences_with(&test.queues[0], &test.queues[1], &wait, |_| Ok(true))
        .unwrap();
    assert_eq!([test.used(0), test.used(1)], [1, 1]);
    assert!(test.frontend.pending_flip_fences.is_empty());
    assert!(test.frontend.fence_state.lock().queue.is_empty());
    test.frontend
        .complete_flip_fences_with(&test.queues[0], &test.queues[1], &wait, |_| Ok(true))
        .unwrap();
    assert_eq!([test.used(0), test.used(1)], [1, 1]);
}

#[test]
fn unfenced_display_ownership_is_retained_with_or_without_response_space() {
    for queue in 0..2 {
        for response in [false, true] {
            let mut test = Harness::new(true);
            let _events = display_readers(&mut test, 2);
            let mut command = request(99);
            command.flags = 0.into();
            test.submit(command.as_bytes(), queue, response);
            assert_eq!([test.used(0), test.used(1)], [0, 0]);
            assert!(test.observed.lock().is_empty());
            let wait = WaitContext::new().unwrap();
            test.frontend
                .complete_flip_fences_with(&test.queues[0], &test.queues[1], &wait, |_| Ok(true))
                .unwrap();
            assert_eq!(test.used(queue), 1);
            assert_eq!(test.used(1 - queue), 0);
            if response {
                let response = test.response(queue, size_of_val(&command));
                assert_eq!(response.flags.to_native(), 0);
                assert_eq!(response.fence_id.to_native(), 0);
            }
        }
    }
}

#[test]
fn display_reader_error_after_partial_success_retains_descriptor() {
    let mut test = Harness::new(true);
    let _events = display_readers(&mut test, 2);
    test.submit(request(3).as_bytes(), 0, true);
    let first_fd = test.frontend.pending_flip_fences[0]
        .fence
        .as_raw_descriptor();
    let wait = WaitContext::new().unwrap();
    let result =
        test.frontend
            .complete_flip_fences_with(&test.queues[0], &test.queues[1], &wait, |fd| {
                if fd.as_raw_descriptor() == first_fd {
                    Ok(true)
                } else {
                    Err(anyhow!("injected display-reader failure"))
                }
            });
    assert!(result.is_err());
    assert_eq!(test.used(0), 0);
    assert_eq!(test.frontend.pending_flip_fences.len(), 1);
    assert!(!test.frontend.fence_state.lock().queue.is_empty());
}

#[test]
fn last_display_reader_timeout_never_completes_its_descriptor() {
    let mut test = Harness::new(true);
    let _events = display_readers(&mut test, 2);
    test.submit(request(3).as_bytes(), 1, true);
    let first_fd = test.frontend.pending_flip_fences[0]
        .fence
        .as_raw_descriptor();
    test.frontend.pending_flip_fences[1].deadline = std::time::Instant::now();
    let wait = WaitContext::new().unwrap();
    assert!(test
        .frontend
        .complete_flip_fences_with(&test.queues[0], &test.queues[1], &wait, |fd| Ok(fd
            .as_raw_descriptor()
            == first_fd),)
        .is_err());
    assert_eq!(test.used(1), 0);
    assert_eq!(test.frontend.pending_flip_fences.len(), 1);
    assert!(!test.frontend.fence_state.lock().queue.is_empty());
}

#[test]
fn partial_command_failure_keeps_display_readers_and_error_response() {
    let mut test = Harness::new(true);
    let _events = display_readers(&mut test, 2);
    // Model an earlier successful scanout and then a failed scanout command.
    let command = virtio_gpu_resource_flush {
        hdr: virtio_gpu_ctrl_hdr {
            type_: VIRTIO_GPU_CMD_RESOURCE_FLUSH.into(),
            ..request(5)
        },
        resource_id: 999.into(),
        ..Default::default()
    };
    test.submit(command.as_bytes(), 0, true);
    assert_eq!(test.used(0), 0);
    assert_eq!(
        test.response(0, size_of_val(&command)).type_.to_native(),
        VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID
    );
    let wait = WaitContext::new().unwrap();
    test.frontend
        .complete_flip_fences_with(&test.queues[0], &test.queues[1], &wait, |_| Ok(true))
        .unwrap();
    assert_eq!(test.used(0), 1);
    assert!(test.observed.lock().is_empty());
}

#[cfg(any(target_os = "android", target_os = "linux"))]
#[test]
fn readable_non_sync_file_is_not_successful_display_completion() {
    let mut test = Harness::new(true);
    let events = display_readers(&mut test, 1);
    test.submit(request(7).as_bytes(), 0, false);
    let wait = WaitContext::new().unwrap();
    test.frontend.register_flip_fences(&wait).unwrap();
    test.frontend
        .complete_flip_fences(&test.queues[0], &test.queues[1], &wait)
        .unwrap();
    assert_eq!(test.used(0), 0);
    events[0].signal().unwrap();
    assert!(test
        .frontend
        .complete_flip_fences(&test.queues[0], &test.queues[1], &wait)
        .is_err());
    assert_eq!(test.used(0), 0);
    assert_eq!(test.frontend.pending_flip_fences.len(), 1);
}

// Opt-in device tests: create only private userspace KGSL timelines. They
// submit no GPU commands and neither inspect nor signal the VM's timelines.
// UAPI: virglrenderer/src/drm/drm2kgsl/msm_kgsl.h (KGSL 0x58..0x5d).
#[cfg(target_os = "android")]
mod kgsl_sync {
    use std::fs::File;
    use std::fs::OpenOptions;

    use super::*;

    #[repr(C)]
    #[derive(Default)]
    struct TimelineValue {
        seqno: u64,
        id: u32,
        padding: u32,
    }

    #[repr(C)]
    struct TimelineFence {
        seqno: u64,
        id: u32,
        fd: i32,
    }

    #[repr(C)]
    struct TimelineSignal {
        values: u64,
        count: u32,
        size: u32,
    }

    base::ioctl_iowr_nr!(TIMELINE_CREATE, 0x09, 0x58, TimelineValue);
    base::ioctl_iow_nr!(TIMELINE_SIGNAL, 0x09, 0x5b, TimelineSignal);
    base::ioctl_iowr_nr!(TIMELINE_FENCE_GET, 0x09, 0x5c, TimelineFence);
    base::ioctl_iow_nr!(TIMELINE_DESTROY, 0x09, 0x5d, u32);

    struct Timeline {
        device: File,
        id: Option<u32>,
    }

    impl Timeline {
        fn new() -> Self {
            let device = OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/kgsl-3d0")
                .unwrap();
            let mut value = TimelineValue::default();
            // SAFETY: mutable initialized UAPI buffer, valid for the whole ioctl.
            assert_eq!(
                unsafe { base::ioctl_with_mut_ref(&device, TIMELINE_CREATE, &mut value) },
                0,
                "timeline create: {}",
                std::io::Error::last_os_error()
            );
            Self {
                device,
                id: Some(value.id),
            }
        }

        fn fence(&self) -> SafeDescriptor {
            let mut value = TimelineFence {
                seqno: 1,
                id: self.id.unwrap(),
                fd: -1,
            };
            // SAFETY: mutable initialized UAPI buffer, as in msm_kgsl.h.
            assert_eq!(
                unsafe { base::ioctl_with_mut_ref(&self.device, TIMELINE_FENCE_GET, &mut value) },
                0,
                "timeline fence: {}",
                std::io::Error::last_os_error()
            );
            assert!(value.fd >= 0);
            // SAFETY: a successful ioctl returns a new fd owned by this call.
            unsafe { SafeDescriptor::from_raw_descriptor(value.fd) }
        }

        fn signal(&self) {
            let value = TimelineValue {
                seqno: 1,
                id: self.id.unwrap(),
                padding: 0,
            };
            let signal = TimelineSignal {
                values: &value as *const TimelineValue as u64,
                count: 1,
                size: size_of::<TimelineValue>() as u32,
            };
            // SAFETY: signal and its single pointed-to value live through ioctl.
            assert_eq!(
                unsafe { base::ioctl_with_ref(&self.device, TIMELINE_SIGNAL, &signal) },
                0,
                "timeline signal: {}",
                std::io::Error::last_os_error()
            );
        }

        fn destroy(&mut self) -> i32 {
            match self.id {
                Some(id) => {
                    // SAFETY: the ID belongs only to this test's private timeline.
                    let result =
                        unsafe { base::ioctl_with_ref(&self.device, TIMELINE_DESTROY, &id) };
                    if result == 0 {
                        self.id = None;
                    }
                    result
                }
                None => 0,
            }
        }
    }

    impl Drop for Timeline {
        fn drop(&mut self) {
            self.destroy();
        }
    }

    #[test]
    #[ignore = "requires access to Android KGSL timeline ioctls"]
    fn real_sync_files_wait_for_every_display_reader() {
        let mut test = Harness::new(true);
        let mut first = Timeline::new();
        let mut second = Timeline::new();
        test.frontend
            .virtio_gpu
            .inject_flip_fences_for_test(vec![first.fence(), second.fence()]);
        let mut command = request(0);
        command.flags = 0.into();
        test.submit(command.as_bytes(), 0, false);
        let wait = WaitContext::new().unwrap();
        test.frontend.register_flip_fences(&wait).unwrap();
        test.frontend
            .complete_flip_fences(&test.queues[0], &test.queues[1], &wait)
            .unwrap();
        assert_eq!(test.frontend.pending_flip_fences.len(), 2);
        assert_eq!(test.used(0), 0);
        second.signal();
        test.frontend
            .complete_flip_fences(&test.queues[0], &test.queues[1], &wait)
            .unwrap();
        assert_eq!(test.frontend.pending_flip_fences.len(), 1);
        assert_eq!(test.used(0), 0);
        first.signal();
        test.frontend
            .complete_flip_fences(&test.queues[0], &test.queues[1], &wait)
            .unwrap();
        assert!(test.frontend.pending_flip_fences.is_empty());
        assert_eq!([test.used(0), test.used(1)], [1, 0]);
        assert_eq!(first.destroy(), 0);
        assert_eq!(second.destroy(), 0);
    }

    #[test]
    #[ignore = "requires access to Android KGSL timeline ioctls"]
    fn destroyed_timeline_cannot_grant_display_buffer_reuse() {
        let mut test = Harness::new(true);
        let mut timeline = Timeline::new();
        test.frontend
            .virtio_gpu
            .inject_flip_fences_for_test(vec![timeline.fence()]);
        test.submit(request(19).as_bytes(), 1, true);
        assert_eq!(timeline.destroy(), 0);
        let wait = WaitContext::new().unwrap();
        assert!(test
            .frontend
            .complete_flip_fences(&test.queues[0], &test.queues[1], &wait)
            .is_err());
        assert_eq!(test.used(1), 0);
        assert_eq!(test.frontend.pending_flip_fences.len(), 1);
        assert!(!test.frontend.fence_state.lock().queue.is_empty());
    }
}

#[test]
fn global_cookies_survive_real_responses_and_correct_used_rings() {
    let mut test = Harness::new(false);
    for (index, cookie) in [u64::MAX, 0, 7, 7, 2].into_iter().enumerate() {
        let queue = index % 2;
        let before = [test.used(0), test.used(1)];
        let command = request(cookie);
        test.submit(command.as_bytes(), queue, true);
        assert_eq!([test.used(0), test.used(1)], before);
        let response = test.response(queue, size_of::<virtio_gpu_ctrl_hdr>());
        assert_eq!(response.fence_id.to_native(), cookie);
        assert_eq!(response.flags.to_native(), VIRTIO_GPU_FLAG_FENCE);
        assert_eq!(response.type_.to_native(), VIRTIO_GPU_RESP_OK_DISPLAY_INFO);
        let fence = *test.observed.lock().last().unwrap();
        assert_eq!(fence.fence_id, index as u64 + 1);
        test.callback.call(fence);
        assert_eq!(test.used(queue), before[queue] + 1);
        assert_eq!(test.used(1 - queue), before[1 - queue]);
        test.callback.call(fence);
        assert_eq!(test.used(queue), before[queue] + 1);
    }
}

#[test]
fn inline_global_completion_handles_descriptors_without_response_space() {
    let mut test = Harness::new(true);
    test.submit(request(u64::MAX).as_bytes(), 0, false);
    assert_eq!(test.used(0), 1);
    assert_eq!(test.observed.lock().len(), 1);
    assert!(test.frontend.fence_state.lock().queue.is_empty());
    assert!(test.frontend.fence_state.lock().global_tokens.is_empty());
}

#[test]
fn exhausted_tokens_reject_resource_creation_before_side_effects() {
    let mut test = Harness::new(false);
    assert!(test
        .frontend
        .fence_state
        .lock()
        .global_tokens
        .restore_last_issued(u32::MAX));
    let command = virtio_gpu_resource_create_2d {
        hdr: virtio_gpu_ctrl_hdr {
            type_: VIRTIO_GPU_CMD_RESOURCE_CREATE_2D.into(),
            ..request(0)
        },
        resource_id: 88.into(),
        format: 1.into(),
        width: 16.into(),
        height: 16.into(),
    };
    test.submit(command.as_bytes(), 0, true);
    assert_eq!(test.used(0), 1);
    assert!(test.observed.lock().is_empty());
    assert_eq!(
        test.response(0, size_of_val(&command)).type_.to_native(),
        VIRTIO_GPU_RESP_ERR_UNSPEC
    );
    assert!(matches!(
        test.frontend.virtio_gpu.resource_assign_uuid(88),
        Err(GpuResponse::ErrInvalidResourceId)
    ));
}

#[test]
fn suspended_callback_retains_completion_until_queue_is_available() {
    let mut test = Harness::new(false);
    test.submit(request(9).as_bytes(), 0, true);
    let activation = test.activation.lock().take();
    test.callback.call(test.observed.lock()[0]);
    assert_eq!(test.used(0), 0);
    assert!(test.frontend.fence_state.lock().global_tokens.is_empty());
    assert!(!test.frontend.fence_state.lock().queue.is_empty());
    *test.activation.lock() = activation;
    return_fenced_descriptors(
        test.frontend.fence_state.lock().queue.drain_ready(),
        &test.queues[0],
        Some(&test.queues[1]),
    );
    assert_eq!(test.used(0), 1);
}

#[test]
fn global_snapshot_rejects_legacy_or_invalid_watermark_without_reusing_tokens() {
    let mut state = FenceState {
        uses_global_tokens: true,
        ..Default::default()
    };
    let legacy = FenceStateSnapshot {
        completed_fences: BTreeMap::from([(VirtioGpuRing::Global, u64::MAX)]),
        global_tokens_last_issued: None,
        context_tokens_last_issued: None,
    };
    assert!(state.restore(legacy).is_err());
    let invalid = FenceStateSnapshot {
        completed_fences: BTreeMap::from([(VirtioGpuRing::Global, 9)]),
        global_tokens_last_issued: Some(8),
        context_tokens_last_issued: Some(0),
    };
    assert!(state.restore(invalid).is_err());
    assert_eq!(state.global_tokens.last_issued(), 0);
    let snapshot = state.snapshot().unwrap();
    let token = state.global_tokens.reserve().unwrap();
    state.global_tokens.cancel(token);
    state.restore(snapshot).unwrap();
    assert_eq!(state.global_tokens.reserve(), Some(token + 1));
}

#[test]
fn empty_submit_with_input_fences_is_rejected() {
    let mut test = Harness::new(true);
    let command = virtio_gpu_cmd_submit {
        hdr: virtio_gpu_ctrl_hdr {
            type_: VIRTIO_GPU_CMD_SUBMIT_3D.into(),
            ..request(17)
        },
        size: 0.into(),
        num_in_fences: 1.into(),
    };
    test.submit(command.as_bytes(), 0, true);
    assert_eq!(test.used(0), 1);
    assert_eq!(
        test.response(0, size_of_val(&command)).type_.to_native(),
        VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER
    );
}

#[test]
fn unsupported_shareable_flag_is_rejected_before_renderer_work() {
    let mut test = Harness::new(true);
    let mut command = request(17);
    command.flags = (VIRTIO_GPU_FLAG_FENCE | VIRTIO_GPU_FLAG_FENCE_HOST_SHAREABLE).into();
    test.submit(command.as_bytes(), 0, true);
    assert_eq!(test.used(0), 1);
    assert!(test.observed.lock().is_empty());
    assert_eq!(
        test.response(0, size_of_val(&command)).type_.to_native(),
        VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER
    );
}

#[test]
fn capset_selection_matches_global_fence_protocol_choice() {
    let cases = [
        (
            RutabagaComponentType::Gfxstream,
            1 << RUTABAGA_CAPSET_DRM,
            RutabagaComponentType::VirglRenderer,
        ),
        (
            RutabagaComponentType::VirglRenderer,
            0,
            RutabagaComponentType::VirglRenderer,
        ),
        (
            RutabagaComponentType::Gfxstream,
            0,
            RutabagaComponentType::Gfxstream,
        ),
        (
            RutabagaComponentType::VirglRenderer,
            1 << RUTABAGA_CAPSET_GFXSTREAM_VULKAN,
            RutabagaComponentType::Gfxstream,
        ),
        (
            RutabagaComponentType::Rutabaga2D,
            0,
            RutabagaComponentType::Rutabaga2D,
        ),
    ];
    for (requested, mask, selected) in cases {
        assert!(RutabagaBuilder::new(requested, mask).default_component_type() == selected);
    }
}
