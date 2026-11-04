// Copyright 2022 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Implementation of the EDID specification provided by software.
//! EDID spec: <https://glenwing.github.io/docs/VESA-EEDID-A2.pdf>

use std::fmt;
use std::fmt::Debug;

use super::protocol::GpuResponse::*;
use super::protocol::VirtioGpuResult;
use crate::virtio::gpu::GpuDisplayParameters;

const EDID_DATA_LENGTH: usize = 128;
const DEFAULT_HORIZONTAL_BLANKING: u16 = 560;
const DEFAULT_VERTICAL_BLANKING: u16 = 50;
const DEFAULT_HORIZONTAL_FRONT_PORCH: u16 = 64;
const DEFAULT_VERTICAL_FRONT_PORCH: u16 = 1;
const DEFAULT_HORIZONTAL_SYNC_PULSE: u16 = 192;
const DEFAULT_VERTICAL_SYNC_PULSE: u16 = 3;
const MILLIMETERS_PER_INCH: f32 = 25.4;

/// This class is used to create the Extended Display Identification Data (EDID), which will be
/// exposed to the guest system.
///
/// We ignore most of the spec, the point here being for us to provide enough for graphics to work
/// and to allow us to configure the resolution and refresh rate (via the preferred timing mode
/// pixel clock).
///
/// Legacy detailed timings describe modes within their encoding limits. A DisplayID extension
/// carries the configured preferred timing when its clock or geometry exceeds those limits.
#[repr(C)]
pub struct EdidBytes {
    bytes: Vec<u8>,
}

impl EdidBytes {
    /// Creates a virtual EDID block.
    pub fn new(info: &DisplayInfo) -> VirtioGpuResult {
        let clock = pixel_clock_10khz(info).ok_or_else(|| {
            ErrEdid("Display timing is outside the DisplayID type-I encoding".to_owned())
        })?;
        let legacy = info.width() <= 0xfff && info.height() <= 0xfff && clock <= u16::MAX as u32;
        let mut edid = vec![
            0;
            if legacy {
                EDID_DATA_LENGTH
            } else {
                EDID_DATA_LENGTH * 2
            }
        ];

        populate_header(&mut edid);
        populate_edid_version(&mut edid);
        populate_input_and_features(&mut edid);
        populate_size(&mut edid, info);
        populate_standard_timings(&mut edid)?;

        // 4 available descriptor blocks
        let block0 = &mut edid[54..72];
        if legacy {
            populate_detailed_timing(block0, info, clock as u16);
        } else {
            // Keep a valid legacy recovery timing; the configured preferred mode
            // lives in DisplayID with its full pixel clock and geometry.
            let mut fallback = *info;
            if fallback.width() > 0xfff || fallback.height() > 0xfff {
                fallback.resolution = Resolution::new(1920, 1080);
            }
            fallback.refresh_rate = 60;
            let fallback_clock = pixel_clock_10khz(&fallback).unwrap();
            if fallback_clock > u16::MAX as u32 {
                fallback.resolution = Resolution::new(1920, 1080);
            }
            populate_detailed_timing(
                block0,
                &fallback,
                pixel_clock_10khz(&fallback).unwrap() as u16,
            );
            edid[126] = 1;
            populate_displayid(&mut edid[128..256], info, clock);
        }

        let block1 = &mut edid[72..90];
        populate_display_name(block1);
        // An unused descriptor must use the Dummy Descriptor tag. An all-zero
        // descriptor is malformed and can cause an otherwise valid EDID to
        // fail monitor validation.
        edid[93] = 0x10;
        edid[111] = 0x10;

        calculate_checksum(&mut edid);

        Ok(OkEdid(Box::new(Self { bytes: edid })))
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl Debug for EdidBytes {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.bytes[..].fmt(f)
    }
}

impl PartialEq for EdidBytes {
    fn eq(&self, other: &EdidBytes) -> bool {
        self.bytes[..] == other.bytes[..]
    }
}

#[derive(Copy, Clone)]
pub struct Resolution {
    width: u32,
    height: u32,
}

impl Resolution {
    fn new(width: u32, height: u32) -> Resolution {
        Resolution { width, height }
    }

