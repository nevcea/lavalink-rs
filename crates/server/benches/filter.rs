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
///
/// Every value here has to be one its filter's `is_enabled` accepts, which is not
/// the same as "present in the request": `distortion` at all-zero offsets and
/// unit scales, and `channelMix` at the identity matrix, are both *neutral*, so
/// `FilterChain::process` skips them and they contribute nothing to the number.
/// That silently left the two out of this case — including `distortion`, the only
/// stage with three transcendentals per sample — so the values below are
/// deliberately off-neutral.
fn all_filters() -> Filters {
    serde_json::from_str(
        r#"{
            "volume": 1.2,
            "equalizer": [{"band":0,"gain":0.5},{"band":5,"gain":0.3}],
            "karaoke": {"level":1.0,"monoLevel":1.0,"filterBand":220.0,"filterWidth":100.0},
            "timescale": {"speed":1.2,"pitch":1.0,"rate":1.0},
            "tremolo": {"frequency":5.0,"depth":0.5},
            "vibrato": {"frequency":5.0,"depth":0.5},
            "distortion": {"sinOffset":0.0,"sinScale":2.0,"cosOffset":0.0,"cosScale":2.0,"tanOffset":0.0,"tanScale":2.0,"offset":0.0,"scale":1.0},
            "rotation": {"rotationHz":0.2},
            "channelMix": {"leftToLeft":0.8,"leftToRight":0.2,"rightToLeft":0.2,"rightToRight":0.8},
            "lowPass": {"smoothing":20.0}
        }"#,
    )
    .unwrap()
}

/// Each implemented filter on its own, at the same settings [`all_filters`] uses
/// so the parts are comparable against the whole.
///
/// `all_filters` answers "what does the worst case cost" but not "which stage is
/// the worst case", and the two are not the same question: the stages differ by
/// more than an order of magnitude per sample — a multiply and a clamp for
/// `channelMix` against three `libm` calls per sample for `distortion` — so
/// without this split there is no way to tell which one a chain's cost is.
fn single_filters() -> Vec<(&'static str, Filters)> {
    [
        ("volume", r#"{"volume": 1.2}"#),
        (
            "equalizer",
            r#"{"equalizer": [{"band":0,"gain":0.5},{"band":5,"gain":0.3}]}"#,
        ),
        (
            "karaoke",
            r#"{"karaoke": {"level":1.0,"monoLevel":1.0,"filterBand":220.0,"filterWidth":100.0}}"#,
        ),
        (
            "timescale",
            r#"{"timescale": {"speed":1.2,"pitch":1.0,"rate":1.0}}"#,
        ),
        ("tremolo", r#"{"tremolo": {"frequency":5.0,"depth":0.5}}"#),
        ("vibrato", r#"{"vibrato": {"frequency":5.0,"depth":0.5}}"#),
        (
            "distortion",
            r#"{"distortion": {"sinOffset":0.0,"sinScale":2.0,"cosOffset":0.0,"cosScale":2.0,"tanOffset":0.0,"tanScale":2.0,"offset":0.0,"scale":1.0}}"#,
        ),
        ("rotation", r#"{"rotation": {"rotationHz":0.2}}"#),
        (
            "channel_mix",
            r#"{"channelMix": {"leftToLeft":0.8,"leftToRight":0.2,"rightToLeft":0.2,"rightToRight":0.8}}"#,
        ),
        ("low_pass", r#"{"lowPass": {"smoothing":20.0}}"#),
    ]
    .into_iter()
    .map(|(name, json)| (name, serde_json::from_str(json).unwrap()))
    .collect()
}

/// An equalizer with the `count` lowest bands boosted.
///
/// The gains vary per band only so that no two bands are identical; what is being
/// swept is how the cost scales with the number of active bands, not the sound.
fn equalizer_bands(count: usize) -> Filters {
    let bands: Vec<String> = (0..count)
        .map(|band| format!(r#"{{"band":{band},"gain":{}}}"#, 0.1 + band as f32 / 40.0))
        .collect();
    serde_json::from_str(&format!(r#"{{"equalizer":[{}]}}"#, bands.join(","))).unwrap()
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

/// Per-stage cost, and how the equalizer scales with active band count.
///
/// Both groups run the chain directly rather than through `filter_interleaved`:
/// the transpose is a fixed cost per buffer that every case would pay equally, so
/// including it would only add the same constant to every number here.
fn bench_filter_stages(c: &mut Criterion) {
    let mut group = c.benchmark_group("filter_chain");
    group.throughput(Throughput::Elements(FRAMES as u64));

    for (name, filters) in single_filters() {
        group.bench_function(BenchmarkId::new("single", name), |b| {
            let mut chain = FilterChain::new(&filters, CHANNELS);
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
    }

    // 15 is EQUALIZER_BAND_COUNT, i.e. every band the protocol has. The bands are
    // a parallel filterbank — each one reads the same input sample and only its own
    // history — so the cost should be linear in the count; a curve that flattens
    // instead would mean the per-band work is hidden behind the serial dependency
    // of each band's own recurrence, which changes what is worth optimising.
    for count in [1, 4, 8, 15] {
        let filters = equalizer_bands(count);
        group.bench_function(BenchmarkId::new("equalizer_bands", count), |b| {
            let mut chain = FilterChain::new(&filters, CHANNELS);
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
    }

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
            // The pump's own scratch, reused across buffers (pump.rs's planar
            // and filtered), so this measures the transpose and not a per-buffer
            // allocation.
            let mut scratch = vec![Vec::new(); CHANNELS];
            let mut out = Vec::new();
            let source = interleaved(CHANNELS, FRAMES);
            b.iter_batched_ref(
                || source.clone(),
                |buffer| {
                    filter_interleaved(
                        &mut chain,
                        black_box(buffer.as_slice()),
                        &mut scratch,
                        &mut out,
                    );
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_filter_chain,
    bench_filter_stages,
    bench_filter_interleaved
);
criterion_main!(benches);
