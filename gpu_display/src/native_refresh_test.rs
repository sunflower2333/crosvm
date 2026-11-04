// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license.
//! Deterministic consumer for device protocol tests; only built with the
//! nonfunctional android_display_stub feature. No hardware success is implied.
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use base::{AsRawDescriptor, Event, RawDescriptor, SafeDescriptor};
use super::*;

#[derive(Default)]
pub struct Consumer {
    pub blt_pairs: RefCell<Vec<(u64,u64)>>,
    pub allocated: Cell<u64>,
    pub allocation_size_override: Cell<Option<u64>>,
    pub fail_allocation: Cell<bool>,
    pub fail_import: Cell<bool>,
    pub strict_buffers: Cell<bool>,
    pub release_as_sync_file: Cell<bool>,
    pub held_buffers: RefCell<Vec<(u64, usize)>>,
    pub imports: RefCell<Vec<(u32,u64)>>,
    pub blt_uploads: RefCell<Vec<(NativeBltCpuLayout, Vec<u8>, Vec<NativeBltRegion>)>>,
    pub blt_readbacks: Cell<u64>,
    pub blt_calls: Cell<u64>,
    pub fail_blt: Cell<bool>,
    pub timing: Cell<Option<DisplayTimingSample>>,
    pub timing_queries: Cell<usize>,
    pub generation: Cell<u64>,
    pub presented: Cell<u64>,
    pub expected: Cell<u64>,
    pub race_on_submit: Cell<bool>,
    pub fail_next_flip: Cell<bool>,
    pub detached: Cell<bool>,
    pub attempts: Cell<usize>,
    pub on_submit: RefCell<Option<Box<dyn FnOnce()>>>,
    pub geometry: Cell<Option<ScanoutGeometry>>,
    pub geometry_resets: RefCell<Vec<(u64, u64)>>,
    pub fail_geometry_reset: Cell<bool>,
    pub surfaces_created: Cell<usize>,
    pub profile: Cell<Option<ScanoutProfile>>,
    pub configured_profile: Cell<Option<ScanoutProfile>>,
    pub fail_geometry: Cell<bool>,
    pub geometry_submissions: RefCell<Vec<ScanoutGeometry>>,
    pub submissions: RefCell<Vec<(u64, Event)>>,
}
impl Consumer {
    pub fn release_buffer(&self, index: usize) -> anyhow::Result<()> {
        // The consumer retires its identity before notifying the host. The host
        // consumes that eventfd, so its later readability is not consumer state.
        self.held_buffers.borrow_mut().retain(|(_, held)| *held != index);
        self.submissions.borrow()[index].1.signal()?;
        Ok(())
    }
}
struct Display { event: Event, consumer: Rc<Consumer> }
struct Surface { consumer: Rc<Consumer>, fence: Option<FlipCompletionFence> }
struct TestAllocation(SharedAllocationDescription);
impl HostSharedAllocation for TestAllocation {
    fn description(&self) -> SharedAllocationDescription { self.0 }
    fn duplicate_descriptor(&self) -> anyhow::Result<SafeDescriptor> { anyhow::bail!("test allocation") }
}
impl AsRawDescriptor for Display {
    fn as_raw_descriptor(&self) -> RawDescriptor { self.event.as_raw_descriptor() }
}
impl SysDisplayT for Display {}
impl DisplayT for Display {
    fn allocate_host_shared_allocation(&mut self, width:u32, height:u32, fourcc:u32)
        -> anyhow::Result<Box<dyn HostSharedAllocation>> {
        let count=self.consumer.allocated.get()+1;
        self.consumer.allocated.set(count);
        anyhow::ensure!(!self.consumer.fail_allocation.replace(false), "allocation refused");
        Ok(Box::new(TestAllocation(SharedAllocationDescription {
            version:1,size:64,buffer_id:0x100000+count,
            allocation_size:self.consumer.allocation_size_override.get().unwrap_or(u64::from(width)*u64::from(height)*4),
            plane_stride:width*4,width,height,fourcc,plane_count:1,layout_flags:1,..Default::default()
        })))
    }
    fn native_blt_upload(&mut self, destination: &dyn HostSharedAllocation, source: &[u8],
        layout: NativeBltCpuLayout, regions: &[NativeBltRegion]) -> anyhow::Result<u64> {
        self.consumer.blt_uploads.borrow_mut().push((layout, source.to_vec(), regions.to_vec()));
        anyhow::ensure!(!self.consumer.fail_blt.get(), "injected upload failure");
        let d = destination.description();
        native_blt_bytes(layout.width, layout.height, d.width, d.height, regions)
            .ok_or_else(|| anyhow::anyhow!("bad upload clip"))
    }
    fn native_blt_readback(&mut self, source: &dyn HostSharedAllocation, destination: &mut [u8],
        layout: NativeBltCpuLayout, regions: &[NativeBltRegion]) -> anyhow::Result<u64> {
        self.consumer.blt_readbacks.set(self.consumer.blt_readbacks.get()+1);
        anyhow::ensure!(!self.consumer.fail_blt.get(), "injected readback failure");
        let s = source.description();
        let bytes = native_blt_bytes(s.width, s.height, layout.width, layout.height, regions)
            .ok_or_else(|| anyhow::anyhow!("bad readback clip"))?;
        for r in regions {
            for y in 0..r.height { for x in 0..r.width {
                let offset = ((r.destination_y+y)*layout.stride+(r.destination_x+x)*4) as usize;
                destination[offset..offset+4].copy_from_slice(&[
                    0x40+(r.source_x+x) as u8, 0x70+(r.source_y+y) as u8, 0xb3, 0xff]);
            }}
        }
        Ok(bytes)
    }
    fn native_blt_copy(&mut self, source:&dyn HostSharedAllocation, destination:&dyn HostSharedAllocation,
        regions:&[NativeBltRegion])->anyhow::Result<u64> {
        self.consumer.blt_calls.set(self.consumer.blt_calls.get()+1);
        anyhow::ensure!(!self.consumer.fail_blt.get(),"injected GPU copy failure");
        let s=source.description();let d=destination.description();
        self.consumer.blt_pairs.borrow_mut().push((s.buffer_id,d.buffer_id));
        native_blt_bytes(s.width,s.height,d.width,d.height,regions).ok_or_else(||anyhow::anyhow!("bad clip"))
    }

