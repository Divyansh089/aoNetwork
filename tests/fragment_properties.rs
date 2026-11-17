//! Seeded properties for fragmentation and reassembly.

use std::time::Duration;

use ao_network::{Fragmenter, MAX_MTU, MIN_MTU, NetworkConfig, Reassembler};
use proptest::prelude::*;
use proptest::test_runner::{Config, MapFailurePersistence, RngSeed};

fn config(cases: u32) -> Config {
    let mut config = Config::with_cases(cases);
    config.rng_seed = RngSeed::Fixed(0xA005_0001);
    // In-memory persistence: Miri forbids the getcwd that file persistence
    // needs. The fixed seed replays every run.
    config.failure_persistence = Some(Box::new(MapFailurePersistence::default()));
    config
}

/// Each case walks all 495 MTUs; Miri runs a token number of them.
const EVERY_MTU_CASES: u32 = if cfg!(miri) { 1 } else { 32 };

proptest! {
    #![proptest_config(config(EVERY_MTU_CASES))]

    #[test]
    fn any_frame_survives_every_mtu(frame in prop::collection::vec(any::<u8>(), 1..2048)) {
        for mtu in MIN_MTU..=MAX_MTU {
            let cap = usize::from(mtu) - 3;
            let mut reassembler = Reassembler::new(&NetworkConfig::default());
            let chunks: Vec<Vec<u8>> = Fragmenter::new(&frame, mtu).unwrap().collect();
            let (last, full) = chunks.split_last().unwrap();
            prop_assert!(full.iter().all(|c| c.len() == cap), "mtu {}", mtu);
            prop_assert!(last.len() <= cap, "mtu {}", mtu);
            for chunk in full {
                prop_assert_eq!(reassembler.push(Duration::ZERO, chunk), Ok(None));
            }
            prop_assert_eq!(
                reassembler.push(Duration::ZERO, last),
                Ok(Some(frame.clone())),
                "mtu {}", mtu
            );
        }
    }
}

proptest! {
    #![proptest_config(config(256))]

    #[test]
    fn hostile_chunks_never_panic_or_exceed_max_frame(
        chunks in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..48), 0..32),
    ) {
        let config = NetworkConfig::default().with_max_frame(64).unwrap();
        let mut reassembler = Reassembler::new(&config);
        for chunk in &chunks {
            if let Ok(Some(frame)) = reassembler.push(Duration::ZERO, chunk) {
                prop_assert!(!frame.is_empty() && frame.len() <= 64);
            }
        }
    }
}
