//! The yt-dlp subprocess, shared by every source that resolves through it.
//!
//! This module knows how to run yt-dlp and read what it prints. It does not know
//! which sites exist — youtube and
//! soundcloud own their own URL shapes and hand this layer a
//! URL and a SourceKind.
//!
//! A runtime-optional dependency
//!
//! yt-dlp is detected at startup and, if it is missing, every source that needs it is
//! simply not registered and does not appear in /v4/info.sourceManagers. The node
//! works fully without it — local and HTTP are unaffected. That is the whole point
//! of the arrangement: these sites break often, and when they do the failure should
//! be confined to them rather than deciding whether the server starts.
//!
//! Expiring URLs
//!
//! A resolved media URL is valid for hours at best, so it is deliberately not
//! stored in the encoded track. What is stored is whatever
//! SourceKind::playback_url can turn back into a page URL; the direct stream is
//! resolved again at playback time by YtDlp::resolve_stream_url. A track queued
//! in the morning therefore still plays in the evening, which storing the URL would
//! not give.
//!
//! No circumvention
//!
//! Rate limits, bot checks, age gates and region blocks are reported as they come
//! back — TrackException with the message yt-dlp gave. Nothing here works around
//! them.

use std::io::Read as _;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use lavalink_protocol::encoded_track::SourceTail;
use lavalink_protocol::player::TrackInfo;
use serde::Deserialize;

use super::{SourceError, SourceLoad, SourcePlaylist, SourceTrack};

/// How long any one yt-dlp invocation may take before it is killed.
const PROCESS_TIMEOUT: Duration = Duration::from_secs(30);

/// The User-Agent a resolved stream URL is negotiated under.
///
/// googlevideo.com validates the fetching client against the one that obtained the
/// URL; a stream URL resolved under one UA and then fetched with another (e.g.
/// reqwest's default lavalink-rs/x.y.z) comes back 403 Forbidden. This is passed
/// both to yt-dlp via --user-agent and to the HTTP client that reads the bytes, so
/// the two always agree.
pub const STREAM_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

/// Results returned for a bare search query, matching what clients expect from the
/// original's search behavior.
pub(super) const SEARCH_RESULTS: usize = 10;

/// The default cap on how many entries a playlist load will take, when nothing in
/// application.yml overrides it via youtubePlaylistLoadLimit.
///
/// The original bounds this too (pages of 100; youtubePlaylistLoadLimit: 6 is
/// its default, so 600 tracks). A cap is not optional: a several-thousand-entry
/// playlist would otherwise hold a loader permit for minutes and return a
/// response no client wants. Ours is a flat track count rather than a page
/// count, so main.rs multiplies the config value by 100 before it reaches
/// YtDlp::detect. Only YtDlp::stub (test-only) reads this directly —
/// detect always takes the caller's value.
#[cfg(test)]
const DEFAULT_PLAYLIST_TRACK_LIMIT: usize = 600;

/// AAC first: symphonia (used to decode every source before it is mixed and
/// re-encoded for Discord) has no Opus decoder, so an opus-only stream cannot be
/// played at all despite being what Discord ultimately wants. acodec!=opus on the
/// fallback keeps a plain "bestaudio" from re-selecting an opus stream when no m4a
/// track exists.
const FORMAT: &str = "bestaudio[acodec=aac]/bestaudio[ext=m4a]/bestaudio[acodec!=opus]/bestaudio/best";

/// Which site a track came from.
///
/// Carries the two things that differ per site once yt-dlp has done the extraction:
/// the sourceName clients branch on, and how to get back from a stored identifier
/// to a page URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    YouTube,
    SoundCloud,
    Bandcamp,
}

