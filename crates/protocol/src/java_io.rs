//! The DataInput/DataOutput primitives lavaplayer's track format is built from.
//!
//! Kept separate from crate::encoded_track so the byte-level layer can be tested
//! on its own — most codec bugs are really string-encoding bugs.
//!
//! The one that bites: writeUTF is not UTF-8. Java's "modified UTF-8" encodes
//! U+0000 as two bytes so no NUL appears mid-string, and encodes characters outside
//! the BMP as a surrogate pair of two three-byte sequences (six bytes) instead of
//! one four-byte sequence. A track titled with an emoji round-trips through the
//! original but not through String::as_bytes, so we work in UTF-16 code units.

use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum JavaIoError {
    #[error("unexpected end of input: wanted {wanted} bytes at offset {offset}, {available} left")]
    Eof {
        offset: usize,
        wanted: usize,
        available: usize,
    },
    #[error("malformed modified UTF-8 at offset {offset}")]
    MalformedUtf8 { offset: usize },
    #[error("string is {len} bytes when encoded, exceeding the 65535 byte limit of writeUTF")]
    StringTooLong { len: usize },
}

type Result<T> = std::result::Result<T, JavaIoError>;

pub struct DataInput<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> DataInput<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    pub fn remaining(&self) -> &'a [u8] {
        &self.bytes[self.offset..]
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let available = self.bytes.len() - self.offset;
        if available < n {
            return Err(JavaIoError::Eof {
                offset: self.offset,
                wanted: n,
                available,
            });
        }
        let slice = &self.bytes[self.offset..self.offset + n];
        self.offset += n;
        Ok(slice)
    }

    pub fn read_u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn read_bool(&mut self) -> Result<bool> {
        Ok(self.read_u8()? != 0)
    }

    pub fn read_u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    pub fn read_i32(&mut self) -> Result<i32> {
        let b = self.take(4)?;
        Ok(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn read_i64(&mut self) -> Result<i64> {
        let b = self.take(8)?;
        Ok(i64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    /// Java DataInput.readUTF.
    pub fn read_utf(&mut self) -> Result<String> {
        let len = self.read_u16()? as usize;
        let start = self.offset;
        let bytes = self.take(len)?;
        decode_modified_utf8(bytes).map_err(|relative| JavaIoError::MalformedUtf8 {
            offset: start + relative,
        })
    }

    /// lavaplayer DataFormatTools.readNullableText: a presence flag, then the text.
    pub fn read_nullable_utf(&mut self) -> Result<Option<String>> {
        if self.read_bool()? {
            Ok(Some(self.read_utf()?))
        } else {
            Ok(None)
        }
    }
}

#[derive(Debug, Default)]
pub struct DataOutput {
    bytes: Vec<u8>,
}

impl DataOutput {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    pub fn write_u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    pub fn write_bool(&mut self, value: bool) {
        self.bytes.push(u8::from(value));
    }

    pub fn write_i32(&mut self, value: i32) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    pub fn write_i64(&mut self, value: i64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    pub fn write_raw(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
    }

    /// Java DataOutput.writeUTF.
    ///
    /// Encodes straight into self.bytes behind a placeholder length, then patches
    /// the length in — rather than through encode_modified_utf8, whose Vec
    /// would be copied here and dropped. A track carries seven of these strings, so
    /// that is seven allocations per encode, and loadtracks encodes a whole
    /// playlist in a loop.
    pub fn write_utf(&mut self, value: &str) -> Result<()> {
        let start = self.bytes.len();
        self.bytes.extend_from_slice(&0u16.to_be_bytes());
        encode_modified_utf8_into(value, &mut self.bytes);

        let len = self.bytes.len() - start - 2;
        let Ok(encoded_len) = u16::try_from(len) else {
            // Roll back, or the half-written string stays in the buffer and every
            // field after it decodes as garbage. The error is recoverable for the
            // caller only if self is still usable.
            self.bytes.truncate(start);
            return Err(JavaIoError::StringTooLong { len });
        };
        self.bytes[start..start + 2].copy_from_slice(&encoded_len.to_be_bytes());
        Ok(())
    }

    /// lavaplayer DataFormatTools.writeNullableText.
    pub fn write_nullable_utf(&mut self, value: Option<&str>) -> Result<()> {
        let start = self.bytes.len();
        self.write_bool(value.is_some());
        if let Some(text) = value {
            if let Err(error) = self.write_utf(text) {
                self.bytes.truncate(start);
                return Err(error);
            }
        }
        Ok(())
    }
}

/// Java modified UTF-8, operating on UTF-16 code units.
pub fn encode_modified_utf8(value: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len());
    encode_modified_utf8_into(value, &mut out);
    out
}

/// encode_modified_utf8, appending to an existing buffer.
fn encode_modified_utf8_into(value: &str, out: &mut Vec<u8>) {
    for unit in value.encode_utf16() {
        match unit {
            // NUL is escaped to two bytes so it never appears as a 0 byte.
            0x0001..=0x007F => out.push(unit as u8),
            0x0000 | 0x0080..=0x07FF => {
                out.push(0xC0 | (unit >> 6) as u8);
                out.push(0x80 | (unit & 0x3F) as u8);
            }
            // Includes surrogates: each half is written as its own 3-byte sequence.
            _ => {
                out.push(0xE0 | (unit >> 12) as u8);
                out.push(0x80 | ((unit >> 6) & 0x3F) as u8);
                out.push(0x80 | (unit & 0x3F) as u8);
            }
        }
    }
}

/// Inverse of encode_modified_utf8. On failure returns the offset of the bad byte.
///
/// Byte-for-byte re-encoding is only guaranteed for input encode_modified_utf8
/// itself could have produced. An overlong sequence (e.g. 0xC1 0xA1 for 'a') is
/// accepted, matching DataInputStream.readUTF's lack of a shortest-form check, but
/// normalised: it re-encodes shorter. See the surrogate caveat below for the other
/// input this can't preserve.
pub fn decode_modified_utf8(bytes: &[u8]) -> std::result::Result<String, usize> {
    // Modified UTF-8 and UTF-8 agree byte-for-byte over 0x00..=0x7F, which is what
    // the overwhelming majority of these fields are. \0 is the one ASCII character
    // the two encodings disagree on, and it encodes to 0xC0 0x80 — not ASCII, so
    // it fails this check and takes the loop below. Same for every multi-byte
    // sequence and every surrogate half.
    if bytes.is_ascii() {
        // is_ascii already established this is valid UTF-8.
        return Ok(String::from_utf8_lossy(bytes).into_owned());
    }

    let mut units: Vec<u16> = Vec::with_capacity(bytes.len());
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];
        let continuation = |index: usize| -> std::result::Result<u16, usize> {
            match bytes.get(index) {
                Some(byte) if byte & 0xC0 == 0x80 => Ok((byte & 0x3F) as u16),
                _ => Err(index.min(bytes.len())),
            }
        };

        match b {
            0x00..=0x7F => {
                units.push(b as u16);
                i += 1;
            }
            0xC0..=0xDF => {
                units.push(((b & 0x1F) as u16) << 6 | continuation(i + 1)?);
                i += 2;
            }
            0xE0..=0xEF => {
                units.push(
                    ((b & 0x0F) as u16) << 12 | continuation(i + 1)? << 6 | continuation(i + 2)?,
                );
                i += 3;
            }
            // 0x80..=0xBF is a stray continuation byte; 0xF0.. never appears in
            // modified UTF-8, which has no 4-byte form.
            _ => return Err(i),
        }
    }

    // Unpaired surrogates are possible here — Java permits them — but a Rust
    // String can't hold one, so from_utf16_lossy replaces each with U+FFFD
    // instead. A real, accepted divergence from Java's readUTF, not a round
    // trip: re-encoding a title that had one produces a different byte sequence
    // than the input. See encoded_track.rs's module docs for what "byte-for-byte"
    // is scoped to.
    Ok(String::from_utf16_lossy(&units))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(value: &str) {
        let encoded = encode_modified_utf8(value);
        assert_eq!(decode_modified_utf8(&encoded).unwrap(), value, "{value:?}");
    }

    #[test]
    fn ascii_is_one_byte_per_char() {
        assert_eq!(encode_modified_utf8("abc"), b"abc");
        round_trip("Rick Astley - Never Gonna Give You Up");
    }

    #[test]
    fn nul_takes_two_bytes_unlike_utf8() {
        assert_eq!(encode_modified_utf8("\0"), vec![0xC0, 0x80]);
        round_trip("a\0b");
    }

    #[test]
    fn supplementary_chars_take_six_bytes_unlike_utf8() {
        // U+1F3B5 MUSICAL NOTE: 4 bytes in real UTF-8, 6 in modified UTF-8.
        let encoded = encode_modified_utf8("\u{1F3B5}");
        assert_eq!(encoded.len(), 6);
        assert_ne!(encoded, "\u{1F3B5}".as_bytes());
        round_trip("\u{1F3B5}");
        round_trip("track \u{1F3B5} title");
    }

    #[test]
    fn multibyte_bmp_round_trips() {
        round_trip("한국어 제목");
        round_trip("café");
    }

    #[test]
    fn truncated_sequence_is_rejected() {
        assert!(decode_modified_utf8(&[0xE0, 0x80]).is_err());
        assert!(decode_modified_utf8(&[0xC0]).is_err());
    }

    /// write_utf encodes in place behind a placeholder length, so an over-long
    /// string has already written its bytes by the time the length is found not to
    /// fit. It has to roll those back: a DataOutput left holding half a string
    /// would make every field written after it decode as garbage, and the caller has
    /// no way to tell that from a clean failure.
    #[test]
    fn an_over_long_string_leaves_the_buffer_untouched() {
        let mut out = DataOutput::new();
        out.write_i32(7);
        let before = out.bytes.clone();

        // Two bytes per char in modified UTF-8, so this overflows the u16 length.
        let too_long = "\u{0080}".repeat(40_000);
        assert!(matches!(
            out.write_utf(&too_long),
            Err(JavaIoError::StringTooLong { .. })
        ));

        assert_eq!(out.bytes, before, "the failed write must leave no residue");

        // And the buffer is still usable for the caller that wants to recover.
        out.write_utf("ok").unwrap();
        let mut input = DataInput::new(&out.bytes);
        assert_eq!(input.read_i32().unwrap(), 7);
        assert_eq!(input.read_utf().unwrap(), "ok");
    }

    #[test]
    fn an_over_long_nullable_string_rolls_back_its_presence_flag() {
        let mut out = DataOutput::new();
        out.write_i32(7);
        let before = out.bytes.clone();
        let too_long = "\u{0080}".repeat(40_000);

        assert!(matches!(
            out.write_nullable_utf(Some(&too_long)),
            Err(JavaIoError::StringTooLong { .. })
        ));
        assert_eq!(out.bytes, before, "the nullable flag must be rolled back too");

        out.write_nullable_utf(Some("ok")).unwrap();
        let mut input = DataInput::new(&out.bytes);
        assert_eq!(input.read_i32().unwrap(), 7);
        assert_eq!(input.read_nullable_utf().unwrap().as_deref(), Some("ok"));
    }

    #[test]
    fn data_output_round_trips_primitives() {
        let mut out = DataOutput::new();
        out.write_u8(2);
        out.write_utf("title").unwrap();
        out.write_i64(-1);
        out.write_bool(true);
        out.write_nullable_utf(None).unwrap();
        out.write_nullable_utf(Some("uri")).unwrap();

        let bytes = out.into_bytes();
        let mut input = DataInput::new(&bytes);
        assert_eq!(input.read_u8().unwrap(), 2);
        assert_eq!(input.read_utf().unwrap(), "title");
        assert_eq!(input.read_i64().unwrap(), -1);
        assert!(input.read_bool().unwrap());
        assert_eq!(input.read_nullable_utf().unwrap(), None);
        assert_eq!(input.read_nullable_utf().unwrap().as_deref(), Some("uri"));
        assert!(input.remaining().is_empty());
    }

    #[test]
    fn reading_past_the_end_reports_eof() {
        let mut input = DataInput::new(&[0x00]);
        assert!(matches!(input.read_i64(), Err(JavaIoError::Eof { .. })));
    }
}
