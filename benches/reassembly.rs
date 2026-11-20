//! Criterion benches for fragmentation and reassembly (NET-24).

use std::hint::black_box;

use ao_network::NetworkConfig;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

#[path = "common/reassembly.rs"]
mod workload;

fn fragment(c: &mut Criterion) {
    let frame = workload::frame();
    let mut group = c.benchmark_group("fragment");
    group.throughput(Throughput::Bytes(frame.len() as u64));
    for mtu in workload::MTUS {
        group.bench_with_input(BenchmarkId::from_parameter(mtu), &mtu, |b, &mtu| {
            b.iter(|| workload::fragment(black_box(&frame), mtu))
        });
    }
    group.finish();
}

fn reassemble(c: &mut Criterion) {
    let config = NetworkConfig::default();
    let frame = workload::frame();
    let mut group = c.benchmark_group("reassemble");
    group.throughput(Throughput::Bytes(frame.len() as u64));
    for mtu in workload::MTUS {
        let chunks = workload::chunks(&frame, mtu);
        group.bench_with_input(BenchmarkId::from_parameter(mtu), &chunks, |b, chunks| {
            b.iter(|| workload::reassemble(&config, black_box(chunks)))
        });
    }
    group.finish();
}

criterion_group!(benches, fragment, reassemble);
criterion_main!(benches);
