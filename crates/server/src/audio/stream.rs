//! Opening a track's bytes for the pump.
//!
//! Produces a MediaSource — the demuxer's view of a byte stream — for each of
//! our sources. The interesting one is HTTP: whether it is seekable, and what
//! happens when a long-running stream drops mid-track.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lavalink_protocol::player::TrackInfo;
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, RANGE};
use reqwest::{Client, Response, StatusCode};
use symphonia::core::io::MediaSource;

use super::source::http::accepts_ranges;
use super::source::youtube;
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
/// symphonia retrying it forever with the pump thread and socket pinned.
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
                let page_url = match kind {
                    SourceKind::YouTube => {
                        youtube::playback_url(&info.identifier, info.uri.as_deref())
                    }
                    _ => kind.playback_url(&info.identifier),
                };
                let url = ytdlp.resolve_stream_url(&page_url)?;
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
    /// Drives async reqwest on the pump's existing dedicated thread. No helper
    /// thread is left behind when a timed-out response is replaced.
    runtime: tokio::runtime::Runtime,
    url: String,
    /// None for a live stream with no declared length.
    length: Option<u64>,
    seekable: bool,
    position: u64,
    /// Behind a mutex purely to satisfy MediaSource's Sync bound — the pump
    /// owns this exclusively and never shares it.
    reader: Mutex<Option<ResponseReader>>,
    /// Consecutive reconnects since the last byte actually read. Resets on any
    /// successful read, so it counts a run of failures, not a track's total.
    reconnect_attempts: u32,
    /// READ_TIMEOUT in production; shrunk in tests so a stall scenario doesn't
    /// have to burn the real multi-second timeout to exercise it.
    read_timeout: Duration,
    /// Set by the pump whenever a command (Seek, Stop, ...) is waiting to be
    /// applied. Checked between reconnect attempts so a stalled connection gives
    /// up its remaining retry budget immediately instead of making the command
    /// wait out the whole thing — up to MAX_RECONNECT_ATTEMPTS full
    /// connect-and-stall cycles, tens of seconds, otherwise. Cleared by the pump
    /// once it has drained the commands that set it.
    interrupt: Arc<AtomicBool>,
}

/// One async response, read synchronously by the owning pump thread. Reqwest's
/// native idle-read timeout cancels the socket read itself, so reconnecting drops
/// the old response without abandoning an OS thread in blocking I/O.
struct ResponseReader {
    response: Response,
    /// Bytes already received but not yet handed to the caller of read.
    leftover: Vec<u8>,
    leftover_pos: usize,
    /// Bytes received since window_start, for the throughput floor below.
    window_bytes: usize,
    window_start: Instant,
}

/// The least a source may deliver within one read_timeout window and still count
/// as alive.
///
/// The idle-gap timeout alone only catches a source that goes fully silent; one
/// that sends a byte or two just before every timeout never trips it, while still
/// pinning the pump thread indefinitely — next_packet()
/// only checks for a pending stop between packets, so a source that never finishes
/// a packet never gives the pump a chance to notice it was asked to stop. Set far
/// below any real stream's bitrate (even an 8kbps low-bitrate radio feed is
/// roughly 6 000 bytes over READ_TIMEOUT) so this only catches a source making
/// essentially no progress, not a genuinely slow one.
const MIN_WINDOW_BYTES: usize = 256;

impl ResponseReader {
    fn new(response: Response) -> Self {
        Self {
            response,
            leftover: Vec::new(),
            leftover_pos: 0,
            window_bytes: 0,
            window_start: Instant::now(),
        }
    }

