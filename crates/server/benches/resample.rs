//! Throughput of audio::resample's three resamplingQuality tiers.
//!
//! Low is the hand-rolled Catmull-Rom resampler — the one piece of the
//! pipeline that trades a dependency for its own arithmetic (see
//! audio::resample's module docs) — worth watching for a regression a
//! refactor could introduce silently. Medium/High are rubato's
//! windowed-sinc resampler, expected to cost more per the same trade
//! lavaplayer itself makes at those tiers; benchmarked here so that cost is
//! visible rather than assumed.

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use lavalink_server::audio::resample::Resampler;
use lavalink_server::audio::ring::CHANNELS;
use lavalink_server::config::ResamplingQuality;

mod common;
use common::interleaved;

/// One symphonia decode buffer's worth of frames, roughly — big enough that the
/// per-call overhead around process doesn't dominate the measurement.
const FRAMES: usize = 4096;

/// Times process_into with a reused output buffer, which is what the pump does
/// (pump.rs's decode loop hands it the same pcm every packet). The allocating
/// process wrapper exists for tests, and timing it would fold a Vec growth
/// sequence into a number that is supposed to be about interpolation.
///
/// reset() runs once, before the timed loop, not inside it: an empty history is a
/// cold start and takes a different path than a call with three frames carried over
/// from the previous buffer (see resample.rs's module docs on chunking). A real
/// pump converts many buffers in a row without resetting between them, so measuring
/// the reset path every iteration would report the one case that never recurs in
/// steady-state playback.
fn bench_into(group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
              name: &str, source_rate: u32, source_channels: usize, quality: ResamplingQuality) {
    group.bench_function(BenchmarkId::new("convert", name), |b| {
        let mut resampler = Resampler::new(source_rate, source_channels, quality);
        let input = interleaved(source_channels, FRAMES);
        let mut out = Vec::new();
        resampler.reset();
        // Warm out to its steady-state capacity, for the same reason reset is
        // outside the loop: the pump never pays that growth twice either.
        resampler.process_into(&input, &mut out);
        b.iter(|| {
            resampler.process_into(black_box(&input), black_box(&mut out));
            // Weak sanity check, not a correctness guarantee: catches a regression
            // that silently turns conversion into a no-op (a broken is_passthrough
            // check, sinc never getting constructed when it should) rather than
            // reporting a fast, wrong number with no signal.
            debug_assert!(!out.is_empty(), "process_into produced nothing");
        });
    });
}

fn bench_resample(c: &mut Criterion) {
    let mut group = c.benchmark_group("resample");
    group.throughput(Throughput::Elements(FRAMES as u64));

    // The common case per the module docs: a 44.1kHz stereo source going to 48kHz.
    bench_into(&mut group, "44100_stereo_to_48000", 44_100, CHANNELS, ResamplingQuality::Low);
    // Also common: a mono source, which is widened to stereo on top of the rate
    // conversion.
    bench_into(&mut group, "44100_mono_to_48000_stereo", 44_100, 1, ResamplingQuality::Low);
    // The no-op path: source already matches the ring's format, so this measures
    // the pass-through cost rather than any interpolation. Quality is irrelevant
    // here — Resampler::new never builds a SincEngine when no rate conversion
    // is needed — but Low is passed for consistency with the other cases.
    bench_into(&mut group, "48000_stereo_passthrough", 48_000, CHANNELS, ResamplingQuality::Low);

    // The rubato-backed tiers, same source shape as the first case above, so the
    // three numbers are directly comparable: this is the cost lavaplayer itself
    // pays for Medium/High, not a Catmull-Rom regression.
    bench_into(&mut group, "44100_stereo_to_48000_medium", 44_100, CHANNELS, ResamplingQuality::Medium);
    bench_into(&mut group, "44100_stereo_to_48000_high", 44_100, CHANNELS, ResamplingQuality::High);

    group.finish();
}

criterion_group!(benches, bench_resample);
criterion_main!(benches);
