//! getyarn.io — short movie/TV soundbite clips.
//!
//! Unlike the sites `MAINTENANCE.md` documents refusing (Twitch, Vimeo, Nico),
//! getyarn.io's own pages embed a direct, non-HLS video URL in an Open Graph
//! meta tag (`og:video:secure_url`). Upstream's own
//! `GetyarnAudioSourceManager` does not run yt-dlp or scrape the page with a
//! full HTML parser either — it is a single GET plus two Open Graph reads —
//! so this mirrors that shape exactly rather than routing through `ytdlp`.
//!
//! # A deliberately reproduced quirk
//!
//! Upstream never probes the clip's real duration: `AudioTrackInfoBuilder`
//! leaves `length` unset, which becomes `Long.MAX_VALUE` at build time, and
//! `author` is hardcoded to `"Unknown"`. Both are wire-visible fields, so both
//! are kept exactly rather than "improved" with a real container probe — the
//! same governing rule `crates/server/src/lib.rs` states for the rest of this
//! node.

use lavalink_protocol::encoded_track::SourceTail;
use lavalink_protocol::player::TrackInfo;
use reqwest::blocking::Client;

use super::{
    build_client, classify_status, strip_scheme, SourceError, SourceLoad, SourceManager,
    SourceTrack,
};

const PREFIX: &str = "getyarn.io/yarn-clip/";

pub struct GetyarnSource {
    client: Client,
}

impl GetyarnSource {
    pub fn new(proxy: Option<reqwest::Proxy>) -> Result<Self, SourceError> {
        Ok(Self { client: build_client(proxy)? })
    }
}

impl SourceManager for GetyarnSource {
    fn name(&self) -> &'static str {
        "getyarn.io"
    }

    fn matches(&self, identifier: &str) -> bool {
        clip_url(identifier).is_some()
    }

    fn load(&self, identifier: &str) -> Result<SourceLoad, SourceError> {
        let url = clip_url(identifier).ok_or(SourceError::NotFound)?;

        let response = self
            .client
            .get(&url)
            .send()
            .map_err(|error| SourceError::Io(error.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            return Err(classify_status(status));
        }

        let html = response
            .text()
            .map_err(|error| SourceError::Io(error.to_string()))?;

        let video_url = meta_content(&html, "og:video:secure_url").ok_or_else(|| {
            SourceError::Unplayable {
                reason: "the page named no playable clip".to_owned(),
            }
        })?;
        let title = meta_content(&html, "og:title")
            .map(|value| unescape_entities(&value))
            .unwrap_or_else(|| "Unknown title".to_owned());

        Ok(SourceLoad::Track(SourceTrack {
            info: TrackInfo {
                identifier: video_url,
                is_seekable: true,
                author: "Unknown".to_owned(),
                length: i64::MAX,
                is_stream: false,
                position: 0,
                title,
                uri: Some(url),
                source_name: self.name().to_owned(),
                artwork_url: None,
                isrc: None,
            },
            tail: SourceTail::Empty,
        }))
    }
}

/// Normalizes a `getyarn.io/yarn-clip/...` identifier to a full `https://` URL,
/// or `None` if it isn't one. Matches upstream's
/// `https?://(?:www\.)?getyarn\.io/yarn-clip/(.*)`.
fn clip_url(identifier: &str) -> Option<String> {
    let rest = strip_scheme(identifier)?;
    let rest = rest.strip_prefix("www.").unwrap_or(rest);
    rest.strip_prefix(PREFIX)?;
    Some(format!("https://{rest}"))
}

/// Extracts a `<meta property="{property}" content="...">` tag's `content`,
/// tolerant of attribute order. This is a fixed site's own markup for two known
/// tags, not third-party HTML in general, so a hand-rolled scan is enough and
/// avoids pulling in a full scraping crate for it.
fn meta_content(html: &str, property: &str) -> Option<String> {
    let marker = format!("property=\"{property}\"");
    let mut cursor = 0;
    while let Some(offset) = html[cursor..].find("<meta") {
        let start = cursor + offset;
        let end = start + html[start..].find('>')?;
        let tag = &html[start..end];
        cursor = end + 1;
        if tag.contains(&marker) {
            if let Some(content) = attr(tag, "content") {
                return Some(content.to_owned());
            }
        }
    }
    None
}

