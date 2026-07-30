//! The DSP chain.
//!
//! Nine of the original's ten filters, in its relative order. Order is part of the
//! sound, so it comes from [`lavalink_protocol::filters::FILTER_ORDER`] rather than
//! from the order fields happen to appear in a request.
//!
//! Everything here operates on planar `f32` samples, one slice per channel, which is
//! what the decode stage produces and what the send stage wants.
//!
//! # Fidelity
//!
//! "Implemented the same filter" is not the same as "sounds the same". These are
//! ports of specific implementations — lavaplayer's `Equalizer` and `PcmVolumeProcessor`,
//! and lavadsp's converters — where the coefficients and the shape of the update loop
//! matter as much as the algorithm does. Each filter below names the upstream file it
//! came from, and the arithmetic follows it including where that arithmetic is odd:
//! tremolo's depth is halved on the way in, the equalizer attenuates by 0.25 and
//! restores by 4.0, player volume quantises through an integer multiplier.
//!
//! # `timescale` is not implemented
//!
//! It is the one filter with no port here, and the reason is that lavadsp does not
//! implement it either: `TimescalePcmAudioFilter` is a JNI wrapper around
//! **SoundTouch**, a large C++ WSOLA time-stretcher. Independent speed, pitch and
//! rate need real time-domain stretching with period detection — an approximation
//! would be audibly wrong in a way that is easy to ship and hard to notice, which is
//! the failure this module's history already has one example of.
//!
//! So it is refused rather than faked: `timescale` is absent from
//! [`IMPLEMENTED_FILTERS`], `/v4/info` does not advertise it, and a request naming it
//! gets the original's 400.
//!
//! # `sin`/`cos` per sample, left alone
//!
//! Tremolo, vibrato and rotation each call `sin`/`cos` once per sample rather than
//! advancing the LFO with an angle-addition recurrence
//! (`sin(a+b) = sin(a)cos(b) + cos(a)sin(b)`, which turns a transcendental call into
//! a couple of multiplies). That trade was measured and rejected: an `f32`
//! recurrence loses magnitude at roughly 3e-3 per second and compounds over a long
//! track, and `rotation_pans_across_the_stereo_image` (below) asserts
//! `left_max > 0.99 && left_min < 0.01` across a full 48 000-sample revolution —
//! exactly the invariant drift breaks first. Renormalising the recurrence each
//! sample would fix that at the cost of a `sqrt`, which gives back most of what the
//! recurrence was for. Against a filter chain that already costs well under 1% of
//! one core per player (see `crates/server/benches/filter.rs`), this is the worst
//! risk-per-microsecond change available here, so it stays as `sin`/`cos`.

use lavalink_protocol::filters::{
    Band, ChannelMix, Distortion, Filters, Karaoke, LowPass, Rotation, Tremolo, Vibrato,
};
use lavalink_protocol::Omissible;

/// What `/v4/info` advertises and what `PATCH` accepts.
///
/// Nine of the original's ten. `timescale` is the omission — see the module docs.
pub const IMPLEMENTED_FILTERS: [&str; 9] = [
    "volume",
    "equalizer",
    "karaoke",
    "tremolo",
    "vibrato",
    "distortion",
    "rotation",
    "channelMix",
    "lowPass",
];

pub const EQUALIZER_BAND_COUNT: usize = 15;

/// One band's biquad coefficients.
#[derive(Debug, Clone, Copy)]
struct Coefficients {
    beta: f32,
    alpha: f32,
    gamma: f32,
}

/// The sample rate everything downstream of the resampler runs at.
///
/// The equalizer's coefficients are derived for exactly this rate — lavaplayer
/// refuses to build one at any other (`Equalizer.isCompatible`) — and the LFO filters
/// need it to turn a frequency in Hz into a per-sample phase step.
const SAMPLE_RATE: f32 = super::ring::SAMPLE_RATE as f32;

/// lavaplayer `Equalizer.coefficients48000`, diffed against
/// `lavaplayer/main/src/main/java/…/filter/equalizer/Equalizer.java`.
// Kept at the precision upstream prints them at, even though `f32` cannot hold all
// of it. Truncating to what `f32` represents would make a future diff against
// lavaplayer's source harder to read for no numerical gain.
#[allow(clippy::excessive_precision)]
const COEFFICIENTS_48000: [Coefficients; EQUALIZER_BAND_COUNT] = [
    Coefficients { beta: 9.9847546664e-01, alpha: 7.6226668143e-04, gamma: 1.9984647656e+00 },
    Coefficients { beta: 9.9756184654e-01, alpha: 1.2190767289e-03, gamma: 1.9975344645e+00 },
    Coefficients { beta: 9.9616261379e-01, alpha: 1.9186931041e-03, gamma: 1.9960947369e+00 },
    Coefficients { beta: 9.9391578543e-01, alpha: 3.0421072865e-03, gamma: 1.9937449618e+00 },
    Coefficients { beta: 9.9028307215e-01, alpha: 4.8584639242e-03, gamma: 1.9898465702e+00 },
    Coefficients { beta: 9.8485897264e-01, alpha: 7.5705136795e-03, gamma: 1.9837962543e+00 },
    Coefficients { beta: 9.7588512657e-01, alpha: 1.2057436715e-02, gamma: 1.9731772447e+00 },
    Coefficients { beta: 9.6228521814e-01, alpha: 1.8857390928e-02, gamma: 1.9556164694e+00 },
    Coefficients { beta: 9.4080933132e-01, alpha: 2.9595334338e-02, gamma: 1.9242054384e+00 },
    Coefficients { beta: 9.0702059196e-01, alpha: 4.6489704022e-02, gamma: 1.8653476166e+00 },
    Coefficients { beta: 8.5868004289e-01, alpha: 7.0659978553e-02, gamma: 1.7600401337e+00 },
    Coefficients { beta: 7.8409610788e-01, alpha: 1.0795194606e-01, gamma: 1.5450725522e+00 },
    Coefficients { beta: 6.8332861002e-01, alpha: 1.5833569499e-01, gamma: 1.1426447155e+00 },
    Coefficients { beta: 5.5267518228e-01, alpha: 2.2366240886e-01, gamma: 4.0186190803e-01 },
    Coefficients { beta: 4.1811888447e-01, alpha: 2.9094055777e-01, gamma: -7.0905944223e-01 },
];

/// One stage of the chain.
trait AudioFilter: std::fmt::Debug {
    /// Whether this filter, as configured, would actually change the audio — volume
    /// 1.0, an all-zero equalizer and lowPass smoothing <= 1.0 are all no-ops.
    fn is_enabled(&self) -> bool;

    fn process(&mut self, channels: &mut [Vec<f32>]);
}

/// The chain built from a `Filters` request.
///
/// Constructed once per filter change and then driven per buffer; the per-filter
/// history lives inside, so replacing the chain resets it — same as the original,
/// which builds a fresh `FilterChain` on every `PATCH`.
#[derive(Debug)]
pub struct FilterChain {
    /// In the original's application order, which is part of how the audio sounds.
    stages: Vec<Box<dyn AudioFilter>>,
}

