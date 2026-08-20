//! The pipeline, assembled.
//!
//! pump thread                          ring                songbird mixer
//! ───────────                          ────                ──────────────
//! decode → resample → filter ──write──▶ ... ──RawAdapter──▶ pull every 20ms
//!                                                           → Opus → Discord
//!
//! Two things about the right-hand side are not choices we get to make:
//!
//! • The mixer pulls. There is no API for handing it finished frames; it reads
//!   from a MediaSource on its own clock. So the ring's read end is what we expose.
//! • The mixer encodes. Opus passthrough — forwarding a pre-encoded stream
//!   untouched — requires track volume to be exactly 1.0, and volume is a filter
//!   we have to support. So passthrough is off and encoding is songbird's.
//!
//! Which leaves seeking as ours to implement, which is pump's job.
//!
//! Where the boundaries are
//!
//! The actor calls into here and never blocks: every method below either flips an
//! atomic, sends on a channel, or spawns. The pump runs on a
//! dedicated OS thread rather than the blocking pool, because it lives for the
//! length of a track and would otherwise occupy a pool slot indefinitely.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lavalink_protocol::filters::Filters;
use lavalink_protocol::Exception;
use songbird::input::{Input, RawAdapter};
use songbird::tracks::TrackHandle;
use tokio::sync::mpsc::Sender as AsyncSender;

use super::pump::{self, PumpCommand, PumpConfig};
use super::ring::{self, CHANNELS, SAMPLE_RATE};
use super::stream::StreamOpener;
use super::{Engine, EngineEvent, PlayRequest};
use crate::config::ResamplingQuality;
use crate::lock;
use crate::player::EventSlot;
use crate::voice::SharedVoice;

/// How often the pump reports that it is still producing. Throttled hard: this only
/// has to beat trackStuckThresholdMs, and the actor's queue is small.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(500);

pub struct PipelineEngine {
    guild_id: u64,
    position_ms: Arc<AtomicI64>,
    buffer_ms: u32,
    resampling_quality: ResamplingQuality,
    voice: SharedVoice,
    opener: Arc<StreamOpener>,
    runtime: tokio::runtime::Handle,
    /// Filled in once the actor exists. Its own queue, not the actor's general
    /// command queue — see Engine::attach.
    events: EventSlot,
    /// Shared so the task that hands the input to the mixer can store the resulting
    /// handle back, which is what pause later needs.
    active: Arc<Mutex<Option<Active>>>,
    /// One per player, not one per track: see ring::FrameCounters's docs.
    frames: Arc<ring::FrameCounters>,
    /// The next generation to hand out. Lives on the engine, not derived from
    /// whatever stop_active tears down — a full stop clears active to None,
    /// so deriving the next generation from the outgoing one would restart the
    /// count at 1 after every stop, letting a pump parked on a stalled source
    /// (still holding the old generation) pass is_current for a later,
    /// unrelated track that happens to reuse it.
    next_generation: AtomicU64,
}

/// A running track.
struct Active {
    /// Commands to the pump thread. Dropping it stops the pump.
    commands: Sender<PumpCommand>,
    /// Set alongside every send on commands, so a pump stuck retrying a
    /// stalled HTTP source (stream.rs's interrupt) notices a command is
    /// waiting and gives up its remaining retry budget instead of making the
    /// command wait out the whole thing.
    interrupt: Arc<AtomicBool>,
    /// The songbird side, so pause and stop reach the mixer.
    track: Option<TrackHandle>,
    /// A clone of the pump's ring writer, kept only so stop_active can revoke
    /// this ring's claim on the shared position counter — see
    /// ring::RingWriter::detach_position.
    ring: ring::RingWriter,
    /// Bumped on every new track; checked by both the handle-storing task and the
    /// terminal-outcome dispatch (play, below) via is_current so a late
    /// outcome from a superseded pump is ignored rather than applied to whatever
    /// track replaced it.
    generation: u64,
    /// Desired pause state. Source of truth for both play's spawned task (once
    /// track is filled in) and set_paused (whenever it runs) — whichever of the
    /// two runs last is what actually gets applied to the handle, instead of a
    /// value snapshotted before the handle existed.
    paused: bool,
}

