// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license.
//! Actual wire decoder, frontend, resource binding and scanout path; display peer only.
use super::*;
use std::rc::Rc;

struct Allocation {
    width: u32,
    height: u32,
    id: u64,
}
impl HostSharedAllocation for Allocation {
    fn description(&self) -> SharedAllocationDescription {
        SharedAllocationDescription {
            version: 1,
            size: 64,
            buffer_id: self.id,
            allocation_size: u64::from(self.width * self.height * 4),
            plane_stride: self.width * 4,
            width: self.width,
            height: self.height,
            fourcc: u32::from_le_bytes(*b"AB24"),
            plane_count: 1,
            layout_flags: 1,
            ..Default::default()
        }
    }
    fn duplicate_descriptor(&self) -> anyhow::Result<SafeDescriptor> {
        anyhow::bail!("fixture has no DMA fd")
    }
}
fn hdr(type_: u32) -> virtio_gpu_ctrl_hdr {
    virtio_gpu_ctrl_hdr {
        type_: type_.into(),
        ..Default::default()
    }
}
fn fixture() -> (
    Harness,
    Rc<gpu_display::native_refresh_test::Consumer>,
    ScanoutGeometry,
) {
    let mut test = Harness::new(true);
    let consumer = Rc::new(gpu_display::native_refresh_test::Consumer::default());
    consumer.generation.set(1);
    let display = gpu_display::native_refresh_test::open(consumer.clone()).unwrap();
    test.frontend
        .virtio_gpu
        .set_native_geometry_display_for_test(display, 4, 2);
    let mut g = ScanoutGeometry::identity(4, 2, 1, 1);
    g.storage_width = 2;
    g.storage_height = 4;
    g.content_rotation = 3;
    (test, consumer, g)
}
fn send<T: IntoBytes + zerocopy::Immutable>(test: &mut Harness, command: &T) -> u32 {
    test.submit(command.as_bytes(), 0, true);
    test.response(0, size_of_val(command)).type_.to_native()
}
fn config(g: ScanoutGeometry) -> virtio_gpu_configure_native_geometry {
    virtio_gpu_configure_native_geometry {
        hdr: hdr(VIRTIO_GPU_CMD_CONFIGURE_NATIVE_GEOMETRY),
        geometry: g.into(),
        ..Default::default()
    }
}
fn binding(id: u32, g: ScanoutGeometry) -> virtio_gpu_bind_native_geometry {
    virtio_gpu_bind_native_geometry {
        hdr: hdr(VIRTIO_GPU_CMD_BIND_NATIVE_GEOMETRY),
        resource_id: id.into(),
        reset_generation: g.reset_generation.into(),
        mode_generation: g.mode_generation.into(),
        ..Default::default()
    }
}
fn present(id: u32, g: ScanoutGeometry, sequence: u64) -> virtio_gpu_present_native_geometry {
    virtio_gpu_present_native_geometry {
        operation: virtio_gpu_native_ahb_operation {
            hdr: hdr(if sequence == 0 {
                VIRTIO_GPU_CMD_PRESENT_NATIVE_GEOMETRY
            } else {
                VIRTIO_GPU_CMD_REFRESH_NATIVE_GEOMETRY
            }),
            resource_id: id.into(),
            sequence: sequence.into(),
            ..Default::default()
        },
        reset_generation: g.reset_generation.into(),
        mode_generation: g.mode_generation.into(),
    }
}
fn response<T: zerocopy::FromBytes + zerocopy::IntoBytes>(test: &Harness, request_size: u64) -> T {
    let address = 0x1800 + u64::from((test.submitted[0] - 1) % 4) * 0x400 + request_size;
    test.mem.read_obj_from_addr(GuestAddress(address)).unwrap()
}
fn query(test: &mut Harness) -> virtio_gpu_configure_native_geometry {
    let q = virtio_gpu_query_native_geometry {
        hdr: hdr(VIRTIO_GPU_CMD_QUERY_NATIVE_GEOMETRY),
        ..Default::default()
    };
    assert_eq!(send(test, &q), VIRTIO_GPU_RESP_OK_NATIVE_GEOMETRY);
    response(test, 32)
}
fn resource(test: &mut Harness, id: u32, g: ScanoutGeometry) {
    test.frontend.virtio_gpu.inject_geometry_resource_for_test(
        id,
        Box::new(Allocation {
            width: g.storage_width,
            height: g.storage_height,
            id: u64::from(id),
        }),
    );
}
fn scanout(test: &mut Harness, id: u32, g: ScanoutGeometry) -> u32 {
    let command = virtio_gpu_set_scanout_blob {
        hdr: hdr(VIRTIO_GPU_CMD_SET_SCANOUT_BLOB),
        resource_id: id.into(),
        r: virtio_gpu_rect {
            width: g.storage_width.into(),
            height: g.storage_height.into(),
            ..Default::default()
        },
        width: g.storage_width.into(),
        height: g.storage_height.into(),
        format: VIRTIO_GPU_FORMAT_R8G8B8A8_UNORM.into(),
        strides: [(g.storage_width * 4).into(), 0.into(), 0.into(), 0.into()],
        ..Default::default()
    };
    send(test, &command)
}
fn complete(test: &mut Harness) {
    let wait = WaitContext::new().unwrap();
    test.frontend.register_flip_fences(&wait).unwrap();
    test.frontend
        .complete_flip_fences(&test.queues[0], &test.queues[1], &wait)
        .unwrap();
}

