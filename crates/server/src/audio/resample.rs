//! Sample-rate and channel conversion to Discord's format.
//!
//! Everything reaching the ring must be 48kHz stereo. Sources are usually 44.1kHz,
//! often mono, occasionally 5.1, so this sits between the decoder and the filters.
//!
//! resamplingQuality
//!
//! lavaplayer offers three quality settings backed by a windowed-sinc
//! implementation. ResamplingQuality::Low (lavaplayer's own default) stays on
//! the hand-written Catmull-Rom interpolation below: continuous in the first
//! derivative, so no discontinuities, but with more high-frequency imaging than a
//! proper band-limited resampler — and, being arithmetic rather than a filter
//! bank, free. Medium/High route through SincEngine, a thin wrapper over
//! rubato::Async in sinc mode, matching lavaplayer's actual approach at those tiers.

use rubato::audioadapter_buffers::direct::SequentialSliceOfVecs;
use rubato::{
    Async, FixedAsync, Indexing, Resampler as RubatoResampler, SincInterpolationParameters,
    SincInterpolationType, WindowFunction,
};

use crate::config::ResamplingQuality;

use super::ring::{CHANNELS, SAMPLE_RATE};

/// Converts decoded audio to 48kHz stereo, carrying interpolation state across
/// buffers so block boundaries are not audible.
#[derive(Debug)]
pub struct Resampler {
    source_rate: u32,
    source_channels: usize,
    /// Fractional read position within the source stream, in frames.
    cursor: f64,
    /// The last frames of the previous buffer, so an upsampling call can interpolate
    /// across a block boundary instead of guessing at the edge. Interpolation reads
    /// one frame behind the cursor and two ahead, so three is all that is ever
    /// needed, and a fixed array says so — this used to be a Vec that the whole
    /// decoded packet was appended onto and then drained back down to three frames,
    /// which cost a full-buffer copy per packet to carry three frames across.
    prologue: [[f32; CHANNELS]; PROLOGUE_FRAMES],
    /// How much of prologue is live. Below PROLOGUE_FRAMES only at the start of a
    /// stream and after a Resampler::reset.
    prologue_len: usize,
    /// The working buffer: Resampler::fill_stereo_frames writes the live
    /// prologue followed by this call's converted frames, and the interpolation
    /// loop reads straight out of it. Reused across calls, so a warmed-up pump
    /// allocates nothing here.
    frames: Vec<[f32; CHANNELS]>,
    /// Set only when actual rate conversion is needed and Medium/High was
    /// configured; Low, and any source that is already 48kHz, never construct
    /// one. When present, it replaces the Catmull-Rom loop below entirely rather
    /// than the two coexisting — prologue/cursor stay unused in that case,
    /// since rubato carries its own delay-line state across calls instead of
    /// needing the artificial neighbor padding Catmull-Rom does.
    sinc: Option<SincEngine>,
}

/// Frames carried from one buffer to the next: one for the interpolator's left-hand
/// neighbor, two for the right-hand pair it stops short of.
const PROLOGUE_FRAMES: usize = 3;

impl Resampler {
    pub fn new(source_rate: u32, source_channels: usize, quality: ResamplingQuality) -> Self {
        let source_rate = source_rate.max(1);
        let needs_conversion = source_rate != SAMPLE_RATE;
        let sinc = (needs_conversion && quality != ResamplingQuality::Low)
            .then(|| SincEngine::new(quality, source_rate));
        Self {
            source_rate,
            source_channels: source_channels.max(1),
            cursor: 0.0,
            prologue: [[0.0; CHANNELS]; PROLOGUE_FRAMES],
            prologue_len: 0,
            frames: Vec::new(),
            sinc,
        }
    }

    /// Whether the source already matches the output format, in which case the
    /// samples pass through untouched.
    pub fn is_passthrough(&self) -> bool {
        self.source_rate == SAMPLE_RATE && self.source_channels == CHANNELS
    }

    /// Whether this resampler is configured for exactly rate/channels.
    ///
    /// The pump checks every decoded packet against what the decoder actually
    /// produced, because Resampler::new is built from what the container
    /// declared and the two can disagree — see pump.rs's decode_loop.
    /// Normalised the same way new normalises its arguments, so a caller that
    /// passes the same values new was given always gets true.
    pub fn matches_source(&self, rate: u32, channels: usize) -> bool {
        self.source_rate == rate.max(1) && self.source_channels == channels.max(1)
    }

