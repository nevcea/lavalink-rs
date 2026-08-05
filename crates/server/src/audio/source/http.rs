//! Direct HTTP(S) media source.
//!
//! # Probing without downloading
//!
//! A track can be an hour long; the metadata is in the first few kilobytes. This
//! fetches a bounded prefix and probes that. The cost is that formats which put
//! their index at the end — a WebM whose cues trail the stream, an MP3 with no
//! Xing header — report a duration of 0 rather than a wrong one, which is what the
//! original reports for them too.
//!
//! # Seekability
//!
//! Comes from `Accept-Ranges`, not from optimism. Without range support a seek would
//! mean re-fetching from byte zero, so the track is advertised as non-seekable and
//! clients do not offer the control.

use std::io::Read;

use lavalink_protocol::encoded_track::SourceTail;
use lavalink_protocol::player::TrackInfo;
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_TYPE, RANGE};
use reqwest::StatusCode;

use super::probe::probe;
use super::{
    build_client, classify_status, extension_of, last_path_segment, SourceError, SourceLoad,
    SourceManager, SourceTrack,
};

/// How much of the stream to pull for probing.
const PROBE_PREFIX_BYTES: u64 = 512 * 1024;

pub struct HttpSource {
    client: Client,
}

impl HttpSource {
    pub fn new(proxy: Option<reqwest::Proxy>) -> Result<Self, SourceError> {
        Ok(Self { client: build_client(proxy)? })
    }
}

impl SourceManager for HttpSource {
    fn name(&self) -> &'static str {
        "http"
    }

    fn matches(&self, identifier: &str) -> bool {
        identifier.starts_with("http://") || identifier.starts_with("https://")
    }

    fn load(&self, identifier: &str) -> Result<SourceLoad, SourceError> {
        let response = self
            .client
            .get(identifier)
            .header(RANGE, format!("bytes=0-{}", PROBE_PREFIX_BYTES - 1))
            .send()
            .map_err(|error| SourceError::Io(error.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            return Err(classify_status(status));
        }

        let headers = response.headers();

        let is_seekable = accepts_ranges(status, headers);

        // A live stream has no length, and a stream must not be advertised as
        // seekable — the original refuses to seek one.
        let content_length = headers
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        let is_stream = content_length.is_none() && !is_seekable;

        if let Some(content_type) = headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()) {
            if is_definitely_not_media(content_type) {
                // `NotFound`, which becomes `loadType: "empty"` — not an error.
                //
                // This source claims every http(s) URL, so it is where a link
                // belonging to a source this node does not run ends up: a YouTube
                // URL on a node without yt-dlp arrives here and fetches a web page.
                // An unsupported source is 200 + "empty", and clients act on that
                // distinction — "empty" is "no results", while "error" reads as the
                // node being broken.
                return Err(SourceError::NotFound);
            }
        }

        let extension = extension_of(identifier);
        // Bounded by construction, not by trusting the server: a `Range` header is
        // only a request, and a server that ignores it and answers `200` with the
        // full body would otherwise have every `loadtracks` call against it buffer
        // an entire (possibly multi-gigabyte) file in memory rather than the
        // "bounded prefix" this module's own docs promise.
        let mut body = Vec::new();
        response
            .take(PROBE_PREFIX_BYTES)
            .read_to_end(&mut body)
            .map_err(|error| SourceError::Io(error.to_string()))?;
        if body.is_empty() {
            return Err(SourceError::Unplayable {
                reason: "the response body was empty".to_owned(),
            });
        }

        let probed = probe(Box::new(std::io::Cursor::new(body)), extension.as_deref())?;

        Ok(SourceLoad::Track(SourceTrack {
            info: TrackInfo {
                identifier: identifier.to_owned(),
                is_seekable: is_seekable && !is_stream,
                author: probed.author.unwrap_or_else(|| "Unknown artist".to_owned()),
                length: probed.duration_ms,
                is_stream,
                position: 0,
                title: probed
                    .title
                    .unwrap_or_else(|| last_path_segment(identifier).to_owned()),
                uri: Some(identifier.to_owned()),
                source_name: self.name().to_owned(),
                artwork_url: None,
                isrc: probed.isrc,
            },
            tail: SourceTail::Probe(probed.container),
        }))
    }
}

