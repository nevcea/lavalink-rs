//! Opening a track's bytes for the pump.
//!
//! Produces a MediaSource — the demuxer's view of a byte stream — for each of
//! our sources. The interesting one is HTTP: whether it is seekable, and what
//! happens when a long-running stream drops mid-track.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lavalink_protocol::player::TrackInfo;
use reqwest::blocking::{Client, Response};
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, RANGE};
use reqwest::StatusCode;
use symphonia::core::io::MediaSource;

use super::source::http::accepts_ranges;
use super::source::ytdlp::{SourceKind, STREAM_USER_AGENT};
use super::source::{SourceError, YtDlp};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// What a stalled source returns instead of retrying, once the pump has a command
/// waiting. Carried as an io::Error payload rather than as an io::ErrorKind.
///
/// ErrorKind::Interrupted is the obvious spelling and is what this used to be —
/// but it is the one kind symphonia deliberately retries rather than propagates.
/// MediaSourceStream::read_buf_exact swallows it and calls read again, and that
/// is the path a packet body takes (read_mpeg_frame reads the frame that way), so
/// the error never escaped to decode_loop from a real demuxer at all — only from
/// the mock FormatReader the pump's tests use.
///
/// The result was worse than having no interrupt at all: the flag is only cleared
/// by drain_commands, which a pump parked inside next_packet() never reaches,
/// and the check sits above the reconnect guard, so the source could not recover
/// either. A silent connection plus a pending stop meant read returning this and
/// symphonia retrying it every read_timeout until MAX_REQUEST_DURATION — six
/// hours — with the pump thread, reader thread and socket pinned throughout.
/// Without the flag the same source would have failed for good after
/// MAX_RECONNECT_ATTEMPTS, and that error does propagate.
///
/// Every other kind propagates, so what identifies this is the payload type rather
/// than the kind: a genuine ErrorKind::Other off the network must not be mistaken
/// for a pending command and skipped.
#[derive(Debug)]
pub struct CommandPending;

impl std::fmt::Display for CommandPending {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a pump command is pending")
    }
}

impl std::error::Error for CommandPending {}

/// Whether an I/O error is CommandPending — a cue for decode_loop to go drain
/// its command queue, not a real read failure.
pub fn is_command_pending(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|inner| inner.is::<CommandPending>())
}

/// How long a single read may go without a byte before it is treated as stalled.
///
/// Comfortably under trackStuckThresholdMs's 10s default, so a stall is caught
/// and reconnected (see MAX_RECONNECT_ATTEMPTS) before the player ever has to
/// report the track stuck.
const READ_TIMEOUT: Duration = Duration::from_secs(6);

/// How many times in a row a dropped connection is re-established mid-track before
/// the read is allowed to fail for good. A single blip (the "request or response
/// body error" a proxy throws mid-stream) would otherwise end a track that has
/// barely started, exactly like a moment of decoder noise would if the pump didn't
/// tolerate a run of those either.
const MAX_RECONNECT_ATTEMPTS: u32 = 3;

/// Hard ceiling on how long a single HTTP request (one HttpMediaSource::connect
/// call) may run, applied per-request rather than at the client level.
///
/// reqwest::blocking has no idle-read timeout of its own in its public API — the
/// async client's ClientBuilder::read_timeout would be exactly right, but the
/// blocking wrapper never exposes it — and tcp_user_timeout doesn't help either,
/// since it only fires on data left unacknowledged, not on a connection that is
/// simply silent. Without some bound, a connection that goes quiet without closing
/// pins ReaderChannel::spawn's reader thread in a blocking Response::read()
/// forever once a reconnect replaces it: HttpMediaSource::read's reconnect logic
/// only stops the caller from waiting past read_timeout, it does nothing to the
/// thread being left behind, so a long-running stream that stalls repeatedly can
/// leak one OS thread and socket per stall, without limit.
///
/// A seekable source (the only kind that ever reconnects, see connect's callers)
/// resumes transparently well before this fires; hitting it just forces that
/// resume a little early. For a non-seekable source it ends a request that has
/// been running for an implausibly long time instead of leaking its thread
/// forever — a fixed, attributable failure instead of an unbounded one.
// ponytail: a generous fixed ceiling, not a real idle-read timeout — replace with
// one if reqwest's blocking client ever exposes ClientBuilder::read_timeout
// publicly (it already exists on the async client; the blocking wrapper's own
// with_inner that could reach it is a private method, not part of its API).
const MAX_REQUEST_DURATION: Duration = Duration::from_secs(6 * 60 * 60);

/// Opens byte streams for resolved tracks.
///
/// Holds the yt-dlp handle because those sources' media URLs are not stored — they
/// expire in hours, so they are resolved again here, at playback time.
pub struct StreamOpener {
    ytdlp: Option<Arc<YtDlp>>,
    proxy: Option<reqwest::Proxy>,
    /// lavalink.server.timeouts.connectTimeoutMs.
    connect_timeout: Duration,
    /// lavalink.server.timeouts.socketTimeoutMs — the idle-read stall threshold
    /// (Apache's SO_TIMEOUT), not an overall request timeout.
    read_timeout: Duration,
}

/// Matches the constructor a zero-config StreamOpener gets everywhere except
/// main.rs: no yt-dlp, no proxy, and the same timeouts production used before
/// they became configurable.
impl Default for StreamOpener {
    fn default() -> Self {
        Self {
            ytdlp: None,
            proxy: None,
            connect_timeout: CONNECT_TIMEOUT,
            read_timeout: READ_TIMEOUT,
        }
    }
}

impl StreamOpener {
    pub fn new(
        ytdlp: Option<Arc<YtDlp>>,
        proxy: Option<reqwest::Proxy>,
        connect_timeout: Duration,
        read_timeout: Duration,
    ) -> Self {
        Self {
            ytdlp,
            proxy,
            connect_timeout,
            read_timeout,
        }
    }

