//! Linux release-gate workload used by benchmarks/compare/run.py.
//!
//! This is deliberately an example rather than production code: it reuses the
//! real pump, ring, filters, Opus encoder and HTTP client dependencies without
//! adding a benchmark framework or a server API.

use std::error::Error;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use lavalink_protocol::encoded_track::{self, SourceTail};
use lavalink_protocol::filters::Filters;
use lavalink_protocol::player::TrackInfo;
use lavalink_server::audio::pump::{self, PumpConfig};
use lavalink_server::audio::ring::{self, FRAME_SAMPLES, SAMPLE_RATE};
use lavalink_server::audio::stream::StreamOpener;
use lavalink_server::audio::PumpOutcome;
use lavalink_server::config::ResamplingQuality;
use serde_json::{json, Value};
use songbird::driver::opus::{Application, Bitrate, Channels, Encoder};

const FRAME_DURATION: Duration = Duration::from_millis(20);
const OPUS_MAX_PACKET: usize = 4_000;
const LATENCY_BUCKET_US: u64 = 100;
const LATENCY_BUCKETS: usize = 100_001;

type AnyResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Clone)]
struct Args {
    values: Vec<String>,
}

impl Args {
    fn new(values: impl Iterator<Item = String>) -> Self {
        Self { values: values.collect() }
    }

    fn value(&self, name: &str) -> AnyResult<&str> {
        self.values
            .windows(2)
            .find(|pair| pair[0] == name)
            .map(|pair| pair[1].as_str())
            .ok_or_else(|| format!("missing {name}").into())
    }

    fn parse<T>(&self, name: &str) -> AnyResult<T>
    where
        T: std::str::FromStr,
        T::Err: Error + Send + Sync + 'static,
    {
        Ok(self.value(name)?.parse()?)
    }
}

#[derive(Clone, Default)]
struct Window {
    frames: u64,
    bytes: u64,
    misses: u64,
    requests: u64,
    errors: u64,
}

#[derive(Default)]
struct WorkerStats {
    windows: Vec<Window>,
    latency: Vec<u64>,
}

impl WorkerStats {
    fn new(seconds: usize) -> Self {
        Self {
            windows: vec![Window::default(); seconds.max(1)],
            latency: vec![0; LATENCY_BUCKETS],
        }
    }

    fn record_latency(&mut self, elapsed: Duration) {
        let micros = elapsed.as_micros().min(u128::from(u64::MAX)) as u64;
        let bucket = (micros / LATENCY_BUCKET_US).min((LATENCY_BUCKETS - 1) as u64);
        self.latency[bucket as usize] += 1;
    }
}

fn main() -> AnyResult<()> {
    let mut raw = std::env::args();
    let _program = raw.next();
    let command = raw.next().ok_or("usage: compare <audio|http|encoded-track> ...")?;
    let args = Args::new(raw);

    match command.as_str() {
        "audio" => audio(&args),
        "http" => http(&args),
        "encoded-track" => encoded_track(&args),
        _ => Err(format!("unknown command: {command}").into()),
    }
}

fn encoded_track(args: &Args) -> AnyResult<()> {
    let path = PathBuf::from(args.value("--input")?).canonicalize()?;
    let info = track_info(&path, args.parse("--track-ms").unwrap_or(60_000));
    println!("{}", encoded_track::encode(&info, &SourceTail::Probe("wav".into()))?);
    Ok(())
}