    /// Forgets carried-over state. Called after a seek, where the previous buffer's
    /// tail is from somewhere else in the track entirely.
    pub fn reset(&mut self) {
        self.cursor = 0.0;
        self.prologue_len = 0;
        if let Some(sinc) = &mut self.sinc {
            sinc.reset();
        }
    }

    /// Whether anything from a previous buffer is still carried — what
    /// Self::reset throws away. Exists so pump's tests can tell a seek that
    /// reset this from one that left it alone, which is otherwise only observable
    /// as a discontinuity in the audio itself.
    #[cfg(test)]
    pub fn carries_state(&self) -> bool {
        self.cursor != 0.0 || self.prologue_len > 0
    }

    /// Converts one buffer of interleaved source samples, allocating a fresh Vec
    /// for the result. A thin wrapper over Self::process_into that exists only so
    /// the tests below can read a return value; nothing on the playback path allocates
    /// per buffer, so this must not grow a production caller.
    #[cfg(test)]
    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        let mut out = Vec::new();
        self.process_into(input, &mut out);
        out
    }

    /// Converts one buffer of interleaved source samples into out, which is
    /// cleared first and otherwise reused across calls — the pump's hot-path entry
    /// point, so a decoded packet costs no allocation here once warmed up.
    pub fn process_into(&mut self, input: &[f32], out: &mut Vec<f32>) {
        out.clear();

        // Already 48kHz stereo: fill_stereo_frames plus a flatten below would be two
        // full-buffer copies for zero conversion. is_passthrough names the same
        // condition this used to check inline (source_rate == SAMPLE_RATE) without
        // also covering channel count, which meant this path ran through the
        // planar round-trip even when there was nothing to convert.
        if self.is_passthrough() {
            self.cursor = 0.0;
            self.prologue_len = 0;
            let usable = input.len() - input.len() % CHANNELS;
            out.extend_from_slice(&input[..usable]);
            return;
        }

        // Channel conversion only: every frame is emitted as-is, so nothing is
        // interpolated and nothing has to be carried across the boundary.
        if self.source_rate == SAMPLE_RATE {
            self.fill_stereo_frames(input, 0);
            self.cursor = 0.0;
            self.prologue_len = 0;
            out.extend(self.frames.iter().flatten());
            return;
        }

        // Sinc quality: no prologue, no manual cursor — fill_stereo_frames only
        // needs to do the channel conversion (mono duplication, surround crop),
        // and SincEngine owns everything about the rate conversion itself.
        if self.sinc.is_some() {
            self.fill_stereo_frames(input, 0);
            let Resampler { frames, sinc, .. } = self;
            sinc.as_mut().unwrap().process_into(frames, out);
            return;
        }

        // The carried prologue followed by this buffer, written in one pass so the
        // interpolation below can read a single contiguous slice.
        self.fill_stereo_frames(input, self.prologue_len);
        if self.frames.len() <= self.prologue_len {
            // No new frames. Leave the prologue and the cursor exactly as they were,
            // so the next call with real input picks up where this one would have.
            return;
        }
        let source = &self.frames;

        let step = f64::from(self.source_rate) / f64::from(SAMPLE_RATE);
        // self.cursor was already rebased onto the retained history at the end of
        // the previous call, so index 0 of source is exactly where it points.
        let mut cursor = self.cursor;

        // Stop two frames short: the last two have no right-hand neighbours yet, and
        // they become the next buffer's history instead of being guessed at.
        let usable = source.len().saturating_sub(2) as f64;

        // An estimate of how many frames the loop below will emit, not an exact
        // count: the loop advances cursor by step each iteration and stops on a
        // float comparison (cursor < usable), which can disagree with this ceil()
        // by one iteration. Good enough that the pushes essentially never re-grow,
        // not a guarantee that they can't.
        out.reserve((((usable - cursor).max(0.0) / step).ceil() as usize) * CHANNELS);

        // needless_range_loop: channel is not indexing one slice, it is picking
        // the same lane out of four separate frames. Iterating any one of them would
        // not give the other three, so the index is the point.
        #[allow(clippy::needless_range_loop)]
        while cursor < usable {
            let base = cursor.floor();
            let index = base as usize;
            let t = (cursor - base) as f32;
            let weights = catmull_rom_weights(t);

            if index == 0 {
                // The one frame with no left-hand neighbor, so p0 repeats p1.
                // This is what the clamp used to produce, hoisted out of the inner
                // loop: it is true for at most the first output frame of a buffer,
                // and was being re-decided for every tap of every frame.
                for channel in 0..CHANNELS {
                    out.push(catmull_rom(
                        source[0][channel],
                        source[0][channel],
                        source[1][channel],
                        source[2][channel],
                        &weights,
                    ));
                }
            } else {
                // cursor < usable means index <= source.len() - 3, so all four
                // taps are in range on their own — the upper clamp never bound
                // anything. Taking the window once lets the four reads share a
                // single bounds check instead of paying one each.
                let window = &source[index - 1..index + 3];
                for channel in 0..CHANNELS {
                    out.push(catmull_rom(
                        window[0][channel],
                        window[1][channel],
                        window[2][channel],
                        window[3][channel],
                        &weights,
                    ));
                }
            }

            cursor += step;
        }

        // Carry the tail into the next call, and re-express the cursor relative to it
        // so no frame is played twice or skipped.
        let keep = PROLOGUE_FRAMES.min(self.frames.len());
        let dropped = self.frames.len() - keep;
        self.prologue[..keep].copy_from_slice(&self.frames[dropped..]);
        self.prologue_len = keep;
        self.cursor = cursor - dropped as f64;
    }

    /// Emits the finite tail held by a windowed-sinc resampler at EOF.
    pub fn finish_into(&mut self, out: &mut Vec<f32>) {
        out.clear();
        if let Some(sinc) = &mut self.sinc {
            sinc.finish_into(out);
        }
    }

    /// Writes the first prologue frames of Self::prologue followed by input
    /// converted to stereo frames, into self.frames.
    fn fill_stereo_frames(&mut self, input: &[f32], prologue: usize) {
        let channels = self.source_channels;
        self.frames.clear();
        self.frames.extend_from_slice(&self.prologue[..prologue]);
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

/// Windowed-sinc resampling for ResamplingQuality::Medium/High, wrapping
/// rubato::Async (sinc, fixed input size). Never constructed for Low — that
/// stays on the Catmull-Rom path above.
///
/// Debug is hand-rolled (not derived) because Async itself has no Debug
/// impl — its precomputed sinc filter table isn't something a {:?} on the
/// owning Resampler needs to see anyway.
struct SincEngine {
    resampler: Async<f32>,
    ratio: f64,
    input_frames: usize,
    output_frames: usize,
    delay_remaining: usize,
    /// Stereo frames already converted by Resampler::fill_stereo_frames,
    /// waiting for enough to submit one call to resampler
    /// (rubato::Resampler::input_frames_next frames, fixed for the engine's
    /// lifetime since the resample ratio is never changed after construction).
    pending: Vec<[f32; CHANNELS]>,
    /// Planar scratch reused across calls — rubato takes and returns
    /// non-interleaved buffers, unlike everything else in this pipeline.
    planar_in: Vec<Vec<f32>>,
    planar_out: Vec<Vec<f32>>,
}

impl std::fmt::Debug for SincEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SincEngine").field("pending", &self.pending.len()).finish_non_exhaustive()
    }
}

