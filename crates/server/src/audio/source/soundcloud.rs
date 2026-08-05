//! SoundCloud.
//!
//! The original resolves SoundCloud through lavaplayer's own API client, which needs
//! a client id scraped from the site and re-scraped whenever it rotates. Here it goes
//! through the same yt-dlp binary [`youtube`](super::youtube) already depends on:
//! there is no second failure mode to operate, and yt-dlp tracks the site's changes
//! for us.
//!
//! What a client observes is unaffected — `sourceName` is `"soundcloud"` and
//! `scsearch:` behaves as it does on the original.

use std::sync::Arc;

use super::ytdlp::{SourceKind, YtDlp, SEARCH_RESULTS};
use super::{strip_scheme, SourceError, SourceLoad, SourceManager};

pub struct SoundCloudSource {
    backend: Arc<YtDlp>,
    /// `soundcloudSearchEnabled`. `false` leaves direct URLs (including sets)
    /// claimed — only `scsearch:` stops being claimed.
    search_enabled: bool,
}

impl SoundCloudSource {
    pub fn new(backend: Arc<YtDlp>, search_enabled: bool) -> Self {
        Self {
            backend,
            search_enabled,
        }
    }
}

impl SourceManager for SoundCloudSource {
    fn name(&self) -> &'static str {
        SourceKind::SoundCloud.name()
    }

    fn matches(&self, identifier: &str) -> bool {
        (self.search_enabled && identifier.starts_with("scsearch:")) || is_soundcloud_url(identifier)
    }

    fn load(&self, identifier: &str) -> Result<SourceLoad, SourceError> {
        if let Some(query) = identifier.strip_prefix("scsearch:") {
            // See YouTubeSource::load's equivalent comment: this must not shell
            // out even if called directly with search disabled.
            if !self.search_enabled || query.trim().is_empty() {
                return Err(SourceError::NotFound);
            }
            return self.backend.search(
                &format!("scsearch{SEARCH_RESULTS}:{query}"),
                SourceKind::SoundCloud,
            );
        }

        if !is_soundcloud_url(identifier) {
            return Err(SourceError::NotFound);
        }

        // A set is SoundCloud's playlist. Nothing names an entry point within one —
        // there is no watch?v=…&list=… equivalent — so the selection is always
        // "none", which is what -1 on the wire means.
        if is_set_url(identifier) {
            return self
                .backend
                .load_playlist(identifier, SourceKind::SoundCloud, None);
        }

        self.backend.load_single(identifier, SourceKind::SoundCloud)
    }
}

/// Whether this is a SoundCloud page URL.
///
/// `on.soundcloud.com` is the share shortener, which yt-dlp follows; it is claimed
/// here so a shared link does not come back `empty`.
fn is_soundcloud_url(identifier: &str) -> bool {
    let Some(rest) = strip_scheme(identifier) else {
        return false;
    };
    let rest = rest.strip_prefix("www.").unwrap_or(rest);
    let rest = rest.strip_prefix("m.").unwrap_or(rest);

    // A path is required: soundcloud.com alone names no track, and claiming it
    // would turn the front page into a failed load rather than empty.
    let host_matches = rest.starts_with("soundcloud.com/") || rest.starts_with("on.soundcloud.com/");
    host_matches
        && rest
            .split_once('/')
            .is_some_and(|(_, path)| !path.trim().is_empty())
}

/// SoundCloud spells a playlist `/sets/`.
fn is_set_url(identifier: &str) -> bool {
    identifier.contains("/sets/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> SoundCloudSource {
        SoundCloudSource::new(Arc::new(YtDlp::stub()), true)
    }

    #[test]
    fn claims_soundcloud_urls_and_searches() {
        let source = source();
        for identifier in [
            "https://soundcloud.com/an-artist/a-track",
            "http://www.soundcloud.com/an-artist/a-track",
            "https://m.soundcloud.com/an-artist/a-track",
            "https://on.soundcloud.com/abc123",
            "https://soundcloud.com/an-artist/sets/an-album",
            "scsearch:something",
        ] {
            assert!(source.matches(identifier), "failed to claim {identifier}");
        }
    }

    #[test]
    fn other_sites_are_not_ours() {
        let source = source();
        for identifier in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://example.invalid/a.mp3",
            "ytsearch:something",
            "/tmp/a.mp3",
            // No path: names no track, so it is empty rather than a failed load.
            "https://soundcloud.com/",
            "https://soundcloud.com",
            // A lookalike host must not be claimed.
            "https://notsoundcloud.com/artist/track",
        ] {
            assert!(!source.matches(identifier), "wrongly claimed {identifier}");
        }
    }

    #[test]
    fn a_set_is_a_playlist_and_a_track_is_not() {
        assert!(is_set_url("https://soundcloud.com/an-artist/sets/an-album"));
        assert!(!is_set_url("https://soundcloud.com/an-artist/a-track"));
    }

    #[test]
    fn an_empty_search_query_is_not_found() {
        assert!(matches!(
            source().load("scsearch:   "),
            Err(SourceError::NotFound)
        ));
    }

    /// `soundcloudSearchEnabled: false` stops `scsearch:` from being claimed, but
    /// direct URLs (including sets) are unaffected.
    #[test]
    fn disabling_search_leaves_direct_urls_claimed() {
        let source = SoundCloudSource::new(Arc::new(YtDlp::stub()), false);

        assert!(!source.matches("scsearch:something"));
        assert!(source.matches("https://soundcloud.com/an-artist/a-track"));

        assert!(matches!(
            source.load("scsearch:something"),
            Err(SourceError::NotFound)
        ));
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