    fn get_aspect_ratio(&self) -> (u32, u32) {
        let divisor = gcd(self.width, self.height);
        (self.width / divisor, self.height / divisor)
    }
}

fn gcd(x: u32, y: u32) -> u32 {
    match y {
        0 => x,
        _ => gcd(y, x % y),
    }
}

#[derive(Copy, Clone)]
pub struct DisplayInfo {
    resolution: Resolution,
    refresh_rate: u32,
    horizontal_blanking: u16,
    vertical_blanking: u16,
    horizontal_front: u16,
    vertical_front: u16,
    horizontal_sync: u16,
    vertical_sync: u16,
    width_millimeters: u16,
    height_millimeters: u16,
}

impl DisplayInfo {
    /// Only width, height and refresh rate are required for the graphics stack to work, so instead
    /// of pulling actual numbers from the system, we just use some typical values to populate other
    /// fields for now.
    pub fn new(params: &GpuDisplayParameters) -> Self {
        let (width, height) = params.get_virtual_display_size();

        let width_millimeters = if params.horizontal_dpi() != 0 {
            ((width as f32 / params.horizontal_dpi() as f32) * MILLIMETERS_PER_INCH) as u16
        } else {
            0
        };
        let height_millimeters = if params.vertical_dpi() != 0 {
            ((height as f32 / params.vertical_dpi() as f32) * MILLIMETERS_PER_INCH) as u16
        } else {
            0
        };

        Self {
            resolution: Resolution::new(width, height),
            refresh_rate: params.refresh_rate,
            horizontal_blanking: DEFAULT_HORIZONTAL_BLANKING,
            vertical_blanking: DEFAULT_VERTICAL_BLANKING,
            horizontal_front: DEFAULT_HORIZONTAL_FRONT_PORCH,
            vertical_front: DEFAULT_VERTICAL_FRONT_PORCH,
            horizontal_sync: DEFAULT_HORIZONTAL_SYNC_PULSE,
            vertical_sync: DEFAULT_VERTICAL_SYNC_PULSE,
            width_millimeters,
            height_millimeters,
        }
    }

    pub fn width(&self) -> u32 {
        self.resolution.width
    }

    pub fn height(&self) -> u32 {
        self.resolution.height
    }

    pub fn width_centimeters(&self) -> u8 {
        (self.width_millimeters / 10) as u8
    }

