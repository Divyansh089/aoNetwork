//! Send queue workloads, shared by `queue.rs` and `instructions.rs`.

use std::hint::black_box;
use std::time::Duration;

use ao_network::{Channel, NetworkConfig, SendOpts, SendQueue};

const CHANNELS: [Channel; 4] = [
    Channel::Control,
    Channel::Message,
    Channel::Telemetry,
    Channel::Bulk,
];

/// Payload sizes for filling the queue: small, medium and large.
pub const FILL_LENS: [usize; 3] = [16, 256, 4096];

/// Payloads of `len` bytes over every channel, as many as fit.
pub fn payloads(config: &NetworkConfig, len: usize) -> Vec<(Channel, Vec<u8>)> {
    let mut queue = SendQueue::new(config);
    let mut out = Vec::new();
    for channel in CHANNELS {
        loop {
            let before = queue.queued_bytes(channel);
            // Telemetry evicts rather than refusing; stop once it is full.
            if queue.push(channel, vec![0; len]).is_err() || queue.queued_bytes(channel) <= before {
                break;
            }
            out.push((channel, vec![0xC4; len]));
        }
    }
    out
}

/// Fills every channel, then drains it in scheduling order: admission,
/// strict priority and deficit round-robin.
pub fn fill_and_drain(config: &NetworkConfig, batch: Vec<(Channel, Vec<u8>)>) {
    let mut queue = SendQueue::new(config);
    for (channel, payload) in batch {
        queue.push(channel, payload).expect("fits");
    }
    while let Some(next) = queue.pop(Duration::ZERO) {
        black_box(next);
    }
}

/// 256 latest-wins updates.
pub fn updates() -> Vec<Vec<u8>> {
    (0..=255u8).map(|i| vec![i; 32]).collect()
}

/// Admits `updates` under eight keys, each replacing the one still waiting.
pub fn coalesce(config: &NetworkConfig, updates: Vec<Vec<u8>>) -> usize {
    let mut queue = SendQueue::new(config);
    for payload in updates {
        let opts = SendOpts {
            coalesce_key: Some([payload[0] % 8; 16]),
            expires_at: None,
        };
        queue
            .push_opts(Channel::Telemetry, opts, payload)
            .expect("fits");
    }
    queue.queued_bytes(Channel::Telemetry)
}
