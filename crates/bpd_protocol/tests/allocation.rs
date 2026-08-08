//! the framing's allocation behaviour, asserted exactly
//!
//! this is the performance gate that works. wall-clock on a shared runner
//! varies by more than the effects worth catching, so CI cannot assert on it —
//! but an allocation count is the same number on every machine, every run
//!
//! what it protects is a documented design property: `read_frame_into` takes
//! the caller's buffer so a long lived reader reuses one allocation. that claim
//! is either true or it is a comment, and the difference is this file

use bpd_protocol::frame::{read_frame_into, write_frame};
use bpd_test::alloc::measure;

#[global_allocator]
static ALLOCATOR: bpd_test::alloc::Counting = bpd_test::alloc::Counting;

const PAYLOAD: &[u8] = &[0xa5; 4096];
const TOKEN: [u8; bpd_protocol::TOKEN_LEN] = [7; bpd_protocol::TOKEN_LEN];

fn framed(payload: &[u8]) -> Vec<u8> {
    let mut wire = Vec::new();
    write_frame(&mut wire, payload).expect("writing to a vec cannot fail");
    wire
}

#[test]
fn the_counting_allocator_is_installed() {
    // a test binary that forgot the `#[global_allocator]` line reports zero for
    // everything, which is exactly what every assertion below is looking for.
    // without this check they would all pass while measuring nothing
    let (allocated, allocations) = measure(|| Vec::<u8>::with_capacity(4096));
    drop(allocated);

    allocations.assert_measured();
}

#[test]
fn writing_into_a_prepared_buffer_does_not_allocate() {
    let mut wire = Vec::with_capacity(PAYLOAD.len() + 8);

    // the first write is what sizes nothing — the capacity is already there
    let (result, allocations) = measure(|| write_frame(&mut wire, PAYLOAD));
    result.expect("writing to a vec cannot fail");

    assert_eq!(
        allocations.count, 0,
        "framing a payload allocated {} time(s) for {} bytes",
        allocations.count, allocations.bytes
    );
}

#[test]
fn a_reused_read_buffer_settles_at_zero_allocations() {
    let wire = framed(PAYLOAD);
    let mut buffer = Vec::new();

    // the first read has to size the buffer, so it is allowed to allocate
    let (present, _) = measure(|| read_frame_into(&mut wire.as_slice(), &mut buffer));
    assert!(present.expect("the frame is whole"));

    // every read after it must not, which is the whole reason the caller owns
    // the buffer rather than receiving a fresh `Vec` per frame
    for _ in 0..8 {
        let (present, allocations) = measure(|| read_frame_into(&mut wire.as_slice(), &mut buffer));
        assert!(present.expect("the frame is whole"));
        assert_eq!(
            allocations.count, 0,
            "re-reading into a warm buffer allocated {} time(s)",
            allocations.count
        );
    }
}

#[test]
fn a_growing_payload_allocates_only_when_it_outgrows_the_buffer() {
    let small = framed(&PAYLOAD[..64]);
    let large = framed(PAYLOAD);
    let mut buffer = Vec::new();

    let (_, first) = measure(|| read_frame_into(&mut small.as_slice(), &mut buffer));
    first.assert_measured();

    // growing past the capacity is the one case that may allocate, and it is
    // bounded: once, for the larger frame
    let (_, grown) = measure(|| read_frame_into(&mut large.as_slice(), &mut buffer));
    assert!(
        grown.count <= 1,
        "growing the buffer allocated {} time(s), expected at most one",
        grown.count
    );

    // and shrinking back must not, because the capacity is already there
    let (_, shrunk) = measure(|| read_frame_into(&mut small.as_slice(), &mut buffer));
    assert_eq!(shrunk.count, 0);
}

#[test]
fn a_refused_frame_does_not_allocate_what_it_refused() {
    // the length prefix is attacker shaped even from a trusted peer: a
    // desynchronised stream produces an arbitrary number. the bound has to be
    // checked before the allocation, not after
    let mut wire = (bpd_protocol::MAX_FRAME_LEN + 1).to_le_bytes().to_vec();
    wire.extend_from_slice(b"nowhere near that long");
    let mut buffer = Vec::new();

    let (refused, allocations) = measure(|| read_frame_into(&mut wire.as_slice(), &mut buffer));
    refused.expect_err("the announced length is over the limit");

    assert_eq!(
        allocations.count, 0,
        "refusing an oversized frame allocated {} time(s) for {} bytes",
        allocations.count, allocations.bytes
    );
}

#[test]
fn the_handshake_does_not_allocate() {
    let mut wire = Vec::with_capacity(64);

    let ((), allocations) = measure(|| {
        bpd_protocol::frame::write_handshake(&mut wire, &TOKEN)
            .expect("writing to a vec cannot fail");
        bpd_protocol::frame::read_handshake(&mut wire.as_slice(), &TOKEN)
            .expect("this build agrees with itself");
    });

    assert_eq!(allocations.count, 0);
}