    fn read(
        &mut self,
        out: &mut [u8],
        timeout: Duration,
        runtime: &tokio::runtime::Runtime,
    ) -> io::Result<usize> {
        if self.leftover_pos >= self.leftover.len() {
            match runtime.block_on(async { self.response.chunk().await }) {
                Ok(Some(data)) => {
                    self.window_bytes += data.len();
                    self.leftover.clear();
                    self.leftover.extend_from_slice(&data);
                    self.leftover_pos = 0;

                    // Evaluated on the same clock as the idle-read timeout,
                    // but only once a whole window has actually elapsed — so a
                    // source that goes quiet between chunks (a paused player never
                    // calls read at all) is never charged for time nobody asked
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
                Ok(None) => return Ok(0),
                Err(error) => return Err(io::Error::other(error)),
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
            Arc::new(AtomicBool::new(false)),
        )
    }

    /// connect_timeout is timeouts.connectTimeoutMs; read_timeout is
    /// timeouts.socketTimeoutMs — an idle-read stall threshold (Apache's
    /// SO_TIMEOUT), not an overall request timeout.
    fn open_with_timeouts(
        url: &str,
        user_agent: Option<&str>,
        proxy: Option<reqwest::Proxy>,
        connect_timeout: Duration,
        read_timeout: Duration,
        interrupt: Arc<AtomicBool>,
    ) -> Result<Self, SourceError> {
        // No client-level overall request timeout: this is a whole track, and a
        // long one is not a stuck one. Async reqwest exposes the idle-read timeout
        // the blocking wrapper does not, so a silent response is cancelled at the
        // socket read itself instead of abandoning a blocking reader thread.
        // Every connect() below replaces self.reader outright rather than reusing
        // the old response, so a pooled idle connection is never actually reused —
        // it can only go stale and race a server that's already closed it (seen as
        // a flaky "error sending request" on reconnect/seek). Disabling the pool
        // trades nothing for removing that race.
        let mut builder = Client::builder()
            .connect_timeout(connect_timeout)
            .read_timeout(read_timeout)
            .pool_max_idle_per_host(0)
            .user_agent(user_agent.unwrap_or(concat!("lavalink-rs/", env!("CARGO_PKG_VERSION"))));
        if let Some(proxy) = proxy {
            builder = builder.proxy(proxy);
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| SourceError::Internal(error.to_string()))?;
        let client = runtime
            .block_on(async move { builder.build() })
            .map_err(|error| SourceError::Internal(error.to_string()))?;

        let mut source = Self {
            client,
            runtime,
            url: url.to_owned(),
            length: None,
            seekable: false,
            position: 0,
            reader: Mutex::new(None),
            reconnect_attempts: 0,
            read_timeout,
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
            .header(RANGE, format!("bytes={offset}-"));

        let response = self
            .runtime
            .block_on(async move { request.send().await })
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
        *self.lock_reader() = Some(ResponseReader::new(response));
        Ok(())
    }

    fn lock_reader(&self) -> std::sync::MutexGuard<'_, Option<ResponseReader>> {
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
                channel.read(out, self.read_timeout, &self.runtime)
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

            let mut last_error = error;
            loop {
                // Only a seekable source can be resumed — without Range support a
                // reconnect would restart at byte zero and corrupt the stream far
                // worse than the dropped connection did.
                if !self.seekable || self.reconnect_attempts >= MAX_RECONNECT_ATTEMPTS {
                    return Err(last_error);
                }
                if self.interrupt.load(Ordering::Relaxed) {
                    return Err(io::Error::other(CommandPending));
                }

                self.reconnect_attempts += 1;
                tracing::debug!(
                    %last_error,
                    attempt = self.reconnect_attempts,
                    position = self.position,
                    "reconnecting after a mid-stream read error"
                );
                match self.connect(self.position) {
                    Ok(()) => break,
                    Err(error) => last_error = io::Error::other(error.to_string()),
                }
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

    fn listening_server() -> (TcpListener, String) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        (listener, url)
    }

    /// A server that answers the first request with a body cut short of what
    /// Content-Length promised — as if the connection dropped mid-stream — and the
    /// second (the resume, identified by its Range header) with the rest, as 200
    /// or 206 depending on resume_status.
    fn spawn_dropping_server(full_body: &'static [u8], cut_at: usize) -> String {
        let (listener, url) = listening_server();

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

        url
    }

    /// A server that answers the first request with the full body and Accept-Ranges,
    /// and any subsequent Range request with 416 Range Not Satisfiable — what a
    /// real server sends when the requested range starts at or past the resource's
    /// end.
    fn spawn_server_that_416s_past_eof(full_body: &'static [u8]) -> String {
        let (listener, url) = listening_server();

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

        url
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

        let (listener, url) = listening_server();

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

        let mut source = HttpMediaSource::open(&url, None, None).unwrap();
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

    #[test]
    fn a_failed_reconnect_uses_the_remaining_retry_budget() {
        const BODY: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
        const CUT_AT: usize = 10;
        let (listener, url) = listening_server();

        std::thread::spawn(move || {
            for attempt in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                consume_request(&stream);
                match attempt {
                    0 => {
                        write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\n\r\n",
                            BODY.len()
                        )
                        .unwrap();
                        stream.write_all(&BODY[..CUT_AT]).unwrap();
                    }
                    1 | 2 => {
                        write!(stream, "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n")
                            .unwrap();
                    }
                    _ => {
                        let remaining = &BODY[CUT_AT..];
                        write!(
                            stream,
                            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {}-{}/{}\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\n\r\n",
                            CUT_AT,
                            BODY.len() - 1,
                            BODY.len(),
                            remaining.len()
                        )
                        .unwrap();
                        stream.write_all(remaining).unwrap();
                    }
                }
            }
        });

        let mut source = HttpMediaSource::open(&url, None, None).unwrap();
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

        let (listener, url) = listening_server();
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
            &url,
            None,
            None,
            CONNECT_TIMEOUT,
            Duration::from_millis(200),
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
    /// a silent connection plus a pending stop retried forever with the pump
    /// thread and socket pinned.
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

    /// A connection that stays open but stops sending bytes entirely is cancelled
    /// by reqwest's native idle-read timeout, then resumed with a Range request.
    #[test]
    fn a_stalled_connection_is_reconnected_after_the_read_timeout() {
        const BODY: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
        const STALL_AT: usize = 10;

        let (listener, url) = listening_server();

        std::thread::spawn(move || {
            let (mut stalled, _) = listener.accept().unwrap();
            consume_request(&stalled);
            write!(
                stalled,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\n\r\n",
                BODY.len()
            )
            .unwrap();
            stalled.write_all(&BODY[..STALL_AT]).unwrap();
            std::thread::spawn(move || {
                // Keep the first connection open and silent beyond the client's
                // idle timeout while the server accepts its resumed request.
                std::thread::sleep(Duration::from_millis(500));
                drop(stalled);
            });

            let (mut resumed, _) = listener.accept().unwrap();
            consume_request(&resumed);
            let remaining = &BODY[STALL_AT..];
            write!(
                resumed,
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {}-{}/{}\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\n\r\n",
                STALL_AT,
                BODY.len() - 1,
                BODY.len(),
                remaining.len()
            )
            .unwrap();
            resumed.write_all(remaining).unwrap();
        });

        let mut source = HttpMediaSource::open_with_timeouts(
            &url,
            None,
            None,
            CONNECT_TIMEOUT,
            Duration::from_millis(200),
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

    /// The bug this fix targets: a connection that never goes fully silent but
    /// also never makes real progress — a byte or two just before every idle-gap
    /// timeout, forever. Before the throughput floor this satisfied the previous
    /// test's exact protection on every single chunk and never failed, which is
    /// precisely the shape of connection that can pin the pump thread
    /// indefinitely: pump.rs's next_packet() only checks for a
    /// pending stop between finished packets, and a source that never finishes
    /// one never gives it the chance. Not seekable, so the error must surface on
    /// its own rather than through the reconnect path.
    #[test]
    fn a_trickling_connection_fails_the_throughput_floor() {
        let (listener, url) = listening_server();

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
            &url,
            None,
            None,
            CONNECT_TIMEOUT,
            Duration::from_millis(200),
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

    /// A clean EOF is not a zero-byte throughput sample. It must return directly,
    /// otherwise a low-bitrate response whose previous chunk reset the window can
    /// be reported as a stall instead of a finished track.
    ///
    /// The pacing has to be on the consumer, which is where it is in production:
    /// the server hands over both chunks at once, and the reads are spread out the
    /// way the pump's are by a full ring. With a 300ms window and ~420ms spent
    /// draining each 300-byte chunk, the second chunk resets the window and the
    /// marker then arrives a full window later against a window_bytes of 0.
    #[test]
    fn a_clean_end_of_stream_is_not_charged_against_the_throughput_floor() {
        let (listener, url) = listening_server();

        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            consume_request(&stream);
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 600\r\n\r\n").unwrap();
            // Two writes rather than one, so two response chunks give the first
            // window a chance to reset.
            for _ in 0..2 {
                if stream.write_all(&[b'x'; 300]).is_err() {
                    return;
                }
                stream.flush().ok();
                std::thread::sleep(Duration::from_millis(100));
            }
        });

        let mut source = HttpMediaSource::open_with_timeouts(
            &url,
            None,
            None,
            CONNECT_TIMEOUT,
            Duration::from_millis(300),
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
        let (listener, url) = listening_server();

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

        let mut source = HttpMediaSource::open(&url, None, None).unwrap();
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
