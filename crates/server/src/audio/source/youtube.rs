//! YouTube.
//!
//! Everything site-specific lives here — which URLs are ours, and how to turn one
//! into something yt-dlp can extract. The extraction itself belongs to
//! YtDlp.

use std::sync::Arc;

use super::ytdlp::{SourceKind, YtDlp, SEARCH_RESULTS};
use super::{strip_scheme, SourceError, SourceLoad, SourceManager};

pub struct YouTubeSource {
    backend: Arc<YtDlp>,
    /// youtubeSearchEnabled. false leaves direct URLs and playlists claimed —
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
            || watch_videos_ids(identifier).is_some()
    }

    fn load(&self, identifier: &str) -> Result<SourceLoad, SourceError> {
        // Claimed and refused rather than left unclaimed: an unclaimed prefix
        // comes back loadType: "empty", indistinguishable from "no results".
        // yt-dlp has no way to answer this query (checked against 2026.07):
        // music.youtube.com/search returns MPREb_* browse/album ids with
        // title: null, and the #songs-scoped variant returns no entries —
        // neither is a playable song track. Mapping to ytsearch: would return
        // ordinary YouTube results under the ytmsearch: name — silently wrong,
        // the same shape MAINTENANCE.md refuses elsewhere.
        if identifier.starts_with("ytmsearch:") {
            return Err(SourceError::Unplayable {
                reason: "ytmsearch: (YouTube Music search) is not supported by this node; \
                         see MAINTENANCE.md"
                    .to_owned(),
            });
        }

        if let Some(query) = identifier.strip_prefix("ytsearch:") {
            let query = query.trim();
            // matches already gates this on search_enabled in the ordinary
            // path; checked again here so a direct load call (as in a test, or
            // a future caller) cannot shell out to yt-dlp for a prefix this node
            // was configured not to serve.
            if !self.search_enabled || query.is_empty() {
                return Err(SourceError::NotFound);
            }
            return self
                .backend
                .search(&format!("ytsearch{SEARCH_RESULTS}:{query}"), SourceKind::YouTube);
        }

        if let Some(video_ids) = watch_videos_ids(identifier) {
            let selected = video_ids.split(',').find(|id| is_video_id(id));
            let url = format!("https://www.youtube.com/watch_videos?video_ids={video_ids}");
            return self
                .backend
                .load_playlist(&url, SourceKind::YouTube, selected);
        }

        let video_id = video_id_of(identifier);

        // A list= normally wins over a v=: watch?v=…&list=… loads the playlist
        // with that video selected. Account-only LL/WL/LM lists are the exception.
        if let Some(playlist_id) = playlist_id_of(identifier) {
            let selected = video_id.as_deref().or_else(|| mix_video_id(&playlist_id));
            if is_personal_watch_playlist(&playlist_id, selected) {
                let video_id = video_id.expect("personal lists only reach here from a watch URL");
                return self.backend.load_single(
                    &video_url(&video_id, is_music_url(identifier)),
                    SourceKind::YouTube,
                );
            }
            let url = playlist_url(&playlist_id, selected);
            return self
                .backend
                .load_playlist(&url, SourceKind::YouTube, selected);
        }

        let video_id = video_id.ok_or(SourceError::NotFound)?;
        self.backend.load_single(
            &video_url(&video_id, is_music_url(identifier)),
            SourceKind::YouTube,
        )
    }
}

/// The URL to hand yt-dlp for a playlist.
///
/// A mix (list=RD…) is generated from a video and /playlist?list=RD… does not
/// resolve, so when a video id is known the watch form is used — it works for both
/// kinds, while the bare form only works for real playlists.
fn playlist_url(playlist_id: &str, video_id: Option<&str>) -> String {
    match video_id {
        Some(video_id) => format!("https://www.youtube.com/watch?v={video_id}&list={playlist_id}"),
        None => format!("https://www.youtube.com/playlist?list={playlist_id}"),
    }
}

/// Strips an optional scheme and the host prefixes YouTube answers on.
///
fn youtube_host(identifier: &str) -> &str {
    let rest = strip_scheme(identifier).unwrap_or(identifier);
    let rest = rest.strip_prefix("www.").unwrap_or(rest);
    let rest = rest.strip_prefix("m.").unwrap_or(rest);
    rest.strip_prefix("music.").unwrap_or(rest)
}

/// Extracts a video id from the URL forms YouTube uses.
fn video_id_of(identifier: &str) -> Option<String> {
    if is_video_id(identifier) {
        return Some(identifier.to_owned());
    }
    let rest = youtube_host(identifier);

    /// The id is whatever precedes the first query, fragment or path separator.
    fn first_segment(rest: &str) -> Option<&str> {
        rest.split(['?', '&', '#', '/']).next()
    }

    let path_forms = [
        "youtu.be/",
        "youtube.com/live/",
        "youtube.com/shorts/",
        "youtube.com/embed/",
    ];
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

    // URL ids may carry extra path data without a separator. Lavaplayer takes the
    // first 11 URL-safe characters; raw identifiers still have to be exactly 11.
    let id = id.get(..11)?;
    if is_video_id(id) {
        Some(id.to_owned())
    } else {
        None
    }
}