impl FilterChain {
    /// Builds the chain, ignoring filters we do not implement — a request naming one
    /// is rejected with 400 before it gets here, so reaching this with an
    /// unimplemented filter set means the rejection was skipped.
    pub fn new(filters: &Filters, channels: usize) -> Self {
        let mut stages: Vec<Box<dyn AudioFilter>> = Vec::new();

        if let Omissible::Present(volume) = filters.volume {
            stages.push(Box::new(VolumeFilter { volume }));
        }
        if let Omissible::Present(bands) = &filters.equalizer {
            stages.push(Box::new(Equalizer::new(bands, channels)));
        }
        if let Some(karaoke) = present(&filters.karaoke) {
            stages.push(Box::new(KaraokeFilter::new(karaoke)));
        }
        // `timescale` belongs here in the order. It is not built — see the module
        // docs — and a request naming it is rejected with 400 before this point.
        if let Some(tremolo) = present(&filters.tremolo) {
            stages.push(Box::new(TremoloFilter::new(tremolo, channels)));
        }
        if let Some(vibrato) = present(&filters.vibrato) {
            stages.push(Box::new(VibratoFilter::new(vibrato, channels)));
        }
        if let Some(config) = present(&filters.distortion) {
            stages.push(Box::new(DistortionFilter { config }));
        }
        if let Some(rotation) = present(&filters.rotation) {
            stages.push(Box::new(RotationFilter::new(rotation)));
        }
        if let Some(config) = present(&filters.channel_mix) {
            stages.push(Box::new(ChannelMixFilter { config }));
        }
        if let Some(low_pass) = present(&filters.low_pass) {
            stages.push(Box::new(LowPassFilter::new(low_pass, channels)));
        }

        Self { stages }
    }

    pub fn empty(channels: usize) -> Self {
        Self::new(&Filters::default(), channels)
    }

    /// Whether any filter would actually change the audio.
    ///
    /// The original uses this to skip building a pipeline at all
    /// (`FilterChain.kt:93`).
    pub fn is_enabled(&self) -> bool {
        self.stages.iter().any(|stage| stage.is_enabled())
    }

    /// Applies every enabled filter in place, in order.
    pub fn process(&mut self, channels: &mut [Vec<f32>]) {
        for stage in &mut self.stages {
            if stage.is_enabled() {
                stage.process(channels);
            }
        }
    }
}

/// A filter that is both present and non-null.
///
/// `Present(None)` is how a client clears one filter without touching the rest, so
/// it has to mean "no stage" rather than "a stage with default settings" — the
/// defaults for several of these are audible.
fn present<T: Copy>(field: &Omissible<Option<T>>) -> Option<T> {
    match field {
        Omissible::Present(Some(value)) => Some(*value),
        _ => None,
    }
}

#[derive(Debug)]
struct VolumeFilter {
    volume: f32,
}

impl AudioFilter for VolumeFilter {
    /// `VolumeConfig.isEnabled` is `volume != 1.0f`, so the chain includes this
    /// stage for anything else — even a value [`process`] then treats as unity.
    fn is_enabled(&self) -> bool {
        self.volume != 1.0
    }

    /// lavadsp `VolumePcmAudioFilter` + `VectorSupport.volume`.
    ///
    /// Not a multiply: the curve is `tan(v * 0.79)` up to 1.5 and linear above,
    /// scaled through an integer-shaped `/ 10000`, and the result is clamped. The
    /// dead zone is upstream's too — within 0.02 of unity the samples are passed
    /// through untouched rather than run through a curve that does not return
    /// exactly 1.0 at 1.0.
    fn process(&mut self, channels: &mut [Vec<f32>]) {
        if (1.0 - self.volume).abs() < 0.02 {
            return;
        }

        let multiplier = if self.volume <= 1.5 {
            (self.volume * 0.79).tan() * 10000.0
        } else {
            24612.0 * (self.volume * 100.0) / 150.0
        };

        for channel in channels {
            for sample in channel.iter_mut() {
                *sample = (*sample * multiplier / 10000.0).clamp(-1.0, 1.0);
            }
        }
    }
}

/// The player's own `volume` field (0..=1000), which is separate from the `volume`
/// *filter* and is applied by the engine rather than by the chain.
///
/// lavaplayer `PcmVolumeProcessor.setupMultipliers`, which is a tangent curve below
/// 150 and linear above it. The integer truncation is upstream's — it works on
/// `short` samples with an integer multiplier — and is reproduced because the
/// quantisation is what the numbers actually are, not an artefact of the port.
///
/// 100 is unity by a short-circuit rather than by the curve, which returns 1.0099
/// there (`applyCurrentVolume` returns early when the volume is 100).
pub fn player_volume_multiplier(volume: i32) -> f32 {
    let volume = volume.clamp(0, 1000);
    if volume == 100 {
        return 1.0;
    }

    let integer_multiplier = if volume <= 150 {
        ((volume as f32 * 0.0079).tan() * 10000.0) as i32
    } else {
        24621 * volume / 150
    };

    integer_multiplier as f32 / 10000.0
}

#[derive(Debug)]
struct Equalizer {
    /// Indexed by band; 0.0 means the band is flat.
    gains: [f32; EQUALIZER_BAND_COUNT],
    /// Indices into `gains`/`COEFFICIENTS_48000` with a non-zero gain, computed
    /// once at construction. `process` walks only these: a zero-gain band's
    /// biquad output is multiplied by `0.0` regardless of its internal state, and
    /// gains never change after construction (the chain is rebuilt wholesale on
    /// any filter change — `pump.rs`'s `apply_filters`), so skipping a flat band
    /// entirely is bit-exact, not an approximation.
    active: Vec<usize>,
    /// Per channel, per band: `[x0, x1, x2, y0, y1, y2]`.
    history: Vec<[[f32; 6]; EQUALIZER_BAND_COUNT]>,
}

impl Equalizer {
    fn new(bands: &[Band], channels: usize) -> Self {
        let mut gains = [0.0; EQUALIZER_BAND_COUNT];
        for band in bands {
            // Gains are *not* clamped. `EqualizerConfiguration.setGain` clamps to
            // [-0.25, 1.0], but Lavalink never calls it: `EqualizerConfig` writes
            // `array[it.band] = it.gain` and hands the array to the constructor,
            // which stores it as-is. Clamping here would make loud settings quieter
            // than the original's.
            //
            // Out-of-range indices are dropped. The original throws
            // `ArrayIndexOutOfBoundsException`, which is one of the few places a
            // crash is not worth reproducing.
            if let Some(slot) = usize::try_from(band.band)
                .ok()
                .and_then(|index| gains.get_mut(index))
            {
                *slot = band.gain;
            }
        }

        let active = (0..EQUALIZER_BAND_COUNT)
            .filter(|&band| gains[band] != 0.0)
            .collect();

        Self {
            gains,
            active,
            history: vec![[[0.0; 6]; EQUALIZER_BAND_COUNT]; channels],
        }
    }
}