/// Sends a command to the pump and marks it interrupted, in that order.
///
/// The order is load-bearing, not cosmetic: drain_commands's Empty branch
/// clears interrupt once it finds nothing to act on, and a channel send is
/// visible to any try_recv that follows it. Setting the flag first would leave
/// a window where drain_commands observes Empty, clears the flag, and only
/// then receives this command — reinstating whatever stall the flag exists to
/// cut short (a full reconnect wait for Stop, up to COMMAND_POLL for others).
/// Centralized here so both call sites share one place that gets the order right,
/// rather than each re-implementing it.
fn signal_pump(commands: &Sender<PumpCommand>, interrupt: &AtomicBool, command: PumpCommand) {
    let _ = commands.send(command);
    interrupt.store(true, Ordering::Relaxed);
}

/// Whether generation is still the one active currently holds. Active's
/// pump-thread commands sender is dropped to signal a stop, but the pump can be
/// mid-next_packet() at that moment and reach a terminal outcome (a natural EOF,
/// a decode error) without ever observing it — so a stale outcome from a
/// superseded pump can still arrive after a new one has taken over.
fn is_current(active: &Mutex<Option<Active>>, generation: u64) -> bool {
    lock(active)
        .as_ref()
        .is_some_and(|current| current.generation == generation)
}

/// Whether a stop whose voice.stop() has not run yet has already been
/// superseded by a play. See Engine::stop for why that matters and why
/// active being filled in is the signal: play clears and reinstalls it
/// synchronously, before it spawns anything.
fn superseded_by_a_replacement(active: &Mutex<Option<Active>>) -> bool {
    lock(active).is_some()
}

impl PipelineEngine {
    pub fn new(
        guild_id: u64,
        buffer_ms: u32,
        resampling_quality: ResamplingQuality,
        voice: SharedVoice,
        opener: Arc<StreamOpener>,
        events: EventSlot,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self {
            guild_id,
            position_ms: Arc::new(AtomicI64::new(0)),
            buffer_ms,
            resampling_quality,
            voice,
            opener,
            runtime,
            events,
            active: Arc::new(Mutex::new(None)),
            frames: Arc::new(ring::FrameCounters::default()),
            next_generation: AtomicU64::new(1),
        }
    }

    fn send_to_pump(&self, command: PumpCommand) {
        if let Some(active) = lock(&self.active).as_ref() {
            signal_pump(&active.commands, &active.interrupt, command);
            // In steady playback the ring is usually full (decode outruns real
            // time), so the pump is usually parked in wait_for_space rather than
            // between packets — without this, a command sent there sits unseen for
            // up to COMMAND_POLL (100ms), which for Seek means up to 100ms of
            // playerUpdates reporting the pre-seek position before it catches up.
            active.ring.wake();
        }
    }

    fn report(&self, event: EngineEvent) {
        let Some(events) = self.events.get().cloned() else {
            return;
        };
        // try_send, not send: this runs on the pump thread, which must not block
        // on a busy actor.
        let _ = events.try_send(event);
    }

    /// Tears down whatever is playing.
    fn stop_active(&self) {
        let Some(previous) = lock(&self.active).take() else {
            return;
        };

        // Stop is the command that most needs signal_pump's ordering: without
        // it, a pump on a stalled source could burn its whole reconnect budget (up
        // to MAX_RECONNECT_ATTEMPTS × (connect_timeout + read_timeout), tens
        // of seconds) before it notices it was told to stop at all.
        //
        // Dropping the sender is what unblocks a pump parked on a full ring.
        signal_pump(&previous.commands, &previous.interrupt, PumpCommand::Stop);
        drop(previous.commands);

        // Before the replacement ring exists, so the reader this one leaves behind
        // inside songbird's Input cannot report the outgoing track's position
        // over the incoming track's.
        previous.ring.detach_position();

        if let Some(track) = previous.track {
            let _ = track.stop();
        }
    }
}

