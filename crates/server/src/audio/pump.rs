//! The decode loop.
//!
//! Runs on its own thread, because everything it does — demux, decode, resample,
//! filter — is CPU-bound and blocking, and doing it on an async worker would stall
//! the runtime for every other player.
//!
//! ```text
//! source → demux → decode → resample → filters → ring
//! ```
//!
//! It has no deadline. Falling behind starves its own ring, which the reader reports
//! as nulled frames; it cannot make another player late. Running ahead blocks on the
//! ring, so it is never more than `frameBufferDurationMs` in front of playback.
//!
//! # Seeking
//!
//! Seeking is ours to implement: the audio the mixer sees is a live stream, so it
//! cannot seek it. The pump seeks the *demuxer*, discards the buffered audio and
//! rebases the position counter. Precision therefore follows the container — exact
//! where there is an index, approximate where the duration was guessed, refused on a
//! live stream.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::Arc;

use lavalink_protocol::filters::Filters;
use lavalink_protocol::player::TrackInfo;
use lavalink_protocol::Exception;
use symphonia::core::audio::{Audio as _, GenericAudioBufferRef};
use symphonia::core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, CODEC_ID_NULL_AUDIO,
};
use symphonia::core::codecs::CodecParameters;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::Time;

use super::filter::{player_volume_multiplier, FilterChain};
use super::resample::Resampler;
use super::ring::{RingWriter, CHANNELS};
use super::stream::{self, StreamOpener};
use super::PumpOutcome;
use crate::config::ResamplingQuality;

/// Commands the pump accepts while a track is running.
#[derive(Debug)]
pub enum PumpCommand {
    Seek { position_ms: i64 },
    SetFilters(Box<Filters>),
    SetVolume(i32),
    SetEndTime(Option<i64>),
    Stop,
}

pub struct PumpConfig {
    pub info: TrackInfo,
    pub start_position_ms: i64,
    pub end_time_ms: Option<i64>,
    pub volume: i32,
    pub filters: Filters,
    pub opener: Arc<StreamOpener>,
    pub resampling_quality: ResamplingQuality,
    /// Set (by the engine) whenever a [`PumpCommand`] is enqueued, and checked
    /// by a stalled HTTP source between reconnect attempts so a waiting command
    /// cuts the retry short instead of waiting out the whole budget. Cleared
    /// once [`State::drain_commands`] has drained the commands that set it.
    pub interrupt: Arc<AtomicBool>,
    /// Whether any audio has reached the ring, kept outside State so a panic
    /// inside run (caught by engine.rs's catch_unwind, which cannot see State
    /// once the unwind has destroyed it) can still tell a load-time crash from
    /// one partway through a track. The engine reads this after the catch,
    /// instead of assuming a panic always means the track had started.
    pub produced: Arc<AtomicBool>,
}

/// Runs one track to completion. Returns how it ended, for the actor to turn into
/// the right event.
///
/// `on_progress` is called (throttled by the caller) whenever audio reaches the
/// ring; the actor uses it to decide the track is not stuck.
pub fn run(
    config: PumpConfig,
    writer: RingWriter,
    commands: Receiver<PumpCommand>,
    position_ms: Arc<AtomicI64>,
    on_progress: &(dyn Fn() + Sync),
) -> PumpOutcome {
    match open(&config, &writer) {
        Ok(mut state) => state.decode_loop(&writer, &commands, &position_ms, on_progress),
        Err(exception) => {
            writer.finish();
            PumpOutcome::Failed {
                exception,
                started: false,
            }
        }
    }
}

struct State {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    resampler: Resampler,
    /// Carried from `PumpConfig` so the `ResetRequired` rebuild (a mid-stream
    /// format change) can reconstruct the resampler with the same quality
    /// tier it started with.
    resampling_quality: ResamplingQuality,
    filters: FilterChain,
    volume: f32,
    end_time_ms: Option<i64>,
    /// Whether any audio has reached the ring. Distinguishes a track that failed to
    /// start (`loadFailed`) from one that died partway (`finished`) — shared with
    /// the engine (see PumpConfig::produced) so that distinction survives a panic
    /// here too.
    produced: Arc<AtomicBool>,
    /// Consecutive decode errors. A few corrupt frames are normal and skipped; a run
    /// of them means the stream is broken.
    consecutive_errors: u32,
    /// Scratch buffers for the decode loop, reused every packet so a healthy stream
    /// settles into decoding without allocating. `interleaved` and `pcm` are taken
    /// out of `self` for the duration of a packet (see `decode_loop`) because they
    /// are passed by `&mut` to methods that themselves take `&mut self`; `planar`
    /// does not need that dance since `filter_interleaved` only ever borrows it from
    /// inside its own `&mut self`.
    interleaved: Vec<f32>,
    pcm: Vec<f32>,
    planar: Vec<Vec<f32>>,
    /// `filter_interleaved`'s output, reused the same way. Separate from `pcm`
    /// because `timescale` can make it a different length than its input — see
    /// `filter.rs`'s module docs — so it can no longer be written back in place.
    filtered: Vec<f32>,
    /// Shared with the source: cleared here once a batch of commands has been
    /// fully drained, so a later, unrelated stall doesn't inherit a stale
    /// "give up early" signal from a command that was already applied.
    interrupt: Arc<AtomicBool>,
}

/// How many corrupt frames in a row before the track is given up on.
const MAX_CONSECUTIVE_ERRORS: u32 = 32;

/// A container-declared sample rate outside this range cannot be a real audio
/// stream. Left unchecked, a degenerate value (e.g. `1`) makes the resampler's
/// step size (`source_rate / 48000`) tiny, so a single ordinary packet reserves
/// tens of millions of output frames on a thread with no memory bound.
const SANE_SAMPLE_RATE_HZ: std::ops::RangeInclusive<u32> = 4_000..=384_000;

/// Reads the sample rate and channel count the resampler needs, validating the
/// rate. Shared between `open` and the `ResetRequired` path (a mid-stream format
/// change, e.g. a chained Ogg segment at a different rate), so both build the
/// resampler from the same source of truth instead of one trusting stale params.
fn source_params(params: &AudioCodecParameters) -> Result<(u32, usize), Exception> {
    let source_rate = params.sample_rate.unwrap_or(super::ring::SAMPLE_RATE);
    if !SANE_SAMPLE_RATE_HZ.contains(&source_rate) {
        return Err(Exception::common(
            format!("The container declares an implausible sample rate: {source_rate}Hz"),
            "implausible sample rate",
        ));
    }
    let source_channels = params
        .channels
        .as_ref()
        .map(|channels| channels.count())
        .unwrap_or(CHANNELS);
    Ok((source_rate, source_channels))
}

/// How long `write_interruptibly` waits for ring space before checking for new
/// commands again. Long enough that an idle wait costs nothing, short enough that a
/// pause or a seek is not sitting behind a full ring for a noticeable time.
const COMMAND_POLL: std::time::Duration = std::time::Duration::from_millis(100);

