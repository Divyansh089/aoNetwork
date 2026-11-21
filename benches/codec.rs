//! Criterion benches for the frame and beacon codecs (NET-24).

use std::hint::black_box;

use ao_network::{Beacon, Frame};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

#[path = "common/codec.rs"]
mod workload;

fn beacon(c: &mut Criterion) {
    let beacon = workload::beacon();
    c.bench_function("beacon_encode", |b| b.iter(|| black_box(&beacon).encode()));

    let bytes = beacon.encode();
    c.bench_function("beacon_decode", |b| {
        b.iter(|| Beacon::decode(black_box(&bytes)).expect("valid beacon"))
    });
}

fn data(c: &mut Criterion) {
    let max_frame = workload::max_frame();
    let mut group = c.benchmark_group("frame_data");
    for len in workload::DATA_LENS {
        let frame = workload::data(len);
        let bytes = frame.encode();
        group.throughput(Throughput::Bytes(bytes.len() as u64));
        group.bench_with_input(BenchmarkId::new("encode", len), &frame, |b, frame| {
            b.iter(|| black_box(frame).encode())
        });
        group.bench_with_input(BenchmarkId::new("decode", len), &bytes, |b, bytes| {
            b.iter(|| Frame::decode(black_box(bytes), max_frame).expect("valid frame"))
        });
    }
    group.finish();
}

fn hello(c: &mut Criterion) {
    let frame = workload::hello();
    let max_frame = workload::max_frame();
    let bytes = frame.encode();
    c.bench_function("hello_encode", |b| b.iter(|| black_box(&frame).encode()));
    c.bench_function("hello_decode", |b| {
        b.iter(|| Frame::decode(black_box(&bytes), max_frame).expect("valid frame"))
    });
}

criterion_group!(benches, beacon, data, hello);
criterion_main!(benches);
