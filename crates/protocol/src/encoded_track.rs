//! The lavaplayer `encodedTrack` binary format.
//!
//! This is the single largest compatibility risk in the port: every client stores
//! these strings and hands them back, so a byte we read differently is a track that
//! will not play.
//!
//! # Layout
//!
//! Base64 (standard alphabet, padded) of:
//!
//! ```text
//! u32  header      size in the low 30 bits, flags in the top 2
//! u8   version     only when flags & 1; absent means version 1
//! utf  title
//! utf  author
//! i64  length      milliseconds
//! utf  identifier
//! u8   isStream
//! ?utf uri
//! ?utf artworkUrl  version >= 3 only
//! ?utf isrc        version >= 3 only
//! utf  sourceName
//! ..   source tail see SourceTail
//! i64  position    milliseconds
//! ```
//!
//! `isSeekable` is absent from the format — lavaplayer derives it from `isStream`,
//! and so do we.
//!
//! # Round-trip guarantee for sources we do not manage
//!
//! The tail is source-specific and we only know the shape of the three sources we
//! support. Rather than reject everything else, an unrecognised source's tail is
//! captured verbatim ([`SourceTail::Raw`]) — the position is always the last eight
//! bytes, so the tail's extent is unambiguous without parsing it. A YouTube track
//! encoded by some other node therefore survives decode/encode byte-for-byte.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use thiserror::Error;

use crate::java_io::{DataInput, DataOutput, JavaIoError};
use crate::player::{Track, TrackInfo};

/// Flag bit meaning "a version byte follows".
const FLAG_VERSIONED: u32 = 1;
/// The version lavaplayer 2.x writes, and therefore what we write.
pub const TRACK_INFO_VERSION: u8 = 3;
/// First version carrying `artworkUrl` and `isrc`.
const VERSION_WITH_ARTWORK_AND_ISRC: u8 = 3;
const SIZE_MASK: u32 = 0x3FFF_FFFF;

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("not valid base64: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error(transparent)]
    Io(#[from] JavaIoError),
    #[error("track version {0} is not supported")]
    UnsupportedVersion(u8),
    #[error("declared payload size {declared} does not fit in {actual} available bytes")]
    SizeMismatch { declared: usize, actual: usize },
    #[error("track payload is too short to contain a trailing position")]
    MissingPosition,
    #[error("{0}")]
    Invalid(String),
}

type Result<T> = std::result::Result<T, CodecError>;

/// The source-specific bytes between `sourceName` and `position`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceTail {
    /// Sources that write nothing, e.g. `youtube`.
    Empty,
    /// Probing sources (`http`, `local`) write the container probe name, sometimes
    /// with parameters appended — `"mp3"`, `"matroska/webm|opus"`.
    Probe(String),
    /// A source we do not model. Preserved so the track re-encodes unchanged.
    Raw(Vec<u8>),
}

impl SourceTail {
    /// Whether a source name is one whose tail we know how to parse.
    fn is_probing(source_name: &str) -> bool {
        matches!(source_name, "http" | "local")
    }

