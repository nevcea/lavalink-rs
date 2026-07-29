//! YouTube.
//!
//! Everything site-specific lives here — which URLs are ours, and how to turn one
//! into something yt-dlp can extract. The extraction itself belongs to
//! [`YtDlp`](super::ytdlp::YtDlp).

use std::sync::Arc;

use super::ytdlp::{SourceKind, YtDlp, SEARCH_RESULTS};
use super::{SourceError, SourceLoad, SourceManager};

pub struct YouTubeSource {
    backend: Arc<YtDlp>,
    /// `youtubeSearchEnabled`. `false` leaves direct URLs and playlists claimed —
    /// only the search prefixes stop being claimed, the same distinction the
    /// original's key draws.
    search_enabled: bool,
}

impl YouTubeSource {
    pub fn new(backend: Arc<YtDlp>, search_enabled: bool) -> Self {
        Self {
            backend,
            search_enabled,
        }
    }
}

impl SourceManager for YouTubeSource {
    fn name(&self) -> &'static str {
        SourceKind::YouTube.name()
    }

    fn matches(&self, identifier: &str) -> bool {
        (self.search_enabled
            && (identifier.starts_with("ytsearch:") || identifier.starts_with("ytmsearch:")))
            || video_id_of(identifier).is_some()
            || playlist_id_of(identifier).is_some()
    }

    fn load(&self, identifier: &str) -> Result<SourceLoad, SourceError> {
        // Claimed and refused rather than left unclaimed: an unclaimed prefix comes
        // back `loadType: "empty"`, indistinguishable from "YouTube Music had no
        // results". yt-dlp (checked against 2026.07) has no way to answer this
        // query: `music.youtube.com/search` returns `MPREb_*` browse/album ids with
        // `title: null`, and the `#songs`-scoped variant returns no entries at all —
        // neither is a playable song track. Mapping this to `ytsearch:` would return
        // ordinary YouTube search results under the `ytmsearch:` name: different
        // ranking, different titles, fan uploads — silently wrong in exactly the way
        // `MAINTENANCE.md` refuses `timescale` for.
        if identifier.starts_with("ytmsearch:") {
            return Err(SourceError::Unplayable {
                reason: "ytmsearch: (YouTube Music search) is not supported by this node; \
                         see MAINTENANCE.md"
                    .to_owned(),
            });
        }

        if let Some(query) = identifier.strip_prefix("ytsearch:") {
            // `matches` already gates this on `search_enabled` in the ordinary
            // path; checked again here so a direct `load` call (as in a test, or
            // a future caller) cannot shell out to yt-dlp for a prefix this node
            // was configured not to serve.
            if !self.search_enabled || query.trim().is_empty() {
                return Err(SourceError::NotFound);
            }
            return self
                .backend
                .search(&format!("ytsearch{SEARCH_RESULTS}:{query}"), SourceKind::YouTube);
        }

        let video_id = video_id_of(identifier);

        // A `list=` wins over a `v=`, which is what lavaplayer does: `watch?v=…&list=…`
        // loads the playlist with that video selected, not the video alone.
        if let Some(playlist_id) = playlist_id_of(identifier) {
            let url = playlist_url(&playlist_id, video_id.as_deref());
            return self
                .backend
                .load_playlist(&url, SourceKind::YouTube, video_id.as_deref());
        }

        let video_id = video_id.ok_or(SourceError::NotFound)?;
        self.backend
            .load_single(&SourceKind::YouTube.playback_url(&video_id), SourceKind::YouTube)
    }
}

/// The URL to hand yt-dlp for a playlist.
///
/// A mix (`list=RD…`) is generated *from* a video and `/playlist?list=RD…` does not
/// resolve, so when a video id is known the watch form is used — it works for both
/// kinds, while the bare form only works for real playlists.
fn playlist_url(playlist_id: &str, video_id: Option<&str>) -> String {
    match video_id {
        Some(video_id) => format!("https://www.youtube.com/watch?v={video_id}&list={playlist_id}"),
        None => format!("https://www.youtube.com/playlist?list={playlist_id}"),
    }
}

