//! `RingReader::read` — the only piece of the pipeline with a hard deadline.
//!
//! Everything else in `audio/` runs on the pump thread, which has no deadline and
//! only ever starves its own ring. This runs on the voice mixer's thread, once per
//! 20ms per playing player, ahead of Opus encoding and packet send. Time spent here
//! is time the mixer does not have, and it is multiplied by the number of players
//! the node is carrying — so it bounds concurrent-player capacity more directly
//! than the decode side does.

use std::io::Read as _;
use std::sync::atomic::AtomicI64;
use std::sync::Arc;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use lavalink_server::audio::ring::{self, FrameCounters, RingReader, RingWriter, CHANNELS};

mod common;
use common::samples;

/// 20ms of 48kHz stereo: 960 frames, 1920 samples, 3840 bytes. This is the buffer
/// size songbird's mixer asks for, so it is the only read size worth measuring.
const FRAME_SAMPLES: usize = 960 * CHANNELS;
const FRAME_BYTES: usize = FRAME_SAMPLES * 4;

/// A ring sized like a real one (the default `frameBufferDurationMs`), plus its two
/// ends. `position` and the counters are kept alive by the returned pair.
fn ring_pair(buffer_ms: u32) -> (RingWriter, RingReader) {
    ring::channel(
        buffer_ms,
        Arc::new(AtomicI64::new(0)),
        Arc::new(FrameCounters::default()),
    )
}

fn bench_ring_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("ring");
    group.throughput(Throughput::Elements(FRAME_SAMPLES as u64));

    let mut out = vec![0u8; FRAME_BYTES];

    // The steady state: the pump is comfortably ahead, so every read is a full
    // frame straight out of the buffer. This is the number that matters.
    //
    // Refilling happens inside the timed closure because it has to — a read drains
    // the ring, and topping it up outside would need `iter_batched`, whose per-batch
    // setup is far more expensive than the write itself. The write is a single
    // `extend` of the same length the read takes, so it is a constant addend, not a
    // term that changes when the read path does.
    group.bench_function(BenchmarkId::new("read", "20ms_frame"), |b| {
        let (writer, mut reader) = ring_pair(1000);
        let chunk = samples(FRAME_SAMPLES);
        b.iter(|| {
            writer.try_write(&chunk);
            black_box(reader.read(black_box(&mut out)).unwrap())
        });
    });

    // The same read, but with the deque's head parked mid-buffer so `as_slices`
    // reports two segments instead of one. A ring that has been playing for any
    // length of time is always in this state; a freshly filled one may not be, and
    // benchmarking only the contiguous case would miss the split entirely.
    group.bench_function(BenchmarkId::new("read", "20ms_frame_wrapped"), |b| {
        let (writer, mut reader) = ring_pair(1000);
        let chunk = samples(FRAME_SAMPLES);
        // Push the head past the halfway mark: fill, drain most of it, and let the
        // steady-state write/read cycle below keep it straddling the wrap point.
        let capacity_samples = 48_000 * CHANNELS;
        writer.try_write(&samples(capacity_samples));
        for _ in 0..(capacity_samples / FRAME_SAMPLES / 2) {
            // Full frames, or the head did not end up where this case needs it.
            assert_eq!(reader.read(&mut out).unwrap(), FRAME_BYTES);
        }
        b.iter(|| {
            writer.try_write(&chunk);
            black_box(reader.read(black_box(&mut out)).unwrap())
        });
    });

    // The starved path: the pump has fallen behind, the buffer is empty, and the
    // reader hands back silence and counts a nulled frame. Cheap by construction,
    // but it is on the same deadline and a node under load takes it often enough
    // that a regression here would show up as stutter.
    group.bench_function(BenchmarkId::new("read", "starved"), |b| {
        let (_writer, mut reader) = ring_pair(1000);
        b.iter(|| black_box(reader.read(black_box(&mut out)).unwrap()));
    });

    group.finish();
}

criterion_group!(benches, bench_ring_read);
criterion_main!(benches);
