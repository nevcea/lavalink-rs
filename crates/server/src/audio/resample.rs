//! Sample-rate and channel conversion to Discord's format.
//!
//! Everything reaching the ring must be 48kHz stereo. Sources are usually 44.1kHz,
//! often mono, occasionally 5.1, so this sits between the decoder and the filters.
//!
//! # Why this is hand-written
//!
//! A resampling library would be better at this — lavaplayer offers three quality
//! settings backed by a windowed-sinc implementation. What is here is Catmull-Rom
//! interpolation: continuous in the first derivative, so no discontinuities, but
//! with more high-frequency imaging than a proper band-limited resampler.
//!
//! That is a deliberate trade against the project's dependency budget, and it is a
//! real trade, not a free one. It is honest about where it stands: the
//! `resamplingQuality` config key is not implemented, because pretending to offer
//! three qualities when there is one would be worse than saying so.

use super::ring::{CHANNELS, SAMPLE_RATE};

/// Converts decoded audio to 48kHz stereo, carrying interpolation state across
/// buffers so block boundaries are not audible.
#[derive(Debug)]
pub struct Resampler {
    source_rate: u32,
    source_channels: usize,
    /// Fractional read position within the source stream, in frames.
    cursor: f64,
    /// The tail of the previous buffer, retained here so an upsampling call can
    /// interpolate across block boundaries. Also doubles as the working buffer for
    /// the call in progress: `process_into` appends the new buffer's frames onto
    /// the retained tail, reads from the combined slice, then drains the consumed
    /// prefix back down to the new tail — no allocation once warmed up.
    history: Vec<[f32; CHANNELS]>,
    /// Scratch space for `to_stereo_frames`, reused across calls for the same
    /// reason.
    frames: Vec<[f32; CHANNELS]>,
}

impl Resampler {
    pub fn new(source_rate: u32, source_channels: usize) -> Self {
        Self {
            source_rate: source_rate.max(1),
            source_channels: source_channels.max(1),
            cursor: 0.0,
            history: Vec::new(),
            frames: Vec::new(),
        }
    }

    /// Whether the source already matches the output format, in which case the
    /// samples pass through untouched.
    pub fn is_passthrough(&self) -> bool {
        self.source_rate == SAMPLE_RATE && self.source_channels == CHANNELS
    }

    /// Forgets carried-over state. Called after a seek, where the previous buffer's
    /// tail is from somewhere else in the track entirely.
    pub fn reset(&mut self) {
        self.cursor = 0.0;
        self.history.clear();
    }

