//! Test signal shared by the benches in this directory.
//!
//! Each bench is its own binary, so this is compiled once per bench and most of it
//! is unused in any given one — hence the blanket `dead_code` allow.

#![allow(dead_code)]

/// A deterministic sawtooth-ish ramp over `[-1.0, 1.0)`.
///
/// 997 is prime, so the period lines up with no buffer size any bench uses and the
/// filters see a genuinely varying signal rather than a repeating block. The exact
/// shape does not matter — what matters is that every bench feeds the same one, so
/// numbers from `filter`, `resample` and `ring` are about the code and not about
/// which signal each happened to pick.
pub fn samples(count: usize) -> Vec<f32> {
    (0..count)
        .map(|i| ((i % 997) as f32 / 997.0) * 2.0 - 1.0)
        .collect()
}

/// [`samples`] laid out the way the filter chain wants it: one `Vec` per channel.
pub fn planar(channels: usize, frames: usize) -> Vec<Vec<f32>> {
    (0..channels).map(|_| samples(frames)).collect()
}

/// [`samples`] laid out the way the pump and the resampler want it.
pub fn interleaved(channels: usize, frames: usize) -> Vec<f32> {
    samples(frames * channels)
}
