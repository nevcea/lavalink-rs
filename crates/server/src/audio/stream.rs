//! Opening a track's bytes for the pump.
//!
//! Produces a [`MediaSource`] — the demuxer's view of a byte stream — for each of
//! our sources. The interesting one is HTTP: whether it is seekable, and what
//! happens when a long-running stream drops mid-track.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError};
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

/// How long a single read may go without a byte before it is treated as stalled.
///
/// Comfortably under `trackStuckThresholdMs`'s 10s default, so a stall is caught
/// and reconnected (see `MAX_RECONNECT_ATTEMPTS`) before the player ever has to
/// report the track stuck.
const READ_TIMEOUT: Duration = Duration::from_secs(6);

/// How many times in a row a dropped connection is re-established mid-track before
/// the read is allowed to fail for good. A single blip (the "request or response
/// body error" a proxy throws mid-stream) would otherwise end a track that has
/// barely started, exactly like a moment of decoder noise would if the pump didn't
/// tolerate a run of those either.
const MAX_RECONNECT_ATTEMPTS: u32 = 3;

/// Opens byte streams for resolved tracks.
///
/// Holds the yt-dlp handle because those sources' media URLs are not stored — they
/// expire in hours, so they are resolved again here, at playback time.
pub struct StreamOpener {
    ytdlp: Option<Arc<YtDlp>>,
    proxy: Option<reqwest::Proxy>,
    /// `lavalink.server.timeouts.connectTimeoutMs`.
    connect_timeout: Duration,
    /// `lavalink.server.timeouts.socketTimeoutMs` — the idle-read stall threshold
    /// (Apache's `SO_TIMEOUT`), not an overall request timeout.
    read_timeout: Duration,
}

/// Matches the constructor a zero-config `StreamOpener` gets everywhere except
/// `main.rs`: no yt-dlp, no proxy, and the same timeouts production used before
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

    pub fn open(&self, info: &TrackInfo) -> Result<Box<dyn MediaSource>, SourceError> {
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
                self.open_http(&url, None)
            }
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
                // googlevideo.com 403s a mismatch. See `STREAM_USER_AGENT`.
                self.open_http(&url, Some(STREAM_USER_AGENT))
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
                self.open_http(&url, Some(STREAM_USER_AGENT))
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
    ) -> Result<Box<dyn MediaSource>, SourceError> {
        Ok(Box::new(HttpMediaSource::open_with_timeouts(
            url,
            user_agent,
            self.proxy.clone(),
            self.connect_timeout,
            self.read_timeout,
        )?))
    }
}

/// An HTTP resource as a seekable byte stream.
///
/// Seeking re-issues the request with a `Range` header. That is the only way to move
/// backwards in a stream we are not storing, and it is why seek support is reported
/// from `Accept-Ranges` rather than assumed: on a server without it, symphonia would
/// otherwise ask for a seek that silently returns the wrong bytes.
pub struct HttpMediaSource {
    client: Client,
    url: String,
    /// `None` for a live stream with no declared length.
    length: Option<u64>,
    seekable: bool,
    position: u64,
    /// Behind a mutex purely to satisfy [`MediaSource`]'s `Sync` bound — the pump
    /// owns this exclusively and never shares it.
    reader: Mutex<Option<ReaderChannel>>,
    /// Consecutive reconnects since the last byte actually read. Resets on any
    /// successful read, so it counts a run of failures, not a track's total.
    reconnect_attempts: u32,
    /// `READ_TIMEOUT` in production; shrunk in tests so a stall scenario doesn't
    /// have to burn the real multi-second timeout to exercise it.
    read_timeout: Duration,
}

/// Bytes off a socket, one chunk at a time, from a dedicated thread — so the
/// consumer can bound how long it waits for the next one via `recv_timeout`.
/// `reqwest::blocking` has no idle-read timeout of its own, and a plain blocking
/// `Response::read` can hang forever on a connection that stopped sending without
/// closing, which is exactly the failure mode a stalled CDN edge produces.
struct ReaderChannel {
    chunks: Receiver<io::Result<Vec<u8>>>,
    /// Bytes already received but not yet handed to the caller of `read`.
    leftover: Vec<u8>,
    leftover_pos: usize,
    /// Bytes received since `window_start`, for the throughput floor below.
    window_bytes: usize,
    window_start: Instant,
}

/// Chunk size for the reader thread's own reads. Unrelated to the caller's buffer
/// size — it only bounds how much a single stalled `recv_timeout` can be behind.
const READ_CHUNK_BYTES: usize = 64 * 1024;

/// The least a source may deliver within one `read_timeout` window and still count
/// as alive.
///
/// The idle-gap timeout alone only catches a source that goes fully silent; one
/// that sends a byte or two just before every timeout never trips it, while still
/// pinning the pump thread and this reader thread indefinitely — `next_packet()`
/// only checks for a pending stop between packets, so a source that never finishes
/// a packet never gives the pump a chance to notice it was asked to stop. Set far
/// below any real stream's bitrate (even an 8kbps low-bitrate radio feed is
/// roughly 6 000 bytes over `READ_TIMEOUT`) so this only catches a source making
/// essentially no progress, not a genuinely slow one.
const MIN_WINDOW_BYTES: usize = 256;