    /// interrupt is polled by a stalled HTTP source between reconnect attempts —
    /// see HttpMediaSource's field of the same name — so a pump command that
    /// arrived while the source was stuck retrying can be acted on immediately
    /// rather than after the retry budget runs out.
    pub fn open(
        &self,
        info: &TrackInfo,
        interrupt: Arc<AtomicBool>,
    ) -> Result<Box<dyn MediaSource>, SourceError> {
        match info.source_name.as_str() {
            "local" => {
                let file = std::fs::File::open(&info.identifier).map_err(|error| {
                    match error.kind() {
                        io::ErrorKind::NotFound => SourceError::NotFound,
                        _ => SourceError::Io(error.to_string()),
                    }
                })?;
                Ok(Box::new(file))
            }
            "http" => {
                let url = info.uri.clone().unwrap_or_else(|| info.identifier.clone());
                self.open_http(&url, None, interrupt)
            }
            // Unlike "http", identifier here is the direct CDN video URL and
            // uri is the original getyarn.io page — the two are not
            // interchangeable the way they are for a plain http(s) track.
            "getyarn.io" => self.open_http(&info.identifier, None, interrupt),
            "youtube" | "soundcloud" | "bandcamp" => {
                let kind = match info.source_name.as_str() {
                    "youtube" => SourceKind::YouTube,
                    "soundcloud" => SourceKind::SoundCloud,
                    _ => SourceKind::Bandcamp,
                };
                let ytdlp = self.ytdlp.as_ref().ok_or_else(|| SourceError::Unplayable {
                    reason: format!(
                        "the {} source is not enabled on this node",
                        kind.name()
                    ),
                })?;
                // Resolved now, not at load time: a track queued hours ago would
                // otherwise carry a dead URL.
                let url = ytdlp.resolve_stream_url(&kind.playback_url(&info.identifier))?;
                // Fetched under the same User-Agent yt-dlp resolved it with —
                // googlevideo.com 403s a mismatch. See STREAM_USER_AGENT.
                self.open_http(&url, Some(STREAM_USER_AGENT), interrupt)
            }
            // Deezer's own API never hands back more than a 30-second preview, so
            // playback substitutes the best YouTube match instead — chosen now,
            // not at load time, so it is not a stream URL going stale but an
            // actual re-search, run fresh for every play.
            "deezer" => {
                let ytdlp = self.ytdlp.as_ref().ok_or_else(|| SourceError::Unplayable {
                    reason: "deezer playback requires yt-dlp, which is not enabled on this node"
                        .to_owned(),
                })?;
                let query = format!("{} {}", info.title, info.author);
                let video_id = ytdlp.find_youtube_match(&query)?;
                let url = ytdlp
                    .resolve_stream_url(&SourceKind::YouTube.playback_url(&video_id))?;
                self.open_http(&url, Some(STREAM_USER_AGENT), interrupt)
            }
            other => Err(SourceError::Unplayable {
                reason: format!("no reader for source {other}"),
            }),
        }
    }

    fn open_http(
        &self,
        url: &str,
        user_agent: Option<&str>,
        interrupt: Arc<AtomicBool>,
    ) -> Result<Box<dyn MediaSource>, SourceError> {
        Ok(Box::new(HttpMediaSource::open_with_timeouts(
            url,
            user_agent,
            self.proxy.clone(),
            self.connect_timeout,
            self.read_timeout,
            MAX_REQUEST_DURATION,
            interrupt,
        )?))
    }
}

/// An HTTP resource as a seekable byte stream.
///
/// Seeking re-issues the request with a Range header. That is the only way to move
/// backwards in a stream we are not storing, and it is why seek support is reported
/// from Accept-Ranges rather than assumed: on a server without it, symphonia would
/// otherwise ask for a seek that silently returns the wrong bytes.
pub struct HttpMediaSource {
    client: Client,
    url: String,
    /// None for a live stream with no declared length.
    length: Option<u64>,
    seekable: bool,
    position: u64,
    /// Behind a mutex purely to satisfy MediaSource's Sync bound — the pump
    /// owns this exclusively and never shares it.
    reader: Mutex<Option<ReaderChannel>>,
    /// Consecutive reconnects since the last byte actually read. Resets on any
    /// successful read, so it counts a run of failures, not a track's total.
    reconnect_attempts: u32,
    /// READ_TIMEOUT in production; shrunk in tests so a stall scenario doesn't
    /// have to burn the real multi-second timeout to exercise it.
    read_timeout: Duration,
    /// MAX_REQUEST_DURATION in production; shrunk in tests that specifically
    /// exercise it.
    request_duration: Duration,
    /// Set by the pump whenever a command (Seek, Stop, ...) is waiting to be
    /// applied. Checked between reconnect attempts so a stalled connection gives
    /// up its remaining retry budget immediately instead of making the command
    /// wait out the whole thing — up to MAX_RECONNECT_ATTEMPTS full
    /// connect-and-stall cycles, tens of seconds, otherwise. Cleared by the pump
    /// once it has drained the commands that set it.
    interrupt: Arc<AtomicBool>,
}

/// Bytes off a socket, one chunk at a time, from a dedicated thread — so the
/// consumer can bound how long it waits for the next one via recv_timeout.
/// reqwest::blocking has no idle-read timeout of its own, and a plain blocking
/// Response::read can hang forever on a connection that stopped sending without
/// closing, which is exactly the failure mode a stalled CDN edge produces.
struct ReaderChannel {
    chunks: Receiver<io::Result<Vec<u8>>>,
    /// Spent read buffers returned to the reader thread for reuse. Bounded to two
    /// entries, matching the reader's two-buffer working set (one in flight in
    /// chunks, one drained here) — the reader consumes from this on every read,
    /// so a try_send here that finds the channel full is a bug, not a normal
    /// case, and dropping the buffer would silently degrade to per-read allocation.
    returns: SyncSender<Vec<u8>>,
    /// Bytes already received but not yet handed to the caller of read.
    leftover: Vec<u8>,
    leftover_pos: usize,
    /// Bytes received since window_start, for the throughput floor below.
    window_bytes: usize,
    window_start: Instant,
}

