//! Source managers.
//!
//! Three of the original's nine (`twitch`, `vimeo` and `nico`) are refused — see
//! `MAINTENANCE.md` — plus `deezer`, which is not one of the original nine at
//! all: upstream ships it as a separate plugin. The trait is the seam anything
//! further would slot into. Each manager answers two questions: does this
//! identifier belong to me, and if so what track does it name.
//!
//! [`youtube`], [`soundcloud`] and [`bandcamp`] are all thin: the URL shapes are
//! theirs, and the extraction belongs to the shared [`ytdlp`] backend. [`getyarn`]
//! is thin in a different way: unlike Twitch/Vimeo/Nico, getyarn.io's pages embed
//! a direct non-HLS video URL in an Open Graph tag, so it needs neither yt-dlp
//! nor a scraping crate — just one GET and two tag reads.
//!
//! [`deezer`] is a different shape entirely: Deezer's own API hands back metadata
//! but never a full-length stream, so it resolves through Deezer's HTTP API at load
//! time and substitutes a YouTube match at playback time, via
//! [`ytdlp::YtDlp::find_youtube_match`]. Spotify and Apple Music would follow that
//! shape rather than the yt-dlp one.
//!
//! Loading is **blocking** by design. Probing a container is file and network I/O
//! plus CPU, so it runs off the async threads; the server never waits on a decoder.
//! The original does the same work on the request thread (`util/loading.kt`), which
//! is why N clients asking for one URL there means N probes each holding a thread.

pub mod bandcamp;
pub mod deezer;
pub mod getyarn;
pub mod http;
pub mod local;
pub mod probe;
pub mod soundcloud;
pub mod youtube;
pub mod ytdlp;

use lavalink_protocol::encoded_track::SourceTail;
use lavalink_protocol::player::TrackInfo;
use lavalink_protocol::{Exception, Severity};

pub use bandcamp::BandcampSource;
pub use deezer::DeezerSource;
pub use getyarn::GetyarnSource;
pub use http::HttpSource;
pub use local::LocalSource;
pub use soundcloud::SoundCloudSource;
pub use youtube::YouTubeSource;
pub use ytdlp::YtDlp;

/// A track as a source manager knows it, before it is encoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceTrack {
    pub info: TrackInfo,
    pub tail: SourceTail,
}

/// What resolving an identifier produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceLoad {
    Track(SourceTrack),
    Search(Vec<SourceTrack>),
    Playlist(SourcePlaylist),
}

/// A named group of tracks, with the entry point the identifier singled out.
///
/// `selected_track` is an index into `tracks`, or `-1` for none — the original's
/// encoding, kept rather than an `Option<usize>` because it is what goes on the wire
/// and translating twice is where an off-by-one would hide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcePlaylist {
    pub name: String,
    pub selected_track: i32,
    pub tracks: Vec<SourceTrack>,
}

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    /// The identifier looked right but there is nothing there. Becomes
    /// `loadType: "empty"`.
    #[error("nothing found for this identifier")]
    NotFound,
    /// Reached, but not playable — wrong content type, unsupported container.
    #[error("{reason}")]
    Unplayable { reason: String },
    /// The remote said no.
    #[error("{status}: {reason}")]
    Remote { status: u16, reason: String },
    /// Network or filesystem failure.
    #[error("{0}")]
    Io(String),
    /// Ours. Becomes severity `fault`.
    #[error("{0}")]
    Internal(String),
}

impl SourceError {
    /// Converts to the wire exception, choosing the severity the original would.
    ///
    /// Everything a user can cause is `common`; only our own failures are `fault`.
    /// Clients surface `fault` differently, so misclassifying here turns a bad URL
    /// into what looks like a server bug.
    pub fn to_exception(&self) -> Exception {
        let severity = match self {
            SourceError::Internal(_) => Severity::Fault,
            SourceError::Io(_) => Severity::Suspicious,
            _ => Severity::Common,
        };
        Exception::new(severity, self.to_string(), self.to_string())
    }
}

/// The last non-empty path segment of a URL or path, without any query or fragment.
///
/// Falls back to the whole input when there is no separator, so a bare file name
/// works as well as a URL.
pub fn last_path_segment(url: &str) -> &str {
    url.rsplit('/')
        .find(|segment| !segment.is_empty())
        .map(|segment| segment.split(['?', '#']).next().unwrap_or(segment))
        .unwrap_or(url)
}

/// The lowercased file extension of a URL or path, when it has a plausible one.
///
/// Only ever a hint for the container probe, so an implausible tail — nothing after
/// the dot, or something too long to be an extension — is better reported as absent
/// than passed on as a bad guess.
pub fn extension_of(url: &str) -> Option<String> {
    let (_, extension) = last_path_segment(url).rsplit_once('.')?;
    if extension.is_empty() || extension.len() > 5 {
        return None;
    }
    Some(extension.to_ascii_lowercase())
}

/// Builds the proxy every blocking HTTP client this node creates should route
/// through, from `httpConfig`'s keys — the same keys `application.yml.example`
/// warns operators to set to avoid exposing this node's IP address. Shared here
/// because [`http::HttpSource`], [`deezer::DeezerSource`] and
/// [`super::stream::HttpMediaSource`] each build their own client.
pub fn configured_proxy(
    config: &crate::config::HttpConfig,
) -> Result<Option<reqwest::Proxy>, SourceError> {
    let Some(url) = config.reqwest_proxy_url() else {
        return Ok(None);
    };
    let mut proxy =
        reqwest::Proxy::all(url).map_err(|error| SourceError::Internal(error.to_string()))?;
    if let Some((user, password)) = config.basic_auth() {
        proxy = proxy.basic_auth(user, password);
    }
    Ok(Some(proxy))
}

/// One source of tracks.
pub trait SourceManager: Send + Sync + 'static {
    /// The `sourceName` clients branch on: `"http"`, `"local"`, `"youtube"`.
    fn name(&self) -> &'static str;

    /// Whether this manager claims the identifier. Claiming means any failure is
    /// reported as an error; an identifier no manager claims is `empty`, not an
    /// error.
    fn matches(&self, identifier: &str) -> bool;

    /// Resolves the identifier. Blocking.
    fn load(&self, identifier: &str) -> Result<SourceLoad, SourceError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_errors_are_common_and_ours_are_faults() {
        assert_eq!(SourceError::NotFound.to_exception().severity, Severity::Common);
        assert_eq!(
            SourceError::Remote {
                status: 404,
                reason: "Not Found".into()
            }
            .to_exception()
            .severity,
            Severity::Common
        );
        assert_eq!(
            SourceError::Internal("bug".into()).to_exception().severity,
            Severity::Fault
        );
    }
}
