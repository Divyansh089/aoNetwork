//! Send queue scheduling, caps and refusal.

use std::collections::VecDeque;

use ao_network::{Channel, NetworkConfig, SendError, SendQueue};

/// Encoded `Data` frame size of a payload under 128 bytes.
fn cost(len: usize) -> usize {
    len + 4
}

#[test]
fn control_goes_ahead_of_queued_traffic() {
    let mut queue = SendQueue::new(&NetworkConfig::default());
    for byte in 0..10 {
        queue.push(Channel::Bulk, vec![byte; 100]).unwrap();
        queue.push(Channel::Message, vec![byte; 100]).unwrap();
    }
    // One frame is in flight when Control arrives; Control is next.
    assert!(queue.pop().is_some());
    queue.push(Channel::Control, vec![0xC0; 4]).unwrap();
    assert_eq!(queue.pop(), Some((Channel::Control, vec![0xC0; 4])));
}

#[test]
fn queue_full_returns_the_payload_unchanged() {
    let config = NetworkConfig::default()
        .with_channel_cap(Channel::Message, 1000)
        .unwrap();
    let mut queue = SendQueue::new(&config);
    queue.push(Channel::Message, vec![1; 900]).unwrap();
    let payload: Vec<u8> = (0..=255).collect();
    assert_eq!(
        queue.push(Channel::Message, payload.clone()),
        Err(SendError::QueueFull(payload))
    );
    // The refusal changed nothing, and other channels are unaffected.
    assert_eq!(queue.queued_bytes(Channel::Message), 900 + 5);
    queue.push(Channel::Bulk, vec![2; 900]).unwrap();
}

#[test]
fn empty_payloads_still_use_up_the_cap() {
    let config = NetworkConfig::default()
        .with_channel_cap(Channel::Telemetry, 64)
        .unwrap();
    let mut queue = SendQueue::new(&config);
    for _ in 0..64 / cost(0) {
        queue.push(Channel::Telemetry, Vec::new()).unwrap();
    }
    assert_eq!(
        queue.push(Channel::Telemetry, Vec::new()),
        Err(SendError::QueueFull(Vec::new()))
    );
}

#[test]
fn busy_channels_share_bytes_by_weight() {
    let mut queue = SendQueue::new(&NetworkConfig::default());
    for channel in [Channel::Message, Channel::Telemetry, Channel::Bulk] {
        while queue.push(channel, vec![0; 96]).is_ok() {}
    }
    // Seven rounds of 7 KiB; every channel still has frames queued after.
    let mut sent = [0usize; 3];
    let mut total = 0;
    while total < 7 * 7 * 1024 {
        let (channel, payload) = queue.pop().unwrap();
        let i = match channel {
            Channel::Message => 0,
            Channel::Telemetry => 1,
            Channel::Bulk => 2,
            Channel::Control => unreachable!(),
        };
        sent[i] += cost(payload.len());
        total += cost(payload.len());
    }
    // Within one frame of 4 : 2 : 1 per round.
    for (i, weight) in [4, 2, 1].into_iter().enumerate() {
        let share = 7 * weight * 1024;
        assert!(sent[i].abs_diff(share) <= 2 * cost(96), "{sent:?}");
    }
}

/// SplitMix64, so the flood is reproducible from `SEED`.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

#[test]
fn caps_hold_under_a_hostile_flood() {
    const SEED: u64 = 0x5EED_0011;
    let config = NetworkConfig::default()
        .with_max_frame(4096)
        .unwrap()
        .with_channel_cap(Channel::Control, 1024)
        .unwrap()
        .with_channel_cap(Channel::Message, 4096)
        .unwrap()
        .with_channel_cap(Channel::Telemetry, 2048)
        .unwrap()
        .with_channel_cap(Channel::Bulk, 8192)
        .unwrap();
    let channels = [
        Channel::Control,
        Channel::Message,
        Channel::Telemetry,
        Channel::Bulk,
    ];
    let mut rng = Rng(SEED);
    let mut queue = SendQueue::new(&config);
    let mut expected: [VecDeque<Vec<u8>>; 4] = Default::default();
    let mut refused = 0;
    for step in 0..50_000u32 {
        // Pushes outnumber pops nine to one, so every channel stays full.
        if rng.below(10) == 0 {
            if let Some((channel, payload)) = queue.pop() {
                let i = channels.iter().position(|c| *c == channel).unwrap();
                assert_eq!(expected[i].pop_front(), Some(payload), "seed {SEED:#x}");
            }
        } else {
            let i = rng.below(4);
            let payload = vec![step as u8; rng.below(3000)];
            match queue.push(channels[i], payload.clone()) {
                Ok(()) => expected[i].push_back(payload),
                Err(err) => {
                    assert_eq!(err.into_payload(), payload, "seed {SEED:#x}");
                    refused += 1;
                }
            }
        }
        for channel in channels {
            assert!(
                queue.queued_bytes(channel) <= config.channel_cap(channel),
                "seed {SEED:#x}, step {step}, {channel:?}"
            );
        }
    }
    assert!(refused > 10_000, "the flood never filled the queue");
}
