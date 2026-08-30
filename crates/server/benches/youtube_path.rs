//! Compares the old AAC/PCM path with WebM/Opus passthrough under one Criterion
//! benchmark ID. Set `LAVALINK_BENCH_PCM=1` when saving the baseline; omit it
//! for the candidate run.

use std::fs::File;
use std::io::{Read as _, Write as _};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicI64};
use std::sync::{mpsc, Arc};

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use lavalink_protocol::filters::Filters;
use lavalink_protocol::player::TrackInfo;
use lavalink_server::audio::pump::{self, PumpConfig};
use lavalink_server::audio::ring;
use lavalink_server::audio::stream::StreamOpener;
use lavalink_server::audio::PumpOutcome;
use songbird::constants::MONO_FRAME_SIZE;
use songbird::driver::bench_internals::mixer::mix_logic::mix_symph_indiv;
use songbird::driver::bench_internals::mixer::state::DecodeState;
use songbird::input::codecs::{get_codec_registry, get_probe};
use songbird::input::core::audio::{AudioBuffer, Channels, SignalSpec};
use songbird::input::core::io::MediaSource;
use songbird::input::{AudioStream, LiveInput};

const SAMPLE_RATE: u32 = 44_100;
const TRACK_SECONDS: usize = 5;

fn write_wav(path: &std::path::Path) {
    let mut file = File::create(path).unwrap();
    let samples = SAMPLE_RATE as usize * TRACK_SECONDS * 2;
    let data_bytes = samples * size_of::<i16>();
    file.write_all(b"RIFF").unwrap();
    file.write_all(&(36 + data_bytes as u32).to_le_bytes()).unwrap();
    file.write_all(b"WAVEfmt ").unwrap();
    file.write_all(&16u32.to_le_bytes()).unwrap();
    file.write_all(&1u16.to_le_bytes()).unwrap();
    file.write_all(&2u16.to_le_bytes()).unwrap();
    file.write_all(&SAMPLE_RATE.to_le_bytes()).unwrap();
    file.write_all(&(SAMPLE_RATE * 4).to_le_bytes()).unwrap();
    file.write_all(&4u16.to_le_bytes()).unwrap();
    file.write_all(&16u16.to_le_bytes()).unwrap();
    file.write_all(b"data").unwrap();
    file.write_all(&(data_bytes as u32).to_le_bytes()).unwrap();
    for frame in 0..SAMPLE_RATE as usize * TRACK_SECONDS {
        let sample = ((frame as f32 * 440.0 * std::f32::consts::TAU / SAMPLE_RATE as f32).sin()
            * 8_000.0) as i16;
        file.write_all(&sample.to_le_bytes()).unwrap();
        file.write_all(&sample.to_le_bytes()).unwrap();
    }
}

fn transcode(input: &std::path::Path, output: &std::path::Path, codec: &str) {
    let status = Command::new("ffmpeg")
        .args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
        .arg(input)
        .args(["-vn", "-c:a", codec, "-b:a", "128k"])
        .arg(output)
        .status()
        .expect("ffmpeg is required for the YouTube path benchmark");
    assert!(status.success(), "ffmpeg could not create {output:?}");
}

fn track_info(path: &std::path::Path) -> TrackInfo {
    TrackInfo {
        identifier: path.to_string_lossy().into_owned(),
        is_seekable: true,
        author: "benchmark".into(),
        length: (TRACK_SECONDS * 1000) as i64,
        is_stream: false,
        position: 0,
        title: "benchmark".into(),
        uri: None,
        source_name: "local".into(),
        artwork_url: None,
        isrc: None,
    }
}

fn run_pcm(path: &std::path::Path) {
    let position = Arc::new(AtomicI64::new(0));
    let (writer, mut reader) = ring::channel(
        (TRACK_SECONDS * 1000 + 1000) as u32,
        Arc::clone(&position),
        Arc::new(ring::FrameCounters::default()),
    );
    let finish = writer.clone();
    let (release_reader, reader_release) = mpsc::channel();
    let drain = std::thread::spawn(move || {
        finish.wait_for_finish();
        let mut sink = [0u8; 4096];
        while reader.read(&mut sink).unwrap_or(0) > 0 {}
        let _ = reader_release.recv();
    });
    let (_commands, pump_commands) = mpsc::channel();
    let outcome = pump::run(
        PumpConfig {
            info: track_info(path),
            start_position_ms: 0,
            end_time_ms: None,
            volume: 100,
            filters: Filters::default(),
            opener: Arc::new(StreamOpener::default()),
            resampling_quality: lavalink_server::config::ResamplingQuality::Low,
            interrupt: Arc::new(AtomicBool::new(false)),
            produced: Arc::new(AtomicBool::new(false)),
        },
        writer,
        pump_commands,
        position,
        &|| {},
    );
    let _ = release_reader.send(());
    drain.join().unwrap();
    assert!(matches!(outcome, PumpOutcome::Finished), "{outcome:?}");
}

fn run_direct(path: &std::path::Path) {
    let source = Box::new(File::open(path).unwrap()) as Box<dyn MediaSource>;
    let input = LiveInput::Raw(AudioStream { input: source })
        .promote(get_codec_registry(), get_probe())
        .unwrap();
    let LiveInput::Parsed(mut input) = input else { unreachable!() };
    let spec = SignalSpec::new(48_000, Channels::FRONT_LEFT | Channels::FRONT_RIGHT);
    let mut mixed = AudioBuffer::<f32>::new(MONO_FRAME_SIZE as u64, spec);
    let mut scratch = AudioBuffer::<f32>::new(MONO_FRAME_SIZE as u64, spec);
    let mut state = DecodeState::default();
    let mut opus = [0u8; 4_096];

    for _ in 0..TRACK_SECONDS * 50 {
        std::hint::black_box(mix_symph_indiv(
            &mut mixed,
            &mut scratch,
            &mut input,
            &mut state,
            1.0,
            Some(&mut opus),
        ));
    }
}

fn bench_youtube_path(c: &mut Criterion) {
    let pcm = std::env::var_os("LAVALINK_BENCH_PCM").is_some();
    let prefix = format!("lavalink-rs-youtube-path-{}", std::process::id());
    let wav = std::env::temp_dir().join(format!("{prefix}.wav"));
    let m4a = std::env::temp_dir().join(format!("{prefix}.m4a"));
    let webm = std::env::temp_dir().join(format!("{prefix}.webm"));
    write_wav(&wav);
    transcode(&wav, &m4a, "aac");
    transcode(&wav, &webm, "libopus");

    let mut group = c.benchmark_group("youtube_path");
    group.sample_size(20);
    group.throughput(Throughput::Elements(TRACK_SECONDS as u64));
    group.bench_function("run", |b| {
        if pcm {
            b.iter(|| run_pcm(&m4a));
        } else {
            b.iter(|| run_direct(&webm));
        }
    });
    group.finish();

    for path in [wav, m4a, webm] {
        let _ = std::fs::remove_file(path);
    }
}

criterion_group!(benches, bench_youtube_path);
criterion_main!(benches);
