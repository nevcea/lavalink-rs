//! Container probing, shared by the local and HTTP sources.
//!
//! Produces the metadata `loadtracks` needs — title, author, duration, seekability —
//! and the container name that goes into the encoded track's source tail
//! ([`lavalink_protocol::SourceTail::Probe`]), which is what lavaplayer's probing
//! sources write there.

use symphonia::core::codecs::audio::{AudioCodecId, CODEC_ID_NULL_AUDIO};
use symphonia::core::codecs::CodecParameters;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::{MetadataOptions, StandardTag, Tag};
use symphonia::core::units::Timestamp;

use super::SourceError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probed {
    /// Container short name, e.g. `"mp3"`. Goes into the encoded track's tail.
    pub container: String,
    pub title: Option<String>,
    pub author: Option<String>,
    pub isrc: Option<String>,
    /// Milliseconds. `0` when the container does not say and we cannot infer it —
    /// which is what the original reports for the same inputs.
    pub duration_ms: i64,
}

/// Probes a media source.
///
/// Blocking; call from a blocking thread. `extension` is a hint only — symphonia
/// falls back to content sniffing, so a mislabelled file still works.
pub fn probe(source: Box<dyn MediaSource>, extension: Option<&str>) -> Result<Probed, SourceError> {
    let stream = MediaSourceStream::new(source, Default::default());

    let mut hint = Hint::new();
    if let Some(extension) = extension {
        hint.with_extension(extension);
    }

    let mut format = symphonia::default::get_probe()
        .probe(&hint, stream, FormatOptions::default(), MetadataOptions::default())
        .map_err(|source| SourceError::Unplayable {
            reason: format!("no supported container: {source}"),
        })?;

    // Read everything needed off the track before touching metadata, which needs a
    // mutable borrow of the reader.
    let (codec, duration_ms) = {
        let track = format
            .tracks()
            .iter()
            .find_map(|track| match &track.codec_params {
                Some(CodecParameters::Audio(params)) if params.codec != CODEC_ID_NULL_AUDIO => {
                    Some((track, params.codec))
                }
                _ => None,
            })
            .ok_or_else(|| SourceError::Unplayable {
                reason: "the container has no audio track".to_owned(),
            })?;

        let duration_ms = match (track.0.time_base, track.0.duration) {
            (Some(time_base), Some(duration)) => time_base
                .calc_time(Timestamp::new(duration.get() as i64))
                .map(|time| time.as_millis() as i64)
                .unwrap_or(0),
            // No frame count in the header: an MP3 without a Xing/VBRI frame, or a
            // WebM without cues. The original is in the same position and reports
            // what it can.
            _ => 0,
        };
        (track.1, duration_ms)
    };

    // Metadata found ahead of the stream (ID3v2) or held by the reader itself (Vorbis
    // comments) is unified onto the format reader's own metadata log.
    let tags = format
        .metadata()
        .current()
        .map(|metadata| metadata.media.tags.clone())
        .unwrap_or_default();

    Ok(Probed {
        container: container_name(codec),
        title: find_tag(&tags, |std| match std {
            StandardTag::TrackTitle(value) => Some(value.as_str()),
            _ => None,
        }),
        author: find_tag(&tags, |std| match std {
            StandardTag::Artist(value) => Some(value.as_str()),
            _ => None,
        })
        .or_else(|| {
            find_tag(&tags, |std| match std {
                StandardTag::AlbumArtist(value) => Some(value.as_str()),
                _ => None,
            })
        }),
        isrc: find_tag(&tags, |std| match std {
            StandardTag::IdentIsrc(value) => Some(value.as_str()),
            _ => None,
        }),
        duration_ms,
    })
}

fn find_tag(tags: &[Tag], matches: impl Fn(&StandardTag) -> Option<&str>) -> Option<String> {
    tags.iter()
        .filter_map(|tag| tag.std.as_ref().and_then(&matches))
        .map(str::to_owned)
        .find(|value| !value.trim().is_empty())
}

/// Best-effort container short name.
///
/// symphonia does not report which format the probe chose, so this derives a name
/// from the codec. The value only has to be stable and survive our own codec — the
/// original writes lavaplayer's probe names, and clients never interpret the tail,
/// they only hand it back.
fn container_name(codec: AudioCodecId) -> String {
    use symphonia::core::codecs::audio::well_known::{
        CODEC_ID_AAC, CODEC_ID_FLAC, CODEC_ID_MP3, CODEC_ID_OPUS, CODEC_ID_PCM_S16LE,
        CODEC_ID_VORBIS,
    };

    // An if-chain rather than a match: these are associated constants, not unit
    // variants, so they are not usable as patterns.
    if codec == CODEC_ID_MP3 {
        "mp3"
    } else if codec == CODEC_ID_FLAC {
        "flac"
    } else if codec == CODEC_ID_AAC {
        "aac"
    } else if codec == CODEC_ID_VORBIS {
        "ogg"
    } else if codec == CODEC_ID_OPUS {
        "matroska/webm|opus"
    } else if codec == CODEC_ID_PCM_S16LE {
        "wav"
    } else {
        "unknown"
    }
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_names_are_stable() {
        use symphonia::core::codecs::audio::well_known::CODEC_ID_MP3;
        assert_eq!(container_name(CODEC_ID_MP3), "mp3");
        assert_eq!(container_name(CODEC_ID_NULL_AUDIO), "unknown");
    }

    #[test]
    fn probing_something_that_is_not_audio_is_an_error() {
        let bytes = vec![0u8; 4096];
        let result = probe(Box::new(std::io::Cursor::new(bytes)), Some("mp3"));
        assert!(matches!(result, Err(SourceError::Unplayable { .. })));
    }

    fn title(value: &str) -> StandardTag {
        StandardTag::TrackTitle(std::sync::Arc::new(value.to_owned()))
    }

    #[test]
    fn tag_lookup_skips_blanks_and_prefers_the_first_real_value() {
        let tags = vec![
            Tag::new_from_parts("TITLE", "  ", Some(title("  "))),
            Tag::new_from_parts("TITLE", "Real", Some(title("Real"))),
        ];
        let find_title = |tags: &[Tag]| {
            find_tag(tags, |std| match std {
                StandardTag::TrackTitle(value) => Some(value.as_str()),
                _ => None,
            })
        };
        assert_eq!(find_title(&tags).as_deref(), Some("Real"));
        assert_eq!(
            find_tag(&tags, |std| match std {
                StandardTag::Artist(value) => Some(value.as_str()),
                _ => None,
            }),
            None
        );
    }
}