#[test]
fn profile_bootstrap_prepares_without_a_resource_and_reset_retires_bridge_mode() {
    let mut test = Harness::new(true);
    test.frontend.native_geometry_negotiated = true;
    let consumer = Rc::new(gpu_display::native_refresh_test::Consumer::default());
    consumer.generation.set(1);
    let p = ScanoutProfile { flags: 7, storage_width: 2, storage_height: 4,
        logical_width: 4, logical_height: 2, content_rotation: 3,
        endpoint_generation: 31, profile_generation: 47, ..ScanoutProfile::empty() };
    consumer.profile.set(Some(p));
    let display = gpu_display::native_refresh_test::open(consumer.clone()).unwrap();
    test.frontend.virtio_gpu.set_native_geometry_display_without_surface_for_test(display, 4, 2);
    assert_eq!(consumer.surfaces_created.get(), 0);
    test.frontend.virtio_gpu.prepare_native_scanout_profile().unwrap();
    test.frontend.virtio_gpu.prepare_native_scanout_profile().unwrap();
    assert_eq!(consumer.surfaces_created.get(), 1);
    assert!(matches!(test.frontend.virtio_gpu.query_native_profile(0).unwrap(),
        GpuResponse::OkNativeProfile { profile, reset_generation: 1, .. }
        if profile.flags.to_native() == 7));
    let g = p.geometry(1, 1);
    test.frontend.virtio_gpu.configure_native_profile(0, g, 31, 47).unwrap();
    assert_eq!(consumer.geometry.get(), Some(g));
    assert!(consumer.submissions.borrow().is_empty());
    assert_eq!(test.frontend.native_ahb_release.released, 0);
    test.frontend.virtio_gpu.reset().unwrap();
    assert_eq!(consumer.geometry_resets.borrow().as_slice(), &[(1, 2)]);
    assert_eq!(consumer.geometry.get(), None);
    assert_eq!(consumer.profile.get(), Some(p)); // Actual endpoint never reset.
    assert!(test.frontend.virtio_gpu.snapshot().is_err()); // Sticky native policy is not migratable.
    test.frontend.virtio_gpu.present_external(4, 2, 16, gpu_display::DRM_FORMAT_ABGR8888, &[0; 32]).unwrap();
    assert!(consumer.submissions.borrow().is_empty()); // No simplefb fallback after reset.
    test.frontend.virtio_gpu.prepare_native_scanout_profile().unwrap();
    let bootstrap = ScanoutGeometry::identity(4, 2, 2, 1);
    resource(&mut test, 12, bootstrap);
    assert_eq!(scanout(&mut test, 12, bootstrap), VIRTIO_GPU_RESP_OK_NODATA);
    let legacy = virtio_gpu_native_ahb_operation {
        hdr: hdr(VIRTIO_GPU_CMD_PRESENT_NATIVE_AHB), resource_id: 12.into(), ..Default::default()
    };
    assert_eq!(send(&mut test, &legacy), VIRTIO_GPU_RESP_OK_NATIVE_AHB_OPERATION);
    assert_eq!(consumer.submissions.borrow().len(), 1); // Feature-on mode0 native bootstrap.
    consumer.submissions.borrow()[0].1.signal().unwrap();
    complete(&mut test);
    assert_eq!(test.frontend.native_ahb_release.released, 1);
    assert_eq!(scanout(&mut test, 0, bootstrap), VIRTIO_GPU_RESP_OK_NODATA);
    assert!(matches!(test.frontend.virtio_gpu.query_native_profile(0).unwrap(),
        GpuResponse::OkNativeProfile { profile, .. } if profile.flags.to_native() == 7));
    // Another complete reset in mode0 still advances the bridge tombstone.
    test.frontend.virtio_gpu.reset().unwrap();
    assert_eq!(consumer.geometry_resets.borrow().as_slice(), &[(1, 2), (2, 3)]);
    test.frontend.virtio_gpu.prepare_native_scanout_profile().unwrap();
    assert_eq!(consumer.surfaces_created.get(), 4);
    assert!(matches!(test.frontend.virtio_gpu.query_native_profile(0).unwrap(),
        GpuResponse::OkNativeProfile { reset_generation: 3, .. }));
    let next = p.geometry(3, 1);
    test.frontend.virtio_gpu.configure_native_profile(0, next, 31, 47).unwrap();
    consumer.fail_geometry_reset.set(true);
    assert!(test.frontend.virtio_gpu.reset().is_err());
    assert_eq!(consumer.geometry_resets.borrow().as_slice(), &[(1, 2), (2, 3)]);
    assert_eq!(consumer.geometry.get(), Some(next));
    // A late bridge failure cannot publish the next reset token.
    assert!(matches!(test.frontend.virtio_gpu.query_native_profile(0).unwrap(),
        GpuResponse::OkNativeProfile { reset_generation: 3, .. }));
}

