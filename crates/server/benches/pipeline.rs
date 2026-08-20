//! End-to-end pump throughput: demux, decode, resample and filter a whole track,
//! the way it happens for every track played.
//!
//! This is the "how fast is the pipeline, overall" number — the other benches in
//! this crate isolate one stage each, but concurrent-player capacity is bounded by
//! the whole chain, not any stage in isolation.

use std::io::Read as _;
use std::sync::atomic::{AtomicBool, AtomicI64};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use lavalink_protocol::filters::Filters;
use lavalink_protocol::player::TrackInfo;
use lavalink_server::audio::pump::{self, PumpConfig};
use lavalink_server::audio::ring;
use lavalink_server::audio::stream::StreamOpener;
use lavalink_server::audio::PumpOutcome;

const SAMPLE_RATE: u32 = 44_100;
const CHANNELS: u16 = 2;
const TRACK_SECONDS: f64 = 5.0;

/// A 16-bit PCM WAV written by hand, so the bench needs neither a fixture file nor
/// an encoder on `PATH`. 44.1kHz is the common source rate the resampler exists
/// for, so this exercises that conversion rather than the passthrough path.
fn write_wav(path: &std::path::Path) {
    let frames = (f64::from(SAMPLE_RATE) * TRACK_SECONDS) as u32;
    let data_len = frames * u32::from(CHANNELS) * 2;

    let mut bytes = Vec::with_capacity(44 + data_len as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&CHANNELS.to_le_bytes());
    bytes.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    let block_align = CHANNELS * 2;
    bytes.extend_from_slice(&(SAMPLE_RATE * u32::from(block_align)).to_le_bytes());
    bytes.extend_from_slice(&block_align.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());

    for frame in 0..frames {
        let t = f64::from(frame) / f64::from(SAMPLE_RATE);
        let value = ((t * 440.0 * std::f64::consts::TAU).sin() * 0.5 * 32767.0) as i16;
        for _ in 0..CHANNELS {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
    }

    std::fs::write(path, bytes).unwrap();
}

fn track_info(path: &std::path::Path) -> TrackInfo {
    TrackInfo {
        identifier: path.to_str().unwrap().to_owned(),
        is_seekable: true,
        author: "bench".into(),
        length: (TRACK_SECONDS * 1000.0) as i64,
        is_stream: false,
        position: 0,
        title: "bench track".into(),
        uri: None,
        source_name: "local".into(),
        artwork_url: None,
        isrc: None,
    }
}

/// A bass-boost equalizer — the filter clients set most often, and enough on its own
/// to make the pump pay the interleaved↔planar transpose around the chain.
fn equalizer() -> Filters {
    serde_json::from_str(r#"{"equalizer":[{"band":0,"gain":0.5},{"band":1,"gain":0.3}]}"#).unwrap()
}

fn bench_pipeline(c: &mut Criterion) {
    // Includes the pid so a concurrent cargo test and cargo bench (both of
    // which write a fixed-name file under pump.rs's tests, and this one) do not
    // collide on the same path.
    let path =
        std::env::temp_dir().join(format!("lavalink-rs-bench-pipeline-{}.wav", std::process::id()));
    write_wav(&path);

    let mut group = c.benchmark_group("pipeline");
    group.sample_size(20);
    group.throughput(Throughput::Elements(TRACK_SECONDS as u64));

    // Two cases, because the filter chain is not just extra arithmetic: turning any
    // filter on also makes the pump transpose every buffer to planar and back
    // (pump::filter_interleaved). The unfiltered case alone would hide that
    // entirely, and it is the case most tracks with an equalizer actually run.
    for (name, filters) in [
        ("5s_44100_stereo_wav", Filters::default()),
        ("5s_44100_stereo_wav_equalizer", equalizer()),
    ] {
        group.bench_function(BenchmarkId::new("run", name), |b| {
            // Setup (ring/channel/config) and the post-run drain are both outside the
            // timed span — only `pump::run` itself, i.e. decode+resample+filter, is
            // measured. Folding the drain in here used to inflate every sample with
            // an unrelated ring-read cost, the same conflation `ring.rs`'s
            // `read_only` bench was restructured to avoid.
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let position = Arc::new(AtomicI64::new(0));
                    // Large enough to hold the whole track, so the pump is never
                    // blocked waiting on the ring — this measures decode+resample+
                    // filter alone, not ring backpressure.
                    let (writer, mut reader) = ring::channel(
                        (TRACK_SECONDS * 1000.0) as u32 + 1000,
                        Arc::clone(&position),
                        Arc::new(ring::FrameCounters::default()),
                    );
                    let (_commands_tx, commands_rx) = mpsc::channel();

                    let config = PumpConfig {
                        info: track_info(&path),
                        start_position_ms: 0,
                        end_time_ms: None,
                        volume: 100,
                        filters: filters.clone(),
                        opener: Arc::new(StreamOpener::default()),
                        resampling_quality: lavalink_server::config::ResamplingQuality::Low,
                        interrupt: Arc::new(AtomicBool::new(false)),
                        produced: Arc::new(AtomicBool::new(false)),
                    };

                    let start = Instant::now();
                    let outcome = pump::run(config, writer, commands_rx, position, &|| {});
                    total += start.elapsed();

                    // A broken `open()` (bad path, decoder registry regression) returns
                    // almost instantly via `PumpOutcome::Failed`, which would otherwise
                    // report a suspiciously fast number for having decoded nothing —
                    // the same check pump.rs's own tests make after every `pump::run`.
                    assert!(matches!(outcome, PumpOutcome::Finished), "{outcome:?}");

                    // Drain so the reader's thread-local state doesn't accumulate across
                    // iterations; the ring itself is dropped with reader regardless.
                    let mut sink = [0u8; 4096];
                    while reader.read(&mut sink).unwrap_or(0) > 0 {}
                }
                total
            });
        });
    }

    group.finish();
    let _ = std::fs::remove_file(&path);
}

criterion_group!(benches, bench_pipeline);
criterion_main!(benches);
