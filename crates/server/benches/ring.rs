//! RingReader::read — the only piece of the pipeline with a hard deadline.
//!
//! Everything else in audio/ runs on the pump thread, which has no deadline and
//! only ever starves its own ring. This runs on the voice mixer's thread, once per
//! 20ms per playing player, ahead of Opus encoding and packet send. Time spent here
//! is time the mixer does not have, and it is multiplied by the number of players
//! the node is carrying — so it bounds concurrent-player capacity more directly
//! than the decode side does.

use std::hint::black_box;
use std::io::Read as _;
use std::sync::atomic::AtomicI64;
use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use lavalink_server::audio::ring::{self, FrameCounters, RingReader, RingWriter, CHANNELS};

mod common;
use common::samples;

/// 20ms of 48kHz stereo: 960 frames, 1920 samples, 3840 bytes. This is the buffer
/// size songbird's mixer asks for, so it is the only read size worth measuring.
const FRAME_SAMPLES: usize = 960 * CHANNELS;
const FRAME_BYTES: usize = FRAME_SAMPLES * 4;

/// A ring sized like a real one (the default frameBufferDurationMs), plus its two
/// ends. position and the counters are kept alive by the returned pair.
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
    // Named "write_and_read", not "read": the write is timed too, since a read
    // drains the ring and topping it back up outside the timed closure would need
    // iter_batched, whose per-batch setup cost isn't free either — see
    // "read_only"/"20ms_frame" below for that measurement taken properly, with the
    // refill excluded. Kept here as well because it is the shape every other read
    // in this file times too, and it is cheap to keep both.
    group.bench_function(BenchmarkId::new("write_and_read", "20ms_frame"), |b| {
        let (writer, mut reader) = ring_pair(1000);
        let chunk = samples(FRAME_SAMPLES);
        b.iter(|| {
            let (written, _closed) = writer.try_write(&chunk);
            // Release builds compile this out; it exists so a broken invariant
            // (the ring silently short of room) fails a debug/test run instead of
            // quietly reading less than a real frame forever after.
            debug_assert_eq!(written, chunk.len(), "ring had no room for a full frame");
            black_box(reader.read(black_box(&mut out)).unwrap())
        });
    });

    // The same read, but with the deque's head parked so as_slices reports two
    // segments instead of one and the read has to cross both. A ring that has
    // been playing for any length of time is in this state some of the time; a
    // freshly filled one never is, so benchmarking only the contiguous case would
    // miss the split entirely.
    //
    // Sized at exactly two frames, not "like a real one" — capacity only controls
    // how often the head wraps, and a real 1000ms ring (50 frames) wraps once
    // every 50 write/read cycles, which is too rare to land reliably inside a
    // benchmark sample. At 2*FRAME_SAMPLES the wrap recurs every other iteration
    // by construction: draining to half a frame short of the physical end (below)
    // puts the head at capacity - FRAME_SAMPLES/2; the next write lands at the
    // (wrapped) low end, so the read after it draws FRAME_SAMPLES/2 from the
    // front segment and the rest from the back one — verified once, non-timed,
    // right after setup rather than inside the closure, since neither as_slices
    // nor the deque's head are reachable from outside the crate to assert on
    // directly. The iteration after that lands contiguous again (the head has
    // moved past the wrap), then the cycle repeats.
    const WRAP_CAPACITY_SAMPLES: usize = FRAME_SAMPLES * 2;
    // capacity_frames(2*960) * 1000 / SAMPLE_RATE(48_000).
    const WRAP_BUFFER_MS: u32 = 40;
    group.bench_function(BenchmarkId::new("write_and_read", "20ms_frame_wrapped"), |b| {
        let (writer, mut reader) = ring_pair(WRAP_BUFFER_MS);
        let chunk = samples(FRAME_SAMPLES);

        let (written, _closed) = writer.try_write(&samples(WRAP_CAPACITY_SAMPLES));
        assert_eq!(written, WRAP_CAPACITY_SAMPLES, "ring did not start empty");
        let mut drain = vec![0u8; (WRAP_CAPACITY_SAMPLES - FRAME_SAMPLES / 2) * 4];
        assert_eq!(
            reader.read(&mut drain).unwrap(),
            drain.len(),
            "the ring did not have a full drain's worth buffered"
        );

        b.iter(|| {
            let (written, _closed) = writer.try_write(&chunk);
            debug_assert_eq!(written, chunk.len(), "ring had no room for a full frame");
            black_box(reader.read(black_box(&mut out)).unwrap())
        });
    });

    // The read alone, refill excluded from the measurement. Unlike the two benches
    // above, the ring is topped back up outside the timed region — in batches, not
    // once per iteration, since a single top-up only buys capacity worth of
    // reads (50 at the default 1000ms) and criterion samples run for far more
    // iterations than that. iter_custom makes the batching explicit: refill,
    // time a batch of reads straight out of the buffer, repeat.
    group.bench_function(BenchmarkId::new("read_only", "20ms_frame"), |b| {
        b.iter_custom(|iters| {
            let (writer, mut reader) = ring_pair(1000);
            // Comfortably under the 1000ms/50-frame capacity, so every refill
            // starts from a ring already drained empty by the batch before it.
            const BATCH_FRAMES: u64 = 40;
            let refill = samples(FRAME_SAMPLES * BATCH_FRAMES as usize);
            let mut out = vec![0u8; FRAME_BYTES];
            let mut total = Duration::ZERO;
            let mut remaining = iters;
            while remaining > 0 {
                let batch = remaining.min(BATCH_FRAMES);
                let (written, _closed) = writer.try_write(&refill[..FRAME_SAMPLES * batch as usize]);
                assert_eq!(written, FRAME_SAMPLES * batch as usize, "ring did not have room for the batch");

                let start = Instant::now();
                for _ in 0..batch {
                    black_box(reader.read(black_box(&mut out)).unwrap());
                }
                total += start.elapsed();
                remaining -= batch;
            }
            total
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