impl SourceKind {
    pub fn name(self) -> &'static str {
        match self {
            SourceKind::YouTube => "youtube",
            SourceKind::SoundCloud => "soundcloud",
            SourceKind::Bandcamp => "bandcamp",
        }
    }

    /// The page URL to re-resolve a stored identifier from.
    ///
    /// YouTube stores the 11-character video id, which is stable and shorter than
    /// the URL. SoundCloud and Bandcamp have no equivalent — neither id can be
    /// turned back into a page URL without an API call — so the permalink is what
    /// gets stored, and this is the identity function there.
    pub fn playback_url(self, identifier: &str) -> String {
        match self {
            SourceKind::YouTube => {
                format!("https://www.youtube.com/watch?v={identifier}")
            }
            SourceKind::SoundCloud | SourceKind::Bandcamp => identifier.to_owned(),
        }
    }

    /// What to store as the track's identifier, given yt-dlp's id and page URL.
    fn identifier(self, id: String, webpage_url: Option<&str>) -> String {
        match self {
            SourceKind::YouTube => id,
            SourceKind::SoundCloud | SourceKind::Bandcamp => {
                webpage_url.map(str::to_owned).unwrap_or(id)
            }
        }
    }
}

/// A detected yt-dlp binary.
pub struct YtDlp {
    program: String,
    pub version: String,
    /// httpConfig's proxy, in the [user:password@]host:port form yt-dlp's own
    /// --proxy flag takes. Not used by detect itself — a version check makes
    /// no network request, so there is nothing there for a proxy to route.
    proxy_arg: Option<String>,
    /// youtubePlaylistLoadLimit, already converted from the original's "pages
    /// of 100" into a flat track count.
    playlist_track_limit: usize,
}

impl YtDlp {
    /// Looks for a usable yt-dlp. None means every source needing it stays
    /// disabled.
    pub fn detect(program: &str, proxy_arg: Option<String>, playlist_track_limit: usize) -> Option<Self> {
        let output = run(program, &["--version"], PROCESS_TIMEOUT).ok()?;
        let version = output.trim().to_owned();
        if version.is_empty() {
            return None;
        }

        // yt-dlp versions are dates: 2024.08.06. Anything older than this is likely
        // to fail against current sites, and saying so at startup beats a confusing
        // extraction error later.
        if version.as_str() < "2024.01.01" {
            tracing::warn!(
                version = %version,
                "this yt-dlp is old and may fail to extract; consider updating"
            );
        }

        Some(Self {
            program: program.to_owned(),
            version,
            proxy_arg,
            playlist_track_limit,
        })
    }

    /// A backend naming a program without probing for it.
    ///
    /// The site modules' tests are about URL shapes — matches never touches the
    /// binary — so they need a backend to hang a source off, not a working one.
    #[cfg(test)]
    pub(super) fn stub() -> Self {
        Self {
            program: "yt-dlp".to_owned(),
            version: "0000.00.00".to_owned(),
            proxy_arg: None,
            playlist_track_limit: DEFAULT_PLAYLIST_TRACK_LIMIT,
        }
    }

    /// ["--proxy", "<url>"] when a proxy is configured, else empty — spread
    /// into every argument list below with args.extend(self.proxy_args()).
    fn proxy_args(&self) -> Vec<&str> {
        match &self.proxy_arg {
            Some(proxy) => vec!["--proxy", proxy.as_str()],
            None => Vec::new(),
        }
    }

    /// Re-resolves a track's direct media URL. Called at playback time, not load
    /// time, because the URL expires.
    pub fn resolve_stream_url(&self, page_url: &str) -> Result<String, SourceError> {
        let mut args = vec![
            "--no-playlist",
            "-f",
            FORMAT,
            "--user-agent",
            STREAM_USER_AGENT,
        ];
        args.extend(self.proxy_args());
        args.push("--get-url");
        args.push(page_url);

        let output = run(&self.program, &args, PROCESS_TIMEOUT)?;

        output
            .lines()
            .map(str::trim)
            .find(|line| line.starts_with("http"))
            .map(str::to_owned)
            .ok_or_else(|| SourceError::Unplayable {
                reason: "yt-dlp returned no stream url".to_owned(),
            })
    }

