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

type Activation = Arc<Mutex<Option<FenceHandlerActivationResources<SharedQueueReader>>>>;

#[cfg(feature = "android_display_stub")]
#[path = "native_geometry_tests.rs"]
mod native_geometry_tests;
#[cfg(feature = "android_display_stub")]
#[path = "native_blt_tests.rs"]
mod native_blt_tests;

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
        let rutabaga = RutabagaBuilder::new(component, 0)
            .build(renderer_callback, None)
            .unwrap();
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
        // Select the Virgl frontend protocol while retaining a deterministic
        // renderer. This test does not validate the real Virgl callback ABI.
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
        self.submit_capacity(command, queue, if response { 512 } else { 0 });
    }

    fn submit_capacity(&mut self, command: &[u8], queue: usize, capacity: u32) {
        self.enqueue_capacity(command, queue, capacity);
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

    fn enqueue_capacity(&mut self, command: &[u8], queue: usize, capacity: u32) {
        let response = capacity != 0;
        let base = if queue == 0 { 0x1000 } else { 0x3000 };
        // Up to four simultaneous requests per queue; do not rewrite memory
        // still referenced by a parked teardown/renderer descriptor.
        let head = (self.submitted[queue] % 4) * 2;
        let payload = base + 0x800 + u64::from(head / 2) * 0x400;
        let mut descriptors = vec![(DescriptorType::Readable, command.len() as u32)];
        if response {
            descriptors.push((DescriptorType::Writable, capacity));
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

#[test]
fn display_timing_requires_negotiation_and_never_substitutes_stub_cadence() {
    let mut test = Harness::new(false);
    let mut command = virtio_gpu_get_display_timing {
        hdr: virtio_gpu_ctrl_hdr { type_: VIRTIO_GPU_CMD_GET_DISPLAY_TIMING.into(), ..Default::default() },
        scanout_id: 0.into(), version: 1.into(), cookie: 9.into(),
    };
    test.submit(command.as_bytes(), 0, true);
    assert_eq!(test.used(0), 1);
    assert_eq!(test.response(0, size_of_val(&command)).type_.to_native(), VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
    test.frontend.display_timing_negotiated = true;
    test.submit(command.as_bytes(), 0, true);
    assert_eq!(test.response(0, size_of_val(&command)).type_.to_native(), VIRTIO_GPU_RESP_ERR_UNSPEC);
    command.cookie = 0.into();
    test.submit(command.as_bytes(), 0, true);
    assert_eq!(test.response(0, size_of_val(&command)).type_.to_native(), VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
    command.cookie = 10.into();
    command.scanout_id = 1.into();
    test.submit(command.as_bytes(), 0, true);
    assert_eq!(test.response(0, size_of_val(&command)).type_.to_native(), VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
    command.scanout_id = 0.into();
    command.version = 2.into();
    test.submit(command.as_bytes(), 0, true);
    assert_eq!(test.response(0, size_of_val(&command)).type_.to_native(), VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
    assert!(test.observed.lock().is_empty()); // No renderer submit/fence to answer timing.
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

#[cfg(feature = "android_display_stub")]
#[test]
fn display_timing_rejects_malformed_descriptors_before_provider_or_fences() {
    use std::rc::Rc;
    use gpu_display::native_refresh_test::{self, Consumer};
    let mut test = Harness::new(false);
    let consumer = Rc::new(Consumer::default());
    test.frontend.virtio_gpu.set_display_timing_provider_for_test(
        native_refresh_test::open(consumer.clone()).unwrap());
    test.frontend.display_timing_negotiated = true;
    let plain = virtio_gpu_get_display_timing {
        hdr: virtio_gpu_ctrl_hdr { type_: VIRTIO_GPU_CMD_GET_DISPLAY_TIMING.into(), ..Default::default() },
        scanout_id: 0.into(), version: 1.into(), cookie: 0x1122334455667788.into(),
    };
    let mut malformed = Vec::new();
    for flags in [VIRTIO_GPU_FLAG_FENCE, VIRTIO_GPU_FLAG_INFO_RING_IDX,
                  VIRTIO_GPU_FLAG_FENCE_HOST_SHAREABLE, 0x8000_0000] {
        let mut command = plain;
        command.hdr.flags = flags.into();
        malformed.push(command);
    }
    let mut command = plain; command.hdr.fence_id = 17.into(); malformed.push(command);
    let mut command = plain; command.hdr.ctx_id = 7.into(); malformed.push(command);
    let mut command = plain; command.hdr.ring_idx = 1; malformed.push(command);
    for index in 0..3 {
        let mut command = plain; command.hdr.padding[index] = 1; malformed.push(command);
    }
    for command in malformed {
        test.submit(command.as_bytes(), 0, true);
        assert_eq!(test.used(0), test.submitted[0]); // No parked renderer completion.
        let response = test.response(0, size_of_val(&command));
        assert_eq!(response.type_.to_native(), VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
        assert_eq!(response.flags.to_native(), 0);
        assert_eq!(response.fence_id.to_native(), 0);
        assert_eq!(response.ctx_id.to_native(), 0);
        assert_eq!(response.ring_idx, 0);
        assert_eq!(response.padding, [0; 3]);
    }
    test.submit(plain.as_bytes(), 1, true);
    assert_eq!(test.used(1), 1);
    assert_eq!(test.response(1, size_of_val(&plain)).type_.to_native(), VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
    let mut trailing = plain.as_bytes().to_vec(); trailing.push(0);
    test.submit(&trailing, 0, true);
    assert_eq!(test.response(0, trailing.len()).type_.to_native(), VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
    for capacity in [0, 1, 23, 24, 32, 80, 111] {
        let payload = 0x1800 + u64::from(test.submitted[0] % 4) * 0x400;
        let address = GuestAddress(payload + size_of_val(&plain) as u64);
        test.mem.write_all_at_addr(&[0xa5; 128], address).unwrap();
        test.submit_capacity(plain.as_bytes(), 0, capacity);
        assert_eq!(test.used(0), test.submitted[0]);
        let slot = u64::from((test.used(0) - 1) % 8);
        let written = test.mem.read_obj_from_addr::<Le32>(GuestAddress(0x1308 + slot * 8)).unwrap().to_native();
        assert_eq!(written, if capacity >= 24 { 24 } else { 0 });
        if capacity >= 24 {
            assert_eq!(test.response(0, size_of_val(&plain)).type_.to_native(), VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
        }
        let mut untouched = [0; 104];
        test.mem.read_exact_at_addr(&mut untouched, GuestAddress(address.0 + 24)).unwrap();
        assert_eq!(untouched, [0xa5; 104]); // Never a partial timing sample.
    }
    assert_eq!(consumer.timing_queries.get(), 0);
    assert!(test.observed.lock().is_empty());
}

#[cfg(feature = "android_display_stub")]
#[test]
fn display_timing_valid_provider_roundtrips_cookie_epochs_and_unknown_accuracy() {
    use std::rc::Rc;
    use gpu_display::native_refresh_test::{self, Consumer};
    let mut test = Harness::new(false);
    let consumer = Rc::new(Consumer::default());
    let sample = gpu_display::DisplayTimingSample {
        version: 1, size: 80, flags: 1, attachment_epoch: 47, mode_epoch: 23,
        sequence: 51, host_now_ns: 1_000_000_000, present_ns: 999_000_000,
        latch_ns: 998_000_000, uncertainty_ns: u64::MAX, ..Default::default()
    };
    consumer.timing.set(Some(sample));
    test.frontend.virtio_gpu.set_display_timing_provider_for_test(
        native_refresh_test::open(consumer.clone()).unwrap());
    test.frontend.display_timing_negotiated = true;
    let command = virtio_gpu_get_display_timing {
        hdr: virtio_gpu_ctrl_hdr { type_: VIRTIO_GPU_CMD_GET_DISPLAY_TIMING.into(), ..Default::default() },
        scanout_id: 0.into(), version: 1.into(), cookie: 0x8877665544332211.into(),
    };
    test.submit_capacity(command.as_bytes(), 0, 112);
    assert_eq!(test.used(0), 1);
    assert_eq!(test.mem.read_obj_from_addr::<Le32>(GuestAddress(0x1308)).unwrap().to_native(), 112);
    let header = test.response(0, size_of_val(&command));
    assert_eq!(header.type_.to_native(), VIRTIO_GPU_RESP_OK_DISPLAY_TIMING);
    assert_eq!(&header.as_bytes()[4..], &[0; 20]);
    let address = 0x1800 + size_of_val(&command) as u64;
    let word = |offset| test.mem.read_obj_from_addr::<Le32>(GuestAddress(address + offset)).unwrap().to_native();
    let wide = |offset| test.mem.read_obj_from_addr::<Le64>(GuestAddress(address + offset)).unwrap().to_native();
    assert_eq!(wide(24), command.cookie.to_native());
    assert_eq!([word(32), word(36), word(40), word(44)], [1, 80, 1, 0]);
    assert_eq!([wide(48), wide(56), wide(64)], [47, 23, 51]);
    assert_eq!([wide(72), wide(80), wide(88)], [1_000_000_000, 999_000_000, 998_000_000]);
    assert_eq!([wide(96), wide(104)], [0, u64::MAX]);
    assert_eq!(consumer.timing_queries.get(), 1);
    consumer.timing.set(Some(gpu_display::DisplayTimingSample { reserved: 1, ..sample }));
    test.submit(command.as_bytes(), 0, true);
    assert_eq!(test.response(0, size_of_val(&command)).type_.to_native(), VIRTIO_GPU_RESP_ERR_UNSPEC);
    assert_eq!(consumer.timing_queries.get(), 2);
    assert!(test.observed.lock().is_empty());
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
    test.frontend.pending_flip_fences[1].deadline = Some(std::time::Instant::now());
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

#[cfg(any(target_os = "android", target_os = "linux"))]
#[test]
fn explicitly_typed_android_release_event_completes_after_signal() {
    let mut test = Harness::new(true);
    let event = Event::new().unwrap();
    let release_fd = SafeDescriptor::try_from(&event as &dyn AsRawDescriptor).unwrap();
    test.frontend
        .virtio_gpu
        .inject_flip_completion_fences_for_test(vec![FlipCompletionFence::AndroidReleaseEvent(
            release_fd,
        )]);
    test.submit(request(8).as_bytes(), 0, false);
    let wait = WaitContext::new().unwrap();
    test.frontend.register_flip_fences(&wait).unwrap();

    test.frontend
        .complete_flip_fences(&test.queues[0], &test.queues[1], &wait)
        .unwrap();
    assert_eq!(test.used(0), 0);
    event.signal().unwrap();
    test.frontend
        .complete_flip_fences(&test.queues[0], &test.queues[1], &wait)
        .unwrap();
    assert_eq!(test.used(0), 1);
    assert!(test.frontend.pending_flip_fences.is_empty());
}

#[test]
fn deferred_native_ahb_id_blocks_reuse_and_reset_until_last_reader() {
    let mut test = Harness::new(true);
    let resource_id = 73;

    // Model RESOURCE_UNREF after the frontend took the display fence, with
    // renderer unref failing before ownership could move into the holder map.
    test.frontend
        .virtio_gpu
        .inject_deferred_native_ahb_id_for_test(resource_id);

    let create_3d = ResourceCreate3D {
        target: RUTABAGA_PIPE_TEXTURE_2D,
        format: 1,
        bind: RUTABAGA_PIPE_BIND_RENDER_TARGET,
        width: 16,
        height: 16,
        depth: 1,
        array_size: 1,
        last_level: 0,
        nr_samples: 0,
        flags: 0,
    };
    assert!(matches!(
        test.frontend
            .virtio_gpu
            .resource_create_3d(resource_id, create_3d),
        Err(GpuResponse::ErrInvalidResourceId)
    ));

    let create_blob = ResourceCreateBlob {
        blob_mem: VIRTIO_GPU_BLOB_MEM_HOST3D,
        blob_flags: 0,
        blob_id: 0,
        size: 4096,
    };
    assert!(matches!(
        test.frontend.virtio_gpu.resource_create_blob(
            0,
            resource_id,
            create_blob,
            Vec::new(),
            &test.mem,
        ),
        Err(GpuResponse::ErrInvalidResourceId)
    ));
    assert!(test.frontend.virtio_gpu.reset().is_err());

    test.frontend
        .virtio_gpu
        .release_deferred_native_ahb(resource_id, true);
    assert!(test.frontend.virtio_gpu.has_deferred_native_ahb());
    test.frontend
        .virtio_gpu
        .release_deferred_native_ahb(resource_id, false);
    assert!(!test.frontend.virtio_gpu.has_deferred_native_ahb());
}

#[cfg(any(target_os = "android", target_os = "linux"))]
#[test]
fn gpu_reset_keeps_undelivered_android_reader_and_resource() {
    let mut test = Harness::new(true);
    let resource_id = 73;
    let create = ResourceCreate3D {
        target: RUTABAGA_PIPE_TEXTURE_2D, format: 1,
        bind: RUTABAGA_PIPE_BIND_RENDER_TARGET, width: 16, height: 16,
        depth: 1, array_size: 1, last_level: 0, nr_samples: 0, flags: 0,
    };
    test.frontend.virtio_gpu.resource_create_3d(resource_id, create).unwrap();
    let event = Event::new().unwrap();
    let release = SafeDescriptor::try_from(&event as &dyn AsRawDescriptor).unwrap();
    test.frontend.virtio_gpu.inject_flip_completion_fences_for_test(vec![
        FlipCompletionFence::AndroidReleaseEvent(release),
    ]);
    assert!(test.frontend.virtio_gpu.reset().is_err());
    // Failed reset preserves the old id; an unsignalled reader cannot be
    // forgotten just because the frontend has not taken its fence yet.
    assert!(test.frontend.virtio_gpu.resource_create_3d(resource_id, create).is_err());
    assert_eq!(test.frontend.virtio_gpu.take_pending_flip_fences().len(), 1);
}

#[cfg(any(target_os = "android", target_os = "linux"))]
mod reset_worker {
    use super::*;

    #[test]
    fn profile_prepare_failure_does_not_install_guest_callback_targets() {
        let (mut worker, _, mut next) = fixture(); // No configured primary scanout.
        worker.activation_resources = None;
        worker.fence_handler_resources.lock().take();
        next.native_geometry_negotiated = true;
        let error = worker.on_activate(next).unwrap_err();
        assert!(error.to_string().contains("prepare native profile"));
        assert!(worker.activation_resources.is_none());
        assert!(worker.fence_handler_resources.lock().is_none());
        assert!(!worker.state.native_geometry_negotiated);
        assert_eq!(worker.last_native_profile, None);
        assert!(!worker.native_profile_event.load(Ordering::Acquire));
    }

    #[cfg(feature = "android_display_stub")]
    #[test]
    fn native_refresh_config_notification_is_per_attachment_and_never_submits_pixels() {
        fn observe(worker: &mut Worker) -> bool {
            Worker::observe_native_refresh_attachment(&mut worker.state,
                &mut worker.last_native_refresh_generation, &worker.native_refresh_event)
        }
        let (mut worker, _, _) = fixture();
        let consumer = Rc::new(gpu_display::native_refresh_test::Consumer::default());
        let display = gpu_display::native_refresh_test::open(consumer.clone()).unwrap();
        *worker.state.display().borrow_mut() = display;
        assert!(!observe(&mut worker));
        consumer.generation.set(1);
        assert!(observe(&mut worker));
        assert!(worker.native_refresh_event.load(Ordering::Acquire));
        for _ in 0..1000 { assert!(!observe(&mut worker)); }
        worker.native_refresh_event.store(false, Ordering::Release); // guest ack
        assert!(!observe(&mut worker));
        consumer.generation.set(3);
        assert!(observe(&mut worker));
        consumer.generation.set(5);
        assert!(observe(&mut worker));
        assert!(worker.native_refresh_event.load(Ordering::Acquire));
        consumer.presented.set(5); // a normal guest paint wins the race
        assert!(!observe(&mut worker));
        consumer.generation.set(0);
        assert!(!observe(&mut worker));
        assert!(consumer.submissions.borrow().is_empty());
        assert!(worker.state.pending_flip_fences.is_empty());
    }

    fn fixture() -> (Worker, base::RecvTube, WorkerActivateRequest) {
        fixture_from(Harness::new(true))
    }

    pub(super) fn fixture_from(test: Harness) -> (Worker, base::RecvTube, WorkerActivateRequest) {
        let (exit_tx, exit_rx) = Tube::directional_pair().unwrap();
        let (control, _) = Tube::pair().unwrap();
        let (_, request_receiver) = mpsc::channel();
        let (response_sender, _) = mpsc::channel();
        let resources = || GpuActivationResources {
            mem: test.mem.clone(), interrupt: Interrupt::new_for_test(),
            ctrl_queue: test.queues[0].clone(), cursor_queue: test.queues[1].clone(),
        };
        let next = WorkerActivateRequest { resources: resources(), display_timing_negotiated: false,
            native_geometry_negotiated: false, native_blt_negotiated: false };
        let active = resources();
        let worker = Worker {
            ctrl_resignals: 0, unkicked_ctrl_drains: 0,
            native_refresh_event: Arc::new(AtomicBool::new(false)),
            last_native_refresh_generation: 0,
            native_profile_event: Arc::new(AtomicBool::new(false)),
            last_native_profile: None,
            request_receiver, response_sender, exit_evt_wrtube: exit_tx,
            gpu_control_tube: control, resource_bridges: ResourceBridges::new(Vec::new()),
            suspend_evt: Event::new().unwrap(), kill_evt: Event::new().unwrap(),
            fence_state: test.frontend.fence_state.clone(),
            fence_handler_resources: test.activation,
            state: test.frontend, activation_resources: Some(active), external_scanout: None,
        };
        (worker, exit_rx, next)
    }

    #[cfg(feature = "android_display_stub")]
    #[test]
    fn transient_replacement_worker_retries_without_another_guest_wakeup() {
        let (test, consumer, _) = super::native_ahb_release::retry_fixture();
        let mem = test.mem.clone();
        let (mut worker, _exit, _next) = fixture_from(test);
        let (control, _control_peer) = Tube::pair().unwrap();
        worker.gpu_control_tube = control;
        let stop = worker.kill_evt.try_clone().unwrap();
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            stop.signal().unwrap();
        });
        assert!(matches!(worker.run_until_sleep_or_exit().unwrap(), WorkerStopReason::Kill));
        thread.join().unwrap();
        assert_eq!(consumer.submissions.borrow().len(), 2);
        assert_eq!(consumer.attempts.get(), 3);
        assert!(!worker.state.native_ahb_present_pending(12));
        assert!(worker.state.native_ahb_has_reader(11));
        assert!(worker.state.native_ahb_has_reader(12));
        let reply: virtio_gpu_native_ahb_operation = mem.read_obj_from_addr(GuestAddress(0x1c28)).unwrap();
        assert_eq!(reply.hdr.type_.to_native(), VIRTIO_GPU_RESP_OK_NATIVE_AHB_OPERATION);
        assert_eq!(mem.read_obj_from_addr::<Le16>(GuestAddress(0x1302)).unwrap().to_native(), 2);
    }

    #[cfg(feature = "android_display_stub")]
    #[test]
    fn transient_replacement_renderer_error_blocks_teardown_cancellation_publication() {
        for suspend in [false, true] {
            let (test, consumer, _) = super::native_ahb_release::retry_fixture();
            let mem = test.mem.clone();
            let token = test.frontend.fence_state.lock().context_tokens.reserve(9, 0).unwrap();
            create_fence_error_handler(test.frontend.fence_state.clone()).call(RutabagaFenceError {
                ctx_id: 9, ring_idx: 0, fence_id: token, error: -libc::EIO,
            });
            let (mut worker, _exit, _next) = fixture_from(test);
            if suspend { assert!(worker.on_suspend().is_err()); }
            else { assert!(worker.on_reset().is_err()); }
            assert_eq!(mem.read_obj_from_addr::<Le16>(GuestAddress(0x1302)).unwrap().to_native(), 1);
            assert_eq!(consumer.submissions.borrow().len(), 1);
            assert!(worker.state.native_ahb_has_reader(11));
            assert!(!worker.state.native_ahb_has_reader(12));
            assert_eq!(worker.state.native_ahb_release.released, 0);
            assert!(!worker.fence_state.lock().queue.is_empty());
            assert!(worker.fence_state.lock().queue.drain_ready().is_empty());
            assert!(worker.activation_resources.is_some());
        }
    }

    #[cfg(feature = "android_display_stub")]
    #[test]
    fn transient_replacement_reset_and_suspend_cancel_before_reader_retirement() {
        for suspend in [false, true] {
            let (test, consumer, _) = super::native_ahb_release::retry_fixture();
            let mem = test.mem.clone();
            consumer.submissions.borrow()[0].1.signal().unwrap();
            let (mut worker, _exit, next) = fixture_from(test);
            if suspend {
                drop(next);
                worker.on_suspend().unwrap();
            } else {
                worker.on_reset_with_drain_timeout(Duration::from_millis(100)).unwrap();
            }
            let reply: virtio_gpu_native_ahb_operation = mem.read_obj_from_addr(GuestAddress(0x1c28)).unwrap();
            assert_eq!(reply.hdr.type_.to_native(), VIRTIO_GPU_RESP_ERR_UNSPEC);
            assert_eq!(mem.read_obj_from_addr::<Le16>(GuestAddress(0x1302)).unwrap().to_native(), 2);
            assert_eq!(consumer.submissions.borrow().len(), 1);
            assert!(!worker.state.native_ahb_present_pending(12));
            assert!(!worker.state.native_ahb_release.has_waiters());
            assert_eq!(worker.state.native_ahb_release.released, 1);
            assert!(worker.activation_resources.is_none());
        }
    }

    #[cfg(feature = "android_display_stub")]
    #[test]
    fn reset_and_suspend_cancel_native_refresh_before_reader_retirement() {
        for suspend in [false, true] {
            let (mut test, consumer, sequence) = super::native_ahb_release::refresh_fixture();
            consumer.generation.set(3);
            let request = virtio_gpu_native_ahb_operation {
                hdr: virtio_gpu_ctrl_hdr { type_: VIRTIO_GPU_CMD_REFRESH_NATIVE_AHB.into(), ..Default::default() },
                resource_id: 11.into(), sequence: sequence.into(), ..Default::default()
            };
            test.submit(request.as_bytes(), 0, true);
            assert_eq!(test.used(0), 1);
            let mem = test.mem.clone();
            // Completion is already readable when reset starts. Cancellation
            // must precede its sweep or that sweep would admit a new reader.
            consumer.submissions.borrow()[0].1.signal().unwrap();
            let (mut worker, _exit, next) = fixture_from(test);
            let next = if suspend {
                // Suspend returns exclusive queue owners; the reusable reset
                // activation fixture must not retain extra queue references.
                drop(next);
                worker.on_suspend().unwrap();
                None
            } else {
                worker.on_reset_with_drain_timeout(Duration::from_millis(100)).unwrap();
                Some(next)
            };
            let reply: virtio_gpu_native_ahb_operation =
                mem.read_obj_from_addr(GuestAddress(0x1c28)).unwrap();
            assert_eq!(reply.hdr.type_.to_native(), VIRTIO_GPU_RESP_OK_NATIVE_AHB_OPERATION);
            assert_eq!(reply.sequence.to_native(), sequence);
            assert_eq!(mem.read_obj_from_addr::<Le16>(GuestAddress(0x1302)).unwrap().to_native(), 2);
            assert_eq!(consumer.submissions.borrow().len(), 1);
            assert_eq!(worker.state.native_ahb_release.released, 1);
            assert!(!worker.state.native_ahb_release.has_waiters());
            assert!(worker.state.pending_flip_fences.is_empty());
            assert!(worker.activation_resources.is_none());
            if let Some(next) = next { worker.on_activate(next).unwrap(); }
        }
    }

    #[test]
    fn failed_reset_preserves_activation_stops_completions_and_requests_exit() {
        let (mut worker, exit, next) = fixture();
        worker.state.display_timing_negotiated = true;
        worker.state.native_ahb_release.sequences.insert(23, 17);
        worker.state.native_ahb_paging.poisoned.insert(23);
        let event = Event::new().unwrap();
        let release = SafeDescriptor::try_from(&event as &dyn AsRawDescriptor).unwrap();
        // This reader is still inside VirtioGpu, exercising the reset error
        // branch itself rather than the worker's earlier frontend guard.
        worker.state.virtio_gpu.inject_flip_completion_fences_for_test(vec![
            FlipCompletionFence::AndroidReleaseEvent(release),
        ]);
        assert!(worker.on_reset().is_err());
        assert!(worker.state.display_failed);
        assert!(worker.activation_resources.is_some());
        assert!(worker.fence_handler_resources.lock().is_some());
        assert_eq!(worker.state.native_ahb_release.sequences.get(&23), Some(&17));
        assert!(worker.state.native_ahb_paging.poisoned.contains(&23));
        assert_eq!(worker.state.virtio_gpu.take_pending_flip_fences().len(), 1);
        let wait = WaitContext::build_with(&[(exit.get_read_notifier(), WorkerToken::Kill)]).unwrap();
        assert!(!wait.wait_timeout(Duration::from_secs(1)).unwrap().is_empty());
        assert!(matches!(exit.recv::<VmEventType>().unwrap(), VmEventType::Exit));
        // Even a late ready descriptor stays owned after reset failure.
        let descriptor = ReturnDescriptor {
            desc_chain: create_descriptor_chain(&next.resources.mem, GuestAddress(0x5000),
                GuestAddress(0x5800), vec![(DescriptorType::Writable, 24)], 0).unwrap(),
            len: 24, queue: GpuQueue::Control, ctx_id: 0,
        };
        worker.fence_state.lock().queue.push_complete(VirtioGpuRing::Global, descriptor);
        assert!(worker.fence_state.lock().queue.drain_ready().is_empty());
        assert!(!worker.fence_state.lock().queue.is_empty());
        assert!(worker.on_activate(next).is_err());
        assert!(worker.state.display_timing_negotiated); // Refused activation never adopts new flags.
    }

    #[test]
    fn successful_reset_retires_identity_and_allows_next_activation() {
        let (mut worker, _exit, mut next) = fixture();
        worker.state.display_timing_negotiated = true;
        next.display_timing_negotiated = true;
        worker.state.native_ahb_release.sequences.insert(23, 17);
        worker.state.native_ahb_release.accepted = 19;
        worker.state.native_ahb_release.released = 19;
        worker.state.native_ahb_paging.poisoned.insert(23);
        worker.state.native_ahb_paging.operations = 3;
        worker.state.native_ahb_paging.completed_read_bytes = 65536;
        worker.on_reset().unwrap();
        assert!(!worker.state.display_timing_negotiated);
        assert!(!worker.state.display_failed);
        assert!(worker.activation_resources.is_none());
        assert!(worker.fence_handler_resources.lock().is_none());
        assert!(worker.state.native_ahb_release.sequences.is_empty());
        assert!(worker.state.native_ahb_paging.poisoned.is_empty());
        assert_eq!(worker.state.native_ahb_release.accepted, 19);
        assert_eq!(worker.state.native_ahb_release.released, 19);
        assert_eq!(worker.state.native_ahb_paging.operations, 3);
        assert_eq!(worker.state.native_ahb_paging.completed_read_bytes, 65536);
        worker.on_activate(next).unwrap();
        assert!(worker.state.display_timing_negotiated);
        assert!(worker.activation_resources.is_some());
        assert!(worker.fence_handler_resources.lock().is_some());
    }

    fn park_native_reader(worker: &mut Worker) -> Event {
        let event = Event::new().unwrap();
        worker.state.pending_flip_fences.push(PendingFlipFence {
            resource_id: 73,
            fence_id: 17,
            ticket: None,
            fence: FlipCompletionFence::AndroidReleaseEvent(
                SafeDescriptor::try_from(&event as &dyn AsRawDescriptor).unwrap(),
            ),
            deadline: Some(std::time::Instant::now() + FLIP_FENCE_TIMEOUT),
            registered: false,
        });
        event
    }

    #[test]
    fn reset_waits_for_real_renderer_completion_with_or_without_display_reader() {
        for display_reader in [false, true] {
            let mut test = Harness::new(false);
            test.submit(request(0x1234).as_bytes(), 0, true);
            assert_eq!(test.used(0), 0);
            let fence = test.observed.lock()[0];
            let callback = test.callback.clone();
            let mem = test.mem.clone();
            let (mut worker, _exit, next) = fixture_from(test);
            let dropped = Arc::new(AtomicBool::new(false));
            worker.state.virtio_gpu.inject_native_ahb_for_test(73, Box::new(ResetAllocation(dropped.clone())));
            let reader = display_reader.then(|| park_native_reader(&mut worker));
            let observed_drop = dropped.clone();
            let observed_mem = mem.clone();
            let fences = worker.fence_state.clone();
            let thread = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(20));
                if let Some(reader) = reader { reader.signal().unwrap(); }
                std::thread::sleep(Duration::from_millis(20));
                assert!(!observed_drop.load(Ordering::SeqCst));
                assert_eq!(observed_mem.read_obj_from_addr::<Le16>(GuestAddress(0x1302)).unwrap().to_native(), 0);
                assert!(fences.lock().waiting_for_reset);
                callback.call(fence); // Actual production callback retires the token and descriptor.
            });
            let started = std::time::Instant::now();
            worker.on_reset_with_drain_timeout(Duration::from_secs(2)).unwrap();
            thread.join().unwrap();
            assert!(started.elapsed() < Duration::from_secs(1)); // Completion woke reset, not its deadline.
            assert_eq!(mem.read_obj_from_addr::<Le16>(GuestAddress(0x1302)).unwrap().to_native(), 1);
            assert!(dropped.load(Ordering::SeqCst));
            assert!(!worker.fence_state.lock().waiting_for_reset);
            assert!(worker.fence_state.lock().global_tokens.is_empty());
            assert!(worker.fence_state.lock().queue.is_empty());
            worker.on_reset_with_drain_timeout(Duration::ZERO).unwrap();
            worker.on_activate(next).unwrap();
        }
    }

    #[test]
    fn reset_context_fences_require_matching_callback_not_retirement_wakeup() {
        for complete in [false, true] {
            let mut test = Harness::with_contexts(false);
            create_context(&mut test, 9);
            let mut command = context_request(9, 0x1234, 0);
            command.type_ = VIRTIO_GPU_CMD_GET_DISPLAY_INFO.into();
            test.submit(command.as_bytes(), 0, true);
            assert_eq!(test.used(0), 1);
            let fence = *test.observed.lock().last().unwrap();
            let callback = test.callback.clone();
            let mem = test.mem.clone();
            let (mut worker, exit, next) = fixture_from(test);
            let wakeup = worker.state.context_retirement_event().unwrap();
            let dropped = Arc::new(AtomicBool::new(false));
            worker.state.virtio_gpu.inject_native_ahb_for_test(73, Box::new(ResetAllocation(dropped.clone())));
            let observed_drop = dropped.clone();
            let fences = worker.fence_state.clone();
            let thread = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(10));
                callback.call(RutabagaFence { ctx_id: 10, ..fence }); // Wrong owner is not completion.
                wakeup.signal().unwrap(); // A wakeup also grants no ownership.
                std::thread::sleep(Duration::from_millis(10));
                assert!(!observed_drop.load(Ordering::SeqCst));
                assert!(!fences.lock().context_tokens.is_empty());
                if complete { callback.call(fence); }
            });
            let result = worker.on_reset_with_drain_timeout(Duration::from_millis(150));
            thread.join().unwrap();
            assert!(!worker.fence_state.lock().waiting_for_reset);
            if complete {
                result.unwrap();
                assert!(dropped.load(Ordering::SeqCst));
                assert_eq!(mem.read_obj_from_addr::<Le16>(GuestAddress(0x1302)).unwrap().to_native(), 2);
                worker.on_activate(next).unwrap();
            } else {
                assert!(result.is_err());
                assert!(!dropped.load(Ordering::SeqCst));
                assert!(!worker.fence_state.lock().context_tokens.is_empty());
                assert!(!worker.fence_state.lock().queue.is_empty());
                assert!(worker.activation_resources.is_some());
                assert!(worker.fence_handler_resources.lock().is_some());
                assert!(matches!(exit.recv::<VmEventType>().unwrap(), VmEventType::Exit));
                assert!(worker.on_activate(next).is_err());
                assert_eq!(mem.read_obj_from_addr::<Le16>(GuestAddress(0x1302)).unwrap().to_native(), 1);
            }
        }
    }

    #[test]
    fn sleep_then_reset_observes_ready_android_release() {
        for release_before_sleep in [true, false] {
            let (mut worker, _exit, next) = fixture();
            // Keep the control peer alive while running the actual worker wait loop.
            let (control, _control_peer) = Tube::pair().unwrap();
            worker.gpu_control_tube = control;
            let event = park_native_reader(&mut worker);
            worker.state.virtio_gpu.inject_deferred_native_ahb_id_for_test(73);
            if release_before_sleep {
                event.signal().unwrap();
            }
            worker.suspend_evt.signal().unwrap();
            assert!(matches!(worker.run_until_sleep_or_exit().unwrap(), WorkerStopReason::Sleep));
            worker.suspend_evt.wait().unwrap();
            assert_eq!(worker.state.pending_flip_fences.len(), 1);
            if !release_before_sleep {
                event.signal().unwrap();
            }
            worker.on_reset().unwrap();
            assert!(worker.state.pending_flip_fences.is_empty());
            assert!(!worker.state.virtio_gpu.has_deferred_native_ahb());
            assert_eq!(worker.state.native_ahb_release.released, 1);
            assert!(!worker.state.display_failed);
            worker.on_activate(next).unwrap();
        }
    }

    #[test]
    fn sleep_then_reset_retains_unsignaled_android_release() {
        let (mut worker, exit, _next) = fixture();
        let (control, _control_peer) = Tube::pair().unwrap();
        worker.gpu_control_tube = control;
        let _event = park_native_reader(&mut worker);
        worker.suspend_evt.signal().unwrap();
        assert!(matches!(worker.run_until_sleep_or_exit().unwrap(), WorkerStopReason::Sleep));
        worker.suspend_evt.wait().unwrap();
        assert!(worker.on_reset().is_err());
        assert_eq!(worker.state.pending_flip_fences.len(), 1);
        assert_eq!(worker.state.native_ahb_release.released, 0);
        assert!(worker.state.display_failed);
        assert!(worker.activation_resources.is_some());
        assert!(matches!(exit.recv::<VmEventType>().unwrap(), VmEventType::Exit));
    }

    #[test]
    fn reset_waits_for_delayed_real_release_without_holding_renderer_lock() {
        let (mut worker, _exit, mut next) = fixture();
        worker.state.native_geometry_negotiated = true;
        worker.last_native_profile = Some(native_geometry::ProfileObservation::Unavailable);
        worker.native_profile_event.store(true, Ordering::Release);
        let event = park_native_reader(&mut worker);
        // A current front has no watchdog; only the reset drain can bound it.
        worker.state.pending_flip_fences[0].deadline = None;
        worker.state.virtio_gpu.inject_deferred_native_ahb_id_for_test(73);
        let fence_state = worker.fence_state.clone();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            // A real callback may need this lock before waking the worker.
            // Reset must not hold it while waiting for consumer completion.
            let _state = fence_state.lock();
            event.signal().unwrap();
        });
        let result = worker.on_reset();
        release.join().unwrap();
        result.unwrap();
        assert!(worker.state.pending_flip_fences.is_empty());
        assert!(!worker.state.virtio_gpu.has_deferred_native_ahb());
        assert_eq!(worker.state.native_ahb_release.released, 1);
        assert!(!worker.state.native_geometry_negotiated);
        assert_eq!(worker.last_native_profile, None);
        assert!(!worker.native_profile_event.load(Ordering::Acquire));
        next.native_geometry_negotiated = true;
        worker.state.virtio_gpu.set_native_geometry_display_without_surface_for_test(
            GpuDisplay::open_stub().unwrap(), 4, 2);
        worker.on_activate(next).unwrap();
        assert!(worker.state.native_geometry_negotiated);
        assert!(worker.last_native_profile.is_some());
        assert!(worker.native_profile_event.load(Ordering::Acquire));
    }

    struct ResetAllocation(Arc<AtomicBool>);
    impl HostSharedAllocation for ResetAllocation {
        fn description(&self) -> SharedAllocationDescription { Default::default() }
        fn duplicate_descriptor(&self) -> anyhow::Result<SafeDescriptor> {
            Err(anyhow!("reset fixture does not DMA import"))
        }
    }
    impl Drop for ResetAllocation {
        fn drop(&mut self) { self.0.store(true, Ordering::SeqCst); }
    }

    fn park_wait_and_allocation(worker: &mut Worker) -> (GuestMemory, Arc<AtomicBool>) {
        let dropped = Arc::new(AtomicBool::new(false));
        worker.state.virtio_gpu.inject_native_ahb_for_test(73, Box::new(ResetAllocation(dropped.clone())));
        worker.state.native_ahb_release.sequences.insert(73, 17);
        let mem = worker.activation_resources.as_ref().unwrap().mem.clone();
        let descriptor = create_descriptor_chain(&mem, GuestAddress(0x5000),
            GuestAddress(0x6000), vec![(DescriptorType::Writable, 40)], 0).unwrap();
        let request = virtio_gpu_native_ahb_operation {
            hdr: virtio_gpu_ctrl_hdr { type_: VIRTIO_GPU_CMD_WAIT_NATIVE_AHB_RELEASE.into(), ..Default::default() },
            resource_id: 73.into(), sequence: 17.into(), ..Default::default()
        };
        assert!(worker.state.process_native_ahb_operation(request, descriptor, GpuQueue::Control).is_none());
        (mem, dropped)
    }

    #[test]
    fn reset_waits_for_last_reader_returns_old_wait_and_admits_no_new_commands() {
        let (mut worker, _exit, _next) = fixture();
        let first = park_native_reader(&mut worker);
        let second = park_native_reader(&mut worker);
        let (mem, dropped) = park_wait_and_allocation(&mut worker);
        // Queue a fresh control command without notifying/processing it. The
        // reset drain must publish only the old WAIT, never consume this head.
        let command = virtio_gpu_ctrl_hdr { type_: VIRTIO_GPU_CMD_GET_DISPLAY_INFO.into(), ..Default::default() };
        drop(create_descriptor_chain(&mem, GuestAddress(0x1020), GuestAddress(0x6800),
            vec![(DescriptorType::Readable, 24), (DescriptorType::Writable, 512)], 0).unwrap());
        mem.write_obj_at_addr(Le16::from(3), GuestAddress(0x102e)).unwrap();
        mem.write_all_at_addr(command.as_bytes(), GuestAddress(0x6800)).unwrap();
        mem.write_obj_at_addr(Le16::from(2), GuestAddress(0x1204)).unwrap();
        mem.write_obj_at_addr(Le16::from(1), GuestAddress(0x1202)).unwrap();
        let observed_mem = mem.clone();
        let observed_drop = dropped.clone();
        let release = std::thread::spawn(move || {
            first.signal().unwrap();
            std::thread::sleep(Duration::from_millis(20));
            assert_eq!(observed_mem.read_obj_from_addr::<Le16>(GuestAddress(0x1302)).unwrap().to_native(), 0);
            assert!(!observed_drop.load(Ordering::SeqCst));
            second.signal().unwrap();
        });
        worker.on_reset_with_drain_timeout(Duration::from_secs(1)).unwrap();
        release.join().unwrap();
        assert_eq!(mem.read_obj_from_addr::<Le16>(GuestAddress(0x1302)).unwrap().to_native(), 1);
        let response: virtio_gpu_native_ahb_operation = mem.read_obj_from_addr(GuestAddress(0x6000)).unwrap();
        assert_eq!(response.hdr.type_.to_native(), VIRTIO_GPU_RESP_OK_NATIVE_AHB_OPERATION);
        assert_eq!(response.sequence.to_native(), 17);
        let untouched: virtio_gpu_ctrl_hdr = mem.read_obj_from_addr(GuestAddress(0x6818)).unwrap();
        assert_eq!(untouched.type_.to_native(), 0);
        assert!(dropped.load(Ordering::SeqCst));
        assert!(worker.activation_resources.is_none());
    }

    #[test]
    fn reset_deadline_retains_last_reader_wait_allocation_and_activation() {
        let (mut worker, exit, next) = fixture();
        worker.state.native_geometry_negotiated = true;
        worker.last_native_profile = Some(native_geometry::ProfileObservation::Unavailable);
        worker.native_profile_event.store(true, Ordering::Release);
        let first = park_native_reader(&mut worker);
        let second = park_native_reader(&mut worker);
        let (mem, dropped) = park_wait_and_allocation(&mut worker);
        worker.state.virtio_gpu.inject_deferred_native_ahb_id_for_test(73);
        first.signal().unwrap();
        let started = std::time::Instant::now();
        assert!(worker.on_reset_with_drain_timeout(Duration::from_millis(25)).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(worker.state.pending_flip_fences.len(), 1);
        assert_eq!(worker.state.native_ahb_release.waiting_count(), 1);
        assert!(worker.state.virtio_gpu.is_native_ahb_resource(73));
        assert!(worker.state.virtio_gpu.has_deferred_native_ahb());
        assert!(!dropped.load(Ordering::SeqCst));
        assert!(worker.activation_resources.is_some());
        assert!(worker.fence_handler_resources.lock().is_some());
        assert!(worker.state.native_geometry_negotiated);
        assert_eq!(worker.last_native_profile, Some(native_geometry::ProfileObservation::Unavailable));
        assert!(worker.native_profile_event.load(Ordering::Acquire));
        assert!(matches!(exit.recv::<VmEventType>().unwrap(), VmEventType::Exit));
        // A late callback cannot reopen a failed device or route old waits to
        // a new activation. Only final VM teardown can release its owners.
        second.signal().unwrap();
        assert!(worker.on_reset_with_drain_timeout(Duration::ZERO).is_err());
        assert!(worker.on_activate(next).is_err());
        assert_eq!(worker.state.pending_flip_fences.len(), 1);
        assert_eq!(mem.read_obj_from_addr::<Le16>(GuestAddress(0x1302)).unwrap().to_native(), 0);
        assert!(!dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn reset_drain_wakes_on_renderer_error_or_vm_teardown() {
        for (renderer_failure, busy_detach) in [(false, false), (true, false), (false, true), (true, true)] {
            let (mut worker, _exit, _next) = fixture();
            let _reader = park_native_reader(&mut worker);
            let (_mem, dropped) = park_wait_and_allocation(&mut worker);
            let token = worker.fence_state.lock().context_tokens.reserve(9, 0).unwrap();
            let error_callback = create_fence_error_handler(worker.fence_state.clone());
            let kill = worker.kill_evt.try_clone().unwrap();
            let interrupt = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(20));
                if renderer_failure {
                    error_callback.call(RutabagaFenceError {
                        ctx_id: 9, ring_idx: 0, fence_id: token, error: -libc::EIO,
                    });
                } else {
                    kill.signal().unwrap();
                }
            });
            let started = std::time::Instant::now();
            if busy_detach {
                let mut attempts = 0;
                assert!(worker.drain_display_for_reset(Duration::from_secs(2), |_| {
                    attempts += 1;
                    false // The native Surface/presenter lock remains busy.
                }).is_err());
                assert!(attempts > 0);
            } else {
                assert!(worker.on_reset_with_drain_timeout(Duration::from_secs(2)).is_err());
            }
            interrupt.join().unwrap();
            assert!(started.elapsed() < Duration::from_secs(1));
            assert_eq!(worker.state.pending_flip_fences.len(), 1);
            assert_eq!(worker.state.native_ahb_release.waiting_count(), 1);
            assert!(!dropped.load(Ordering::SeqCst));
            assert!(worker.activation_resources.is_some());
        }
    }

    #[test]
    fn reset_keeps_deferred_context_after_last_display_reader_releases() {
        let (mut worker, exit, next) = fixture();
        let release = park_native_reader(&mut worker);
        let (mem, dropped) = park_wait_and_allocation(&mut worker);
        worker.state.virtio_gpu.inject_deferred_native_ahb_id_for_test(73);
        worker.state.deferred_context_destroy = Some(DeferredContextDestroy {
            hdr: virtio_gpu_ctrl_hdr { ctx_id: 9.into(), ..Default::default() },
            descriptor: ReturnDescriptor {
                desc_chain: create_descriptor_chain(&mem, GuestAddress(0x5000),
                    GuestAddress(0x5800), vec![(DescriptorType::Writable, 24)], 0).unwrap(),
                len: 24, queue: GpuQueue::Control, ctx_id: 9,
            },
            deadline: std::time::Instant::now() + FLIP_FENCE_TIMEOUT,
        });
        worker.state.context_retirement_event.signal().unwrap();
        release.signal().unwrap();
        // Drain may retire the real reader and old WAIT. It must not treat a
        // retirement wakeup as permission to forget the parked context command.
        assert!(worker.on_reset_with_drain_timeout(Duration::from_millis(25)).is_err());
        assert!(worker.state.pending_flip_fences.is_empty());
        assert!(!worker.state.virtio_gpu.has_deferred_native_ahb());
        assert_eq!(worker.state.native_ahb_release.waiting_count(), 0);
        assert_eq!(mem.read_obj_from_addr::<Le16>(GuestAddress(0x1302)).unwrap().to_native(), 1);
        assert!(worker.state.deferred_context_destroy.is_some());
        assert!(worker.activation_resources.is_some());
        assert!(worker.fence_handler_resources.lock().is_some());
        assert!(!dropped.load(Ordering::SeqCst));
        assert!(matches!(exit.recv::<VmEventType>().unwrap(), VmEventType::Exit));
        assert!(worker.on_activate(next).is_err());
    }

    #[test]
    fn reset_detach_admission_retries_are_bounded_even_without_readers() {
        let (mut worker, _exit, _next) = fixture();
        let mut attempts = 0;
        let started = std::time::Instant::now();
        assert!(worker.drain_display_for_reset(Duration::from_millis(25), |_| {
            attempts += 1;
            false // Models a busy native Surface lock, not a release fence.
        }).is_err());
        assert!(attempts >= 2);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(worker.activation_resources.is_some());
        assert!(worker.fence_handler_resources.lock().is_some());
        // Admission can recover on a later retry, but never bypass a reader.
        let release = park_native_reader(&mut worker);
        let signal = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            release.signal().unwrap();
        });
        attempts = 0;
        worker.drain_display_for_reset(Duration::from_secs(1), |_| {
            attempts += 1;
            attempts >= 2
        }).unwrap();
        signal.join().unwrap();
        assert!(worker.state.pending_flip_fences.is_empty());
        assert_eq!(worker.state.native_ahb_release.released, 1);
        // Drain itself must not dispose the old queues or renderer context.
        assert!(worker.activation_resources.is_some());
    }

    #[test]
    fn reset_sweep_keeps_holder_until_last_real_release() {
        let (mut worker, _exit, _next) = fixture();
        let first = park_native_reader(&mut worker);
        let _second = park_native_reader(&mut worker);
        worker.state.virtio_gpu.inject_deferred_native_ahb_id_for_test(73);
        first.signal().unwrap();
        assert!(worker.on_reset().is_err());
        assert_eq!(worker.state.pending_flip_fences.len(), 1);
        assert!(worker.state.virtio_gpu.has_deferred_native_ahb());
        assert_eq!(worker.state.native_ahb_release.released, 0);
        assert!(worker.activation_resources.is_some());
    }

    #[test]
    fn reset_sweep_rejects_readable_non_sync_file() {
        let (mut worker, _exit, _next) = fixture();
        let event = park_native_reader(&mut worker);
        // Readability is insufficient: this descriptor is not a real sync_file.
        worker.state.pending_flip_fences[0].fence = FlipCompletionFence::SyncFile(
            SafeDescriptor::try_from(&event as &dyn AsRawDescriptor).unwrap(),
        );
        event.signal().unwrap();
        assert!(worker.on_reset().is_err());
        assert_eq!(worker.state.pending_flip_fences.len(), 1);
        assert_eq!(worker.state.native_ahb_release.released, 0);
        assert!(worker.state.display_failed);
        assert!(worker.activation_resources.is_some());
    }

    #[test]
    fn reset_sweep_cannot_recover_previously_failed_display() {
        let (mut worker, _exit, _next) = fixture();
        let event = park_native_reader(&mut worker);
        event.signal().unwrap();
        worker.state.display_failed = true;
        assert!(worker.on_reset().is_err());
        assert_eq!(worker.state.pending_flip_fences.len(), 1);
        assert_eq!(worker.state.native_ahb_release.released, 0);
        assert!(worker.activation_resources.is_some());
    }
}

#[cfg(any(target_os = "android", target_os = "linux"))]
mod native_ahb_paging {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Default)]
    struct Storage {
        bytes: Vec<u8>,
        operations: usize,
        fail_after_write: bool,
    }
    struct Allocation(Rc<RefCell<Storage>>);
    impl HostSharedAllocation for Allocation {
        fn description(&self) -> SharedAllocationDescription {
            SharedAllocationDescription { allocation_size: self.0.borrow().bytes.len() as u64,
                buffer_id: 0x1234, ..Default::default() }
        }
        fn duplicate_descriptor(&self) -> anyhow::Result<SafeDescriptor> {
            Err(anyhow!("No DMA import in frontend fixture"))
        }
        fn read_bytes(&self, offset: u64, output: &mut [u8]) -> anyhow::Result<()> {
            let mut storage = self.0.borrow_mut();
            storage.operations += 1;
            output.copy_from_slice(&storage.bytes[offset as usize..offset as usize + output.len()]);
            Ok(())
        }
        fn write_bytes(&self, offset: u64, data: &[u8]) -> anyhow::Result<()> {
            let mut storage = self.0.borrow_mut();
            storage.operations += 1;
            storage.bytes[offset as usize..offset as usize + data.len()].copy_from_slice(data);
            if storage.fail_after_write { anyhow::bail!("sync END failure after mutation"); }
            Ok(())
        }
        fn fill_bytes(&self, offset: u64, length: u64, pattern: u32) -> anyhow::Result<()> {
            let mut storage = self.0.borrow_mut();
            storage.operations += 1;
            for word in storage.bytes[offset as usize..(offset + length) as usize].chunks_exact_mut(4) {
                word.copy_from_slice(&pattern.to_le_bytes());
            }
            Ok(())
        }
    }
    fn fixture() -> (Harness, Rc<RefCell<Storage>>) {
        let mut test = Harness::new(true);
        let storage = Rc::new(RefCell::new(Storage { bytes: vec![0xa5; 256], ..Default::default() }));
        test.frontend.virtio_gpu.inject_native_ahb_for_test(11, Box::new(Allocation(storage.clone())));
        (test, storage)
    }
    fn command(op: u32, offset: u64, length: u32) -> virtio_gpu_native_ahb_paging {
        virtio_gpu_native_ahb_paging {
            hdr: virtio_gpu_ctrl_hdr { type_: VIRTIO_GPU_CMD_PAGE_NATIVE_AHB.into(), ..Default::default() },
            resource_id: 11.into(), operation: op.into(), offset: offset.into(),
            length: length.into(), pattern: 0.into(),
        }
    }
    fn send(test: &mut Harness, cmd: virtio_gpu_native_ahb_paging, bytes: &[u8], queue: usize) -> (u32, GuestAddress) {
        let index = test.submitted[queue];
        let mut request = cmd.as_bytes().to_vec();
        request.extend_from_slice(bytes);
        let base = if queue == 0 { 0x1800 } else { 0x3800 };
        let response = GuestAddress(base + u64::from(index % 4) * 0x400 + request.len() as u64);
        test.submit(&request, queue, true);
        let header: virtio_gpu_ctrl_hdr = test.mem.read_obj_from_addr(response).unwrap();
        (header.type_.to_native(), response)
    }
    #[test]
    fn preservation_round_trip_keeps_padding_and_reports_exact_completed_bytes() {
        let (mut test, storage) = fixture();
        let pattern: Vec<u8> = (0..17).map(|i| i * 7).collect();
        let write = command(VIRTIO_GPU_NATIVE_AHB_PAGE_WRITE, 239, 17);
        let (status, address) = send(&mut test, write, &pattern, 0);
        assert_eq!(status, VIRTIO_GPU_RESP_OK_NATIVE_AHB_PAGING);
        let reply: virtio_gpu_native_ahb_paging = test.mem.read_obj_from_addr(address).unwrap();
        assert_eq!(&reply.as_bytes()[24..], &write.as_bytes()[24..]);
        assert!(storage.borrow().bytes[..239].iter().all(|byte| *byte == 0xa5));
        assert_eq!(storage.borrow().bytes[239..], pattern);
        let (status, address) = send(&mut test, command(VIRTIO_GPU_NATIVE_AHB_PAGE_READ, 239, 17), &[], 0);
        assert_eq!(status, VIRTIO_GPU_RESP_OK_NATIVE_AHB_PAGING);
        let mut actual = [0; 17];
        test.mem.read_exact_at_addr(&mut actual, GuestAddress(address.0 + 48)).unwrap();
        assert_eq!(actual.as_slice(), &pattern);
        let mut fill = command(VIRTIO_GPU_NATIVE_AHB_PAGE_FILL, 8, 12);
        fill.pattern = 0x78563412.into();
        assert_eq!(send(&mut test, fill, &[], 0).0, VIRTIO_GPU_RESP_OK_NATIVE_AHB_PAGING);
        assert_eq!(&storage.borrow().bytes[8..20], &[0x12,0x34,0x56,0x78].repeat(3));
        let stats = &test.frontend.native_ahb_paging;
        assert_eq!((stats.completed_read_bytes, stats.completed_write_bytes, stats.completed_fill_bytes), (17,17,12));
        assert_eq!(storage.borrow().operations, 3);
        assert_eq!(test.frontend.native_ahb_release.accepted, 0);
    }

    #[test]
    fn paging_completion_interrupt_precedes_the_next_descriptor() {
        struct ObservedQueue {
            inner: SharedQueueReader,
            events: RefCell<Vec<&'static str>>,
        }
        impl QueueReader for ObservedQueue {
            fn pop(&self) -> Option<DescriptorChain> {
                let events = self.events.borrow();
                if !events.is_empty() && !events.contains(&"signal") {
                    panic!("paging response must signal before the next command can block");
                }
                drop(events);
                self.events.borrow_mut().push("pop");
                self.inner.pop()
            }
            fn add_used(&self, chain: DescriptorChain, len: u32) {
                self.inner.add_used(chain, len);
                self.events.borrow_mut().push("used");
            }
            fn signal_used(&self) {
                assert_eq!(self.events.borrow().last(), Some(&"used"));
                self.inner.signal_used();
                self.events.borrow_mut().push("signal");
            }
        }
        for op in [VIRTIO_GPU_NATIVE_AHB_PAGE_READ, VIRTIO_GPU_NATIVE_AHB_PAGE_WRITE,
                   VIRTIO_GPU_NATIVE_AHB_PAGE_FILL] {
            for rejected in [false, true] {
                let (mut test, storage) = fixture();
                let offset = if rejected { 256 } else { 0 };
                let cmd = command(op, offset, 4);
                let mut bytes = cmd.as_bytes().to_vec();
                if op == VIRTIO_GPU_NATIVE_AHB_PAGE_WRITE {
                    bytes.extend_from_slice(&[1, 2, 3, 4]);
                }
                test.enqueue_capacity(&bytes, 0, 512);
                let later = virtio_gpu_ctrl_hdr {
                    type_: VIRTIO_GPU_CMD_GET_DISPLAY_INFO.into(), ..Default::default()
                };
                test.enqueue_capacity(later.as_bytes(), 0, 512);
                let queue = ObservedQueue {
                    inner: test.queues[0].clone(), events: RefCell::new(Vec::new()),
                };
                // This processes the real decoder, paging handler, response
                // encoder and used ring. The observer checks at the next pop,
                // before a later command can enter rendering or teardown.
                assert!(test.frontend.process_queue(&test.mem, &queue));
                assert_eq!(*queue.events.borrow(), ["pop", "used", "signal", "pop", "used", "pop"]);
                assert_eq!(test.used(0), 2);
                let reply: virtio_gpu_ctrl_hdr = test.mem
                    .read_obj_from_addr(GuestAddress(0x1800 + bytes.len() as u64)).unwrap();
                assert_eq!(reply.type_.to_native(), if rejected {
                    VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER
                } else { VIRTIO_GPU_RESP_OK_NATIVE_AHB_PAGING });
                assert_eq!(storage.borrow().operations, usize::from(!rejected));
            }
        }
    }
    #[test]
    fn bad_ranges_payloads_headers_and_short_responses_never_touch_storage() {
        let (mut test, storage) = fixture();
        let read = VIRTIO_GPU_NATIVE_AHB_PAGE_READ;
        for cmd in [command(read, 0, 0), command(read, 0, 65537), command(read, u64::MAX, 4),
            command(read, 250, 7), command(99, 0, 4), command(read, 0, 500),
            command(VIRTIO_GPU_NATIVE_AHB_PAGE_FILL, 1, 4), command(VIRTIO_GPU_NATIVE_AHB_PAGE_FILL, 0, 3)] {
            assert_eq!(send(&mut test, cmd, &[], 0).0, VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
        }
        let mut cmd = command(read, 0, 4);
        cmd.hdr.ctx_id = 1.into();
        assert_eq!(send(&mut test, cmd, &[], 0).0, VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
        cmd.hdr.ctx_id = 0.into();
        assert_eq!(send(&mut test, cmd, &[], 1).0, VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
        assert_eq!(send(&mut test, cmd, &[0], 0).0, VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
        cmd.operation = VIRTIO_GPU_NATIVE_AHB_PAGE_WRITE.into();
        assert_eq!(send(&mut test, cmd, &[1,2,3], 0).0, VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
        cmd.pattern = 1.into();
        assert_eq!(send(&mut test, cmd, &[1,2,3,4], 0).0, VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
        assert_eq!(storage.borrow().operations, 0);
    }
    #[test]
    fn pending_android_reader_denies_paging_without_waiting_or_touching_pixels() {
        let (mut test, storage) = fixture();
        let event = Event::new().unwrap();
        test.frontend.pending_flip_fences.push(PendingFlipFence {
            resource_id: 11, fence_id: 1, ticket: None, registered: false, deadline: None,
            fence: FlipCompletionFence::AndroidReleaseEvent(SafeDescriptor::try_from(&event as &dyn AsRawDescriptor).unwrap()),
        });
        for op in [VIRTIO_GPU_NATIVE_AHB_PAGE_READ, VIRTIO_GPU_NATIVE_AHB_PAGE_WRITE, VIRTIO_GPU_NATIVE_AHB_PAGE_FILL] {
            let bytes = if op == VIRTIO_GPU_NATIVE_AHB_PAGE_WRITE { &[0;4][..] } else { &[][..] };
            assert_eq!(send(&mut test, command(op, 0, 4), bytes, 0).0, VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
        }
        assert_eq!(storage.borrow().operations, 0);
        assert_eq!(test.used(0), 3);
    }
    #[test]
    fn partial_failure_poison_denies_later_paging_and_present() {
        let (mut test, storage) = fixture();
        storage.borrow_mut().fail_after_write = true;
        assert_eq!(send(&mut test, command(VIRTIO_GPU_NATIVE_AHB_PAGE_WRITE, 0, 4), &[1,2,3,4], 0).0,
            VIRTIO_GPU_RESP_ERR_UNSPEC);
        assert_eq!(&storage.borrow().bytes[..4], &[1,2,3,4]);
        assert_eq!(test.frontend.native_ahb_paging.failures, 1);
        assert_eq!(send(&mut test, command(VIRTIO_GPU_NATIVE_AHB_PAGE_READ, 0, 4), &[], 0).0,
            VIRTIO_GPU_RESP_ERR_UNSPEC);
        let present = virtio_gpu_native_ahb_operation {
            hdr: virtio_gpu_ctrl_hdr { type_: VIRTIO_GPU_CMD_PRESENT_NATIVE_AHB.into(), ..Default::default() },
            resource_id: 11.into(), ..Default::default()
        };
        test.submit(present.as_bytes(), 0, true);
        assert_eq!(test.response(0, 40).type_.to_native(), VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID);
        let flush = virtio_gpu_resource_flush {
            hdr: virtio_gpu_ctrl_hdr { type_: VIRTIO_GPU_CMD_RESOURCE_FLUSH.into(), ..Default::default() },
            resource_id: 11.into(), ..Default::default()
        };
        test.submit(flush.as_bytes(), 0, true);
        assert_eq!(test.response(0, size_of_val(&flush)).type_.to_native(), VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
        assert_eq!(storage.borrow().operations, 1);
    }
}

#[cfg(any(target_os = "android", target_os = "linux"))]
mod native_ahb_release {
    use super::*;

    #[cfg(feature = "android_display_stub")]
    pub(super) fn refresh_fixture() -> (Harness, Rc<gpu_display::native_refresh_test::Consumer>, u64) {
        let mut test = Harness::new(true);
        resource(&mut test, 11);
        let consumer = Rc::new(gpu_display::native_refresh_test::Consumer::default());
        consumer.generation.set(1);
        let display = gpu_display::native_refresh_test::open(consumer.clone()).unwrap();
        test.frontend.virtio_gpu.set_native_refresh_display_for_test(display, 11);
        let (sequence, _) = present(&mut test, 11, 0);
        assert_eq!(consumer.submissions.borrow().len(), 1);
        assert_eq!(test.frontend.pending_flip_fences.len(), 1);
        (test, consumer, sequence)
    }

    #[cfg(feature = "android_display_stub")]
    pub(super) fn retry_fixture() -> (Harness, Rc<gpu_display::native_refresh_test::Consumer>, u64) {
        let (mut test, consumer, sequence) = refresh_fixture();
        resource(&mut test, 12);
        test.frontend.virtio_gpu.bind_native_refresh_resource_for_test(12);
        consumer.fail_next_flip.set(true);
        test.submit(command(VIRTIO_GPU_CMD_PRESENT_NATIVE_AHB, 12, 0).as_bytes(), 0, true);
        assert_eq!(test.used(0), 1); // PRESENT has not been acknowledged.
        assert!(test.frontend.native_ahb_present_pending(12));
        (test, consumer, sequence)
    }

    #[cfg(feature = "android_display_stub")]
    fn retry_due(test: &mut Harness) {
        let deadline = test.frontend.native_ahb_present_retry_deadline().unwrap();
        assert_eq!(test.frontend.next_flip_fence_deadline(), Some(deadline));
        let completed = test.frontend.retry_native_ahb_presents(deadline);
        return_fenced_descriptors(completed, &test.queues[0], Some(&test.queues[1]));
    }

    #[cfg(feature = "android_display_stub")]
    #[test]
    fn transient_replacement_renderer_error_before_retry_retains_reservation() {
        let (mut test, consumer, _) = retry_fixture();
        let token = test.frontend.fence_state.lock().context_tokens.reserve(9, 0).unwrap();
        let error = RutabagaFenceError { ctx_id: 9, ring_idx: 0, fence_id: token, error: -libc::EIO };
        create_fence_error_handler(test.frontend.fence_state.clone()).call(error);
        let deadline = test.frontend.native_ahb_present_retry_deadline().unwrap();
        assert!(test.frontend.process_native_ahb_present_retries(deadline, &test.queues[0], Some(&test.queues[1])).is_err());
        assert_eq!(consumer.attempts.get(), 2); // No extra backend admission.
        assert_eq!(consumer.submissions.borrow().len(), 1);
        assert_eq!(test.used(0), 1);
        assert!(test.frontend.native_ahb_present_pending(12));
        assert!(test.frontend.native_ahb_has_reader(11));
        assert!(!test.frontend.native_ahb_has_reader(12));
        assert_eq!(test.frontend.native_ahb_release.accepted, 1);
        assert_eq!(test.frontend.native_ahb_release.released, 0);
        assert_eq!(test.frontend.fence_state.lock().renderer_error, Some(error));
        assert!(test.frontend.check_quiescent().is_err());
    }

    #[cfg(feature = "android_display_stub")]
    #[test]
    fn transient_replacement_renderer_error_during_submit_retains_reader_and_completion() {
        let (mut test, consumer, _) = retry_fixture();
        let token = test.frontend.fence_state.lock().context_tokens.reserve(9, 0).unwrap();
        let error = RutabagaFenceError { ctx_id: 9, ring_idx: 0, fence_id: token, error: -libc::EIO };
        let handler = create_fence_error_handler(test.frontend.fence_state.clone());
        *consumer.on_submit.borrow_mut() = Some(Box::new(move || handler.call(error)));
        let deadline = test.frontend.native_ahb_present_retry_deadline().unwrap();
        assert!(test.frontend.process_native_ahb_present_retries(deadline, &test.queues[0], Some(&test.queues[1])).is_err());
        assert_eq!(consumer.attempts.get(), 3);
        assert_eq!(consumer.submissions.borrow().len(), 2); // Genuine admission raced failure.
        assert_eq!(test.used(0), 1); // No success or error response published after failure.
        assert!(!test.frontend.native_ahb_present_pending(12));
        assert!(test.frontend.native_ahb_has_reader(11));
        assert!(test.frontend.native_ahb_has_reader(12));
        assert_eq!(test.frontend.native_ahb_release.released, 0);
        let mut state = test.frontend.fence_state.lock();
        assert_eq!(state.renderer_error, Some(error));
        assert!(!state.queue.is_empty()); // Completion is retained by the stopped queue.
        assert!(state.queue.drain_ready().is_empty());
        drop(state);
        // A later successful renderer callback cannot publish that descriptor.
        test.callback.call(RutabagaFence { flags: RUTABAGA_FLAG_INFO_RING_IDX,
            ctx_id: 9, ring_idx: 0, fence_id: token });
        assert_eq!(test.used(0), 1);
        assert!(test.frontend.check_quiescent().is_err());
    }

    #[cfg(feature = "android_display_stub")]
    #[test]
    fn transient_replacement_retry_keeps_reservation_until_real_admission() {
        let (mut test, consumer, sequence) = retry_fixture();
        assert_eq!(consumer.generation.get(), 1);
        assert_eq!(consumer.submissions.borrow().len(), 1);
        assert!(test.frontend.native_ahb_has_reader(11));
        assert!(!test.frontend.native_ahb_has_reader(12));
        assert!(!test.frontend.virtio_gpu.is_scanout_resource(11));
        assert!(!test.frontend.native_ahb_release.sequences.contains_key(&12));
        assert_eq!(test.frontend.native_ahb_release.accepted, 1);
        assert!(test.frontend.check_quiescent().is_err());
        let completed = test.used(0);
        test.submit(command(VIRTIO_GPU_CMD_WAIT_NATIVE_AHB_RELEASE, 11, sequence).as_bytes(), 0, true);
        complete(&mut test).unwrap();
        assert_eq!(test.used(0), completed);
        assert_eq!(test.frontend.native_ahb_release.waiting_count(), 1);
        retry_due(&mut test); // No new guest paint or descriptor is needed.
        assert_eq!(consumer.attempts.get(), 3);
        assert_eq!(consumer.submissions.borrow().len(), 2);
        assert_eq!(test.used(0), completed + 1);
        assert_eq!(response(&test, 1).hdr.type_.to_native(), VIRTIO_GPU_RESP_OK_NATIVE_AHB_OPERATION);
        assert!(response(&test, 1).sequence.to_native() > sequence);
        assert_eq!(test.frontend.native_ahb_release.accepted, 2);
        assert!(test.frontend.native_ahb_has_reader(11));
        assert!(test.frontend.native_ahb_has_reader(12));
        assert_eq!(test.frontend.native_ahb_release.released, 0);
        assert_eq!(test.frontend.native_ahb_release.waiting_count(), 1);
        consumer.submissions.borrow()[0].1.signal().unwrap();
        complete(&mut test).unwrap();
        assert_eq!(test.used(0), completed + 2);
        assert_eq!(test.frontend.native_ahb_release.released, 1);
        assert!(test.frontend.next_flip_fence_deadline().is_none());
        assert!(test.frontend.retry_native_ahb_presents(std::time::Instant::now() + Duration::from_secs(60)).is_empty());
        assert_eq!(consumer.submissions.borrow().len(), 2);
        assert!(test.frontend.native_ahb_has_reader(12)); // Static front stays owned indefinitely.
    }

    #[cfg(feature = "android_display_stub")]
    #[test]
    fn transient_replacement_retry_is_bounded_and_never_fabricates_release() {
        for overdue in [false, true] {
            let (mut test, consumer, _) = retry_fixture();
            if overdue {
                let completed = test.frontend.retry_native_ahb_presents(std::time::Instant::now() + Duration::from_secs(1));
                return_fenced_descriptors(completed, &test.queues[0], Some(&test.queues[1]));
                assert_eq!(consumer.attempts.get(), 2);
            } else {
                for _ in 0..7 {
                    consumer.fail_next_flip.set(true);
                    retry_due(&mut test);
                }
                assert_eq!(consumer.attempts.get(), 9); // Original front + eight replacement attempts.
            }
            assert_eq!(test.used(0), 2);
            assert_eq!(response(&test, 1).hdr.type_.to_native(), VIRTIO_GPU_RESP_ERR_UNSPEC);
            assert!(!test.frontend.native_ahb_present_pending(12));
            assert!(test.frontend.native_ahb_present_retry_deadline().is_none());
            assert!(test.frontend.native_ahb_has_reader(11));
            assert!(!test.frontend.native_ahb_has_reader(12));
            assert_eq!(test.frontend.native_ahb_release.accepted, 1);
            assert_eq!(test.frontend.native_ahb_release.released, 0);
            assert_eq!(consumer.submissions.borrow().len(), 1);
        }
    }

    #[cfg(feature = "android_display_stub")]
    #[test]
    fn transient_replacement_retry_cancels_closed_changed_or_superseded_owner() {
        for change in 0..4 {
            let (mut test, consumer, _) = retry_fixture();
            match change {
                0 => { consumer.detached.set(true); consumer.generation.set(2); }
                1 => consumer.generation.set(3),
                2 => { resource(&mut test, 13); test.frontend.virtio_gpu.bind_native_refresh_resource_for_test(13); }
                _ => consumer.race_on_submit.set(true),
            }
            retry_due(&mut test);
            assert_eq!(test.used(0), 2);
            assert_eq!(response(&test, 1).hdr.type_.to_native(), VIRTIO_GPU_RESP_OK_NATIVE_AHB_OPERATION);
            assert_eq!(consumer.submissions.borrow().len(), 1);
            assert_eq!(consumer.attempts.get(), if change == 3 { 3 } else { 2 });
            assert!(!test.frontend.native_ahb_present_pending(12));
            assert!(!test.frontend.native_ahb_has_reader(12));
            assert!(test.frontend.native_ahb_has_reader(11));
            assert_eq!(test.frontend.native_ahb_release.released, 0);
        }
    }

    #[cfg(feature = "android_display_stub")]
    #[test]
    fn transient_replacement_headless_present_still_completes_without_retry() {
        let (mut test, consumer, _) = refresh_fixture();
        resource(&mut test, 12);
        test.frontend.virtio_gpu.bind_native_refresh_resource_for_test(12);
        consumer.detached.set(true);
        consumer.generation.set(2);
        let (sequence, _) = present(&mut test, 12, 0);
        assert_eq!(test.used(0), 2);
        assert!(!test.frontend.native_ahb_present_pending(12));
        assert!(test.frontend.native_ahb_present_retry_deadline().is_none());
        test.submit(command(VIRTIO_GPU_CMD_WAIT_NATIVE_AHB_RELEASE, 12, sequence).as_bytes(), 0, true);
        assert_eq!(test.used(0), 3);
        assert!(test.frontend.native_ahb_has_reader(11));
        assert!(!test.frontend.native_ahb_has_reader(12));
        assert_eq!(consumer.submissions.borrow().len(), 1);
        assert_eq!(test.frontend.native_ahb_release.released, 0);
    }

    #[cfg(feature = "android_display_stub")]
    #[test]
    fn transient_replacement_pending_rejects_reuse_unref_paging_and_unsequenced_flush() {
        for operation in 0..5 {
            let (mut test, consumer, _) = retry_fixture();
            match operation {
                0 => test.submit(command(VIRTIO_GPU_CMD_PRESENT_NATIVE_AHB, 12, 0).as_bytes(), 0, true),
                1 => test.submit(command(VIRTIO_GPU_CMD_WAIT_NATIVE_AHB_RELEASE, 12, 0).as_bytes(), 0, true),
                2 => test.submit(virtio_gpu_resource_unref {
                    hdr: virtio_gpu_ctrl_hdr { type_: VIRTIO_GPU_CMD_RESOURCE_UNREF.into(), ..Default::default() },
                    resource_id: 12.into(), padding: 0.into(),
                }.as_bytes(), 0, true),
                3 => test.submit(virtio_gpu_native_ahb_paging {
                    hdr: virtio_gpu_ctrl_hdr { type_: VIRTIO_GPU_CMD_PAGE_NATIVE_AHB.into(), ..Default::default() },
                    resource_id: 12.into(), operation: VIRTIO_GPU_NATIVE_AHB_PAGE_FILL.into(),
                    offset: 0.into(), length: 4.into(), pattern: 0.into(),
                }.as_bytes(), 0, true),
                _ => test.submit(virtio_gpu_resource_flush {
                    hdr: virtio_gpu_ctrl_hdr { type_: VIRTIO_GPU_CMD_RESOURCE_FLUSH.into(), ..Default::default() },
                    resource_id: 12.into(), ..Default::default()
                }.as_bytes(), 0, true),
            }
            let size = match operation { 2 => 32, 3 => 48, 4 => 48, _ => 40 };
            assert_eq!(test.response(0, size).type_.to_native(), VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
            assert_eq!(test.used(0), 2); // Old front and rejection only.
            assert!(test.frontend.native_ahb_present_pending(12));
            assert!(test.frontend.virtio_gpu.is_native_ahb_resource(12));
            assert_eq!(consumer.submissions.borrow().len(), 1);
            retry_due(&mut test);
            assert_eq!(consumer.submissions.borrow().len(), 2);
        }
    }

    #[cfg(feature = "android_display_stub")]
    #[test]
    fn refresh_first_attach_replacement_progresses_after_generation_change() {
        for refresh_first in [false, true] {
            let mut test = Harness::new(true);
            resource(&mut test, 11);
            resource(&mut test, 12);
            let consumer = Rc::new(gpu_display::native_refresh_test::Consumer::default());
            let display = gpu_display::native_refresh_test::open(consumer.clone()).unwrap();
            test.frontend.virtio_gpu.set_native_refresh_display_for_test(display, 11);
            // A closed Activity creates no reader and leaves the normal
            // one-frame retry delay on each resource that attempted a flip.
            let (old_sequence, _) = present(&mut test, 11, 0);
            test.frontend.virtio_gpu.bind_native_refresh_resource_for_test(12);
            present(&mut test, 12, 0);
            assert!(test.frontend.pending_flip_fences.is_empty());
            assert!(consumer.submissions.borrow().is_empty());

            // A refresh or ordinary PRESENT can win first attach. Neither
            // may leave stale backoff on the other replacement resource.
            test.frontend.virtio_gpu.bind_native_refresh_resource_for_test(11);
            consumer.generation.set(1);
            let refresh_index = test.submitted[0];
            if refresh_first {
                test.submit(command(VIRTIO_GPU_CMD_REFRESH_NATIVE_AHB, 11, old_sequence).as_bytes(), 0, true);
            } else {
                present(&mut test, 11, 0);
            }
            let refresh_sequence = response(&test, refresh_index).sequence.to_native();
            assert!(refresh_sequence > old_sequence);
            assert_eq!(consumer.submissions.borrow().len(), 1);
            assert_eq!(test.frontend.pending_flip_fences.len(), 1);

            // The replacement must perform a real transaction, otherwise the
            // old physical front remains read-owned after logical rebind.
            test.frontend.virtio_gpu.bind_native_refresh_resource_for_test(12);
            present(&mut test, 12, 0);
            assert!(!test.frontend.virtio_gpu.is_scanout_resource(11));
            assert!(test.frontend.virtio_gpu.is_scanout_resource(12));
            assert_eq!(consumer.submissions.borrow().len(), 2);
            assert_eq!(test.frontend.pending_flip_fences.len(), 2);

            let completed_before_wait = test.used(0);
            test.submit(command(VIRTIO_GPU_CMD_WAIT_NATIVE_AHB_RELEASE, 11, refresh_sequence).as_bytes(), 0, true);
            complete(&mut test).unwrap();
            assert_eq!(test.used(0), completed_before_wait);
            assert_eq!(test.frontend.native_ahb_release.waiting_count(), 1);
            // Only a genuine consumer release retires the pending WAIT.
            consumer.submissions.borrow()[0].1.signal().unwrap();
            complete(&mut test).unwrap();
            assert_eq!(test.used(0), completed_before_wait + 1);
            assert_eq!(test.frontend.native_ahb_release.waiting_count(), 0);
            assert!(!test.frontend.native_ahb_has_reader(11));
            assert!(test.frontend.native_ahb_has_reader(12));

            // A resource created after attach starts without stale retry
            // state and must replace the current front on its first present.
            resource(&mut test, 13);
            test.frontend.virtio_gpu.bind_native_refresh_resource_for_test(13);
            present(&mut test, 13, 0);
            assert_eq!(consumer.submissions.borrow().len(), 3);
            assert!(test.frontend.native_ahb_has_reader(12));
            assert!(test.frontend.native_ahb_has_reader(13));
        }
    }

    #[cfg(feature = "android_display_stub")]
    #[test]
    fn refresh_retries_after_first_ordinary_present_fails_in_new_generation() {
        let mut test = Harness::new(true);
        resource(&mut test, 11);
        let consumer = Rc::new(gpu_display::native_refresh_test::Consumer::default());
        let display = gpu_display::native_refresh_test::open(consumer.clone()).unwrap();
        test.frontend.virtio_gpu.set_native_refresh_display_for_test(display, 11);
        present(&mut test, 11, 0);
        consumer.generation.set(1);
        consumer.fail_next_flip.set(true);
        // Ordinary PRESENT observes the generation first, but the direct
        // flip fails and recreates backoff before the sole refresh arrives.
        let (sequence, _) = present(&mut test, 11, 0);
        assert_eq!(consumer.attempts.get(), 2);
        assert!(consumer.submissions.borrow().is_empty());
        assert!(!test.frontend.native_ahb_has_reader(11));
        let index = test.submitted[0];
        test.submit(command(VIRTIO_GPU_CMD_REFRESH_NATIVE_AHB, 11, sequence).as_bytes(), 0, true);
        assert_eq!(consumer.attempts.get(), 3);
        assert_eq!(response(&test, index).hdr.type_.to_native(), VIRTIO_GPU_RESP_OK_NATIVE_AHB_OPERATION);
        assert!(response(&test, index).sequence.to_native() > sequence);
        assert_eq!(consumer.submissions.borrow().len(), 1);
        assert!(test.frontend.native_ahb_has_reader(11));
        assert_eq!(test.frontend.native_ahb_release.released, 0);
    }

    #[cfg(feature = "android_display_stub")]
    #[test]
    fn refresh_import_backoff_retains_same_generation_and_clears_on_reattach() {
        let (mut test, consumer, sequence) = refresh_fixture();
        // Closing increments the actual attachment generation, even though
        // native_refresh_generation() reports no attached consumer to refresh.
        consumer.detached.set(true);
        consumer.generation.set(2);
        test.submit(command(VIRTIO_GPU_CMD_WAIT_NATIVE_AHB_RELEASE, 11, sequence).as_bytes(), 0, true);
        complete(&mut test).unwrap();
        assert!(test.frontend.native_ahb_has_reader(11));
        consumer.submissions.borrow()[0].1.signal().unwrap();
        complete(&mut test).unwrap();
        assert!(!test.frontend.native_ahb_has_reader(11));

        let attempts = consumer.attempts.get();
        present(&mut test, 11, 0); // closed failure sets retry=1
        assert_eq!(consumer.attempts.get(), attempts + 1);
        present(&mut test, 11, 0); // same generation skips once
        assert_eq!(consumer.attempts.get(), attempts + 1);
        let (closed_sequence, _) = present(&mut test, 11, 0); // still closed; no fake reader
        assert_eq!(consumer.attempts.get(), attempts + 2);
        assert!(test.frontend.pending_flip_fences.is_empty());

        consumer.detached.set(false);
        consumer.generation.set(3);
        let index = test.submitted[0];
        test.submit(command(VIRTIO_GPU_CMD_REFRESH_NATIVE_AHB, 11, closed_sequence).as_bytes(), 0, true);
        assert!(response(&test, index).sequence.to_native() > closed_sequence);
        assert_eq!(consumer.attempts.get(), attempts + 3);
        assert_eq!(consumer.submissions.borrow().len(), 2);
        assert_eq!(test.frontend.pending_flip_fences.len(), 1);
        assert_eq!(test.frontend.native_ahb_release.released, 1);
    }

    #[cfg(feature = "android_display_stub")]
    #[test]
    fn refresh_waits_real_old_reader_then_presents_once_without_renderer_work() {
        let (mut test, consumer, sequence) = refresh_fixture();
        consumer.generation.set(3);
        test.submit(command(VIRTIO_GPU_CMD_REFRESH_NATIVE_AHB, 11, sequence).as_bytes(), 0, true);
        assert_eq!(test.used(0), 1);
        assert_eq!(consumer.submissions.borrow().len(), 1);
        // A different resource still progresses while the old consumer holds it.
        resource(&mut test, 12);
        present(&mut test, 12, 0);
        assert_eq!(test.used(0), 2);
        complete(&mut test).unwrap();
        assert_eq!(test.used(0), 2);
        consumer.submissions.borrow()[0].1.signal().unwrap();
        complete(&mut test).unwrap();
        assert_eq!(test.used(0), 3);
        assert!(response(&test, 1).sequence.to_native() > sequence);
        assert_eq!(consumer.submissions.borrow().len(), 2);
        assert_eq!(consumer.submissions.borrow()[1].0, 3);
        assert_eq!(test.frontend.pending_flip_fences.len(), 1);
        assert!(test.observed.lock().is_empty());
        assert_eq!(test.frontend.native_ahb_release.released, 1);
    }

    #[cfg(feature = "android_display_stub")]
    #[test]
    fn refresh_after_normal_present_is_unchanged_not_release() {
        let (mut test, consumer, sequence) = refresh_fixture();
        test.submit(command(VIRTIO_GPU_CMD_REFRESH_NATIVE_AHB, 11, sequence).as_bytes(), 0, true);
        assert_eq!(response(&test, 1).sequence.to_native(), sequence);
        assert_eq!(consumer.submissions.borrow().len(), 1);
        assert_eq!(test.frontend.pending_flip_fences.len(), 1);
        assert_eq!(test.frontend.native_ahb_release.released, 0);
        test.submit(command(VIRTIO_GPU_CMD_WAIT_NATIVE_AHB_RELEASE, 11, sequence).as_bytes(), 0, true);
        assert_eq!(test.used(0), 2); // unchanged acknowledgement did not release
    }

    #[cfg(feature = "android_display_stub")]
    #[test]
    fn refresh_attachment_changes_before_or_during_submit_do_not_replay_into_new_activity() {
        for at_submit in [false, true] {
            let (mut test, consumer, sequence) = refresh_fixture();
            consumer.generation.set(3);
            test.submit(command(VIRTIO_GPU_CMD_REFRESH_NATIVE_AHB, 11, sequence).as_bytes(), 0, true);
            if at_submit { consumer.race_on_submit.set(true); }
            else { consumer.generation.set(5); }
            consumer.submissions.borrow()[0].1.signal().unwrap();
            complete(&mut test).unwrap();
            assert_eq!(response(&test, 1).sequence.to_native(), sequence);
            assert_eq!(consumer.submissions.borrow().len(), 1);
            assert!(test.frontend.pending_flip_fences.is_empty());
            // A later genuine attach event uses the latest generation; even a
            // failed-import frame backoff must not swallow its one refresh.
            test.submit(command(VIRTIO_GPU_CMD_REFRESH_NATIVE_AHB, 11, sequence).as_bytes(), 0, true);
            assert!(response(&test, 2).sequence.to_native() > sequence);
            assert_eq!(consumer.submissions.borrow().len(), 2);
            assert_eq!(consumer.submissions.borrow()[1].0, consumer.generation.get());
        }
    }

    #[cfg(feature = "android_display_stub")]
    #[test]
    fn refresh_reset_cancellation_keeps_original_reader_and_never_submits() {
        let (mut test, consumer, sequence) = refresh_fixture();
        consumer.generation.set(3);
        test.submit(command(VIRTIO_GPU_CMD_REFRESH_NATIVE_AHB, 11, sequence).as_bytes(), 0, true);
        let cancelled = test.frontend.cancel_native_ahb_refreshes();
        assert_eq!(cancelled.len(), 1);
        return_fenced_descriptors(cancelled, &test.queues[0], Some(&test.queues[1]));
        assert_eq!(response(&test, 1).sequence.to_native(), sequence);
        assert_eq!(test.frontend.pending_flip_fences.len(), 1);
        assert_eq!(test.frontend.native_ahb_release.released, 0);
        consumer.submissions.borrow()[0].1.signal().unwrap();
        complete(&mut test).unwrap();
        assert_eq!(consumer.submissions.borrow().len(), 1);
        assert!(!test.frontend.native_ahb_release.has_waiters());
    }

    #[cfg(feature = "android_display_stub")]
    #[test]
    fn refresh_rejects_stale_sequence_and_duplicate_reservation() {
        let (mut test, consumer, sequence) = refresh_fixture();
        consumer.generation.set(3);
        test.submit(command(VIRTIO_GPU_CMD_REFRESH_NATIVE_AHB, 11, sequence + 1).as_bytes(), 0, true);
        assert_eq!(response(&test, 1).hdr.type_.to_native(), VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
        test.submit(command(VIRTIO_GPU_CMD_REFRESH_NATIVE_AHB, 11, sequence).as_bytes(), 0, true);
        test.submit(command(VIRTIO_GPU_CMD_REFRESH_NATIVE_AHB, 11, sequence).as_bytes(), 0, true);
        assert_eq!(response(&test, 3).hdr.type_.to_native(), VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
        assert_eq!(test.frontend.native_ahb_release.waiting_count(), 1);
        assert_eq!(consumer.submissions.borrow().len(), 1);
    }

    #[cfg(feature = "android_display_stub")]
    #[test]
    fn refresh_unref_waits_for_old_holder_and_rejects_deleted_identity() {
        let (mut test, consumer, sequence) = refresh_fixture();
        consumer.generation.set(3);
        test.submit(command(VIRTIO_GPU_CMD_REFRESH_NATIVE_AHB, 11, sequence).as_bytes(), 0, true);
        let unref = virtio_gpu_resource_unref {
            hdr: virtio_gpu_ctrl_hdr { type_: VIRTIO_GPU_CMD_RESOURCE_UNREF.into(), ..Default::default() },
            resource_id: 11.into(), padding: 0.into(),
        };
        test.submit(unref.as_bytes(), 0, true);
        assert!(test.frontend.virtio_gpu.has_deferred_native_ahb());
        assert_eq!(test.frontend.native_ahb_release.waiting_count(), 1);
        consumer.submissions.borrow()[0].1.signal().unwrap();
        complete(&mut test).unwrap();
        assert_eq!(response(&test, 1).hdr.type_.to_native(), VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID);
        assert_eq!(consumer.submissions.borrow().len(), 1);
        assert!(!test.frontend.virtio_gpu.has_deferred_native_ahb());
        assert!(test.frontend.pending_flip_fences.is_empty());
        resource(&mut test, 11);
        test.submit(command(VIRTIO_GPU_CMD_REFRESH_NATIVE_AHB, 11, sequence).as_bytes(), 0, true);
        assert_eq!(response(&test, 3).hdr.type_.to_native(), VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
        assert_eq!(consumer.submissions.borrow().len(), 1);
    }

    #[cfg(feature = "android_display_stub")]
    #[test]
    fn refresh_failed_release_retains_reader_and_never_submits_new_frame() {
        let (mut test, consumer, sequence) = refresh_fixture();
        consumer.generation.set(3);
        test.submit(command(VIRTIO_GPU_CMD_REFRESH_NATIVE_AHB, 11, sequence).as_bytes(), 0, true);
        let result = test.frontend.complete_flip_fences_with(
            &test.queues[0], &test.queues[1], &WaitContext::new().unwrap(),
            |_| Err(anyhow!("injected failed release")));
        assert!(result.is_err());
        assert_eq!(test.used(0), 1);
        assert_eq!(test.frontend.pending_flip_fences.len(), 1);
        assert_eq!(test.frontend.native_ahb_release.waiting_count(), 1);
        assert_eq!(test.frontend.native_ahb_release.released, 0);
        assert_eq!(consumer.submissions.borrow().len(), 1);
        assert!(test.frontend.check_quiescent().is_err());
    }

    struct TestAllocation;
    impl HostSharedAllocation for TestAllocation {
        fn description(&self) -> SharedAllocationDescription {
            SharedAllocationDescription { version: 1, size: 64, buffer_id: 11, allocation_size: 4,
                width: 1, height: 1, plane_stride: 4, fourcc: u32::from_le_bytes(*b"AB24"),
                plane_count: 1, layout_flags: 1, ..Default::default() }
        }
        fn duplicate_descriptor(&self) -> anyhow::Result<SafeDescriptor> {
            Err(anyhow!("fixture does not perform DMA import"))
        }
    }

    fn resource(test: &mut Harness, id: u32) {
        test.frontend.virtio_gpu.inject_native_ahb_for_test(id, Box::new(TestAllocation));
    }

    fn command(opcode: u32, id: u32, sequence: u64) -> virtio_gpu_native_ahb_operation {
        virtio_gpu_native_ahb_operation {
            hdr: virtio_gpu_ctrl_hdr { type_: opcode.into(), ..Default::default() },
            resource_id: id.into(), reserved: 0.into(), sequence: sequence.into(),
        }
    }

    fn response(test: &Harness, request: u16) -> virtio_gpu_native_ahb_operation {
        test.mem.read_obj_from_addr(GuestAddress(0x1800 + u64::from(request % 4) * 0x400 + 40)).unwrap()
    }

    fn present(test: &mut Harness, id: u32, count: usize) -> (u64, Vec<Event>) {
        let events: Vec<_> = (0..count).map(|_| Event::new().unwrap()).collect();
        let readers = events.iter().map(|event| {
            FlipCompletionFence::AndroidReleaseEvent(
                SafeDescriptor::try_from(event as &dyn AsRawDescriptor).unwrap())
        }).collect();
        test.frontend.virtio_gpu.inject_native_ahb_readers_for_test(id, readers);
        let index = test.submitted[0];
        test.submit(command(VIRTIO_GPU_CMD_PRESENT_NATIVE_AHB, id, 0).as_bytes(), 0, true);
        let reply = response(test, index);
        assert_eq!(reply.hdr.type_.to_native(), VIRTIO_GPU_RESP_OK_NATIVE_AHB_OPERATION);
        assert_eq!(reply.resource_id.to_native(), id);
        assert_eq!(reply.reserved.to_native(), 0);
        assert_ne!(reply.sequence.to_native(), 0);
        (reply.sequence.to_native(), events)
    }

    fn complete(test: &mut Harness) -> anyhow::Result<()> {
        test.frontend.complete_flip_fences(&test.queues[0], &test.queues[1], &WaitContext::new().unwrap())
    }

    #[test]
    fn undisplayed_present_completes_and_releases_immediately() {
        // A closed native display takes no reader; the present must still
        // succeed and its release wait must complete without a reader.
        let mut test = Harness::new(true);
        resource(&mut test, 11);
        let (sequence, _readers) = present(&mut test, 11, 0);
        assert!(test.frontend.pending_flip_fences.is_empty());
        test.submit(command(VIRTIO_GPU_CMD_WAIT_NATIVE_AHB_RELEASE, 11, sequence).as_bytes(), 0, true);
        let reply = response(&test, 1);
        assert_eq!(reply.hdr.type_.to_native(), VIRTIO_GPU_RESP_OK_NATIVE_AHB_OPERATION);
        assert_eq!(reply.sequence.to_native(), sequence);
        let (next, _readers) = present(&mut test, 11, 0);
        assert!(next > sequence);
    }

    #[test]
    fn next_buffer_presents_while_previous_wait_retains_every_reader() {
        let mut test = Harness::new(true);
        resource(&mut test, 11);
        resource(&mut test, 12);
        let (first, first_readers) = present(&mut test, 11, 2);
        assert_eq!(test.used(0), 1); // Acceptance does not wait for release.
        test.submit(command(VIRTIO_GPU_CMD_WAIT_NATIVE_AHB_RELEASE, 11, first).as_bytes(), 0, true);
        assert_eq!(test.used(0), 1);
        let (second, second_readers) = present(&mut test, 12, 1);
        assert!(second > first);
        assert_eq!(test.used(0), 2); // The earlier waiter cannot block this reply.
        assert!(!test.frontend.context_has_pending_work(999));
        first_readers[0].signal().unwrap();
        complete(&mut test).unwrap();
        assert_eq!(test.used(0), 2);
        assert_eq!(test.frontend.native_ahb_release.accepted, 2);
        assert_eq!(test.frontend.native_ahb_release.released, 0);
        first_readers[1].signal().unwrap();
        complete(&mut test).unwrap();
        assert_eq!(test.used(0), 3);
        assert_eq!(response(&test, 1).sequence.to_native(), first);
        assert_eq!(response(&test, 1).hdr.type_.to_native(), VIRTIO_GPU_RESP_OK_NATIVE_AHB_OPERATION);
        test.submit(command(VIRTIO_GPU_CMD_WAIT_NATIVE_AHB_RELEASE, 12, second).as_bytes(), 0, true);
        assert_eq!(test.used(0), 3);
        second_readers[0].signal().unwrap();
        complete(&mut test).unwrap();
        assert_eq!(test.used(0), 4);
        assert_eq!(test.frontend.native_ahb_release.released, 2);
        assert!(test.observed.lock().is_empty()); // No renderer fence fabricated.
        assert!(test.frontend.fence_state.lock().queue.completed_renderer.is_empty());
        assert!(test.frontend.check_quiescent().is_ok());
    }

    #[test]
    fn idle_front_buffer_has_no_false_watchdog_or_reuse() {
        let mut test = Harness::new(true);
        resource(&mut test, 11);
        let (sequence, _readers) = present(&mut test, 11, 1);
        test.submit(command(VIRTIO_GPU_CMD_WAIT_NATIVE_AHB_RELEASE, 11, sequence).as_bytes(), 0, true);
        assert!(test.frontend.next_flip_fence_deadline().is_none());
        complete(&mut test).unwrap();
        assert_eq!(test.used(0), 1);
        assert_eq!(test.frontend.pending_flip_fences.len(), 1);
        assert!(test.frontend.check_quiescent().is_err());
    }

    #[test]
    fn stale_sequence_duplicate_present_and_id_reuse_are_rejected() {
        let mut test = Harness::new(true);
        resource(&mut test, 11);
        let (sequence, readers) = present(&mut test, 11, 1);
        test.submit(command(VIRTIO_GPU_CMD_WAIT_NATIVE_AHB_RELEASE, 11, sequence + 1).as_bytes(), 0, true);
        assert_eq!(response(&test, 1).hdr.type_.to_native(), VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
        test.submit(command(VIRTIO_GPU_CMD_PRESENT_NATIVE_AHB, 11, 0).as_bytes(), 0, true);
        assert_eq!(response(&test, 2).hdr.type_.to_native(), VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
        assert_eq!(test.frontend.pending_flip_fences.len(), 1);
        readers[0].signal().unwrap();
        complete(&mut test).unwrap();
        let unref = virtio_gpu_resource_unref {
            hdr: virtio_gpu_ctrl_hdr { type_: VIRTIO_GPU_CMD_RESOURCE_UNREF.into(), ..Default::default() },
            resource_id: 11.into(), padding: 0.into(),
        };
        test.submit(unref.as_bytes(), 0, true);
        assert_eq!(test.response(0, size_of_val(&unref)).type_.to_native(), VIRTIO_GPU_RESP_OK_NODATA);
        resource(&mut test, 11);
        let (new_sequence, _readers) = present(&mut test, 11, 1);
        assert!(new_sequence > sequence);
        test.submit(command(VIRTIO_GPU_CMD_WAIT_NATIVE_AHB_RELEASE, 11, sequence).as_bytes(), 0, true);
        assert_eq!(response(&test, 5).hdr.type_.to_native(), VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
    }

    #[test]
    fn reader_error_or_timeout_never_grants_reuse() {
        for timeout in [false, true] {
            let mut test = Harness::new(true);
            resource(&mut test, 11);
            let (sequence, _readers) = present(&mut test, 11, 1);
            test.submit(command(VIRTIO_GPU_CMD_WAIT_NATIVE_AHB_RELEASE, 11, sequence).as_bytes(), 0, true);
            test.frontend.pending_flip_fences[0].deadline = Some(std::time::Instant::now());
            let result = test.frontend.complete_flip_fences_with(
                &test.queues[0], &test.queues[1], &WaitContext::new().unwrap(),
                |_| if timeout { Ok(false) } else { Err(anyhow!("injected failed release")) });
            // A failed release ends the device; an overdue one stays owned and pending.
            assert_eq!(result.is_err(), !timeout);
            assert_eq!(test.used(0), 1);
            assert_eq!(test.frontend.pending_flip_fences.len(), 1);
            assert_eq!(test.frontend.native_ahb_release.released, 0);
            assert!(test.frontend.check_quiescent().is_err());
        }
    }

    #[test]
    fn overdue_native_reader_completes_wait_only_after_late_release() {
        let mut test = Harness::new(true);
        resource(&mut test, 11);
        let (sequence, readers) = present(&mut test, 11, 2);
        test.submit(
            command(VIRTIO_GPU_CMD_WAIT_NATIVE_AHB_RELEASE, 11, sequence).as_bytes(),
            0,
            true,
        );
        let wait = WaitContext::new().unwrap();
        test.frontend.register_flip_fences(&wait).unwrap();
        test.frontend.pending_flip_fences[0].deadline = Some(std::time::Instant::now());
        test.frontend
            .complete_flip_fences_with(&test.queues[0], &test.queues[1], &wait, |_| Ok(false))
            .unwrap();
        assert_eq!(test.used(0), 1);
        assert_eq!(test.frontend.native_ahb_release.released, 0);
        assert_eq!(test.frontend.pending_flip_fences.len(), 2);
        assert!(test.frontend.pending_flip_fences[0].deadline.is_none());

        readers[0].signal().unwrap();
        complete(&mut test).unwrap();
        assert_eq!(test.used(0), 1);
        assert_eq!(test.frontend.native_ahb_release.released, 0);

        readers[1].signal().unwrap();
        complete(&mut test).unwrap();
        assert_eq!(test.used(0), 2);
        assert_eq!(test.frontend.native_ahb_release.released, 1);
        assert_eq!(response(&test, 1).sequence.to_native(), sequence);
        assert!(test.frontend.pending_flip_fences.is_empty());
        let (next, _) = present(&mut test, 11, 0);
        assert!(next > sequence);
    }

    #[test]
    fn malformed_private_requests_have_no_display_side_effects() {
        let mut test = Harness::new(true);
        resource(&mut test, 11);
        let mut request = command(VIRTIO_GPU_CMD_PRESENT_NATIVE_AHB, 11, 0);
        request.hdr.flags = VIRTIO_GPU_FLAG_FENCE.into();
        test.submit(request.as_bytes(), 0, true);
        assert_eq!(response(&test, 0).hdr.type_.to_native(), VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
        request.hdr.flags = 0.into();
        test.submit(request.as_bytes(), 0, false); // no space for exact sequence reply
        assert!(test.frontend.native_ahb_release.sequences.is_empty());
        assert!(test.frontend.pending_flip_fences.is_empty());
        assert!(test.observed.lock().is_empty());
    }

    #[test]
    fn unref_during_display_retains_holder_and_invalidates_waiter() {
        let mut test = Harness::new(true);
        resource(&mut test, 11);
        let (sequence, readers) = present(&mut test, 11, 1);
        test.submit(command(VIRTIO_GPU_CMD_WAIT_NATIVE_AHB_RELEASE, 11, sequence).as_bytes(), 0, true);
        let unref = virtio_gpu_resource_unref {
            hdr: virtio_gpu_ctrl_hdr { type_: VIRTIO_GPU_CMD_RESOURCE_UNREF.into(), ..Default::default() },
            resource_id: 11.into(), padding: 0.into(),
        };
        test.submit(unref.as_bytes(), 0, true);
        assert!(test.frontend.virtio_gpu.has_deferred_native_ahb());
        readers[0].signal().unwrap();
        complete(&mut test).unwrap();
        assert_eq!(response(&test, 1).hdr.type_.to_native(), VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID);
        assert!(!test.frontend.virtio_gpu.has_deferred_native_ahb());
    }
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

#[test]
fn native_reader_timeout_keeps_ownership_without_ending_the_device() {
    let mut test = Harness::new(true);
    let (_signal, event) = {
        let event = base::Event::new().unwrap();
        let fence = event.try_clone().unwrap();
        (event, fence)
    };
    test.frontend.pending_flip_fences.push(PendingFlipFence {
        resource_id: 11,
        fence_id: 7,
        ticket: None,
        fence: FlipCompletionFence::AndroidReleaseEvent(SafeDescriptor::from(event)),
        deadline: Some(std::time::Instant::now()),
        registered: false,
    });
    let wait = WaitContext::new().unwrap();
    assert!(test
        .frontend
        .complete_flip_fences_with(&test.queues[0], &test.queues[1], &wait, |_| Ok(false))
        .is_ok());
    assert_eq!(test.frontend.pending_flip_fences.len(), 1);
    assert!(test.frontend.pending_flip_fences[0].deadline.is_none());
}
