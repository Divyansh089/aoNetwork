//! Criterion benches for the beacon codec (README, Development).

use std::hint::black_box;

use ao_network::{Beacon, BeaconFlags};
use criterion::{criterion_group, criterion_main};

fn sample() -> Beacon {
    Beacon {
        flags: BeaconFlags {
            accepting: true,
            relay_willing: true,
            low_battery: false,
        },
        degree: 3,
        max_links: 5,
        hint: [0xA5; 8],
        digest: [0x5A; 4],
    }
}

fn beacon(c: &mut criterion::Criterion) {
    let beacon = sample();
    c.bench_function("beacon_encode", |b| b.iter(|| black_box(&beacon).encode()));

    let bytes = beacon.encode();
    c.bench_function("beacon_decode", |b| {
        b.iter(|| Beacon::decode(black_box(&bytes)).expect("valid beacon"))
    });
}

criterion_group!(benches, beacon);
criterion_main!(benches);
