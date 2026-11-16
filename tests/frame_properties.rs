//! Seeded properties for the frame codec through the public API.

use ao_network::{AppHello, Channel, Frame, FrameError, Hello, LocalTag, MAX_APP_HELLO};
use proptest::prelude::*;
use proptest::test_runner::{Config, MapFailurePersistence, RngSeed};

/// Receiver limit used where the size check is not under test.
const MAX: usize = 1024 * 1024;

fn config() -> Config {
    let mut config = Config::with_cases(256);
    config.rng_seed = RngSeed::Fixed(0xA004_0002);
    // In-memory persistence: Miri forbids the getcwd that file persistence
    // needs. The fixed seed replays every run; promote any minimal failing
    // input to a regression test.
    config.failure_persistence = Some(Box::new(MapFailurePersistence::default()));
    config
}

fn channel() -> impl Strategy<Value = Channel> {
    prop_oneof![
        Just(Channel::Control),
        Just(Channel::Message),
        Just(Channel::Telemetry),
        Just(Channel::Bulk),
    ]
}

fn hello() -> impl Strategy<Value = Hello> {
    (
        any::<u8>(),
        any::<u8>(),
        any::<u32>(),
        any::<u32>(),
        any::<[u8; 16]>(),
        prop::collection::vec(any::<u8>(), 0..=MAX_APP_HELLO),
    )
        .prop_map(|(proto_min, proto_max, caps, max_frame, tag, blob)| Hello {
            proto_min,
            proto_max,
            caps,
            max_frame,
            tag: LocalTag::new(tag),
            app_hello: AppHello::new(blob).expect("strategy stays within bound"),
        })
}

/// Every variant, with payloads long enough to need multi-byte lengths.
fn frame() -> impl Strategy<Value = Frame> {
    prop_oneof![
        hello().prop_map(Frame::Hello),
        (channel(), prop::collection::vec(any::<u8>(), 0..600))
            .prop_map(|(channel, payload)| Frame::Data { channel, payload }),
        Just(Frame::Ping),
        Just(Frame::Pong),
        Just(Frame::Bye),
        any::<u32>().prop_map(|credits| Frame::Credit { credits }),
    ]
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn every_frame_round_trips(frame in frame()) {
        let bytes = frame.encode();
        prop_assert_eq!(Frame::decode(&bytes, bytes.len()), Ok(frame));
    }

    #[test]
    fn every_strict_prefix_is_truncated(frame in frame(), cut in any::<prop::sample::Index>()) {
        let bytes = frame.encode();
        let len = cut.index(bytes.len());
        prop_assert_eq!(Frame::decode(&bytes[..len], MAX), Err(FrameError::Truncated));
    }

    #[test]
    fn appended_bytes_are_rejected(frame in frame(), extra in prop::collection::vec(any::<u8>(), 1..8)) {
        let mut bytes = frame.encode();
        bytes.extend_from_slice(&extra);
        prop_assert_eq!(
            Frame::decode(&bytes, MAX),
            Err(FrameError::TrailingBytes { extra: extra.len() })
        );
    }

    #[test]
    fn input_over_the_limit_is_oversize(frame in frame()) {
        let bytes = frame.encode();
        let max = bytes.len() - 1;
        prop_assert_eq!(
            Frame::decode(&bytes, max),
            Err(FrameError::Oversize { len: bytes.len(), max })
        );
    }

    #[test]
    fn hostile_bodies_never_panic_and_accepted_bytes_are_canonical(
        frame_type in 0u8..8,
        body in prop::collection::vec(any::<u8>(), 0..64),
    ) {
        // Version pinned so the body parsers, not the version check, see the
        // hostile bytes. A typed error is the expected outcome; a panic
        // fails the property.
        let mut input = vec![1, frame_type];
        input.extend_from_slice(&body);
        if let Ok(frame) = Frame::decode(&input, MAX) {
            prop_assert_eq!(frame.encode(), input);
        }
    }
}
