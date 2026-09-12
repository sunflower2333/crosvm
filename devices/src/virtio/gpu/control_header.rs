// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license.

//! Ordinary virtio GPU framing, shared by production codecs and wire fixtures.
#![allow(non_camel_case_types)]
use data_model::{Le32, Le64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

#[derive(Copy, Clone, Debug, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
#[repr(C)]
pub struct virtio_gpu_ctrl_hdr {
    pub type_: Le32,
    pub flags: Le32,
    pub fence_id: Le64,
    pub ctx_id: Le32,
    pub ring_idx: u8,
    pub padding: [u8; 3],
}

const _: () = assert!(std::mem::size_of::<virtio_gpu_ctrl_hdr>() == 24);