impl SincEngine {
    fn new(quality: ResamplingQuality, source_rate: u32) -> Self {
        let params = match quality {
            // Resampler::new never constructs a SincEngine for Low.
            ResamplingQuality::Low => unreachable!("Low stays on the Catmull-Rom path"),
            // sinc_len/oversampling_factor scaled down from rubato's own
            // documented "high quality" preset (256/256/Cubic/BlackmanHarris2);
            // cheaper interpolation (Linear) too, since Medium exists
            // specifically to cost less than High.
            ResamplingQuality::Medium => sinc_params(128, 128, SincInterpolationType::Linear),
            ResamplingQuality::High => sinc_params(256, 256, SincInterpolationType::Cubic),
        };

        let ratio = f64::from(SAMPLE_RATE) / f64::from(source_rate);
        // max_resample_ratio_relative: 1.0 — the ratio is fixed for the whole
        // track, never ramped, so input_frames_next() stays constant too.
        // FixedAsync::Input matches the old SincFixedIn's shape: input size fixed,
        // output size varies.
        let resampler =
            Async::<f32>::new_sinc(ratio, 1.0, &params, 1024, CHANNELS, FixedAsync::Input)
                .expect("static parameters and a positive ratio are always valid");
        let chunk = resampler.input_frames_next();
        let delay_remaining = resampler.output_delay();

        Self {
            resampler,
            ratio,
            input_frames: 0,
            output_frames: 0,
            delay_remaining,
            pending: Vec::with_capacity(chunk),
            planar_in: (0..CHANNELS).map(|_| Vec::with_capacity(chunk)).collect(),
            planar_out: vec![Vec::new(); CHANNELS],
        }
    }

