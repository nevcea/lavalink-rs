//! The pipeline, assembled.
//!
//! ```text
//! pump thread                          ring                songbird mixer
//! ───────────                          ────                ──────────────
//! decode → resample → filter ──write──▶ ... ──RawAdapter──▶ pull every 20ms
//!                                                           → Opus → Discord
//! ```
//!
//! Two things about the right-hand side are not choices we get to make:
//!
//! * **The mixer pulls.** There is no API for handing it finished frames; it reads
//!   from a `MediaSource` on its own clock. So the ring's read end is what we expose.
//! * **The mixer encodes.** Opus passthrough — forwarding a pre-encoded stream
//!   untouched — requires track volume to be exactly 1.0, and `volume` is a filter
//!   we have to support. So passthrough is off and encoding is songbird's.
//!
//! Which leaves seeking as ours to implement, which is [`pump`]'s job.
//!
//! # Where the boundaries are
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
use crate::player::{Command, EventSlot};
use crate::voice::SharedVoice;

/// How often the pump reports that it is still producing. Throttled hard: this only
/// has to beat `trackStuckThresholdMs`, and the actor's queue is small.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(500);

pub struct PipelineEngine {
    guild_id: u64,
    position_ms: Arc<AtomicI64>,
    buffer_ms: u32,
    resampling_quality: ResamplingQuality,
    voice: SharedVoice,
    opener: Arc<StreamOpener>,
    runtime: tokio::runtime::Handle,
    /// Shared with the voice connection, which reports into the same actor.
    events: EventSlot,
    /// Shared so the task that hands the input to the mixer can store the resulting
    /// handle back, which is what `pause` later needs.
    active: Arc<Mutex<Option<Active>>>,
    /// One per player, not one per track: see [`ring::FrameCounters`]'s docs.
    frames: Arc<ring::FrameCounters>,
    /// The next generation to hand out. Lives on the engine, not derived from
    /// whatever `stop_active` tears down — a full stop clears `active` to `None`,
    /// so deriving the next generation from the outgoing one would restart the
    /// count at 1 after every stop, letting a pump parked on a stalled source
    /// (still holding the old generation) pass `is_current` for a later,
    /// unrelated track that happens to reuse it.
    next_generation: AtomicU64,
}

/// A running track.
struct Active {
    /// Commands to the pump thread. Dropping it stops the pump.
    commands: Sender<PumpCommand>,
    /// Set alongside every send on `commands`, so a pump stuck retrying a
    /// stalled HTTP source (`stream.rs`'s `interrupt`) notices a command is
    /// waiting and gives up its remaining retry budget instead of making the
    /// command wait out the whole thing.
    interrupt: Arc<AtomicBool>,
    /// The songbird side, so pause and stop reach the mixer.
    track: Option<TrackHandle>,
    /// Bumped on every new track; checked by both the handle-storing task and the
    /// terminal-outcome dispatch (`play`, below) via [`is_current`] so a late
    /// outcome from a superseded pump is ignored rather than applied to whatever
    /// track replaced it.
    generation: u64,
    /// Desired pause state. Source of truth for both `play`'s spawned task (once
    /// `track` is filled in) and `set_paused` (whenever it runs) — whichever of the
    /// two runs last is what actually gets applied to the handle, instead of a
    /// value snapshotted before the handle existed.
    paused: bool,
}

