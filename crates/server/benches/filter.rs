//! Throughput of the DSP filter chain.
//!
//! Every stage here is a lavaplayer/lavadsp port that runs on every buffer of every
//! playing track (`audio::filter`'s module docs), so this answers "what does turning
//! filters on cost" rather than anything about a single algorithm in isolation.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use lavalink_protocol::filters::Filters;
use lavalink_server::audio::filter::FilterChain;
use lavalink_server::audio::ring::CHANNELS;

/// 20ms at 48kHz — one ring write's worth of frames, the chain's natural unit.
const FRAMES: usize = 960;

fn planar(frames: usize) -> Vec<Vec<f32>> {
    (0..CHANNELS)
        .map(|_| {
            (0..frames)
                .map(|i| ((i % 997) as f32 / 997.0) * 2.0 - 1.0)
                .collect()
        })
        .collect()
}

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
        let mut chain = FilterChain::empty(CHANNELS);
        let source = planar(FRAMES);
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
        let source = planar(FRAMES);
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
        let source = planar(FRAMES);
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

criterion_group!(benches, bench_filter_chain);
criterion_main!(benches);
