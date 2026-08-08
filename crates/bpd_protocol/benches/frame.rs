//! the framing cost on the control plane
//!
//! this is the baseline the message encoding decision needs. "json is fast
//! enough" and "we need a binary encoding" are both claims about how much of a
//! round trip is framing and how much is serialisation, and neither is worth
//! arguing without a number for the first half
//!
//! the sizes span what the control plane actually carries: an empty
//! acknowledgement, a stop event, a stack, and an object graph at the default
//! budget

// `criterion_group!` generates an undocumented public function, and a bench
// target has no public api for `missing_docs` to be protecting
#![allow(missing_docs)]

use std::hint::black_box;

use bpd_protocol::frame::{read_frame_into, write_frame};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

const SIZES: &[usize] = &[0, 256, 4 * 1024, 256 * 1024];

fn round_trip(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("frame/round_trip");

    for &size in SIZES {
        let payload = vec![0xa5; size];
        let mut wire = Vec::with_capacity(size + 4);
        let mut buffer = Vec::with_capacity(size);

        group.throughput(Throughput::Bytes(
            u64::try_from(size).expect("a bench size fits in a u64"),
        ));
        group.bench_with_input(
            BenchmarkId::from_parameter(size),
            &payload,
            |bencher, payload| {
                bencher.iter(|| {
                    wire.clear();
                    write_frame(&mut wire, black_box(payload)).expect("a vec never fails to write");
                    let present = read_frame_into(&mut wire.as_slice(), &mut buffer)
                        .expect("the frame just written is whole");
                    black_box((present, buffer.len()))
                });
            },
        );
    }

    group.finish();
}

fn handshake(criterion: &mut Criterion) {
    // paid once per session, so it is here to catch it becoming something other
    // than eight bytes rather than because it is hot
    criterion.bench_function("frame/handshake", |bencher| {
        let mut wire = Vec::with_capacity(8);
        bencher.iter(|| {
            wire.clear();
            bpd_protocol::frame::write_handshake(&mut wire).expect("a vec never fails to write");
            bpd_protocol::frame::read_handshake(&mut wire.as_slice())
                .expect("this build agrees with itself");
            black_box(wire.len())
        });
    });
}

criterion_group!(benches, round_trip, handshake);
criterion_main!(benches);