/// Chunk size for the reader thread's own reads. Unrelated to the caller's buffer
/// size — it only bounds how much a single stalled recv_timeout can be behind.
const READ_CHUNK_BYTES: usize = 64 * 1024;

/// The least a source may deliver within one read_timeout window and still count
/// as alive.
///
/// The idle-gap timeout alone only catches a source that goes fully silent; one
/// that sends a byte or two just before every timeout never trips it, while still
/// pinning the pump thread and this reader thread indefinitely — next_packet()
/// only checks for a pending stop between packets, so a source that never finishes
/// a packet never gives the pump a chance to notice it was asked to stop. Set far
/// below any real stream's bitrate (even an 8kbps low-bitrate radio feed is
/// roughly 6 000 bytes over READ_TIMEOUT) so this only catches a source making
/// essentially no progress, not a genuinely slow one.
const MIN_WINDOW_BYTES: usize = 256;

impl ReaderChannel {
    fn spawn(mut response: Response) -> Self {
        let (chunks_tx, chunks_rx) = sync_channel::<io::Result<Vec<u8>>>(2);
        // Two entries — one buffer sits in chunks_rx awaiting the receiver while
        // the other is being read into. Pre-filled so the reader never has to
        // allocate: a warmed-up track cycles the same two Vecs forever.
        let (returns_tx, returns_rx) = sync_channel::<Vec<u8>>(2);
        returns_tx
            .send(vec![0u8; READ_CHUNK_BYTES])
            .expect("the returns channel has capacity for two, and is empty here");
        returns_tx
            .send(vec![0u8; READ_CHUNK_BYTES])
            .expect("the returns channel has capacity for two, and holds one here");
        std::thread::spawn(move || {
            loop {
                // Blocks the reader whenever both buffers are queued in chunks_tx
                // and the receiver has not sent one back yet — natural backpressure
                // that matches what sync_channel(2) gave us before this change.
                // The Err arm means the receiver has been dropped; nothing to do.
                let Ok(mut buf) = returns_rx.recv() else { return };
                buf.resize(READ_CHUNK_BYTES, 0);
                let outcome = match response.read(&mut buf) {
                    Ok(n) => {
                        buf.truncate(n);
                        Ok(buf)
                    }
                    Err(error) => Err(error),
                };
                let at_end = matches!(&outcome, Ok(data) if data.is_empty()) || outcome.is_err();
                if chunks_tx.send(outcome).is_err() || at_end {
                    // Either nobody is listening any more (a reconnect replaced
                    // this reader before it noticed) or the response is done.
                    return;
                }
            }
        });
        Self {
            chunks: chunks_rx,
            returns: returns_tx,
            leftover: Vec::new(),
            leftover_pos: 0,
            window_bytes: 0,
            window_start: Instant::now(),
        }
    }

    fn read(&mut self, out: &mut [u8], timeout: Duration) -> io::Result<usize> {
        if self.leftover_pos >= self.leftover.len() {
            match self.chunks.recv_timeout(timeout) {
                Ok(Ok(data)) => {
                    // spawn sends an empty chunk to mark the end of the response
                    // before it exits — not a throughput sample, and must not be
                    // charged as one: recv_timeout is paced by playback (the pump
                    // runs at most frameBufferDurationMs ahead), so on any stream
                    // under roughly 87 kbps every chunk resets the window, leaving
                    // window_bytes at 0 when the marker lands. Counting it turned
                    // a clean end-of-stream into a stall on every such track — a
                    // playback exception where the track simply finished.
                    let end_of_stream = data.is_empty();
                    self.window_bytes += data.len();
                    // Hand the just-drained buffer back to the reader for reuse.
                    // Only if it can hold a full chunk without reallocating — the
                    // first swap sees Vec::new() from the constructor, which is
                    // pointless to return. try_send never blocks: with the
                    // two-buffer working set the channel has room by construction
                    // (the reader just consumed the entry we're refilling), but a
                    // full channel simply means we drop the buf and the reader
                    // allocates a fresh one — correctness intact.
                    let spent = std::mem::replace(&mut self.leftover, data);
                    if spent.capacity() >= READ_CHUNK_BYTES {
                        let _ = self.returns.try_send(spent);
                    }
                    self.leftover_pos = 0;

                    // Evaluated on the same clock as the idle-gap timeout above,
                    // but only once a whole window has actually elapsed — so a
                    // source that goes quiet between chunks (a paused player never
                    // calls read at all) is never charged for time nobody asked
                    // it to spend.
                    if !end_of_stream && self.window_start.elapsed() >= timeout {
                        if self.window_bytes < MIN_WINDOW_BYTES {
                            return Err(io::Error::new(
                                io::ErrorKind::TimedOut,
                                "read stalled: throughput below the minimum floor",
                            ));
                        }
                        self.window_start = Instant::now();
                        self.window_bytes = 0;
                    }
                }
                Ok(Err(error)) => return Err(error),
                // The reader thread hasn't produced a byte in timeout — treated
                // the same as any other read error, so the caller's reconnect
                // logic kicks in instead of the track hanging until it is
                // reported stuck with no way to recover on its own.
                Err(RecvTimeoutError::Timeout) => {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "read stalled"))
                }
                Err(RecvTimeoutError::Disconnected) => return Ok(0),
            }
        }

        let available = &self.leftover[self.leftover_pos..];
        if available.is_empty() {
            return Ok(0);
        }
        let n = available.len().min(out.len());
        out[..n].copy_from_slice(&available[..n]);
        self.leftover_pos += n;
        Ok(n)
    }
}