/// Whether a response shows the server honours range requests.
///
/// A 206 is proof it already did; `Accept-Ranges: bytes` is a promise it would. Both
/// the load-time probe and the playback-time reader need this, and they must agree —
/// advertising a track as seekable that the reader then cannot seek would strand a
/// client's seek control.
pub fn accepts_ranges(status: StatusCode, headers: &reqwest::header::HeaderMap) -> bool {
    status == StatusCode::PARTIAL_CONTENT
        || headers
            .get(ACCEPT_RANGES)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.eq_ignore_ascii_case("bytes"))
}

/// Content types that are certainly not playable — a web page rather than a track.
///
/// Deliberately a denylist: servers mislabel audio as `application/octet-stream`
/// constantly, and rejecting on an allowlist would break URLs the original plays.
/// The probe is the real gate; this only catches the "that's a website" case early,
/// so it can be reported as "no results" rather than as a failure.
fn is_definitely_not_media(content_type: &str) -> bool {
    let content_type = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    matches!(
        content_type.as_str(),
        "text/html" | "text/plain" | "application/json" | "application/xml" | "text/xml"
    )
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    use super::*;

    fn source() -> HttpSource {
        HttpSource::new(None).unwrap()
    }

    /// A server that ignores the probe's `Range` header, answers `200` (not `206`),
    /// declares a `Content-Length` far larger than `PROBE_PREFIX_BYTES`, but only
    /// ever sends `PROBE_PREFIX_BYTES` before dropping the connection — standing in
    /// for a multi-gigabyte file behind a host that doesn't honor `Range`.
    ///
    /// If the probe ever goes back to reading the whole declared body (`response
    /// .bytes()`), this becomes exactly the short-read-against-`Content-Length`
    /// case `stream.rs`'s own tests already cover, and surfaces as
    /// `SourceError::Io` instead of stopping cleanly at the bound.
    fn spawn_range_ignoring_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            {
                let mut reader = BufReader::new(&stream);
                let mut line = String::new();
                loop {
                    line.clear();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                }
            }

            let declared_len = PROBE_PREFIX_BYTES * 10;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {declared_len}\r\nContent-Type: audio/mpeg\r\n\r\n",
            )
            .unwrap();
            stream
                .write_all(&vec![0u8; PROBE_PREFIX_BYTES as usize])
                .unwrap();
            // Connection drops here, far short of `declared_len`.
        });

        format!("http://{addr}")
    }

    #[test]
    fn the_probe_never_reads_past_its_bounded_prefix() {
        let source = source();
        let url = spawn_range_ignoring_server();

        let result = source.load(&url);

        assert!(
            !matches!(result, Err(SourceError::Io(_))),
            "expected the probe to stop at PROBE_PREFIX_BYTES rather than trying to \
             read the full (short-delivered) declared Content-Length, got {result:?}"
        );
    }

    #[test]
    fn matches_only_http_urls() {
        let source = source();
        assert!(source.matches("http://example.invalid/a.mp3"));
        assert!(source.matches("https://example.invalid/a.mp3"));
        assert!(!source.matches("/tmp/a.mp3"));
        assert!(!source.matches("ytsearch:never gonna"));
    }

    /// A link to a source this node does not run must read as "no results", because
    /// that is what a client can act on.
    #[test]
    fn a_web_page_is_reported_as_no_results_not_as_a_failure() {
        assert!(matches!(
            SourceError::NotFound.to_exception().severity,
            lavalink_protocol::Severity::Common
        ));
        // The content-type gate is what routes a YouTube URL on a yt-dlp-less node
        // to `empty` rather than `error`.
        assert!(is_definitely_not_media("text/html; charset=utf-8"));
    }

    #[test]
    fn html_is_rejected_but_octet_stream_is_not() {
        assert!(is_definitely_not_media("text/html; charset=utf-8"));
        assert!(is_definitely_not_media("application/json"));
        assert!(!is_definitely_not_media("application/octet-stream"));
        assert!(!is_definitely_not_media("audio/mpeg"));
    }

    #[test]
    fn extensions_come_from_the_path_not_the_query() {
        assert_eq!(extension_of("https://a.invalid/b/c.mp3").as_deref(), Some("mp3"));
        assert_eq!(
            extension_of("https://a.invalid/b/c.MP3?token=x").as_deref(),
            Some("mp3")
        );
        assert_eq!(extension_of("https://a.invalid/stream"), None);
        assert_eq!(extension_of("https://a.invalid/"), None);
    }

    #[test]
    fn the_fallback_title_is_the_file_name() {
        assert_eq!(last_path_segment("https://a.invalid/b/song.mp3"), "song.mp3");
        assert_eq!(last_path_segment("https://a.invalid/b/song.mp3?x=1"), "song.mp3");
    }
}