    fn reset_native_scanout(&self, old_reset: u64, new_reset: u64) -> anyhow::Result<()> {
        anyhow::ensure!(!self.consumer.fail_geometry_reset.get(), "bridge reset refused");
        self.consumer.geometry_resets.borrow_mut().push((old_reset, new_reset));
        self.consumer.geometry.set(None);
        self.consumer.configured_profile.set(None);
        Ok(())
    }
    fn display_timing(&self) -> Option<DisplayTimingSample> {
        self.consumer.timing_queries.set(self.consumer.timing_queries.get() + 1);
        self.consumer.timing.get()
    }
    fn has_consumer(&self) -> bool { self.consumer.generation.get() != 0 && !self.consumer.detached.get() }
    fn consumer_generation(&self) -> u64 { self.consumer.generation.get() }
    fn native_refresh_generation(&self) -> u64 {
        let generation = self.consumer.generation.get();
        if self.has_consumer() && generation != self.consumer.presented.get() { generation } else { 0 }
    }
    fn set_native_refresh_generation(&self, generation: u64) { self.consumer.expected.set(generation); }
    fn create_surface(&mut self, _: Option<u32>, _: u32, _: Option<u32>, _: &DisplayParameters,
                      _: SurfaceType) -> GpuDisplayResult<Box<dyn GpuDisplaySurface>> {
        self.consumer.surfaces_created.set(self.consumer.surfaces_created.get() + 1);
        Ok(Box::new(Surface { consumer: self.consumer.clone(), fence: None }))
    }
    fn import_resource(&mut self, id: u32, _: u32, resource: DisplayExternalResourceImport) -> anyhow::Result<()> {
        anyhow::ensure!(!self.consumer.fail_import.replace(false), "import refused");
        if let DisplayExternalResourceImport::SharedAhb {allocation} = resource {
            self.consumer.imports.borrow_mut().push((id,allocation.description().buffer_id));
        } else {panic!("expected shared AHB");}
        Ok(())
    }
}
impl GpuDisplaySurface for Surface {
    fn query_scanout_profile(&mut self) -> anyhow::Result<ScanoutProfile> {
        if self.consumer.detached.get() { return Ok(ScanoutProfile::empty()); }
        Ok(self.consumer.profile.get().unwrap_or_else(ScanoutProfile::empty))
    }
    fn configure_scanout_profile(&mut self, geometry: ScanoutGeometry, endpoint: u64, profile: u64) -> anyhow::Result<()> {
        let current = self.query_scanout_profile()?;
        anyhow::ensure!(current.matches(geometry, endpoint, profile), "stale profile");
        self.configure_scanout_geometry(geometry)?;
        self.consumer.configured_profile.set(Some(current));
        Ok(())
    }
    fn configure_scanout_geometry(&mut self, geometry: ScanoutGeometry) -> anyhow::Result<()> {
        anyhow::ensure!(self.consumer.generation.get() != 0 && !self.consumer.detached.get()
            && !self.consumer.fail_geometry.replace(false), "geometry parent unavailable");
        self.consumer.geometry.set(Some(geometry));
        self.consumer.presented.set(0);
        Ok(())
    }
    fn flip_to_geometry(&mut self, import: u32, geometry: ScanoutGeometry) -> anyhow::Result<Waitable> {
        if let Some(configured) = self.consumer.configured_profile.get() {
            anyhow::ensure!(self.query_scanout_profile()? == configured, "revoked profile");
        }
        anyhow::ensure!(self.consumer.geometry.get() == Some(geometry), "stale geometry");
        let result = self.flip_to(import, None, None, None)?;
        self.consumer.geometry_submissions.borrow_mut().push(geometry);
        Ok(result)
    }
    fn flip_to(&mut self, import: u32, _: Option<SemaphoreTimepoint>, _: Option<SemaphoreTimepoint>,
               _: Option<FlipToExtraInfo>) -> anyhow::Result<Waitable> {
        if self.consumer.race_on_submit.replace(false) {
            self.consumer.generation.set(self.consumer.generation.get() + 1);
        }
        let generation = self.consumer.generation.get();
        let expected = self.consumer.expected.get();
        self.consumer.attempts.set(self.consumer.attempts.get() + 1);
        anyhow::ensure!(generation != 0 && !self.consumer.detached.get() &&
                        (expected == 0 || generation == expected), "stale attachment");
        anyhow::ensure!(!self.consumer.fail_next_flip.replace(false), "transient flip refusal");
        if self.consumer.strict_buffers.get() {
            let imports = self.consumer.imports.borrow();
            let buffer = imports.iter().rev().find(|(id, _)| *id == import)
                .ok_or_else(|| anyhow::anyhow!("missing import"))?.1;
            let submissions = self.consumer.submissions.borrow();
            let mut held = self.consumer.held_buffers.borrow_mut();
            anyhow::ensure!(!held.iter().any(|(id, _)| *id == buffer), "AHB still held by consumer");
            held.push((buffer, submissions.len()));
        }
        let event = Event::new()?;
        let fd = SafeDescriptor::try_from(&event as &dyn AsRawDescriptor)?;
        self.fence = Some(if self.consumer.release_as_sync_file.get() {
            FlipCompletionFence::SyncFile(fd)
        } else {
            FlipCompletionFence::AndroidReleaseEvent(fd)
        });
        self.consumer.submissions.borrow_mut().push((generation, event));
        self.consumer.presented.set(generation);
        if let Some(callback) = self.consumer.on_submit.borrow_mut().take() { callback(); }
        Ok(Waitable::signaled())
    }
    fn take_flip_completion_fence(&mut self) -> Option<FlipCompletionFence> { self.fence.take() }
}
pub fn open(consumer: Rc<Consumer>) -> GpuDisplayResult<GpuDisplay> {
    let display = Display { event: Event::new()?, consumer };
    let wait_ctx = WaitContext::new()?;
    wait_ctx.add(&display, DisplayEventToken::Display)?;
    Ok(GpuDisplay { inner: Box::new(display), next_id: 1, event_devices: Default::default(),
                    surfaces: Default::default(), dmabuf_import_capped: false, wait_ctx })
}