impl AudioFilter for Equalizer {
    fn is_enabled(&self) -> bool {
        !self.active.is_empty()
    }

    fn process(&mut self, channels: &mut [Vec<f32>]) {
        const X0: usize = 0;
        const X1: usize = 1;
        const X2: usize = 2;
        const Y0: usize = 3;
        const Y1: usize = 4;
        const Y2: usize = 5;

        for (channel_index, channel) in channels.iter_mut().enumerate() {
            let Some(history) = self.history.get_mut(channel_index) else {
                // More channels than the chain was built for: leave them untouched
                // rather than index out of bounds. The chain is rebuilt on format
                // change, so this is a transient.
                continue;
            };

            for sample in channel.iter_mut() {
                let current = *sample;
                // The dry signal is attenuated so that summing 15 wet bands on top
                // does not clip; lavaplayer uses the same 0.25 factor.
                let mut result = current * 0.25;

                for &band in &self.active {
                    let coefficients = &COEFFICIENTS_48000[band];
                    let state = &mut history[band];

                    state[X0] = current;
                    state[Y0] = coefficients.alpha * (state[X0] - state[X2])
                        + coefficients.gamma * state[Y1]
                        - coefficients.beta * state[Y2];

                    result += state[Y0] * self.gains[band];

                    state[X2] = state[X1];
                    state[X1] = state[X0];
                    state[Y2] = state[Y1];
                    state[Y1] = state[Y0];
                }

                // The 0.25 above and this 4.0 undo each other for a flat equalizer;
                // the attenuation exists so that summing wet bands cannot overflow
                // mid-loop, not to make the output quieter. Dropping the restore
                // costs 12 dB, which is the difference between "the equalizer is
                // subtle" and "this node is quiet".
                *sample = (result * 4.0).clamp(-1.0, 1.0);
            }
        }
    }
}

/// lavadsp `LowPassPcmAudioFilter`: a one-pole smoother.
#[derive(Debug)]
struct LowPassFilter {
    smoothing: f32,
    previous: Vec<f32>,
}

impl LowPassFilter {
    fn new(config: LowPass, channels: usize) -> Self {
        Self {
            smoothing: config.smoothing,
            previous: vec![0.0; channels],
        }
    }
}

impl AudioFilter for LowPassFilter {
    fn is_enabled(&self) -> bool {
        self.smoothing > 1.0
    }

    fn process(&mut self, channels: &mut [Vec<f32>]) {
        for (channel_index, channel) in channels.iter_mut().enumerate() {
            let Some(previous) = self.previous.get_mut(channel_index) else {
                continue;
            };
            for sample in channel.iter_mut() {
                *previous += (*sample - *previous) / self.smoothing;
                *sample = *previous;
            }
        }
    }
}

/// lavadsp `KaraokeConverter`.
///
/// Stereo only: the original builds no converter for other channel counts and passes
/// the audio through, so a mono track is unaffected rather than silenced.
#[derive(Debug)]
struct KaraokeFilter {
    level: f32,
    mono_level: f32,
    /// Bandpass coefficients, derived once from `filterBand`/`filterWidth`.
    a: f32,
    b: f32,
    c: f32,
    y1: f32,
    y2: f32,
}

impl KaraokeFilter {
    fn new(config: Karaoke) -> Self {
        // Neither the protocol nor `Filters::validate` bounds `filterWidth`, and
        // unlike `vibrato`/`tremolo`'s LFOs this coefficient is fed back through
        // `y1`/`y2` forever rather than recomputed per sample: an unclamped
        // `filterWidth` around 8e5 or beyond makes `exp` saturate to `0.0` or
        // `+inf`, which turns the division below into `0.0/0.0` — a `NaN` that
        // then never leaves the filter chain's state until the client sends a new
        // `filters` patch. `c` is also this recurrence's pole radius: even a
        // *finite* but `c >= 1` (any negative `filterWidth`) makes the feedback
        // loop diverge to infinity within a few dozen samples instead — clamping
        // strictly below 1 is what actually keeps it stable, not just finite.
        // Every realistic `filterWidth` (Nyquist and far past it) already lands
        // well under this bound, so this is bit-identical there.
        let c = (-2.0 * std::f32::consts::PI * config.filter_width / SAMPLE_RATE)
            .exp()
            .clamp(1e-6, 0.999);
        let b = -4.0 * c / (1.0 + c)
            * (2.0 * std::f32::consts::PI * config.filter_band / SAMPLE_RATE).cos();
        let a = (1.0 - b * b / (4.0 * c)).sqrt() * (1.0 - c);

        Self {
            level: config.level,
            mono_level: config.mono_level,
            a,
            b,
            c,
            y1: 0.0,
            y2: 0.0,
        }
    }
}

impl AudioFilter for KaraokeFilter {
    /// `KaraokeConfig.isEnabled` is unconditionally true — asking for karaoke at all
    /// is the switch, and there is no neutral setting.
    fn is_enabled(&self) -> bool {
        true
    }

    fn process(&mut self, channels: &mut [Vec<f32>]) {
        let [left, right] = channels else {
            return;
        };

        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let (dry_left, dry_right) = (*l, *r);

            let y = (self.a * ((dry_left + dry_right) / 2.0) - self.b * self.y1) - self.c * self.y2;
            self.y2 = self.y1;
            self.y1 = y;

            // The centre channel, extracted by the bandpass, added back to both
            // sides after the sides have been subtracted from each other.
            let o = y * self.mono_level * self.level;
            *l = dry_left - (dry_right * self.level) + o;
            *r = dry_right - (dry_left * self.level) + o;
        }
    }
}

/// lavadsp `TremoloPcmAudioFilter` + `VectorSupport.tremolo`: amplitude modulation.
#[derive(Debug)]
struct TremoloFilter {
    frequency: f32,
    /// **Half** the requested depth. `setDepth` stores `depth / 2`, and Lavalink
    /// passes the protocol value straight in, so the halving is part of the wire
    /// meaning rather than an implementation detail.
    depth: f32,
    /// Per channel, so the two sides stay in phase with each other across buffers.
    phases: Vec<f32>,
}

impl TremoloFilter {
    fn new(config: Tremolo, channels: usize) -> Self {
        Self {
            frequency: config.frequency,
            depth: config.depth / 2.0,
            phases: vec![0.0; channels],
        }
    }
}

impl AudioFilter for TremoloFilter {
    fn is_enabled(&self) -> bool {
        self.depth != 0.0
    }

