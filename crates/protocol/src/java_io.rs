//! The `DataInput`/`DataOutput` primitives lavaplayer's track format is built from.
//!
//! Kept separate from [`crate::encoded_track`] so the byte-level layer can be tested
//! on its own — most codec bugs are really string-encoding bugs.
//!
//! The one that bites: `writeUTF` is *not* UTF-8. Java's "modified UTF-8" encodes
//! U+0000 as two bytes so no NUL appears mid-string, and encodes characters outside
//! the BMP as a surrogate pair of two three-byte sequences (six bytes) instead of
//! one four-byte sequence. A track titled with an emoji round-trips through the
//! original but not through `String::as_bytes`, so we work in UTF-16 code units.

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

    pub fn offset(&self) -> usize {
        self.offset
    }

    pub fn remaining(&self) -> &'a [u8] {
        &self.bytes[self.offset..]
    }

    pub fn is_empty(&self) -> bool {
        self.offset >= self.bytes.len()
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

    /// Java `DataInput.readUTF`.
    pub fn read_utf(&mut self) -> Result<String> {
        let len = self.read_u16()? as usize;
        let start = self.offset;
        let bytes = self.take(len)?;
        decode_modified_utf8(bytes).map_err(|relative| JavaIoError::MalformedUtf8 {
            offset: start + relative,
        })
    }

    /// lavaplayer `DataFormatTools.readNullableText`: a presence flag, then the text.
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

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
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

    /// Java `DataOutput.writeUTF`.
    pub fn write_utf(&mut self, value: &str) -> Result<()> {
        let encoded = encode_modified_utf8(value);
        let len = u16::try_from(encoded.len()).map_err(|_| JavaIoError::StringTooLong {
            len: encoded.len(),
        })?;
        self.bytes.extend_from_slice(&len.to_be_bytes());
        self.bytes.extend_from_slice(&encoded);
        Ok(())
    }

    /// lavaplayer `DataFormatTools.writeNullableText`.
    pub fn write_nullable_utf(&mut self, value: Option<&str>) -> Result<()> {
        match value {
            Some(text) => {
                self.write_bool(true);
                self.write_utf(text)
            }
            None => {
                self.write_bool(false);
                Ok(())
            }
        }
    }
}

/// Java modified UTF-8, operating on UTF-16 code units.
pub fn encode_modified_utf8(value: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len());
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
    out
}

/// Inverse of [`encode_modified_utf8`]. On failure returns the offset of the bad byte.
pub fn decode_modified_utf8(bytes: &[u8]) -> std::result::Result<String, usize> {
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

    // Unpaired surrogates are possible here — Java permits them, and a title that
    // contains one must survive the round trip rather than be rejected.
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
        assert!(input.is_empty());
    }

    #[test]
    fn reading_past_the_end_reports_eof() {
        let mut input = DataInput::new(&[0x00]);
        assert!(matches!(input.read_i64(), Err(JavaIoError::Eof { .. })));
    }
}
