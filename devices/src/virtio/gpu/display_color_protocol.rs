// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license found in LICENSE.

//! DroidVM private control-queue extension; not an upstream virtio-gpu feature.
//! Unknown hosts reject discovery. No capability can be inferred from EDID or format alone.
use data_model::{Le32, Le64};
use gpu_display::display_color::{DisplayFrameColor, DisplayTransform, HostDisplayColorCapabilities};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use super::protocol::virtio_gpu_ctrl_hdr;

pub const CMD_GET_DISPLAY_COLOR: u32 = 0xd100;
pub const CMD_SET_RESOURCE_COLOR: u32 = 0xd101;
pub const CMD_SET_TARGET_TRANSFORM: u32 = 0xd102;
pub const RESP_DISPLAY_COLOR: u32 = 0xd200;
pub const COLOR_MAGIC: u32 = 0x4c435644; // DVCL
pub const COLOR_VERSION: u32 = 1;
pub const DRM_AB30: u32 = u32::from_le_bytes(*b"AB30");
pub const DRM_AR30: u32 = u32::from_le_bytes(*b"AR30");

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct GetDisplayColor {
    pub hdr: virtio_gpu_ctrl_hdr,
    pub magic: Le32,
    pub version: Le32,
    pub size: Le32,
    pub scanout_id: Le32,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct DisplayColorResponse {
    pub query: GetDisplayColor,
    pub generation: Le64,
    pub observed_hdr_types: Le32,
    pub usable_hdr_types: Le32,
    // Physical observations only. 0xffffffff means unknown, units are 0.0001 cd/m2.
    pub max_luminance: Le32,
    pub max_average_luminance: Le32,
    pub min_luminance: Le32,
    pub reserved: Le32,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct SetResourceColor {
    pub query: GetDisplayColor,
    pub generation: Le64,
    pub resource_id: Le32,
    pub format: Le32,
    pub encoding: Le32,
    pub has_static_metadata: Le32,
    // Rxy, Gxy, Bxy, Wxy in units of 0.00002, matching DXGI HDR10.
    pub chromaticities: [Le32; 8],
    pub max_mastering_luminance: Le32,       // cd/m2
    pub min_mastering_luminance: Le32,       // 0.0001 cd/m2
    pub max_content_light_level: Le32,       // cd/m2
    pub max_frame_average_light_level: Le32, // cd/m2
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct SetTargetTransform {
    pub query: GetDisplayColor,
    pub generation: Le64,
    pub reserved: [Le32; 2],
}
#[repr(C)]
#[derive(Copy, Clone, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub struct TransformPayload {
    pub version: Le32, pub size: Le32, pub kind: Le32, pub lut_count: Le32,
    pub matrix: [Le32; 12], pub scalar: Le32, pub scale: [Le32; 3],
    pub offset: [Le32; 3], pub reserved: Le32, pub lut: [[Le32; 3]; 4096],
}
const _: () = assert!(std::mem::size_of::<SetTargetTransform>() == 56);
const _: () = assert!(std::mem::size_of::<TransformPayload>() == 49248);

impl TransformPayload {
    pub fn decode(&self) -> Option<DisplayTransform> {
        let f = |v: Le32| f32::from_bits(v.to_native());
        let result = DisplayTransform {
            version: self.version.to_native(), size: self.size.to_native(),
            kind: self.kind.to_native(), lut_count: self.lut_count.to_native(),
            matrix: self.matrix.map(f), scalar: f(self.scalar), scale: self.scale.map(f),
            offset: self.offset.map(f), reserved: self.reserved.to_native(),
            lut: self.lut.map(|v| v.map(f)),
        };
        if result.version != 1 || result.size != 49248 || result.reserved != 0 ||
            !matches!((result.kind, result.lut_count), (0, 0) | (1, 1025) | (2, 4096)) ||
            !result.scalar.is_finite() ||
            result.matrix.iter().any(|v| !v.is_finite() || !(v * result.scalar).is_finite()) ||
            result.scale.iter().chain(result.offset.iter()).any(|v| !v.is_finite()) ||
            result.lut.iter().enumerate().any(|(i, rgb)| rgb.iter().any(|v|
                !v.is_finite() || (i >= result.lut_count as usize && *v != 0.0))) {
            return None;
        }
        Some(result)
    }
}

const _: () = assert!(std::mem::size_of::<GetDisplayColor>() == 40);
const _: () = assert!(std::mem::size_of::<DisplayColorResponse>() == 72);
const _: () = assert!(std::mem::size_of::<SetResourceColor>() == 112);

impl GetDisplayColor {
    pub fn valid(&self, size: usize) -> bool {
        self.magic.to_native() == COLOR_MAGIC
            && self.version.to_native() == COLOR_VERSION
            && self.size.to_native() as usize == size
            && self.scanout_id.to_native() == 0
            && self.hdr.ctx_id.to_native() == 0
            && self.hdr.ring_idx == 0
    }
}

impl DisplayColorResponse {
    pub fn from_host(query: GetDisplayColor, caps: HostDisplayColorCapabilities) -> Self {
        fn luminance(value: f32) -> Le32 {
            if value.is_finite() && value >= 0.0 && value < (u32::MAX as f32) / 10000.0 {
                ((value * 10000.0).round() as u32).into()
            } else {
                u32::MAX.into()
            }
        }
        Self {
            query: GetDisplayColor {
                size: (std::mem::size_of::<Self>() as u32).into(),
                ..query
            },
            generation: caps.generation.into(),
            observed_hdr_types: caps.hdr_types.into(),
            usable_hdr_types: caps.usable_hdr_types.into(),
            max_luminance: luminance(caps.max_luminance),
            max_average_luminance: luminance(caps.max_average_luminance),
            min_luminance: luminance(caps.min_luminance),
            reserved: 0.into(),
        }
    }
}

impl SetResourceColor {
    pub fn frame(
        &self,
        caps: HostDisplayColorCapabilities,
        source_format: u32,
    ) -> Option<DisplayFrameColor> {
        let encoding = self.encoding.to_native();
        let format = self.format.to_native();
        let metadata = self.has_static_metadata.to_native();
        let expected_format = match source_format {
            8 => DRM_AB30,   // VIRGL_FORMAT_R10G10B10A2_UNORM
            131 => DRM_AR30, // VIRGL_FORMAT_B10G10R10A2_UNORM
            _ => return None,
        };
        let hdr_type = match encoding {
            1 => 1,
            2 => 2,
            _ => return None,
        };
        if !self.query.valid(std::mem::size_of::<Self>())
            || !caps.valid()
            || caps.generation == 0
            || self.generation.to_native() != caps.generation
            || caps.usable_hdr_types & hdr_type == 0
            || format != expected_format
            || metadata > 1
            || (encoding == 2 && metadata != 0)
            || self.chromaticities.iter().any(|v| v.to_native() > 50000)
            || self.max_mastering_luminance.to_native() > 10000
            || self.min_mastering_luminance.to_native() > 100000000
            || self.max_content_light_level.to_native() > 10000
            || self.max_frame_average_light_level.to_native() > 10000
        {
            return None;
        }
        let max = self.max_mastering_luminance.to_native();
        let min = self.min_mastering_luminance.to_native();
        let cll = self.max_content_light_level.to_native();
        let fall = self.max_frame_average_light_level.to_native();
        if metadata == 0 {
            if self.chromaticities.iter().any(|v| v.to_native() != 0)
                || max != 0
                || min != 0
                || cll != 0
                || fall != 0
            {
                return None;
            }
        } else if max == 0
            || min >= max * 10000
            || self.chromaticities[6].to_native() == 0
            || self.chromaticities[7].to_native() == 0
            || (cll != 0 && fall > cll)
            || self
                .chromaticities
                .chunks_exact(2)
                .any(|xy| {
                    let sum = xy[0].to_native() + xy[1].to_native();
                    sum == 0 || sum > 50000
                })
        {
            return None;
        }
        let xy = self.chromaticities.map(|v| v.to_native() as f32 / 50000.0);
        Some(DisplayFrameColor {
            version: 1,
            size: std::mem::size_of::<DisplayFrameColor>() as u32,
            generation: caps.generation,
            format,
            encoding,
            has_static_metadata: metadata,
            reserved: 0,
            primaries: [xy[0], xy[1], xy[2], xy[3], xy[4], xy[5]],
            white_point: [xy[6], xy[7]],
            max_mastering_luminance: max as f32,
            min_mastering_luminance: min as f32 / 10000.0,
            max_content_light_level: cll as f32,
            max_frame_average_light_level: fall as f32,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (SetResourceColor, HostDisplayColorCapabilities) {
        let mut request = SetResourceColor::default();
        request.query.magic = COLOR_MAGIC.into();
        request.query.version = 1.into();
        request.query.size = 112.into();
        request.generation = 7.into();
        request.resource_id = 42.into();
        request.format = DRM_AB30.into();
        request.encoding = 1.into();
        request.has_static_metadata = 1.into();
        request.chromaticities =
            [35400, 14600, 8500, 39850, 6550, 2300, 15635, 16450].map(Le32::from);
        request.max_mastering_luminance = 1000.into();
        request.min_mastering_luminance = 50.into();
        request.max_content_light_level = 1000.into();
        request.max_frame_average_light_level = 400.into();
        let caps = HostDisplayColorCapabilities {
            version: 1,
            size: 48,
            generation: 7,
            hdr_types: 3,
            usable_hdr_types: 3,
            ..Default::default()
        };
        (request, caps)
    }

    #[test]
    fn transform_payload_preserves_bits_and_rejects_nonfinite_or_trailing_lut() {
        let mut bytes = vec![0u8; 49248];
        for (offset, value) in [(0, 1u32), (4, 49248), (8, 2), (12, 4096),
                                (16, 1.0f32.to_bits()), (64, 0.5f32.to_bits()),
                                (96 + (4095 * 3 + 2) * 4, 0.75f32.to_bits())] {
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        let payload = TransformPayload::read_from_bytes(&bytes).unwrap();
        let t = payload.decode().unwrap();
        assert_eq!(t.matrix[0], 1.0);
        assert_eq!(t.scalar, 0.5);
        assert_eq!(t.lut[4095][2], 0.75);
        bytes[16..20].copy_from_slice(&f32::NAN.to_bits().to_le_bytes());
        assert!(TransformPayload::read_from_bytes(&bytes).unwrap().decode().is_none());
        bytes[16..20].copy_from_slice(&1.0f32.to_bits().to_le_bytes());
        bytes[8..12].copy_from_slice(&1u32.to_le_bytes());
        bytes[12..16].copy_from_slice(&1025u32.to_le_bytes());
        assert!(TransformPayload::read_from_bytes(&bytes).unwrap().decode().is_none());
        assert!(TransformPayload::read_from_bytes(&bytes[..49247]).is_err());
    }

    #[test]
    fn hdr10_units_and_exact_storage() {
        let (request, caps) = fixture();
        let frame = request.frame(caps, 8).unwrap();
        assert_eq!(frame.max_mastering_luminance, 1000.0);
        assert_eq!(frame.min_mastering_luminance, 0.005);
        assert_eq!(frame.primaries[0], 0.708);
        assert!(request.frame(caps, 67).is_none());
        assert!(request.frame(caps, 131).is_none());
        let bytes = request.as_bytes();
        assert_eq!(&bytes[40..48], &7u64.to_le_bytes());
        assert_eq!(&bytes[48..52], &42u32.to_le_bytes());
        assert_eq!(&bytes[52..56], b"AB30");
        assert_eq!(&bytes[100..104], &50u32.to_le_bytes());
        assert!(SetResourceColor::read_from_bytes(&bytes[..111]).is_err());
    }

    #[test]
    fn stale_disabled_and_malformed_never_become_hdr() {
        let (request, mut caps) = fixture();
        caps.generation = 8;
        assert!(request.frame(caps, 8).is_none());
        caps.generation = 7;
        caps.usable_hdr_types = 0;
        assert!(request.frame(caps, 8).is_none());
        caps.usable_hdr_types = 3;
        for offset in [24, 28, 32, 36, 56, 60] {
            let mut bytes = request.as_bytes().to_vec();
            bytes[offset..offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());
            let corrupt = SetResourceColor::read_from_bytes(&bytes).unwrap();
            assert!(corrupt.frame(caps, 8).is_none(), "offset {offset}");
        }
        let mut corrupt = request;
        corrupt.min_mastering_luminance = 10000001.into();
        assert!(corrupt.frame(caps, 8).is_none());
        corrupt = request;
        corrupt.max_frame_average_light_level = 1001.into();
        assert!(corrupt.frame(caps, 8).is_none());
        corrupt = request;
        corrupt.encoding = 2.into();
        assert!(corrupt.frame(caps, 8).is_none());
    }
}