    pub(super) fn load_single(
        &self,
        url: &str,
        kind: SourceKind,
    ) -> Result<SourceLoad, SourceError> {
        let mut args = vec!["--no-playlist", "-J", "-f", FORMAT];
        args.extend(self.proxy_args());
        args.push(url);

        let output = run(&self.program, &args, PROCESS_TIMEOUT)?;

        let video: Video = parse(&output)?;
        Ok(SourceLoad::Track(video.into_track(kind)))
    }

    /// Loads a playlist, flat.
    ///
    /// --yes-playlist is required because a URL naming both a track and a playlist
    /// makes yt-dlp default to the single track; lavaplayer treats the same URL as a
    /// playlist with that track selected, and what a client observes follows the
    /// original.
    ///
    /// --flat-playlist keeps this to one extraction rather than one per entry. The
    /// cost is that entries carry no is_live flag, so a live entry is only
    /// discovered when it is played — the same trade the search path makes.
    pub(super) fn load_playlist(
        &self,
        url: &str,
        kind: SourceKind,
        selected: Option<&str>,
    ) -> Result<SourceLoad, SourceError> {
        let end = self.playlist_track_limit.to_string();
        let mut args = vec!["-J", "--flat-playlist", "--yes-playlist", "--playlist-end", &end];
        args.extend(self.proxy_args());
        args.push(url);

        let output = run(&self.program, &args, PROCESS_TIMEOUT)?;

        let results: PlaylistResults = parse(&output)?;
        let tracks = into_tracks(results.entries, kind)?;

        // The track a "…&list=…" URL names is the entry point. When it is not in the
        // playlist — a private entry, or past the limit above — -1 says so rather
        // than silently pointing at the wrong track.
        let selected_track = selected
            .and_then(|id| tracks.iter().position(|track| track.info.identifier == id))
            .and_then(|index| i32::try_from(index).ok())
            .unwrap_or(-1);

        Ok(SourceLoad::Playlist(SourcePlaylist {
            name: results.title.unwrap_or_else(|| "Unknown playlist".to_owned()),
            selected_track,
            tracks,
        }))
    }

    /// Finds a YouTube video id to substitute for a track whose own site has no
    /// full-length stream — Deezer's public API only ever hands back a 30-second
    /// preview clip. query is free text, ordinarily "title author".
    ///
    /// Public rather than pub(super): crate::audio::stream::StreamOpener
    /// calls this at playback time, once per track, so the substitute is found
    /// fresh rather than stored — a stored video id would go stale exactly like a
    /// resolved stream URL would.
    pub fn find_youtube_match(&self, query: &str) -> Result<String, SourceError> {
        match self.search(&format!("ytsearch1:{query}"), SourceKind::YouTube)? {
            SourceLoad::Search(tracks) => tracks
                .into_iter()
                .next()
                .map(|track| track.info.identifier)
                .ok_or(SourceError::NotFound),
            _ => Err(SourceError::NotFound),
        }
    }

    /// Runs a search. target is a full yt-dlp search spec, e.g. ytsearch10:query.
    pub(super) fn search(&self, target: &str, kind: SourceKind) -> Result<SourceLoad, SourceError> {
        // --flat-playlist skips a full extraction per result. A search that
        // resolved every hit would take tens of seconds.
        let mut args = vec!["-J", "--flat-playlist"];
        args.extend(self.proxy_args());
        args.push(target);

        let output = run(&self.program, &args, PROCESS_TIMEOUT)?;

        let results: SearchResults = parse(&output)?;
        Ok(SourceLoad::Search(into_tracks(results.entries, kind)?))
    }
}

fn parse<T: serde::de::DeserializeOwned>(output: &str) -> Result<T, SourceError> {
    serde_json::from_str(output).map_err(|error| SourceError::Unplayable {
        reason: format!("could not read yt-dlp output: {error}"),
    })
}