    /// Converts one buffer of interleaved source samples, allocating a fresh `Vec`
    /// for the result. A thin wrapper over [`Self::process_into`] for callers (tests,
    /// the bench) that don't need to reuse a buffer across calls.
    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        let mut out = Vec::new();
        self.process_into(input, &mut out);
        out
    }

    /// Converts one buffer of interleaved source samples into `out`, which is
    /// cleared first and otherwise reused across calls — the pump's hot-path entry
    /// point, so a decoded packet costs no allocation here once warmed up.
    pub fn process_into(&mut self, input: &[f32], out: &mut Vec<f32>) {
        out.clear();

        // Already 48kHz stereo: `to_stereo_frames` plus a flatten below would be two
        // full-buffer copies for zero conversion. `is_passthrough` names the same
        // condition this used to check inline (`source_rate == SAMPLE_RATE`) without
        // also covering channel count, which meant this path ran through the
        // planar round-trip even when there was nothing to convert.
        if self.is_passthrough() {
            self.cursor = 0.0;
            self.history.clear();
            let usable = input.len() - input.len() % CHANNELS;
            out.extend_from_slice(&input[..usable]);
            return;
        }

        self.to_stereo_frames(input);
        if self.frames.is_empty() {
            return;
        }

        if self.source_rate == SAMPLE_RATE {
            self.cursor = 0.0;
            self.history.clear();
            out.extend(self.frames.iter().flatten());
            return;
        }

        // Three frames of history plus this buffer; interpolation reads one behind
        // and two ahead of the cursor.
        self.history.append(&mut self.frames);
        let source = &self.history;

        let step = f64::from(self.source_rate) / f64::from(SAMPLE_RATE);
        // `self.cursor` was already rebased onto the retained history at the end of
        // the previous call, so index 0 of `source` is exactly where it points.
        let mut cursor = self.cursor;

        // Stop two frames short: the last two have no right-hand neighbours yet, and
        // they become the next buffer's history instead of being guessed at.
        let usable = source.len().saturating_sub(2) as f64;
        while cursor < usable {
            let index = cursor.floor() as usize;
            let t = (cursor - cursor.floor()) as f32;

            for channel in 0..CHANNELS {
                let at = |offset: isize| -> f32 {
                    let i = index as isize + offset;
                    let i = i.clamp(0, source.len() as isize - 1) as usize;
                    source[i][channel]
                };
                out.push(catmull_rom(at(-1), at(0), at(1), at(2), t));
            }

            cursor += step;
        }

        // Keep enough tail for the next call, and re-express the cursor relative to
        // it so no frame is played twice or skipped.
        let keep = 3.min(self.history.len());
        let dropped = self.history.len() - keep;
        self.history.drain(..dropped);
        self.cursor = cursor - dropped as f64;
    }

    /// Interleaved source channels to stereo frames, written into `self.frames`.
    fn to_stereo_frames(&mut self, input: &[f32]) {
        let channels = self.source_channels;
        self.frames.clear();
        self.frames
            .extend(input.chunks_exact(channels).map(|frame| match channels {
                // Mono is duplicated rather than panned, matching every other player.
                1 => [frame[0], frame[0]],
                // More than two channels: take the front pair. Downmixing surround
                // properly needs the channel layout, which the decoder does not
                // always give us, and a wrong downmix sounds worse than a crop.
                _ => [frame[0], frame[1]],
            }));
    }
}

