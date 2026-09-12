// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license found in LICENSE.

/// Versioned host observations. Never directly advertise these as guest HDR capability:
/// WDDM Advanced Color, metadata transport, precise primary storage and output must agree.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HostDisplayColorCapabilities {
    pub version: u32,
    pub size: u32,
    pub generation: u64,
    pub display_id: i32,
    pub hdr_types: u32,
    pub wide_color_gamut: u32,
    pub usable_hdr_types: u32,
    pub max_luminance: f32,
    pub max_average_luminance: f32,
    pub min_luminance: f32,
    pub reserved: u32,
}

const _: () = assert!(std::mem::size_of::<HostDisplayColorCapabilities>() == 48);

/// Immutable color state belongs to an import and its Surface generation.
/// encoding: 0 SDR, 1 BT2020/PQ, 2 BT2020/HLG, 3 scRGB-linear (currently refused).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct DisplayFrameColor {
    pub version: u32,
    pub size: u32,
    pub generation: u64,
    pub format: u32,
    pub encoding: u32,
    pub has_static_metadata: u32,
    pub reserved: u32,
    pub primaries: [f32; 6],
    pub white_point: [f32; 2],
    pub max_mastering_luminance: f32,
    pub min_mastering_luminance: f32,
    pub max_content_light_level: f32,
    pub max_frame_average_light_level: f32,
}
const _: () = assert!(std::mem::size_of::<DisplayFrameColor>() == 80);

/// Immutable native/std430 transform. Kernel transports float bits; host validates them.
#[repr(C)]
#[derive(Clone, Debug)]
pub struct DisplayTransform {
    pub version: u32,
    pub size: u32,
    pub kind: u32,
    pub lut_count: u32,
    pub matrix: [f32; 12],
    pub scalar: f32,
    pub scale: [f32; 3],
    pub offset: [f32; 3],
    pub reserved: u32,
    pub lut: [[f32; 3]; 4096],
}
const _: () = assert!(std::mem::size_of::<DisplayTransform>() == 49248);

impl DisplayFrameColor {
    pub fn sdr(format: u32, generation: u64) -> Self {
        Self { version: 1, size: std::mem::size_of::<Self>() as u32, generation, format,
            encoding: 0, has_static_metadata: 0, reserved: 0, primaries: [0.; 6],
            white_point: [0.; 2], max_mastering_luminance: 0., min_mastering_luminance: 0.,
            max_content_light_level: 0., max_frame_average_light_level: 0. }
    }
}

impl HostDisplayColorCapabilities {
    pub fn valid(&self) -> bool {
        self.version == 1
            && self.size as usize == std::mem::size_of::<Self>()
            && self.reserved == 0
            && self.hdr_types & !3 == 0
            && self.usable_hdr_types & !self.hdr_types == 0
            && self.wide_color_gamut <= 1
    }
}