    fn process(&mut self, channels: &mut [Vec<f32>]) {
        for (channel_index, channel) in channels.iter_mut().enumerate() {
            let Some(phase) = self.phases.get_mut(channel_index) else {
                continue;
            };

            let offset = 1.0 - self.depth;
            for sample in channel.iter_mut() {
                *sample *= offset + self.depth * phase.sin();
                // lavadsp's `VectorSupport.tremolo` accumulates this phase in an
                // unwrapped `float` and never resets it, so on a real node the LFO's
                // rate quantises away after a couple of minutes and eventually
                // freezes as the increment falls below the f32 ULP at that
                // magnitude. That is reproduced faithfully by lavadsp's own
                // upstream, lavaplayer, but not by us: unlike volume curves or
                // status codes, LFO phase is not something a client observes or
                // branches on, so there is no wire fidelity to preserve here, only
                // an inherited defect. `rem_euclid` keeps the same pattern used for
                // vibrato below, and rotation's `f64` phase already sidesteps the
                // same class of problem.
                *phase = (*phase + 2.0 * std::f32::consts::PI / SAMPLE_RATE * self.frequency)
                    .rem_euclid(2.0 * std::f32::consts::PI);
            }
        }
    }
}

/// lavadsp `VibratoConverter`: frequency modulation via a modulated delay line.
///
/// The delay is read with a 4-point Hermite interpolator, which is what makes the
/// pitch bend smooth rather than stepped — a nearest-sample read would zipper.
#[derive(Debug)]
struct VibratoFilter {
    frequency: f32,
    depth: f32,
    channels: Vec<VibratoChannel>,
}

/// 2 ms of delay, upstream's `BASE_DELAY_SEC`.
const VIBRATO_BASE_DELAY_SEC: f32 = 0.002;
/// Keeps the read index off the write index even at zero modulation.
const VIBRATO_ADDITIONAL_DELAY: f32 = 3.0;
/// The interpolator reads four consecutive samples, so the first three are mirrored
/// past the end of the ring and the buffer is allocated that much longer.
const VIBRATO_INTERPOLATOR_MARGIN: usize = 3;

#[derive(Debug)]
struct VibratoChannel {
    buffer: Vec<f32>,
    size: usize,
    write_index: usize,
    phase: f32,
}

impl VibratoChannel {
    fn new() -> Self {
        let size = (VIBRATO_BASE_DELAY_SEC * SAMPLE_RATE * 2.0) as usize;
        Self {
            buffer: vec![0.0; size + VIBRATO_INTERPOLATOR_MARGIN],
            size,
            write_index: 0,
            phase: 0.0,
        }
    }

    /// Writes one sample, mirroring the first few past the end so the interpolator
    /// can read four in a row without wrapping mid-read.
    fn write(&mut self, sample: f32) {
        self.buffer[self.write_index] = sample;
        if self.write_index < VIBRATO_INTERPOLATOR_MARGIN {
            self.buffer[self.size + self.write_index] = sample;
        }
        self.write_index += 1;
        if self.write_index == self.size {
            self.write_index = 0;
        }
    }

    fn read_hermite(&self, delay: f32) -> f32 {
        // `rem_euclid` wraps in one step regardless of how far out of range `delay`
        // is (an unclamped client-supplied depth can push it arbitrarily far), where
        // the previous increment/decrement loop could spin for a very long time.
        let read_index = (self.write_index as f32 - 1.0 - delay).rem_euclid(self.size as f32);

        // `rem_euclid` is mathematically confined to `[0, size)`, but floating-point
        // rounding can round its result up to exactly `size` (seen when `delay` is
        // within an epsilon of a whole number of samples behind the write index),
        // which would then read `size + 3` — one past the mirrored margin at the end
        // of `buffer`. Clamping the index rather than the float keeps this exact for
        // every other value; the one sample it can shift by is inaudible.
        let offset = (read_index as usize).min(self.size - 1);
        let x = read_index - offset as f32;

        let (y0, y1, y2, y3) = (
            self.buffer[offset],
            self.buffer[offset + 1],
            self.buffer[offset + 2],
            self.buffer[offset + 3],
        );

        let c1 = 0.5 * (y2 - y0);
        let c2 = (y0 - 2.5 * y1) + (2.0 * y2 - 0.5 * y3);
        let c3 = 0.5 * (y3 - y0) + 1.5 * (y1 - y2);
        ((c3 * x + c2) * x + c1) * x + y1
    }

    /// Upstream's LFO is unipolar — `(sin + 1) * 0.5` — so the delay only ever
    /// lengthens. A bipolar one would centre the pitch instead of bending it up.
    fn next_lfo(&mut self, frequency: f32) -> f32 {
        let value = (self.phase.sin() + 1.0) * 0.5;
        // `rem_euclid`, not a decrement loop, so an extreme client-supplied
        // frequency can't spin this for a very long time.
        self.phase = (self.phase + 2.0 * std::f32::consts::PI * frequency / SAMPLE_RATE)
            .rem_euclid(2.0 * std::f32::consts::PI);
        value
    }
}

impl VibratoFilter {
    fn new(config: Vibrato, channels: usize) -> Self {
        Self {
            frequency: config.frequency,
            depth: config.depth,
            channels: (0..channels).map(|_| VibratoChannel::new()).collect(),
        }
    }
}

impl AudioFilter for VibratoFilter {
    fn is_enabled(&self) -> bool {
        self.depth != 0.0
    }

    fn process(&mut self, channels: &mut [Vec<f32>]) {
        let max_delay = VIBRATO_BASE_DELAY_SEC * SAMPLE_RATE;

        for (channel_index, channel) in channels.iter_mut().enumerate() {
            let Some(state) = self.channels.get_mut(channel_index) else {
                continue;
            };

            for sample in channel.iter_mut() {
                let lfo = state.next_lfo(self.frequency);
                let delay = lfo * self.depth * max_delay + VIBRATO_ADDITIONAL_DELAY;
                let out = state.read_hermite(delay);
                state.write(*sample);
                *sample = out;
            }
        }
    }
}

/// lavadsp `DistortionConverter`.
///
/// All three trigonometric functions are always enabled: `DistortionPcmAudioFilter`
/// starts with `ALL_FUNCTIONS` and Lavalink's `DistortionConfig` never disables any,
/// so the `useSin`/`useCos`/`useTan` branches upstream are dead code in this context
/// and are not reproduced.
#[derive(Debug)]
struct DistortionFilter {
    config: Distortion,
}

impl AudioFilter for DistortionFilter {
    fn is_enabled(&self) -> bool {
        let c = &self.config;
        c.sin_offset != 0.0
            || c.sin_scale != 1.0
            || c.cos_offset != 0.0
            || c.cos_scale != 1.0
            || c.tan_offset != 0.0
            || c.tan_scale != 1.0
            || c.offset != 0.0
            || c.scale != 1.0
    }

    fn process(&mut self, channels: &mut [Vec<f32>]) {
        let c = &self.config;
        for channel in channels {
            for sample in channel.iter_mut() {
                let sin = c.sin_offset + (*sample * c.sin_scale).sin();
                let cos = c.cos_offset + (*sample * c.cos_scale).cos();
                let tan = c.tan_offset + (*sample * c.tan_scale).tan();
                *sample = (c.offset + c.scale * sin * cos * tan).clamp(-1.0, 1.0);
            }
        }
    }
}

