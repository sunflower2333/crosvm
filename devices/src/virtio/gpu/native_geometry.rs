// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license.

//! Paired geometry transport. Normal operation remains disabled by default.
use super::*;

pub(super) const GEOMETRY_REQUIRED_FEATURES: u64 = (1 << VIRTIO_GPU_F_NATIVE_AHB_GEOMETRY)
    | (1 << VIRTIO_GPU_F_NATIVE_AHB_V2)
    | (1 << VIRTIO_GPU_F_NATIVE_AHB_RELEASE)
    | (1 << VIRTIO_GPU_F_NATIVE_AHB_PAGING);

/// Offer transport only for a deliberately paired diagnostic. This does not
/// promise an attached profile, a rotation-aware guest or physical scanout.
/// Prerequisites must come from this GPU's actual generated features, never
/// from base_features supplied by a caller. In particular Native AHB=0 wins.
pub(super) fn geometry_feature_mask(
    combined_features: u64,
    generated_features: u64,
    android_display: bool,
    diagnostic: Option<&str>,
) -> u64 {
    let geometry = 1 << VIRTIO_GPU_F_NATIVE_AHB_GEOMETRY;
    let prerequisites = GEOMETRY_REQUIRED_FEATURES & !geometry;
    let masked = combined_features & !geometry;
    if android_display
        && (VIRTIO_GPU_NATIVE_AHB_GEOMETRY_ENABLED || diagnostic == Some("1"))
        && generated_features & prerequisites == prerequisites
    {
        masked | geometry
    } else {
        masked
    }
}

pub(super) fn geometry_negotiated(offered: u64, acknowledged: u64) -> bool {
    (offered & acknowledged & GEOMETRY_REQUIRED_FEATURES) == GEOMETRY_REQUIRED_FEATURES
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ProfileObservation {
    Unavailable,
    Snapshot { host_reset: u64, profile: ScanoutProfile },
}

impl Worker {
    // An attachment refresh is suppressed after its first displayed frame.
    // Profile revocation is independent: even an idle, already-displayed
    // Surface can change its root hint, dimensions or availability.
    pub(super) fn observe_native_scanout_profile(
        state: &mut Frontend,
        previous: &mut Option<ProfileObservation>,
        event: &AtomicBool,
    ) -> bool {
        if !state.native_geometry_negotiated {
            return false;
        }
        let observed = match state.virtio_gpu.query_native_profile(0) {
            Ok(GpuResponse::OkNativeProfile { profile, reset_generation, .. }) => {
                let profile: ScanoutProfile = profile.into();
                if reset_generation != 0 && profile.valid() {
                    ProfileObservation::Snapshot { host_reset: reset_generation, profile }
                } else {
                    ProfileObservation::Unavailable
                }
            }
            _ => ProfileObservation::Unavailable,
        };
        if previous.as_ref() == Some(&observed) {
            return false;
        }
        *previous = Some(observed);
        // Coalesced notification only. ACK must precede the guest's fresh
        // QUERY_PROFILE, so a later change cannot be cleared after that query.
        // Neither this event nor a query renews a resource or a reader lease.
        event.store(true, Ordering::Release);
        true
    }
}

impl Frontend {
    pub(super) fn process_native_geometry(
        &mut self,
        cmd: GpuCommand,
        desc_chain: DescriptorChain,
        source: GpuQueue,
    ) -> Option<ReturnDescriptor> {
        let header = *cmd.ctrl_hdr();
        let mut descriptor = ReturnDescriptor {
            desc_chain,
            len: 0,
            queue: source,
            ctx_id: 0,
        };
        // Activation carries the intersection of the actual offered and
        // acknowledged prerequisite bits. A guest cannot enable the diagnostic
        // by sending a command or acknowledging a bit we did not offer.
        let enabled = self.native_geometry_negotiated;
        #[cfg(test)]
        let enabled = enabled || self.native_geometry_test_enabled;
        let required_response = match cmd {
            GpuCommand::QueryNativeProfile(_) => 112,
            GpuCommand::QueryNativeGeometry(_) => 96,
            GpuCommand::PresentNativeGeometry(_) | GpuCommand::RefreshNativeGeometry(_) => 40,
            _ => 24,
        };
        let valid = enabled
            && !self.display_failed
            && source == GpuQueue::Control
            && descriptor.desc_chain.reader.available_bytes() == 0
            && descriptor.desc_chain.writer.available_bytes() >= required_response
            && header.flags.to_native() == 0
            && header.fence_id.to_native() == 0
            && header.ctx_id.to_native() == 0
            && header.ring_idx == 0
            && header.padding == [0; 3];
        let result = if !valid {
            Err(GpuResponse::ErrInvalidParameter)
        } else {
            match cmd {
                GpuCommand::QueryNativeProfile(info) if info.reserved.to_native() == 0 =>
                    self.virtio_gpu.query_native_profile(info.scanout_id.to_native()),
                GpuCommand::ConfigureNativeProfile(info) if info.config.reserved.to_native() == 0 =>
                    self.virtio_gpu.configure_native_profile(info.config.scanout_id.to_native(),
                        info.config.geometry.into(), info.endpoint_generation.to_native(), info.profile_generation.to_native()),
                GpuCommand::QueryNativeGeometry(info) if info.reserved.to_native() == 0 => self
                    .virtio_gpu
                    .query_native_geometry(info.scanout_id.to_native()),
                GpuCommand::ConfigureNativeGeometry(info) if info.reserved.to_native() == 0 => self
                    .virtio_gpu
                    .configure_native_geometry(info.scanout_id.to_native(), info.geometry.into()),
                GpuCommand::BindNativeGeometry(info) => {
                    let resource = info.resource_id.to_native();
                    if self.native_ahb_has_reader(resource)
                        || self.native_ahb_operation_pending(resource)
                        || self.native_ahb_paging.poisoned.contains(&resource)
                    {
                        Err(GpuResponse::ErrInvalidParameter)
                    } else {
                        self.virtio_gpu.bind_native_geometry(
                            resource,
                            info.scanout_id.to_native(),
                            info.reset_generation.to_native(),
                            info.mode_generation.to_native(),
                        )
                    }
                }
                GpuCommand::PresentNativeGeometry(mut info)
                | GpuCommand::RefreshNativeGeometry(mut info) => {
                    if !self.virtio_gpu.native_geometry_matches(
                        info.operation.resource_id.to_native(),
                        info.reset_generation.to_native(),
                        info.mode_generation.to_native(),
                    ) {
                        Err(GpuResponse::ErrInvalidParameter)
                    } else {
                        info.operation.hdr.type_ =
                            if header.type_.to_native() == VIRTIO_GPU_CMD_REFRESH_NATIVE_GEOMETRY {
                                VIRTIO_GPU_CMD_REFRESH_NATIVE_AHB.into()
                            } else {
                                VIRTIO_GPU_CMD_PRESENT_NATIVE_AHB.into()
                            };
                        return self.process_native_ahb_operation_bound(
                            info.operation,
                            descriptor.desc_chain,
                            source,
                            true,
                        );
                    }
                }
                _ => Err(GpuResponse::ErrInvalidParameter),
            }
        };
        descriptor.len = result
            .unwrap_or_else(|error| error)
            .encode(0, 0, 0, 0, &mut descriptor.desc_chain.writer)
            .unwrap_or(0);
        Some(descriptor)
    }
}