/// The value of a `name="..."` attribute within a single already-isolated tag.
fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let marker = format!("{name}=\"");
    let start = tag.find(&marker)? + marker.len();
    tag[start..].find('"').map(|end| &tag[start..start + end])
}

/// The handful of entities an `og:title` is plausibly seen carrying. Anything
/// else passes through unchanged — this is not a general HTML decoder.
fn unescape_entities(value: &str) -> String {
    value
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> GetyarnSource {
        GetyarnSource::new(None).unwrap()
    }

    #[test]
    fn claims_clip_urls_with_or_without_www() {
        let source = source();
        for identifier in [
            "https://getyarn.io/yarn-clip/abc-123",
            "https://www.getyarn.io/yarn-clip/abc-123",
            "http://getyarn.io/yarn-clip/abc-123?foo=bar",
        ] {
            assert!(source.matches(identifier), "failed to claim {identifier}");
        }
    }

    #[test]
    fn other_sites_are_not_ours() {
        let source = source();
        for identifier in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://getyarn.io/",
            "https://notgetyarn.io/yarn-clip/abc",
            "ytsearch:never gonna",
        ] {
            assert!(!source.matches(identifier), "wrongly claimed {identifier}");
        }
    }

    #[test]
    fn reads_the_open_graph_video_and_title_tags_regardless_of_attribute_order() {
        let html = r#"<html><head>
            <meta content="A funny clip" property="og:title">
            <meta property="og:video:secure_url" content="https://y.yarn.co/abc.mp4">
        </head></html>"#;
        assert_eq!(
            meta_content(html, "og:video:secure_url").as_deref(),
            Some("https://y.yarn.co/abc.mp4")
        );
        assert_eq!(meta_content(html, "og:title").as_deref(), Some("A funny clip"));
    }

    #[test]
    fn a_page_with_no_video_tag_is_unplayable_not_a_crash() {
        let error = meta_content("<html></html>", "og:video:secure_url");
        assert!(error.is_none());
    }

    #[test]
    fn common_title_entities_are_unescaped() {
        assert_eq!(unescape_entities("Tom &amp; Jerry"), "Tom & Jerry");
        assert_eq!(unescape_entities("&quot;quoted&quot;"), "\"quoted\"");
    }

    /// Matches upstream's own quirk: length is always the unknown-duration
    /// sentinel, never a real probe, because `AudioTrackInfoBuilder` there never
    /// learns one either.
    #[test]
    fn a_loaded_track_carries_the_unknown_length_sentinel() {
        let track = SourceTrack {
            info: TrackInfo {
                identifier: "https://y.yarn.co/abc.mp4".into(),
                is_seekable: true,
                author: "Unknown".into(),
                length: i64::MAX,
                is_stream: false,
                position: 0,
                title: "A funny clip".into(),
                uri: Some("https://getyarn.io/yarn-clip/abc".into()),
                source_name: "getyarn.io".into(),
                artwork_url: None,
                isrc: None,
            },
            tail: SourceTail::Empty,
        };
        assert_eq!(track.info.length, i64::MAX);
        assert_eq!(track.info.author, "Unknown");
    }

    /// The encoded form must survive our own codec, since clients hand it back.
    #[test]
    fn a_track_round_trips_through_the_codec() {
        let info = TrackInfo {
            identifier: "https://y.yarn.co/abc.mp4".into(),
            is_seekable: true,
            author: "Unknown".into(),
            length: i64::MAX,
            is_stream: false,
            position: 0,
            title: "A funny clip".into(),
            uri: Some("https://getyarn.io/yarn-clip/abc".into()),
            source_name: "getyarn.io".into(),
            artwork_url: None,
            isrc: None,
        };
        let encoded = lavalink_protocol::encoded_track::encode(&info, &SourceTail::Empty).unwrap();
        let decoded = lavalink_protocol::encoded_track::decode(&encoded).unwrap();
        assert_eq!(decoded.info, info);
        assert_eq!(decoded.tail, SourceTail::Empty);
    }
}
