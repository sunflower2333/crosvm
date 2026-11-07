// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license.
//! Actual resource ownership and ordinary SG transfer. Ordinary bytes are never
//! treated as an alias of a native AHB. The worker holds all borrowed mappings
//! until the synchronous GPU operation and destination cache update finish.
use super::*;
use crate::virtio::gpu::native_blt_wire::VirtioGpuNativeBltEndpoint as Endpoint;

const MAX_ORDINARY_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug)]
pub(crate) struct NativeBltCompletion {
    pub copied: u64,
    pub uploaded: u64,
    pub readback: u64,
}

/// The list has already been bounded to the declared layout size and validated
/// in full. Retain the actual slices, including split pixels/rows, through GPU
/// completion; do not look up guest addresses only after the image was written.
struct Backing<'a> {
    slices: Vec<VolatileSlice<'a>>,
    size: usize,
}
impl<'a> Backing<'a> {
    fn new(
        mem: &'a GuestMemory,
        resource: &VirtioGpuResource,
        size: u64,
    ) -> Result<Self, GpuResponse> {
        if size == 0 || size > MAX_ORDINARY_BYTES {
            return Err(ErrInvalidParameter);
        }
        let ranges = guest_blob_iovecs(
            resource
                .backing_iovecs
                .as_deref()
                .ok_or(ErrInvalidParameter)?,
            size,
        )?;
        let mut slices = Vec::with_capacity(ranges.len());
        let mut addresses = Vec::with_capacity(ranges.len());
        for (address, length) in ranges {
            let slice = mem
                .get_slice_at_addr(address, length)
                .map_err(|_| ErrInvalidParameter)?;
            let start = slice.as_mut_ptr() as usize;
            let end = start.checked_add(length).ok_or(ErrInvalidParameter)?;
            addresses.push((start, end));
            slices.push(slice);
        }
        addresses.sort_unstable();
        if addresses.windows(2).any(|p| p[0].1 > p[1].0) {
            return Err(ErrInvalidParameter);
        }
        Ok(Self {
            slices,
            size: size as usize,
        })
    }

    fn snapshot(&self) -> Result<Vec<u8>, GpuResponse> {
        let mut data = Vec::new();
        data.try_reserve_exact(self.size)
            .map_err(|_| ErrOutOfMemory)?;
        data.resize(self.size, 0);
        let mut offset = 0;
        for slice in &self.slices {
            slice.copy_to(&mut data[offset..offset + slice.size()]);
            offset += slice.size();
        }
        Ok(data)
    }

    fn write_clips(&self, data: &[u8], stride: u32, regions: &[NativeBltRegion]) {
        // All region endpoints and every slice were checked before GPU work.
        // Publish final bytes only for clipped rows, preserving concurrent
        // unrelated bytes as well as padding. Overlapping clips use final data.
        for r in regions {
            for y in r.destination_y..r.destination_y + r.height {
                let start =
                    (u64::from(y) * u64::from(stride) + u64::from(r.destination_x) * 4) as usize;
                let end = start + r.width as usize * 4;
                let mut base = 0;
                for slice in &self.slices {
                    let lo = start.max(base);
                    let hi = end.min(base + slice.size());
                    if lo < hi {
                        slice
                            .sub_slice(lo - base, hi - lo)
                            .expect("prevalidated SG slice")
                            .copy_from(&data[lo..hi]);
                    }
                    base += slice.size();
                    if base >= end {
                        break;
                    }
                }
            }
        }
        std::sync::atomic::fence(Ordering::SeqCst);
    }
}

impl VirtioGpu {
    pub(in crate::virtio::gpu) fn has_native_blt_output_state(&self) -> bool {
        self.native_blt_outputs.has_state()
    }
    pub(in crate::virtio::gpu) fn native_blt_output_is_current(&self, resource: u32) -> bool {
        self.resources.get(&resource).is_some_and(|r|
            self.native_blt_outputs.is_current(resource, r.native_blt_incarnation))
    }
    pub(in crate::virtio::gpu) fn poll_native_blt_outputs(&mut self) -> anyhow::Result<bool> {
        self.native_blt_outputs.pending()
            .map_err(|e| anyhow::anyhow!("owned Blt output completion failed: {e:?}"))
    }
    pub(in crate::virtio::gpu) fn native_blt_outputs_poisoned(&self) -> bool {
        self.native_blt_outputs.poisoned
    }

