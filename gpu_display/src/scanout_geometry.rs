// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license.

/// Host-endian C ABI. Wire conversion is explicit in the virtio protocol.
/// This describes producer pixels; it does not request a host rotation copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct ScanoutGeometry {
    pub version: u32,
    pub size: u32,
    pub storage_width: u32,
    pub storage_height: u32,
    pub logical_width: u32,
    pub logical_height: u32,
    pub content_rotation: u32,
    pub reserved: u32,
    pub reset_generation: u64,
    pub mode_generation: u64,
    pub reserved_tail: [u64; 2],
}
const _: () = assert!(std::mem::size_of::<ScanoutGeometry>() == 64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct ScanoutProfile {
    pub version: u32, pub size: u32, pub flags: u32, pub content_rotation: u32,
    pub storage_width: u32, pub storage_height: u32, pub logical_width: u32, pub logical_height: u32,
    pub endpoint_generation: u64, pub profile_generation: u64, pub reserved_tail: [u64; 2],
}
const _: () = assert!(std::mem::size_of::<ScanoutProfile>() == 64);
impl ScanoutProfile {
    pub fn empty() -> Self {
        Self { version: 1, size: 64, flags: 0, content_rotation: 0, storage_width: 0,
            storage_height: 0, logical_width: 0, logical_height: 0, endpoint_generation: 0,
            profile_generation: 0, reserved_tail: [0; 2] }
    }
    pub fn valid(self) -> bool {
        self.version == 1 && self.size == 64 && self.reserved_tail == [0; 2] &&
        (if self.flags == 0 {
            self.content_rotation == 0 && self.storage_width == 0 && self.storage_height == 0 &&
                self.logical_width == 0 && self.logical_height == 0
        } else {
            self.flags == 7 && self.endpoint_generation != 0 && self.profile_generation != 0 &&
                self.geometry(1, 1).valid()
        })
    }
    pub fn geometry(self, reset: u64, mode: u64) -> ScanoutGeometry {
        ScanoutGeometry { version: 1, size: 64, storage_width: self.storage_width,
            storage_height: self.storage_height, logical_width: self.logical_width,
            logical_height: self.logical_height, content_rotation: self.content_rotation,
            reserved: 0, reset_generation: reset, mode_generation: mode, reserved_tail: [0; 2] }
    }
    pub fn matches(self, geometry: ScanoutGeometry, endpoint: u64, profile: u64) -> bool {
        self.valid() && self.flags == 7 && self.endpoint_generation == endpoint &&
            self.profile_generation == profile && self.geometry(geometry.reset_generation,
                geometry.mode_generation) == geometry
    }
}

impl ScanoutGeometry {
    pub fn identity(width: u32, height: u32, reset: u64, mode: u64) -> Self {
        Self {
            version: 1,
            size: 64,
            storage_width: width,
            storage_height: height,
            logical_width: width,
            logical_height: height,
            content_rotation: 0,
            reserved: 0,
            reset_generation: reset,
            mode_generation: mode,
            reserved_tail: [0; 2],
        }
    }

    pub fn valid(self) -> bool {
        self.version == 1
            && self.size == 64
            && self.reserved == 0
            && self.reserved_tail == [0; 2]
            && self.reset_generation != 0
            && self.mode_generation != 0
            && self.content_rotation <= 3
            && [
                self.storage_width,
                self.storage_height,
                self.logical_width,
                self.logical_height,
            ]
            .into_iter()
            .all(|extent| (1..=8192).contains(&extent))
            && if self.content_rotation & 1 != 0 {
                (self.storage_width, self.storage_height)
                    == (self.logical_height, self.logical_width)
            } else {
                (self.storage_width, self.storage_height)
                    == (self.logical_width, self.logical_height)
            }
    }

    pub fn can_replace(self, previous: Self) -> bool {
        self.valid()
            && (self.reset_generation > previous.reset_generation
                || (self.reset_generation == previous.reset_generation
                    && (self.mode_generation > previous.mode_generation || self == previous)))
    }

    pub fn matches_ids(self, reset: u64, mode: u64) -> bool {
        self.reset_generation == reset && self.mode_generation == mode
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn authentic_extents_and_immutable_generation() {
        let mut g = ScanoutGeometry::identity(3040, 1904, 7, 1);
        assert!(g.valid());
        for rotation in 0..4 {
            g.content_rotation = rotation;
            (g.storage_width, g.storage_height) = if rotation & 1 != 0 {
                (1904, 3040)
            } else {
                (3040, 1904)
            };
            assert!(g.valid());
        }
        let old = g;
        g.content_rotation = 1;
        assert!(g.valid() && !g.can_replace(old));
        g.mode_generation += 1;
        assert!(g.can_replace(old) && !old.can_replace(g));
        g.reset_generation = 8;
        g.mode_generation = 1;
        assert!(g.can_replace(old) && !old.can_replace(g));
        g.storage_width = 3040;
        assert!(!g.valid());
        g = old;
        g.reserved_tail[1] = 1;
        assert!(!g.valid());
        g = old;
        g.mode_generation = 0;
        assert!(!g.valid());
    }
}