fn audio(args: &Args) -> AnyResult<()> {
    let input = PathBuf::from(args.value("--input")?).canonicalize()?;
    let filter = args.value("--filter")?.to_owned();
    let mode = args.value("--mode")?.to_owned();
    let concurrency: usize = args.parse("--concurrency")?;
    let warmup: u64 = args.parse("--warmup-seconds")?;
    let measure: u64 = args.parse("--measure-seconds")?;
    let track_ms: i64 = args.parse("--track-ms").unwrap_or(60_000);
    if concurrency == 0 || measure == 0 {
        return Err("concurrency and measurement duration must be positive".into());
    }
    if !matches!(mode.as_str(), "throughput" | "realtime") {
        return Err("--mode must be throughput or realtime".into());
    }

    let start = Instant::now() + Duration::from_millis(100);
    let measure_start = start + Duration::from_secs(warmup);
    let end = measure_start + Duration::from_secs(measure);
    let mut workers = Vec::with_capacity(concurrency);
    for _ in 0..concurrency {
        let input = input.clone();
        let filter = filter.clone();
        let mode = mode.clone();
        workers.push(thread::spawn(move || {
            audio_worker(&input, &filter, &mode, track_ms, start, measure_start, end, measure)
        }));
    }

    sleep_until(measure_start);
    measurement_marker();

    let mut stats = WorkerStats::new(measure as usize);
    for worker in workers {
        merge(&mut stats, worker.join().map_err(|_| "audio worker panicked")??);
    }
    let wall_seconds = measure as f64;
    let frames: u64 = stats.windows.iter().map(|window| window.frames).sum();
    let bytes: u64 = stats.windows.iter().map(|window| window.bytes).sum();
    let misses: u64 = stats.windows.iter().map(|window| window.misses).sum();
    let result = json!({
        "implementation": "rust",
        "workload": "audio",
        "case": filter,
        "mode": mode,
        "concurrency": concurrency,
        "wall_seconds": wall_seconds,
        "frames": frames,
        "bytes": bytes,
        "misses": misses,
        "p99_service_us": percentile(&stats.latency, 0.99),
        "audio_seconds_per_wall_second": frames as f64 * 0.02 / wall_seconds,
        "output_bitrate": if frames == 0 { 0.0 } else { bytes as f64 * 8.0 * 50.0 / frames as f64 },
        "windows": windows_json(&stats.windows),
    });
    println!("{result}");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn audio_worker(
    input: &Path,
    filter: &str,
    mode: &str,
    track_ms: i64,
    start: Instant,
    measure_start: Instant,
    end: Instant,
    measure_seconds: u64,
) -> AnyResult<WorkerStats> {
    sleep_until(start);
    let mut stats = WorkerStats::new(measure_seconds as usize);
    while Instant::now() < end {
        play_once(input, filter, mode, track_ms, measure_start, end, &mut stats)?;
    }
    Ok(stats)
}

fn play_once(
    input: &Path,
    filter: &str,
    mode: &str,
    track_ms: i64,
    measure_start: Instant,
    end: Instant,
    stats: &mut WorkerStats,
) -> AnyResult<()> {
    let position = Arc::new(AtomicI64::new(0));
    let counters = Arc::new(ring::FrameCounters::default());
    let (writer, mut reader) = ring::channel(5_000, Arc::clone(&position), counters);
    let (_commands_tx, commands_rx) = mpsc::channel();
    let config = PumpConfig {
        info: track_info(input, track_ms),
        start_position_ms: 0,
        end_time_ms: None,
        volume: 100,
        filters: filters(filter)?,
        opener: Arc::new(StreamOpener::default()),
        resampling_quality: ResamplingQuality::Low,
        interrupt: Arc::new(AtomicBool::new(false)),
        produced: Arc::new(AtomicBool::new(false)),
    };
    let pump = thread::spawn(move || pump::run(config, writer, commands_rx, position, &|| {}));

    let mut encoder = Encoder::new(SAMPLE_RATE, Channels::Stereo, Application::Audio)?;
    encoder.set_bitrate(Bitrate::Bits(128_000))?;
    let mut pcm_bytes = vec![0u8; FRAME_SAMPLES * 4];
    let mut pcm = vec![0.0f32; FRAME_SAMPLES];
    let mut opus = vec![0u8; OPUS_MAX_PACKET];
    let realtime = mode == "realtime";
    let mut next_frame = Instant::now();

    while Instant::now() < end {
        if realtime {
            sleep_until(next_frame);
            next_frame += FRAME_DURATION;
        }
        let service_start = Instant::now();
        let mut filled = 0;
        while filled < pcm_bytes.len() {
            let read = reader.read(&mut pcm_bytes[filled..])?;
            if read == 0 {
                break;
            }
            filled += read;
        }
        if filled == 0 {
            break;
        }

        let (_sent, nulled) = reader.take_frame_stats();
        let now = Instant::now();
        if !realtime && nulled > 0 {
            if now >= measure_start {
                let index = window_index(now, measure_start, stats.windows.len());
                stats.windows[index].misses += u64::from(nulled);
            }
            thread::sleep(Duration::from_micros(50));
            continue;
        }
        if filled < pcm_bytes.len() {
            break;
        }

        for (sample, bytes) in pcm.iter_mut().zip(pcm_bytes.chunks_exact(4)) {
            *sample = f32::from_le_bytes(bytes.try_into().expect("four-byte chunk"));
        }
        let encoded = encoder.encode_float(&pcm, &mut opus)?;
        let finished = Instant::now();
        if finished >= measure_start {
            let index = window_index(finished, measure_start, stats.windows.len());
            let window = &mut stats.windows[index];
            if nulled == 0 {
                window.frames += 1;
                window.bytes += encoded as u64;
            } else {
                window.misses += u64::from(nulled);
            }
            stats.record_latency(finished.duration_since(service_start));
        }
    }

    drop(reader);
    let outcome = pump.join().map_err(|_| "pump panicked")?;
    if Instant::now() < end && !matches!(outcome, PumpOutcome::Finished) {
        return Err(format!("pump failed before the measurement ended: {outcome:?}").into());
    }
    Ok(())
}

fn track_info(path: &Path, track_ms: i64) -> TrackInfo {
    TrackInfo {
        identifier: path.to_string_lossy().into_owned(),
        is_seekable: true,
        author: "benchmark".into(),
        length: track_ms,
        is_stream: false,
        position: 0,
        title: "deterministic benchmark fixture".into(),
        uri: None,
        source_name: "local".into(),
        artwork_url: None,
        isrc: None,
    }
}

fn filters(name: &str) -> AnyResult<Filters> {
    let json = match name {
        "default" => "{}",
        "eq" => r#"{"equalizer":[{"band":0,"gain":0.15},{"band":7,"gain":-0.10},{"band":14,"gain":0.20}]}"#,
        "timescale" => r#"{"timescale":{"speed":1.10,"pitch":1.05,"rate":1.0}}"#,
        _ => return Err(format!("unknown filter case: {name}").into()),
    };
    Ok(serde_json::from_str(json)?)
}

fn http(args: &Args) -> AnyResult<()> {
    let base_url = args.value("--base-url")?.trim_end_matches('/').to_owned();
    let password = args.value("--password")?.to_owned();
    let encoded = args.value("--encoded-track")?.to_owned();
    let concurrency: usize = args.parse("--concurrency")?;
    let warmup: u64 = args.parse("--warmup-seconds")?;
    let measure: u64 = args.parse("--measure-seconds")?;
    if concurrency == 0 || measure == 0 {
        return Err("concurrency and measurement duration must be positive".into());
    }

    let start = Instant::now() + Duration::from_millis(100);
    let measure_start = start + Duration::from_secs(warmup);
    let end = measure_start + Duration::from_secs(measure);
    let mut workers = Vec::with_capacity(concurrency);
    for worker_id in 0..concurrency {
        let base_url = base_url.clone();
        let password = password.clone();
        let encoded = encoded.clone();
        workers.push(thread::spawn(move || {
            http_worker(
                worker_id,
                &base_url,
                &password,
                &encoded,
                start,
                measure_start,
                end,
                measure,
            )
        }));
    }

    sleep_until(measure_start);
    measurement_marker();

    let mut stats = WorkerStats::new(measure as usize);
    for worker in workers {
        merge(&mut stats, worker.join().map_err(|_| "HTTP worker panicked")??);
    }
    let requests: u64 = stats.windows.iter().map(|window| window.requests).sum();
    let errors: u64 = stats.windows.iter().map(|window| window.errors).sum();
    let result = json!({
        "implementation": "driver",
        "workload": "http",
        "case": "mixed",
        "concurrency": concurrency,
        "wall_seconds": measure as f64,
        "requests": requests,
        "errors": errors,
        "requests_per_second": requests as f64 / measure as f64,
        "p95_latency_us": percentile(&stats.latency, 0.95),
        "p99_latency_us": percentile(&stats.latency, 0.99),
        "windows": windows_json(&stats.windows),
    });
    println!("{result}");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn http_worker(
    worker_id: usize,
    base_url: &str,
    password: &str,
    encoded: &str,
    start: Instant,
    measure_start: Instant,
    end: Instant,
    measure_seconds: u64,
) -> AnyResult<WorkerStats> {
    let client = reqwest::blocking::Client::builder().build()?;
    let info = client
        .get(format!("{base_url}/v4/info"))
        .header("Authorization", password)
        .build()?;
    let node_stats = client
        .get(format!("{base_url}/v4/stats"))
        .header("Authorization", password)
        .build()?;
    let decoded = client
        .get(format!("{base_url}/v4/decodetrack"))
        .header("Authorization", password)
        .query(&[("encodedTrack", encoded)])
        .build()?;
    let mut requests = Vec::with_capacity(10);
    for _ in 0..7 {
        requests.push(info.try_clone().ok_or("request is not cloneable")?);
    }
    for _ in 0..2 {
        requests.push(node_stats.try_clone().ok_or("request is not cloneable")?);
    }
    requests.push(decoded);

    sleep_until(start);
    let mut stats = WorkerStats::new(measure_seconds as usize);
    let mut sequence = worker_id;
    while Instant::now() < end {
        let request = requests[sequence % requests.len()]
            .try_clone()
            .ok_or("request is not cloneable")?;
        sequence += 1;
        let request_start = Instant::now();
        let successful = match client.execute(request) {
            Ok(response) => {
                let status = response.status();
                status.is_success() && response.bytes().is_ok()
            }
            Err(_) => false,
        };
        let finished = Instant::now();
        if finished < measure_start {
            continue;
        }
        let index = window_index(finished, measure_start, stats.windows.len());
        let window = &mut stats.windows[index];
        window.requests += 1;
        if !successful {
            window.errors += 1;
        }
        stats.record_latency(finished.duration_since(request_start));
    }
    Ok(stats)
}

fn measurement_marker() {
    println!("BENCHMARK_MEASURE");
    let _ = std::io::stdout().flush();
}

fn sleep_until(deadline: Instant) {
    let now = Instant::now();
    if deadline > now {
        thread::sleep(deadline - now);
    }
}

fn window_index(now: Instant, start: Instant, len: usize) -> usize {
    now.saturating_duration_since(start)
        .as_secs()
        .min((len - 1) as u64) as usize
}

fn merge(target: &mut WorkerStats, source: WorkerStats) {
    for (target, source) in target.windows.iter_mut().zip(source.windows) {
        target.frames += source.frames;
        target.bytes += source.bytes;
        target.misses += source.misses;
        target.requests += source.requests;
        target.errors += source.errors;
    }
    for (target, source) in target.latency.iter_mut().zip(source.latency) {
        *target += source;
    }
}

fn percentile(histogram: &[u64], quantile: f64) -> u64 {
    let total: u64 = histogram.iter().sum();
    if total == 0 {
        return 0;
    }
    let wanted = (total as f64 * quantile).ceil() as u64;
    let mut seen = 0;
    for (index, count) in histogram.iter().enumerate() {
        seen += count;
        if seen >= wanted {
            return index as u64 * LATENCY_BUCKET_US;
        }
    }
    (histogram.len() - 1) as u64 * LATENCY_BUCKET_US
}

fn windows_json(windows: &[Window]) -> Vec<Value> {
    windows
        .iter()
        .map(|window| {
            json!({
                "frames": window.frames,
                "bytes": window.bytes,
                "misses": window.misses,
                "requests": window.requests,
                "errors": window.errors,
            })
        })
        .collect()
}
