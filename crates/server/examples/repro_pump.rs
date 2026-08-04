//! The whole pump, without Discord — for #9.
//!
//! `repro_stuck.rs` drives symphonia's probe and decoder directly, which is only the
//! left half of the pipeline. This runs the *real* `pump::run` against the *real*
//! `StreamOpener`, writing into a real ring, with a reader pulling on a 20ms clock
//! the way songbird's mixer does. That is the only piece left between "the standalone
//! harness decodes this file fine" and "the node reports the track stuck": everything
//! here is the service's own code path minus the voice connection.
//!
//! ```sh
//! cargo run -p lavalink-server --release --example repro_pump -- J8io3r9b3rs
//! cargo run -p lavalink-server --release --example repro_pump -- https://example.invalid/a.mp3
//! ```
//!
//! Prints a line per second: playback position, frames delivered, frames nulled. A
//! position that never leaves 0 with nulled frames climbing is the reported bug; the
//! pump is not filling the ring. A position that climbs with nulled at 0 means the
//! pump is fine and the fault is further downstream (the mixer never pulling).

use std::io::Read as _;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lavalink_protocol::filters::Filters;
use lavalink_protocol::player::TrackInfo;
use lavalink_server::audio::pump::{self, PumpConfig};
use lavalink_server::audio::ring::{self, FrameCounters, FRAME_SAMPLES};
use lavalink_server::audio::source::YtDlp;
use lavalink_server::audio::stream::StreamOpener;
use lavalink_server::config::ResamplingQuality;

/// `frameBufferDurationMs`' default (`config.rs`), so the ring is the size service
/// actually runs with — a smaller one would park the pump far sooner and change the
/// very timing under investigation.
const BUFFER_MS: u32 = 5_000;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(6);

/// How long to watch before giving up and reporting what happened.
const RUN_FOR: Duration = Duration::from_secs(30);

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "lavalink_server=debug,symphonia=info".into()),
        )
        .init();

    let identifier = std::env::args()
        .nth(1)
        .expect("usage: repro_pump <youtube-video-id|http-url>");

    let ytdlp = YtDlp::detect("yt-dlp", None, 100).map(Arc::new);
    let info = track_info(&identifier);
    if info.source_name == "youtube" && ytdlp.is_none() {
        panic!("yt-dlp is not on PATH, so a youtube identifier cannot be resolved");
    }

    let opener = Arc::new(StreamOpener::new(
        ytdlp,
        None,
        CONNECT_TIMEOUT,
        READ_TIMEOUT,
    ));

    let position_ms = Arc::new(AtomicI64::new(0));
    let frames = Arc::new(FrameCounters::default());
    let (writer, mut reader) = ring::channel(
        BUFFER_MS,
        Arc::clone(&position_ms),
        Arc::clone(&frames),
    );
    // Kept alive for the whole run: dropping the sender is how the engine tells the
    // pump to stop, and a pump that stops immediately proves nothing.
    let (commands, pump_commands) = mpsc::channel();

    let pump_position = Arc::clone(&position_ms);
    let pump = std::thread::Builder::new()
        .name("pump-repro".to_owned())
        .spawn(move || {
            let config = PumpConfig {
                info,
                start_position_ms: 0,
                end_time_ms: None,
                volume: 100,
                filters: Filters::default(),
                opener,
                resampling_quality: ResamplingQuality::Low,
                interrupt: Arc::new(AtomicBool::new(false)),
                produced: Arc::new(AtomicBool::new(false)),
            };
            pump::run(config, writer, pump_commands, pump_position, &|| {})
        })
        .expect("spawning the pump thread");

    // Stands in for the mixer: one 20ms frame every 20ms, never faster. Pulling as
    // fast as possible would drain the ring and manufacture the starvation this is
    // trying to observe.
    let started = Instant::now();
    let mut next_pull = Instant::now();
    let mut buffer = vec![0u8; FRAME_SAMPLES * 4];
    let mut total_sent = 0u64;
    let mut total_nulled = 0u64;
    let mut last_report = Instant::now();
    let mut ended = false;

    while started.elapsed() < RUN_FOR {
        next_pull += Duration::from_millis(20);
        if let Some(sleep) = next_pull.checked_duration_since(Instant::now()) {
            std::thread::sleep(sleep);
        }

        if reader.read(&mut buffer).expect("the ring never errors") == 0 {
            ended = true;
            break;
        }

        let (sent, nulled) = reader.take_frame_stats();
        total_sent += u64::from(sent);
        total_nulled += u64::from(nulled);

        if last_report.elapsed() >= Duration::from_secs(1) {
            last_report = Instant::now();
            eprintln!(
                "t={:>5}ms position={:>7}ms sent={total_sent} nulled={total_nulled}",
                started.elapsed().as_millis(),
                position_ms.load(Ordering::Relaxed),
            );
        }
    }

    eprintln!(
        "\n{} after {:?}: position={}ms sent={total_sent} nulled={total_nulled}",
        if ended { "end of stream" } else { "time limit" },
        started.elapsed(),
        position_ms.load(Ordering::Relaxed),
    );

    drop(commands);
    drop(reader);
    eprintln!("pump outcome: {:?}", pump.join().expect("the pump panicked"));
}

/// The same shape the loader produces, which is what decides `StreamOpener::open`'s
/// branch — a bare video id is a youtube track, anything else is a plain http one.
fn track_info(identifier: &str) -> TrackInfo {
    let is_url = identifier.starts_with("http");
    let youtube = !is_url || identifier.contains("youtube.com") || identifier.contains("youtu.be");

    let (source_name, identifier, uri) = if youtube {
        let id = identifier
            .rsplit_once("v=")
            .map(|(_, id)| id)
            .or_else(|| identifier.rsplit_once("youtu.be/").map(|(_, id)| id))
            .unwrap_or(identifier)
            .to_owned();
        let uri = format!("https://www.youtube.com/watch?v={id}");
        ("youtube", id, uri)
    } else {
        ("http", identifier.to_owned(), identifier.to_owned())
    };

    TrackInfo {
        identifier,
        is_seekable: true,
        author: "repro".into(),
        length: 0,
        is_stream: false,
        position: 0,
        title: "repro".into(),
        uri: Some(uri),
        source_name: source_name.to_owned(),
        artwork_url: None,
        isrc: None,
    }
}