impl Engine for PipelineEngine {
    fn position_handle(&self) -> Arc<AtomicI64> {
        Arc::clone(&self.position_ms)
    }

    fn frame_counters(&self) -> Arc<ring::FrameCounters> {
        Arc::clone(&self.frames)
    }

    fn attach(&self, events: AsyncSender<EngineEvent>) {
        // Engine::attach's own contract: called once, at construction.
        let _ = self.events.set(events);
    }

    fn play(&self, request: PlayRequest) {
        self.stop_active();
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);

        let (writer, reader) = ring::channel(
            self.buffer_ms,
            Arc::clone(&self.position_ms),
            Arc::clone(&self.frames),
        );
        let (commands, pump_commands) = mpsc::channel();
        let interrupt = Arc::new(AtomicBool::new(false));

        lock(&self.active).replace(Active {
            commands,
            track: None,
            generation,
            paused: request.paused,
            interrupt: Arc::clone(&interrupt),
            ring: writer.clone(),
        });

        // The mixer's end of the ring, dressed as an input it understands. Raw f32
        // PCM at Discord's own rate, so nothing downstream resamples again.
        let input: Input = RawAdapter::new(reader, SAMPLE_RATE, CHANNELS as u32).into();

        {
            let voice = Arc::clone(&self.voice);
            let active = Arc::clone(&self.active);
            let guild_id = self.guild_id;
            self.runtime.spawn(async move {
                let handle = voice.play(input).await;

                // Store the handle only if this track is still current — by the
                // time the mixer takes the input, a newer play request may have
                // replaced it.
                //
                // paused is read from current, not a snapshot captured at the top
                // of play: Active::paused is the same field set_paused writes, so
                // whichever of the two runs last wins, instead of a stale value
                // from before the handle existed.
                let should_pause = {
                    let mut active = lock(&active);
                    match active.as_mut() {
                        Some(current) if current.generation == generation => {
                            current.track = Some(handle.clone());
                            Some(current.paused)
                        }
                        _ => None,
                    }
                };

                match should_pause {
                    Some(true) => {
                        let _ = handle.pause();
                    }
                    Some(false) => {}
                    None => {
                        let _ = handle.stop();
                        tracing::debug!(guild_id, "discarded a superseded input");
                    }
                }
            });
        }

        let position_ms = Arc::clone(&self.position_ms);
        let events = self.events.get().cloned();
        let guild_id = self.guild_id;
        let active = Arc::clone(&self.active);
        // Kept outside config so catch_unwind below can still read it after a
        // panic destroys run's own State — see PumpConfig::produced.
        let produced = Arc::new(AtomicBool::new(false));
        let config = PumpConfig {
            info: request.track.info.clone(),
            start_position_ms: request.start_position_ms,
            end_time_ms: request.end_time_ms,
            volume: request.volume,
            filters: request.filters.clone(),
            opener: Arc::clone(&self.opener),
            resampling_quality: self.resampling_quality,
            interrupt,
            produced: Arc::clone(&produced),
        };