    pub(in crate::virtio::gpu) fn prepare_native_blt_output(
        &mut self,
        mem: &GuestMemory,
        destination: Endpoint,
        refresh: bool,
    ) -> Result<super::native_blt_output::OutputTicket, GpuResponse> {
        let resource = self.native_blt_endpoint(destination)?;
        if resource.shared_ahb.is_none() {
            Backing::new(mem, resource, destination.size.into())?;
        }
        let scanout = self.scanouts.get(&0).ok_or(ErrInvalidScanoutId)?;
        // Normal Windows scanout uses the existing identity presenter. It does
        // not negotiate/configure the separate rotation diagnostic. Keep mode
        // zero as a bootstrap ticket, never pass it to the geometry API.
        let geometry = scanout.native_geometry.unwrap_or_else(||
            ScanoutGeometry::identity(scanout.width, scanout.height,
                self.native_geometry_reset_generation, 0));
        let surface = scanout.surface_id.ok_or(ErrInvalidScanoutId)?;
        if !self.scanout_profile_current(scanout)
            || (scanout.native_geometry.is_none() && scanout.native_profile.is_some())
            || destination.width.to_native() != geometry.storage_width
            || destination.height.to_native() != geometry.storage_height
            || (resource.shared_ahb.is_some() && resource.native_geometry != scanout.native_geometry)
        {
            return Err(ErrInvalidParameter);
        }
        let mut display = self.display.borrow_mut();
        let generation = display.consumer_generation();
        if generation == 0 || !display.has_consumer() {
            return Err(ErrInvalidParameter);
        }
        if refresh {
            self.native_blt_outputs.current_ticket(
                &display,
                destination.resource_id.into(),
                destination.incarnation.into(),
                surface,
                geometry,
                generation,
            )
        } else {
            let guest_bytes = self
                .native_ahb_allocated_bytes()
                .saturating_sub(self.native_blt_outputs.bytes());
            // XR24 remains the authenticated ordinary resource format. Its
            // separately allocated output image is AR24; upload supplies the
            // opaque alpha that the ordinary X byte does not represent.
            let output_fourcc = match destination.fourcc.to_native() {
                value if value == u32::from_le_bytes(*b"XR24") => u32::from_le_bytes(*b"AR24"),
                value => value,
            };
            self.native_blt_outputs.prepare(
                &mut display,
                geometry,
                surface,
                generation,
                output_fourcc,
                guest_bytes,
            )
        }
    }

    pub(in crate::virtio::gpu) fn publish_native_blt_output(
        &mut self,
        mem: &GuestMemory,
        destination: Endpoint,
        ticket: super::native_blt_output::OutputTicket,
        refresh: bool,
    ) -> Result<u64, GpuResponse> {
        let mut uploaded = 0;
        if !refresh {
            // Validate/allocate every ordinary snapshot before marking an output
            // as possibly owned by the GPU. Source borrows remain valid until
            // this synchronous copy and the subsequent publication return.
            let resource = self.native_blt_endpoint(destination)?;
            let snapshot = if resource.shared_ahb.is_none() {
                Some(Backing::new(mem, resource, destination.size.into())?.snapshot()?)
            } else {
                None
            };
            let region = NativeBltRegion {
                width: destination.width.into(),
                height: destination.height.into(),
                ..Default::default()
            };
            let expected = u64::from(region.width) * u64::from(region.height) * 4;
            // Split the resource and pool field borrows explicitly.
            let resource = self
                .resources
                .get(&destination.resource_id.to_native())
                .ok_or(ErrInvalidResourceId)?;
            let output = self.native_blt_outputs.begin_copy(ticket)?;
            let completed = if let Some(native) = resource.shared_ahb.as_deref() {
                self.display
                    .borrow_mut()
                    .native_blt_copy(native, output, &[region])
            } else {
                uploaded = expected;
                self.display.borrow_mut().native_blt_upload(
                    output,
                    snapshot.as_ref().unwrap(),
                    destination.layout(),
                    &[region],
                )
            }
            .map_err(|e| {
                base::error!("owned Blt snapshot failed: {e:#}");
                ErrUnspec
            })?;
            if completed != expected {
                return Err(ErrUnspec);
            }
            self.native_blt_outputs.copy_completed(ticket, expected);
        }
        self.native_blt_outputs.publish(
            &mut self.display.borrow_mut(),
            ticket,
            destination.resource_id.into(),
            destination.incarnation.into(),
        )?;
        let scanout = self.scanouts.get_mut(&0).ok_or(ErrInvalidScanoutId)?;
        scanout.resource_id = NonZeroU32::new(destination.resource_id.into());
        self.last_guest_present = Some(Instant::now());
        self.external_had_display = false;
        Ok(uploaded)
    }

    pub(in crate::virtio::gpu) fn native_blt_reset_generation(&self) -> u64 {
        self.native_geometry_reset_generation
    }

    pub(in crate::virtio::gpu) fn native_blt_identity(&self, id: u32) -> Option<(u64, u64, u32)> {
        let r = self.resources.get(&id)?;
        if let Some(a) = r.shared_ahb.as_ref() {
            return Some((r.native_blt_incarnation, a.description().buffer_id, 2));
        }
        if r.native_blt_ordinary && r.backing_iovecs.is_some() {
            return Some((r.native_blt_incarnation, 0, 1));
        }
        None
    }