/// Catmull-Rom spline through `p1` and `p2`, using `p0` and `p3` for the slopes.
fn catmull_rom(p0: f32, p1: f32, p2: f32, p3: f32, t: f32) -> f32 {
    let t2 = t * t;
    let t3 = t2 * t;
    0.5 * ((2.0 * p1)
        + (-p0 + p2) * t
        + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * t2
        + (-p0 + 3.0 * p1 - 3.0 * p2 + p3) * t3)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds a sine through the resampler in chunks and returns the output.
    fn resample_sine(source_rate: u32, channels: usize, seconds: f64, chunk: usize) -> Vec<f32> {
        let mut resampler = Resampler::new(source_rate, channels);
        let total_frames = (source_rate as f64 * seconds) as usize;

        let mut input = Vec::with_capacity(total_frames * channels);
        for frame in 0..total_frames {
            let t = frame as f64 / source_rate as f64;
            let sample = (t * 440.0 * std::f64::consts::TAU).sin() as f32;
            for _ in 0..channels {
                input.push(sample);
            }
        }

        let mut out = Vec::new();
        for block in input.chunks(chunk * channels) {
            out.extend(resampler.process(block));
        }
        out
    }

    #[test]
    fn matching_format_is_passthrough() {
        let mut resampler = Resampler::new(SAMPLE_RATE, CHANNELS);
        assert!(resampler.is_passthrough());
        assert_eq!(resampler.process(&[0.5, -0.5]), vec![0.5, -0.5]);
    }

    /// The passthrough shortcut must actually skip the planar round-trip, not just
    /// happen to produce the same bytes — otherwise every already-48kHz-stereo
    /// packet pays two full-buffer copies for a straight pass-through.
    #[test]
    fn passthrough_does_not_touch_the_planar_scratch_buffers() {
        let mut resampler = Resampler::new(SAMPLE_RATE, CHANNELS);
        resampler.process(&[0.5, -0.5, 0.25, -0.25]);
        assert!(resampler.frames.is_empty(), "to_stereo_frames should not have run");
        assert!(resampler.history.is_empty(), "the interpolation history should stay empty");
    }

    /// A stray trailing sample (an odd-length buffer, which should not happen for
    /// interleaved stereo but is not this function's job to assume) is dropped
    /// rather than copied half-formed, matching what `to_stereo_frames`'s
    /// `chunks_exact` used to do on the slow path.
    #[test]
    fn passthrough_drops_an_incomplete_trailing_frame() {
        let mut resampler = Resampler::new(SAMPLE_RATE, CHANNELS);
        assert_eq!(resampler.process(&[0.5, -0.5, 0.25]), vec![0.5, -0.5]);
    }

    #[test]
    fn mono_is_duplicated_to_both_channels() {
        let mut resampler = Resampler::new(SAMPLE_RATE, 1);
        assert!(!resampler.is_passthrough());
        assert_eq!(resampler.process(&[0.25, -0.75]), vec![0.25, 0.25, -0.75, -0.75]);
    }

    #[test]
    fn surround_is_cropped_to_the_front_pair() {
        let mut resampler = Resampler::new(SAMPLE_RATE, 6);
        let frame = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        assert_eq!(resampler.process(&frame), vec![1.0, 2.0]);
    }

    #[test]
    fn output_length_tracks_the_rate_ratio() {
        let out = resample_sine(44_100, 2, 1.0, 1024);
        let frames = out.len() / CHANNELS;
        // One second in, one second out, within a few frames of slack for the tail
        // that is held back for the next buffer.
        assert!(
            (frames as i64 - i64::from(SAMPLE_RATE)).abs() < 64,
            "expected about {} frames, got {frames}",
            SAMPLE_RATE
        );
    }

    #[test]
    fn upsampling_preserves_the_waveform() {
        let out = resample_sine(44_100, 2, 0.1, 1024);
        assert!(!out.is_empty());
        assert!(out.iter().all(|sample| sample.is_finite()));

        // A 440Hz sine peaks near ±1; interpolation must not blow that up or crush it.
        let peak = out.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
        assert!((0.9..=1.1).contains(&peak), "peak was {peak}");
    }

    #[test]
    fn downsampling_works_too() {
        let out = resample_sine(96_000, 2, 0.1, 4096);
        let frames = out.len() / CHANNELS;
        let expected = (SAMPLE_RATE as f64 * 0.1) as i64;
        assert!((frames as i64 - expected).abs() < 64, "got {frames} frames");
    }

    /// The reason history is carried between calls: chunking the input must not
    /// change the output, or every decoder buffer boundary would click.
    #[test]
    fn chunking_does_not_change_the_result() {
        let whole = resample_sine(44_100, 2, 0.2, 1 << 20);
        let split = resample_sine(44_100, 2, 0.2, 512);

        assert!((whole.len() as i64 - split.len() as i64).abs() <= CHANNELS as i64);
        let compare = whole.len().min(split.len());
        for (index, (a, b)) in whole[..compare].iter().zip(&split[..compare]).enumerate() {
            assert!(
                (a - b).abs() < 1e-4,
                "sample {index} differs: {a} vs {b}"
            );
        }
    }

    #[test]
    fn a_reset_clears_carried_state() {
        let mut resampler = Resampler::new(44_100, 2);
        resampler.process(&vec![1.0; 4096]);
        resampler.reset();

        // After a reset the first output frame follows the new input, not the old
        // buffer's tail.
        let out = resampler.process(&vec![0.0; 4096]);
        assert!(out.iter().all(|sample| sample.abs() < 1e-6));
    }

    #[test]
    fn silence_stays_silent() {
        let mut resampler = Resampler::new(44_100, 2);
        let out = resampler.process(&vec![0.0; 8192]);
        assert!(out.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn an_empty_buffer_produces_nothing() {
        let mut resampler = Resampler::new(44_100, 2);
        assert!(resampler.process(&[]).is_empty());
    }

    #[test]
    fn catmull_rom_passes_through_its_control_points() {
        assert!((catmull_rom(0.0, 1.0, 2.0, 3.0, 0.0) - 1.0).abs() < 1e-6);
        assert!((catmull_rom(0.0, 1.0, 2.0, 3.0, 1.0) - 2.0).abs() < 1e-6);
    }

    #[test]
    fn catmull_rom_is_linear_on_a_straight_line() {
        // Interpolating a ramp must give the ramp back.
        assert!((catmull_rom(0.0, 1.0, 2.0, 3.0, 0.5) - 1.5).abs() < 1e-6);
    }
}