/// Extracts a list= id from a YouTube URL.
///
/// Held to the same standard as video_id_of: an id that does not look like one
/// is None, so a malformed URL becomes empty rather than a subprocess launch.
fn playlist_id_of(identifier: &str) -> Option<String> {
    if is_direct_playlist_id(identifier) {
        return Some(identifier.to_owned());
    }
    let rest = youtube_host(identifier);
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

fn is_video_id(id: &str) -> bool {
    id.len() == 11 && id.chars().all(is_url_safe)
}

fn is_direct_playlist_id(id: &str) -> bool {
    ["PL", "LL", "FL", "UU"]
        .iter()
        .any(|prefix| id.starts_with(prefix))
        && id.len() > 2
        && id.chars().all(is_url_safe)
}

fn is_personal_playlist(id: &str) -> bool {
    ["LL", "WL", "LM"].iter().any(|prefix| id.starts_with(prefix))
}

fn is_personal_watch_playlist(id: &str, video_id: Option<&str>) -> bool {
    video_id.is_some() && is_personal_playlist(id)
}

fn mix_video_id(id: &str) -> Option<&str> {
    id.strip_prefix("RD")?.get(..11).filter(|id| is_video_id(id))
}

fn watch_videos_ids(identifier: &str) -> Option<&str> {
    let rest = youtube_host(identifier);
    let query = rest.strip_prefix("youtube.com/watch_videos?")?;
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix("video_ids="))
        .filter(|ids| !ids.is_empty())
}

fn is_music_url(identifier: &str) -> bool {
    strip_scheme(identifier)
        .unwrap_or(identifier)
        .starts_with("music.youtube.com/")
}

fn video_url(video_id: &str, music: bool) -> String {
    let host = if music { "music.youtube.com" } else { "www.youtube.com" };
    format!("https://{host}/watch?v={video_id}")
}

/// Rebuilds a safe page URL for playback. URI is client-provided in an
/// encodedTrack, so it may select the Music host only when it names the same
/// YouTube video as identifier.
pub(crate) fn playback_url(identifier: &str, uri: Option<&str>) -> String {
    let music = uri.is_some_and(|uri| {
        is_music_url(uri) && video_id_of(uri).as_deref() == Some(identifier)
    });
    video_url(identifier, music)
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
            "youtube.com/watch?v=dQw4w9WgXcQ",
            "https://youtu.be/dQw4w9WgXcQ",
            "https://youtu.be/dQw4w9WgXcQ?t=43",
            "https://music.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://www.youtube.com/live/dQw4w9WgXcQ",
            "https://www.youtube.com/shorts/dQw4w9WgXcQ",
            "https://www.youtube.com/embed/dQw4w9WgXcQ",
            "dQw4w9WgXcQ",
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
        assert_eq!(video_id_of("https://www.youtube.com/watch?v="), None);
        assert_eq!(video_id_of("https://www.youtube.com/watch"), None);
    }

    #[test]
    fn url_video_ids_are_truncated_but_raw_ids_are_not() {
        assert_eq!(
            video_id_of("https://youtu.be/dQw4w9WgXcQextra").as_deref(),
            Some("dQw4w9WgXcQ")
        );
        assert_eq!(video_id_of("dQw4w9WgXcQextra"), None);
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

    #[test]
    fn direct_playlist_ids_and_watch_videos_are_claimed() {
        let source = source();
        assert_eq!(
            playlist_id_of("PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI").as_deref(),
            Some("PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI")
        );
        assert_eq!(playlist_id_of("LLabcdefghijk").as_deref(), Some("LLabcdefghijk"));
        assert_eq!(playlist_id_of("FLabcdefghijk").as_deref(), Some("FLabcdefghijk"));
        assert_eq!(playlist_id_of("UUabcdefghijk").as_deref(), Some("UUabcdefghijk"));
        assert!(source.matches(
            "https://www.youtube.com/watch_videos?video_ids=dQw4w9WgXcQ,9bZkp7q19f0"
        ));
    }

    #[test]
    fn personal_watch_lists_still_name_the_video() {
        for list in ["LL", "WL", "LM123"] {
            assert!(is_personal_watch_playlist(list, Some("dQw4w9WgXcQ")));
            assert_eq!(
                video_id_of(&format!(
                    "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list={list}"
                ))
                .as_deref(),
                Some("dQw4w9WgXcQ")
            );
        }
    }

    #[test]
    fn playback_only_trusts_a_matching_music_uri() {
        assert_eq!(
            playback_url(
                "dQw4w9WgXcQ",
                Some("https://music.youtube.com/watch?v=dQw4w9WgXcQ")
            ),
            "https://music.youtube.com/watch?v=dQw4w9WgXcQ"
        );
        for uri in [
            "https://music.youtube.com/watch?v=9bZkp7q19f0",
            "https://example.invalid/watch?v=dQw4w9WgXcQ",
        ] {
            assert_eq!(
                playback_url("dQw4w9WgXcQ", Some(uri)),
                "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
            );
        }
    }

    /// A mix does not resolve through /playlist?list=, so a known video keeps the
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
        assert_eq!(mix_video_id("RDdQw4w9WgXcQ"), Some("dQw4w9WgXcQ"));
    }

    /// matches has to claim playlists, or they never reach load at all and the
    /// node answers empty for a perfectly good URL.
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

    /// youtubeSearchEnabled: false stops search prefixes from being claimed, but
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

    /// ytmsearch: must be claimed (so it reports loadType: "error", not
    /// "empty") but refused rather than silently degraded to a plain YouTube
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