#[test]
fn profile_notification_is_independent_of_resources_and_displayed_attachment() {
    use super::super::native_geometry::ProfileObservation;
    let (mut test, consumer, g) = fixture();
    let mut previous = None;
    let event = AtomicBool::new(false);
    let observe = |test: &mut Harness, previous: &mut Option<ProfileObservation>| {
        Worker::observe_native_scanout_profile(&mut test.frontend, previous, &event)
    };
    assert!(!observe(&mut test, &mut previous));
    assert_eq!(previous, None);
    assert!(!event.load(Ordering::Acquire));
    test.frontend.native_geometry_negotiated = true;
    consumer.detached.set(true);
    assert!(observe(&mut test, &mut previous)); // Headless, no resource.
    assert!(event.swap(false, Ordering::AcqRel)); // ACK before QUERY.
    assert!(matches!(previous, Some(ProfileObservation::Snapshot { profile, .. })
        if profile.flags == 0));
    assert!(!observe(&mut test, &mut previous));
    let p = ScanoutProfile {
        version: 1, size: 64, flags: 7, content_rotation: g.content_rotation,
        storage_width: g.storage_width, storage_height: g.storage_height,
        logical_width: g.logical_width, logical_height: g.logical_height,
        endpoint_generation: 31, profile_generation: 47, reserved_tail: [0; 2],
    };
    consumer.detached.set(false);
    consumer.profile.set(Some(p));
    assert!(observe(&mut test, &mut previous));
    assert!(event.swap(false, Ordering::AcqRel));
    consumer.presented.set(consumer.generation.get());
    assert_eq!(test.frontend.display().borrow().native_refresh_generation(), 0);
    let mut newer = p;
    newer.profile_generation += 1;
    consumer.profile.set(Some(newer));
    assert!(observe(&mut test, &mut previous)); // Same displayed Surface.
    assert!(event.load(Ordering::Acquire));
    for _ in 0..100 { assert!(!observe(&mut test, &mut previous)); }
    newer.endpoint_generation += 1; // Same orientation, new root.
    consumer.profile.set(Some(newer));
    assert!(observe(&mut test, &mut previous));
    assert!(event.swap(false, Ordering::AcqRel));
    newer.profile_generation += 1; // Change after ACK remains pending.
    consumer.profile.set(Some(newer));
    assert!(observe(&mut test, &mut previous));
    let queried = test.frontend.virtio_gpu.query_native_profile(0).unwrap();
    assert!(matches!(queried, GpuResponse::OkNativeProfile { profile, .. }
        if profile.profile_generation.to_native() == newer.profile_generation));
    assert!(event.load(Ordering::Acquire));
    consumer.detached.set(true);
    assert!(observe(&mut test, &mut previous));
    assert!(!observe(&mut test, &mut previous));
    assert_eq!(consumer.geometry.get(), None);
    assert_eq!(consumer.configured_profile.get(), None);
    assert!(consumer.submissions.borrow().is_empty());
    assert!(test.frontend.pending_flip_fences.is_empty());
    assert_eq!(test.frontend.native_ahb_release.released, 0);
}

