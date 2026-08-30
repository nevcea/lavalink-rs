//! Test fixtures shared by the benches in this directory.
//!
//! Each bench is its own binary, so this is compiled once per bench and most of it
//! is unused in any given one — hence the blanket dead_code allow.

#![allow(dead_code)]

use lavalink_protocol::player::TrackInfo;

pub fn write_wav(
    path: &std::path::Path,
    sample_rate: u32,
    channels: u16,
    seconds: usize,
    sample: impl Fn(usize) -> i16,
) {
    let frames = sample_rate as usize * seconds;
    let data_len = frames * usize::from(channels) * size_of::<i16>();
    let block_align = channels * 2;
    let mut bytes = Vec::with_capacity(44 + data_len);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&channels.to_le_bytes());
    bytes.extend_from_slice(&sample_rate.to_le_bytes());
    bytes.extend_from_slice(&(sample_rate * u32::from(block_align)).to_le_bytes());
    bytes.extend_from_slice(&block_align.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&(data_len as u32).to_le_bytes());
    for frame in 0..frames {
        let value = sample(frame);
        for _ in 0..channels {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
    }
    std::fs::write(path, bytes).unwrap();
}

pub fn track_info(
    path: &std::path::Path,
    seconds: usize,
    author: &str,
    title: &str,
) -> TrackInfo {
    TrackInfo {
        identifier: path.to_str().unwrap().to_owned(),
        is_seekable: true,
        author: author.into(),
        length: (seconds * 1000) as i64,
        is_stream: false,
        position: 0,
        title: title.into(),
        uri: None,
        source_name: "local".into(),
        artwork_url: None,
        isrc: None,
    }
}

/// A deterministic sawtooth-ish ramp over [-1.0, 1.0).
///
/// 997 is prime, so the period lines up with no buffer size any bench uses and the
/// filters see a genuinely varying signal rather than a repeating block. The exact
/// shape does not matter — what matters is that every bench feeds the same one, so
/// numbers from filter, resample and ring are about the code and not about
/// which signal each happened to pick.
pub fn samples(count: usize) -> Vec<f32> {
    (0..count)
        .map(|i| ((i % 997) as f32 / 997.0) * 2.0 - 1.0)
        .collect()
}

/// samples laid out the way the filter chain wants it: one Vec per channel.
pub fn planar(channels: usize, frames: usize) -> Vec<Vec<f32>> {
    (0..channels).map(|_| samples(frames)).collect()
}

/// samples laid out the way the pump and the resampler want it.
pub fn interleaved(channels: usize, frames: usize) -> Vec<f32> {
    samples(frames * channels)
}