    fn reset(&mut self) {
        RubatoResampler::reset(&mut self.resampler);
        self.input_frames = 0;
        self.output_frames = 0;
        self.delay_remaining = self.resampler.output_delay();
        self.pending.clear();
    }

    /// Feeds newly-converted frames in, writing any fully-resampled output onto
    /// out (interleaved). Frames short of a full chunk are held in pending
    /// for the next call, the same carry-across-buffers shape the Catmull-Rom
    /// path uses prologue for.
    fn process_into(&mut self, frames: &[[f32; CHANNELS]], out: &mut Vec<f32>) {
        self.pending.extend_from_slice(frames);

        let chunk = self.resampler.input_frames_next();
        let mut consumed = 0;
        while self.pending.len() - consumed >= chunk {
            for lane in &mut self.planar_in {
                lane.clear();
            }
            for frame in &self.pending[consumed..consumed + chunk] {
                for (channel, lane) in self.planar_in.iter_mut().enumerate() {
                    lane.push(frame[channel]);
                }
            }
            consumed += chunk;
            self.input_frames += chunk;
            self.process_planar(chunk, None, None, out);
        }

        self.pending.drain(..consumed);
    }

    fn finish_into(&mut self, out: &mut Vec<f32>) {
        out.clear();
        let partial = self.pending.len();
        if partial > 0 {
            for lane in &mut self.planar_in {
                lane.clear();
            }
            for frame in self.pending.drain(..) {
                for (channel, lane) in self.planar_in.iter_mut().enumerate() {
                    lane.push(frame[channel]);
                }
            }
            self.input_frames += partial;
            let expected = (self.ratio * self.input_frames as f64).ceil() as usize;
            self.process_planar(
                partial,
                Some(Indexing::new().partial_len(partial)),
                Some(expected),
                out,
            );
        }

        let expected = (self.ratio * self.input_frames as f64).ceil() as usize;
        while self.output_frames < expected {
            for lane in &mut self.planar_in {
                lane.clear();
            }
            self.process_planar(
                0,
                Some(Indexing::new().partial_len(0)),
                Some(expected),
                out,
            );
        }
    }

    fn process_planar(
        &mut self,
        input_frames: usize,
        indexing: Option<Indexing>,
        output_limit: Option<usize>,
        out: &mut Vec<f32>,
    ) {
        let out_frames = self.resampler.output_frames_next();
        for lane in &mut self.planar_out {
            lane.clear();
            lane.resize(out_frames, 0.0);
        }

        let input_adapter = SequentialSliceOfVecs::new(&self.planar_in, CHANNELS, input_frames)
            .expect("planar_in holds every submitted frame per channel");
        let mut output_adapter =
            SequentialSliceOfVecs::new_mut(&mut self.planar_out, CHANNELS, out_frames)
                .expect("planar_out holds exactly out_frames frames per channel");
        let (read, produced) = self
            .resampler
            .process_into_buffer(&input_adapter, &mut output_adapter, indexing.as_ref())
            .expect("input matches the full or partial chunk submitted to rubato");
        if indexing.is_none() {
            debug_assert_eq!(read, input_frames);
        }

        let skip = self.delay_remaining.min(produced);
        self.delay_remaining -= skip;
        let available = produced - skip;
        let take = output_limit
            .map_or(available, |limit| limit.saturating_sub(self.output_frames).min(available));
        out.reserve(take * CHANNELS);
        for index in skip..skip + take {
            for lane in &self.planar_out {
                out.push(lane[index]);
            }
        }
        self.output_frames += take;
    }
}

fn sinc_params(
    sinc_len: usize,
    oversampling_factor: usize,
    interpolation: SincInterpolationType,
) -> SincInterpolationParameters {
    let window = WindowFunction::BlackmanHarris2;
    SincInterpolationParameters {
        sinc_len,
        // None lets rubato derive the cutoff itself via the same calculate_cutoff
        // this used to call explicitly — the recommended way to set this field.
        f_cutoff: None,
        oversampling_factor,
        interpolation,
        window,
    }
}