#[test]
fn malformed_profile_observation_revokes_and_recovers_without_submission() {
    use super::super::native_geometry::ProfileObservation;
    let (mut test, consumer, _) = fixture();
    test.frontend.native_geometry_negotiated = true;
    let mut previous = None;
    let event = AtomicBool::new(false);
    let mut bad = ScanoutProfile::empty();
    bad.flags = 1;
    consumer.profile.set(Some(bad));
    assert!(Worker::observe_native_scanout_profile(&mut test.frontend, &mut previous, &event));
    assert_eq!(previous, Some(ProfileObservation::Unavailable));
    event.store(false, Ordering::Release);
    assert!(!Worker::observe_native_scanout_profile(&mut test.frontend, &mut previous, &event));
    consumer.profile.set(None);
    assert!(Worker::observe_native_scanout_profile(&mut test.frontend, &mut previous, &event));
    assert!(matches!(previous, Some(ProfileObservation::Snapshot { host_reset: 1, profile })
        if profile.flags == 0));
    assert!(consumer.submissions.borrow().is_empty());
}

#[test]
fn geometry_capability_closed_and_wire_prerequisites_fail_without_state_changes() {
    let (mut test, consumer, g) = fixture();
    assert!(!VIRTIO_GPU_NATIVE_AHB_GEOMETRY_ENABLED);
    assert_eq!(VIRTIO_GPU_F_NATIVE_AHB_GEOMETRY, 11);
    let required = super::super::native_geometry::GEOMETRY_REQUIRED_FEATURES;
    for missing in [
        VIRTIO_GPU_F_NATIVE_AHB_V2,
        VIRTIO_GPU_F_NATIVE_AHB_RELEASE,
        VIRTIO_GPU_F_NATIVE_AHB_PAGING,
        VIRTIO_GPU_F_NATIVE_AHB_GEOMETRY,
    ] {
        assert!(!super::super::native_geometry::geometry_negotiated(
            required,
            required & !(1 << missing)
        ));
        assert!(!super::super::native_geometry::geometry_negotiated(
            required & !(1 << missing),
            required
        ));
    }
    assert!(super::super::native_geometry::geometry_negotiated(
        required, required
    ));
    assert_eq!(
        send(&mut test, &config(g)),
        VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER
    );
    assert_eq!(consumer.geometry.get(), None);
    test.frontend.native_geometry_test_enabled = true;
    assert_eq!(query(&mut test).geometry.mode_generation.to_native(), 0);
    for mutation in 0..6 {
        let mut bad = config(g);
        match mutation {
            0 => bad.hdr.flags = 1.into(),
            1 => bad.reserved = 1.into(),
            2 => bad.geometry.reserved_tail[1] = 1.into(),
            3 => bad.geometry.storage_width = 4.into(),
            4 => bad.geometry.reset_generation = 2.into(),
            _ => bad.geometry.mode_generation = 0.into(),
        }
        assert_eq!(send(&mut test, &bad), VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
    }
    test.submit(config(g).as_bytes(), 1, true);
    assert_eq!(
        test.response(1, 96).type_.to_native(),
        VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER
    );
    let mut excess = config(g).as_bytes().to_vec();
    excess.push(0);
    test.submit(&excess, 0, true);
    assert_eq!(
        test.response(0, 97).type_.to_native(),
        VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER
    );
    test.submit(config(g).as_bytes(), 0, false);
    assert_eq!(consumer.geometry.get(), None);
    consumer.detached.set(true);
    assert_eq!(send(&mut test, &config(g)), VIRTIO_GPU_RESP_ERR_UNSPEC);
    consumer.detached.set(false);
    assert_eq!(query(&mut test).geometry.mode_generation.to_native(), 0);
    assert_eq!(send(&mut test, &config(g)), VIRTIO_GPU_RESP_OK_NODATA);
    let mut next = g;
    next.mode_generation = 2;
    consumer.fail_geometry.set(true);
    assert_eq!(send(&mut test, &config(next)), VIRTIO_GPU_RESP_ERR_UNSPEC);
    assert_eq!(query(&mut test).geometry.mode_generation.to_native(), 1);
    next = g;
    next.content_rotation = 1;
    assert_eq!(
        send(&mut test, &config(next)),
        VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER
    );
    assert_eq!(consumer.geometry.get(), Some(g));
}

#[test]
fn geometry_binding_is_immutable_and_old_mode_keeps_real_release() {
    let (mut test, consumer, g) = fixture();
    test.frontend.native_geometry_test_enabled = true;
    assert_eq!(send(&mut test, &config(g)), VIRTIO_GPU_RESP_OK_NODATA);
    resource(&mut test, 11, g);
    assert_eq!(
        scanout(&mut test, 11, g),
        VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER
    );
    assert_eq!(send(&mut test, &binding(11, g)), VIRTIO_GPU_RESP_OK_NODATA);
    assert_eq!(scanout(&mut test, 11, g), VIRTIO_GPU_RESP_OK_NODATA);
    let logical = query(&mut test).geometry;
    assert_eq!(
        (
            logical.logical_width.to_native(),
            logical.logical_height.to_native()
        ),
        (4, 2)
    );
    let mut legacy = present(11, g, 0).operation;
    legacy.hdr = hdr(VIRTIO_GPU_CMD_PRESENT_NATIVE_AHB);
    assert_eq!(
        send(&mut test, &legacy),
        VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER
    );
    let flush = virtio_gpu_resource_flush {
        hdr: hdr(VIRTIO_GPU_CMD_RESOURCE_FLUSH),
        resource_id: 11.into(),
        ..Default::default()
    };
    assert_eq!(
        send(&mut test, &flush),
        VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER
    );
    resource(&mut test, 13, ScanoutGeometry::identity(4, 2, 1, 1));
    assert_eq!(
        send(&mut test, &binding(13, g)),
        VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER
    );
    assert_eq!(
        send(&mut test, &present(11, g, 0)),
        VIRTIO_GPU_RESP_OK_NATIVE_AHB_OPERATION
    );
    let accepted: virtio_gpu_native_ahb_operation = response(&test, 56);
    assert!(test.frontend.native_ahb_has_reader(11));
    assert_eq!(consumer.geometry_submissions.borrow().as_slice(), &[g]);
    assert_eq!(
        send(&mut test, &binding(11, g)),
        VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER
    );
    let mut next = g;
    next.mode_generation = 2;
    next.content_rotation = 1;
    assert_eq!(send(&mut test, &config(next)), VIRTIO_GPU_RESP_OK_NODATA);
    assert_eq!(
        send(&mut test, &present(11, g, 0)),
        VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER
    );
    assert_eq!(
        send(&mut test, &present(11, next, 0)),
        VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER
    );
    assert!(test.frontend.native_ahb_has_reader(11));
    consumer.submissions.borrow()[0].1.signal().unwrap();
    complete(&mut test);
    assert!(!test.frontend.native_ahb_has_reader(11));
    assert_eq!(
        send(&mut test, &binding(11, next)),
        VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER
    );
    let wait = virtio_gpu_native_ahb_operation {
        hdr: hdr(VIRTIO_GPU_CMD_WAIT_NATIVE_AHB_RELEASE),
        ..accepted
    };
    assert_eq!(
        send(&mut test, &wait),
        VIRTIO_GPU_RESP_OK_NATIVE_AHB_OPERATION
    );
    resource(&mut test, 12, next);
    assert_eq!(
        send(&mut test, &binding(12, next)),
        VIRTIO_GPU_RESP_OK_NODATA
    );
    assert_eq!(scanout(&mut test, 12, next), VIRTIO_GPU_RESP_OK_NODATA);
    assert_eq!(
        send(&mut test, &present(12, next, 0)),
        VIRTIO_GPU_RESP_OK_NATIVE_AHB_OPERATION
    );
    consumer.submissions.borrow()[1].1.signal().unwrap();
    complete(&mut test);
}

#[test]
fn geometry_retry_rechecks_bound_mode_and_reset_token_is_host_owned() {
    let (mut test, consumer, g) = fixture();
    test.frontend.native_geometry_test_enabled = true;
    assert_eq!(send(&mut test, &config(g)), VIRTIO_GPU_RESP_OK_NODATA);
    resource(&mut test, 11, g);
    assert_eq!(send(&mut test, &binding(11, g)), VIRTIO_GPU_RESP_OK_NODATA);
    assert_eq!(scanout(&mut test, 11, g), VIRTIO_GPU_RESP_OK_NODATA);
    assert_eq!(
        send(&mut test, &present(11, g, 0)),
        VIRTIO_GPU_RESP_OK_NATIVE_AHB_OPERATION
    );
    resource(&mut test, 12, g);
    assert_eq!(send(&mut test, &binding(12, g)), VIRTIO_GPU_RESP_OK_NODATA);
    assert_eq!(scanout(&mut test, 12, g), VIRTIO_GPU_RESP_OK_NODATA);
    consumer.fail_next_flip.set(true);
    test.submit(present(12, g, 0).as_bytes(), 0, true);
    assert!(test.frontend.native_ahb_present_pending(12));
    assert_eq!(
        send(&mut test, &binding(12, g)),
        VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER
    );
    assert!(test.frontend.native_ahb_present_pending(12));
    let mut next = g;
    next.mode_generation = 2;
    assert_eq!(send(&mut test, &config(next)), VIRTIO_GPU_RESP_OK_NODATA);
    let completed = test
        .frontend
        .retry_native_ahb_presents(std::time::Instant::now() + Duration::from_secs(1));
    assert_eq!(completed.len(), 1);
    return_fenced_descriptors(completed, &test.queues[0], Some(&test.queues[1]));
    assert_eq!(consumer.submissions.borrow().len(), 1);
    assert!(!test.frontend.native_ahb_present_pending(12));
    assert!(test.frontend.native_ahb_has_reader(11));
    consumer.submissions.borrow()[0].1.signal().unwrap();
    complete(&mut test);
    assert_eq!(query(&mut test).geometry.reset_generation.to_native(), 1);
    assert!(test.frontend.virtio_gpu.snapshot().is_err());
    test.frontend.virtio_gpu.reset().unwrap();
    let state = query(&mut test);
    assert_eq!(state.geometry.reset_generation.to_native(), 2);
    assert_eq!(state.geometry.mode_generation.to_native(), 0);
    assert_eq!(state.reserved.to_native(), 0);
    assert_eq!(
        send(&mut test, &config(g)),
        VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER
    );
}

#[test]
fn geometry_delayed_refresh_cannot_relabel_old_content_after_mode_change() {
    let (mut test, consumer, g) = fixture();
    test.frontend.native_geometry_test_enabled = true;
    assert_eq!(send(&mut test, &config(g)), VIRTIO_GPU_RESP_OK_NODATA);
    resource(&mut test, 11, g);
    assert_eq!(send(&mut test, &binding(11, g)), VIRTIO_GPU_RESP_OK_NODATA);
    assert_eq!(scanout(&mut test, 11, g), VIRTIO_GPU_RESP_OK_NODATA);
    assert_eq!(
        send(&mut test, &present(11, g, 0)),
        VIRTIO_GPU_RESP_OK_NATIVE_AHB_OPERATION
    );
    let accepted: virtio_gpu_native_ahb_operation = response(&test, 56);
    consumer.generation.set(3);
    test.submit(
        present(11, g, accepted.sequence.to_native()).as_bytes(),
        0,
        true,
    );
    let refresh_slot = (test.submitted[0] - 1) % 4;
    assert_eq!(test.frontend.native_ahb_release.waiting_count(), 1);
    assert_eq!(
        send(&mut test, &binding(11, g)),
        VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER
    );
    assert_eq!(test.frontend.native_ahb_release.waiting_count(), 1);
    let mut next = g;
    next.mode_generation = 2;
    assert_eq!(send(&mut test, &config(next)), VIRTIO_GPU_RESP_OK_NODATA);
    assert_eq!(test.frontend.native_ahb_release.released, 0);
    consumer.submissions.borrow()[0].1.signal().unwrap();
    complete(&mut test);
    assert_eq!(test.frontend.native_ahb_release.waiting_count(), 0);
    assert_eq!(test.frontend.native_ahb_release.released, 1);
    assert_eq!(consumer.submissions.borrow().len(), 1);
    let failed: virtio_gpu_ctrl_hdr = test
        .mem
        .read_obj_from_addr(GuestAddress(0x1800 + u64::from(refresh_slot) * 0x400 + 56))
        .unwrap();
    assert_eq!(
        failed.type_.to_native(),
        VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER
    );
}

#[test]
fn preferred_profile_is_distinct_and_revokes_bound_presents_without_release() {
    let (mut test, consumer, g) = fixture();
    let p = gpu_display::ScanoutProfile {
        version: 1, size: 64, flags: 7, content_rotation: g.content_rotation,
        storage_width: g.storage_width, storage_height: g.storage_height,
        logical_width: g.logical_width, logical_height: g.logical_height,
        endpoint_generation: 31, profile_generation: 47, reserved_tail: [0; 2],
    };
    consumer.profile.set(Some(p));
    let query_profile = virtio_gpu_query_native_geometry {
        hdr: hdr(VIRTIO_GPU_CMD_QUERY_NATIVE_PROFILE), ..Default::default()
    };
    assert_eq!(send(&mut test, &query_profile), VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
    test.frontend.native_geometry_test_enabled = true;
    assert_eq!(send(&mut test, &query_profile), VIRTIO_GPU_RESP_OK_NATIVE_PROFILE);
    let reply: virtio_gpu_native_profile_response = response(&test, 32);
    assert_eq!(reply.reset_generation.to_native(), 1);
    assert_eq!(reply.profile.endpoint_generation.to_native(), 31);
    assert_eq!(reply.profile.profile_generation.to_native(), 47);
    assert_eq!(reply.profile.flags.to_native(), 7);
    assert_eq!(query(&mut test).geometry.mode_generation.to_native(), 0);
    let mut command = virtio_gpu_configure_native_profile {
        config: config(g), endpoint_generation: 31.into(), profile_generation: 47.into(),
    };
    command.config.hdr = hdr(VIRTIO_GPU_CMD_CONFIGURE_NATIVE_PROFILE);
    let mut bad = command; bad.profile_generation = 48.into();
    assert_eq!(send(&mut test, &bad), VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
    bad = command; bad.config.reserved = 1.into();
    assert_eq!(send(&mut test, &bad), VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
    consumer.fail_geometry.set(true);
    assert_eq!(send(&mut test, &command), VIRTIO_GPU_RESP_ERR_UNSPEC);
    assert_eq!(query(&mut test).geometry.mode_generation.to_native(), 0);
    assert_eq!(send(&mut test, &command), VIRTIO_GPU_RESP_OK_NODATA);
    assert_eq!(send(&mut test, &config(g)), VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
    resource(&mut test, 11, g);
    assert_eq!(send(&mut test, &binding(11, g)), VIRTIO_GPU_RESP_OK_NODATA);
    assert_eq!(scanout(&mut test, 11, g), VIRTIO_GPU_RESP_OK_NODATA);
    assert_eq!(send(&mut test, &present(11, g, 0)), VIRTIO_GPU_RESP_OK_NATIVE_AHB_OPERATION);
    let accepted: virtio_gpu_native_ahb_operation = response(&test, 56);
    let mut revoked = gpu_display::ScanoutProfile::empty();
    revoked.endpoint_generation = 31; revoked.profile_generation = 48;
    consumer.profile.set(Some(revoked));
    assert_eq!(send(&mut test, &query_profile), VIRTIO_GPU_RESP_OK_NATIVE_PROFILE);
    let unavailable: virtio_gpu_native_profile_response = response(&test, 32);
    assert_eq!(unavailable.profile.flags.to_native(), 0);
    assert_eq!(unavailable.profile.storage_width.to_native(), 0);
    assert_eq!(send(&mut test, &present(11, g, 0)), VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
    assert_eq!(send(&mut test, &present(11, g, accepted.sequence.to_native())), VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
    assert!(test.frontend.native_ahb_has_reader(11));
    assert_eq!(test.frontend.native_ahb_release.released, 0);
    resource(&mut test, 12, g);
    assert_eq!(send(&mut test, &binding(12, g)), VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
    consumer.submissions.borrow()[0].1.signal().unwrap(); complete(&mut test);
    let wait = virtio_gpu_native_ahb_operation { hdr: hdr(VIRTIO_GPU_CMD_WAIT_NATIVE_AHB_RELEASE), ..accepted };
    assert_eq!(send(&mut test, &wait), VIRTIO_GPU_RESP_OK_NATIVE_AHB_OPERATION);
    let mut next = p; next.profile_generation = 49;
    consumer.profile.set(Some(next));
    command.profile_generation = 49.into();
    assert_eq!(send(&mut test, &command), VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER); // Old mode cannot restamp.
    command.config.geometry.mode_generation = 2.into();
    assert_eq!(send(&mut test, &command), VIRTIO_GPU_RESP_OK_NODATA);
    assert_eq!(scanout(&mut test, 11, g), VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
}

#[test]
fn preferred_profile_revocation_cancels_pending_retry_and_refresh() {
    for refresh in [false, true] {
        let (mut test, consumer, g) = fixture();
        test.frontend.native_geometry_test_enabled = true;
        let p = gpu_display::ScanoutProfile {
            version: 1, size: 64, flags: 7, content_rotation: g.content_rotation,
            storage_width: g.storage_width, storage_height: g.storage_height,
            logical_width: g.logical_width, logical_height: g.logical_height,
            endpoint_generation: 31, profile_generation: 47, reserved_tail: [0; 2],
        };
        consumer.profile.set(Some(p));
        let mut command = virtio_gpu_configure_native_profile {
            config: config(g), endpoint_generation: 31.into(), profile_generation: 47.into(),
        };
        command.config.hdr = hdr(VIRTIO_GPU_CMD_CONFIGURE_NATIVE_PROFILE);
        assert_eq!(send(&mut test, &command), VIRTIO_GPU_RESP_OK_NODATA);
        resource(&mut test, 11, g);
        assert_eq!(send(&mut test, &binding(11, g)), VIRTIO_GPU_RESP_OK_NODATA);
        assert_eq!(scanout(&mut test, 11, g), VIRTIO_GPU_RESP_OK_NODATA);
        assert_eq!(send(&mut test, &present(11, g, 0)), VIRTIO_GPU_RESP_OK_NATIVE_AHB_OPERATION);
        let accepted: virtio_gpu_native_ahb_operation = response(&test, 56);
        if refresh {
            consumer.generation.set(3);
            test.submit(present(11, g, accepted.sequence.to_native()).as_bytes(), 0, true);
            assert_eq!(test.frontend.native_ahb_release.waiting_count(), 1);
        } else {
            resource(&mut test, 12, g);
            assert_eq!(send(&mut test, &binding(12, g)), VIRTIO_GPU_RESP_OK_NODATA);
            assert_eq!(scanout(&mut test, 12, g), VIRTIO_GPU_RESP_OK_NODATA);
            consumer.fail_next_flip.set(true);
            test.submit(present(12, g, 0).as_bytes(), 0, true);
            assert!(test.frontend.native_ahb_present_pending(12));
        }
        let mut revoked = p; revoked.profile_generation += 1;
        consumer.profile.set(Some(revoked));
        if !refresh {
            let ready = test.frontend.retry_native_ahb_presents(std::time::Instant::now() + Duration::from_secs(1));
            assert_eq!(ready.len(), 1);
            return_fenced_descriptors(ready, &test.queues[0], Some(&test.queues[1]));
            assert!(!test.frontend.native_ahb_present_pending(12));
        }
        assert!(test.frontend.native_ahb_has_reader(11));
        assert_eq!(test.frontend.native_ahb_release.released, 0);
        consumer.submissions.borrow()[0].1.signal().unwrap(); complete(&mut test);
        assert_eq!(test.frontend.native_ahb_release.waiting_count(), 0);
        assert_eq!(test.frontend.native_ahb_release.released, 1);
        assert_eq!(consumer.submissions.borrow().len(), 1);
    }
}