/// Strips the scheme and the host prefixes YouTube answers on.
///
/// `None` means the URL is not a YouTube one at all, which makes the identifier
/// `empty` rather than an error — so a Vimeo link does not become a YouTube failure.
fn youtube_host(identifier: &str) -> Option<&str> {
    let rest = identifier
        .strip_prefix("https://")
        .or_else(|| identifier.strip_prefix("http://"))?;
    let rest = rest.strip_prefix("www.").unwrap_or(rest);
    Some(rest.strip_prefix("m.").unwrap_or(rest))
}

/// Extracts a video id from the URL forms YouTube uses.
fn video_id_of(identifier: &str) -> Option<String> {
    let rest = youtube_host(identifier)?;

    /// The id is whatever precedes the first query, fragment or path separator.
    fn first_segment(rest: &str) -> Option<&str> {
        rest.split(['?', '&', '#', '/']).next()
    }

    let path_forms = ["youtu.be/", "youtube.com/shorts/", "youtube.com/embed/"];
    let id = if let Some(rest) = rest.strip_prefix("youtube.com/watch") {
        rest.strip_prefix('?')?
            .split('&')
            .find_map(|pair| pair.strip_prefix("v="))?
            .split('#')
            .next()?
    } else {
        let stripped = path_forms
            .iter()
            .find_map(|prefix| rest.strip_prefix(prefix))?;
        first_segment(stripped)?
    };

    // YouTube ids are 11 characters of a URL-safe alphabet. Checking guards against
    // treating a malformed URL as a real id and shelling out for nothing.
    if id.len() == 11 && id.chars().all(is_url_safe) {
        Some(id.to_owned())
    } else {
        None
    }
}

/// Extracts a `list=` id from a YouTube URL.
///
/// Held to the same standard as [`video_id_of`]: an id that does not look like one
/// is `None`, so a malformed URL becomes `empty` rather than a subprocess launch.
fn playlist_id_of(identifier: &str) -> Option<String> {
    let rest = youtube_host(identifier)?;
    if !rest.starts_with("youtube.com/") && !rest.starts_with("youtu.be/") {
        return None;
    }

    let query = rest.split_once('?')?.1;
    let id = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("list="))?
        .split('#')
        .next()?;

    // Playlist ids are URL-safe and at least a couple of characters; the upper bound
    // is generous because mix and radio ids are longer than album ones.
    if (2..=64).contains(&id.len()) && id.chars().all(is_url_safe) {
        Some(id.to_owned())
    } else {
        None
    }
}

