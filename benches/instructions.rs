//! Instruction counts of the bench workloads (NET-24).
//!
//! Wall-clock time on a shared CI runner is too noisy to gate on, so CI
//! runs each workload here under Callgrind for the base and the head of a
//! pull request and compares the instructions executed inside `measure`.
//! The counts are exact, so CI can hold every workload to a tight limit.
//!
//! `--list` prints the workloads; each other argument names one to run.
//! Plain `cargo bench` skips this target: it measures nothing on its own.

use std::hint::black_box;

use ao_network::{Beacon, Frame, NetworkConfig};

#[path = "common/codec.rs"]
mod codec;
#[path = "common/queue.rs"]
mod queue;
#[path = "common/reassembly.rs"]
mod reassembly;

/// Inputs per workload, so even the shortest one runs enough instructions
/// to compare.
const REPS: usize = 64;

/// Runs `work` on each input. Callgrind counts only inside this function
/// (`--toggle-collect`), so building the inputs is left out.
#[inline(never)]
fn measure<T, R>(inputs: Vec<T>, work: impl Fn(T) -> R) {
    for input in inputs {
        black_box(work(black_box(input)));
    }
}

fn reps<T: Clone>(input: T) -> Vec<T> {
    vec![input; REPS]
}

type Workload = (String, Box<dyn Fn()>);

fn workload(name: impl Into<String>, run: impl Fn() + 'static) -> Workload {
    (name.into(), Box::new(run))
}

fn workloads() -> Vec<Workload> {
    let mut all = vec![
        workload("beacon_encode", || {
            measure(reps(codec::beacon()), |beacon| beacon.encode())
        }),
        workload("beacon_decode", || {
            let bytes = codec::beacon().encode();
            measure(reps(bytes), |bytes| Beacon::decode(&bytes))
        }),
        workload("hello_encode", || {
            measure(reps(codec::hello()), |frame| frame.encode())
        }),
        workload("hello_decode", || {
            let bytes = codec::hello().encode();
            measure(reps(bytes.as_slice()), |bytes| {
                Frame::decode(bytes, codec::max_frame())
            })
        }),
        workload("queue_coalesce", || {
            let config = NetworkConfig::default();
            measure(reps(queue::updates()), |updates| {
                queue::coalesce(&config, updates)
            })
        }),
    ];
    for len in codec::DATA_LENS {
        all.push(workload(format!("frame_data_encode_{len}"), move || {
            measure(reps(codec::data(len)), |frame| frame.encode())
        }));
        all.push(workload(format!("frame_data_decode_{len}"), move || {
            let bytes = codec::data(len).encode();
            measure(reps(bytes.as_slice()), |bytes| {
                Frame::decode(bytes, codec::max_frame())
            })
        }));
    }
    for len in queue::FILL_LENS {
        all.push(workload(format!("queue_fill_and_drain_{len}"), move || {
            let config = NetworkConfig::default();
            let batch = queue::payloads(&config, len);
            measure(reps(batch), |batch| queue::fill_and_drain(&config, batch))
        }));
    }
    for mtu in reassembly::MTUS {
        all.push(workload(format!("fragment_{mtu}"), move || {
            let frame = reassembly::frame();
            measure(reps(frame.as_slice()), |frame| {
                reassembly::fragment(frame, mtu)
            })
        }));
        all.push(workload(format!("reassemble_{mtu}"), move || {
            let config = NetworkConfig::default();
            let chunks = reassembly::chunks(&reassembly::frame(), mtu);
            measure(reps(chunks.as_slice()), |chunks| {
                reassembly::reassemble(&config, chunks)
            })
        }));
    }
    all
}

fn main() {
    let workloads = workloads();
    // Cargo passes `--bench`; options other than `--list` are not ours.
    let mut ran = false;
    for arg in std::env::args().skip(1) {
        if arg == "--list" {
            for (name, _) in &workloads {
                println!("{name}");
            }
            return;
        }
        if arg.starts_with("--") {
            continue;
        }
        let Some((_, run)) = workloads.iter().find(|(name, _)| *name == arg) else {
            eprintln!("no workload named {arg}; --list prints them");
            std::process::exit(2);
        };
        run();
        ran = true;
    }
    if !ran {
        eprintln!("name a workload to run, or pass --list");
        std::process::exit(2);
    }
}