/// Catmull-Rom spline through p1 and p2, using p0 and p3 for the slopes.
fn catmull_rom_weights(t: f32) -> [f32; 4] {
    let t2 = t * t;
    let t3 = t2 * t;
    [
        0.5 * (-t + 2.0 * t2 - t3),
        0.5 * (2.0 - 5.0 * t2 + 3.0 * t3),
        0.5 * (t + 4.0 * t2 - 3.0 * t3),
        0.5 * (-t2 + t3),
    ]
}

fn catmull_rom(p0: f32, p1: f32, p2: f32, p3: f32, weights: &[f32; 4]) -> f32 {
    p0 * weights[0] + p1 * weights[1] + p2 * weights[2] + p3 * weights[3]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds a sine through the resampler in chunks and returns the output.
    fn resample_sine(source_rate: u32, channels: usize, seconds: f64, chunk: usize) -> Vec<f32> {
        resample_sine_quality(source_rate, channels, seconds, chunk, ResamplingQuality::Low)
    }

    fn resample_sine_quality(
        source_rate: u32,
        channels: usize,
        seconds: f64,
        chunk: usize,
        quality: ResamplingQuality,
    ) -> Vec<f32> {
        let mut resampler = Resampler::new(source_rate, channels, quality);
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
        let mut tail = Vec::new();
        resampler.finish_into(&mut tail);
        out.extend(tail);
        out
    }

    /// A ramp, so a frame carried over from somewhere else shows up as a jump
    /// rather than hiding inside a periodic signal.
    fn ramp(frames: usize) -> Vec<f32> {
        (0..frames * CHANNELS)
            .map(|i| ((i % 997) as f32 / 997.0) * 2.0 - 1.0)
            .collect()
    }

    /// reset is what a seek calls, and a seek lands somewhere unrelated to where
    /// the last buffer left off. Any frame or cursor offset surviving it would
    /// interpolate the new position against the old one — a click at the start of
    /// every seek.
    ///
    /// Stronger than a_reset_clears_carried_state below, which checks only that
    /// the first output frame follows the new input: this asserts the reset
    /// resampler is indistinguishable from a fresh one over a whole buffer, which is
    /// what the carried cursor and the retained tail together have to guarantee.
    #[test]
    fn reset_leaves_nothing_of_the_previous_position_behind() {
        let input = ramp(2_000);

        let fresh = Resampler::new(44_100, CHANNELS, ResamplingQuality::Low).process(&input);

        let mut resampler = Resampler::new(44_100, CHANNELS, ResamplingQuality::Low);
        resampler.process(&ramp(3_000));
        resampler.reset();
        let after_reset = resampler.process(&input);

        assert_eq!(
            after_reset, fresh,
            "a reset resampler must behave exactly like a new one"
        );
    }

    #[test]
    fn matching_format_is_passthrough() {
        let mut resampler = Resampler::new(SAMPLE_RATE, CHANNELS, ResamplingQuality::Low);
        assert!(resampler.is_passthrough());
        assert_eq!(resampler.process(&[0.5, -0.5]), vec![0.5, -0.5]);
    }

    /// The passthrough shortcut must actually skip the planar round-trip, not just
    /// happen to produce the same bytes — otherwise every already-48kHz-stereo
    /// packet pays two full-buffer copies for a straight pass-through.
    #[test]
    fn passthrough_does_not_touch_the_planar_scratch_buffers() {
        let mut resampler = Resampler::new(SAMPLE_RATE, CHANNELS, ResamplingQuality::Low);
        resampler.process(&[0.5, -0.5, 0.25, -0.25]);
        assert!(resampler.frames.is_empty(), "fill_stereo_frames should not have run");
        assert_eq!(resampler.prologue_len, 0, "no frame should have been carried");
    }

    /// A stray trailing sample (an odd-length buffer, which should not happen for
    /// interleaved stereo but is not this function's job to assume) is dropped
    /// rather than copied half-formed, matching what fill_stereo_frames's
    /// chunks_exact used to do on the slow path.
    #[test]
    fn passthrough_drops_an_incomplete_trailing_frame() {
        let mut resampler = Resampler::new(SAMPLE_RATE, CHANNELS, ResamplingQuality::Low);
        assert_eq!(resampler.process(&[0.5, -0.5, 0.25]), vec![0.5, -0.5]);
    }

    #[test]
    fn mono_is_duplicated_to_both_channels() {
        let mut resampler = Resampler::new(SAMPLE_RATE, 1, ResamplingQuality::Low);
        assert!(!resampler.is_passthrough());
        assert_eq!(resampler.process(&[0.25, -0.75]), vec![0.25, 0.25, -0.75, -0.75]);
    }

    #[test]
    fn surround_is_cropped_to_the_front_pair() {
        let mut resampler = Resampler::new(SAMPLE_RATE, 6, ResamplingQuality::Low);
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
    ///
    /// The pump hands over whatever symphonia happened to decode, so chunk sizes are
    /// not ours to choose — hence the sweep over sizes down to a single frame, well
    /// past anything a real decoder produces. Get the cursor - dropped rebase
    /// wrong and frames are duplicated or skipped at every boundary; at a realistic
    /// packet size that is a click ten times a second.
    ///
    /// Two tolerances, both load-bearing:
    ///
    /// • Length, ±1 frame. The tail is held back two frames for the next call's
    ///   right-hand interpolation neighbours, and where that lands depends on where
    ///   the last boundary fell. The stream never gets those frames back either way.
    /// • Samples, 1e-4. One-shot, the cursor climbs to the length of the whole
    ///   input before its single rebase; chunked, it is rebased near zero every
    ///   call, so cursor += step accumulates rounding differently. That is a
    ///   ~1e-11 effect. A frame slip is a ~1e-1 effect, which is what this catches.
    #[test]
    fn chunking_does_not_change_the_result() {
        for (rate, channels) in [(44_100, 2), (44_100, 1), (22_050, 2), (96_000, 2)] {
            let whole = resample_sine(rate, channels, 0.2, 1 << 20);

            for chunk in [1, 2, 3, 5, 64, 512] {
                let split = resample_sine(rate, channels, 0.2, chunk);
                let case = format!("{rate}Hz/{channels}ch in {chunk}-frame chunks");

                assert!(
                    (whole.len() as i64 - split.len() as i64).abs() <= CHANNELS as i64,
                    "{case}: {} samples against {} one-shot — more than the held-back \
                     tail can explain, so a frame was dropped or duplicated",
                    split.len(),
                    whole.len()
                );

                let compare = whole.len().min(split.len());
                for (index, (a, b)) in whole[..compare].iter().zip(&split[..compare]).enumerate() {
                    assert!(
                        (a - b).abs() < 1e-4,
                        "{case}: sample {index} is {b} against {a} one-shot — far past \
                         cursor rounding, so the samples are misaligned"
                    );
                }
            }
        }
    }

    #[test]
    fn a_reset_clears_carried_state() {
        let mut resampler = Resampler::new(44_100, 2, ResamplingQuality::Low);
        resampler.process(&vec![1.0; 4096]);
        resampler.reset();

        // After a reset the first output frame follows the new input, not the old
        // buffer's tail.
        let out = resampler.process(&vec![0.0; 4096]);
        assert!(out.iter().all(|sample| sample.abs() < 1e-6));
    }

    #[test]
    fn silence_stays_silent() {
        let mut resampler = Resampler::new(44_100, 2, ResamplingQuality::Low);
        let out = resampler.process(&vec![0.0; 8192]);
        assert!(out.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn an_empty_buffer_produces_nothing() {
        let mut resampler = Resampler::new(44_100, 2, ResamplingQuality::Low);
        assert!(resampler.process(&[]).is_empty());
    }

    /// Low, and any source already at 48kHz, must never construct a
    /// SincEngine — it's meant to be the zero-cost tier, not merely
    /// unused-until-called.
    #[test]
    fn low_and_passthrough_never_construct_a_sinc_engine() {
        assert!(Resampler::new(44_100, CHANNELS, ResamplingQuality::Low).sinc.is_none());
        assert!(Resampler::new(SAMPLE_RATE, CHANNELS, ResamplingQuality::High).sinc.is_none());
        assert!(Resampler::new(SAMPLE_RATE, 1, ResamplingQuality::High).sinc.is_none());
    }

    /// The inverse: real rate conversion at Medium/High must actually route
    /// through SincEngine, not silently fall back to Catmull-Rom.
    #[test]
    fn medium_and_high_construct_a_sinc_engine_when_converting() {
        assert!(Resampler::new(44_100, CHANNELS, ResamplingQuality::Medium).sinc.is_some());
        assert!(Resampler::new(44_100, CHANNELS, ResamplingQuality::High).sinc.is_some());
    }

    #[test]
    fn sinc_passthrough_short_circuits_regardless_of_quality() {
        for quality in [ResamplingQuality::Medium, ResamplingQuality::High] {
            let mut resampler = Resampler::new(SAMPLE_RATE, CHANNELS, quality);
            assert!(resampler.is_passthrough());
            assert_eq!(resampler.process(&[0.5, -0.5]), vec![0.5, -0.5]);
        }
    }

    #[test]
    fn sinc_mono_is_duplicated_to_both_channels() {
        for quality in [ResamplingQuality::Medium, ResamplingQuality::High] {
            let mut resampler = Resampler::new(SAMPLE_RATE, 1, quality);
            // No rate conversion at SAMPLE_RATE, so this exercises
            // fill_stereo_frames only — SincEngine is never built here (see
            // low_and_passthrough_never_construct_a_sinc_engine).
            assert_eq!(resampler.process(&[0.25, -0.75]), vec![0.25, 0.25, -0.75, -0.75]);
        }
    }

    #[test]
    fn sinc_output_length_tracks_the_rate_ratio() {
        for quality in [ResamplingQuality::Medium, ResamplingQuality::High] {
            let out = resample_sine_quality(44_100, 2, 2.0, 1024, quality);
            let frames = out.len() / CHANNELS;
            let input_frames = 44_100usize * 2;
            let expected =
                (f64::from(SAMPLE_RATE) / 44_100.0 * input_frames as f64).ceil() as usize;
            assert_eq!(frames, expected, "{quality:?}");
        }
    }

    #[test]
    fn sinc_finish_emits_a_short_final_chunk() {
        for quality in [ResamplingQuality::Medium, ResamplingQuality::High] {
            let mut resampler = Resampler::new(44_100, CHANNELS, quality);
            assert!(resampler.process(&[0.5, -0.5]).is_empty());

            let mut tail = Vec::new();
            resampler.finish_into(&mut tail);
            assert_eq!(tail.len() / CHANNELS, 2, "{quality:?}");
        }
    }

    #[test]
    fn sinc_upsampling_preserves_the_waveform() {
        for quality in [ResamplingQuality::Medium, ResamplingQuality::High] {
            let out = resample_sine_quality(44_100, 2, 0.5, 1024, quality);
            assert!(!out.is_empty());
            assert!(out.iter().all(|sample| sample.is_finite()));

            let peak = out.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
            assert!((0.8..=1.2).contains(&peak), "{quality:?}: peak was {peak}");
        }
    }

    #[test]
    fn sinc_downsampling_works_too() {
        for quality in [ResamplingQuality::Medium, ResamplingQuality::High] {
            let out = resample_sine_quality(96_000, 2, 0.5, 4096, quality);
            let frames = out.len() / CHANNELS;
            assert_eq!(frames, SAMPLE_RATE as usize / 2, "{quality:?}");
        }
    }

    /// Unlike the Catmull-Rom path (which needs a ±1 frame / 1e-4 tolerance,
    /// see chunking_does_not_change_the_result), SincEngine accumulates raw
    /// stereo frames across calls with no per-call boundary effects at all —
    /// rubato only ever sees fixed, buffer-call-independent chunks — so
    /// chunking must reproduce the exact same output, including the final partial
    /// chunk emitted by finish_into.
    #[test]
    fn sinc_chunking_does_not_change_the_result() {
        for quality in [ResamplingQuality::Medium, ResamplingQuality::High] {
            for (rate, channels) in [(44_100, 2), (44_100, 1), (22_050, 2), (96_000, 2)] {
                let whole = resample_sine_quality(rate, channels, 0.2, 1 << 20, quality);

                for chunk in [1, 2, 3, 5, 64, 512] {
                    let split = resample_sine_quality(rate, channels, 0.2, chunk, quality);
                    let case = format!("{quality:?} {rate}Hz/{channels}ch in {chunk}-frame chunks");

                    let compare = whole.len().min(split.len());
                    assert_eq!(
                        &whole[..compare],
                        &split[..compare],
                        "{case}: chunking changed already-resampled output"
                    );
                }
            }
        }
    }

    #[test]
    fn sinc_reset_leaves_nothing_of_the_previous_position_behind() {
        for quality in [ResamplingQuality::Medium, ResamplingQuality::High] {
            let input = ramp(2_000);

            let fresh = Resampler::new(44_100, CHANNELS, quality).process(&input);

            let mut resampler = Resampler::new(44_100, CHANNELS, quality);
            resampler.process(&ramp(3_000));
            resampler.reset();
            let after_reset = resampler.process(&input);

            assert_eq!(
                after_reset, fresh,
                "{quality:?}: a reset resampler must behave exactly like a new one"
            );
        }
    }

    #[test]
    fn sinc_silence_stays_silent() {
        for quality in [ResamplingQuality::Medium, ResamplingQuality::High] {
            let mut resampler = Resampler::new(44_100, 2, quality);
            let out = resampler.process(&vec![0.0; 8192]);
            assert!(out.iter().all(|sample| *sample == 0.0), "{quality:?}");
        }
    }

    #[test]
    fn catmull_rom_passes_through_its_control_points() {
        assert!(
            (catmull_rom(0.0, 1.0, 2.0, 3.0, &catmull_rom_weights(0.0)) - 1.0).abs() < 1e-6
        );
        assert!(
            (catmull_rom(0.0, 1.0, 2.0, 3.0, &catmull_rom_weights(1.0)) - 2.0).abs() < 1e-6
        );
    }

    #[test]
    fn catmull_rom_is_linear_on_a_straight_line() {
        // Interpolating a ramp must give the ramp back.
        assert!(
            (catmull_rom(0.0, 1.0, 2.0, 3.0, &catmull_rom_weights(0.5)) - 1.5).abs() < 1e-6
        );
    }

    #[test]
    fn shared_weights_match_the_direct_polynomial() {
        let [p0, p1, p2, p3] = [-0.75, 0.25, 0.9, -0.4];
        for t in [0.0, 0.1, 0.5, 0.9, 1.0] {
            let t2 = t * t;
            let t3 = t2 * t;
            let direct = 0.5 * ((2.0 * p1)
                + (-p0 + p2) * t
                + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * t2
                + (-p0 + 3.0 * p1 - 3.0 * p2 + p3) * t3);
            let shared = catmull_rom(p0, p1, p2, p3, &catmull_rom_weights(t));
            assert!((shared - direct).abs() < 1e-6, "t={t}: {shared} != {direct}");
        }
    }

    /// The clamp-inside-the-tap-read form the interpolation loop used to have.
    ///
    /// Kept verbatim as a reference because hoisting that clamp out is a claim about
    /// exact output — the same four samples combined the same way, only reached
    /// differently — and the tests above are all property checks (peak, length,
    /// finiteness) that a small arithmetic drift would slip straight past.
    fn clamped_reference(source_rate: u32, source_channels: usize, input: &[f32]) -> Vec<f32> {
        let source: Vec<[f32; CHANNELS]> = input
            .chunks_exact(source_channels)
            .map(|frame| match source_channels {
                1 => [frame[0], frame[0]],
                _ => [frame[0], frame[1]],
            })
            .collect();

        let step = f64::from(source_rate) / f64::from(SAMPLE_RATE);
        let usable = source.len().saturating_sub(2) as f64;
        let mut cursor = 0.0f64;
        let mut out = Vec::new();

        while cursor < usable {
            let index = cursor.floor() as usize;
            let t = (cursor - cursor.floor()) as f32;
            for channel in 0..CHANNELS {
                let at = |offset: isize| -> f32 {
                    let i = index as isize + offset;
                    let i = i.clamp(0, source.len() as isize - 1) as usize;
                    source[i][channel]
                };
                out.push(catmull_rom(
                    at(-1),
                    at(0),
                    at(1),
                    at(2),
                    &catmull_rom_weights(t),
                ));
            }
            cursor += step;
        }
        out
    }

    #[test]
    fn hoisting_the_clamp_changes_no_sample() {
        // Each case is one buffer from a cold start, which is the only time the
        // index == 0 branch — the sole case the clamp ever actually bound — runs.
        for (rate, channels) in [(44_100, 2), (22_050, 2), (44_100, 1), (96_000, 2)] {
            let input: Vec<f32> = (0..4096 * channels)
                .map(|i| ((i % 997) as f32 / 997.0) * 2.0 - 1.0)
                .collect();
            let got = Resampler::new(rate, channels, ResamplingQuality::Low).process(&input);
            assert_eq!(
                got,
                clamped_reference(rate, channels, &input),
                "{rate}Hz {channels}ch"
            );
        }
    }
}
