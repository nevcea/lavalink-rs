//! Deezer.
//!
//! Deezer's public API (`api.deezer.com`, no key required) hands back rich track
//! metadata — including an ISRC, which the platform's own search does not offer —
//! but only ever a 30-second preview clip as audio; the full track needs a paid
//! session this node does not have. So the metadata is Deezer's and the audio is
//! not: playback substitutes the best YouTube match at play time, the same
//! `sourceName`-keeps-its-identity approach the wider Lavalink ecosystem's LavaSrc
//! plugin uses for the same reason. See [`YtDlp::find_youtube_match`](super::ytdlp::YtDlp::find_youtube_match)
//! for the other half, run by `StreamOpener` at playback time rather than here at
//! load time, so the substitute is chosen fresh rather than going stale like a
//! stored stream URL would.
//!
//! Only artist- and track-level metadata is one HTTP call away. An album's or a
//! playlist's own listing endpoint reports each entry without its ISRC (Deezer
//! nests that per-track only on the dedicated track endpoint), so those come back
//! without one — the same trade `--flat-playlist` makes for yt-dlp, for the same
//! reason: fetching every entry individually would turn a playlist load into
//! dozens of requests.

use lavalink_protocol::encoded_track::SourceTail;
use lavalink_protocol::player::TrackInfo;
use reqwest::blocking::Client;
use serde::Deserialize;
use serde_json::Value;

use super::{
    build_client, classify_status, strip_scheme, SourceError, SourceLoad, SourceManager,
    SourcePlaylist, SourceTrack,
};

const API_BASE: &str = "https://api.deezer.com";

/// Deezer's code for "that id does not exist" — the one case worth telling apart
/// from a broken request, since it is what turns a load into `loadType: "empty"`
/// rather than an error.
const NO_DATA_ERROR_CODE: i64 = 800;

pub struct DeezerSource {
    client: Client,
}

impl DeezerSource {
    pub fn new(proxy: Option<reqwest::Proxy>) -> Result<Self, SourceError> {
        Ok(Self { client: build_client(proxy)? })
    }

    fn fetch<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        query: &[(&str, &str)],
    ) -> Result<T, SourceError> {
        let response = self
            .client
            .get(url)
            .query(query)
            .send()
            .map_err(|error| SourceError::Io(error.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            return Err(classify_status(status));
        }

        let text = response
            .text()
            .map_err(|error| SourceError::Io(error.to_string()))?;

        let unplayable = |error: serde_json::Error| SourceError::Unplayable {
            reason: format!("could not read deezer's response: {error}"),
        };
        let value: Value = serde_json::from_str(&text).map_err(unplayable)?;

        if let Some(error) = value.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_owned();
            let code = error.get("code").and_then(Value::as_i64);
            return Err(if code == Some(NO_DATA_ERROR_CODE) {
                SourceError::NotFound
            } else {
                SourceError::Unplayable { reason: message }
            });
        }

        serde_json::from_value(value).map_err(unplayable)
    }

    fn load_track(&self, id: &str) -> Result<SourceLoad, SourceError> {
        let track: DeezerTrack = self.fetch(&format!("{API_BASE}/track/{id}"), &[])?;
        Ok(SourceLoad::Track(track.into_track(None)))
    }

    fn load_album(&self, id: &str) -> Result<SourceLoad, SourceError> {
        let album: DeezerAlbum = self.fetch(&format!("{API_BASE}/album/{id}"), &[])?;
        let cover = album.cover_xl.or(album.cover_medium);
        let entries = album.tracks.map(|list| list.data).unwrap_or_default();
        into_playlist(album.title, entries, cover.as_deref())
    }

    fn load_playlist(&self, id: &str) -> Result<SourceLoad, SourceError> {
        let playlist: DeezerPlaylist = self.fetch(&format!("{API_BASE}/playlist/{id}"), &[])?;
        let entries = playlist.tracks.map(|list| list.data).unwrap_or_default();
        into_playlist(playlist.title, entries, None)
    }

    fn search(&self, query: &str) -> Result<SourceLoad, SourceError> {
        let results: SearchResults = self.fetch(&format!("{API_BASE}/search"), &[("q", query)])?;

        let tracks: Vec<SourceTrack> = results
            .data
            .into_iter()
            .map(|track| track.into_track(None))
            .collect();

        if tracks.is_empty() {
            return Err(SourceError::NotFound);
        }
        Ok(SourceLoad::Search(tracks))
    }
}

