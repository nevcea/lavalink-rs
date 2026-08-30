//! The pipeline, assembled.
//!
//! unfiltered YouTube/WebM ────────────────────────────────▶ Opus passthrough
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
//! • Songbird forwards WebM/Opus untouched while there is one track at unity
//!   volume. Other player volumes make Songbird decode temporarily; enabling a
//!   Lavalink filter switches the track once to the PCM pump at its live position.
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
use songbird::events::{Event, EventContext, EventData, EventHandler, TrackEvent};
use songbird::input::{Input, RawAdapter};
use songbird::tracks::{PlayError, PlayMode, Track, TrackHandle, TrackResult, TrackState};
use tokio::sync::mpsc::Sender as AsyncSender;

use super::filter::player_volume_multiplier;
use super::pump::{self, PumpCommand, PumpConfig};
use super::ring::{self, CHANNELS, SAMPLE_RATE};
use super::stream::StreamOpener;
use super::{Engine, EngineEvent, EngineReport, PlayRequest};
use crate::config::ResamplingQuality;
use crate::lock;
use crate::player::EventSlot;
use crate::voice::SharedVoice;

/// How often the pump reports that it is still producing. Throttled hard: this only
/// has to beat trackStuckThresholdMs, and the actor's queue is small.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(500);
const DIRECT_POSITION_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Clone)]
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
    next_generation: Arc<AtomicU64>,
}

/// A running track.
struct Active {
    /// The songbird side, so pause and stop reach the mixer.
    track: Option<TrackHandle>,
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
    mode: ActiveMode,
}

enum ActiveMode {
    Pump {
        /// Commands to the pump thread. Dropping it stops the pump.
        commands: Sender<PumpCommand>,
        /// Set alongside every command so stalled source I/O yields promptly.
        interrupt: Arc<AtomicBool>,
        /// Revokes this ring's claim on the shared position counter on stop.
        ring: ring::RingWriter,
    },
    Direct(DirectState),
    /// A filter was enabled and the exact Songbird position is being captured
    /// before the existing PCM pump replaces the direct input.
    Transitioning(DirectState),
}