    /// Sources known to write an empty tail.
    fn is_empty_tail(source_name: &str) -> bool {
        matches!(source_name, "youtube")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedTrack {
    pub info: TrackInfo,
    pub tail: SourceTail,
    /// The version this track was encoded with. Preserved so re-encoding an old
    /// track does not silently upgrade it.
    pub version: u8,
}

impl DecodedTrack {
    pub fn new(info: TrackInfo, tail: SourceTail) -> Self {
        Self {
            info,
            tail,
            version: TRACK_INFO_VERSION,
        }
    }

    /// Pairs the decoded info with the string it came from, which is what the REST
    /// `decodetrack(s)` endpoints return.
    pub fn into_track(self, encoded: String) -> Track {
        Track::new(encoded, self.info)
    }
}

/// Decodes a base64 `encodedTrack`.
pub fn decode(encoded: &str) -> Result<DecodedTrack> {
    decode_bytes(&BASE64.decode(encoded)?)
}

pub fn decode_bytes(bytes: &[u8]) -> Result<DecodedTrack> {
    let mut input = DataInput::new(bytes);

    let header = input.read_i32()? as u32;
    let size = (header & SIZE_MASK) as usize;
    let flags = header >> 30;

    let payload = input.remaining();
    if payload.len() < size {
        return Err(CodecError::SizeMismatch {
            declared: size,
            actual: payload.len(),
        });
    }
    // Trailing bytes past the declared size are ignored, as `MessageInput` does.
    let payload = &payload[..size];

    if payload.len() < 8 {
        return Err(CodecError::MissingPosition);
    }
    let (body, position_bytes) = payload.split_at(payload.len() - 8);
    let position = i64::from_be_bytes(
        position_bytes
            .try_into()
            .expect("split_at guarantees 8 bytes"),
    );

    let mut input = DataInput::new(body);
    let version = if flags & FLAG_VERSIONED != 0 {
        input.read_u8()?
    } else {
        1
    };
    if version > TRACK_INFO_VERSION {
        return Err(CodecError::UnsupportedVersion(version));
    }

    let title = input.read_utf()?;
    let author = input.read_utf()?;
    let length = input.read_i64()?;
    let identifier = input.read_utf()?;
    let is_stream = input.read_bool()?;
    let uri = input.read_nullable_utf()?;

    let (artwork_url, isrc) = if version >= VERSION_WITH_ARTWORK_AND_ISRC {
        (input.read_nullable_utf()?, input.read_nullable_utf()?)
    } else {
        (None, None)
    };

    let source_name = input.read_utf()?;

    let tail = if SourceTail::is_probing(&source_name) {
        SourceTail::Probe(input.read_utf()?)
    } else if SourceTail::is_empty_tail(&source_name) {
        SourceTail::Empty
    } else {
        let rest = input.remaining();
        if rest.is_empty() {
            SourceTail::Empty
        } else {
            SourceTail::Raw(rest.to_vec())
        }
    };

    Ok(DecodedTrack {
        info: TrackInfo {
            identifier,
            // Not stored; lavaplayer derives it the same way.
            is_seekable: !is_stream,
            author,
            length,
            is_stream,
            position,
            title,
            uri,
            source_name,
            artwork_url,
            isrc,
        },
        tail,
        version,
    })
}

/// Encodes a track, writing version [`TRACK_INFO_VERSION`].
pub fn encode(info: &TrackInfo, tail: &SourceTail) -> Result<String> {
    encode_with_version(info, tail, TRACK_INFO_VERSION)
}

pub fn encode_with_version(info: &TrackInfo, tail: &SourceTail, version: u8) -> Result<String> {
    Ok(BASE64.encode(encode_to_bytes(info, tail, version)?))
}

pub fn encode_to_bytes(info: &TrackInfo, tail: &SourceTail, version: u8) -> Result<Vec<u8>> {
    if version == 0 || version > TRACK_INFO_VERSION {
        return Err(CodecError::UnsupportedVersion(version));
    }

    let mut body = DataOutput::new();
    let versioned = version > 1;
    if versioned {
        body.write_u8(version);
    }

    body.write_utf(&info.title)?;
    body.write_utf(&info.author)?;
    body.write_i64(info.length);
    body.write_utf(&info.identifier)?;
    body.write_bool(info.is_stream);
    body.write_nullable_utf(info.uri.as_deref())?;

    if version >= VERSION_WITH_ARTWORK_AND_ISRC {
        body.write_nullable_utf(info.artwork_url.as_deref())?;
        body.write_nullable_utf(info.isrc.as_deref())?;
    }

    body.write_utf(&info.source_name)?;

    match tail {
        SourceTail::Empty => {}
        SourceTail::Probe(probe) => body.write_utf(probe)?,
        SourceTail::Raw(raw) => body.write_raw(raw),
    }

    body.write_i64(info.position);

    let body = body.into_bytes();
    let size = u32::try_from(body.len())
        .ok()
        .filter(|size| *size <= SIZE_MASK)
        .ok_or_else(|| CodecError::Invalid(format!("track payload of {} bytes is too large", body.len())))?;

    let flags = if versioned { FLAG_VERSIONED } else { 0 };
    let mut out = DataOutput::new();
    out.write_i32((size | (flags << 30)) as i32);
    out.write_raw(&body);
    Ok(out.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Produced by the original server; lifted from the upstream protocol tests
    /// (`PlayerSerializerTest.kt:16`). Version 2, so no artworkUrl/isrc.
    const RICK: &str = "QAAAjQIAJVJpY2sgQXN0bGV5IC0gTmV2ZXIgR29ubmEgR2l2ZSBZb3UgVXAADlJpY2tBc3RsZXlWRVZPAAAAAAADPCAAC2RRdzR3OVdnWGNRAAEAK2h0dHBzOi8vd3d3LnlvdXR1YmUuY29tL3dhdGNoP3Y9ZFF3NHc5V2dYY1EAB3lvdXR1YmUAAAAAAAAAAA==";

    fn sample(source_name: &str) -> TrackInfo {
        TrackInfo {
            identifier: "id".into(),
            is_seekable: true,
            author: "author".into(),
            length: 212_000,
            is_stream: false,
            position: 0,
            title: "title".into(),
            uri: Some("https://example.invalid/a.mp3".into()),
            source_name: source_name.into(),
            artwork_url: None,
            isrc: None,
        }
    }

    #[test]
    fn decodes_an_original_encoded_track() {
        let track = decode(RICK).unwrap();

        assert_eq!(track.version, 2);
        assert_eq!(track.info.title, "Rick Astley - Never Gonna Give You Up");
        assert_eq!(track.info.author, "RickAstleyVEVO");
        assert_eq!(track.info.length, 212_000);
        assert_eq!(track.info.identifier, "dQw4w9WgXcQ");
        assert!(!track.info.is_stream);
        assert!(track.info.is_seekable);
        assert_eq!(
            track.info.uri.as_deref(),
            Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ")
        );
        assert_eq!(track.info.source_name, "youtube");
        assert_eq!(track.info.position, 0);
        // Version 2 predates both fields.
        assert_eq!(track.info.artwork_url, None);
        assert_eq!(track.info.isrc, None);
        assert_eq!(track.tail, SourceTail::Empty);
    }

    /// Re-encoding at the track's own version reproduces the original bytes exactly.
    #[test]
    fn re_encodes_an_original_track_byte_for_byte() {
        let track = decode(RICK).unwrap();
        let re_encoded = encode_with_version(&track.info, &track.tail, track.version).unwrap();
        assert_eq!(re_encoded, RICK);
    }

    #[test]
    fn header_declares_size_and_versioned_flag() {
        let bytes = BASE64.decode(RICK).unwrap();
        let header = u32::from_be_bytes(bytes[..4].try_into().unwrap());
        assert_eq!(header >> 30, FLAG_VERSIONED);
        assert_eq!((header & SIZE_MASK) as usize, bytes.len() - 4);
    }

    /// Our encode → our decode agrees, across the boundary cases that have bitten
    /// this format before.
    #[test]
    fn round_trips_boundary_cases() {
        let cases = [
            ("plain http", {
                let mut info = sample("http");
                info.position = 1234;
                (info, SourceTail::Probe("mp3".into()))
            }),
            ("null uri", {
                let mut info = sample("local");
                info.uri = None;
                (info, SourceTail::Probe("matroska/webm|opus".into()))
            }),
            ("unicode and emoji title", {
                let mut info = sample("http");
                info.title = "한국어 \u{1F3B5} title".into();
                info.author = "작곡가\0".into();
                (info, SourceTail::Probe("mp3".into()))
            }),
            ("stream", {
                let mut info = sample("http");
                info.is_stream = true;
                info.is_seekable = false;
                info.length = i64::MAX;
                (info, SourceTail::Probe("mp3".into()))
            }),
            ("artwork and isrc present", {
                let mut info = sample("youtube");
                info.artwork_url = Some("https://example.invalid/art.jpg".into());
                info.isrc = Some("USRC17607839".into());
                (info, SourceTail::Empty)
            }),
            ("unknown source with an opaque tail", {
                let info = sample("bandcamp");
                (info, SourceTail::Raw(vec![0x00, 0x01, 0xFF, 0x7F]))
            }),
        ];

        for (name, (info, tail)) in cases {
            let encoded = encode(&info, &tail).unwrap();
            let decoded = decode(&encoded).unwrap();
            assert_eq!(decoded.info, info, "info mismatch for {name}");
            assert_eq!(decoded.tail, tail, "tail mismatch for {name}");
            assert_eq!(decoded.version, TRACK_INFO_VERSION, "version for {name}");
        }
    }

    #[test]
    fn is_seekable_is_derived_from_is_stream() {
        let mut info = sample("http");
        info.is_stream = true;
        // Deliberately inconsistent input: the format has no seekable bit, so the
        // decoded value follows isStream regardless of what we encoded from.
        info.is_seekable = true;

        let decoded = decode(&encode(&info, &SourceTail::Probe("mp3".into())).unwrap()).unwrap();
        assert!(!decoded.info.is_seekable);
    }

    #[test]
    fn rejects_truncated_input() {
        let bytes = BASE64.decode(RICK).unwrap();
        let truncated = BASE64.encode(&bytes[..bytes.len() / 2]);
        assert!(matches!(
            decode(&truncated),
            Err(CodecError::SizeMismatch { .. })
        ));
    }

    #[test]
    fn rejects_a_future_version() {
        let mut bytes = BASE64.decode(RICK).unwrap();
        bytes[4] = TRACK_INFO_VERSION + 1;
        assert!(matches!(
            decode(&BASE64.encode(&bytes)),
            Err(CodecError::UnsupportedVersion(_))
        ));
    }

    #[test]
    fn rejects_non_base64() {
        assert!(matches!(decode("not base64!!"), Err(CodecError::Base64(_))));
    }
}