impl SourceManager for DeezerSource {
    fn name(&self) -> &'static str {
        "deezer"
    }

    fn matches(&self, identifier: &str) -> bool {
        identifier.starts_with("dzsearch:") || resource_of(identifier).is_some()
    }

    fn load(&self, identifier: &str) -> Result<SourceLoad, SourceError> {
        if let Some(query) = identifier.strip_prefix("dzsearch:") {
            if query.trim().is_empty() {
                return Err(SourceError::NotFound);
            }
            return self.search(query);
        }

        match resource_of(identifier) {
            Some((Resource::Track, id)) => self.load_track(&id),
            Some((Resource::Album, id)) => self.load_album(&id),
            Some((Resource::Playlist, id)) => self.load_playlist(&id),
            None => Err(SourceError::NotFound),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Resource {
    Track,
    Album,
    Playlist,
}

/// Extracts the resource kind and numeric id from a `deezer.com` URL.
///
/// Deezer pages optionally carry a locale segment (`/en/track/123`) ahead of the
/// kind; both forms are accepted. Dynamic share links (`deezer.page.link/...`) are
/// not — resolving one needs following a redirect first, which is unclaimed here
/// the same way `on.soundcloud.com` would be if this source did not special-case
/// it.
fn resource_of(identifier: &str) -> Option<(Resource, String)> {
    let rest = strip_scheme(identifier)?;
    let rest = rest.strip_prefix("www.").unwrap_or(rest);
    let rest = rest.strip_prefix("deezer.com/")?;
    let path = rest.split(['?', '#']).next().unwrap_or(rest);

    let mut segments = path.split('/').filter(|segment| !segment.is_empty());
    let mut kind = segments.next()?;
    if !["track", "album", "playlist"].contains(&kind) {
        kind = segments.next()?;
    }
    let id = segments.next()?;
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }

    let resource = match kind {
        "track" => Resource::Track,
        "album" => Resource::Album,
        "playlist" => Resource::Playlist,
        _ => return None,
    };
    Some((resource, id.to_owned()))
}

/// Builds a playlist result, filling in any entry with no cover of its own — the
/// case for every album entry, since Deezer nests covers per-album rather than
/// per-track there — from the parent's.
///
/// There is no entry point within a Deezer album or playlist the way a YouTube
/// `watch?v=…&list=…` names one, so the selection is always "none", which is what
/// `-1` on the wire means.
fn into_playlist(
    name: Option<String>,
    entries: Vec<DeezerTrack>,
    fallback_cover: Option<&str>,
) -> Result<SourceLoad, SourceError> {
    if entries.is_empty() {
        return Err(SourceError::NotFound);
    }
    let tracks = entries
        .into_iter()
        .map(|track| track.into_track(fallback_cover))
        .collect();

    Ok(SourceLoad::Playlist(SourcePlaylist {
        name: name.unwrap_or_else(|| "Unknown playlist".to_owned()),
        selected_track: -1,
        tracks,
    }))
}

#[derive(Debug, Deserialize)]
struct Artist {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AlbumCover {
    #[serde(default)]
    cover_xl: Option<String>,
    #[serde(default)]
    cover_medium: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DeezerTrack {
    id: i64,
    #[serde(default)]
    title: Option<String>,
    /// Seconds.
    #[serde(default)]
    duration: Option<i64>,
    #[serde(default)]
    isrc: Option<String>,
    #[serde(default)]
    link: Option<String>,
    #[serde(default)]
    artist: Option<Artist>,
    #[serde(default)]
    album: Option<AlbumCover>,
}

#[derive(Debug, Deserialize)]
struct TrackList {
    #[serde(default)]
    data: Vec<DeezerTrack>,
}

#[derive(Debug, Deserialize)]
struct DeezerAlbum {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    cover_xl: Option<String>,
    #[serde(default)]
    cover_medium: Option<String>,
    #[serde(default)]
    tracks: Option<TrackList>,
}

#[derive(Debug, Deserialize)]
struct DeezerPlaylist {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    tracks: Option<TrackList>,
}

#[derive(Debug, Deserialize)]
struct SearchResults {
    #[serde(default)]
    data: Vec<DeezerTrack>,
}

impl DeezerTrack {
    /// `fallback_cover` is the parent album's or playlist's own cover, used when
    /// this entry carries none of its own — always the case for an album listing.
    fn into_track(self, fallback_cover: Option<&str>) -> SourceTrack {
        let id = self.id.to_string();
        let uri = self
            .link
            .unwrap_or_else(|| format!("https://www.deezer.com/track/{id}"));
        let artwork_url = self
            .album
            .and_then(|album| album.cover_xl.or(album.cover_medium))
            .or_else(|| fallback_cover.map(str::to_owned));

        SourceTrack {
            info: TrackInfo {
                identifier: id,
                is_seekable: true,
                author: self
                    .artist
                    .and_then(|artist| artist.name)
                    .unwrap_or_else(|| "Unknown artist".to_owned()),
                length: self.duration.map(|seconds| seconds.saturating_mul(1000)).unwrap_or(0),
                is_stream: false,
                position: 0,
                title: self.title.unwrap_or_else(|| "Unknown title".to_owned()),
                uri: Some(uri),
                source_name: "deezer".to_owned(),
                artwork_url,
                isrc: self.isrc,
            },
            tail: SourceTail::Empty,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> DeezerSource {
        DeezerSource::new(None).unwrap()
    }

    #[test]
    fn claims_track_album_and_playlist_urls_with_or_without_a_locale() {
        let source = source();
        for identifier in [
            "https://www.deezer.com/track/3135556",
            "https://deezer.com/track/3135556",
            "https://www.deezer.com/en/track/3135556",
            "https://www.deezer.com/fr/album/302127",
            "https://www.deezer.com/us/playlist/908622995",
            "https://www.deezer.com/track/3135556?utm_source=x",
            "dzsearch:daft punk",
        ] {
            assert!(source.matches(identifier), "failed to claim {identifier}");
        }
    }

    #[test]
    fn other_sites_and_malformed_ids_are_not_ours() {
        let source = source();
        for identifier in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://open.spotify.com/track/abc",
            "https://www.deezer.com/",
            "https://www.deezer.com/track/",
            "https://www.deezer.com/track/not-a-number",
            "https://notdeezer.com/track/123",
            "/tmp/a.mp3",
        ] {
            assert!(!source.matches(identifier), "wrongly claimed {identifier}");
        }
    }

    #[test]
    fn resource_kind_and_id_are_parsed_correctly() {
        assert_eq!(
            resource_of("https://www.deezer.com/track/3135556"),
            Some((Resource::Track, "3135556".to_owned()))
        );
        assert_eq!(
            resource_of("https://www.deezer.com/en/album/302127"),
            Some((Resource::Album, "302127".to_owned()))
        );
        assert_eq!(
            resource_of("https://deezer.com/us/playlist/908622995"),
            Some((Resource::Playlist, "908622995".to_owned()))
        );
    }

    #[test]
    fn an_empty_search_query_is_not_found() {
        assert!(matches!(
            source().load("dzsearch:   "),
            Err(SourceError::NotFound)
        ));
    }

    /// `load` is only ever called after `matches`, but a mismatch must not become
    /// an HTTP request against an arbitrary string.
    #[test]
    fn an_unclaimed_identifier_is_not_found_rather_than_fetched() {
        assert!(matches!(
            source().load("https://example.invalid/a.mp3"),
            Err(SourceError::NotFound)
        ));
    }

    #[test]
    fn a_track_converts_with_its_own_metadata() {
        let track: DeezerTrack = serde_json::from_str(
            r#"{"id":3135556,"title":"Harder, Better, Faster, Stronger","duration":226,
                "isrc":"GBDUW0000059","link":"https://www.deezer.com/track/3135556",
                "artist":{"name":"Daft Punk"},
                "album":{"cover_xl":"https://example.invalid/cover.jpg"}}"#,
        )
        .unwrap();
        let source_track = track.into_track(None);

        assert_eq!(source_track.info.identifier, "3135556");
        assert_eq!(source_track.info.title, "Harder, Better, Faster, Stronger");
        assert_eq!(source_track.info.author, "Daft Punk");
        assert_eq!(source_track.info.length, 226_000);
        assert_eq!(source_track.info.isrc.as_deref(), Some("GBDUW0000059"));
        assert_eq!(source_track.info.source_name, "deezer");
        assert_eq!(
            source_track.info.artwork_url.as_deref(),
            Some("https://example.invalid/cover.jpg")
        );
    }

    /// An album listing entry carries no cover of its own; the parent's is used.
    #[test]
    fn a_coverless_entry_falls_back_to_the_parents_cover() {
        let track: DeezerTrack =
            serde_json::from_str(r#"{"id":1,"title":"t","artist":{"name":"a"}}"#).unwrap();
        let source_track = track.into_track(Some("https://example.invalid/album-cover.jpg"));
        assert_eq!(
            source_track.info.artwork_url.as_deref(),
            Some("https://example.invalid/album-cover.jpg")
        );
    }

    #[test]
    fn an_empty_track_list_is_not_found_rather_than_an_empty_playlist() {
        assert!(matches!(
            into_playlist(Some("Empty".to_owned()), Vec::new(), None),
            Err(SourceError::NotFound)
        ));
    }

    /// Deezer names no entry point within an album or playlist, so the selection
    /// is always "none".
    #[test]
    fn a_playlist_has_no_selected_track() {
        let track: DeezerTrack =
            serde_json::from_str(r#"{"id":1,"title":"t","artist":{"name":"a"}}"#).unwrap();
        let SourceLoad::Playlist(playlist) =
            into_playlist(Some("A Playlist".to_owned()), vec![track], None).unwrap()
        else {
            panic!("expected a playlist result");
        };
        assert_eq!(playlist.selected_track, -1);
        assert_eq!(playlist.name, "A Playlist");
    }

    /// The encoded form must survive our own codec, since clients hand it back.
    #[test]
    fn a_track_round_trips_through_the_codec() {
        let track: DeezerTrack =
            serde_json::from_str(r#"{"id":42,"title":"한국어 🎵","duration":180}"#).unwrap();
        let source_track = track.into_track(None);

        let encoded =
            lavalink_protocol::encoded_track::encode(&source_track.info, &source_track.tail)
                .unwrap();
        let decoded = lavalink_protocol::encoded_track::decode(&encoded).unwrap();

        assert_eq!(decoded.info, source_track.info);
        assert_eq!(decoded.tail, source_track.tail);
    }
}