#[derive(Clone)]
struct DirectState {
    request: Box<PlayRequest>,
    end_schedule: Arc<AtomicU64>,
    pending_seek_ms: Arc<AtomicI64>,
    seek_serial: u64,
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
fn signal_pump(
    commands: &Sender<PumpCommand>,
    interrupt: &AtomicBool,
    command: PumpCommand,
    guild_id: u64,
    generation: u64,
) {
    if let Err(error) = commands.send(command) {
        tracing::warn!(
            guild_id,
            generation,
            error = ?error,
            "could not deliver a pump command"
        );
    }
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

fn log_track_control(
    active: &Mutex<Option<Active>>,
    guild_id: u64,
    generation: u64,
    action: &'static str,
    result: TrackResult<()>,
) {
    let Err(error) = result else { return };
    if is_current(active, generation) {
        tracing::warn!(
            guild_id,
            generation,
            action,
            error_debug = ?error,
            error_display = %error,
            "track control failed"
        );
    } else {
        tracing::debug!(
            guild_id,
            generation,
            action,
            error_debug = ?error,
            error_display = %error,
            "stale track control failed"
        );
    }
}

fn is_current_direct(active: &Mutex<Option<Active>>, generation: u64) -> bool {
    lock(active).as_ref().is_some_and(|current| {
        current.generation == generation
            && matches!(current.mode, ActiveMode::Direct(_) | ActiveMode::Transitioning(_))
    })
}

fn can_start_direct(active: &Mutex<Option<Active>>, generation: u64) -> bool {
    lock(active).as_ref().is_some_and(|current| {
        current.generation == generation && matches!(current.mode, ActiveMode::Direct(_))
    })
}

fn direct_path_eligible(request: &PlayRequest) -> bool {
    request.track.info.source_name == "youtube" && request.filters == Filters::default()
}

/// Reliable delivery happens off the actor thread, so waiting for the actor is
/// safe here and prevents a full report queue from dropping ordered events.
/// Progress reports remain deliberately lossy.
fn send_reliable(events: &AsyncSender<EngineReport>, report: EngineReport) {
    let generation = report.generation;
    if events.blocking_send(report).is_err() {
        tracing::debug!(generation, "engine report receiver closed");
    }
}

fn duration_ms(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

fn direct_exception(error: &PlayError) -> Exception {
    let message = error.to_string();
    if matches!(error, PlayError::Decode(_)) {
        Exception::fault(message.clone(), message)
    } else {
        Exception::common(message.clone(), message)
    }
}

struct DirectProgress {
    active: Arc<Mutex<Option<Active>>>,
    events: Option<AsyncSender<EngineReport>>,
    position_ms: Arc<AtomicI64>,
    pending_seek_ms: Arc<AtomicI64>,
    frames: Arc<ring::FrameCounters>,
    counted_play_time_ms: Arc<AtomicU64>,
    last_report_ms: AtomicU64,
    generation: u64,
}

impl DirectProgress {
    fn update(&self, state: &TrackState) {
        if self.pending_seek_ms.load(Ordering::Relaxed) == -1 {
            self.position_ms
                .store(duration_ms(state.position), Ordering::Relaxed);
        }

        let now = state.play_time.as_millis() as u64;
        let previous = self.counted_play_time_ms.swap(now, Ordering::Relaxed);
        let frames = now.saturating_sub(previous) / 20;
        if let Ok(frames) = u32::try_from(frames) {
            self.frames.record_sent_frames(frames);
        }
    }
}

#[async_trait::async_trait]
impl EventHandler for DirectProgress {
    async fn act(&self, context: &EventContext<'_>) -> Option<Event> {
        let EventContext::Track(&[(state, _)]) = context else {
            return None;
        };
        if !is_current_direct(&self.active, self.generation) {
            return Some(Event::Cancel);
        }

        self.update(state);
        let now = state.play_time.as_millis() as u64;
        let previous = self.last_report_ms.load(Ordering::Relaxed);
        if now.saturating_sub(previous) >= PROGRESS_INTERVAL.as_millis() as u64 {
            self.last_report_ms.store(now, Ordering::Relaxed);
            if let Some(events) = &self.events {
                let _ = events.try_send(EngineReport {
                    generation: self.generation,
                    event: EngineEvent::Progress,
                });
            }
        }
        None
    }
}

struct DirectTerminal {
    active: Arc<Mutex<Option<Active>>>,
    events: Option<AsyncSender<EngineReport>>,
    position_ms: Arc<AtomicI64>,
    pending_seek_ms: Arc<AtomicI64>,
    frames: Arc<ring::FrameCounters>,
    counted_play_time_ms: Arc<AtomicU64>,
    generation: u64,
    on_error: bool,
}

impl DirectTerminal {
    fn update(&self, state: &TrackState) {
        if self.pending_seek_ms.load(Ordering::Relaxed) == -1 {
            self.position_ms
                .store(duration_ms(state.position), Ordering::Relaxed);
        }
        let now = state.play_time.as_millis() as u64;
        let previous = self.counted_play_time_ms.swap(now, Ordering::Relaxed);
        let frames = now.saturating_sub(previous) / 20;
        if let Ok(frames) = u32::try_from(frames) {
            self.frames.record_sent_frames(frames);
        }
    }

    async fn send(&self, event: EngineEvent) {
        if let Some(events) = &self.events {
            if events
                .send(EngineReport {
                    generation: self.generation,
                    event,
                })
                .await
                .is_err()
            {
                tracing::debug!(generation = self.generation, "engine report receiver closed");
            }
        }
    }
}

#[async_trait::async_trait]
impl EventHandler for DirectTerminal {
    async fn act(&self, context: &EventContext<'_>) -> Option<Event> {
        let EventContext::Track(&[(state, _)]) = context else {
            return None;
        };
        if !is_current_direct(&self.active, self.generation) {
            return Some(Event::Cancel);
        }

        self.update(state);
        match (&state.playing, self.on_error) {
            (PlayMode::Errored(error), true) => {
                self.send(EngineEvent::Exception {
                    exception: direct_exception(error),
                })
                .await;
                self.send(if state.play_time.is_zero() {
                    EngineEvent::LoadFailed
                } else {
                    EngineEvent::Finished
                })
                .await;
            }
            (PlayMode::End | PlayMode::Stop, false) => self.send(EngineEvent::Finished).await,
            _ => {}
        }
        None
    }
}

struct DirectEndTime {
    active: Arc<Mutex<Option<Active>>>,
    schedule: Arc<AtomicU64>,
    schedule_id: u64,
    guild_id: u64,
    generation: u64,
}

#[async_trait::async_trait]
impl EventHandler for DirectEndTime {
    async fn act(&self, context: &EventContext<'_>) -> Option<Event> {
        if self.schedule.load(Ordering::Relaxed) != self.schedule_id
            || !is_current_direct(&self.active, self.generation)
        {
            return None;
        }
        if let EventContext::Track(&[(_, handle)]) = context {
            log_track_control(
                &self.active,
                self.guild_id,
                self.generation,
                "end_time_stop",
                handle.stop(),
            );
        }
        None
    }
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
            next_generation: Arc::new(AtomicU64::new(1)),
        }
    }

    fn start_direct(&self, generation: u64, request: PlayRequest, input: Input) {
        let end_schedule = Arc::new(AtomicU64::new(0));
        let pending_seek_ms = Arc::new(AtomicI64::new(-1));
        let counted_play_time_ms = Arc::new(AtomicU64::new(0));

        // Install controls before allowing the lazy source to produce its first
        // frame; a non-zero start position must never leak audio from time zero.
        let mut track = Track::from(input)
            .volume(player_volume_multiplier(request.volume))
            .pause();

        track.events.add_event(
            EventData::new(
                Event::Periodic(DIRECT_POSITION_INTERVAL, None),
                DirectProgress {
                    active: Arc::clone(&self.active),
                    events: self.events.get().cloned(),
                    position_ms: Arc::clone(&self.position_ms),
                    pending_seek_ms: Arc::clone(&pending_seek_ms),
                    frames: Arc::clone(&self.frames),
                    counted_play_time_ms: Arc::clone(&counted_play_time_ms),
                    last_report_ms: AtomicU64::new(0),
                    generation,
                },
            ),
            Duration::ZERO,
        );
        for (event, on_error) in [
            (TrackEvent::Error, true),
            (TrackEvent::End, false),
        ] {
            track.events.add_event(
                EventData::new(
                    Event::Track(event),
                    DirectTerminal {
                        active: Arc::clone(&self.active),
                        events: self.events.get().cloned(),
                        position_ms: Arc::clone(&self.position_ms),
                        pending_seek_ms: Arc::clone(&pending_seek_ms),
                        frames: Arc::clone(&self.frames),
                        counted_play_time_ms: Arc::clone(&counted_play_time_ms),
                        generation,
                        on_error,
                    },
                ),
                Duration::ZERO,
            );
        }

        lock(&self.active).replace(Active {
            track: None,
            generation,
            paused: request.paused,
            mode: ActiveMode::Direct(DirectState {
                request: Box::new(request),
                end_schedule: Arc::clone(&end_schedule),
                pending_seek_ms: Arc::clone(&pending_seek_ms),
                seek_serial: 0,
            }),
        });

        let engine = self.clone();
        let voice = Arc::clone(&self.voice);
        let guild_id = self.guild_id;
        self.runtime.spawn(async move {
            let Some(handle) = voice
                .play_track_if(track, || can_start_direct(&engine.active, generation))
                .await
            else {
                tracing::debug!(guild_id, "discarded a superseded direct input");
                return;
            };

            let state = {
                let mut active = lock(&engine.active);
                match active.as_mut() {
                    Some(current)
                        if current.generation == generation
                            && matches!(current.mode, ActiveMode::Direct(_)) =>
                    {
                        current.track = Some(handle.clone());
                        let ActiveMode::Direct(direct) = &current.mode else {
                            unreachable!()
                        };
                        Some((
                            current.paused,
                            direct.request.volume,
                            direct.request.start_position_ms,
                            direct.request.end_time_ms,
                            Arc::clone(&direct.end_schedule),
                            Arc::clone(&direct.pending_seek_ms),
                            direct.seek_serial,
                        ))
                    }
                    _ => None,
                }
            };

            let Some((paused, volume, position, end_time, schedule, pending, serial)) = state
            else {
                log_track_control(
                    &engine.active,
                    guild_id,
                    generation,
                    "superseded_stop",
                    handle.stop(),
                );
                tracing::debug!(guild_id, "discarded a superseded direct input");
                return;
            };

            log_track_control(
                &engine.active,
                guild_id,
                generation,
                "set_volume",
                handle.set_volume(player_volume_multiplier(volume)),
            );
            if position > 0 || serial > 0 {
                engine.seek_direct(
                    handle.clone(),
                    generation,
                    serial,
                    position,
                    pending,
                );
            } else if paused {
                // A paused direct track should still resolve and parse now, like
                // the PCM pump filling its buffer while the mixer is paused.
                let _ = handle.make_playable();
            } else {
                log_track_control(
                    &engine.active,
                    guild_id,
                    generation,
                    "play",
                    handle.play(),
                );
            }
            engine.schedule_direct_end(handle, generation, end_time, position, schedule);
        });
    }

    fn seek_direct(
        &self,
        handle: TrackHandle,
        generation: u64,
        serial: u64,
        position_ms: i64,
        pending_seek_ms: Arc<AtomicI64>,
    ) {
        pending_seek_ms.store(position_ms, Ordering::Relaxed);
        let active = Arc::clone(&self.active);
        let position = Arc::clone(&self.position_ms);
        let guild_id = self.guild_id;
        self.runtime.spawn(async move {
            let result = handle
                .seek_async(Duration::from_millis(position_ms.max(0) as u64))
                .await;
            let current = lock(&active).as_ref().is_some_and(|current| {
                current.generation == generation
                    && match &current.mode {
                        ActiveMode::Direct(direct) | ActiveMode::Transitioning(direct) => {
                            direct.seek_serial == serial
                        }
                        ActiveMode::Pump { .. } => false,
                    }
            });
            if !current {
                return;
            }
            pending_seek_ms.store(-1, Ordering::Relaxed);
            match result {
                Ok(actual) => position.store(duration_ms(actual), Ordering::Relaxed),
                Err(error) => tracing::warn!(
                    guild_id,
                    generation,
                    position_ms,
                    error_debug = ?error,
                    error_display = %error,
                    "direct track seek failed"
                ),
            }
            let paused = lock(&active)
                .as_ref()
                .is_some_and(|current| current.generation == generation && current.paused);
            if !paused {
                log_track_control(
                    &active,
                    guild_id,
                    generation,
                    "play_after_seek",
                    handle.play(),
                );
            }
        });
    }

    fn schedule_direct_end(
        &self,
        handle: TrackHandle,
        generation: u64,
        end_time_ms: Option<i64>,
        position_ms: i64,
        schedule: Arc<AtomicU64>,
    ) {
        let schedule_id = schedule.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        let Some(end_time_ms) = end_time_ms else { return };
        let delay = Duration::from_millis(end_time_ms.saturating_sub(position_ms).max(0) as u64);
        log_track_control(
            &self.active,
            self.guild_id,
            generation,
            "schedule_end_time",
            handle.add_event(
                Event::Delayed(delay),
                DirectEndTime {
                    active: Arc::clone(&self.active),
                    schedule,
                    schedule_id,
                    guild_id: self.guild_id,
                    generation,
                },
            ),
        );
    }

    fn start_pump(&self, generation: u64, request: PlayRequest, transition: bool) -> bool {
        let (writer, reader) = ring::channel(
            self.buffer_ms,
            Arc::clone(&self.position_ms),
            Arc::clone(&self.frames),
        );
        let (commands, pump_commands) = mpsc::channel();
        let interrupt = Arc::new(AtomicBool::new(false));

        {
            let mut active = lock(&self.active);
            if transition {
                let Some(current) = active.as_ref() else { return false };
                if current.generation != generation
                    || !matches!(current.mode, ActiveMode::Transitioning(_))
                {
                    return false;
                }
                if let ActiveMode::Transitioning(direct) = &current.mode {
                    direct.end_schedule.fetch_add(1, Ordering::Relaxed);
                }
            } else if active.is_some() {
                return false;
            }

            active.replace(Active {
                track: None,
                generation,
                paused: request.paused,
                mode: ActiveMode::Pump {
                    commands,
                    interrupt: Arc::clone(&interrupt),
                    ring: writer.clone(),
                },
            });
        }

        // The mixer's end of the ring, dressed as an input it understands. Raw f32
        // PCM at Discord's own rate, so nothing downstream resamples again.
        let input: Input = RawAdapter::new(reader, SAMPLE_RATE, CHANNELS as u32).into();

        {
            let voice = Arc::clone(&self.voice);
            let active = Arc::clone(&self.active);
            let guild_id = self.guild_id;
            self.runtime.spawn(async move {
                let Some(handle) = voice
                    .play_if(input, || is_current(&active, generation))
                    .await
                else {
                    tracing::debug!(guild_id, "discarded a superseded input");
                    return;
                };

                let should_pause = {
                    let mut active = lock(&active);
                    match active.as_mut() {
                        Some(current)
                            if current.generation == generation
                                && matches!(current.mode, ActiveMode::Pump { .. }) =>
                        {
                            current.track = Some(handle.clone());
                            Some(current.paused)
                        }
                        _ => None,
                    }
                };

                match should_pause {
                    Some(true) => {
                        log_track_control(
                            &active,
                            guild_id,
                            generation,
                            "pause",
                            handle.pause(),
                        );
                    }
                    Some(false) => {}
                    None => {
                        log_track_control(
                            &active,
                            guild_id,
                            generation,
                            "superseded_stop",
                            handle.stop(),
                        );
                        tracing::debug!(guild_id, "discarded a superseded input");
                    }
                }
            });
        }

        let position_ms = Arc::clone(&self.position_ms);
        let events = self.events.get().cloned();
        let guild_id = self.guild_id;
        let active = Arc::clone(&self.active);
        let produced = Arc::new(AtomicBool::new(false));
        let config = PumpConfig {
            info: request.track.info,
            start_position_ms: request.start_position_ms,
            end_time_ms: request.end_time_ms,
            volume: request.volume,
            filters: request.filters,
            opener: Arc::clone(&self.opener),
            resampling_quality: self.resampling_quality,
            interrupt,
            produced: Arc::clone(&produced),
        };

        let spawned = std::thread::Builder::new()
            .name(format!("pump-{guild_id}"))
            .spawn(move || {
                let last_progress = Mutex::new(Instant::now());
                let on_progress = || {
                    let mut last = lock(&last_progress);
                    if last.elapsed() < PROGRESS_INTERVAL {
                        return;
                    }
                    *last = Instant::now();
                    drop(last);
                    if !is_current(&active, generation) {
                        return;
                    }
                    if let Some(events) = &events {
                        let _ = events.try_send(EngineReport {
                            generation,
                            event: EngineEvent::Progress,
                        });
                    }
                };

                let writer_after_panic = writer.clone();
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    pump::run(config, writer, pump_commands, position_ms, &on_progress)
                }));

                if outcome.is_err() {
                    writer_after_panic.finish();
                }

                let outcome = outcome.unwrap_or_else(|_| super::PumpOutcome::Failed {
                    exception: Exception::fault("The audio pipeline panicked", "pump panic"),
                    started: produced.load(Ordering::Relaxed),
                });

                if let Some(events) = &events {
                    match outcome {
                        super::PumpOutcome::Finished => {
                            if is_current(&active, generation) {
                                send_reliable(events, EngineReport {
                                    generation,
                                    event: EngineEvent::Finished,
                                });
                            }
                        }
                        super::PumpOutcome::Failed { exception, started } => {
                            if is_current(&active, generation) {
                                send_reliable(events, EngineReport {
                                    generation,
                                    event: EngineEvent::Exception { exception },
                                });
                            }
                            writer_after_panic.finish();
                            while started
                                && is_current(&active, generation)
                                && !writer_after_panic.wait_for_drain(PROGRESS_INTERVAL)
                            {
                                if writer_after_panic.is_closed() {
                                    break;
                                }
                            }
                            if is_current(&active, generation) {
                                send_reliable(events, EngineReport {
                                    generation,
                                    event: if started {
                                        EngineEvent::Finished
                                    } else {
                                        EngineEvent::LoadFailed
                                    },
                                });
                            }
                        }
                        super::PumpOutcome::Stopped => {}
                    }
                }
            });

        if let Err(error) = spawned {
            self.report(
                generation,
                EngineEvent::StartFailed {
                    exception: Exception::fault(
                        format!("Could not start the audio pipeline: {error}"),
                        error.to_string(),
                    ),
                },
            );
        }
        true
    }

    fn finish_direct_transition(
        &self,
        generation: u64,
        seek_serial: u64,
        observed_position_ms: Option<i64>,
    ) {
        let request = {
            let mut active = lock(&self.active);
            let Some(current) = active.as_mut() else { return };
            if current.generation != generation {
                return;
            }
            let ActiveMode::Transitioning(direct) = &mut current.mode else {
                return;
            };
            if direct.seek_serial == seek_serial
                && direct.pending_seek_ms.load(Ordering::Relaxed) == -1
            {
                if let Some(position_ms) = observed_position_ms {
                    direct.request.start_position_ms = position_ms;
                }
            }
            direct.request.paused = current.paused;
            (*direct.request).clone()
        };
        if !self.start_pump(generation, request, true) {
            tracing::debug!(guild_id = self.guild_id, generation, "discarded a superseded PCM transition");
        }
    }

    fn send_to_pump(&self, command: PumpCommand) {
        if let Some(active) = lock(&self.active).as_ref() {
            let ActiveMode::Pump {
                commands,
                interrupt,
                ring,
            } = &active.mode
            else {
                return;
            };
            signal_pump(
                commands,
                interrupt,
                command,
                self.guild_id,
                active.generation,
            );
            // In steady playback the ring is usually full (decode outruns real
            // time), so the pump is usually parked in wait_for_space rather than
            // between packets — without this, a command sent there sits unseen for
            // up to COMMAND_POLL (100ms), which for Seek means up to 100ms of
            // playerUpdates reporting the pre-seek position before it catches up.
            ring.wake();
        }
    }

    fn report(&self, generation: u64, event: EngineEvent) {
        let Some(events) = self.events.get().cloned() else {
            return;
        };
        match events.try_send(EngineReport { generation, event }) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => tracing::error!(
                guild_id = self.guild_id,
                generation,
                "engine report queue is full"
            ),
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => tracing::debug!(
                guild_id = self.guild_id,
                generation,
                "engine report receiver is closed"
            ),
        }
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
        match previous.mode {
            ActiveMode::Pump {
                commands,
                interrupt,
                ring,
            } => {
                signal_pump(
                    &commands,
                    &interrupt,
                    PumpCommand::Stop,
                    self.guild_id,
                    previous.generation,
                );
                drop(commands);

                // Before the replacement ring exists, so the reader this one leaves behind
                // inside songbird's Input cannot report the outgoing track's position
                // over the incoming track's.
                ring.detach_position();
            }
            ActiveMode::Direct(state) | ActiveMode::Transitioning(state) => {
                state.end_schedule.fetch_add(1, Ordering::Relaxed);
            }
        }

        if let Some(track) = previous.track {
            log_track_control(
                &self.active,
                self.guild_id,
                previous.generation,
                "stop",
                track.stop(),
            );
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

    fn attach(&self, events: AsyncSender<EngineReport>) {
        // Engine::attach's own contract: called once, at construction.
        if self.events.set(events).is_err() {
            tracing::error!(guild_id = self.guild_id, "engine event channel attached twice");
        }
    }

    fn play(&self, request: PlayRequest) -> u64 {
        self.stop_active();
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);

        if direct_path_eligible(&request) {
            if let Some(input) = self.opener.direct_input(&request.track.info) {
                self.start_direct(generation, request, input);
                return generation;
            }
        }

        if !self.start_pump(generation, request, false) {
            tracing::error!(
                guild_id = self.guild_id,
                generation,
                "could not install the PCM pipeline as the active track"
            );
        }

        generation
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
        let (track, seek_pending, generation) = {
            let mut active = lock(&self.active);
            let Some(current) = active.as_mut() else { return };
            current.paused = paused;
            let seek_pending = match &mut current.mode {
                ActiveMode::Direct(direct) | ActiveMode::Transitioning(direct) => {
                    direct.request.paused = paused;
                    direct.pending_seek_ms.load(Ordering::Relaxed) != -1
                }
                ActiveMode::Pump { .. } => false,
            };
            (current.track.clone(), seek_pending, current.generation)
        };
        let Some(track) = track else { return };

        if !paused && seek_pending {
            return;
        }

        log_track_control(
            &self.active,
            self.guild_id,
            generation,
            if paused { "pause" } else { "play" },
            if paused { track.pause() } else { track.play() },
        );
    }

    fn seek(&self, position_ms: i64) {
        // Announced before the command is even sent, mirroring lavaplayer's own
        // LocalAudioTrackExecutor.setPosition, which sets queuedSeek
        // synchronously on the calling thread — so position reporting holds at
        // the target from this call's return, not from whenever the pump gets
        // around to the command. See RingWriter::begin_seek's docs.
        let mut pump = false;
        let direct = {
            let mut active = lock(&self.active);
            let Some(current) = active.as_mut() else { return };
            match &mut current.mode {
                ActiveMode::Pump { ring, .. } => {
                    ring.begin_seek(position_ms);
                    pump = true;
                    None
                }
                ActiveMode::Direct(direct) | ActiveMode::Transitioning(direct) => {
                    direct.request.start_position_ms = position_ms;
                    direct.seek_serial = direct.seek_serial.wrapping_add(1);
                    direct.pending_seek_ms.store(position_ms, Ordering::Relaxed);
                    Some((
                        current.track.clone(),
                        current.generation,
                        direct.seek_serial,
                        direct.request.end_time_ms,
                        Arc::clone(&direct.end_schedule),
                        Arc::clone(&direct.pending_seek_ms),
                    ))
                }
            }
        };
        if pump {
            self.send_to_pump(PumpCommand::Seek { position_ms });
        }
        if let Some((Some(track), generation, serial, end_time, schedule, pending)) = direct {
            self.schedule_direct_end(
                track.clone(),
                generation,
                end_time,
                position_ms,
                schedule,
            );
            self.seek_direct(track, generation, serial, position_ms, pending);
        }
    }

    fn set_volume(&self, volume: i32) {
        let mut pump = false;
        let track = {
            let mut active = lock(&self.active);
            let Some(current) = active.as_mut() else { return };
            match &mut current.mode {
                ActiveMode::Pump { .. } => {
                    pump = true;
                    None
                }
                ActiveMode::Direct(direct) | ActiveMode::Transitioning(direct) => {
                    direct.request.volume = volume;
                    current
                        .track
                        .clone()
                        .map(|track| (track, current.generation))
                }
            }
        };
        if pump {
            self.send_to_pump(PumpCommand::SetVolume(volume));
        }
        if let Some((track, generation)) = track {
            log_track_control(
                &self.active,
                self.guild_id,
                generation,
                "set_volume",
                track.set_volume(player_volume_multiplier(volume)),
            );
        }
    }

    fn set_filters(&self, filters: &Filters) {
        let mut pump = false;
        let transition = {
            let mut active = lock(&self.active);
            let Some(current) = active.as_mut() else { return };
            match &mut current.mode {
                ActiveMode::Pump { .. } => {
                    pump = true;
                    None
                }
                ActiveMode::Direct(direct) => {
                    direct.request.filters = filters.clone();
                    if filters == &Filters::default() {
                        None
                    } else {
                        let state = direct.clone();
                        let result = Some((
                            current.track.clone(),
                            current.generation,
                            state.seek_serial,
                        ));
                        current.mode = ActiveMode::Transitioning(state);
                        result
                    }
                }
                ActiveMode::Transitioning(direct) => {
                    direct.request.filters = filters.clone();
                    None
                }
            }
        };
        if pump {
            self.send_to_pump(PumpCommand::SetFilters(Box::new(filters.clone())));
        }

        if let Some((track, generation, seek_serial)) = transition {
            if let Some(track) = track {
                let engine = self.clone();
                self.runtime.spawn(async move {
                    let position = track
                        .get_info()
                        .await
                        .ok()
                        .map(|state| duration_ms(state.position));
                    engine.finish_direct_transition(generation, seek_serial, position);
                });
            } else {
                self.finish_direct_transition(generation, seek_serial, None);
            }
        }
    }

    fn set_end_time(&self, end_time_ms: Option<i64>) {
        let mut pump = false;
        let direct = {
            let mut active = lock(&self.active);
            let Some(current) = active.as_mut() else { return };
            match &mut current.mode {
                ActiveMode::Pump { .. } => {
                    pump = true;
                    None
                }
                ActiveMode::Direct(direct) | ActiveMode::Transitioning(direct) => {
                    direct.request.end_time_ms = end_time_ms;
                    direct.end_schedule.fetch_add(1, Ordering::Relaxed);
                    Some((
                        current.track.clone(),
                        current.generation,
                        Arc::clone(&direct.end_schedule),
                    ))
                }
            }
        };
        if pump {
            self.send_to_pump(PumpCommand::SetEndTime(end_time_ms));
        }
        if let Some((Some(track), generation, schedule)) = direct {
            self.schedule_direct_end(
                track,
                generation,
                end_time_ms,
                self.position_ms.load(Ordering::Relaxed),
                schedule,
            );
        }
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
            track: None,
            generation,
            paused: false,
            mode: ActiveMode::Pump {
                commands,
                interrupt: Arc::new(AtomicBool::new(false)),
                ring,
            },
        }
    }

    fn direct_request(source_name: &str, filters: Filters) -> PlayRequest {
        PlayRequest {
            track: lavalink_protocol::player::Track::new(
                String::new(),
                lavalink_protocol::player::TrackInfo {
                    identifier: "id".into(),
                    is_seekable: true,
                    author: "author".into(),
                    length: 1_000,
                    is_stream: false,
                    position: 0,
                    title: "title".into(),
                    uri: None,
                    source_name: source_name.into(),
                    artwork_url: None,
                    isrc: None,
                },
            ),
            start_position_ms: 0,
            end_time_ms: None,
            paused: false,
            volume: 100,
            filters,
        }
    }

    #[test]
    fn only_unfiltered_youtube_tracks_take_the_direct_path() {
        assert!(direct_path_eligible(&direct_request("youtube", Filters::default())));
        assert!(!direct_path_eligible(&direct_request("http", Filters::default())));

        let filtered = Filters {
            volume: lavalink_protocol::omissible::Omissible::Present(1.0),
            ..Filters::default()
        };
        assert!(!direct_path_eligible(&direct_request("youtube", filtered)));
    }

    #[test]
    fn direct_events_stop_as_soon_as_the_pcm_pump_takes_over() {
        let direct = DirectState {
            request: Box::new(direct_request("youtube", Filters::default())),
            end_schedule: Arc::new(AtomicU64::new(0)),
            pending_seek_ms: Arc::new(AtomicI64::new(-1)),
            seek_serial: 0,
        };
        let active = Mutex::new(Some(Active {
            track: None,
            generation: 7,
            paused: false,
            mode: ActiveMode::Direct(direct.clone()),
        }));

        assert!(is_current_direct(&active, 7));
        lock(&active).as_mut().unwrap().mode = ActiveMode::Transitioning(direct);
        assert!(is_current_direct(&active, 7));
        lock(&active).as_mut().unwrap().mode = dummy_active(7).mode;
        assert!(!is_current_direct(&active, 7));
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

    #[test]
    fn a_reliable_report_waits_for_queue_capacity() {
        let (events, mut reports) = tokio::sync::mpsc::channel(1);
        events
            .try_send(EngineReport {
                generation: 1,
                event: EngineEvent::Progress,
            })
            .unwrap();

        let sender = std::thread::spawn(move || {
            send_reliable(
                &events,
                EngineReport {
                    generation: 1,
                    event: EngineEvent::Finished,
                },
            );
        });
        std::thread::sleep(Duration::from_millis(10));
        assert!(!sender.is_finished(), "reliable report was dropped from a full queue");

        assert!(matches!(reports.blocking_recv().unwrap().event, EngineEvent::Progress));
        sender.join().unwrap();
        assert!(matches!(reports.blocking_recv().unwrap().event, EngineEvent::Finished));
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