/// Drops entries yt-dlp could not identify, and reports "nothing here" rather than
/// an empty list — NotFound becomes loadType: "empty", which is what a client
/// expects from a search that matched nothing.
fn into_tracks(entries: Vec<Video>, kind: SourceKind) -> Result<Vec<SourceTrack>, SourceError> {
    let tracks: Vec<SourceTrack> = entries
        .into_iter()
        .filter(|video| !video.id.is_empty())
        .map(|video| video.into_track(kind))
        .collect();

    if tracks.is_empty() {
        return Err(SourceError::NotFound);
    }
    Ok(tracks)
}

/// yt-dlp's -J output, only the fields we use.
#[derive(Debug, Deserialize)]
pub(super) struct Video {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    uploader: Option<String>,
    #[serde(default)]
    channel: Option<String>,
    /// Seconds, absent for live streams.
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    is_live: Option<bool>,
    #[serde(default)]
    thumbnail: Option<String>,
    #[serde(default)]
    webpage_url: Option<String>,
    /// SoundCloud reports one; YouTube does not.
    #[serde(default)]
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SearchResults {
    #[serde(default)]
    entries: Vec<Video>,
}

/// Same shape as SearchResults plus the playlist's own title, which a search
/// result does not have and a loadType: "playlist" response requires.
#[derive(Debug, Deserialize)]
struct PlaylistResults {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    entries: Vec<Video>,
}

impl Video {
    pub(super) fn into_track(self, kind: SourceKind) -> SourceTrack {
        let is_stream = self.is_live.unwrap_or(false) || self.duration.is_none();
        let length = self
            .duration
            .map(|seconds| (seconds * 1000.0) as i64)
            // A live stream has no end. The original reports the same sentinel, and
            // clients render it as "live" rather than as a duration.
            .filter(|_| !is_stream)
            .unwrap_or(i64::MAX);

        // --flat-playlist entries carry url but not always webpage_url, so both
        // are consulted before falling back to a URL built from the id.
        let page_url = self
            .webpage_url
            .or(self.url)
            .unwrap_or_else(|| kind.playback_url(&self.id));

        SourceTrack {
            info: TrackInfo {
                identifier: kind.identifier(self.id, Some(&page_url)),
                // A live stream is not seekable, and the original refuses too.
                is_seekable: !is_stream,
                author: self
                    .uploader
                    .or(self.channel)
                    .unwrap_or_else(|| "Unknown artist".to_owned()),
                length,
                is_stream,
                position: 0,
                title: self.title.unwrap_or_else(|| "Unknown title".to_owned()),
                uri: Some(page_url),
                source_name: kind.name().to_owned(),
                artwork_url: self.thumbnail,
                // yt-dlp does not report an ISRC for either site.
                isrc: None,
            },
            // The original's managers for these sites write nothing after the source
            // name.
            tail: SourceTail::Empty,
        }
    }
}

/// Runs the program and captures stdout, killing it if it overruns.
///
/// KillOnDrop is what makes the timeout real: without it a killed child that is
/// never reaped stays around as a zombie, and an early return would leak the process
/// entirely.
fn run(program: &str, args: &[&str], timeout: Duration) -> Result<String, SourceError> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => {
                SourceError::Internal(format!("{program} is not installed"))
            }
            _ => SourceError::Internal(format!("could not start {program}: {error}")),
        })?;

    // -J output regularly exceeds the OS pipe buffer (64KiB on Linux). Draining
    // both pipes on their own threads, concurrently with the wait loop below, is
    // what keeps a large stdout from deadlocking against a full pipe: without this,
    // yt-dlp blocks on write() while we are only polling try_wait, and the process
    // just sits there until it is killed at the deadline.
    let mut stdout_pipe = child.stdout.take().expect("stdout is piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr is piped");
    let stdout_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stdout_pipe.read_to_string(&mut buf);
        buf
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stderr_pipe.read_to_string(&mut buf);
        buf
    });

    let mut child = KillOnDrop(Some(child));
    let deadline = Instant::now() + timeout;

    loop {
        match child.0.as_mut().expect("child is present").try_wait() {
            Ok(Some(status)) => {
                let stdout = stdout_thread.join().unwrap_or_default();
                let stderr = stderr_thread.join().unwrap_or_default();

                if !status.success() {
                    return Err(classify(&stderr));
                }
                return Ok(stdout);
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    return Err(SourceError::Io(format!(
                        "{program} timed out after {}s",
                        timeout.as_secs()
                    )));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(SourceError::Internal(error.to_string())),
        }
    }
}