fn open(config: &PumpConfig, writer: &RingWriter) -> Result<State, Exception> {
    let source = config
        .opener
        .open(&config.info, Arc::clone(&config.interrupt))
        .map_err(|error| error.to_exception())?;

    let mut hint = Hint::new();
    if let Some(extension) = extension_hint(&config.info) {
        hint.with_extension(&extension);
    }

    let stream = MediaSourceStream::new(source, Default::default());
    let format = symphonia::default::get_probe()
        .probe(&hint, stream, FormatOptions::default(), MetadataOptions::default())
        .map_err(|error| {
            Exception::common(
                format!("Could not read the container: {error}"),
                error.to_string(),
            )
        })?;

    let (track_id, params) = format
        .tracks()
        .iter()
        .find_map(|track| match &track.codec_params {
            Some(CodecParameters::Audio(params)) if params.codec != CODEC_ID_NULL_AUDIO => {
                Some((track.id, params.clone()))
            }
            _ => None,
        })
        .ok_or_else(|| {
            Exception::common("The container has no audio track", "no audio track")
        })?;

    let decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .map_err(|error| {
            Exception::common(
                format!("Unsupported codec: {error}"),
                error.to_string(),
            )
        })?;

    let (source_rate, source_channels) = source_params(&params)?;

    let mut state = State {
        format,
        decoder,
        track_id,
        resampler: Resampler::new(source_rate, source_channels, config.resampling_quality),
        resampling_quality: config.resampling_quality,
        filters: FilterChain::new(&config.filters, CHANNELS),
        volume: player_volume_multiplier(config.volume),
        end_time_ms: config.end_time_ms,
        produced: Arc::clone(&config.produced),
        consecutive_errors: 0,
        interleaved: Vec::new(),
        pcm: Vec::new(),
        planar: vec![Vec::new(); CHANNELS],
        filtered: Vec::new(),
        interrupt: Arc::clone(&config.interrupt),
    };

    // Starting mid-track is the same operation as seeking; doing it here rather than
    // decoding and discarding keeps position in a play request cheap.
    let mut start_position_ms = config.start_position_ms;
    if start_position_ms > 0 && !state.seek(start_position_ms) {
        // The source is not seekable (a live stream, an unindexed container): decoding
        // actually starts at the beginning, so the reported position must too.
        start_position_ms = 0;
    }
    writer.reset(start_position_ms);

    Ok(state)
}

impl State {
    fn decode_loop(
        &mut self,
        writer: &RingWriter,
        commands: &Receiver<PumpCommand>,
        position_ms: &Arc<AtomicI64>,
        on_progress: &(dyn Fn() + Sync),
    ) -> PumpOutcome {
        loop {
            match self.drain_commands(commands, writer) {
                ControlFlow::Continue { .. } => {}
                ControlFlow::Stopped => return PumpOutcome::Stopped,
            }

            if writer.is_closed() {
                return PumpOutcome::Stopped;
            }

            // The configured end time is enforced against the *playback* position,
            // not the decode position, so the track ends when the listener reaches
            // it rather than when the pump does.
            if let Some(end_time) = self.end_time_ms {
                if position_ms.load(Ordering::Relaxed) >= end_time {
                    writer.finish();
                    return PumpOutcome::Finished;
                }
            }

            let packet = match self.format.next_packet() {
                Ok(Some(packet)) => packet,
                Ok(None) => {
                    writer.finish();
                    return PumpOutcome::Finished;
                }
                Err(SymphoniaError::ResetRequired) => {
                    // The stream changed format mid-flight (a chained Ogg segment is
                    // the common case), which for us means the decoder's state is
                    // stale and the resampler's *configuration* — the source rate and
                    // channel count open read once — is stale too, not just its
                    // interpolation state. Rebuilding it from the track's current
                    // codec params is what resampler.reset() alone did not do.
                    self.decoder.reset();
                    let params = self
                        .format
                        .tracks()
                        .iter()
                        .find(|track| track.id == self.track_id)
                        .and_then(|track| match &track.codec_params {
                            Some(CodecParameters::Audio(params)) => Some(params.clone()),
                            _ => None,
                        });
                    let Some(params) = params else {
                        writer.finish();
                        return PumpOutcome::Failed {
                            exception: Exception::fault(
                                "The audio track disappeared after a format change",
                                "track lost on reset",
                            ),
                            started: self.produced.load(Ordering::Relaxed),
                        };
                    };
                    match source_params(&params) {
                        Ok((source_rate, source_channels)) => {
                            self.resampler =
                                Resampler::new(source_rate, source_channels, self.resampling_quality);
                        }
                        Err(exception) => {
                            writer.finish();
                            return PumpOutcome::Failed {
                                exception,
                                started: self.produced.load(Ordering::Relaxed),
                            };
                        }
                    }
                    continue;
                }
                // The source gave up a stalled reconnect early because a command
                // was waiting (see stream.rs's interrupt field) — not a real
                // failure, just a cue to check that command now instead of
                // burning the rest of the reconnect budget first.
                //
                // Matched on the payload type, not ErrorKind: the kind that says
                // this best is the one symphonia retries instead of propagating
                // — see stream::CommandPending.
                Err(SymphoniaError::IoError(error)) if stream::is_command_pending(&error) => {
                    continue;
                }
                Err(error) => {
                    writer.finish();
                    return self.fail(error);
                }
            };

            if packet.track_id != self.track_id {
                continue;
            }

            let decoded = match self.decoder.decode(&packet) {
                Ok(decoded) => decoded,
                // A corrupt frame is normal in a stream and is skipped, exactly as
                // lavaplayer does — but a run of them is a broken stream, not noise.
                Err(SymphoniaError::DecodeError(_)) | Err(SymphoniaError::IoError(_))
                    if self.consecutive_errors < MAX_CONSECUTIVE_ERRORS =>
                {
                    self.consecutive_errors += 1;
                    continue;
                }
                Err(error) => {
                    writer.finish();
                    return self.fail(error);
                }
            };
            self.consecutive_errors = 0;

            // What the container declared (`source_params`, which `open` and the
            // ResetRequired arm above both build the resampler from) is a hint,
            // not a guarantee. The decoder is the authority on what it actually
            // produced, and the two disagree often enough to matter:
            //
            // * HE-AAC's SBR outputs at twice the rate the container declares,
            //   so the resample ratio is off by a factor of two and the whole
            //   track plays at half speed.
            // * `params.channels` is `None` for more containers than not, and
            //   `source_params` falls back to stereo. For a mono stream
            //   `to_interleaved` then emits one sample per frame while
            //   `fill_stereo_frames` reads them back two at a time
            //   (`resample.rs`'s `chunks_exact`), so adjacent mono samples
            //   become an L/R pair and the track plays at double speed with the
            //   channels smeared into each other.
            //
            // Neither fails loudly — that is exactly why this is checked rather
            // than assumed. Two integer compares per packet against a decode
            // that already costs tens of µs (benches/pipeline.rs).
            let spec = decoded.spec();
            let (decoded_rate, decoded_channels) = (spec.rate(), spec.channels().count());
            if !self.resampler.matches_source(decoded_rate, decoded_channels) {
                // Same bound `source_params` puts on a container-declared rate,
                // and for the same reason: the rate is the resampler's step
                // divisor, so a degenerate one reserves an unbounded output
                // buffer on a thread with no memory limit.
                if !SANE_SAMPLE_RATE_HZ.contains(&decoded_rate) {
                    writer.finish();
                    return PumpOutcome::Failed {
                        exception: Exception::common(
                            format!("The decoder produced an implausible sample rate: {decoded_rate}Hz"),
                            "implausible sample rate",
                        ),
                        started: self.produced.load(Ordering::Relaxed),
                    };
                }
                tracing::debug!(
                    decoded_rate,
                    decoded_channels,
                    "the decoder's output format differs from the container's; rebuilding the resampler"
                );
                self.resampler =
                    Resampler::new(decoded_rate, decoded_channels, self.resampling_quality);
            }

            to_interleaved(&decoded, &mut self.interleaved);
            if self.interleaved.is_empty() {
                continue;
            }

            // Taken out of self because apply_volume takes &self — a field
            // can't also be passed to it by &mut while still attached. Moved back
            // in below, regardless of which path out of the loop is taken.
            let mut pcm = std::mem::take(&mut self.pcm);
            self.resampler.process_into(&self.interleaved, &mut pcm);
            if pcm.is_empty() {
                self.pcm = pcm;
                continue;
            }

            self.apply_volume(&mut pcm);

            let mut filtered = std::mem::take(&mut self.filtered);
            filter_interleaved(&mut self.filters, &pcm, &mut self.planar, &mut filtered);
            self.pcm = pcm;

            self.produced.store(true, Ordering::Relaxed);
            let outcome = self.write_interruptibly(writer, &filtered, commands);
            self.filtered = filtered;
            if let ControlFlow::Stopped = outcome {
                return PumpOutcome::Stopped;
            }
            on_progress();
        }
    }

