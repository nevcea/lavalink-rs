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

use std::sync::atomic::{AtomicI64, Ordering};
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
}

/// A running track.
struct Active {
    /// Commands to the pump thread. Dropping it stops the pump.
    commands: Sender<PumpCommand>,
    /// The songbird side, so pause and stop reach the mixer.
    track: Option<TrackHandle>,
    /// Bumped on every new track; a late outcome from a superseded pump is ignored.
    generation: u64,
}

impl PipelineEngine {
    pub fn new(
        guild_id: u64,
        buffer_ms: u32,
        voice: SharedVoice,
        opener: Arc<StreamOpener>,
        events: EventSlot,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self {
            guild_id,
            position_ms: Arc::new(AtomicI64::new(0)),
            buffer_ms,
            voice,
            opener,
            runtime,
            events,
            active: Arc::new(Mutex::new(None)),
            frames: Arc::new(ring::FrameCounters::default()),
        }
    }

    fn send_to_pump(&self, command: PumpCommand) {
        if let Some(active) = lock(&self.active).as_ref() {
            let _ = active.commands.send(command);
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

    /// Tears down whatever is playing. Returns the generation that was cancelled.
    fn stop_active(&self) -> u64 {
        let previous = lock(&self.active).take();
        let Some(previous) = previous else { return 0 };

        // Dropping the sender is what unblocks a pump parked on a full ring.
        let _ = previous.commands.send(PumpCommand::Stop);
        drop(previous.commands);

        if let Some(track) = previous.track {
            let _ = track.stop();
        }
        previous.generation
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
        let generation = self.stop_active().wrapping_add(1);

        let (writer, reader) = ring::channel(
            self.buffer_ms,
            Arc::clone(&self.position_ms),
            Arc::clone(&self.frames),
        );
        let (commands, pump_commands) = mpsc::channel();

        lock(&self.active).replace(Active {
            commands,
            track: None,
            generation,
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
                let mut active = lock(&active);
                match active.as_mut() {
                    Some(current) if current.generation == generation => {
                        current.track = Some(handle);
                    }
                    _ => {
                        let _ = handle.stop();
                        tracing::debug!(guild_id, "discarded a superseded input");
                    }
                }
            });
        }

        let position_ms = Arc::clone(&self.position_ms);
        let events = self.events.get().cloned();
        let guild_id = self.guild_id;
        let config = PumpConfig {
            info: request.track.info.clone(),
            start_position_ms: request.start_position_ms,
            end_time_ms: request.end_time_ms,
            volume: request.volume,
            filters: request.filters.clone(),
            opener: Arc::clone(&self.opener),
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
                // from, but it must not reach another player, and the track has to
                // end with a defined event rather than silence.
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    pump::run(config, writer, pump_commands, position_ms, &on_progress)
                }));

                let outcome = match outcome {
                    Ok(outcome) => outcome,
                    Err(_) => super::PumpOutcome::Failed {
                        exception: Exception::fault(
                            "The audio pipeline panicked",
                            "pump panic",
                        ),
                        started: true,
                    },
                };

                if let Some(events) = &events {
                    let event = match outcome {
                        super::PumpOutcome::Finished => Some(EngineEvent::Finished),
                        super::PumpOutcome::Failed { exception, started } => {
                            Some(EngineEvent::Failed { exception, started })
                        }
                        // A stop was requested; the actor already knows.
                        super::PumpOutcome::Stopped => None,
                    };
                    if let Some(event) = event {
                        let _ = events.try_send(Command::Engine(event));
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
        let track = lock(&self.active)
            .as_ref()
            .and_then(|active| active.track.clone());
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