impl ReaderChannel {
    fn spawn(mut response: Response) -> Self {
        let (tx, rx) = sync_channel(2);
        std::thread::spawn(move || {
            let mut buf = vec![0u8; READ_CHUNK_BYTES];
            loop {
                let outcome = response.read(&mut buf).map(|n| buf[..n].to_vec());
                let at_end = matches!(&outcome, Ok(data) if data.is_empty()) || outcome.is_err();
                if tx.send(outcome).is_err() || at_end {
                    // Either nobody is listening any more (a reconnect replaced
                    // this reader before it noticed) or the response is done.
                    return;
                }
            }
        });
        Self {
            chunks: rx,
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
                    self.window_bytes += data.len();
                    self.leftover = data;
                    self.leftover_pos = 0;

                    // Evaluated on the same clock as the idle-gap timeout above,
                    // but only once a whole window has actually elapsed — so a
                    // source that goes quiet between chunks (a paused player never
                    // calls `read` at all) is never charged for time nobody asked
                    // it to spend.
                    if self.window_start.elapsed() >= timeout {
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
                // The reader thread hasn't produced a byte in `timeout` — treated
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
    /// `user_agent` overrides the default when the resource must be fetched as the
    /// same client that negotiated its URL (see `STREAM_USER_AGENT`).
    pub fn open(
        url: &str,
        user_agent: Option<&str>,
        proxy: Option<reqwest::Proxy>,
    ) -> Result<Self, SourceError> {
        Self::open_with_timeouts(url, user_agent, proxy, CONNECT_TIMEOUT, READ_TIMEOUT)
    }

    /// `connect_timeout` is `timeouts.connectTimeoutMs`; `read_timeout` is
    /// `timeouts.socketTimeoutMs` — an idle-read stall threshold (Apache's
    /// `SO_TIMEOUT`), not an overall request timeout.
    fn open_with_timeouts(
        url: &str,
        user_agent: Option<&str>,
        proxy: Option<reqwest::Proxy>,
        connect_timeout: Duration,
        read_timeout: Duration,
    ) -> Result<Self, SourceError> {
        let mut builder = Client::builder()
            .connect_timeout(connect_timeout)
            // No overall request timeout: this is a whole track, and a long one is
            // not a stuck one. `reqwest::blocking` has no idle read timeout of its
            // own (unlike the async client), so `spawn_reader` below reads on a
            // dedicated thread and applies `READ_TIMEOUT` on the receiving end —
            // stalls surface as read errors, same as a dropped connection would.
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
        };
        source.connect(0)?;
        Ok(source)
    }

    /// (Re-)issues the request starting at `offset`.
    fn connect(&mut self, offset: u64) -> Result<(), SourceError> {
        let mut request = self.client.get(&self.url);
        if offset > 0 {
            request = request.header(RANGE, format!("bytes={offset}-"));
        }

        let response = request
            .send()
            .map_err(|error| SourceError::Io(error.to_string()))?;

        let status = response.status();
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

/// The resource's full length, from `Content-Range` when the response is partial and
/// `Content-Length` otherwise.
fn total_length(response: &Response) -> Option<u64> {
    let headers = response.headers();

    if let Some(range) = headers.get(CONTENT_RANGE).and_then(|v| v.to_str().ok()) {
        // "bytes 200-1000/67589"
        if let Some((_, total)) = range.rsplit_once('/') {
            if let Ok(total) = total.trim().parse::<u64>() {
                return Some(total);
            }
        }
    }

    headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
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

    /// Reads one HTTP request off `stream` far enough to know it happened, ignoring
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
    /// `Content-Length` promised — as if the connection dropped mid-stream — and the
    /// second (the resume, identified by its `Range` header) with the rest, as `200`
    /// or `206` depending on `resume_status`.
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

    /// The scenario from the bug report: a track that has already produced audio
    /// hits a mid-stream read error. Before this fix that error propagated straight
    /// out of `read`, which the pump turns into a track-ending `Failed` outcome even
    /// though the source could have kept going. Now the source reconnects with a
    /// `Range` request from where it left off and the read succeeds transparently.
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

    /// The bug this fix targets: a connection that stays open but stops sending
    /// bytes entirely (no error, no close — the failure mode behind the reported
    /// `TrackStuckEvent`s on long-running streams). Before `ReaderChannel`, this
    /// hung in `Response::read` until the pump's own stuck-detector fired, and
    /// nothing about that detector could recover the track. Now a stall surfaces
    /// as a timeout, which the existing reconnect-with-`Range` path already
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
                    // edge looks like on the wire.
                    std::thread::sleep(Duration::from_secs(2));
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

    /// The bug this fix targets: a connection that never goes fully silent but
    /// also never makes real progress — a byte or two just before every idle-gap
    /// timeout, forever. Before the throughput floor this satisfied the previous
    /// test's exact protection on every single chunk and never failed, which is
    /// precisely the shape of connection that can pin the pump thread and this
    /// reader thread indefinitely: `pump.rs`'s `next_packet()` only checks for a
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

    /// `Box<dyn MediaSource>` is not `Debug`, so `unwrap_err` is unavailable.
    fn open_err(info: &TrackInfo) -> SourceError {
        match StreamOpener::default().open(info) {
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
            .open(&info("local", path.to_str().unwrap()))
            .unwrap();
        assert!(source.is_seekable());
        assert_eq!(source.byte_len(), Some(23));

        std::fs::remove_file(&path).ok();
    }
}
