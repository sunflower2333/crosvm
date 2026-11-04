// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license.
//! Actual decoder/used-ring tests. The display peer is deterministic, not hardware.
use super::*;
use std::rc::Rc;
struct Allocation(u64);
impl HostSharedAllocation for Allocation {
    fn description(&self) -> SharedAllocationDescription {
        SharedAllocationDescription {
            version: 1,
            size: 64,
            buffer_id: self.0,
            allocation_size: 128,
            plane_stride: 32,
            width: 8,
            height: 4,
            fourcc: u32::from_le_bytes(*b"AB24"),
            plane_count: 1,
            layout_flags: 1,
            ..Default::default()
        }
    }
    fn duplicate_descriptor(&self) -> anyhow::Result<SafeDescriptor> {
        anyhow::bail!("no DMA fixture")
    }
}
fn fixture() -> (Harness, Rc<gpu_display::native_refresh_test::Consumer>) {
    fixture_with_inline(true)
}

#[test]
fn native_blt_offer_requires_actual_backend_and_all_prerequisites() {
    use super::super::native_blt::{blt_feature_mask, blt_negotiated, BLT_REQUIRED_FEATURES};
    let bit = 1 << VIRTIO_GPU_F_NATIVE_BLT;
    let prerequisites = BLT_REQUIRED_FEATURES & !bit;
    assert_eq!(blt_feature_mask(0, prerequisites, true, true), bit);
    for (android, virgl) in [(false, true), (true, false), (false, false)] {
        assert_eq!(blt_feature_mask(u64::MAX, prerequisites, android, virgl) & bit, 0);
    }
    for index in 0..64 {
        let missing = 1u64 << index;
        if prerequisites & missing != 0 {
            assert_eq!(blt_feature_mask(u64::MAX, prerequisites & !missing, true, true) & bit, 0);
        }
        if BLT_REQUIRED_FEATURES & missing != 0 {
            assert!(!blt_negotiated(BLT_REQUIRED_FEATURES, BLT_REQUIRED_FEATURES & !missing));
            assert!(!blt_negotiated(BLT_REQUIRED_FEATURES & !missing, u64::MAX));
        }
    }
    assert!(blt_negotiated(BLT_REQUIRED_FEATURES, BLT_REQUIRED_FEATURES));
}

#[test]
fn native_blt_worker_activation_replaces_negotiation_and_reset_revokes_it() {
    let (h, _) = fixture();
    let (mut worker, _exit, mut request) = super::reset_worker::fixture_from(h);
    request.native_blt_negotiated = true;
    worker.on_activate(request).unwrap();
    assert!(worker.state.native_blt_negotiated);
    worker.state.native_blt.poisoned.insert(17);
    worker.on_reset_with_drain_timeout(Duration::from_millis(100)).unwrap();
    assert!(!worker.state.native_blt_negotiated);
    assert!(worker.state.native_blt.poisoned.is_empty());
    // Even an existing negotiated frontend cannot authorize a new activation.
    let (h, _) = fixture();
    let (mut worker, _exit, request) = super::reset_worker::fixture_from(h);
    worker.on_activate(request).unwrap();
    assert!(!worker.state.native_blt_negotiated);
}
fn fixture_with_inline(inline: bool) -> (Harness, Rc<gpu_display::native_refresh_test::Consumer>) {
    let mut h = Harness::new(inline);
    let consumer = Rc::new(gpu_display::native_refresh_test::Consumer::default());
    h.frontend.virtio_gpu.set_display_timing_provider_for_test(
        gpu_display::native_refresh_test::open(consumer.clone()).unwrap(),
    );
    h.frontend.native_blt_negotiated = true;
    for id in [17, 29] {
        h.frontend
            .virtio_gpu
            .inject_geometry_resource_for_test(id, Box::new(Allocation(u64::from(id))));
    }
    (h, consumer)
}
fn hdr(t: u32) -> virtio_gpu_ctrl_hdr {
    virtio_gpu_ctrl_hdr {
        type_: t.into(),
        ..Default::default()
    }
}
fn region() -> VirtioGpuNativeBltRegion {
    VirtioGpuNativeBltRegion {
        source_x: 1.into(),
        source_y: 0.into(),
        destination_x: 2.into(),
        destination_y: 1.into(),
        width: 4.into(),
        height: 2.into(),
    }
}
fn endpoint(h: &Harness, id: u32) -> VirtioGpuNativeBltEndpoint {
    VirtioGpuNativeBltEndpoint {
        resource_id: id.into(),
        width: 8.into(),
        height: 4.into(),
        stride: 32.into(),
        fourcc: u32::from_le_bytes(*b"AB24").into(),
        incarnation: h
            .frontend
            .virtio_gpu
            .native_blt_identity(id)
            .unwrap()
            .0
            .into(),
        size: 128.into(),
        buffer_id: u64::from(id).into(),
        ..Default::default()
    }
}
fn command(h: &Harness) -> virtio_gpu_native_blt {
    virtio_gpu_native_blt {
        hdr: hdr(VIRTIO_GPU_CMD_NATIVE_BLT),
        cookie: 71.into(),
        reset_generation: 1.into(),
        flags: VIRTIO_GPU_NATIVE_BLT_COPY_ONLY.into(),
        region_count: 1.into(),
        source: endpoint(h, 17),
        destination: endpoint(h, 29),
    }
}

