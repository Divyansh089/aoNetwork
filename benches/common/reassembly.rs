//! Fragmentation workloads, shared by `reassembly.rs` and `instructions.rs`.

use std::time::Duration;

use ao_network::{Fragmenter, MAX_MTU, MIN_MTU, NetworkConfig, Reassembler};

/// The BLE minimum, a common Android value and the maximum.
pub const MTUS: [u16; 3] = [MIN_MTU, 247, MAX_MTU];

/// One largest default frame.
pub fn frame() -> Vec<u8> {
    vec![0xF1; NetworkConfig::default().max_frame()]
}

/// `frame` split into chunks for `mtu`.
pub fn chunks(frame: &[u8], mtu: u16) -> Vec<Vec<u8>> {
    Fragmenter::new(frame, mtu).expect("valid MTU").collect()
}

/// Splits `frame` for `mtu`, returning the chunk count.
pub fn fragment(frame: &[u8], mtu: u16) -> usize {
    Fragmenter::new(frame, mtu).expect("valid MTU").count()
}

/// Joins `chunks` back into a frame.
pub fn reassemble(config: &NetworkConfig, chunks: &[Vec<u8>]) -> Option<Vec<u8>> {
    let mut reassembler = Reassembler::new(config);
    let mut out = None;
    for chunk in chunks {
        out = reassembler
            .push(Duration::ZERO, chunk)
            .expect("valid chunk");
    }
    out
}