/// lavadsp `RotationPcmAudioFilter` + `VectorSupport.rotation`: the "8D audio" pan.
#[derive(Debug)]
struct RotationFilter {
    /// Phase increment per sample. Zero when `rotationHz` is zero, which upstream
    /// special-cases to avoid a division that would otherwise be by infinity.
    step: f64,
    phase: f64,
    enabled: bool,
}

impl RotationFilter {
    fn new(config: Rotation) -> Self {
        let step = if config.rotation_hz == 0.0 {
            0.0
        } else {
            let samples_per_cycle =
                f64::from(SAMPLE_RATE) / (config.rotation_hz * 2.0 * std::f64::consts::PI);
            1.0 / samples_per_cycle
        };
        // An extreme client-supplied `rotationHz` overflows the multiplication
        // above to `f64::INFINITY`, and a non-finite step turns `phase.sin()`
        // into `NaN` forever after — the same class of defect `KaraokeFilter`
        // guards `filterWidth` against. Falling back to disabled rather than
        // reproducing the NaN sink: like tremolo's phase wrap, this is not
        // something a client observes on the wire, only ever a defect.
        let step = if step.is_finite() { step } else { 0.0 };

        Self {
            step,
            phase: 0.0,
            enabled: step != 0.0,
        }
    }
}

impl AudioFilter for RotationFilter {
    fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn process(&mut self, channels: &mut [Vec<f32>]) {
        let [left, right] = channels else {
            return;
        };

        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let sin = self.phase.sin() as f32;
            // The two sides are driven by opposite halves of the same sine, so the
            // image swings across rather than both ends fading together.
            *l = *l * (sin + 1.0) / 2.0;
            *r = *r * (-sin + 1.0) / 2.0;
            self.phase += self.step;
        }
    }
}

/// lavadsp `ChannelMixPcmAudioFilter` + `VectorSupport.channelMix`: a 2×2 matrix.
#[derive(Debug)]
struct ChannelMixFilter {
    config: ChannelMix,
}

impl AudioFilter for ChannelMixFilter {
    fn is_enabled(&self) -> bool {
        let c = &self.config;
        c.left_to_left != 1.0
            || c.left_to_right != 0.0
            || c.right_to_left != 0.0
            || c.right_to_right != 1.0
    }