fn ordinary(h: &mut Harness) -> (VirtioGpuNativeBltEndpoint, Vec<u8>) {
    ordinary_format(h, VIRTIO_GPU_FORMAT_R8G8B8A8_UNORM, u32::from_le_bytes(*b"AB24"))
}
fn ordinary_format(h: &mut Harness, format: u32, fourcc: u32) -> (VirtioGpuNativeBltEndpoint, Vec<u8>) {
    let create = virtio_gpu_resource_create_2d {
        hdr: hdr(VIRTIO_GPU_CMD_RESOURCE_CREATE_2D),
        resource_id: 41.into(),
        format: format.into(),
        width: 8.into(),
        height: 4.into(),
    };
    h.submit(create.as_bytes(), 0, true);
    assert!(matches!(
        h.response(0, create.as_bytes().len()).type_.to_native(),
        VIRTIO_GPU_RESP_OK_NODATA | VIRTIO_GPU_RESP_OK_RESOURCE_PLANE_INFO
    ));
    h.frontend
        .virtio_gpu
        .attach_backing(
            41,
            &h.mem,
            vec![(GuestAddress(0x8000), 43), (GuestAddress(0x9000), 117)],
        )
        .unwrap();
    let data: Vec<u8> = (0..160).map(|i| (i * 7 + 3) as u8).collect();
    h.mem
        .write_all_at_addr(&data[..43], GuestAddress(0x8000))
        .unwrap();
    h.mem
        .write_all_at_addr(&data[43..], GuestAddress(0x9000))
        .unwrap();
    (
        VirtioGpuNativeBltEndpoint {
            resource_id: 41.into(),
            width: 8.into(),
            height: 4.into(),
            stride: 40.into(),
            fourcc: fourcc.into(),
            size: 160.into(),
            incarnation: h
                .frontend
                .virtio_gpu
                .native_blt_identity(41)
                .unwrap()
                .0
                .into(),
            ..Default::default()
        },
        data,
    )
}
fn ordinary_bytes(h: &Harness) -> Vec<u8> {
    let mut data = vec![0; 160];
    h.mem
        .read_exact_at_addr(&mut data[..43], GuestAddress(0x8000))
        .unwrap();
    h.mem
        .read_exact_at_addr(&mut data[43..], GuestAddress(0x9000))
        .unwrap();
    data
}
fn last_blt_response(h: &Harness) -> virtio_gpu_native_blt_response {
    h.mem
        .read_obj_from_addr(GuestAddress(
            0x1800 + u64::from((h.submitted[0] - 1) % 4) * 0x400 + 168,
        ))
        .unwrap()
}
#[test]
fn native_blt_ordinary_upload_uses_actual_split_guest_backing() {
    let (mut h, peer) = fixture();
    let (e, before) = ordinary(&mut h);
    let mut c = command(&h);
    c.source = e;
    assert_eq!(
        send(&mut h, c, &[region()], 80, 0),
        VIRTIO_GPU_RESP_OK_NATIVE_BLT
    );
    let uploads = peer.blt_uploads.borrow();
    assert_eq!(uploads.len(), 1);
    assert_eq!(uploads[0].0, e.layout());
    assert_eq!(uploads[0].1, before);
    assert_eq!(ordinary_bytes(&h), before);
    assert_eq!(last_blt_response(&h).uploaded_bytes.to_native(), 32);
    assert_eq!(last_blt_response(&h).readback_bytes.to_native(), 0);
    assert!(peer.submissions.borrow().is_empty());
}
#[test]
fn native_blt_ordinary_readback_preserves_padding_and_refreshes_host_texture() {
    let (mut h, peer) = fixture();
    let (e, mut expected) = ordinary(&mut h);
    let mut c = command(&h);
    c.destination = e;
    assert_eq!(
        send(&mut h, c, &[region()], 80, 0),
        VIRTIO_GPU_RESP_OK_NATIVE_BLT
    );
    for y in 0..2 {
        for x in 0..4 {
            let offset = (y + 1) * 40 + (x + 2) * 4;
            expected[offset..offset + 4].copy_from_slice(&[
                0x41 + x as u8,
                0x70 + y as u8,
                0xb3,
                0xff,
            ]);
        }
    }
    assert_eq!(ordinary_bytes(&h), expected);
    assert_eq!(peer.blt_readbacks.get(), 1);
    assert_eq!(last_blt_response(&h).readback_bytes.to_native(), 32);
    assert_eq!(last_blt_response(&h).uploaded_bytes.to_native(), 128);
    let slice = h.mem.get_slice_at_addr(GuestAddress(0xa000), 128).unwrap();
    h.frontend
        .virtio_gpu
        .transfer_read(
            0,
            41,
            rutabaga_gfx::Transfer3D {
                stride: 32,
                ..rutabaga_gfx::Transfer3D::new_2d(0, 0, 8, 4, 0)
            },
            Some(slice),
        )
        .unwrap();
    let mut cached = vec![0; 128];
    h.mem
        .read_exact_at_addr(&mut cached, GuestAddress(0xa000))
        .unwrap();
    for y in 0..4 {
        assert_eq!(
            &cached[y * 32..(y + 1) * 32],
            &expected[y * 40..y * 40 + 32]
        );
    }
    assert!(peer.submissions.borrow().is_empty());
}
#[test]
fn native_blt_ordinary_failed_readback_never_writes_guest_bytes() {
    let (mut h, peer) = fixture();
    let (e, before) = ordinary(&mut h);
    let mut c = command(&h);
    c.destination = e;
    peer.fail_blt.set(true);
    assert_eq!(
        send(&mut h, c, &[region()], 80, 0),
        VIRTIO_GPU_RESP_ERR_UNSPEC
    );
    assert_eq!(peer.blt_readbacks.get(), 1);
    assert_eq!(ordinary_bytes(&h), before);
    assert!(h.frontend.display_failed);
}

