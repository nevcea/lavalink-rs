//! Throughput of the hand-rolled Catmull-Rom resampler.
//!
//! This is the one piece of the pipeline that trades a dependency for its own
//! arithmetic (see `audio::resample`'s module docs), so it is the one worth
//! watching for a regression a refactor could introduce silently.

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use lavalink_server::audio::resample::Resampler;
use lavalink_server::audio::ring::CHANNELS;

mod common;
use common::interleaved;

/// One symphonia decode buffer's worth of frames, roughly — big enough that the
/// per-call overhead around `process` doesn't dominate the measurement.
const FRAMES: usize = 4096;

/// Times `process_into` with a reused output buffer, which is what the pump does
/// (`pump.rs`'s decode loop hands it the same `pcm` every packet). The allocating
/// `process` wrapper exists for tests, and timing it would fold a `Vec` growth
/// sequence into a number that is supposed to be about interpolation.
///
/// `reset()` runs once, before the timed loop, not inside it: an empty history is a
/// cold start and takes a different path than a call with three frames carried over
/// from the previous buffer (see `resample.rs`'s module docs on chunking). A real
/// pump converts many buffers in a row without resetting between them, so measuring
/// the reset path every iteration would report the one case that never recurs in
/// steady-state playback.
fn bench_into(group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
              name: &str, source_rate: u32, source_channels: usize) {
    group.bench_function(BenchmarkId::new("convert", name), |b| {
        let mut resampler = Resampler::new(source_rate, source_channels);
        let input = interleaved(source_channels, FRAMES);
        let mut out = Vec::new();
        resampler.reset();
        // Warm `out` to its steady-state capacity, for the same reason `reset` is
        // outside the loop: the pump never pays that growth twice either.
        resampler.process_into(&input, &mut out);
        b.iter(|| {
            resampler.process_into(black_box(&input), black_box(&mut out));
        });
    });
}

fn bench_resample(c: &mut Criterion) {
    let mut group = c.benchmark_group("resample");
    group.throughput(Throughput::Elements(FRAMES as u64));

    // The common case per the module docs: a 44.1kHz stereo source going to 48kHz.
    bench_into(&mut group, "44100_stereo_to_48000", 44_100, CHANNELS);
    // Also common: a mono source, which is widened to stereo on top of the rate
    // conversion.
    bench_into(&mut group, "44100_mono_to_48000_stereo", 44_100, 1);
    // The no-op path: source already matches the ring's format, so this measures
    // the pass-through cost rather than any interpolation.
    bench_into(&mut group, "48000_stereo_passthrough", 48_000, CHANNELS);

    group.finish();
}

criterion_group!(benches, bench_resample);
criterion_main!(benches);