    fn process(&mut self, channels: &mut [Vec<f32>]) {
        let [left, right] = channels else {
            return;
        };

        let c = &self.config;
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let (dry_left, dry_right) = (*l, *r);
            *l = (c.left_to_left * dry_left + c.right_to_left * dry_right).clamp(-1.0, 1.0);
            *r = (c.left_to_right * dry_left + c.right_to_right * dry_right).clamp(-1.0, 1.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filters(json: &str) -> Filters {
        serde_json::from_str(json).unwrap()
    }

    fn ramp(len: usize) -> Vec<Vec<f32>> {
        vec![(0..len).map(|i| (i as f32 / len as f32) - 0.5).collect()]
    }

    #[test]
    fn an_empty_chain_is_disabled() {
        assert!(!FilterChain::empty(2).is_enabled());
    }

    #[test]
    fn unit_volume_is_a_no_op() {
        let chain = FilterChain::new(&filters(r#"{"volume":1.0}"#), 2);
        assert!(!chain.is_enabled());
    }

    /// The volume filter is a tangent curve, not a multiply: at 0.5 the multiplier is
    /// `tan(0.5 * 0.79) * 10000 / 10000` ≈ 0.4169, not 0.5. Getting this wrong is
    /// inaudible in isolation and obvious next to the original.
    #[test]
    fn non_unit_volume_follows_the_original_curve() {
        let mut chain = FilterChain::new(&filters(r#"{"volume":0.5}"#), 1);
        assert!(chain.is_enabled());

        let mut channels = vec![vec![1.0, -1.0, 0.25]];
        chain.process(&mut channels);

        for (got, want) in channels[0].iter().zip([0.416_911_8, -0.416_911_8, 0.104_227_96]) {
            assert!((got - want).abs() < 1e-6, "got {got}, want {want}");
        }
    }

    /// Within 0.02 of unity the original passes the buffer through untouched rather
    /// than running it through a curve that does not return exactly 1.0 at 1.0.
    #[test]
    fn volume_near_unity_is_passed_through_untouched() {
        let mut chain = FilterChain::new(&filters(r#"{"volume":1.01}"#), 1);
        // The stage is still built — `VolumeConfig.isEnabled` is `volume != 1.0`.
        assert!(chain.is_enabled());

        let mut channels = vec![vec![0.5, -0.25]];
        chain.process(&mut channels);
        assert_eq!(channels[0], vec![0.5, -0.25]);
    }

    #[test]
    fn loud_volume_saturates_rather_than_wrapping() {
        let mut chain = FilterChain::new(&filters(r#"{"volume":2.0}"#), 1);
        let mut channels = vec![vec![1.0, -1.0]];
        chain.process(&mut channels);
        assert_eq!(channels[0], vec![1.0, -1.0]);
    }

    #[test]
    fn an_all_zero_equalizer_is_a_no_op() {
        let chain = FilterChain::new(&filters(r#"{"equalizer":[{"band":0,"gain":0.0}]}"#), 2);
        assert!(!chain.is_enabled());
    }

    #[test]
    fn a_nonzero_equalizer_is_enabled_and_changes_the_signal() {
        let mut chain = FilterChain::new(&filters(r#"{"equalizer":[{"band":0,"gain":1.0}]}"#), 1);
        assert!(chain.is_enabled());

        let original = ramp(256);
        let mut channels = original.clone();
        chain.process(&mut channels);
        assert_ne!(channels, original);
        assert!(channels[0].iter().all(|sample| sample.is_finite()));
    }

    /// Skipping zero-gain bands (`Equalizer::active`) must not change output:
    /// explicit zero entries for the other 13 bands are equivalent to leaving them
    /// absent, since a zero-gain band contributes `state[Y0] * 0.0` either way and
    /// its history never feeds back into a band that IS active. Same two nonzero
    /// bands, same input, byte-for-byte identical output is the bar.
    #[test]
    fn skipping_zero_gain_bands_does_not_change_the_output() {
        let sparse = filters(r#"{"equalizer":[{"band":2,"gain":0.6},{"band":9,"gain":-0.4}]}"#);
        let padded = filters(
            r#"{"equalizer":[
                {"band":0,"gain":0.0},{"band":1,"gain":0.0},{"band":2,"gain":0.6},
                {"band":3,"gain":0.0},{"band":4,"gain":0.0},{"band":5,"gain":0.0},
                {"band":6,"gain":0.0},{"band":7,"gain":0.0},{"band":8,"gain":0.0},
                {"band":9,"gain":-0.4},{"band":10,"gain":0.0},{"band":11,"gain":0.0},
                {"band":12,"gain":0.0},{"band":13,"gain":0.0},{"band":14,"gain":0.0}
            ]}"#,
        );

        let mut sparse_chain = FilterChain::new(&sparse, 1);
        let mut padded_chain = FilterChain::new(&padded, 1);

        let mut sparse_out = ramp(512);
        let mut padded_out = sparse_out.clone();
        sparse_chain.process(&mut sparse_out);
        padded_chain.process(&mut padded_out);

        assert_eq!(sparse_out, padded_out);
    }

    #[test]
    /// `EqualizerConfiguration.setGain` clamps to [-0.25, 1.0], but Lavalink never
    /// calls it — `EqualizerConfig` fills the array directly and passes it to the
    /// constructor. Clamping here would make a loud equalizer quieter than the
    /// original's, so the gain is stored as sent.
    fn equalizer_gains_are_stored_unclamped_like_the_original() {
        let equalizer = Equalizer::new(
            &[
                Band {
                    band: 0,
                    gain: 99.0,
                },
                Band {
                    band: 1,
                    gain: -99.0,
                },
            ],
            1,
        );
        assert_eq!(equalizer.gains[0], 99.0);
        assert_eq!(equalizer.gains[1], -99.0);
    }

    #[test]
    fn out_of_range_bands_are_ignored_not_fatal() {
        let equalizer = Equalizer::new(
            &[
                Band {
                    band: 99,
                    gain: 1.0,
                },
                Band {
                    band: -1,
                    gain: 1.0,
                },
            ],
            1,
        );
        assert!(!equalizer.is_enabled());
    }

    #[test]
    fn equalizer_keeps_history_across_buffers() {
        let make = || FilterChain::new(&filters(r#"{"equalizer":[{"band":7,"gain":0.5}]}"#), 1);

        let source = ramp(512);
        let mut whole = make();
        let mut channels = source.clone();
        whole.process(&mut channels);

        let mut split = make();
        let mut first = vec![source[0][..256].to_vec()];
        let mut second = vec![source[0][256..].to_vec()];
        split.process(&mut first);
        split.process(&mut second);

        // Processing in two calls must give the same result as one, or every buffer
        // boundary would be an audible discontinuity.
        let rejoined: Vec<f32> = first[0].iter().chain(second[0].iter()).copied().collect();
        for (a, b) in channels[0].iter().zip(rejoined.iter()) {
            assert!((a - b).abs() < 1e-6, "{a} != {b}");
        }
    }

    #[test]
    fn low_pass_below_the_threshold_is_a_no_op() {
        let chain = FilterChain::new(&filters(r#"{"lowPass":{"smoothing":1.0}}"#), 2);
        assert!(!chain.is_enabled());
    }

    #[test]
    fn low_pass_attenuates_a_fast_alternating_signal() {
        let mut chain = FilterChain::new(&filters(r#"{"lowPass":{"smoothing":20.0}}"#), 1);
        assert!(chain.is_enabled());

        let mut channels = vec![(0..256)
            .map(|i| if i % 2 == 0 { 1.0 } else { -1.0 })
            .collect::<Vec<f32>>()];
        chain.process(&mut channels);

        let peak = channels[0]
            .iter()
            .skip(64)
            .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
        assert!(peak < 0.2, "nyquist content should be crushed, peak was {peak}");
    }

    #[test]
    fn an_explicit_null_low_pass_disables_it() {
        let chain = FilterChain::new(&filters(r#"{"lowPass":null}"#), 2);
        assert!(!chain.is_enabled());
    }

    #[test]
    fn filters_apply_in_the_original_order() {
        // volume before lowPass: scaling then smoothing is not the same as smoothing
        // then scaling for the first sample, where the smoother's history is zero.
        let mut chain = FilterChain::new(
            &filters(r#"{"volume":0.5,"lowPass":{"smoothing":2.0}}"#),
            1,
        );
        let mut channels = vec![vec![1.0f32]];
        chain.process(&mut channels);
        // 1.0 through the volume curve is 0.4169118, then smoothed from 0 with
        // factor 2 => half of that. The other order would give 0.2084559 * 0.4169,
        // which is a different number.
        assert!(
            (channels[0][0] - 0.208_455_9).abs() < 1e-6,
            "got {}",
            channels[0][0]
        );
    }

    /// lavaplayer `PcmVolumeProcessor`: a tangent curve below 150 and linear above,
    /// quantised through an integer multiplier, with 100 short-circuited to unity.
    #[test]
    fn player_volume_follows_the_lavaplayer_curve() {
        assert_eq!(player_volume_multiplier(100), 1.0);
        assert_eq!(player_volume_multiplier(0), 0.0);

        for (volume, want) in [(50, 0.4169), (150, 2.4621), (200, 3.2828), (1000, 16.414)] {
            let got = player_volume_multiplier(volume);
            assert!((got - want).abs() < 1e-4, "volume {volume}: got {got}, want {want}");
        }

        // The player's own field is clamped to 0..=1000 before the curve.
        assert_eq!(
            player_volume_multiplier(5_000),
            player_volume_multiplier(1_000)
        );
    }

    /// A stereo pair of constant channels, which makes the matrix and centre-channel
    /// filters easy to reason about.
    fn stereo(left: f32, right: f32, len: usize) -> Vec<Vec<f32>> {
        vec![vec![left; len], vec![right; len]]
    }

    #[test]
    fn channel_mix_is_the_identity_at_its_defaults() {
        let chain = FilterChain::new(
            &filters(
                r#"{"channelMix":{"leftToLeft":1.0,"leftToRight":0.0,
                    "rightToLeft":0.0,"rightToRight":1.0}}"#,
            ),
            2,
        );
        assert!(!chain.is_enabled());
    }

    #[test]
    fn channel_mix_swaps_and_collapses() {
        let mut chain = FilterChain::new(
            &filters(
                r#"{"channelMix":{"leftToLeft":0.0,"leftToRight":1.0,
                    "rightToLeft":1.0,"rightToRight":0.0}}"#,
            ),
            2,
        );
        assert!(chain.is_enabled());

        let mut channels = stereo(1.0, -0.5, 4);
        chain.process(&mut channels);
        // A swap: what was on the right is now on the left.
        assert_eq!(channels[0], vec![-0.5; 4]);
        assert_eq!(channels[1], vec![1.0; 4]);
    }

    /// Both sides summing into one channel is the case that would clip without the
    /// clamp the original applies.
    #[test]
    fn channel_mix_saturates_rather_than_wrapping() {
        let mut chain = FilterChain::new(
            &filters(
                r#"{"channelMix":{"leftToLeft":1.0,"leftToRight":0.0,
                    "rightToLeft":1.0,"rightToRight":1.0}}"#,
            ),
            2,
        );
        let mut channels = stereo(0.8, 0.8, 2);
        chain.process(&mut channels);
        assert_eq!(channels[0], vec![1.0, 1.0]);
    }

    #[test]
    fn karaoke_cancels_a_centred_signal() {
        let mut chain = FilterChain::new(
            &filters(
                r#"{"karaoke":{"level":1.0,"monoLevel":0.0,
                    "filterBand":220.0,"filterWidth":100.0}}"#,
            ),
            2,
        );
        assert!(chain.is_enabled());

        // Identical on both sides is what a centred vocal looks like. With monoLevel
        // at 0 the extracted centre is not added back, so L-R leaves silence.
        let mut channels = stereo(0.5, 0.5, 8);
        chain.process(&mut channels);
        for sample in channels.iter().flatten() {
            assert!(sample.abs() < 1e-6, "expected silence, got {sample}");
        }
    }

    #[test]
    fn karaoke_leaves_a_hard_panned_signal_audible() {
        let mut chain = FilterChain::new(
            &filters(r#"{"karaoke":{"level":1.0,"monoLevel":0.0}}"#),
            2,
        );
        let mut channels = stereo(0.5, 0.0, 8);
        chain.process(&mut channels);
        assert!(channels[0].iter().any(|s| s.abs() > 0.1));
    }

    /// A mono track has no centre to subtract, and the original builds no converter
    /// for anything but stereo — so it must pass through rather than be silenced.
    #[test]
    fn karaoke_passes_mono_through_untouched() {
        let mut chain = FilterChain::new(&filters(r#"{"karaoke":{}}"#), 1);
        let mut channels = vec![vec![0.5, -0.5, 0.25]];
        chain.process(&mut channels);
        assert_eq!(channels[0], vec![0.5, -0.5, 0.25]);
    }

    /// Unlike `vibrato`/`tremolo`, karaoke's coefficient is computed once and then
    /// fed back through `y1`/`y2` forever, so a `NaN` here doesn't just spoil one
    /// sample — it never leaves the filter chain's state. `filterWidth` around 8e5
    /// or beyond used to make `exp` saturate to `0.0` or `+inf`, turning the `b*b /
    /// (4.0*c)` division into `0.0/0.0`.
    #[test]
    fn karaoke_survives_unclamped_filter_width() {
        let mut chain =
            FilterChain::new(&filters(r#"{"karaoke":{"filterWidth":900000}}"#), 2);
        let mut channels = stereo(0.5, -0.3, 4800);
        chain.process(&mut channels);
        assert!(
            channels.iter().all(|c| c.iter().all(|s| s.is_finite())),
            "unclamped positive filterWidth produced non-finite output"
        );

        let mut chain =
            FilterChain::new(&filters(r#"{"karaoke":{"filterWidth":-900000}}"#), 2);
        let mut channels = stereo(0.5, -0.3, 4800);
        chain.process(&mut channels);
        assert!(
            channels.iter().all(|c| c.iter().all(|s| s.is_finite())),
            "unclamped negative filterWidth produced non-finite output"
        );
    }

    /// Unlike `vibrato`/`tremolo`'s per-sample phase increment, rotation's `step`
    /// is derived once in `new()` from `rotationHz * 2 * PI`, which overflows to
    /// `f64::INFINITY` for an extreme `rotationHz` — and `phase.sin()` is `NaN`
    /// from the first sample on once `phase` itself goes non-finite.
    #[test]
    fn rotation_survives_unclamped_rotation_hz() {
        let mut chain = FilterChain::new(&filters(r#"{"rotation":{"rotationHz":1e308}}"#), 2);
        let mut channels = stereo(0.5, -0.3, 4800);
        chain.process(&mut channels);
        assert!(
            channels.iter().all(|c| c.iter().all(|s| s.is_finite())),
            "unclamped positive rotationHz produced non-finite output"
        );

        let mut chain = FilterChain::new(&filters(r#"{"rotation":{"rotationHz":-1e308}}"#), 2);
        let mut channels = stereo(0.5, -0.3, 4800);
        chain.process(&mut channels);
        assert!(
            channels.iter().all(|c| c.iter().all(|s| s.is_finite())),
            "unclamped negative rotationHz produced non-finite output"
        );
    }

    #[test]
    fn tremolo_modulates_amplitude_without_exceeding_the_input() {
        let mut chain = FilterChain::new(
            &filters(r#"{"tremolo":{"frequency":100.0,"depth":1.0}}"#),
            1,
        );
        assert!(chain.is_enabled());

        let mut channels = vec![vec![1.0f32; 4800]];
        chain.process(&mut channels);

        // Depth is halved on the way in, so the envelope swings over [0.5, 1.5]·0.5
        // of the carrier rather than all the way to silence.
        let max = channels[0].iter().cloned().fold(f32::MIN, f32::max);
        let min = channels[0].iter().cloned().fold(f32::MAX, f32::min);
        assert!(max > min, "the envelope never moved");
        assert!(min < 0.99, "no attenuation happened at all");
    }

    /// With an unwrapped `f32` phase (lavadsp's own behaviour), a 1000 Hz LFO's
    /// per-sample increment falls below the f32 ULP around phase 2^14 — reached
    /// after ~200k samples at this frequency, corresponding to what a 5 Hz LFO
    /// would only hit after several minutes of playback. `rem_euclid` keeps the
    /// phase small, so the envelope must still be swinging at the tail.
    #[test]
    fn tremolo_keeps_modulating_past_where_an_unwrapped_phase_would_freeze() {
        let mut chain = FilterChain::new(
            &filters(r#"{"tremolo":{"frequency":1000.0,"depth":1.0}}"#),
            1,
        );
        assert!(chain.is_enabled());

        let mut channels = vec![vec![1.0f32; 200_000]];
        chain.process(&mut channels);

        let tail = &channels[0][198_000..];
        let max = tail.iter().cloned().fold(f32::MIN, f32::max);
        let min = tail.iter().cloned().fold(f32::MAX, f32::min);
        assert!(max > 0.9, "the LFO froze low: max was {max}");
        assert!(min < 0.1, "the LFO froze high: min was {min}");
    }

    #[test]
    fn zero_depth_tremolo_and_vibrato_are_no_ops() {
        assert!(!FilterChain::new(&filters(r#"{"tremolo":{"depth":0.0}}"#), 2).is_enabled());
        assert!(!FilterChain::new(&filters(r#"{"vibrato":{"depth":0.0}}"#), 2).is_enabled());
    }

    /// Vibrato is a modulated delay line, so its first output samples come from an
    /// empty buffer. What matters is that the signal reappears rather than that it
    /// starts immediately.
    #[test]
    fn vibrato_delays_then_reproduces_the_signal() {
        let mut chain = FilterChain::new(
            &filters(r#"{"vibrato":{"frequency":2.0,"depth":0.5}}"#),
            1,
        );
        assert!(chain.is_enabled());

        let mut channels = vec![vec![0.7f32; 4800]];
        chain.process(&mut channels);

        assert!(channels[0][0].abs() < 1e-6, "the delay line started full");
        let tail = &channels[0][2000..];
        assert!(
            tail.iter().all(|s| (s - 0.7).abs() < 0.05),
            "the signal did not come back through the delay"
        );
    }

    /// Neither the protocol nor `Filters::validate` clamps `depth`/`frequency` to
    /// upstream's expected ranges, so the DSP itself has to survive whatever a
    /// client sends. Extreme values used to spin `read_hermite`'s and `next_lfo`'s
    /// wrap loops for a very long time; they're `rem_euclid`-based now, so this just
    /// has to finish and produce finite output.
    #[test]
    fn vibrato_survives_unclamped_depth_and_frequency() {
        let mut chain = FilterChain::new(
            &filters(r#"{"vibrato":{"frequency":1e9,"depth":1e6}}"#),
            2,
        );
        assert!(chain.is_enabled());

        let mut channels = stereo(1.0, 1.0, 4800);
        chain.process(&mut channels);

        assert!(
            channels.iter().all(|c| c.iter().all(|s| s.is_finite())),
            "unclamped vibrato parameters produced non-finite output"
        );

        let mut chain = FilterChain::new(
            &filters(r#"{"vibrato":{"frequency":-1e9,"depth":-1e6}}"#),
            2,
        );
        let mut channels = stereo(1.0, 1.0, 4800);
        chain.process(&mut channels);
        assert!(
            channels.iter().all(|c| c.iter().all(|s| s.is_finite())),
            "negative unclamped vibrato parameters produced non-finite output"
        );
    }

    /// `read_hermite`'s `rem_euclid` is mathematically confined to `[0, size)`, but
    /// float rounding can round its result up to exactly `size` when `write_index -
    /// 1 - delay` is a tiny negative number — indistinguishable from zero at
    /// `size`'s magnitude once `size` is added back in. That previously read
    /// `buffer[size + 3]` against a `size + 3`-long buffer and panicked. Engineered
    /// directly rather than swept for, since hitting the exact rounding case by
    /// running ordinary audio through it is luck, not a guarantee.
    #[test]
    fn vibrato_survives_a_delay_that_rounds_the_read_index_up_to_size() {
        let mut channel = VibratoChannel::new();
        for _ in 0..50 {
            channel.write(0.0);
        }

        let base = channel.write_index as f32 - 1.0;
        // The smallest float strictly greater than `base`. Subtracting it makes
        // `write_index - 1 - delay` a single ULP below zero — near enough to zero
        // that adding `size` back in (`rem_euclid`'s negative branch) rounds the
        // sum up to exactly `size`.
        let delay = f32::from_bits(base.to_bits() + 1);

        let _ = channel.read_hermite(delay);
    }

    #[test]
    fn rotation_pans_across_the_stereo_image() {
        let mut chain = FilterChain::new(&filters(r#"{"rotation":{"rotationHz":1.0}}"#), 2);
        assert!(chain.is_enabled());

        // A full second at 1 Hz is one complete revolution.
        let mut channels = stereo(1.0, 1.0, 48_000);
        chain.process(&mut channels);

        let left_max = channels[0].iter().cloned().fold(f32::MIN, f32::max);
        let left_min = channels[0].iter().cloned().fold(f32::MAX, f32::min);
        assert!(left_max > 0.99 && left_min < 0.01, "the left side did not sweep");

        // The two sides are driven by opposite halves of the same sine, so they sum
        // to the input at every instant.
        for (l, r) in channels[0].iter().zip(&channels[1]) {
            assert!((l + r - 1.0).abs() < 1e-5, "sides not complementary: {l} + {r}");
        }
    }

    #[test]
    fn zero_hz_rotation_is_a_no_op() {
        assert!(!FilterChain::new(&filters(r#"{"rotation":{"rotationHz":0.0}}"#), 2).is_enabled());
    }

    #[test]
    fn distortion_at_its_defaults_is_a_no_op() {
        let chain = FilterChain::new(
            &filters(
                r#"{"distortion":{"sinOffset":0.0,"sinScale":1.0,"cosOffset":0.0,
                    "cosScale":1.0,"tanOffset":0.0,"tanScale":1.0,
                    "offset":0.0,"scale":1.0}}"#,
            ),
            2,
        );
        assert!(!chain.is_enabled());
    }

    #[test]
    fn distortion_reshapes_and_stays_in_range() {
        let mut chain = FilterChain::new(&filters(r#"{"distortion":{"sinScale":4.0}}"#), 1);
        assert!(chain.is_enabled());

        let mut channels = ramp(512);
        let before = channels[0].clone();
        chain.process(&mut channels);

        assert_ne!(channels[0], before);
        for sample in &channels[0] {
            assert!(
                (-1.0..=1.0).contains(sample),
                "distortion left the range: {sample}"
            );
        }
    }

    /// A `Present(None)` clears one filter without touching the others, so it must
    /// build no stage rather than a stage at default settings.
    #[test]
    fn a_null_filter_builds_no_stage() {
        let chain = FilterChain::new(&filters(r#"{"karaoke":null,"rotation":null}"#), 2);
        assert!(!chain.is_enabled());
    }

    /// `timescale` is the one filter with no implementation. It is rejected with a
    /// 400 before reaching the chain, so building one must not add a stage.
    #[test]
    fn timescale_is_not_advertised_and_builds_no_stage() {
        assert!(!IMPLEMENTED_FILTERS.contains(&"timescale"));
        let chain = FilterChain::new(&filters(r#"{"timescale":{"speed":2.0}}"#), 2);
        assert!(!chain.is_enabled());
    }

    /// Feeds a fixed PCM input through the equalizer and compares against output
    /// captured from the original server.
    ///
    /// The constants themselves are no longer in question: `COEFFICIENTS_48000`, the
    /// 0.25/4.0 pair and both volume curves have been diffed against lavaplayer's
    /// `Equalizer.java` and `PcmVolumeProcessor.java` and lavadsp's converters, and
    /// that diff found two wrong digits and a missing output stage. What this test
    /// would add is end-to-end confirmation over a real buffer, including the
    /// accumulated `f32` error that a constant-by-constant reading cannot show.
    ///
    /// Ignored until the vectors are captured — the corpus has to come from running
    /// the original node, since numbers written here would only test this code
    /// against itself. To capture it: run a real Lavalink v4 node with a single
    /// `equalizer` filter set to known band gains, feed it a fixed-content local
    /// file as the source, capture its RTP/Opus output (or, more directly, patch
    /// lavaplayer's `Equalizer.java` to dump its pre-filter and post-filter PCM
    /// buffers to `input.pcm` / `output.pcm`), and load both here with this
    /// module's WAV helpers.
    #[test]
    #[ignore = "needs golden PCM captured from the original server"]
    fn equalizer_matches_golden_vectors() {
        unimplemented!("capture input.pcm / output.pcm from the original node first");
    }
}