impl HttpMediaSource {
    /// user_agent overrides the default when the resource must be fetched as the
    /// same client that negotiated its URL (see STREAM_USER_AGENT).
    pub fn open(
        url: &str,
        user_agent: Option<&str>,
        proxy: Option<reqwest::Proxy>,
    ) -> Result<Self, SourceError> {
        Self::open_with_timeouts(
            url,
            user_agent,
            proxy,
            CONNECT_TIMEOUT,
            READ_TIMEOUT,
            MAX_REQUEST_DURATION,
            Arc::new(AtomicBool::new(false)),
        )
    }

    /// connect_timeout is timeouts.connectTimeoutMs; read_timeout is
    /// timeouts.socketTimeoutMs — an idle-read stall threshold (Apache's
    /// SO_TIMEOUT), not an overall request timeout. request_duration is
    /// MAX_REQUEST_DURATION in production; only tests exercising it directly
    /// pass anything else.
    fn open_with_timeouts(
        url: &str,
        user_agent: Option<&str>,
        proxy: Option<reqwest::Proxy>,
        connect_timeout: Duration,
        read_timeout: Duration,
        request_duration: Duration,
        interrupt: Arc<AtomicBool>,
    ) -> Result<Self, SourceError> {
        // No client-level overall request timeout: this is a whole track, and a
        // long one is not a stuck one. reqwest::blocking has no idle-read timeout
        // of its own, so ReaderChannel::spawn below reads on a dedicated thread
        // and applies read_timeout on the receiving end — stalls surface as read
        // errors, same as a dropped connection. connect applies request_duration
        // per request instead, as a ceiling on how long any one request may run.
        // Every connect() below replaces self.reader outright rather than reusing
        // the old response, so a pooled idle connection is never actually reused —
        // it can only go stale and race a server that's already closed it (seen as
        // a flaky "error sending request" on reconnect/seek). Disabling the pool
        // trades nothing for removing that race.
        let mut builder = Client::builder()
            .connect_timeout(connect_timeout)
            .pool_max_idle_per_host(0)
            .user_agent(user_agent.unwrap_or(concat!("lavalink-rs/", env!("CARGO_PKG_VERSION"))));
        if let Some(proxy) = proxy {
            builder = builder.proxy(proxy);
        }
        let client = builder
            .build()
            .map_err(|error| SourceError::Internal(error.to_string()))?;

        let mut source = Self {
            client,
            url: url.to_owned(),
            length: None,
            seekable: false,
            position: 0,
            reader: Mutex::new(None),
            reconnect_attempts: 0,
            read_timeout,
            request_duration,
            interrupt,
        };
        source.connect(0)?;
        Ok(source)
    }

    /// (Re-)issues the request starting at offset.
    ///
    /// Always sent as a Range request, even at offset == 0 (bytes=0-): some
    /// CDNs (googlevideo's gir=yes URLs among them) 403 a rangeless GET but serve
    /// the same bytes fine once a Range header is present at all.
    fn connect(&mut self, offset: u64) -> Result<(), SourceError> {
        let request = self
            .client
            .get(&self.url)
            .timeout(self.request_duration)
            .header(RANGE, format!("bytes={offset}-"));

        let response = request
            .send()
            .map_err(|error| SourceError::Io(error.to_string()))?;

        let status = response.status();

        // A range request landing exactly at (or past) the end of the resource is a
        // valid "nothing left to read" answer, not a failure — symphonia's probe
        // seeks near the end of the stream for trailing metadata (ID3v1/APE),
        // which a seekable-but-short source can legitimately walk past.
        //
        // Only past an end we actually know, though. A 416 for a range within the
        // length the first response declared means the resource changed
        // underneath us (an expired or rotated CDN URL) — swallowing that as a
        // clean EOF would end the track mid-way as finished with no explanation,
        // so it falls through to the failure below instead. A source with unknown
        // length can never reach here: Seek refuses a seek from the end of one.
        let past_known_end = self.length.is_some_and(|length| offset >= length);
        if offset > 0 && status == StatusCode::RANGE_NOT_SATISFIABLE && past_known_end {
            self.position = offset;
            *self.lock_reader() = None;
            return Ok(());
        }

        if !status.is_success() {
            return Err(match status {
                StatusCode::NOT_FOUND | StatusCode::GONE => SourceError::NotFound,
                _ => SourceError::Remote {
                    status: status.as_u16(),
                    reason: status.canonical_reason().unwrap_or("failed").to_owned(),
                },
            });
        }

        // A range request that came back 200 means the server ignored the header and
        // is sending from byte zero. Playing that as if it were the requested offset
        // would be silently wrong, so treat it as unseekable instead.
        if offset > 0 && status != StatusCode::PARTIAL_CONTENT {
            return Err(SourceError::Unplayable {
                reason: "the server ignored the Range request".to_owned(),
            });
        }

        self.seekable = accepts_ranges(status, response.headers());

        // A reconnect that comes back reporting a different total than the first
        // response did means the resource changed underneath us — a proxy serving a
        // live-edge window, say — and treating its bytes as a continuation of the
        // same track would silently misalign playback. Refusing is safer than
        // guessing which length was right.
        if offset > 0 {
            if let (Some(expected), Some(actual)) = (self.length, total_length(&response)) {
                if expected != actual {
                    return Err(SourceError::Unplayable {
                        reason: "the server's resumed response reported a different length \
                                 than before"
                            .to_owned(),
                    });
                }
            }
        }

        if self.length.is_none() {
            self.length = total_length(&response);
        }

        self.position = offset;
        *self.lock_reader() = Some(ReaderChannel::spawn(response));
        Ok(())
    }

    fn lock_reader(&self) -> std::sync::MutexGuard<'_, Option<ReaderChannel>> {
        crate::lock(&self.reader)
    }
}

