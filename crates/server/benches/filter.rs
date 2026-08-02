//! Throughput of the DSP filter chain.
//!
//! Every stage here is a lavaplayer/lavadsp port that runs on every buffer of every
//! playing track (`audio::filter`'s module docs), so this answers "what does turning
//! filters on cost" rather than anything about a single algorithm in isolation.

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use lavalink_protocol::filters::Filters;
use lavalink_server::audio::filter::FilterChain;
use lavalink_server::audio::pump::filter_interleaved;
use lavalink_server::audio::ring::CHANNELS;

mod common;
use common::{interleaved, planar};

/// 20ms at 48kHz — one ring write's worth of frames, the chain's natural unit.
const FRAMES: usize = 960;

/// A realistic single-filter case: a bass-boost equalizer, the filter clients set
/// most often.
fn equalizer_only() -> Filters {
    serde_json::from_str(r#"{"equalizer":[{"band":0,"gain":0.5},{"band":1,"gain":0.3}]}"#)
        .unwrap()
}

/// Every implemented filter enabled at once — the worst case the chain can be
/// asked to run per buffer.
fn all_filters() -> Filters {
    serde_json::from_str(
        r#"{
            "volume": 1.2,
            "equalizer": [{"band":0,"gain":0.5},{"band":5,"gain":0.3}],
            "karaoke": {"level":1.0,"monoLevel":1.0,"filterBand":220.0,"filterWidth":100.0},
            "tremolo": {"frequency":5.0,"depth":0.5},
            "vibrato": {"frequency":5.0,"depth":0.5},
            "distortion": {"sinOffset":0.0,"sinScale":1.0,"cosOffset":0.0,"cosScale":1.0,"tanOffset":0.0,"tanScale":1.0,"offset":0.0,"scale":1.0},
            "rotation": {"rotationHz":0.2},
            "channelMix": {"leftToLeft":1.0,"leftToRight":0.0,"rightToLeft":0.0,"rightToRight":1.0},
            "lowPass": {"smoothing":20.0}
        }"#,
    )
    .unwrap()
}

fn bench_filter_chain(c: &mut Criterion) {
    let mut group = c.benchmark_group("filter_chain");
    group.throughput(Throughput::Elements(FRAMES as u64));

    group.bench_function(BenchmarkId::new("process", "no_filters"), |b| {
        let mut chain = FilterChain::new(&Filters::default(), CHANNELS);
        let source = planar(CHANNELS, FRAMES);
        b.iter_batched(
            || source.clone(),
            |mut buffer| {
                chain.process(black_box(&mut buffer));
                black_box(buffer)
            },
            criterion::BatchSize::SmallInput,
        );
    });

    group.bench_function(BenchmarkId::new("process", "equalizer_only"), |b| {
        let mut chain = FilterChain::new(&equalizer_only(), CHANNELS);
        let source = planar(CHANNELS, FRAMES);
        b.iter_batched(
            || source.clone(),
            |mut buffer| {
                chain.process(black_box(&mut buffer));
                black_box(buffer)
            },
            criterion::BatchSize::SmallInput,
        );
    });

    group.bench_function(BenchmarkId::new("process", "all_filters"), |b| {
        let mut chain = FilterChain::new(&all_filters(), CHANNELS);
        let source = planar(CHANNELS, FRAMES);
        b.iter_batched(
            || source.clone(),
            |mut buffer| {
                chain.process(black_box(&mut buffer));
                black_box(buffer)
            },
            criterion::BatchSize::SmallInput,
        );
    });

    group.finish();
}

/// The same chain as above, but through `pump::filter_interleaved` — the entry
/// point playback actually uses.
///
/// The pump holds PCM interleaved (that is what the ring and the mixer want) while
/// every filter is a planar port, so a buffer with filters on pays a transpose in
/// and a transpose back around `FilterChain::process`. `bench_filter_chain` above
/// deliberately excludes that; this group is the honest per-buffer cost, and the
/// gap between the two groups is what the transpose itself costs.
fn bench_filter_interleaved(c: &mut Criterion) {
    let mut group = c.benchmark_group("filter_interleaved");
    group.throughput(Throughput::Elements(FRAMES as u64));

    for (name, filters) in [
        ("equalizer_only", equalizer_only()),
        ("all_filters", all_filters()),
    ] {
        group.bench_function(BenchmarkId::new("process", name), |b| {
            let mut chain = FilterChain::new(&filters, CHANNELS);
            // The pump's own scratch, reused across buffers (`pump.rs`'s `planar`),
            // so this measures the transpose and not a per-buffer allocation.
            let mut scratch = vec![Vec::new(); CHANNELS];
            let source = interleaved(CHANNELS, FRAMES);
            b.iter_batched_ref(
                || source.clone(),
                |buffer| {
                    filter_interleaved(&mut chain, black_box(buffer), &mut scratch);
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

criterion_group!(benches, bench_filter_chain, bench_filter_interleaved);
criterion_main!(benches);