    /// Writes `samples`, blocking while the ring is full — like
    /// [`RingWriter::write`], but checks for new commands between attempts
    /// instead of leaving them queued.
    ///
    /// Pausing stalls the pump on a full ring by design (`engine.rs`'s
    /// `set_paused` docs): without this, a Seek/SetVolume/SetFilters/SetEndTime
    /// sent while paused would sit unprocessed until the player is unpaused and
    /// the write already in progress finally drains, rather than taking effect
    /// right away.
    fn write_interruptibly(
        &mut self,
        writer: &RingWriter,
        samples: &[f32],
        commands: &Receiver<PumpCommand>,
    ) -> ControlFlow {
        let mut remaining = samples;
        loop {
            let (written, closed) = writer.try_write(remaining);
            if closed {
                return ControlFlow::Stopped;
            }
            remaining = &remaining[written..];
            if remaining.is_empty() {
                return ControlFlow::Continue { reset: false };
            }

            match self.drain_commands(commands, writer) {
                ControlFlow::Stopped => return ControlFlow::Stopped,
                // A seek that landed already discarded the ring's stale
                // buffered audio (RingWriter::reset, called from within
                // drain_commands) — remaining is exactly that kind of
                // stale audio, decoded before the seek, so it must be dropped
                // rather than appended right back after the discard.
                ControlFlow::Continue { reset: true } => {
                    return ControlFlow::Continue { reset: true };
                }
                ControlFlow::Continue { reset: false } => {}
            }

            if !writer.wait_for_space(COMMAND_POLL) {
                return ControlFlow::Stopped;
            }
        }
    }

    /// Applies commands that arrived since the last packet.
    fn drain_commands(
        &mut self,
        commands: &Receiver<PumpCommand>,
        writer: &RingWriter,
    ) -> ControlFlow {
        let mut reset = false;
        // Whether `interrupt` has been cleared since the last command was taken
        // off the channel. See the `Empty` arm below for why one clear is not
        // enough on its own.
        let mut cleared = false;
        loop {
            let command = match commands.try_recv() {
                Ok(command) => command,
                Err(TryRecvError::Empty) => {
                    if cleared {
                        // Empty *after* a clear: nothing arrived in the window
                        // that clear could have wiped, so the flag is genuinely
                        // stale and staying false is correct.
                        return ControlFlow::Continue { reset };
                    }
                    // Nothing left to act on — safe to let a future stall use
                    // its full reconnect budget again, rather than carrying
                    // this batch's "something is waiting" forward forever.
                    //
                    // But one clear is not enough. `signal_pump` sends before
                    // it sets the flag, which closes the window where this
                    // observes Empty and only *then* receives the command — it
                    // does not close the mirror of it, where the engine's
                    // send-and-set both land between the `try_recv` above and
                    // this store, and the store then wipes a flag raised for a
                    // command still sitting in the channel. The stalled HTTP
                    // source would go back to burning its whole reconnect
                    // budget (`stream.rs`'s `interrupt` check) with that
                    // command waiting behind it — for `Stop`, tens of seconds
                    // of a track that was already replaced. So clear, then go
                    // round once more: only an Empty seen after the clear
                    // proves nothing was lost to it.
                    self.interrupt.store(false, Ordering::Relaxed);
                    cleared = true;
                    continue;
                }
                // The engine dropped the sender: nobody is listening any more.
                Err(TryRecvError::Disconnected) => return ControlFlow::Stopped,
            };

            // A command was taken, so the next clear has a fresh window to
            // prove empty rather than inheriting this one's.
            cleared = false;

            match command {
                PumpCommand::Stop => return ControlFlow::Stopped,
                PumpCommand::Seek { position_ms } => {
                    // Discarding here is what makes the seek audible immediately
                    // rather than after the buffer drains — but only when the seek
                    // actually landed. On failure the decoder just keeps going from
                    // where it was, so the still-buffered audio is still correct and
                    // the reported position must stay where it actually is too.
                    if self.seek(position_ms) {
                        writer.reset(position_ms);
                        reset = true;
                    } else {
                        // The engine's begin_seek already announced this target
                        // (see RingWriter::begin_seek's docs) before the command
                        // ever reached here — a failure must give position
                        // reporting back to the real, unmoved position instead of
                        // holding at a target that is never going to land.
                        writer.cancel_seek();
                    }
                }
                PumpCommand::SetFilters(filters) => {
                    self.filters = FilterChain::new(&filters, CHANNELS);
                }
                PumpCommand::SetVolume(volume) => {
                    self.volume = player_volume_multiplier(volume);
                }
                PumpCommand::SetEndTime(end_time_ms) => {
                    self.end_time_ms = end_time_ms;
                }
            }
        }
    }

    /// Returns whether the seek actually landed. The caller must not rebase the
    /// reported position or discard buffered audio on a failed seek — the decoder
    /// carries on from wherever it actually is, and treating that as "now at
    /// `position_ms`" would desync the reported position from the real audio
    /// forever, since nothing after this ever corrects it back.
    fn seek(&mut self, position_ms: i64) -> bool {
        let position_ms = position_ms.max(0) as u64;
        let time = Time::try_new((position_ms / 1000) as i64, (position_ms % 1000) as u32 * 1_000_000)
            .expect("a millisecond remainder is always < 1_000_000_000ns");

        // Coarse mode where the container allows it: an exact seek re-decodes from
        // the previous keyframe, which on a long track is a noticeable stall for
        // accuracy nobody asked for.
        let result = self.format.seek(
            SeekMode::Coarse,
            SeekTo::Time {
                time,
                track_id: Some(self.track_id),
            },
        );

        let succeeded = result.is_ok();
        if let Err(error) = result {
            // Unseekable input: the original refuses too, and the track carries on
            // from where it was rather than dying.
            tracing::debug!(%error, position_ms, "seek failed");
        }

        self.decoder.reset();
        self.resampler.reset();
        self.filters.reset();
        succeeded
    }

    fn apply_volume(&self, samples: &mut [f32]) {
        if (self.volume - 1.0).abs() < f32::EPSILON {
            return;
        }
        for sample in samples {
            *sample *= self.volume;
        }
    }