/// Whether `generation` is still the one `active` currently holds. `Active`'s
/// pump-thread `commands` sender is dropped to signal a stop, but the pump can be
/// mid-`next_packet()` at that moment and reach a terminal outcome (a natural EOF,
/// a decode error) without ever observing it — so a stale outcome from a
/// superseded pump can still arrive after a new one has taken over.
fn is_current(active: &Mutex<Option<Active>>, generation: u64) -> bool {
    lock(active)
        .as_ref()
        .is_some_and(|current| current.generation == generation)
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
            // Sent before the flag is set, not after: `drain_commands`'s `Empty`
            // branch clears `interrupt` once it finds nothing to act on, and a
            // channel `send` is visible to any `try_recv` that follows it. Setting
            // the flag first left a window where `drain_commands` could observe
            // `Empty` (this send hadn't landed yet), clear the flag, and then the
            // command would arrive with nothing left to tell a stalled source a
            // command was waiting — reinstating the full reconnect stall the flag
            // exists to cut short.
            let _ = active.commands.send(command);
            active.interrupt.store(true, Ordering::Relaxed);
        }
    }

    fn report(&self, event: EngineEvent) {
        let Some(events) = self.events.get().cloned() else {
            return;
        };
        // `try_send`, not `send`: this runs on the pump thread, which must not block
        // on a busy actor.
        let _ = events.try_send(Command::Engine(event));
    }

    /// Tears down whatever is playing.
    fn stop_active(&self) {
        let Some(previous) = lock(&self.active).take() else {
            return;
        };

        previous.interrupt.store(true, Ordering::Relaxed);
        // Dropping the sender is what unblocks a pump parked on a full ring.
        let _ = previous.commands.send(PumpCommand::Stop);
        drop(previous.commands);

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

    fn attach(&self, events: AsyncSender<Command>) {
        // `Engine::attach`'s own contract: called once, at construction.
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

                // Store the handle only if this track is still the current one: by
                // the time the mixer has taken the input, a newer play request may
                // already have replaced it.
                //
                // `paused` is read from `current` here, not from a `request.paused`
                // captured at the top of `play` — `Active::paused` is the same field
                // `set_paused` writes, so whichever of the two runs last (a separate
                // `paused`-only patch landing before this task gets to run, or this
                // task completing first) is what gets applied, instead of a snapshot
                // from before the handle even existed.
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
        // Kept outside config too (not just inside it) so the catch_unwind below
        // can still read it after a panic has destroyed run's own State — that is
        // the whole reason this exists rather than staying a plain bool on State,
        // see PumpConfig::produced.
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
                // A `Mutex<Instant>` taken on every decoded packet just to throttle
                // progress events — measured and left alone rather than swapped for
                // an `AtomicU64` of millis. An uncontended lock is ~15ns against the
                // packet's own decode cost (tens of µs, see
                // `crates/server/benches/pipeline.rs`), so it is not on the list of
                // things worth the churn.
                let last_progress = Mutex::new(Instant::now());
                let on_progress = || {
                    let mut last = lock(&last_progress);
                    if last.elapsed() < PROGRESS_INTERVAL {
                        return;
                    }
                    *last = Instant::now();
                    drop(last);
                    if let Some(events) = &events {
                        let _ = events.try_send(Command::Engine(EngineEvent::Progress));
                    }
                };

                // A panic in one pump is contained here. It cannot be recovered
                // from, but it must not reach another player, and it should end the
                // track with a defined event rather than silence. That last part is
                // not guaranteed by this alone: the try_send below is best-effort
                // (a full command queue drops the event silently), and unlike a
                // normal Finished/Failed return this path never calls
                // writer.finish() on the ring it is abandoning, so the reader keeps
                // handing the mixer silence and advancing position forever if the
                // event above is what gets lost.
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    pump::run(config, writer, pump_commands, position_ms, &on_progress)
                }));

                let outcome = outcome.unwrap_or_else(|_| super::PumpOutcome::Failed {
                    exception: Exception::fault("The audio pipeline panicked", "pump panic"),
                    // Not hardcoded true: a panic inside pump::open, before a
                    // single sample reached the ring, is a load failure, and the
                    // actor turns started into loadFailed vs finished — clients
                    // use exactly that distinction to decide whether to advance
                    // the queue. produced is read here, after State (and its own
                    // copy of the flag) has already been destroyed by the unwind,
                    // which is why it has to live outside State at all.
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
                    // A pump superseded by a replace can still be mid-`next_packet`
                    // when `Stop` is sent, so it can reach a terminal outcome
                    // (natural EOF or a decode error) without ever seeing the
                    // command. Reporting that outcome unconditionally would end
                    // whatever track replaced this one, not the track that
                    // actually produced it.
                    if let Some(event) = event {
                        if is_current(&active, generation) {
                            let _ = events.try_send(Command::Engine(event));
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
        self.runtime.spawn(async move { voice.stop().await });
    }

    /// Pausing is the mixer's job.
    ///
    /// Stopping the pump instead would only stop *decoding*; the ring would keep
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
        Active {
            commands,
            track: None,
            generation,
            paused: false,
            interrupt: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The bug: a pump superseded by a track replace can still race a natural
    /// EOF or decode error and report its outcome after a new pump has already
    /// taken over `active`. `is_current` is what `play`'s terminal-outcome
    /// dispatch checks before sending that outcome on — this must reject a
    /// generation that is no longer the one `active` holds.
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

    /// The bug this guards: deriving the next generation from the one being torn
    /// down (`stop_active().wrapping_add(1)`) returns 0 once `active` is already
    /// `None` — a full stop before the replacement track starts — so the next
    /// `play` reused generation 1. A pump still parked on a stalled source from
    /// the *first* generation-1 track would then pass `is_current` for whatever
    /// later track happened to land on generation 1 again.
    #[test]
    fn a_generation_survives_a_full_stop_and_does_not_restart_at_one() {
        let next_generation = AtomicU64::new(1);
        let active: Mutex<Option<Active>> = Mutex::new(None);

        let first = next_generation.fetch_add(1, Ordering::Relaxed);
        *lock(&active) = Some(dummy_active(first));

        // The pump for `first` is stuck retrying a stalled source and never
        // observes the stop; `active` is cleared out from under it anyway.
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
