//! Container probing, shared by the local and HTTP sources.
//!
//! Produces the metadata `loadtracks` needs — title, author, duration, seekability —
//! and the container name that goes into the encoded track's source tail
//! ([`lavalink_protocol::SourceTail::Probe`]), which is what lavaplayer's probing
//! sources write there.

use symphonia::core::codecs::{CodecType, CODEC_TYPE_NULL};
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::{MetadataOptions, StandardTagKey, Tag};
use symphonia::core::probe::Hint;

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

    let mut probed = symphonia::default::get_probe()
        .format(
            &hint,
            stream,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|source| SourceError::Unplayable {
            reason: format!("no supported container: {source}"),
        })?;

    let mut format = probed.format;

    // Read everything needed off the track before touching metadata, which needs a
    // mutable borrow of the reader.
    let (codec, duration_ms) = {
        let track = format
            .tracks()
            .iter()
            .find(|track| track.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or_else(|| SourceError::Unplayable {
                reason: "the container has no audio track".to_owned(),
            })?;

        let params = &track.codec_params;
        let duration_ms = match (params.time_base, params.n_frames) {
            (Some(time_base), Some(frames)) => {
                let time = time_base.calc_time(frames);
                (time.seconds as i64) * 1000 + (time.frac * 1000.0) as i64
            }
            // No frame count in the header: an MP3 without a Xing/VBRI frame, or a
            // WebM without cues. The original is in the same position and reports
            // what it can.
            _ => 0,
        };
        (params.codec, duration_ms)
    };

    let mut tags = Vec::new();
    // Metadata arrives either ahead of the stream (ID3v2, collected by the probe) or
    // from inside it (Vorbis comments, held by the reader).
    if let Some(metadata) = probed.metadata.get().as_ref().and_then(|m| m.current()) {
        tags.extend_from_slice(metadata.tags());
    }
    if let Some(metadata) = format.metadata().current() {
        tags.extend_from_slice(metadata.tags());
    }

    Ok(Probed {
        container: container_name(codec),
        title: find_tag(&tags, StandardTagKey::TrackTitle),
        author: find_tag(&tags, StandardTagKey::Artist)
            .or_else(|| find_tag(&tags, StandardTagKey::AlbumArtist)),
        isrc: find_tag(&tags, StandardTagKey::IdentIsrc),
        duration_ms,
    })
}

fn find_tag(tags: &[Tag], key: StandardTagKey) -> Option<String> {
    tags.iter()
        .filter(|tag| tag.std_key == Some(key))
        .map(|tag| tag.value.to_string())
        .find(|value| !value.trim().is_empty())
}

/// Best-effort container short name.
///
/// symphonia does not report which format the probe chose, so this derives a name
/// from the codec. The value only has to be stable and survive our own codec — the
/// original writes lavaplayer's probe names, and clients never interpret the tail,
/// they only hand it back.
fn container_name(codec: CodecType) -> String {
    use symphonia::core::codecs::{
        CODEC_TYPE_AAC, CODEC_TYPE_FLAC, CODEC_TYPE_MP3, CODEC_TYPE_OPUS, CODEC_TYPE_PCM_S16LE,
        CODEC_TYPE_VORBIS,
    };

    // An if-chain rather than a match: these are associated constants, not unit
    // variants, so they are not usable as patterns.
    if codec == CODEC_TYPE_MP3 {
        "mp3"
    } else if codec == CODEC_TYPE_FLAC {
        "flac"
    } else if codec == CODEC_TYPE_AAC {
        "aac"
    } else if codec == CODEC_TYPE_VORBIS {
        "ogg"
    } else if codec == CODEC_TYPE_OPUS {
        "matroska/webm|opus"
    } else if codec == CODEC_TYPE_PCM_S16LE {
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
        use symphonia::core::codecs::CODEC_TYPE_MP3;
        assert_eq!(container_name(CODEC_TYPE_MP3), "mp3");
        assert_eq!(container_name(CODEC_TYPE_NULL), "unknown");
    }

    #[test]
    fn probing_something_that_is_not_audio_is_an_error() {
        let bytes = vec![0u8; 4096];
        let result = probe(Box::new(std::io::Cursor::new(bytes)), Some("mp3"));
        assert!(matches!(result, Err(SourceError::Unplayable { .. })));
    }

    #[test]
    fn tag_lookup_skips_blanks_and_prefers_the_first_real_value() {
        use symphonia::core::meta::Value;

        let tags = vec![
            Tag::new(Some(StandardTagKey::TrackTitle), "TITLE", Value::from("  ")),
            Tag::new(Some(StandardTagKey::TrackTitle), "TITLE", Value::from("Real")),
        ];
        assert_eq!(find_tag(&tags, StandardTagKey::TrackTitle).as_deref(), Some("Real"));
        assert_eq!(find_tag(&tags, StandardTagKey::Artist), None);
    }
}