    fn fail(&self, error: SymphoniaError) -> PumpOutcome {
        PumpOutcome::Failed {
            exception: Exception::fault(
                format!("Playback failed: {error}"),
                error.to_string(),
            ),
            started: self.produced.load(Ordering::Relaxed),
        }
    }
}

enum ControlFlow {
    /// `reset` is set when a command in this batch was a seek that landed,
    /// discarding whatever the ring held before it.
    Continue { reset: bool },
    Stopped,
}

/// Runs the filter chain over an interleaved buffer, writing the result into `out`
/// (cleared first, capacity reused — a healthy pump allocates nothing here once
/// warm) rather than back into `samples` in place, since `timescale` (the one
/// length-changing filter — see `filter.rs`'s module docs) can make the output a
/// different number of frames than the input.
///
/// `planar` is the transpose scratch, also reused across calls. The chain works on
/// planar channels and the ring wants interleaved, so a buffer with any filter
/// enabled pays two extra full passes on top of the DSP itself (three once
/// `timescale` is active, for the same reason `out` exists at all). That transpose
/// is the pump's cost, not the chain's, which is why this lives here rather than in
/// `filter.rs` — and why it is free-standing and public rather than a method on
/// `State`: `benches/filter.rs` measures the chain *with* the transpose around it,
/// which is the only shape playback actually runs.
pub fn filter_interleaved(
    chain: &mut FilterChain,
    samples: &[f32],
    planar: &mut [Vec<f32>],
    out: &mut Vec<f32>,
) {
    out.clear();
    if !chain.is_enabled() {
        out.extend_from_slice(samples);
        return;
    }

    let in_frames = samples.len() / CHANNELS;
    for (channel, plane) in planar.iter_mut().enumerate() {
        plane.clear();
        plane.extend((0..in_frames).map(|frame| samples[frame * CHANNELS + channel]));
    }

    chain.process(planar);

    let out_frames = planar[0].len();
    out.reserve(out_frames * CHANNELS);
    for frame in 0..out_frames {
        for plane in planar.iter() {
            out.push(plane[frame]);
        }
    }
}

/// Flattens any of symphonia's buffer types into interleaved `f32`, appended into
/// `out` (cleared first) so a healthy decode loop never allocates here.
fn to_interleaved(buffer: &GenericAudioBufferRef<'_>, out: &mut Vec<f32>) {
    use symphonia::core::audio::sample::{i24, u24, Sample as _};

    out.clear();

    macro_rules! interleave {
        ($buffer:expr, $convert:expr) => {{
            let channels = $buffer.spec().channels().count();
            let frames = $buffer.frames();
            out.reserve(frames * channels);

            // plane resolves a channel's slice out of an Option, so calling it
            // inside the frame loop would re-resolve the same slices once per
            // sample. Each plane is resolved once here instead.
            //
            // Stereo gets its own arm because it's what almost everything decodes
            // to, and reading the pair in step keeps loads and pushes sequential;
            // the general arm has to write with a stride, not worth it here.
            if channels == 2 {
                let (left, right) = $buffer.plane_pair(0, 1).unwrap();
                for frame in 0..frames {
                    out.push($convert(left[frame]));
                    out.push($convert(right[frame]));
                }
            } else {
                out.resize(frames * channels, 0.0);
                for channel in 0..channels {
                    let plane = $buffer.plane(channel).unwrap();
                    for frame in 0..frames {
                        out[frame * channels + channel] = $convert(plane[frame]);
                    }
                }
            }
        }};
    }

    match buffer {
        GenericAudioBufferRef::F32(buffer) => interleave!(buffer, |sample: f32| sample),
        GenericAudioBufferRef::F64(buffer) => interleave!(buffer, |sample: f64| sample as f32),
        GenericAudioBufferRef::S32(buffer) => {
            interleave!(buffer, |sample: i32| sample as f32 / i32::MAX as f32)
        }
        GenericAudioBufferRef::S24(buffer) => {
            interleave!(buffer, |sample: i24| sample.0 as f32 / 8_388_608.0)
        }
        GenericAudioBufferRef::S16(buffer) => {
            interleave!(buffer, |sample: i16| sample as f32 / i16::MAX as f32)
        }
        GenericAudioBufferRef::S8(buffer) => {
            interleave!(buffer, |sample: i8| sample as f32 / i8::MAX as f32)
        }
        GenericAudioBufferRef::U32(buffer) => interleave!(buffer, |sample: u32| {
            (sample as f32 - u32::MID as f32) / u32::MID as f32
        }),
        GenericAudioBufferRef::U24(buffer) => interleave!(buffer, |sample: u24| {
            (sample.0 as f32 - 8_388_608.0) / 8_388_608.0
        }),
        GenericAudioBufferRef::U16(buffer) => interleave!(buffer, |sample: u16| {
            (sample as f32 - u16::MID as f32) / u16::MID as f32
        }),
        GenericAudioBufferRef::U8(buffer) => interleave!(buffer, |sample: u8| {
            (sample as f32 - u8::MID as f32) / u8::MID as f32
        }),
    }
}