/// The resource's full length, from Content-Range when the response is partial and
/// Content-Length otherwise.
fn total_length(response: &Response) -> Option<u64> {
    let headers = response.headers();

    headers
        .get(CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        // "bytes 200-1000/67589"
        .and_then(|range| range.rsplit_once('/'))
        .and_then(|(_, total)| total.trim().parse().ok())
        .or_else(|| {
            headers
                .get(CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse().ok())
        })
}

impl Read for HttpMediaSource {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        loop {
            let result = {
                let mut reader = self.lock_reader();
                let Some(channel) = reader.as_mut() else {
                    return Ok(0);
                };
                channel.read(out, self.read_timeout)
            };

            let error = match result {
                Ok(read) => {
                    self.position += read as u64;
                    self.reconnect_attempts = 0;
                    return Ok(read);
                }
                Err(error) => error,
            };

            // A pump command is waiting: give up the remaining retry budget rather
            // than making it wait out however many attempts are left, each up to
            // connect_timeout + read_timeout. decode_loop treats this
            // particular error kind as "go check the command queue", not a real
            // failure — see its next_packet() match arm.
            if self.interrupt.load(Ordering::Relaxed) {
                return Err(io::Error::other(CommandPending));
            }

            // Only a seekable source can be resumed — without Range support a
            // reconnect would restart at byte zero and corrupt the stream far worse
            // than the dropped connection did.
            if !self.seekable || self.reconnect_attempts >= MAX_RECONNECT_ATTEMPTS {
                return Err(error);
            }

            self.reconnect_attempts += 1;
            tracing::debug!(
                %error,
                attempt = self.reconnect_attempts,
                position = self.position,
                "reconnecting after a mid-stream read error"
            );
            if self.connect(self.position).is_err() {
                return Err(error);
            }
        }
    }
}

impl Seek for HttpMediaSource {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let target = match from {
            SeekFrom::Start(offset) => offset,
            SeekFrom::Current(delta) => self.position.saturating_add_signed(delta),
            SeekFrom::End(delta) => {
                let length = self.length.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        "cannot seek from the end of a stream of unknown length",
                    )
                })?;
                length.saturating_add_signed(delta)
            }
        };

        if target == self.position {
            return Ok(target);
        }
        if !self.seekable {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "this server does not support range requests",
            ));
        }

        self.connect(target)
            .map_err(|error| io::Error::other(error.to_string()))?;
        Ok(target)
    }
}

impl MediaSource for HttpMediaSource {
    fn is_seekable(&self) -> bool {
        self.seekable
    }