    pub fn height_centimeters(&self) -> u8 {
        (self.height_millimeters / 10) as u8
    }
}

fn populate_display_name(edid_block: &mut [u8]) {
    // Display Product Name String Descriptor Tag
    edid_block[0..5].clone_from_slice(&[0x00, 0x00, 0x00, 0xFC, 0x00]);
    edid_block[5..].clone_from_slice("CrosvmDisplay".as_bytes());
}

// EDID DTD uses 16-bit 10kHz units. DisplayID type-I uses 24-bit units minus
// one. Widen BEFORE multiplication and round once at the actual 10kHz unit.
fn pixel_clock_10khz(info: &DisplayInfo) -> Option<u32> {
    if info.width() == 0
        || info.height() == 0
        || info.width() > 65536
        || info.height() > 65536
        || info.refresh_rate == 0
    {
        return None;
    }
    let htotal = u64::from(info.width()) + u64::from(info.horizontal_blanking);
    let vtotal = u64::from(info.height()) + u64::from(info.vertical_blanking);
    let clock = (u64::from(info.refresh_rate)
        .checked_mul(htotal)?
        .checked_mul(vtotal)?
        .checked_add(5000)?)
        / 10000;
    if clock == 0 || clock > 0x1000000 {
        None
    } else {
        Some(clock as u32)
    }
}

fn populate_displayid(extension: &mut [u8], info: &DisplayInfo, clock: u32) {
    // DisplayID 1.3 standalone virtual display. Keep product identity equal to
    // the base EDID, describe the configured geometry, and advertise only the
    // RGB8 virtual interface. No physical connector, HDR or audio is implied.
    // Block payload lengths exclude their three-byte headers.
    extension[0..5].copy_from_slice(&[0x70, 0x13, 79, 3, 0]);
    let product = &mut extension[5..33];
    product[0..3].copy_from_slice(&[0, 0, 25]);
    product[3..6].copy_from_slice(b"GGL");
    product[6..8].copy_from_slice(&1u16.to_le_bytes());
    product[8..12].copy_from_slice(&1u32.to_le_bytes());
    product[12..15].copy_from_slice(&[8, 22, 13]); // same week/year/name as base
    product[15..28].copy_from_slice(b"CrosvmDisplay");

    let parameters = &mut extension[33..48];
    parameters[0..3].copy_from_slice(&[1, 0, 12]);
    for (offset, size) in [(3, info.width_millimeters), (5, info.height_millimeters)] {
        // Zero means unspecified when the inherited DPI-derived size cannot
        // fit DisplayID's 0.1mm field; do not wrap into another physical size.
        let tenths = u16::try_from(u32::from(size) * 10).unwrap_or(0);
        parameters[offset..offset + 2].copy_from_slice(&tenths.to_le_bytes());
    }
    if info.width() <= u16::MAX as u32 && info.height() <= u16::MAX as u32 {
        parameters[7..9].copy_from_slice(&(info.width() as u16).to_le_bytes());
        parameters[9..11].copy_from_slice(&(info.height() as u16).to_le_bytes());
    }
    parameters[12] = 0xff; // transfer characteristics are unspecified
    parameters[13] = ((u64::from(info.width()) * 100 / u64::from(info.height()))
        .saturating_sub(100))
    .min(255) as u8;
    parameters[14] = 0x77; // eight bits per component, native and overall

    extension[48..51].copy_from_slice(&[3, 0, 20]);
    let d = &mut extension[51..71];
    let encoded_clock = clock - 1;
    d[0..3].copy_from_slice(&encoded_clock.to_le_bytes()[0..3]);
    d[3] = 0x88; // preferred, progressive, no stereo, undefined aspect ratio
    for (offset, value) in [
        (4, info.width()),
        (6, u32::from(info.horizontal_blanking)),
        (8, u32::from(info.horizontal_front)),
        (10, u32::from(info.horizontal_sync)),
        (12, info.height()),
        (14, u32::from(info.vertical_blanking)),
        (16, u32::from(info.vertical_front)),
        (18, u32::from(info.vertical_sync)),
    ] {
        d[offset..offset + 2].copy_from_slice(&((value - 1) as u16).to_le_bytes());
    }
    d[9] |= 0x80; // positive horizontal sync
    d[17] |= 0x80; // positive vertical sync
                   // Display Interface block: proprietary digital, one virtual link, no
                   // standard version or content protection, RGB8 only (no YCbCr modes).
    extension[71..84].copy_from_slice(&[0x0f, 0, 10, 0xb1, 0, 2, 0, 0, 0, 0, 0, 0, 0]);
    // Section checksum excludes tag 0x70; EDID checksum covers all 128 bytes.
    extension[84] = 0u8.wrapping_sub(
        extension[1..84]
            .iter()
            .fold(0u8, |sum, byte| sum.wrapping_add(*byte)),
    );
    calculate_checksum(extension);
}

fn populate_detailed_timing(edid_block: &mut [u8], info: &DisplayInfo, clock: u16) {
    assert_eq!(edid_block.len(), 18);

    // Detailed timings
    //
    // 18 Byte Descriptors - 72 Bytes
    // The 72 bytes in this section are divided into four data fields. Each of the four data fields
    // are 18 bytes in length. These 18 byte data fields shall contain either detailed timing data
    // as described in Section 3.10.2 or other types of data as described in Section 3.10.3. The
    // addresses and the contents of the four 18 byte descriptors are shown in Table 3.20.
    //
    // We leave the bottom 6 bytes of this block purposefully empty.
    let horizontal_blanking_lsb: u8 = (info.horizontal_blanking & 0xFF) as u8;
    let horizontal_blanking_msb: u8 = ((info.horizontal_blanking >> 8) & 0x0F) as u8;

    let vertical_blanking_lsb: u8 = (info.vertical_blanking & 0xFF) as u8;
    let vertical_blanking_msb: u8 = ((info.vertical_blanking >> 8) & 0x0F) as u8;

    // The pixel clock is what controls the refresh timing information.
    //
    // The formula for getting refresh rate out of this value is:
    //   refresh_rate = clk * 10000 / (htotal * vtotal)
    // Solving for clk:
    //   clk = (refresh_rate * htotal * votal) / 10000
    //
    // where:
    //   clk - The setting here
    //   vtotal - Total lines
    //   htotal - Total pixels per line
    //
    // Value here is pixel clock divided by10,000, in10kHz steps.
    //
    // Pseudocode of kernel logic for vrefresh:
    //    vtotal := mode->vtotal;
    //    calc_val := (clock * 1000) / htotal
    //    refresh := (calc_val + vtotal / 2) / vtotal
    //    if flags & INTERLACE: refresh *= 2
    //    if flags & DBLSCAN: refresh /= 2
    //    if vscan > 1: refresh /= vscan
    //
    edid_block[0..2].copy_from_slice(&clock.to_le_bytes());

    let width_lsb: u8 = (info.width() & 0xFF) as u8;
    let width_msb: u8 = ((info.width() >> 8) & 0x0F) as u8;

    // Horizointal Addressable Video in pixels.
    edid_block[2] = width_lsb;
    // Horizontal blanking in pixels.
    edid_block[3] = horizontal_blanking_lsb;
    // Upper bits of the two above vals.
    edid_block[4] = horizontal_blanking_msb | (width_msb << 4);

    let vertical_active: u32 = info.height();
    let vertical_active_lsb: u8 = (vertical_active & 0xFF) as u8;
    let vertical_active_msb: u8 = ((vertical_active >> 8) & 0x0F) as u8;

    // Vertical addressable video in *lines*
    edid_block[5] = vertical_active_lsb;
    // Vertical blanking in lines
    edid_block[6] = vertical_blanking_lsb;
    // Sigbits of the above.
    edid_block[7] = vertical_blanking_msb | (vertical_active_msb << 4);

    let horizontal_front_lsb: u8 = (info.horizontal_front & 0xFF) as u8; // least sig 8 bits
    let horizontal_front_msb: u8 = ((info.horizontal_front >> 8) & 0x03) as u8; // most sig 2 bits
    let horizontal_sync_lsb: u8 = (info.horizontal_sync & 0xFF) as u8; // least sig 8 bits
    let horizontal_sync_msb: u8 = ((info.horizontal_sync >> 8) & 0x03) as u8; // most sig 2 bits

    let vertical_front_lsb: u8 = (info.vertical_front & 0x0F) as u8; // least sig 4 bits
    let vertical_front_msb: u8 = ((info.vertical_front >> 8) & 0x0F) as u8; // most sig 2 bits
    let vertical_sync_lsb: u8 = (info.vertical_sync & 0xFF) as u8; // least sig 4 bits
    let vertical_sync_msb: u8 = ((info.vertical_sync >> 8) & 0x0F) as u8; // most sig 2 bits

    // Horizontal front porch in pixels.
    edid_block[8] = horizontal_front_lsb;
    // Horizontal sync pulse width in pixels.
    edid_block[9] = horizontal_sync_lsb;
    // LSB of vertical front porch and sync pulse
    edid_block[10] = vertical_sync_lsb | (vertical_front_lsb << 4);
    // Upper 2 bits of these values.
    edid_block[11] = vertical_sync_msb
        | (vertical_front_msb << 2)
        | (horizontal_sync_msb << 4)
        | (horizontal_front_msb << 6);

    let width_millimeters_lsb: u8 = (info.width_millimeters & 0xFF) as u8; // least sig 8 bits
    let width_millimeters_msb: u8 = ((info.width_millimeters >> 8) & 0xF) as u8; // most sig 4 bits

    let height_millimeters_lsb: u8 = (info.height_millimeters & 0xFF) as u8; // least sig 8 bits
    let height_millimeters_msb: u8 = ((info.height_millimeters >> 8) & 0xF) as u8; // most sig 4 bits

    edid_block[12] = width_millimeters_lsb;
    edid_block[13] = height_millimeters_lsb;
    edid_block[14] = height_millimeters_msb | (width_millimeters_msb << 4);

    // Signal definitions: digital separate sync, positive h/v polarity (same as a real digital
    // panel and QEMU's generator). A zero here means "analog composite sync", which newer guest
    // kernels reject outright -- dropping this (the preferred!) mode from the connector's list,
    // so the guest falls back to whichever standard timing is largest and renders at the wrong
    // aspect ratio.
    edid_block[17] = 0x1E;
}

// The EDID header. This is defined by the EDID spec.
fn populate_header(edid: &mut [u8]) {
    edid[0] = 0x00;
    edid[1] = 0xFF;
    edid[2] = 0xFF;
    edid[3] = 0xFF;
    edid[4] = 0xFF;
    edid[5] = 0xFF;
    edid[6] = 0xFF;
    edid[7] = 0x00;

    let manufacturer_name: [char; 3] = ['G', 'G', 'L'];
    // 00001 -> A, 00010 -> B, etc
    let manufacturer_id: u16 = manufacturer_name
        .iter()
        .map(|c| (*c as u8 - b'A' + 1) & 0x1F)
        .fold(0u16, |res, lsb| (res << 5) | (lsb as u16));
    edid[8..10].copy_from_slice(&manufacturer_id.to_be_bytes());

    let manufacture_product_id: u16 = 1;
    edid[10..12].copy_from_slice(&manufacture_product_id.to_le_bytes());

    let serial_id: u32 = 1;
    edid[12..16].copy_from_slice(&serial_id.to_le_bytes());

    let manufacture_week: u8 = 8;
    edid[16] = manufacture_week;

    let manufacture_year: u32 = 2022;
    edid[17] = (manufacture_year - 1990u32) as u8;
}

// The standard timings are 8 timing modes with a lower priority (and different data format)
// than the 4 detailed timing modes.
fn populate_standard_timings(edid: &mut [u8]) -> VirtioGpuResult {
    let resolutions = [
        Resolution::new(1440, 900),
        Resolution::new(1600, 900),
        Resolution::new(800, 600),
        Resolution::new(1680, 1050),
        Resolution::new(1856, 1392),
        Resolution::new(1280, 1024),
        Resolution::new(1400, 1050),
        Resolution::new(1920, 1200),
    ];

    // Index 0 is horizontal pixels / 8 - 31
    // Index 1 is the aspect ratio in bits 7:6 and the refresh_rate - 60 in bits 5:0 (so we are
    // setting the refresh field to 0 = 60 Hz, for now).
    for (index, r) in resolutions.iter().enumerate() {
        edid[0x26 + (index * 2)] = (r.width / 8 - 31) as u8;
        let ar_bits: u8 = match r.get_aspect_ratio() {
            (8, 5) => 0x0,
            (4, 3) => 0x1,
            (5, 4) => 0x2,
            (16, 9) => 0x3,
            (x, y) => return Err(ErrEdid(format!("Unsupported aspect ratio: {} {}", x, y))),
        };
        // The aspect ratio lives in the TOP two bits; writing it to the low bits (a former bug)
        // declares every entry as 16:10 with a bogus 61-63 Hz refresh, which turns the whole
        // list into phantom modes (e.g. 1856x1160) the host never meant to advertise.
        edid[0x27 + (index * 2)] = ar_bits << 6;
    }
    Ok(OkNoData)
}

// Per the EDID spec, needs to be 1 and 4.
fn populate_edid_version(edid: &mut [u8]) {
    edid[18] = 1;
    edid[19] = 4;
}

// Video input definition (byte 20) and feature support (byte 24). We are a digital display (the
// detailed timing also declares digital separate sync) and the first detailed timing block is the
// native/preferred mode -- stating it explicitly keeps EDID 1.3-era parsers on the same page as
// the EDID 1.4 "first detailed timing is preferred" rule.
fn populate_input_and_features(edid: &mut [u8]) {
    edid[20] = 0x80; // digital input, bit depth/interface undefined
    edid[24] = 0x02; // preferred timing mode includes the native timing
}

fn populate_size(edid: &mut [u8], info: &DisplayInfo) {
    edid[21] = info.width_centimeters();
    edid[22] = info.height_centimeters();
}

fn calculate_checksum(edid: &mut [u8]) {
    let mut checksum: u8 = 0;
    for byte in edid.iter().take(EDID_DATA_LENGTH - 1) {
        checksum = checksum.wrapping_add(*byte);
    }

    if checksum != 0 {
        checksum = 255 - checksum + 1;
    }

    edid[127] = checksum;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(width: u32, height: u32, refresh_rate: u32) -> DisplayInfo {
        DisplayInfo {
            resolution: Resolution::new(width, height),
            refresh_rate,
            horizontal_blanking: DEFAULT_HORIZONTAL_BLANKING,
            vertical_blanking: DEFAULT_VERTICAL_BLANKING,
            horizontal_front: DEFAULT_HORIZONTAL_FRONT_PORCH,
            vertical_front: DEFAULT_VERTICAL_FRONT_PORCH,
            horizontal_sync: DEFAULT_HORIZONTAL_SYNC_PULSE,
            vertical_sync: DEFAULT_VERTICAL_SYNC_PULSE,
            width_millimeters: 300,
            height_millimeters: 200,
        }
    }

    fn bytes(info: &DisplayInfo) -> Vec<u8> {
        match EdidBytes::new(info).unwrap() {
            OkEdid(edid) => edid.as_bytes().to_vec(),
            _ => panic!("missing EDID"),
        }
    }

    #[test]
    fn reported_7183hz_wrap_is_replaced_by_full_displayid_clock() {
        let edid = bytes(&info(3040, 1904, 165));
        assert_eq!(edid.len(), 256);
        assert_eq!(edid[126], 1);
        assert_eq!(&edid[128..133], &[0x70, 0x13, 79, 3, 0]);
        assert_eq!(&edid[176..179], &[3, 0, 20]);
        let units =
            1 + u32::from(edid[179]) + (u32::from(edid[180]) << 8) + (u32::from(edid[181]) << 16);
        assert_eq!(units * 10000, 1_160_680_000);
        assert_ne!(u16::from_le_bytes([edid[54], edid[55]]), 50530);
        for block in edid.chunks_exact(128) {
            assert_eq!(
                block.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)),
                0
            );
        }
        assert_eq!(
            edid[129..213]
                .iter()
                .fold(0u8, |sum, byte| sum.wrapping_add(*byte)),
            0
        );
    }

    #[test]
    fn legacy_clock_quantizes_once_and_does_not_overflow_rounding() {
        let edid = bytes(&info(1920, 1080, 165));
        assert_eq!(edid.len(), 128);
        assert_eq!(u16::from_le_bytes([edid[54], edid[55]]), 46240);
        let mut edge = info(640, 480, 1);
        edge.horizontal_blanking = 1;
        edge.vertical_blanking = 1;
        // Exact10kHz boundary and values that used to overflow clock+5.
        for units in [65530u16, 65531, 65534, 65535] {
            let mut dtd = [0; 18];
            populate_detailed_timing(&mut dtd, &edge, units);
            assert_eq!(u16::from_le_bytes([dtd[0], dtd[1]]), units);
        }
        assert_eq!(pixel_clock_10khz(&info(3040, 1904, 93)), Some(65420));
        assert_eq!(bytes(&info(3040, 1904, 93)).len(), 128);
        assert_eq!(bytes(&info(3040, 1904, 94)).len(), 256);
    }

    #[test]
    fn high_geometry_and_wide_multiplication_remain_exact() {
        let edid = bytes(&info(7680, 4320, 240));
        assert_eq!(edid.len(), 256);
        let units = pixel_clock_10khz(&info(7680, 4320, 240)).unwrap();
        assert_eq!(u64::from(units) * 10000, 8_642_110_000);
        assert_eq!(pixel_clock_10khz(&info(0, 1080, 60)), None);
        assert_eq!(pixel_clock_10khz(&info(1920, 1080, 0)), None);
        assert_eq!(pixel_clock_10khz(&info(u32::MAX, 1080, 60)), None);
        assert_eq!(pixel_clock_10khz(&info(65536, 65536, u32::MAX)), None);
        assert!(EdidBytes::new(&info(65537, 1080, 60)).is_err());
    }
}
