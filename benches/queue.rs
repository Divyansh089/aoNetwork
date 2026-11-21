//! Criterion benches for the send queue (NET-24).

use std::hint::black_box;

use ao_network::NetworkConfig;
use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};

#[path = "common/queue.rs"]
mod workload;

fn fill_and_drain(c: &mut Criterion) {
    let config = NetworkConfig::default();
    let mut group = c.benchmark_group("queue_fill_and_drain");
    for len in workload::FILL_LENS {
        let batch = workload::payloads(&config, len);
        group.bench_with_input(BenchmarkId::from_parameter(len), &batch, |b, batch| {
            b.iter_batched(
                || batch.clone(),
                |batch| workload::fill_and_drain(&config, batch),
                BatchSize::SmallInput,
            )
        });
    }
    group.finish();
}

fn coalesce(c: &mut Criterion) {
    let config = NetworkConfig::default();
    c.bench_function("queue_coalesce", |b| {
        b.iter_batched(
            workload::updates,
            |updates| black_box(workload::coalesce(&config, updates)),
            BatchSize::SmallInput,
        )
    });
}

criterion_group!(benches, fill_and_drain, coalesce);
criterion_main!(benches);