    fn byte_len(&self) -> Option<u64> {
        self.length
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    use super::*;

    /// Reads one HTTP request off stream far enough to know it happened, ignoring
    /// its content — these tests only care about what the server sends back.
    fn consume_request(stream: &std::net::TcpStream) {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                break;
            }
        }
    }

    /// A server that answers the first request with a body cut short of what
    /// Content-Length promised — as if the connection dropped mid-stream — and the
    /// second (the resume, identified by its Range header) with the rest, as 200
    /// or 206 depending on resume_status.
    fn spawn_dropping_server(full_body: &'static [u8], cut_at: usize) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        std::thread::spawn(move || {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                consume_request(&stream);

                if attempt == 0 {
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\n\r\n",
                        full_body.len()
                    )
                    .unwrap();
                    stream.write_all(&full_body[..cut_at]).unwrap();
                    // Dropping here without sending the rest is the "body error":
                    // the client was promised more bytes than it received.
                } else {
                    let remaining = &full_body[cut_at..];
                    write!(
                        stream,
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {}-{}/{}\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\n\r\n",
                        cut_at,
                        full_body.len() - 1,
                        full_body.len(),
                        remaining.len()
                    )
                    .unwrap();
                    stream.write_all(remaining).unwrap();
                }
            }
        });

        format!("http://{addr}")
    }

    /// A server that answers the first request with the full body and Accept-Ranges,
    /// and any subsequent Range request with 416 Range Not Satisfiable — what a
    /// real server sends when the requested range starts at or past the resource's
    /// end.
    fn spawn_server_that_416s_past_eof(full_body: &'static [u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        std::thread::spawn(move || {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                consume_request(&stream);

                if attempt == 0 {
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\n\r\n",
                        full_body.len()
                    )
                    .unwrap();
                    stream.write_all(full_body).unwrap();
                } else {
                    write!(
                        stream,
                        "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\n\r\n",
                        full_body.len()
                    )
                    .unwrap();
                }
            }
        });

        format!("http://{addr}")
    }

    /// The bug this fix targets: symphonia 0.6's probe seeks near the end of a
    /// seekable source looking for trailing metadata (ID3v1/APE tags). A seek to
    /// (or past) the resource's actual end is answered by real servers with 416,
    /// which used to propagate straight out of connect as a hard failure —
    /// surfacing as Could not read the container: 416: Range Not Satisfiable
    /// even for an otherwise perfectly playable file. It must instead be treated
    /// as "nothing left to read from here", the same as any other clean EOF.
    #[test]
    fn a_seek_past_eof_reads_as_empty_instead_of_failing() {
        const BODY: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
        let url = spawn_server_that_416s_past_eof(BODY);

        let mut source = HttpMediaSource::open(&url, None, None).unwrap();
        assert!(source.is_seekable());

        source.seek(SeekFrom::End(0)).unwrap();

        let mut buffer = [0u8; 4096];
        let read = source.read(&mut buffer).unwrap();
        assert_eq!(read, 0, "a seek past EOF must read as empty, not error");
    }

    /// The bug this fix targets: connect treated every 416 at a non-zero
    /// offset as a clean end of stream, which is only true past the end of the
    /// resource. A resume for a range well inside the declared length gets a 416
    /// when the URL has expired or the resource rotated — googlevideo's do, hours
    /// after yt-dlp hands them over — and swallowing that as EOF ends the track
    /// as finished part way through, with no error anywhere to explain the
    /// truncation. It has to surface as a failure instead.
    #[test]
    fn a_416_inside_the_declared_length_is_a_failure_not_an_end_of_stream() {
        const BODY: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
        const CUT_AT: usize = 10;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        std::thread::spawn(move || {
            // One initial response plus every reconnect the retry budget allows.
            for attempt in 0..=MAX_RECONNECT_ATTEMPTS {
                let Ok((mut stream, _)) = listener.accept() else { return };
                consume_request(&stream);

                if attempt == 0 {
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\n\r\n",
                        BODY.len()
                    )
                    .unwrap();
                    // Short of what Content-Length promised: the connection drops
                    // mid-body and the source tries to resume from CUT_AT.
                    stream.write_all(&BODY[..CUT_AT]).unwrap();
                } else {
                    write!(
                        stream,
                        "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\n\r\n",
                        BODY.len()
                    )
                    .unwrap();
                }
            }
        });

        let mut source = HttpMediaSource::open(&format!("http://{addr}"), None, None).unwrap();
        assert_eq!(source.byte_len(), Some(BODY.len() as u64));

        let mut collected = Vec::new();
        let mut buffer = [0u8; 4096];
        let mut saw_error = false;
        loop {
            match source.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => collected.extend_from_slice(&buffer[..read]),
                Err(_) => {
                    saw_error = true;
                    break;
                }
            }
        }

        assert!(
            saw_error,
            "a 416 at byte {CUT_AT} of a {}-byte resource must fail, not read as a \
             clean end of stream after only {} bytes",
            BODY.len(),
            collected.len()
        );
    }

    /// The scenario from the bug report: a track that has already produced audio
    /// hits a mid-stream read error. Before this fix that error propagated straight
    /// out of read, which the pump turns into a track-ending Failed outcome even
    /// though the source could have kept going. Now the source reconnects with a
    /// Range request from where it left off and the read succeeds transparently.
    #[test]
    fn a_dropped_connection_is_resumed_with_a_range_request() {
        const BODY: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
        let url = spawn_dropping_server(BODY, 10);

        let mut source = HttpMediaSource::open(&url, None, None).unwrap();
        assert!(source.is_seekable());

        let mut collected = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            let read = source.read(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            collected.extend_from_slice(&buffer[..read]);
        }

        assert_eq!(collected, BODY);
    }

    /// The bug this fix targets: HttpMediaSource::read's reconnect loop had no
    /// way to know a pump command was waiting, so a stalled or dropped
    /// connection made a Seek/Stop/SetFilters wait out the whole reconnect
    /// budget — up to MAX_RECONNECT_ATTEMPTS attempts, each up to
    /// connect_timeout + read_timeout — before the pump ever got a chance to
    /// see it (pump.rs only checks for commands between packets). Setting
    /// interrupt (what pump.rs does the moment a command arrives) must make
    /// the very next read give up immediately instead of even trying to
    /// reconnect.
    #[test]
    fn an_interrupt_flag_skips_reconnecting_and_fails_fast() {
        const BODY: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
        const CUT_AT: usize = 10;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let reconnected = Arc::new(AtomicBool::new(false));
        let reconnected_flag = Arc::clone(&reconnected);

        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            consume_request(&stream);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\n\r\n",
                BODY.len()
            )
            .unwrap();
            stream.write_all(&BODY[..CUT_AT]).unwrap();
            drop(stream);

            // Only reached if the client reconnects despite the interrupt flag —
            // a resumed track never gets this far in the buggy version either,
            // since the reconnect there just takes longer, not never; the
            // assertion below is what actually proves the difference.
            if listener.accept().is_ok() {
                reconnected_flag.store(true, Ordering::Relaxed);
            }
        });

        let interrupt = Arc::new(AtomicBool::new(false));
        let mut source = HttpMediaSource::open_with_timeouts(
            &format!("http://{addr}"),
            None,
            None,
            CONNECT_TIMEOUT,
            Duration::from_millis(200),
            MAX_REQUEST_DURATION,
            Arc::clone(&interrupt),
        )
        .unwrap();
        assert!(source.is_seekable());

        let mut buffer = [0u8; 4096];
        let read = source.read(&mut buffer).unwrap();
        assert_eq!(&buffer[..read], &BODY[..CUT_AT]);

        // A pump command is now "pending" — set right before the read that
        // will hit the dropped connection and would otherwise reconnect.
        interrupt.store(true, Ordering::Relaxed);

        let started = Instant::now();
        let error = source.read(&mut buffer).unwrap_err();
        assert!(
            is_command_pending(&error),
            "expected the CommandPending marker, got {:?}: {error}",
            error.kind()
        );
        assert_ne!(
            error.kind(),
            io::ErrorKind::Interrupted,
            "symphonia's read_buf_exact swallows Interrupted and retries it forever, \
             so this must never be reported with that kind"
        );
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "the interrupt must skip straight past the reconnect attempt \
             instead of waiting out any part of its budget: took {:?}",
            started.elapsed()
        );

        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !reconnected.load(Ordering::Relaxed),
            "a pending command must prevent the reconnect from being attempted at all"
        );
    }

    /// The bug: this used to be ErrorKind::Interrupted, which is the one kind
    /// symphonia retries rather than propagates —
    /// MediaSourceStream::read_buf_exact swallows it and calls read again, and
    /// that is the path a packet body takes. So the interrupt never reached
    /// decode_loop from a real demuxer at all, and since only drain_commands
    /// clears the flag (which a pump inside next_packet() never reaches) and the
    /// check sits above the reconnect guard, the source could not recover either:
    /// a silent connection plus a pending stop retried until MAX_REQUEST_DURATION
    /// — six hours — with the pump thread, reader thread and socket pinned.
    ///
    /// pump.rs's own interrupt test cannot catch this: it uses a mock
    /// FormatReader, so real symphonia is never in the path. This drives the real
    /// thing, and asserts the error escapes on the first attempt rather than
    /// being retried at all.
    #[test]
    fn a_pending_command_escapes_symphonias_retry_loop() {
        use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions, ReadBytes};

        struct Stalling(Arc<std::sync::atomic::AtomicUsize>);

        impl Read for Stalling {
            fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
                if self.0.fetch_add(1, Ordering::Relaxed) >= 64 {
                    // Breaks a regressed retry loop, so this fails rather than
                    // hanging the suite.
                    return Err(io::Error::other("retried well past the first attempt"));
                }
                Err(io::Error::other(CommandPending))
            }
        }

        impl Seek for Stalling {
            fn seek(&mut self, _from: SeekFrom) -> io::Result<u64> {
                Ok(0)
            }
        }

        impl MediaSource for Stalling {
            fn is_seekable(&self) -> bool {
                false
            }

            fn byte_len(&self) -> Option<u64> {
                None
            }
        }

        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut stream = MediaSourceStream::new(
            Box::new(Stalling(Arc::clone(&reads))),
            MediaSourceStreamOptions::default(),
        );

        // Fully qualified: read_buf_exact collides with a std method name that
        // is still unstable, which unstable_name_collisions warns on.
        let mut packet = [0u8; 256];
        let error = ReadBytes::read_buf_exact(&mut stream, &mut packet).unwrap_err();

        assert!(
            is_command_pending(&error),
            "expected the CommandPending marker, got {:?}: {error}",
            error.kind()
        );
        assert_eq!(
            reads.load(Ordering::Relaxed),
            1,
            "symphonia must propagate this on the first read, not retry it"
        );
    }

    /// The bug this fix targets: a connection that stays open but stops sending
    /// bytes entirely (no error, no close — the failure mode behind the reported
    /// TrackStuckEvents on long-running streams). Before ReaderChannel, this
    /// hung in Response::read until the pump's own stuck-detector fired, and
    /// nothing about that detector could recover the track. Now a stall surfaces
    /// as a timeout, which the existing reconnect-with-Range path already
    /// handles exactly like a dropped connection.
    #[test]
    fn a_stalled_connection_is_reconnected_after_the_read_timeout() {
        const BODY: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
        const STALL_AT: usize = 10;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        std::thread::spawn(move || {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                consume_request(&stream);

                if attempt == 0 {
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\n\r\n",
                        BODY.len()
                    )
                    .unwrap();
                    stream.write_all(&BODY[..STALL_AT]).unwrap();
                    // Deliberately not sending the rest and not closing: the
                    // connection just goes quiet, which is what a stalled CDN
                    // edge looks like on the wire. 2.5x the 200ms read_timeout
                    // below is margin enough for scheduler jitter without
                    // paying for a full 2s of real sleep per test run.
                    std::thread::sleep(Duration::from_millis(500));
                } else {
                    let remaining = &BODY[STALL_AT..];
                    write!(
                        stream,
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {}-{}/{}\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\n\r\n",
                        STALL_AT,
                        BODY.len() - 1,
                        BODY.len(),
                        remaining.len()
                    )
                    .unwrap();
                    stream.write_all(remaining).unwrap();
                }
            }
        });

        let mut source = HttpMediaSource::open_with_timeouts(
            &format!("http://{addr}"),
            None,
            None,
            CONNECT_TIMEOUT,
            Duration::from_millis(200),
            MAX_REQUEST_DURATION,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        assert!(source.is_seekable());

        let mut collected = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            let read = source.read(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            collected.extend_from_slice(&buffer[..read]);
        }

        assert_eq!(collected, BODY);
    }

    /// The bug this fix targets: nothing keeps a JoinHandle for the reader
    /// thread ReaderChannel::spawn starts, so once a reconnect replaces it, the
    /// old thread's blocking Response::read() had no bound of its own and
    /// could stay parked forever on a connection that goes silent without
    /// closing — reqwest::blocking exposes no idle-read timeout in its public
    /// API. connect now puts a ceiling on the request itself
    /// (MAX_REQUEST_DURATION in production), so even a silent connection's read
    /// eventually returns instead of hanging. This test isolates that mechanism
    /// from the pre-existing idle-gap check by making read_timeout deliberately
    /// larger than the request ceiling, so only the new per-request bound can be
    /// what ends the read.
    #[test]
    fn a_silent_connection_is_bounded_by_the_request_ceiling_not_just_the_idle_gap() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            consume_request(&stream);
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 1000000\r\n\r\n").unwrap();
            stream.write_all(b"partial").unwrap();
            // Holds the connection open and silent for longer than the request
            // ceiling but well under the (deliberately larger) idle-gap timeout,
            // so only the ceiling can recover this — otherwise an implicit close
            // from the thread ending would let the test pass for the wrong
            // reason. 1s keeps that margin (5x the ceiling, a tenth of the
            // idle-gap) without sleeping this thread for a full 5s after the test
            // has finished with it.
            std::thread::sleep(Duration::from_secs(1));
        });

        let mut source = HttpMediaSource::open_with_timeouts(
            &format!("http://{addr}"),
            None,
            None,
            CONNECT_TIMEOUT,
            Duration::from_secs(10),
            Duration::from_millis(200),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        assert!(!source.is_seekable());

        let mut buffer = [0u8; 4096];
        let started = Instant::now();
        loop {
            match source.read(&mut buffer) {
                Ok(0) => break,
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the 200ms request ceiling, not the 10s idle-gap timeout or the \
             server's 5s hold, must be what ends the read"
        );
    }

    /// The bug this fix targets: a connection that never goes fully silent but
    /// also never makes real progress — a byte or two just before every idle-gap
    /// timeout, forever. Before the throughput floor this satisfied the previous
    /// test's exact protection on every single chunk and never failed, which is
    /// precisely the shape of connection that can pin the pump thread and this
    /// reader thread indefinitely: pump.rs's next_packet() only checks for a
    /// pending stop between finished packets, and a source that never finishes
    /// one never gives it the chance. Not seekable, so the error must surface on
    /// its own rather than through the reconnect path.
    #[test]
    fn a_trickling_connection_fails_the_throughput_floor() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            consume_request(&stream);
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 1000000\r\n\r\n").unwrap();
            // One byte every 60ms: comfortably faster than the 200ms idle-gap
            // timeout used below, but far under the throughput floor over any
            // full window of it.
            for _ in 0..40 {
                if stream.write_all(b"x").is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(60));
            }
        });

        let mut source = HttpMediaSource::open_with_timeouts(
            &format!("http://{addr}"),
            None,
            None,
            CONNECT_TIMEOUT,
            Duration::from_millis(200),
            MAX_REQUEST_DURATION,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        assert!(!source.is_seekable());

        let mut buffer = [0u8; 4096];
        let mut saw_error = false;
        for _ in 0..40 {
            match source.read(&mut buffer) {
                Ok(0) => break,
                Ok(_) => continue,
                Err(error) => {
                    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
                    saw_error = true;
                    break;
                }
            }
        }
        assert!(
            saw_error,
            "a trickle below the throughput floor must eventually fail the read"
        );
    }

    /// The bug: the end-of-stream marker (an empty chunk) was charged against the
    /// throughput floor like any other chunk. recv_timeout is paced by playback,
    /// so on a stream whose chunk lasts longer than the timeout every chunk resets
    /// the window and the marker arrives one gap later against a window_bytes of
    /// 0 — failing the floor. A 64 kbps mp3 over READ_CHUNK_BYTES does exactly
    /// that, deterministically, on every track: not seekable, so the error
    /// surfaces straight to decode_loop and a finished track is reported to the
    /// client as a playback exception instead.
    ///
    /// The pacing has to be on the consumer, which is where it is in production:
    /// the server hands over both chunks at once, and the reads are spread out the
    /// way the pump's are by a full ring. With a 300ms window and ~420ms spent
    /// draining each 300-byte chunk, the second chunk resets the window and the
    /// marker then arrives a full window later against a window_bytes of 0.
    #[test]
    fn a_clean_end_of_stream_is_not_charged_against_the_throughput_floor() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            consume_request(&stream);
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 600\r\n\r\n").unwrap();
            // Two writes rather than one, so the reader thread produces two
            // chunks and the first window actually gets to reset.
            for _ in 0..2 {
                if stream.write_all(&[b'x'; 300]).is_err() {
                    return;
                }
                stream.flush().ok();
                std::thread::sleep(Duration::from_millis(100));
            }
        });

        let mut source = HttpMediaSource::open_with_timeouts(
            &format!("http://{addr}"),
            None,
            None,
            CONNECT_TIMEOUT,
            Duration::from_millis(300),
            MAX_REQUEST_DURATION,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        assert!(!source.is_seekable());

        let mut buffer = [0u8; 50];
        let mut read_total = 0usize;
        loop {
            match source.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => read_total += read,
                Err(error) => panic!(
                    "a stream that ended cleanly must report EOF, not {:?}: {error}",
                    error.kind()
                ),
            }
            std::thread::sleep(Duration::from_millis(70));
        }
        assert_eq!(read_total, 600);
    }

    /// A source without range support cannot safely resume — reconnecting would
    /// restart at byte zero and silently duplicate audio — so the read error must
    /// still surface as an error rather than retry forever.
    #[test]
    fn a_non_seekable_source_does_not_retry_on_a_dropped_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            consume_request(&stream);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\n"
            )
            .unwrap();
            stream.write_all(b"short").unwrap();
        });

        let mut source = HttpMediaSource::open(&format!("http://{addr}"), None, None).unwrap();
        assert!(!source.is_seekable());

        let mut buffer = [0u8; 4096];
        let mut saw_error = false;
        loop {
            match source.read(&mut buffer) {
                Ok(0) => break,
                Ok(_) => continue,
                Err(_) => {
                    saw_error = true;
                    break;
                }
            }
        }
        assert!(saw_error, "expected the short body to surface as an error");
    }

    fn info(source: &str, identifier: &str) -> TrackInfo {
        TrackInfo {
            identifier: identifier.to_owned(),
            is_seekable: true,
            author: "a".into(),
            length: 0,
            is_stream: false,
            position: 0,
            title: "t".into(),
            uri: Some(identifier.to_owned()),
            source_name: source.to_owned(),
            artwork_url: None,
            isrc: None,
        }
    }

    /// Box<dyn MediaSource> is not Debug, so unwrap_err is unavailable.
    fn open_err(info: &TrackInfo) -> SourceError {
        match StreamOpener::default().open(info, Arc::new(AtomicBool::new(false))) {
            Err(error) => error,
            Ok(_) => panic!("expected opening to fail"),
        }
    }

    #[test]
    fn an_unknown_source_has_no_reader() {
        let error = open_err(&info("soundcloud", "123"));
        assert!(matches!(error, SourceError::Unplayable { .. }));
    }

    /// Without yt-dlp registered, a YouTube track cannot be opened — and says so,
    /// rather than failing somewhere less obvious.
    #[test]
    fn youtube_without_yt_dlp_is_unplayable() {
        let error = open_err(&info("youtube", "dQw4w9WgXcQ"));
        match error {
            SourceError::Unplayable { reason } => assert!(reason.contains("not enabled")),
            other => panic!("expected Unplayable, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_local_file_is_not_found() {
        let error = open_err(&info("local", "./definitely-not-here-8f3a.mp3"));
        assert!(matches!(error, SourceError::NotFound));
    }

    #[test]
    fn a_real_local_file_opens() {
        let path = std::env::temp_dir().join("lavalink-rs-stream-test.bin");
        std::fs::write(&path, b"not audio, but readable").unwrap();

        let source = StreamOpener::default()
            .open(
                &info("local", path.to_str().unwrap()),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
        assert!(source.is_seekable());
        assert_eq!(source.byte_len(), Some(23));

        std::fs::remove_file(&path).ok();
    }
}