    fn native_blt_endpoint(&self, e: Endpoint) -> Result<&VirtioGpuResource, GpuResponse> {
        let r = self
            .resources
            .get(&e.resource_id.to_native())
            .ok_or(ErrInvalidResourceId)?;
        if e.reserved.to_native() != 0 || e.incarnation.to_native() != r.native_blt_incarnation {
            return Err(ErrInvalidParameter);
        }
        if let Some(a) = r.shared_ahb.as_deref() {
            let d = a.description();
            if !d.valid(d.width, d.height)
                || e.buffer_id.to_native() != d.buffer_id
                || e.width.to_native() != d.width
                || e.height.to_native() != d.height
                || e.stride.to_native() != d.plane_stride
                || e.fourcc.to_native() != d.fourcc
                || e.size.to_native() != d.allocation_size
            {
                return Err(ErrInvalidParameter);
            }
        } else if !r.native_blt_ordinary
            || r.backing_iovecs.is_none()
            || e.buffer_id.to_native() != 0
            || e.width.to_native() != r.width
            || e.height.to_native() != r.height
            || Some(e.fourcc.to_native()) != r.format_fourcc
            || e.size.to_native() > MAX_ORDINARY_BYTES
            || !e.layout().valid(e.size.to_native() as usize)
        {
            return Err(ErrInvalidParameter);
        }
        Ok(r)
    }

    pub(in crate::virtio::gpu) fn validate_native_blt(
        &self,
        mem: &GuestMemory,
        source: Endpoint,
        destination: Endpoint,
        regions: &[NativeBltRegion],
    ) -> Result<u64, GpuResponse> {
        let s = self.native_blt_endpoint(source)?;
        let d = self.native_blt_endpoint(destination)?;
        if source.resource_id == destination.resource_id {
            return Err(ErrInvalidParameter);
        }
        match (s.shared_ahb.as_deref(), d.shared_ahb.as_deref()) {
            (Some(a), Some(b)) if a.description().buffer_id != b.description().buffer_id => (),
            (None, Some(_)) => {
                Backing::new(mem, s, source.size.into())?;
            }
            (Some(_), None) => {
                Backing::new(mem, d, destination.size.into())?;
            }
            _ => return Err(ErrInvalidParameter),
        }
        let bytes = native_blt_bytes(
            source.width.into(),
            source.height.into(),
            destination.width.into(),
            destination.height.into(),
            regions,
        )
        .ok_or(ErrInvalidParameter)?;
        if (s.shared_ahb.is_none() || d.shared_ahb.is_none()) && bytes > MAX_ORDINARY_BYTES {
            return Err(ErrInvalidParameter);
        }
        Ok(bytes)
    }

    pub(in crate::virtio::gpu) fn execute_native_blt(
        &mut self,
        mem: &GuestMemory,
        source: Endpoint,
        destination: Endpoint,
        regions: &[NativeBltRegion],
    ) -> Result<NativeBltCompletion, GpuResponse> {
        let expected = self.validate_native_blt(mem, source, destination, regions)?;
        let s = self.native_blt_endpoint(source)?;
        let d = self.native_blt_endpoint(destination)?;
        let failure = |e: anyhow::Error| {
            base::error!("native Blt completion failed: {e:#}");
            ErrUnspec
        };
        let mut done = NativeBltCompletion {
            copied: expected,
            uploaded: 0,
            readback: 0,
        };
        match (s.shared_ahb.as_deref(), d.shared_ahb.as_deref()) {
            (Some(a), Some(b)) => {
                done.copied = self
                    .display
                    .borrow_mut()
                    .native_blt_copy(a, b, regions)
                    .map_err(failure)?;
            }
            (None, Some(b)) => {
                let backing = Backing::new(mem, s, source.size.into())?;
                let data = backing.snapshot()?;
                let bytes = self
                    .display
                    .borrow_mut()
                    .native_blt_upload(b, &data, source.layout(), regions)
                    .map_err(failure)?;
                if bytes != expected {
                    return Err(ErrUnspec);
                }
                done.uploaded = bytes;
            }
            (Some(a), None) => {
                let backing = Backing::new(mem, d, destination.size.into())?;
                let mut data = backing.snapshot()?;
                let bytes = self
                    .display
                    .borrow_mut()
                    .native_blt_readback(a, &mut data, destination.layout(), regions)
                    .map_err(failure)?;
                if bytes != expected {
                    return Err(ErrUnspec);
                }
                backing.write_clips(&data, destination.stride.into(), regions);
                // Keep the renderer's ordinary texture coherent even for
                // CopyOnly. Full upload avoids partial classic transfer bugs;
                // no scanout/flush/flip is invoked by this synchronization.
                self.force_ctx_0();
                self.transfer_write(
                    0,
                    destination.resource_id.into(),
                    Transfer3D {
                        stride: destination.stride.into(),
                        ..Transfer3D::new_2d(
                            0,
                            0,
                            destination.width.into(),
                            destination.height.into(),
                            0,
                        )
                    },
                )?;
                done.readback = bytes;
                done.uploaded = u64::from(destination.width.to_native())
                    * u64::from(destination.height.to_native())
                    * 4;
            }
            _ => return Err(ErrInvalidParameter),
        }
        Ok(done)
    }
}
