//! Bandcamp.
//!
//! Bandcamp streams full tracks with no subscription gate, so — unlike Deezer —
//! there is a real audio stream to hand back, and this follows the same shape as
//! [`youtube`](super::youtube) and [`soundcloud`](super::soundcloud): the URL shapes
//! are ours, extraction belongs to the shared [`ytdlp`](super::ytdlp) backend.
//!
//! Only artist subdomains (`artist.bandcamp.com`) are recognised, matching what the
//! original's Bandcamp source claims — a label's custom domain proxying Bandcamp is
//! not detectable from the URL alone.

use std::sync::Arc;

use super::ytdlp::{SourceKind, YtDlp};
use super::{strip_scheme, SourceError, SourceLoad, SourceManager};

pub struct BandcampSource {
    backend: Arc<YtDlp>,
}

impl BandcampSource {
    pub fn new(backend: Arc<YtDlp>) -> Self {
        Self { backend }
    }
}

impl SourceManager for BandcampSource {
    fn name(&self) -> &'static str {
        SourceKind::Bandcamp.name()
    }

    fn matches(&self, identifier: &str) -> bool {
        is_bandcamp_url(identifier)
    }

    fn load(&self, identifier: &str) -> Result<SourceLoad, SourceError> {
        if !is_bandcamp_url(identifier) {
            return Err(SourceError::NotFound);
        }

        if is_album_url(identifier) {
            return self
                .backend
                .load_playlist(identifier, SourceKind::Bandcamp, None);
        }

        self.backend.load_single(identifier, SourceKind::Bandcamp)
    }
}

/// Whether this is an artist-subdomain Bandcamp page naming a track or an album.
///
/// The apex domain (`bandcamp.com` itself, `www.bandcamp.com`) names no track, so it
/// is deliberately left unclaimed — claiming it would turn the front page or the
/// discovery feed into a failed load rather than `empty`.
fn is_bandcamp_url(identifier: &str) -> bool {
    let Some(rest) = strip_scheme(identifier) else {
        return false;
    };

    let Some((host, path)) = rest.split_once('/') else {
        return false;
    };

    if host.is_empty() || host == "bandcamp.com" || host == "www.bandcamp.com" {
        return false;
    }
    if !host.ends_with(".bandcamp.com") {
        return false;
    }

    let path = path.split(['?', '#']).next().unwrap_or(path);
    for prefix in ["track/", "album/"] {
        if let Some(rest) = path.strip_prefix(prefix) {
            if !rest.trim().is_empty() {
                return true;
            }
        }
    }
    false
}

/// Bandcamp spells a playlist `/album/`.
fn is_album_url(identifier: &str) -> bool {
    identifier
        .split_once("bandcamp.com/")
        .is_some_and(|(_, rest)| rest.split(['?', '#']).next().unwrap_or(rest).starts_with("album/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> BandcampSource {
        BandcampSource::new(Arc::new(YtDlp::stub()))
    }

    #[test]
    fn claims_artist_subdomain_tracks_and_albums() {
        let source = source();
        for identifier in [
            "https://anartist.bandcamp.com/track/a-track",
            "http://anartist.bandcamp.com/album/an-album",
            "https://anartist.bandcamp.com/track/a-track?from=search",
        ] {
            assert!(source.matches(identifier), "failed to claim {identifier}");
        }
    }

    #[test]
    fn other_sites_and_the_apex_domain_are_not_ours() {
        let source = source();
        for identifier in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://soundcloud.com/an-artist/a-track",
            "https://bandcamp.com/",
            "https://bandcamp.com/discover",
            "https://www.bandcamp.com/",
            // No path at all.
            "https://anartist.bandcamp.com",
            // A lookalike host must not be claimed.
            "https://notbandcamp.com/track/a-track",
            "/tmp/a.mp3",
        ] {
            assert!(!source.matches(identifier), "wrongly claimed {identifier}");
        }
    }

    #[test]
    fn an_album_is_a_playlist_and_a_track_is_not() {
        assert!(is_album_url("https://anartist.bandcamp.com/album/an-album"));
        assert!(!is_album_url("https://anartist.bandcamp.com/track/a-track"));
    }

    /// `load` is only ever called after `matches`, but a mismatch must not become a
    /// subprocess launch against an arbitrary string.
    #[test]
    fn an_unclaimed_identifier_is_not_found_rather_than_extracted() {
        assert!(matches!(
            source().load("https://example.invalid/a.mp3"),
            Err(SourceError::NotFound)
        ));
    }
}