        // A dedicated thread, not the blocking pool: this lives for the whole track.
        let spawned = std::thread::Builder::new()
            .name(format!("pump-{guild_id}"))
            .spawn(move || {
                // A Mutex<Instant> per decoded packet, just to throttle progress
                // events. Measured and left as-is: an uncontended lock is ~15ns
                // against the packet's own decode cost (tens of µs, see
                // benches/pipeline.rs) — not worth swapping for an AtomicU64 of
                // millis.
                let last_progress = Mutex::new(Instant::now());
                let on_progress = || {
                    let mut last = lock(&last_progress);
                    if last.elapsed() < PROGRESS_INTERVAL {
                        return;
                    }
                    *last = Instant::now();
                    drop(last);
                    // Gated the same way the terminal outcome below is, and for
                    // the same reason: a superseded pump keeps decoding until it
                    // observes Stop, and the actor treats Progress as "the
                    // current track is alive" — it restamps last_progress and
                    // clears stuck_reported (actor.rs's apply_engine_event). An
                    // ungated one therefore resets the replacement track's
                    // stuck clock, suppressing or delaying a TrackStuckEvent the
                    // new track had genuinely earned. Checked after the throttle,
                    // so this takes the lock once per PROGRESS_INTERVAL rather
                    // than once per decoded packet.
                    if !is_current(&active, generation) {
                        return;
                    }
                    if let Some(events) = &events {
                        let _ = events.try_send(EngineEvent::Progress);
                    }
                };

                // Contains a panic in this pump so it can't reach another player,
                // and still ends the track with a defined event. Cloned before the
                // move below so it survives the unwind: writer is consumed by
                // pump::run and gone once it panics, but this clone (same
                // Arc<Shared>) can still call finish() — the reader's only way to
                // learn the track ended if the event below never reaches the actor.
                // Engine events have their own queue now, so that is no longer the
                // routine case it once was, but try_send can still fail and the
                // reader must not be left waiting on a pump that no longer exists.
                let writer_after_panic = writer.clone();
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    pump::run(config, writer, pump_commands, position_ms, &on_progress)
                }));

                if outcome.is_err() {
                    writer_after_panic.finish();
                }

                let outcome = outcome.unwrap_or_else(|_| super::PumpOutcome::Failed {
                    exception: Exception::fault("The audio pipeline panicked", "pump panic"),
                    // Not hardcoded true: a panic inside pump::open, before any
                    // sample reached the ring, is a load failure — the actor turns
                    // started into loadFailed vs finished, and clients use that to
                    // decide whether to advance the queue. Read here because State
                    // (and its own copy) is already gone by the time the unwind
                    // reaches this point.
                    started: produced.load(Ordering::Relaxed),
                });

                if let Some(events) = &events {
                    let event = match outcome {
                        super::PumpOutcome::Finished => Some(EngineEvent::Finished),
                        super::PumpOutcome::Failed { exception, started } => {
                            Some(EngineEvent::Failed { exception, started })
                        }
                        // A stop was requested; the actor already knows.
                        super::PumpOutcome::Stopped => None,
                    };
                    // A superseded pump can still be mid-next_packet when Stop is
                    // sent, so it can reach EOF/error without ever seeing the
                    // command. Reporting that outcome unconditionally would end
                    // whatever track replaced it.
                    if let Some(event) = event {
                        if is_current(&active, generation) {
                            let _ = events.try_send(event);
                        }
                    }
                }
            });

        if let Err(error) = spawned {
            self.report(EngineEvent::Failed {
                exception: Exception::fault(
                    format!("Could not start the audio pipeline: {error}"),
                    error.to_string(),
                ),
                started: false,
            });
        }
    }

    fn stop(&self) {
        self.stop_active();
        self.position_ms.store(0, Ordering::Relaxed);

        let voice = Arc::clone(&self.voice);
        let active = Arc::clone(&self.active);
        self.runtime.spawn(async move {
            // Skipped once a play has already installed its replacement.
            //
            // Replace is stop then play (actor.rs's stop_track(Replaced) then
            // engine.play), each spawning a task that contends for the same voice
            // mutex with nothing ordering the two. songbird serializes both onto
            // one channel, so whichever task takes the lock last wins — and
            // tokio's LIFO slot makes the later spawn (play) run first, so this
            // stop task landing last is the common case, not a rare race. Left
            // unguarded, that order would leave the mixer with no track: the new
            // pump fills a ring nobody reads, and the player answers Playing with
            // a frozen position until check_stuck fires.
            //
            // play installs active synchronously before spawning, and the actor
            // issues both calls from one handle() that never awaits in between,
            // so a superseded stop always observes Some here — skipping is
            // correct, not just safe: play_only_input is itself a replace, and
            // stop_active already stopped the outgoing track's handle directly.
            if superseded_by_a_replacement(&active) {
                return;
            }
            voice.stop().await;
        });
    }

    /// Pausing is the mixer's job.
    ///
    /// Stopping the pump instead would only stop decoding; the ring would keep
    /// feeding buffered audio for another few seconds. Pausing the mixer stops the
    /// pull, which stalls the pump on a full ring and freezes the position counter,
    /// because the counter advances on consumption. All three follow from the one
    /// call.
    fn set_paused(&self, paused: bool) {
        let track = {
            let mut active = lock(&self.active);
            let Some(current) = active.as_mut() else { return };
            current.paused = paused;
            current.track.clone()
        };
        let Some(track) = track else { return };

        let _ = if paused { track.pause() } else { track.play() };
    }

    fn seek(&self, position_ms: i64) {
        // Announced before the command is even sent, mirroring lavaplayer's own
        // LocalAudioTrackExecutor.setPosition, which sets queuedSeek
        // synchronously on the calling thread — so position reporting holds at
        // the target from this call's return, not from whenever the pump gets
        // around to the command. See RingWriter::begin_seek's docs.
        if let Some(active) = lock(&self.active).as_ref() {
            active.ring.begin_seek(position_ms);
        }
        self.send_to_pump(PumpCommand::Seek { position_ms });
    }

    fn set_volume(&self, volume: i32) {
        self.send_to_pump(PumpCommand::SetVolume(volume));
    }

    fn set_filters(&self, filters: &Filters) {
        self.send_to_pump(PumpCommand::SetFilters(Box::new(filters.clone())));
    }

    fn set_end_time(&self, end_time_ms: Option<i64>) {
        self.send_to_pump(PumpCommand::SetEndTime(end_time_ms));
    }

    fn shutdown(&self) {
        self.stop_active();
        let voice = Arc::clone(&self.voice);
        self.runtime.spawn(async move { voice.leave().await });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_active(generation: u64) -> Active {
        let (commands, _rx) = mpsc::channel();
        let (ring, _reader) = ring::channel(
            20,
            Arc::new(AtomicI64::new(0)),
            Arc::new(ring::FrameCounters::default()),
        );
        Active {
            commands,
            track: None,
            generation,
            paused: false,
            interrupt: Arc::new(AtomicBool::new(false)),
            ring,
        }
    }

    /// The bug: a pump superseded by a track replace can still race a natural
    /// EOF or decode error and report its outcome after a new pump has already
    /// taken over active. is_current is what play's terminal-outcome
    /// dispatch checks before sending that outcome on — this must reject a
    /// generation that is no longer the one active holds.
    #[test]
    fn a_superseded_generation_is_not_current() {
        let active = Mutex::new(Some(dummy_active(2)));
        assert!(is_current(&active, 2));
        assert!(!is_current(&active, 1));
    }

    #[test]
    fn nothing_is_current_once_active_is_cleared() {
        let active: Mutex<Option<Active>> = Mutex::new(None);
        assert!(!is_current(&active, 1));
    }

    /// The bug: a track replace is stop then play, each spawning a task onto
    /// the same voice mutex with nothing ordering them. songbird turns both into
    /// SetTrack, so a stop that lands after the play — the order tokio's
    /// LIFO slot makes usual — leaves the mixer with no track and the player
    /// silently Playing. A stop that finds active filled in has been
    /// superseded and must not reach the mixer; a plain stop leaves active
    /// cleared and must.
    #[test]
    fn a_stop_superseded_by_a_replacement_does_not_reach_the_mixer() {
        let active: Mutex<Option<Active>> = Mutex::new(None);
        assert!(!superseded_by_a_replacement(&active));

        // What play installs, synchronously, before it spawns anything.
        *lock(&active) = Some(dummy_active(1));
        assert!(superseded_by_a_replacement(&active));
    }

    /// The bug this guards: deriving the next generation from the one being torn
    /// down (stop_active().wrapping_add(1)) returns 0 once active is already
    /// None — a full stop before the replacement track starts — so the next
    /// play reused generation 1. A pump still parked on a stalled source from
    /// the first generation-1 track would then pass is_current for whatever
    /// later track happened to land on generation 1 again.
    #[test]
    fn a_generation_survives_a_full_stop_and_does_not_restart_at_one() {
        let next_generation = AtomicU64::new(1);
        let active: Mutex<Option<Active>> = Mutex::new(None);

        let first = next_generation.fetch_add(1, Ordering::Relaxed);
        *lock(&active) = Some(dummy_active(first));

        // The pump for first is stuck retrying a stalled source and never
        // observes the stop; active is cleared out from under it anyway.
        *lock(&active) = None;

        let second = next_generation.fetch_add(1, Ordering::Relaxed);
        *lock(&active) = Some(dummy_active(second));

        assert_ne!(second, first);
        assert!(!is_current(&active, first));
        assert!(is_current(&active, second));
    }

    /// The bug this guards: play used to hardcode started: true in the
    /// catch_unwind recovery arm, so a panic during pump::open, before a single
    /// sample reached the ring, was reported as finished rather than
    /// loadFailed — exactly the distinction actor.rs uses to decide whether a
    /// client should advance its queue. produced is read after catch_unwind
    /// instead now, exercised here in isolation (the same shape play's spawned
    /// closure uses) since driving a real panic through a live pump thread and
    /// tokio runtime is what test-bot, not a unit test, is for.
    #[test]
    fn a_panic_before_producing_anything_is_reported_as_not_started() {
        let produced = Arc::new(AtomicBool::new(false));
        let outcome = recover_from_panic(&produced, || panic!("simulated load-time panic"));
        assert!(matches!(outcome, crate::audio::PumpOutcome::Failed { started: false, .. }));
    }

    #[test]
    fn a_panic_after_producing_something_is_reported_as_started() {
        let produced = Arc::new(AtomicBool::new(false));
        produced.store(true, Ordering::Relaxed);
        let outcome = recover_from_panic(&produced, || panic!("simulated mid-track panic"));
        assert!(matches!(outcome, crate::audio::PumpOutcome::Failed { started: true, .. }));
    }

    /// The bug this guards: the panic-recovery arm used to leave the ring open
    /// with neither finish() nor the terminal event delivered (simulated here
    /// by never sending one), so the reader would starve forever instead of
    /// ever reaching EOF. A clone of the writer taken before the panicking call
    /// must still be able to close it out afterwards.
    #[test]
    fn a_panicked_pump_still_finishes_the_ring_it_abandoned() {
        use std::io::Read as _;

        let (writer, mut reader) = ring::channel(
            20,
            Arc::new(AtomicI64::new(0)),
            Arc::new(ring::FrameCounters::default()),
        );
        let writer_after_panic = writer.clone();

        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(writer);
            panic!("simulated mid-track panic");
        }));
        assert!(outcome.is_err());
        writer_after_panic.finish();

        let mut out = [0u8; 4];
        assert_eq!(reader.read(&mut out).unwrap(), 0);
    }

    /// The exact recovery shape play's spawned closure uses around pump::run.
    fn recover_from_panic(
        produced: &Arc<AtomicBool>,
        pump: impl FnOnce() -> crate::audio::PumpOutcome + std::panic::UnwindSafe,
    ) -> crate::audio::PumpOutcome {
        std::panic::catch_unwind(pump).unwrap_or_else(|_| crate::audio::PumpOutcome::Failed {
            exception: Exception::fault("panicked", "panicked"),
            started: produced.load(Ordering::Relaxed),
        })
    }
}