/// Turns yt-dlp's stderr into the right kind of failure.
///
/// The severity matters: a private track is the user's problem (common), while a
/// broken extractor is not something the client can act on. Everything here is
/// caused by the site or the request, so none of it is fault.
fn classify(stderr: &str) -> SourceError {
    let lower = stderr.to_ascii_lowercase();

    if lower.contains("video unavailable")
        || lower.contains("private video")
        || lower.contains("removed by the uploader")
        || lower.contains("does not exist")
        // SoundCloud's wording for the same situations.
        || lower.contains("not found")
        || lower.contains("this track is not available")
    {
        return SourceError::NotFound;
    }

    let message = stderr
        .lines()
        .find(|line| line.starts_with("ERROR:"))
        .unwrap_or_else(|| stderr.lines().next().unwrap_or("yt-dlp failed"))
        .trim()
        .to_owned();

    SourceError::Unplayable { reason: message }
}

/// Ensures a child process cannot outlive our interest in it.
struct KillOnDrop(Option<Child>);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_proxy_configured_means_no_proxy_args() {
        let backend = YtDlp::stub();
        assert!(backend.proxy_args().is_empty());
    }

    #[test]
    fn a_configured_proxy_becomes_a_flag_pair() {
        let backend = YtDlp {
            proxy_arg: Some("http://localhost:3128".to_owned()),
            ..YtDlp::stub()
        };
        assert_eq!(backend.proxy_args(), vec!["--proxy", "http://localhost:3128"]);
    }

    #[test]
    fn an_unavailable_track_is_reported_as_missing_not_broken() {
        assert!(matches!(
            classify("ERROR: [youtube] xyz: Video unavailable"),
            SourceError::NotFound
        ));
        assert!(matches!(
            classify("ERROR: [youtube] xyz: Private video. Sign in if you've been granted access"),
            SourceError::NotFound
        ));
        assert!(matches!(
            classify("ERROR: [soundcloud] 404 Not Found"),
            SourceError::NotFound
        ));
    }

    #[test]
    fn other_failures_keep_yt_dlps_own_message() {
        let error = classify("WARNING: something\nERROR: Sign in to confirm you're not a bot\n");
        match error {
            SourceError::Unplayable { reason } => {
                assert!(reason.contains("not a bot"), "got {reason}");
            }
            other => panic!("expected Unplayable, got {other:?}"),
        }
        // Nothing yt-dlp reports is our fault, so nothing here is fault.
        assert_ne!(
            classify("ERROR: whatever").to_exception().severity,
            lavalink_protocol::Severity::Fault
        );
    }

    #[test]
    fn a_live_video_is_a_stream_and_not_seekable() {
        let video: Video = serde_json::from_str(
            r#"{"id":"dQw4w9WgXcQ","title":"Live","is_live":true,"duration":null}"#,
        )
        .unwrap();
        let track = video.into_track(SourceKind::YouTube);

        assert!(track.info.is_stream);
        assert!(!track.info.is_seekable);
        assert_eq!(track.info.length, i64::MAX);
    }

    #[test]
    fn a_normal_video_carries_its_duration_in_milliseconds() {
        let video: Video = serde_json::from_str(
            r#"{"id":"dQw4w9WgXcQ","title":"Never Gonna Give You Up",
                "uploader":"RickAstleyVEVO","duration":212.0,
                "thumbnail":"https://i.ytimg.com/vi/dQw4w9WgXcQ/maxresdefault.jpg"}"#,
        )
        .unwrap();
        let track = video.into_track(SourceKind::YouTube);

        assert_eq!(track.info.length, 212_000);
        assert_eq!(track.info.title, "Never Gonna Give You Up");
        assert_eq!(track.info.author, "RickAstleyVEVO");
        assert!(track.info.is_seekable);
        assert!(!track.info.is_stream);
        assert_eq!(track.info.source_name, "youtube");
        assert!(track.info.artwork_url.is_some());
    }

    /// The id is stored, not the expiring media URL, so a queued track still plays
    /// hours later.
    #[test]
    fn youtube_stores_the_video_id_as_its_identifier() {
        let video: Video =
            serde_json::from_str(r#"{"id":"dQw4w9WgXcQ","title":"t","duration":1.0}"#).unwrap();
        let track = video.into_track(SourceKind::YouTube);

        assert_eq!(track.info.identifier, "dQw4w9WgXcQ");
        assert_eq!(
            track.info.uri.as_deref(),
            Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ")
        );
        assert_eq!(track.tail, SourceTail::Empty);
        assert_eq!(
            SourceKind::YouTube.playback_url(&track.info.identifier),
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
        );
    }

    /// SoundCloud's numeric id cannot be turned back into a page URL, so the
    /// permalink is what survives into the encoded track.
    #[test]
    fn soundcloud_stores_the_permalink_as_its_identifier() {
        let video: Video = serde_json::from_str(
            r#"{"id":"123456789","title":"A Track","uploader":"An Artist","duration":180.0,
                "webpage_url":"https://soundcloud.com/an-artist/a-track"}"#,
        )
        .unwrap();
        let track = video.into_track(SourceKind::SoundCloud);

        assert_eq!(
            track.info.identifier,
            "https://soundcloud.com/an-artist/a-track"
        );
        assert_eq!(track.info.source_name, "soundcloud");
        assert_eq!(
            SourceKind::SoundCloud.playback_url(&track.info.identifier),
            "https://soundcloud.com/an-artist/a-track"
        );
    }

    /// --flat-playlist entries carry url rather than webpage_url.
    #[test]
    fn a_flat_entry_falls_back_to_its_url_field() {
        let video: Video = serde_json::from_str(
            r#"{"id":"1","title":"t","duration":1.0,
                "url":"https://soundcloud.com/artist/track"}"#,
        )
        .unwrap();
        let track = video.into_track(SourceKind::SoundCloud);
        assert_eq!(track.info.identifier, "https://soundcloud.com/artist/track");
    }

    #[test]
    fn an_empty_result_set_is_not_found_rather_than_an_empty_list() {
        assert!(matches!(
            into_tracks(Vec::new(), SourceKind::YouTube),
            Err(SourceError::NotFound)
        ));
    }

    /// The encoded form must survive our own codec, since clients hand it back.
    #[test]
    fn a_track_round_trips_through_the_codec() {
        let video: Video =
            serde_json::from_str(r#"{"id":"dQw4w9WgXcQ","title":"한국어 🎵","duration":212.0}"#)
                .unwrap();
        let track = video.into_track(SourceKind::YouTube);

        let encoded = lavalink_protocol::encoded_track::encode(&track.info, &track.tail).unwrap();
        let decoded = lavalink_protocol::encoded_track::decode(&encoded).unwrap();

        assert_eq!(decoded.info, track.info);
        assert_eq!(decoded.tail, track.tail);
    }

    #[test]
    fn a_missing_program_is_detected_rather_than_panicking() {
        assert!(YtDlp::detect("definitely-not-a-program-8f3a", None, 600).is_none());
    }

    #[test]
    fn running_a_missing_program_is_an_internal_error() {
        let error =
            run("definitely-not-a-program-8f3a", &["--version"], PROCESS_TIMEOUT).unwrap_err();
        assert!(matches!(error, SourceError::Internal(_)));
    }
}