/// The container hint for a track: taken from its URL if it has one, and from the
/// identifier otherwise, which is where a local file keeps its path.
fn extension_hint(info: &TrackInfo) -> Option<String> {
    super::source::extension_of(info.uri.as_deref().unwrap_or(&info.identifier))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use lavalink_protocol::filters::Filters;

    /// A 16-bit PCM WAV, written by hand so the test does not need a fixture file or
    /// an encoder on PATH.
    fn write_wav(path: &std::path::Path, sample_rate: u32, channels: u16, seconds: f64) {
        let frames = (sample_rate as f64 * seconds) as u32;
        let data_len = frames * u32::from(channels) * 2;

        let mut bytes = Vec::with_capacity(44 + data_len as usize);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
        bytes.extend_from_slice(&channels.to_le_bytes());
        bytes.extend_from_slice(&sample_rate.to_le_bytes());
        let block_align = channels * 2;
        bytes.extend_from_slice(&(sample_rate * u32::from(block_align)).to_le_bytes());
        bytes.extend_from_slice(&block_align.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());

        for frame in 0..frames {
            let t = f64::from(frame) / f64::from(sample_rate);
            let value = ((t * 440.0 * std::f64::consts::TAU).sin() * 0.5 * 32767.0) as i16;
            for _ in 0..channels {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
        }

        std::fs::write(path, bytes).unwrap();
    }

    struct TempWav(std::path::PathBuf);

    impl TempWav {
        fn new(name: &str, sample_rate: u32, channels: u16, seconds: f64) -> Self {
            let path = std::env::temp_dir().join(format!("lavalink-rs-{name}.wav"));
            write_wav(&path, sample_rate, channels, seconds);
            Self(path)
        }

        fn track(&self) -> TrackInfo {
            info("local", None, self.0.to_str().unwrap())
        }
    }

    impl Drop for TempWav {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// Runs a whole track and returns (outcome, real samples delivered, position).
    ///
    /// The reader here stands in for the mixer, which pulls on a 20ms clock. An
    /// unpaced reader outruns the decoder and gets starvation silence, which is
    /// correct behaviour but is not delivered audio — so each read is classified by
    /// `nulled` (whether *this* read was a starved one), not `sent`: `sent` counts
    /// whole [`super::super::ring::FRAME_SAMPLES`] frames of real audio with a
    /// carried remainder, so a partial real read (buffer briefly short of a whole
    /// frame) may not tick it on the same call it happened. `nulled` stays a
    /// reliable per-call flag here because every read in this loop asks for
    /// exactly one frame, so a starved read's silence is never partial.
    fn play(config: PumpConfig) -> (PumpOutcome, usize, i64) {
        let position = Arc::new(AtomicI64::new(0));
        // Small buffer so the pump has to block and be woken, as it will in service.
        let (writer, mut reader) = super::super::ring::channel(
            200,
            Arc::clone(&position),
            Arc::new(super::super::ring::FrameCounters::default()),
        );
        let (commands_tx, commands_rx) = std::sync::mpsc::channel();

        let pump_position = Arc::clone(&position);
        let pump =
            std::thread::spawn(move || run(config, writer, commands_rx, pump_position, &|| {}));

        let mut delivered = 0;
        let mut reads = 0;
        let mut buffer = vec![0u8; super::super::ring::FRAME_SAMPLES * 4];
        loop {
            let read = std::io::Read::read(&mut reader, &mut buffer).unwrap();
            if read == 0 {
                break;
            }

            let (_, nulled) = reader.take_frame_stats();
            if nulled > 0 {
                // Give the decoder a moment rather than spinning on an empty ring.
                std::thread::sleep(Duration::from_millis(1));
            } else {
                delivered += read / 4;
            }

            reads += 1;
            assert!(reads < 200_000, "the reader is not making progress");
        }

        drop(commands_tx);
        let outcome = pump.join().unwrap();
        let final_position = position.load(Ordering::Relaxed);
        (outcome, delivered, final_position)
    }

    fn config(info: TrackInfo) -> PumpConfig {
        PumpConfig {
            info,
            start_position_ms: 0,
            end_time_ms: None,
            volume: 100,
            filters: Filters::default(),
            opener: Arc::new(StreamOpener::default()),
            resampling_quality: ResamplingQuality::Low,
            interrupt: Arc::new(AtomicBool::new(false)),
            produced: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The whole path end to end: demux, decode, resample 44.1k → 48k, and out
    /// through the ring at Discord's format.
    #[test]
    fn a_whole_track_decodes_resamples_and_reaches_the_ring() {
        let wav = TempWav::new("pump-whole", 44_100, 2, 0.5);
        let (outcome, delivered, position) = play(config(wav.track()));

        assert!(matches!(outcome, PumpOutcome::Finished), "{outcome:?}");

        // Half a second of 48kHz stereo, within a frame or two of slack for the
        // resampler's held-back tail.
        let expected_frames = (super::super::ring::SAMPLE_RATE / 2) as usize;
        let frames = delivered / CHANNELS;
        assert!(
            frames.abs_diff(expected_frames) < 256,
            "expected about {expected_frames} frames, got {frames}"
        );

        // Position is advanced by the reader, so it covers at least the track. It
        // can exceed it, because silence delivered during a starved moment is time
        // the listener really experienced.
        assert!(position >= 480, "expected at least 480ms, got {position}");
    }

    /// Both of `to_interleaved`'s arms — the stereo one and the general one — have
    /// to agree on frame-major order, and both have to keep dividing by `i16::MAX`.
    ///
    /// The divisor is the reason this asserts on exact values rather than shape.
    /// symphonia ships `copy_to_vec_interleaved`, which does everything this
    /// function does and skips the per-sample plane lookup, but it scales 16-bit
    /// samples by 32768 rather than 32767 — so adopting it would quietly rescale
    /// every sample of every 16-bit source. This test is what catches that.
    #[test]
    fn to_interleaved_is_frame_major_for_every_channel_count() {
        use symphonia::core::audio::{AudioBuffer, AudioMut as _, AudioSpec, Channels};

        const FRAMES: usize = 4;

        // 1 and 3 take the general arm, 2 takes the stereo one.
        for channels in [1usize, 2, 3] {
            let spec = AudioSpec::new(48_000, Channels::Discrete(channels as u16));
            let mut buffer = AudioBuffer::<i16>::new(spec, FRAMES);
            buffer.render_silence(Some(FRAMES));
            for channel in 0..channels {
                let plane = buffer.plane_mut(channel).unwrap();
                for (frame, sample) in plane.iter_mut().enumerate() {
                    // Distinct per (frame, channel), so a transposed or off-by-one
                    // result cannot coincidentally match.
                    *sample = (frame * 10 + channel) as i16;
                }
            }

            let mut out = Vec::new();
            to_interleaved(&GenericAudioBufferRef::S16(&buffer), &mut out);

            let expected: Vec<f32> = (0..FRAMES)
                .flat_map(|frame| {
                    (0..channels).map(move |channel| (frame * 10 + channel) as f32 / 32_767.0)
                })
                .collect();
            assert_eq!(out, expected, "channels = {channels}");
        }
    }

    #[test]
    fn a_mono_track_is_widened_to_stereo() {
        let wav = TempWav::new("pump-mono", 48_000, 1, 0.2);
        let (outcome, delivered, _) = play(config(wav.track()));

        assert!(matches!(outcome, PumpOutcome::Finished), "{outcome:?}");
        let frames = delivered / CHANNELS;
        assert!(frames.abs_diff(9_600) < 256, "got {frames} frames");
    }

    /// Starting mid-track is a seek, so this exercises the same path a `position`
    /// in a play request takes.
    #[test]
    fn starting_part_way_in_skips_that_much_audio() {
        let wav = TempWav::new("pump-offset", 48_000, 2, 1.0);
        let mut config = config(wav.track());
        config.start_position_ms = 500;

        let (outcome, delivered, position) = play(config);
        assert!(matches!(outcome, PumpOutcome::Finished), "{outcome:?}");

        // Roughly half the track remains, and the position resumes from the offset
        // rather than restarting at zero.
        let frames = delivered / CHANNELS;
        assert!(
            (10_000..40_000).contains(&frames),
            "expected about half a second of audio, got {frames} frames"
        );
        assert!(
            position > 700,
            "position should resume from the offset, got {position}"
        );
    }

    /// `endTime` is enforced against playback position, so the track stops early.
    #[test]
    fn an_end_time_cuts_the_track_short() {
        let wav = TempWav::new("pump-endtime", 48_000, 2, 2.0);
        let mut config = config(wav.track());
        config.end_time_ms = Some(300);

        let (outcome, delivered, _) = play(config);
        assert!(matches!(outcome, PumpOutcome::Finished), "{outcome:?}");

        // Well short of the full two seconds. The cut is not sample-exact — it takes
        // effect at the next packet boundary, as the original's marker does.
        let frames = delivered / CHANNELS;
        assert!(
            frames < 48_000,
            "expected the track to stop early, got {frames} frames"
        );
    }

    #[test]
    fn a_filtered_track_still_plays_through() {
        let wav = TempWav::new("pump-filtered", 44_100, 2, 0.3);
        let mut config = config(wav.track());
        config.filters = serde_json::from_str(
            r#"{"volume":0.5,"equalizer":[{"band":3,"gain":0.4}],"lowPass":{"smoothing":15.0}}"#,
        )
        .unwrap();

        let (outcome, delivered, _) = play(config);
        assert!(matches!(outcome, PumpOutcome::Finished), "{outcome:?}");
        assert!(delivered > 0);
    }

    /// Wraps a real format reader but always fails to seek — standing in for a
    /// live stream or an unindexed container without needing a genuinely
    /// unseekable source in the test.
    struct FailingSeekFormat(Box<dyn FormatReader>);

    impl FormatReader for FailingSeekFormat {
        fn format_info(&self) -> &symphonia::core::formats::FormatInfo {
            self.0.format_info()
        }

        fn media_info(&self) -> &symphonia::core::formats::MediaInfo {
            self.0.media_info()
        }

        fn metadata(&mut self) -> symphonia::core::meta::Metadata<'_> {
            self.0.metadata()
        }

        fn seek(
            &mut self,
            _mode: SeekMode,
            _to: SeekTo,
        ) -> symphonia::core::errors::Result<symphonia::core::formats::SeekedTo> {
            Err(SymphoniaError::SeekError(
                symphonia::core::errors::SeekErrorKind::Unseekable,
            ))
        }

        fn tracks(&self) -> &[symphonia::core::formats::Track] {
            self.0.tracks()
        }

        fn next_packet(
            &mut self,
        ) -> symphonia::core::errors::Result<Option<symphonia::core::packet::Packet>> {
            self.0.next_packet()
        }

        fn into_inner<'s>(self: Box<Self>) -> MediaSourceStream<'s>
        where
            Self: 's,
        {
            self.0.into_inner()
        }
    }

    /// Wraps a real format reader but returns the `interrupt` error kind from
    /// `stream.rs`'s reconnect loop exactly once, then delegates normally — a
    /// stalled HTTP source giving up early because a command is waiting,
    /// without needing a genuinely stalled source in the test.
    struct InterruptOnceFormat {
        inner: Box<dyn FormatReader>,
        fired: bool,
    }

    impl FormatReader for InterruptOnceFormat {
        fn format_info(&self) -> &symphonia::core::formats::FormatInfo {
            self.inner.format_info()
        }

        fn media_info(&self) -> &symphonia::core::formats::MediaInfo {
            self.inner.media_info()
        }

        fn metadata(&mut self) -> symphonia::core::meta::Metadata<'_> {
            self.inner.metadata()
        }

        fn seek(
            &mut self,
            mode: SeekMode,
            to: SeekTo,
        ) -> symphonia::core::errors::Result<symphonia::core::formats::SeekedTo> {
            self.inner.seek(mode, to)
        }

        fn tracks(&self) -> &[symphonia::core::formats::Track] {
            self.inner.tracks()
        }

        fn next_packet(
            &mut self,
        ) -> symphonia::core::errors::Result<Option<symphonia::core::packet::Packet>> {
            if !self.fired {
                self.fired = true;
                // The same marker a stalled HttpMediaSource produces. Not
                // ErrorKind::Interrupted: that is what this used to be, and it
                // is exactly why the mock passed while the real demuxer path did
                // not — symphonia retries that kind rather than propagating it.
                return Err(SymphoniaError::IoError(std::io::Error::other(
                    stream::CommandPending,
                )));
            }
            self.inner.next_packet()
        }

        fn into_inner<'s>(self: Box<Self>) -> MediaSourceStream<'s>
        where
            Self: 's,
        {
            self.inner.into_inner()
        }
    }

    /// The bug this fix targets: `next_packet()` returning the `Interrupted`
    /// error kind — what a stalled HTTP source now does the moment a pump
    /// command is waiting, instead of exhausting its reconnect budget first —
    /// used to have no dedicated handling in `decode_loop`, so it fell into the
    /// catch-all `Err(error) => self.fail(error)` arm and ended the track as a
    /// spurious failure. It must instead drain the waiting command and keep
    /// decoding.
    #[test]
    fn an_interrupted_packet_drains_commands_and_keeps_decoding_instead_of_failing() {
        let wav = TempWav::new("pump-interrupted", 48_000, 2, 0.2);
        let config = config(wav.track());

        let position = Arc::new(AtomicI64::new(0));
        let (writer, _reader) = super::super::ring::channel(
            4096,
            Arc::clone(&position),
            Arc::new(super::super::ring::FrameCounters::default()),
        );

        let mut state = open(&config, &writer).unwrap();
        state.format = Box::new(InterruptOnceFormat {
            inner: state.format,
            fired: false,
        });

        // Kept alive (unlike most tests here) so later try_recv calls in the
        // rest of the loop see Empty rather than Disconnected — a dropped
        // sender would end the track as Stopped regardless of the interrupt,
        // which is not what this test is about.
        let (commands_tx, commands_rx) = std::sync::mpsc::channel();
        commands_tx.send(PumpCommand::SetVolume(50)).unwrap();

        let outcome = state.decode_loop(&writer, &commands_rx, &position, &|| {});

        assert!(matches!(outcome, PumpOutcome::Finished), "{outcome:?}");
        assert_eq!(
            state.volume,
            player_volume_multiplier(50),
            "the command sent alongside the interrupted packet must still be applied"
        );
    }

    /// The bug: `drain_commands` used to call `writer.reset(position_ms)`
    /// unconditionally, so a seek that the container refused (the way an
    /// unindexed container or a live stream would) still rebased the reported
    /// position to the requested target — permanently desyncing it from the
    /// audio actually being decoded, since nothing downstream ever corrects it.
    #[test]
    fn a_failed_seek_does_not_rebase_the_reported_position() {
        let wav = TempWav::new("pump-seek-fail", 48_000, 2, 1.0);
        let config = config(wav.track());

        let position = Arc::new(AtomicI64::new(0));
        let (writer, _reader) = super::super::ring::channel(
            4096,
            Arc::clone(&position),
            Arc::new(super::super::ring::FrameCounters::default()),
        );

        let mut state = open(&config, &writer).unwrap();
        state.format = Box::new(FailingSeekFormat(state.format));
        // open already reset the writer to 0; move it away from 0 so a
        // spurious rebase toward the requested target (5000) is observable.
        writer.reset(250);

        let (commands_tx, commands_rx) = std::sync::mpsc::channel();
        commands_tx.send(PumpCommand::Seek { position_ms: 5_000 }).unwrap();
        drop(commands_tx);

        let flow = state.drain_commands(&commands_rx, &writer);
        assert!(matches!(flow, ControlFlow::Stopped), "the sender was dropped");
        assert_eq!(
            position.load(Ordering::Relaxed),
            250,
            "a failed seek must not move the reported position toward the target"
        );
    }

    /// The bug: `drain_commands` cleared `interrupt` the moment it saw an empty
    /// channel, with nothing re-checking afterwards. `signal_pump` sends before
    /// it sets the flag, which closes the window where the pump observes Empty
    /// and only then receives the command — but not the mirror of it, where the
    /// engine's send-and-set both land between that `try_recv` and the pump's
    /// own store, so the store wipes a flag raised for a command still queued.
    /// The stalled HTTP source then burns its whole reconnect budget with that
    /// command waiting (`stream.rs`'s `interrupt` check).
    ///
    /// The bug: the resampler was built once, from what the *container*
    /// declared (`source_params`), and never checked against what the decoder
    /// actually produced. HE-AAC's SBR outputs at twice the declared rate, and
    /// `params.channels` is `None` often enough that `source_params` falls back
    /// to stereo for a mono stream — either way the track plays at the wrong
    /// speed, silently, with `to_interleaved` framing the buffer by its real
    /// channel count and `fill_stereo_frames` de-framing it by the declared one.
    ///
    /// The lie is injected rather than found: a container whose declaration is
    /// wrong in exactly this way is a fixture this test would have to ship a
    /// real HE-AAC file to produce, and the correction is the same code either
    /// way. Half the rate and half the channels is the shape both real cases
    /// take, and left uncorrected it stretches the same audio to four times its
    /// length — far outside any resampler slack.
    #[test]
    fn a_decoder_format_that_contradicts_the_container_is_corrected() {
        let wav = TempWav::new("pump-format-mismatch", 48_000, 2, 0.2);
        let config = config(wav.track());

        let position = Arc::new(AtomicI64::new(0));
        // Big enough to hold even the uncorrected four-times-too-long result, so
        // the pump never blocks and the failure shows up as a sample count
        // rather than as a hang.
        let (writer, mut reader) = super::super::ring::channel(
            4_000,
            Arc::clone(&position),
            Arc::new(super::super::ring::FrameCounters::default()),
        );
        let mut state = open(&config, &writer).unwrap();
        assert!(
            state.resampler.matches_source(48_000, 2),
            "open should have read the container's real format"
        );

        state.resampler = Resampler::new(24_000, 1, ResamplingQuality::Low);

        // Kept alive so the drain sees Empty rather than Disconnected.
        let (_commands_tx, commands_rx) = std::sync::mpsc::channel();
        let outcome = state.decode_loop(&writer, &commands_rx, &position, &|| {});
        assert!(matches!(outcome, PumpOutcome::Finished), "{outcome:?}");

        let mut delivered = 0;
        let mut buffer = vec![0u8; super::super::ring::FRAME_SAMPLES * 4];
        loop {
            let read = std::io::Read::read(&mut reader, &mut buffer).unwrap();
            if read == 0 {
                break;
            }
            delivered += read / 4;
        }

        // 0.2s of 48kHz stereo, with a frame or two of slack for the
        // resampler's held-back tail. The uncorrected result is ~4x this.
        let frames = delivered / CHANNELS;
        let expected = (super::super::ring::SAMPLE_RATE as usize) / 5;
        assert!(
            frames.abs_diff(expected) < 512,
            "expected about {expected} frames, got {frames} — the resampler kept \
             the container's declared format instead of the decoder's"
        );
        assert!(
            state.resampler.matches_source(48_000, 2),
            "the resampler must have been rebuilt for what the decoder really produced"
        );
    }

    /// The fix itself is an ordering argument and has no test that can fail on
    /// its absence: the window is two instructions wide with no seam to inject
    /// a send into, and a racing soak (tried, 2 000 rounds) never lands in it
    /// because spawning the producer costs orders of magnitude more than the
    /// drain does. `signal_pump`'s own half of the same ordering
    /// (`engine.rs`) is untested for the same reason. What is pinned here is
    /// the part that *is* deterministic and that the rewritten loop could
    /// plausibly break: the clear still happens, and every queued command is
    /// still applied across the extra round trip the recheck adds.
    #[test]
    fn a_fully_drained_channel_still_clears_the_flag_and_applies_every_command() {
        let wav = TempWav::new("pump-interrupt-clear", 48_000, 2, 0.1);
        let config = config(wav.track());
        let interrupt = Arc::clone(&config.interrupt);

        let position = Arc::new(AtomicI64::new(0));
        let (writer, _reader) = super::super::ring::channel(
            4096,
            Arc::clone(&position),
            Arc::new(super::super::ring::FrameCounters::default()),
        );
        let mut state = open(&config, &writer).unwrap();

        // Kept alive so the drain sees Empty rather than Disconnected.
        let (commands_tx, commands_rx) = std::sync::mpsc::channel();
        commands_tx.send(PumpCommand::SetVolume(50)).unwrap();
        commands_tx.send(PumpCommand::SetEndTime(Some(1_000))).unwrap();
        interrupt.store(true, Ordering::Relaxed);

        let flow = state.drain_commands(&commands_rx, &writer);

        assert!(matches!(flow, ControlFlow::Continue { reset: false }));
        assert_eq!(state.volume, player_volume_multiplier(50));
        assert_eq!(state.end_time_ms, Some(1_000));
        assert!(
            !interrupt.load(Ordering::Relaxed),
            "a channel drained to empty must leave the flag clear, or every later \
             stall inherits a give-up-early signal it never earned"
        );
    }

    /// The bug: the pump only checked for new commands between packets, at the
    /// top of `decode_loop`. A full ring — exactly what pausing causes,
    /// deliberately (`engine.rs`'s `set_paused` docs) — used to block inside
    /// `write` with no visibility into the command channel at all, so a
    /// Seek/SetVolume/SetFilters/SetEndTime sent while paused sat unprocessed
    /// until the player was unpaused and the write already in flight finally
    /// drained.
    #[test]
    fn a_command_sent_while_blocked_on_a_full_ring_is_applied_without_waiting_for_space() {
        let wav = TempWav::new("pump-blocked-command", 48_000, 2, 0.1);
        let config = config(wav.track());

        let position = Arc::new(AtomicI64::new(0));
        let buffer_ms = 20;
        let (writer, _reader) = super::super::ring::channel(
            buffer_ms,
            Arc::clone(&position),
            Arc::new(super::super::ring::FrameCounters::default()),
        );

        let mut state = open(&config, &writer).unwrap();

        // Fill the ring completely — nobody is draining _reader, the same
        // situation set_paused deliberately creates.
        let capacity_samples =
            (super::super::ring::SAMPLE_RATE as usize * buffer_ms as usize / 1000) * CHANNELS;
        let (filled, closed) = writer.try_write(&vec![0.0; capacity_samples]);
        assert_eq!(filled, capacity_samples);
        assert!(!closed);

        let (commands_tx, commands_rx) = std::sync::mpsc::channel();
        commands_tx.send(PumpCommand::SetVolume(50)).unwrap();
        commands_tx.send(PumpCommand::Stop).unwrap();
        drop(commands_tx);

        // The ring has no room at all, so if commands were only checked
        // between packets (the old behaviour), this call would have to wait
        // for space that will never come — it must instead see both commands
        // on its very first attempt and return immediately.
        let more_samples = vec![0.0; 10];
        let flow = state.write_interruptibly(&writer, &more_samples, &commands_rx);

        assert!(matches!(flow, ControlFlow::Stopped));
        assert_eq!(
            state.volume,
            player_volume_multiplier(50),
            "SetVolume must be applied even though the ring never had room to write into"
        );
    }

    /// A seek that interrupts a blocked write discards the ring's stale
    /// pre-seek audio (`RingWriter::reset`, from within `drain_commands`) — the
    /// samples this call was in the middle of writing when the seek arrived
    /// are exactly that kind of stale audio, decoded before the seek, and must
    /// not be appended back in once the reset has run.
    #[test]
    fn a_seek_that_interrupts_a_blocked_write_discards_the_stale_remainder() {
        let wav = TempWav::new("pump-seek-interrupt", 48_000, 2, 1.0);
        let config = config(wav.track());

        let position = Arc::new(AtomicI64::new(0));
        let buffer_ms = 20;
        let (writer, mut reader) = super::super::ring::channel(
            buffer_ms,
            Arc::clone(&position),
            Arc::new(super::super::ring::FrameCounters::default()),
        );

        let mut state = open(&config, &writer).unwrap();

        let capacity_samples =
            (super::super::ring::SAMPLE_RATE as usize * buffer_ms as usize / 1000) * CHANNELS;
        // A recognizable marker, filling the ring completely.
        let (filled, _) = writer.try_write(&vec![9.75; capacity_samples]);
        assert_eq!(filled, capacity_samples);

        // Kept alive (unlike the other tests here) so that after this one
        // message, try_recv reports empty rather than disconnected — a
        // dropped sender would make drain_commands return Stopped
        // regardless of the seek, which is not what this test is about.
        let (commands_tx, commands_rx) = std::sync::mpsc::channel();
        commands_tx.send(PumpCommand::Seek { position_ms: 500 }).unwrap();

        // Stale audio decoded before the seek — must not survive the reset.
        let stale_remainder = vec![9.75; 10];
        let flow = state.write_interruptibly(&writer, &stale_remainder, &commands_rx);
        assert!(matches!(flow, ControlFlow::Continue { reset: true }));

        // The marker value must never surface: reset cleared the pre-seek
        // buffer, and the stale remainder must not have been appended back in
        // afterward.
        let mut out = vec![0u8; capacity_samples * 4 + 64];
        let read = std::io::Read::read(&mut reader, &mut out).unwrap();
        let samples: Vec<f32> = out[..read]
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes(chunk.try_into().unwrap()))
            .collect();
        assert!(
            !samples.contains(&9.75),
            "stale pre-seek audio must not reappear after the ring was reset"
        );
    }

    #[test]
    fn stopping_ends_the_pump_without_finishing_the_track() {
        let wav = TempWav::new("pump-stop", 48_000, 2, 30.0);
        let position = Arc::new(AtomicI64::new(0));
        let (writer, _reader) = super::super::ring::channel(
            200,
            Arc::clone(&position),
            Arc::new(super::super::ring::FrameCounters::default()),
        );
        let (commands, commands_rx) = std::sync::mpsc::channel();

        let pump = std::thread::spawn(move || {
            run(config(wav.track()), writer, commands_rx, position, &|| {})
        });

        // Nothing is draining the ring, so the pump is parked on a full buffer —
        // which is exactly when a stop has to still get through.
        commands.send(PumpCommand::Stop).unwrap();
        let outcome = pump.join().unwrap();

        assert!(matches!(outcome, PumpOutcome::Stopped), "{outcome:?}");
    }

    fn info(source: &str, uri: Option<&str>, identifier: &str) -> TrackInfo {
        TrackInfo {
            identifier: identifier.to_owned(),
            is_seekable: true,
            author: "a".into(),
            length: 0,
            is_stream: false,
            position: 0,
            title: "t".into(),
            uri: uri.map(str::to_owned),
            source_name: source.to_owned(),
            artwork_url: None,
            isrc: None,
        }
    }

    #[test]
    fn extensions_come_from_the_uri_when_there_is_one() {
        let track = info("http", Some("https://a.invalid/b/song.mp3?x=1"), "ignored");
        assert_eq!(extension_hint(&track).as_deref(), Some("mp3"));
    }

    #[test]
    fn extensions_fall_back_to_the_identifier() {
        let track = info("local", None, "C:/music/song.FLAC");
        assert_eq!(extension_hint(&track).as_deref(), Some("flac"));
    }

    #[test]
    fn a_path_without_an_extension_gives_no_hint() {
        let track = info("http", Some("https://a.invalid/stream"), "s");
        assert_eq!(extension_hint(&track), None);
    }

    /// `open` and the `ResetRequired` path must agree on what a valid source rate
    /// is — this is the one function both call into.
    #[test]
    fn source_params_reads_the_declared_rate_and_channels() {
        let mut params = AudioCodecParameters::new();
        params.sample_rate = Some(44_100);
        params.channels = Some(symphonia::core::audio::Channels::Positioned(
            symphonia::core::audio::Position::FRONT_LEFT,
        ));
        assert_eq!(source_params(&params).unwrap(), (44_100, 1));
    }

    #[test]
    fn source_params_rejects_an_implausible_rate() {
        let mut params = AudioCodecParameters::new();
        params.sample_rate = Some(1);
        assert!(source_params(&params).is_err());
    }

    /// The bug this guards: an unchecked container-declared sample rate makes the
    /// resampler's step size (`source_rate / 48000`) tiny, so one ordinary packet
    /// would reserve tens of millions of output frames. A WAV declaring 1Hz must be
    /// refused at `open`, not decoded.
    #[test]
    fn an_implausible_sample_rate_fails_without_having_started() {
        let wav = TempWav::new("implausible-rate", 1, 1, 1.0);
        let (outcome, delivered, _position) = play(PumpConfig {
            info: wav.track(),
            start_position_ms: 0,
            end_time_ms: None,
            volume: 100,
            filters: Filters::default(),
            opener: Arc::new(StreamOpener::default()),
            resampling_quality: ResamplingQuality::Low,
            interrupt: Arc::new(AtomicBool::new(false)),
            produced: Arc::new(AtomicBool::new(false)),
        });

        assert!(
            matches!(outcome, PumpOutcome::Failed { started: false, .. }),
            "{outcome:?}"
        );
        assert_eq!(delivered, 0);
    }

    #[test]
    fn opening_something_that_is_not_audio_fails_without_having_started() {
        let path =
            std::env::temp_dir().join(format!("lavalink-rs-pump-test-{}.bin", std::process::id()));
        std::fs::write(&path, vec![0u8; 8192]).unwrap();

        let position = Arc::new(AtomicI64::new(0));
        let (writer, _reader) = super::super::ring::channel(
            400,
            Arc::clone(&position),
            Arc::new(super::super::ring::FrameCounters::default()),
        );
        let (_tx, rx) = std::sync::mpsc::channel();

        let outcome = run(
            PumpConfig {
                info: info("local", None, path.to_str().unwrap()),
                start_position_ms: 0,
                end_time_ms: None,
                volume: 100,
                filters: Filters::default(),
                opener: Arc::new(StreamOpener::default()),
                resampling_quality: ResamplingQuality::Low,
                interrupt: Arc::new(AtomicBool::new(false)),
                produced: Arc::new(AtomicBool::new(false)),
            },
            writer,
            rx,
            position,
            &|| {},
        );

        // started: false is what makes the actor report loadFailed rather than
        // finished.
        assert!(matches!(
            outcome,
            PumpOutcome::Failed { started: false, .. }
        ));

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_missing_file_fails_before_producing_anything() {
        let position = Arc::new(AtomicI64::new(0));
        let (writer, _reader) = super::super::ring::channel(
            400,
            Arc::clone(&position),
            Arc::new(super::super::ring::FrameCounters::default()),
        );
        let (_tx, rx) = std::sync::mpsc::channel();

        let outcome = run(
            PumpConfig {
                info: info("local", None, "./definitely-not-here-8f3a.mp3"),
                start_position_ms: 0,
                end_time_ms: None,
                volume: 100,
                filters: Filters::default(),
                opener: Arc::new(StreamOpener::default()),
                resampling_quality: ResamplingQuality::Low,
                interrupt: Arc::new(AtomicBool::new(false)),
                produced: Arc::new(AtomicBool::new(false)),
            },
            writer,
            rx,
            position,
            &|| {},
        );

        assert!(matches!(
            outcome,
            PumpOutcome::Failed { started: false, .. }
        ));
    }
}