#[test]
fn native_blt_xrgb_destination_preserves_actual_identity_and_clipped_bytes() {
    for publish in [false, true] {
        let (mut h, peer) = owned_fixture();
        let (e, before) = ordinary_format(&mut h, VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM,
            u32::from_le_bytes(*b"XR24"));
        let mut c = command(&h);
        c.destination = e;
        if publish { c.flags = VIRTIO_GPU_NATIVE_BLT_PUBLISH.into(); }
        // Reproduce the old KMD's AR24 claim for an actual XR24 resource.
        c.destination.fourcc = u32::from_le_bytes(*b"AR24").into();
        assert_eq!(send(&mut h,c,&[region()],80,0),VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
        assert_eq!(ordinary_bytes(&h),before);
        assert_eq!(peer.blt_readbacks.get(),0);
        assert!(peer.submissions.borrow().is_empty());
        c.destination = e;
        assert_eq!(send(&mut h,c,&[region()],80,0),VIRTIO_GPU_RESP_OK_NATIVE_BLT);
        let mut expected = before;
        for y in 0..2 { for x in 0..4 {
            let offset=(y+1)*40+(x+2)*4;
            expected[offset..offset+4].copy_from_slice(&[0x41+x as u8,0x70+y as u8,0xb3,0xff]);
        }}
        assert_eq!(ordinary_bytes(&h),expected);
        assert_eq!(peer.blt_readbacks.get(),1);
        assert_eq!(last_blt_response(&h).readback_bytes.to_native(),32);
        assert_eq!(last_blt_response(&h).uploaded_bytes.to_native(),if publish {256} else {128});
        assert_eq!(peer.submissions.borrow().len(),usize::from(publish));
        assert!(!h.frontend.display_failed);
    }
}

#[test]
fn native_blt_ordinary_rejects_bad_layout_and_sg_before_transfer() {
    for case in 0..9 {
        let (mut h, peer) = fixture();
        let (mut e, before) = ordinary(&mut h);
        match case {
            0 => e.size = 161.into(),
            1 => e.size = 151.into(),
            2 => e.stride = 28.into(),
            3 => e.stride = 41.into(),
            4 => e.fourcc = u32::from_le_bytes(*b"AR24").into(),
            5 => e.buffer_id = 41.into(),
            6 => e.width = 9.into(),
            7 => {
                h.frontend
                    .virtio_gpu
                    .attach_backing(
                        41,
                        &h.mem,
                        vec![(GuestAddress(0x8000), 80), (GuestAddress(0x8030), 80)],
                    )
                    .unwrap();
                e.incarnation = h
                    .frontend
                    .virtio_gpu
                    .native_blt_identity(41)
                    .unwrap()
                    .0
                    .into();
            }
            _ => {
                h.frontend
                    .virtio_gpu
                    .attach_backing(
                        41,
                        &h.mem,
                        vec![(GuestAddress(0x8000), 43), (GuestAddress(0x9000), 117)],
                    )
                    .unwrap();
            }
        }
        let mut c = command(&h);
        c.destination = e;
        assert_eq!(
            send(&mut h, c, &[region()], 80, 0),
            VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER,
            "case {case}"
        );
        assert_eq!(peer.blt_readbacks.get(), 0);
        assert_eq!(ordinary_bytes(&h), before);
        assert!(!h.frontend.display_failed);
    }
}

#[test]
fn native_blt_context_attach_attempt_revokes_sg_authority_permanently() {
    let (mut h, peer) = fixture();
    let (e, before) = ordinary(&mut h);
    let _ = h.frontend.virtio_gpu.context_attach_resource(7, 41);
    let _ = h.frontend.virtio_gpu.context_detach_resource(7, 41);
    h.frontend
        .virtio_gpu
        .attach_backing(
            41,
            &h.mem,
            vec![(GuestAddress(0x8000), 43), (GuestAddress(0x9000), 117)],
        )
        .unwrap();
    assert!(h.frontend.virtio_gpu.native_blt_identity(41).is_none());
    let mut c = command(&h);
    c.source = e;
    assert_eq!(
        send(&mut h, c, &[region()], 80, 0),
        VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER
    );
    assert!(peer.blt_uploads.borrow().is_empty());
    assert_eq!(ordinary_bytes(&h), before);
}

#[test]
fn native_blt_cache_waits_for_global_poll_completion_before_response() {
    let (mut h, _) = fixture_with_inline(false);
    let (e, _) = ordinary(&mut h);
    let observed = h.observed.clone();
    let callback = h.callback.clone();
    let mem = h.mem.clone();
    let before = h.used(0);
    let did_poll = Rc::new(std::cell::Cell::new(false));
    let mark = did_poll.clone();
    h.frontend.virtio_gpu.native_blt_test_poll_hook = Some(Box::new(move || {
        assert_eq!(
            mem.read_obj_from_addr::<Le16>(GuestAddress(0x1302))
                .unwrap()
                .to_native(),
            before
        );
        let f = *observed.lock().last().unwrap();
        assert_eq!(f.flags, VIRTIO_GPU_FLAG_FENCE);
        assert_eq!(f.ctx_id, 0);
        callback.call(f);
        mark.set(true);
    }));
    let mut c = command(&h);
    c.destination = e;
    assert_eq!(
        send(&mut h, c, &[region()], 80, 0),
        VIRTIO_GPU_RESP_OK_NATIVE_BLT
    );
    assert!(did_poll.get());
    assert_eq!(h.used(0), before + 1);
    assert!(h.frontend.fence_state.lock().global_tokens.is_empty());
}

#[test]
fn native_blt_cache_timeout_or_late_callback_quarantines_activation() {
    for late in [false, true] {
        let (mut h, _) = fixture_with_inline(false);
        let (e, _) = ordinary(&mut h);
        if late {
            let observed = h.observed.clone();
            let callback = h.callback.clone();
            h.frontend.virtio_gpu.native_blt_test_poll_hook = Some(Box::new(move || {
                std::thread::sleep(Duration::from_millis(1020));
                callback.call(*observed.lock().last().unwrap());
            }));
        }
        let mut c = command(&h);
        c.destination = e;
        assert_eq!(
            send(&mut h, c, &[region()], 80, 0),
            VIRTIO_GPU_RESP_ERR_UNSPEC
        );
        assert!(h.frontend.display_failed);
        assert!(h.frontend.native_blt.poisoned.contains(&41));
        if !late {
            assert!(!h.frontend.fence_state.lock().global_tokens.is_empty());
        }
    }
}

#[test]
fn native_blt_cache_never_accepts_legacy_guest_watermark() {
    for maps in [false, true] {
        let (mut h, peer) = fixture();
        let (e, before) = ordinary(&mut h);
        h.frontend.maps_global_fences = maps;
        h.frontend.fence_state.lock().uses_global_tokens = false;
        h.frontend
            .fence_state
            .lock()
            .queue
            .completed_renderer
            .insert(VirtioGpuRing::Global, u64::MAX);
        let mut c = command(&h);
        c.destination = e;
        assert_eq!(
            send(&mut h, c, &[region()], 80, 0),
            VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER
        );
        assert_eq!(peer.blt_readbacks.get(), 0);
        assert_eq!(ordinary_bytes(&h), before);
    }
}

fn owned_fixture() -> (Harness, Rc<gpu_display::native_refresh_test::Consumer>) {
    let (mut h, peer) = fixture();
    peer.strict_buffers.set(true);
    peer.generation.set(1);
    h.frontend.virtio_gpu.set_native_geometry_display_for_test(
        gpu_display::native_refresh_test::open(peer.clone()).unwrap(),
        8,
        4,
    );
    let g = ScanoutGeometry::identity(8, 4, 1, 1);
    h.frontend
        .virtio_gpu
        .configure_native_geometry(0, g)
        .unwrap();
    h.frontend
        .virtio_gpu
        .bind_native_geometry(29, 0, 1, 1)
        .unwrap();
    (h, peer)
}

fn identity_fixture() -> (Harness, Rc<gpu_display::native_refresh_test::Consumer>) {
    let (mut h, peer) = fixture();
    peer.strict_buffers.set(true);
    peer.generation.set(1);
    h.frontend.virtio_gpu.set_native_geometry_display_for_test(
        gpu_display::native_refresh_test::open(peer.clone()).unwrap(), 8, 4);
    (h, peer)
}

fn bind_identity(h: &mut Harness, resource: u32) {
    let c = virtio_gpu_set_scanout_blob {
        hdr: hdr(VIRTIO_GPU_CMD_SET_SCANOUT_BLOB), resource_id: resource.into(),
        r: virtio_gpu_rect { width: 8.into(), height: 4.into(), ..Default::default() },
        width: 8.into(), height: 4.into(), format: VIRTIO_GPU_FORMAT_R8G8B8A8_UNORM.into(),
        strides: [32.into(), 0.into(), 0.into(), 0.into()], ..Default::default()
    };
    h.submit(c.as_bytes(), 0, true);
    assert_eq!(h.response(0, c.as_bytes().len()).type_.to_native(), VIRTIO_GPU_RESP_OK_NODATA);
}

#[test]
fn native_blt_owned_identity_survives_idle_and_same_size_rebind_oom() {
    let (mut h, peer) = identity_fixture();
    bind_identity(&mut h, 29);
    publish_owned(&mut h);
    h.frontend.virtio_gpu.expire_guest_present_for_test();
    assert!(!h.frontend.virtio_gpu.guest_owns_display());
    assert!(h.frontend.virtio_gpu.present_external(8, 4, 32,
        gpu_display::DRM_FORMAT_ABGR8888, &[0; 128]).is_ok());
    assert_eq!(peer.submissions.borrow().len(), 1);
    bind_identity(&mut h, 17);
    let mut next = command(&h);
    next.flags = VIRTIO_GPU_NATIVE_BLT_PUBLISH_ONLY.into();
    next.source = Default::default(); next.destination = endpoint(&h, 17); next.region_count = 0.into();
    peer.fail_allocation.set(true);
    assert_eq!(send(&mut h, next, &[], 80, 0), VIRTIO_GPU_RESP_ERR_OUT_OF_MEMORY);
    peer.fail_allocation.set(false);
    let calls = peer.blt_calls.get();
    next.flags = VIRTIO_GPU_NATIVE_BLT_REFRESH_ONLY.into(); next.destination = endpoint(&h, 29);
    assert_eq!(send(&mut h, next, &[], 80, 0), VIRTIO_GPU_RESP_OK_NATIVE_BLT);
    assert_eq!(peer.blt_calls.get(), calls);
    assert_eq!(peer.submissions.borrow().len(), 1);
}

#[test]
fn native_blt_owned_identity_retirement_preserves_actual_output_readers() {
    for detach in [false, true] {
        let (mut h, peer) = identity_fixture();
        bind_identity(&mut h, 29);
        publish_owned(&mut h);
        assert!(h.frontend.virtio_gpu.unref_resource(&h.mem, 999).is_err());
        h.frontend.virtio_gpu.unref_resource(&h.mem, 17).unwrap();
        assert!(h.frontend.virtio_gpu.native_blt_output_is_current(29));
        if detach {
            bind_identity(&mut h, 0);
            assert_eq!(h.frontend.virtio_gpu.native_ahb_allocated_bytes(), 256);
        } else {
            h.frontend.virtio_gpu.unref_resource(&h.mem, 29).unwrap();
            assert_eq!(h.frontend.virtio_gpu.native_ahb_allocated_bytes(), 128);
        }
        assert!(!h.frontend.virtio_gpu.native_blt_output_is_current(29));
        assert!(h.frontend.virtio_gpu.poll_native_blt_outputs().unwrap());
        peer.release_buffer(0).unwrap();
        assert!(!h.frontend.virtio_gpu.poll_native_blt_outputs().unwrap());
    }
}

#[test]
fn native_blt_resized_scanout_rejects_old_snapshot_refresh() {
    let (mut h, peer) = identity_fixture();
    bind_identity(&mut h, 29); publish_owned(&mut h);
    let ordinary = ordinary(&mut h).0;
    let resized = virtio_gpu_set_scanout {
        hdr: hdr(VIRTIO_GPU_CMD_SET_SCANOUT), scanout_id: 0.into(), resource_id: 41.into(),
        r: virtio_gpu_rect { width: 4.into(), height: 2.into(), ..Default::default() },
    };
    h.submit(resized.as_bytes(), 0, true);
    assert_eq!(h.response(0, resized.as_bytes().len()).type_.to_native(), VIRTIO_GPU_RESP_OK_NODATA);
    let mut c = command(&h); c.source = Default::default(); c.region_count = 0.into();
    c.destination = ordinary; c.flags = VIRTIO_GPU_NATIVE_BLT_PUBLISH_ONLY.into();
    let copies = peer.blt_calls.get();
    assert_eq!(send(&mut h, c, &[], 80, 0), VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
    c.destination = endpoint(&h, 29); c.flags = VIRTIO_GPU_NATIVE_BLT_REFRESH_ONLY.into();
    assert_eq!(send(&mut h, c, &[], 80, 0), VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
    assert_eq!(peer.blt_calls.get(), copies);
    assert!(h.frontend.virtio_gpu.poll_native_blt_outputs().unwrap());
    // Resize rollback is not implemented: fail closed, retain real readers.
}

#[test]
fn native_blt_identity_mode_publish_and_refresh_without_geometry_diagnostic() {
    for ordinary_destination in [false, true] {
        for initial_bind in [false, true] {
            let (mut h, peer) = identity_fixture();
            assert!(!h.frontend.native_geometry_negotiated);
            let mut c = command(&h);
            if ordinary_destination { c.destination = ordinary(&mut h).0; }
            c.flags = if initial_bind { VIRTIO_GPU_NATIVE_BLT_PUBLISH_ONLY }
                      else { VIRTIO_GPU_NATIVE_BLT_PUBLISH }.into();
            if initial_bind { c.source = Default::default(); c.region_count = 0.into(); }
            let regions = if initial_bind { vec![] } else { vec![region()] };
            assert_eq!(send(&mut h, c, &regions, 80, 0), VIRTIO_GPU_RESP_OK_NATIVE_BLT);
            assert_eq!(peer.submissions.borrow().len(), 1);
            assert!(peer.geometry_submissions.borrow().is_empty());
            assert!(matches!(h.frontend.virtio_gpu.query_native_geometry(0).unwrap(),
                GpuResponse::OkNativeGeometry { configured: false, geometry, .. }
                if geometry.mode_generation.to_native() == 0));
            let calls = peer.blt_calls.get();
            c.flags = VIRTIO_GPU_NATIVE_BLT_REFRESH_ONLY.into();
            c.source = Default::default(); c.region_count = 0.into();
            assert_eq!(send(&mut h, c, &[], 80, 0), VIRTIO_GPU_RESP_OK_NATIVE_BLT);
            assert_eq!(peer.blt_calls.get(), calls);
            assert_eq!(peer.submissions.borrow().len(), 1);
            peer.generation.set(2);
            assert_eq!(send(&mut h, c, &[], 80, 0), VIRTIO_GPU_RESP_ERR_OUT_OF_MEMORY);
            peer.release_buffer(0).unwrap();
            assert_eq!(send(&mut h, c, &[], 80, 0), VIRTIO_GPU_RESP_OK_NATIVE_BLT);
            assert_eq!(peer.submissions.borrow().len(), 2);
            assert_eq!(peer.blt_calls.get(), calls);
        }
    }
}

#[test]
fn native_blt_identity_mode_rejects_dimensions_or_configured_resource_before_copy() {
    for configured_resource in [false, true] {
        let (mut h, peer) = if configured_resource { owned_fixture() } else { identity_fixture() };
        let (width, height) = if configured_resource { (8, 4) } else { (4, 2) };
        h.frontend.virtio_gpu.set_native_geometry_display_for_test(
            gpu_display::native_refresh_test::open(peer.clone()).unwrap(), width, height);
        let mut c = command(&h); c.flags = VIRTIO_GPU_NATIVE_BLT_PUBLISH.into();
        assert_eq!(send(&mut h, c, &[region()], 80, 0), VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
        assert_eq!(peer.blt_calls.get(), 0);
        assert!(peer.submissions.borrow().is_empty());
    }
}
#[test]
fn native_blt_publish_uses_owned_output_and_keeps_primary_writable() {
    let (mut h, peer) = owned_fixture();
    let mut c = command(&h);
    c.flags = VIRTIO_GPU_NATIVE_BLT_PUBLISH.into();
    assert_eq!(
        send(&mut h, c, &[region()], 80, 0),
        VIRTIO_GPU_RESP_OK_NATIVE_BLT
    );
    assert_eq!(
        peer.blt_pairs.borrow().as_slice(),
        &[(17, 29), (29, 0x100001)]
    );
    assert_eq!(peer.imports.borrow()[0].1, 0x100001);
    assert_eq!(peer.submissions.borrow().len(), 1);
    assert!(!h.frontend.native_ahb_has_reader(29));
    c.flags = VIRTIO_GPU_NATIVE_BLT_COPY_ONLY.into();
    assert_eq!(
        send(&mut h, c, &[region()], 80, 0),
        VIRTIO_GPU_RESP_OK_NATIVE_BLT
    );
    assert_eq!(
        peer.submissions.borrow().len(),
        1,
        "CopyOnly must not refresh display"
    );
}
#[test]
fn native_blt_owned_pool_requires_each_actual_release_before_reuse() {
    let (mut h, peer) = owned_fixture();
    let mut c = command(&h);
    c.flags = VIRTIO_GPU_NATIVE_BLT_PUBLISH.into();
    for _ in 0..3 {
        assert_eq!(
            send(&mut h, c, &[region()], 80, 0),
            VIRTIO_GPU_RESP_OK_NATIVE_BLT
        );
    }
    let calls = peer.blt_calls.get();
    assert_ne!(
        send(&mut h, c, &[region()], 80, 0),
        VIRTIO_GPU_RESP_OK_NATIVE_BLT
    );
    assert_eq!(
        peer.blt_calls.get(),
        calls,
        "pool admission before destination mutation"
    );
    assert!(!h.frontend.display_failed);
    assert_eq!(peer.allocated.get(), 3);
    peer.release_buffer(0).unwrap();
    assert_eq!(
        send(&mut h, c, &[region()], 80, 0),
        VIRTIO_GPU_RESP_OK_NATIVE_BLT
    );
    assert_eq!(peer.allocated.get(), 3);
    assert_eq!(peer.blt_pairs.borrow().last().unwrap().1, 0x100001);
}
#[test]
fn native_blt_initial_mode_bind_snapshots_without_copying_primary_to_itself() {
    let (mut h, peer) = owned_fixture();
    let mut c = command(&h);
    c.flags = VIRTIO_GPU_NATIVE_BLT_PUBLISH_ONLY.into();
    c.source = Default::default();
    c.region_count = 0.into();
    assert_eq!(send(&mut h, c, &[], 80, 0), VIRTIO_GPU_RESP_OK_NATIVE_BLT);
    assert_eq!(peer.blt_pairs.borrow().as_slice(), &[(29, 0x100001)]);
    assert!(!h.frontend.native_ahb_has_reader(29));
}
#[test]
fn native_blt_refresh_reuses_published_snapshot_without_copyonly_pixels() {
    let (mut h, peer) = owned_fixture();
    let mut c = command(&h);
    c.flags = VIRTIO_GPU_NATIVE_BLT_PUBLISH.into();
    assert_eq!(
        send(&mut h, c, &[region()], 80, 0),
        VIRTIO_GPU_RESP_OK_NATIVE_BLT
    );
    c.flags = VIRTIO_GPU_NATIVE_BLT_COPY_ONLY.into();
    assert_eq!(
        send(&mut h, c, &[region()], 80, 0),
        VIRTIO_GPU_RESP_OK_NATIVE_BLT
    );
    let calls = peer.blt_calls.get();
    peer.generation.set(2);
    c.flags = VIRTIO_GPU_NATIVE_BLT_REFRESH_ONLY.into();
    c.source = Default::default();
    c.region_count = 0.into();
    assert_ne!(send(&mut h, c, &[], 80, 0), VIRTIO_GPU_RESP_OK_NATIVE_BLT);
    assert!(!h.frontend.display_failed, "held old consumer is a retryable refusal");
    assert_eq!(peer.submissions.borrow().len(), 1);
    peer.release_buffer(0).unwrap();
    assert_eq!(send(&mut h, c, &[], 80, 0), VIRTIO_GPU_RESP_OK_NATIVE_BLT);
    assert_eq!(
        peer.blt_calls.get(),
        calls,
        "refresh must not acquire/read the primary or snapshot"
    );
    assert_eq!(peer.imports.borrow().last().unwrap().1, 0x100001);
    assert_eq!(peer.submissions.borrow().len(), 2);
    // The refreshed consumer now owns this output again.
    let mut next = command(&h);
    next.flags = VIRTIO_GPU_NATIVE_BLT_PUBLISH.into();
    assert_eq!(
        send(&mut h, next, &[region()], 80, 0),
        VIRTIO_GPU_RESP_OK_NATIVE_BLT
    );
    assert_eq!(peer.allocated.get(), 2);
}
#[test]
fn native_blt_refresh_same_consumer_is_idempotent() {
    let (mut h, peer) = owned_fixture();
    let mut c = command(&h);
    c.flags = VIRTIO_GPU_NATIVE_BLT_PUBLISH.into();
    assert_eq!(send(&mut h, c, &[region()], 80, 0), VIRTIO_GPU_RESP_OK_NATIVE_BLT);
    let calls = peer.blt_calls.get();
    let attempts = peer.attempts.get();
    c.flags = VIRTIO_GPU_NATIVE_BLT_REFRESH_ONLY.into();
    c.source = Default::default();
    c.region_count = 0.into();
    for _ in 0..12 {
        assert_eq!(send(&mut h, c, &[], 80, 0), VIRTIO_GPU_RESP_OK_NATIVE_BLT);
    }
    assert_eq!(peer.attempts.get(), attempts);
    assert_eq!(peer.submissions.borrow().len(), 1);
    assert_eq!(peer.blt_calls.get(), calls);
}
#[test]
fn native_blt_failed_output_reservation_preserves_last_snapshot() {
    for failure in 0..3 {
        let (mut h, peer) = owned_fixture();
        let mut c = command(&h);
        c.flags = VIRTIO_GPU_NATIVE_BLT_PUBLISH.into();
        assert_eq!(send(&mut h, c, &[region()], 80, 0), VIRTIO_GPU_RESP_OK_NATIVE_BLT);
        peer.release_buffer(0).unwrap();
        peer.generation.set(2);
        match failure {
            0 => peer.fail_allocation.set(true),
            1 => peer.fail_import.set(true),
            _ => peer.allocation_size_override.set(Some(VIRTIO_GPU_NATIVE_AHB_BUDGET)),
        }
        let calls = peer.blt_calls.get();
        assert_ne!(send(&mut h, c, &[region()], 80, 0), VIRTIO_GPU_RESP_OK_NATIVE_BLT);
        assert_eq!(peer.blt_calls.get(), calls);
        assert!(!h.frontend.display_failed);
        peer.allocation_size_override.set(None);
        c.flags = VIRTIO_GPU_NATIVE_BLT_REFRESH_ONLY.into();
        c.source = Default::default();
        c.region_count = 0.into();
        assert_eq!(send(&mut h, c, &[], 80, 0), VIRTIO_GPU_RESP_OK_NATIVE_BLT, "failure {failure}");
        assert_eq!(peer.imports.borrow().last().unwrap().1, 0x100001);
        assert_eq!(peer.blt_calls.get(), calls);
    }
}
#[test]
fn native_blt_refused_direct_flip_preserves_last_snapshot() {
    let (mut h, peer) = owned_fixture();
    let mut c = command(&h);
    c.flags = VIRTIO_GPU_NATIVE_BLT_PUBLISH.into();
    assert_eq!(send(&mut h, c, &[region()], 80, 0), VIRTIO_GPU_RESP_OK_NATIVE_BLT);
    peer.fail_next_flip.set(true);
    h.frontend.virtio_gpu.flush_resource(29).unwrap();
    assert_eq!(peer.submissions.borrow().len(), 1);
    peer.release_buffer(0).unwrap();
    peer.generation.set(2);
    c.flags = VIRTIO_GPU_NATIVE_BLT_REFRESH_ONLY.into();
    c.source = Default::default();
    c.region_count = 0.into();
    assert_eq!(send(&mut h, c, &[], 80, 0), VIRTIO_GPU_RESP_OK_NATIVE_BLT);
    assert_eq!(peer.imports.borrow().last().unwrap().1, 0x100001);
}
#[test]
fn native_blt_owned_budget_admits_measured_desktop_browser_working_set() {
    // Live DXVK62 failed with 263233536 resident bytes before another
    // 3040x1904 frame (13312-byte stride, 25346048-byte allocation).
    // Exercise real admission/copy/publication without allocating that RAM.
    struct ResidentWorkingSet;
    impl HostSharedAllocation for ResidentWorkingSet {
        fn description(&self) -> SharedAllocationDescription {
            let mut desc = Allocation(0x8001).description();
            desc.allocation_size = 263233536 - 256; // fixture owns two 128-byte images
            desc
        }
        fn duplicate_descriptor(&self) -> anyhow::Result<SafeDescriptor> {
            anyhow::bail!("resident accounting fixture has no DMA descriptor")
        }
    }
    let (mut h, peer) = owned_fixture();
    h.frontend.virtio_gpu.inject_native_ahb_for_test(77, Box::new(ResidentWorkingSet));
    assert_eq!(h.frontend.virtio_gpu.native_ahb_allocated_bytes(), 263233536);
    peer.allocation_size_override.set(Some(25346048));
    let mut c = command(&h);
    c.flags = VIRTIO_GPU_NATIVE_BLT_PUBLISH.into();
    assert_eq!(send(&mut h, c, &[region()], 80, 0), VIRTIO_GPU_RESP_OK_NATIVE_BLT);
    assert_eq!(h.frontend.virtio_gpu.native_ahb_allocated_bytes(), 263233536 + 25346048);
    // Retain the first consumer while admitting the next complete frame.
    assert_eq!(send(&mut h, c, &[region()], 80, 0), VIRTIO_GPU_RESP_OK_NATIVE_BLT);
    assert_eq!(h.frontend.virtio_gpu.native_ahb_allocated_bytes(), 263233536 + 2 * 25346048);
    assert_eq!(peer.submissions.borrow().len(), 2);
    assert!(!h.frontend.display_failed);
}
#[test]
fn native_blt_owned_budget_checks_actual_padded_size_before_copy() {
    struct NearlyFullPool;
    impl HostSharedAllocation for NearlyFullPool {
        fn description(&self) -> SharedAllocationDescription {
            let mut desc = Allocation(0x8002).description();
            desc.allocation_size = VIRTIO_GPU_NATIVE_AHB_BUDGET - 256 - 128;
            desc
        }
        fn duplicate_descriptor(&self) -> anyhow::Result<SafeDescriptor> {
            anyhow::bail!("resident accounting fixture has no DMA descriptor")
        }
    }
    let (mut h, peer) = owned_fixture();
    let mut c = command(&h);
    h.frontend.virtio_gpu.inject_native_ahb_for_test(77, Box::new(NearlyFullPool));
    // The 128-byte minimum fits; the padded 256-byte allocation does not.
    // Keep each image valid even when the pool exceeds the per-image limit.
    peer.allocation_size_override.set(Some(256));
    c.flags = VIRTIO_GPU_NATIVE_BLT_PUBLISH.into();
    assert_eq!(
        send(&mut h, c, &[region()], 80, 0),
        VIRTIO_GPU_RESP_ERR_OUT_OF_MEMORY
    );
    assert_eq!(peer.blt_calls.get(), 0);
    assert!(peer.submissions.borrow().is_empty());
    assert!(!h.frontend.display_failed);
    peer.allocation_size_override.set(None);
    assert_eq!(
        send(&mut h, c, &[region()], 80, 0),
        VIRTIO_GPU_RESP_OK_NATIVE_BLT
    );
    assert_eq!(h.frontend.virtio_gpu.native_ahb_allocated_bytes(), VIRTIO_GPU_NATIVE_AHB_BUDGET);
}
#[test]
fn native_blt_failed_owned_publication_stops_intake() {
    let (mut h, peer) = owned_fixture();
    let mut c = command(&h);
    peer.fail_next_flip.set(true);
    c.flags = VIRTIO_GPU_NATIVE_BLT_PUBLISH.into();
    assert_eq!(
        send(&mut h, c, &[region()], 80, 0),
        VIRTIO_GPU_RESP_ERR_UNSPEC
    );
    assert!(h.frontend.display_failed);
    assert_eq!(peer.blt_calls.get(), 2);
    assert_eq!(h.frontend.virtio_gpu.native_ahb_allocated_bytes(), 384);
}
fn publish_owned(h: &mut Harness) {
    let mut c = command(h);
    c.flags = VIRTIO_GPU_NATIVE_BLT_PUBLISH.into();
    assert_eq!(send(h, c, &[region()], 80, 0), VIRTIO_GPU_RESP_OK_NATIVE_BLT);
}
#[test]
fn native_blt_reset_drain_requires_owned_release_then_clears_pool() {
    let (mut h, peer) = owned_fixture();
    publish_owned(&mut h);
    let (mut worker, _exit, _) = super::reset_worker::fixture_from(h);
    assert!(worker.state.pending_flip_fences.is_empty());
    assert!(worker.drain_display_for_reset(Duration::from_millis(25), |_| true).is_err());
    assert_eq!(worker.state.virtio_gpu.native_ahb_allocated_bytes(), 384);
    assert!(peer.geometry_resets.borrow().is_empty());
    peer.release_buffer(0).unwrap();
    worker.on_reset_with_drain_timeout(Duration::from_millis(100)).unwrap();
    assert!(!worker.state.native_blt_negotiated,
        "reset must revoke the previous guest's Blt negotiation");
    assert_eq!(worker.state.virtio_gpu.native_ahb_allocated_bytes(), 0);
    assert_eq!(peer.geometry_resets.borrow().as_slice(), &[(1, 2)]);
    assert!(worker.activation_resources.is_none());
}
#[test]
fn native_blt_reset_waits_for_delayed_owned_release() {
    let (mut h, peer) = owned_fixture();
    publish_owned(&mut h);
    let event = peer.submissions.borrow()[0].1.try_clone().unwrap();
    let (mut worker, _exit, _) = super::reset_worker::fixture_from(h);
    let fences = worker.fence_state.clone();
    let signal = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(30));
        let _guard = fences.lock();
        event.signal().unwrap();
    });
    let start = std::time::Instant::now();
    let result = worker.on_reset_with_drain_timeout(Duration::from_millis(250));
    let elapsed = start.elapsed();
    signal.join().unwrap();
    result.unwrap();
    assert!(elapsed >= Duration::from_millis(20));
    assert_eq!(worker.state.virtio_gpu.native_ahb_allocated_bytes(), 0);
}
#[test]
fn native_blt_reset_rejects_readable_non_sync_output_fence() {
    let (mut h, peer) = owned_fixture();
    peer.release_as_sync_file.set(true);
    publish_owned(&mut h);
    peer.release_buffer(0).unwrap();
    let (mut worker, _exit, _) = super::reset_worker::fixture_from(h);
    assert!(worker.on_reset_with_drain_timeout(Duration::from_millis(25)).is_err());
    assert_eq!(worker.state.virtio_gpu.native_ahb_allocated_bytes(), 384);
    assert!(worker.activation_resources.is_some());
    assert!(peer.geometry_resets.borrow().is_empty());
}
#[test]
fn native_blt_suspend_retains_owned_reader() {
    let (mut h, _) = owned_fixture();
    publish_owned(&mut h);
    let (mut worker, _exit, _) = super::reset_worker::fixture_from(h);
    assert!(worker.on_suspend().is_err());
    assert_eq!(worker.state.virtio_gpu.native_ahb_allocated_bytes(), 384);
    assert!(worker.activation_resources.is_some());
}
#[test]
fn native_blt_restore_rejects_owned_pool_before_changing_fences() {
    for released in [false, true] {
        let (mut legacy, _, _) = super::reset_worker::fixture_from(Harness::new(true));
        let snapshot = legacy.on_snapshot().unwrap();
        let (mut h, peer) = owned_fixture();
        publish_owned(&mut h);
        if released { peer.release_buffer(0).unwrap(); }
        let (mut worker, _exit, _) = super::reset_worker::fixture_from(h);
        worker.fence_state.lock().queue.completed_renderer.insert(VirtioGpuRing::Global, 999);
        assert!(worker.on_restore(snapshot).is_err());
        assert_eq!(worker.fence_state.lock().queue.completed_renderer.get(&VirtioGpuRing::Global), Some(&999));
        assert_eq!(worker.state.virtio_gpu.native_ahb_allocated_bytes(), 384);
        assert!(peer.geometry_resets.borrow().is_empty());
    }
}
#[test]
fn native_blt_legacy_refresh_cannot_republish_copyonly_primary() {
    let (mut h, peer) = owned_fixture();
    h.frontend.native_geometry_negotiated = true;
    h.frontend.virtio_gpu.set_scanout(
        virtio_gpu_rect { width: 8.into(), height: 4.into(), ..Default::default() },
        0, 29, Some(VirtioScanoutBlobData {
            width: 8, height: 4, drm_format: gpu_display::DRM_FORMAT_ABGR8888.into(),
            strides: [32, 0, 0, 0], offsets: [0; 4],
        })).unwrap();
    let mut direct = virtio_gpu_present_native_geometry {
        operation: virtio_gpu_native_ahb_operation {
            hdr: hdr(VIRTIO_GPU_CMD_PRESENT_NATIVE_GEOMETRY), resource_id: 29.into(),
            ..Default::default()
        }, reset_generation: 1.into(), mode_generation: 1.into(),
    };
    h.submit(direct.as_bytes(), 0, true);
    assert_eq!(h.response(0, direct.as_bytes().len()).type_.to_native(), VIRTIO_GPU_RESP_OK_NATIVE_AHB_OPERATION);
    let sequence = h.frontend.native_ahb_release.sequences[&29];
    peer.release_buffer(0).unwrap();
    h.frontend.complete_flip_fences(&h.queues[0], &h.queues[1], &WaitContext::new().unwrap()).unwrap();
    publish_owned(&mut h);
    let mut c = command(&h);
    assert_eq!(send(&mut h, c, &[region()], 80, 0), VIRTIO_GPU_RESP_OK_NATIVE_BLT);
    let calls = peer.blt_calls.get();
    peer.release_buffer(1).unwrap();
    peer.generation.set(2);
    direct.operation.hdr.type_ = VIRTIO_GPU_CMD_REFRESH_NATIVE_GEOMETRY.into();
    direct.operation.sequence = sequence.into();
    h.submit(direct.as_bytes(), 0, true);
    assert_eq!(h.response(0, direct.as_bytes().len()).type_.to_native(), VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
    assert_eq!(peer.submissions.borrow().len(), 2, "legacy refresh must not publish primary");
    c.flags = VIRTIO_GPU_NATIVE_BLT_REFRESH_ONLY.into(); c.source = Default::default(); c.region_count = 0.into();
    assert_eq!(send(&mut h, c, &[], 80, 0), VIRTIO_GPU_RESP_OK_NATIVE_BLT);
    assert_eq!(peer.imports.borrow().last().unwrap().1, 0x100001);
    assert_eq!(peer.blt_calls.get(), calls);
}
#[test]
fn native_blt_owned_snapshot_cannot_follow_recreated_primary_identity() {
    let (mut h, peer) = owned_fixture();
    publish_owned(&mut h);
    let mut old = command(&h);
    old.flags = VIRTIO_GPU_NATIVE_BLT_REFRESH_ONLY.into(); old.source = Default::default(); old.region_count = 0.into();
    h.frontend.virtio_gpu.unref_resource(&h.mem, 29).unwrap();
    assert_eq!(h.frontend.virtio_gpu.native_ahb_allocated_bytes(), 256);
    h.frontend.virtio_gpu.inject_geometry_resource_for_test(29, Box::new(Allocation(29)));
    h.frontend.virtio_gpu.bind_native_geometry(29, 0, 1, 1).unwrap();
    let mut new = old; new.destination = endpoint(&h, 29);
    assert_ne!(new.destination.incarnation, old.destination.incarnation);
    for c in [old, new] {
        assert_eq!(send(&mut h, c, &[], 80, 0), VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
    }
    assert_eq!(peer.submissions.borrow().len(), 1);
    assert_eq!(h.frontend.virtio_gpu.native_ahb_allocated_bytes(), 384);
}
#[test]
fn native_blt_accepted_direct_flip_retires_owned_current() {
    let (mut h, peer) = owned_fixture();
    publish_owned(&mut h);
    h.frontend.virtio_gpu.flush_resource(29).unwrap();
    assert_eq!(peer.submissions.borrow().len(), 2);
    let mut c = command(&h);
    c.flags = VIRTIO_GPU_NATIVE_BLT_REFRESH_ONLY.into(); c.source = Default::default(); c.region_count = 0.into();
    assert_eq!(send(&mut h, c, &[], 80, 0), VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
    assert_eq!(h.frontend.virtio_gpu.native_ahb_allocated_bytes(), 384);
}
fn send(
    h: &mut Harness,
    c: virtio_gpu_native_blt,
    regions: &[VirtioGpuNativeBltRegion],
    capacity: u32,
    queue: usize,
) -> u32 {
    let mut b = c.as_bytes().to_vec();
    for r in regions {
        b.extend_from_slice(r.as_bytes());
    }
    h.submit_capacity(&b, queue, capacity);
    h.response(queue, b.len()).type_.to_native()
}
#[test]
fn native_blt_wire_rejects_whole_invalid_request_before_gpu() {
    for case in 0..17 {
        let (mut h, peer) = fixture();
        let mut c = command(&h);
        let mut clips = vec![region()];
        let mut capacity = 80;
        let mut queue = 0;
        match case {
            0 => h.frontend.native_blt_negotiated = false,
            1 => c.hdr.flags = 1.into(),
            2 => c.hdr.padding = [1, 0, 0],
            3 => c.cookie = 0.into(),
            4 => c.reset_generation = 2.into(),
            5 => c.source.incarnation = 0.into(),
            6 => c.destination.buffer_id = 42.into(),
            7 => c.source.reserved = 1.into(),
            8 => c.source.width = 9.into(),
            9 => c.destination = c.source,
            10 => {
                c.region_count = 2.into();
                clips.push(VirtioGpuNativeBltRegion {
                    width: u32::MAX.into(),
                    ..region()
                });
            }
            11 => c.region_count = 0.into(),
            12 => c.region_count = 257.into(),
            13 => capacity = 79,
            14 => queue = 1,
            15 => c.flags = VIRTIO_GPU_NATIVE_BLT_PUBLISH.into(),
            _ => clips.push(region()),
        };
        assert_ne!(
            send(&mut h, c, &clips, capacity, queue),
            VIRTIO_GPU_RESP_OK_NATIVE_BLT,
            "case {case}"
        );
        assert_eq!(peer.blt_calls.get(), 0, "case {case}");
    }
}
#[test]
fn native_blt_copy_completes_exact_response_without_display() {
    let (mut h, peer) = fixture();
    let c = command(&h);
    assert_eq!(
        send(&mut h, c, &[region()], 80, 0),
        VIRTIO_GPU_RESP_OK_NATIVE_BLT
    );
    assert_eq!(peer.blt_calls.get(), 1);
    assert!(peer.submissions.borrow().is_empty());
    let address = 0x1800 + 144 + 24;
    let r: virtio_gpu_native_blt_response =
        h.mem.read_obj_from_addr(GuestAddress(address)).unwrap();
    assert_eq!(r.cookie.to_native(), 71);
    assert_eq!(r.copied_bytes.to_native(), 32);
    assert_eq!(r.uploaded_bytes.to_native(), 0);
    assert_eq!(r.readback_bytes.to_native(), 0);
}
#[test]
fn native_blt_gpu_error_quarantines_both_endpoints() {
    let (mut h, peer) = fixture();
    let c = command(&h);
    peer.fail_blt.set(true);
    assert_eq!(
        send(&mut h, c, &[region()], 80, 0),
        VIRTIO_GPU_RESP_ERR_UNSPEC
    );
    assert_eq!(peer.blt_calls.get(), 1);
    peer.fail_blt.set(false);
    let used = h.used(0);
    let _ = send(&mut h, c, &[region()], 80, 0);
    assert_eq!(h.used(0), used);
    assert_eq!(peer.blt_calls.get(), 1);
    assert!(
        h.frontend.native_blt.poisoned.contains(&17)
            && h.frontend.native_blt.poisoned.contains(&29)
    );
}
#[test]
fn native_blt_query_binds_resource_incarnation() {
    let (mut h, _) = fixture();
    let q = virtio_gpu_query_native_blt {
        hdr: hdr(VIRTIO_GPU_CMD_QUERY_NATIVE_BLT),
        resource_id: 17.into(),
        cookie: 93.into(),
        ..Default::default()
    };
    h.submit(q.as_bytes(), 0, true);
    assert_eq!(
        h.response(0, 40).type_.to_native(),
        VIRTIO_GPU_RESP_OK_NATIVE_BLT_IDENTITY
    );
    let r: virtio_gpu_native_blt_identity =
        h.mem.read_obj_from_addr(GuestAddress(0x1800 + 40)).unwrap();
    assert_eq!(r.cookie.to_native(), 93);
    assert_ne!(r.incarnation.to_native(), 0);
    assert_eq!(r.buffer_id.to_native(), 17);
    assert_eq!(r.reset_generation.to_native(), 1);
}

#[test]
fn native_blt_failed_batch_publishes_error_and_stops_renderer_intake() {
    let (mut h, peer) = fixture();
    let render = virtio_gpu_cmd_submit {
        hdr: hdr(VIRTIO_GPU_CMD_SUBMIT_3D),
        size: 4.into(),
        num_in_fences: 0.into(),
    };
    let mut render_bytes = render.as_bytes().to_vec();
    render_bytes.extend_from_slice(&[0; 4]);
    // This valid descriptor reaches the actual backend method (the 2D test
    // renderer rejects native payloads). Count method entry, not GPU success.
    h.submit(&render_bytes, 0, true);
    assert_eq!(h.frontend.virtio_gpu.native_blt_test_submits, 1);
    let c = command(&h);
    let mut blt = c.as_bytes().to_vec();
    blt.extend_from_slice(region().as_bytes());
    peer.fail_blt.set(true);
    h.enqueue_capacity(&blt, 0, 80);
    h.enqueue_capacity(&render_bytes, 0, 80);
    h.frontend.process_queue(&h.mem, &h.queues[0]);
    let reply: virtio_gpu_ctrl_hdr = h
        .mem
        .read_obj_from_addr(GuestAddress(0x1c00 + blt.len() as u64))
        .unwrap();
    assert_eq!(reply.type_.to_native(), VIRTIO_GPU_RESP_ERR_UNSPEC);
    assert_eq!(
        h.frontend.virtio_gpu.native_blt_test_submits, 1,
        "renderer intake after uncertain Blt"
    );
    assert_eq!(
        h.used(0),
        2,
        "only the failing Blt may return after the control"
    );
    h.frontend.process_queue(&h.mem, &h.queues[0]);
    assert_eq!(h.frontend.virtio_gpu.native_blt_test_submits, 1);
}

#[test]
fn native_blt_android_reader_blocks_source_and_destination() {
    for id in [17, 29] {
        let (mut h, peer) = fixture();
        let c = command(&h);
        let event = Event::new().unwrap();
        h.frontend.pending_flip_fences.push(PendingFlipFence {
            resource_id: id,
            fence_id: 31,
            ticket: None,
            deadline: None,
            registered: false,
            fence: FlipCompletionFence::AndroidReleaseEvent(
                SafeDescriptor::try_from(&event as &dyn AsRawDescriptor).unwrap(),
            ),
        });
        assert_eq!(
            send(&mut h, c, &[region()], 80, 0),
            VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER
        );
        assert_eq!(peer.blt_calls.get(), 0);
        // Real event completion is observed by the production release poller.
        event.signal().unwrap();
        h.frontend
            .complete_flip_fences(&h.queues[0], &h.queues[1], &WaitContext::new().unwrap())
            .unwrap();
        assert_eq!(
            send(&mut h, c, &[region()], 80, 0),
            VIRTIO_GPU_RESP_OK_NATIVE_BLT
        );
    }
}
#[test]
fn native_blt_stale_resource_incarnation_is_not_a_new_allocation() {
    let (mut h, peer) = fixture();
    let old = command(&h);
    h.frontend.virtio_gpu.unref_resource(&h.mem, 17).unwrap();
    h.frontend
        .virtio_gpu
        .inject_geometry_resource_for_test(17, Box::new(Allocation(17)));
    assert_ne!(endpoint(&h, 17).incarnation, old.source.incarnation);
    assert_eq!(
        send(&mut h, old, &[region()], 80, 0),
        VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER
    );
    assert_eq!(peer.blt_calls.get(), 0);
    let current = command(&h);
    assert_eq!(
        send(&mut h, current, &[region()], 80, 0),
        VIRTIO_GPU_RESP_OK_NATIVE_BLT
    );
}
