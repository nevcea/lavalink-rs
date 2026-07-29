//! Throughput of the hand-rolled Catmull-Rom resampler.
//!
//! This is the one piece of the pipeline that trades a dependency for its own
//! arithmetic (see `audio::resample`'s module docs), so it is the one worth
//! watching for a regression a refactor could introduce silently.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use lavalink_server::audio::resample::Resampler;
use lavalink_server::audio::ring::CHANNELS;

/// One symphonia decode buffer's worth of frames, roughly — big enough that the
/// per-call overhead around `process` doesn't dominate the measurement.
const FRAMES: usize = 4096;

fn interleaved(channels: usize, frames: usize) -> Vec<f32> {
    (0..frames * channels)
        .map(|i| ((i % 997) as f32 / 997.0) * 2.0 - 1.0)
        .collect()
}

fn bench_resample(c: &mut Criterion) {
    let mut group = c.benchmark_group("resample");
    group.throughput(Throughput::Elements(FRAMES as u64));

    // The common case per the module docs: a 44.1kHz stereo source going to 48kHz.
    //
    // `reset()` runs once, before the timed loop, not inside it: `process` treats
    // an empty history as a cold start and takes a different path than a call with
    // three frames of carried-over history from the previous buffer (see
    // `resample.rs`'s module docs on chunking). A real pump calls `process` many
    // times in a row without resetting between them, so measuring the reset path on
    // every iteration would report the wrong number — the one case that never
    // actually recurs in steady-state playback.
    group.bench_function(BenchmarkId::new("convert", "44100_stereo_to_48000"), |b| {
        let mut resampler = Resampler::new(44_100, CHANNELS);
        let input = interleaved(CHANNELS, FRAMES);
        resampler.reset();
        b.iter(|| black_box(resampler.process(black_box(&input))));
    });

    // Also common: a mono source, which is widened to stereo on top of the rate
    // conversion.
    group.bench_function(BenchmarkId::new("convert", "44100_mono_to_48000_stereo"), |b| {
        let mut resampler = Resampler::new(44_100, 1);
        let input = interleaved(1, FRAMES);
        resampler.reset();
        b.iter(|| black_box(resampler.process(black_box(&input))));
    });

    // The no-op path: source already matches the ring's format, so this measures
    // the pass-through cost rather than any interpolation.
    group.bench_function(BenchmarkId::new("convert", "48000_stereo_passthrough"), |b| {
        let mut resampler = Resampler::new(48_000, CHANNELS);
        let input = interleaved(CHANNELS, FRAMES);
        resampler.reset();
        b.iter(|| black_box(resampler.process(black_box(&input))));
    });

    group.finish();
}

criterion_group!(benches, bench_resample);
criterion_main!(benches);