fn is_url_safe(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> YouTubeSource {
        YouTubeSource::new(Arc::new(YtDlp::stub()), true)
    }

    #[test]
    fn recognises_the_url_forms_youtube_uses() {
        for url in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "http://youtube.com/watch?v=dQw4w9WgXcQ",
            "https://m.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://www.youtube.com/watch?feature=share&v=dQw4w9WgXcQ",
            "https://youtu.be/dQw4w9WgXcQ",
            "https://youtu.be/dQw4w9WgXcQ?t=43",
            "https://www.youtube.com/shorts/dQw4w9WgXcQ",
            "https://www.youtube.com/embed/dQw4w9WgXcQ",
        ] {
            assert_eq!(
                video_id_of(url).as_deref(),
                Some("dQw4w9WgXcQ"),
                "failed on {url}"
            );
        }
    }

    #[test]
    fn other_sites_are_not_ours() {
        for url in [
            "https://soundcloud.com/artist/track",
            "https://example.invalid/a.mp3",
            "https://vimeo.com/123456",
            "/tmp/a.mp3",
        ] {
            assert_eq!(video_id_of(url), None, "wrongly claimed {url}");
            assert!(!source().matches(url), "manager wrongly claimed {url}");
        }
    }

    #[test]
    fn a_malformed_id_is_rejected_rather_than_shelled_out_for() {
        assert_eq!(video_id_of("https://youtu.be/short"), None);
        assert_eq!(video_id_of("https://youtu.be/waytoolongforanid"), None);
        assert_eq!(video_id_of("https://www.youtube.com/watch?v="), None);
        assert_eq!(video_id_of("https://www.youtube.com/watch"), None);
    }

    #[test]
    fn recognises_the_playlist_url_forms() {
        for (url, expected) in [
            (
                "https://www.youtube.com/playlist?list=PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI",
                "PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI",
            ),
            (
                "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PL1234567890",
                "PL1234567890",
            ),
            // A mix, which is generated from a video rather than curated.
            (
                "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=RDdQw4w9WgXcQ",
                "RDdQw4w9WgXcQ",
            ),
            ("http://m.youtube.com/playlist?list=PLabc", "PLabc"),
        ] {
            assert_eq!(
                playlist_id_of(url).as_deref(),
                Some(expected),
                "failed on {url}"
            );
        }
    }

    #[test]
    fn a_url_without_a_list_is_not_a_playlist() {
        for url in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://youtu.be/dQw4w9WgXcQ",
            "https://soundcloud.com/artist/sets/x?list=PLabc",
            // Present but implausible, so not worth a subprocess.
            "https://www.youtube.com/playlist?list=",
            "https://www.youtube.com/playlist?list=has spaces",
        ] {
            assert_eq!(playlist_id_of(url), None, "wrongly claimed {url}");
        }
    }

    /// A mix does not resolve through `/playlist?list=`, so a known video keeps the
    /// watch form.
    #[test]
    fn a_playlist_with_a_known_video_uses_the_watch_form() {
        assert_eq!(
            playlist_url("RDdQw4w9WgXcQ", Some("dQw4w9WgXcQ")),
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=RDdQw4w9WgXcQ"
        );
        assert_eq!(
            playlist_url("PLabc", None),
            "https://www.youtube.com/playlist?list=PLabc"
        );
    }

    /// `matches` has to claim playlists, or they never reach `load` at all and the
    /// node answers `empty` for a perfectly good URL.
    #[test]
    fn playlists_and_searches_are_claimed() {
        let source = source();
        assert!(source.matches("https://www.youtube.com/playlist?list=PLabc"));
        assert!(source.matches("https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PLabc"));
        assert!(source.matches("ytsearch:never gonna give you up"));
        assert!(!source.matches("scsearch:never gonna give you up"));
    }

    #[test]
    fn an_empty_search_query_is_not_found() {
        assert!(matches!(
            source().load("ytsearch:   "),
            Err(SourceError::NotFound)
        ));
    }

    /// `youtubeSearchEnabled: false` stops search prefixes from being claimed, but
    /// direct URLs and playlists are unaffected.
    #[test]
    fn disabling_search_leaves_direct_urls_claimed() {
        let source = YouTubeSource::new(Arc::new(YtDlp::stub()), false);

        assert!(!source.matches("ytsearch:never gonna give you up"));
        assert!(!source.matches("ytmsearch:never gonna give you up"));
        assert!(source.matches("https://www.youtube.com/watch?v=dQw4w9WgXcQ"));
        assert!(source.matches("https://www.youtube.com/playlist?list=PLabc"));

        assert!(matches!(
            source.load("ytsearch:never gonna give you up"),
            Err(SourceError::NotFound)
        ));
    }

    /// `ytmsearch:` must be claimed (so it reports `loadType: "error"`, not
    /// `"empty"`) but refused rather than silently degraded to a plain YouTube
    /// search.
    #[test]
    fn ytmsearch_is_claimed_and_refused_rather_than_downgraded() {
        let source = source();
        assert!(source.matches("ytmsearch:never gonna give you up"));
        assert!(matches!(
            source.load("ytmsearch:never gonna give you up"),
            Err(SourceError::Unplayable { .. })
        ));
    }
}
