//! Executes the actual production factory/encoder, without a fake wire type.
#![allow(dead_code)]
extern crate self as vm_control;
#[path = "../../../../vm_control/src/shared_allocation.rs"]
pub mod shared_allocation;
#[path = "../../../src/virtio/gpu/control_header.rs"]
mod control_header;
#[path = "../../../src/virtio/gpu/shared_allocation_protocol.rs"]
mod shared_allocation_protocol;

use control_header::virtio_gpu_ctrl_hdr;
use shared_allocation::DynamicMappingWindow;
use shared_allocation_protocol::*;

fn main() -> std::io::Result<()> {
    let output = std::path::PathBuf::from(std::env::args_os().nth(1).expect("output directory"));
    std::fs::create_dir_all(&output)?;
    let cases = [
        ("absent", None, 0, 0, 0),
        ("bar8m_reserved8m_align16k", Some(DynamicMappingWindow {
            bar_size: 8 << 20, reserved_prefix: 8 << 20, alignment: 16384 }), 17, 3040, 1904),
        ("bar128m_reserved8m_align4k", Some(DynamicMappingWindow {
            bar_size: 128 << 20, reserved_prefix: 8 << 20, alignment: 4096 }), 17, 3040, 1904),
        ("bar128m_reserved8m_align16k", Some(DynamicMappingWindow {
            bar_size: 128 << 20, reserved_prefix: 8 << 20, alignment: 16384 }), 17, 3040, 1904),
    ];
    for (name, window, generation, width, height) in cases {
        let mut request = SharedAllocationHeader::default();
        request.hdr.type_ = CMD_DISCOVER_SHARED_ALLOCATION.into();
        request.magic = SHARED_ALLOCATION_MAGIC.into();
        request.version = 1.into(); request.size = 64.into();
        assert!(request.valid_discovery());
        let response = SharedAllocationDiscovery::from_host(request, window, generation, width, height);
        assert_eq!(response.feature_bits.to_native(), 0);
        assert_eq!(response.max_allocations.to_native(), 0);
        let mut bytes = Vec::new();
        assert_eq!(response.encode(virtio_gpu_ctrl_hdr::default(), &mut bytes)?, 128);
        assert_eq!(bytes.len(), 128);
        std::fs::write(output.join(format!("{name}.bin")), &bytes)?;
        println!("{name}: generation={generation} width={width} height={height} window={window:?} unavailable={:#x} triple_lower_bound={} feature_bits=0 max_allocations=0 bytes=128",
            response.unavailable_reasons.to_native(), response.triple_bytes_lower_bound.to_native());
    }
    Ok(())
}
