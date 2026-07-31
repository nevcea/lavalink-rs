//! The player actor and its discipline.
//!
//! The original guards each player with `synchronized(player)` and then does slow
//! things inside it — waiting on a voice connection with `.join()`
//! (`PlayerRestHandler.kt:114-141`), which stalls every other request for that guild
//! while Discord is slow.
//!
//! Replacing a lock with an actor only helps if the actor cannot block either;
//! otherwise the lock has just become a queue. So three rules are enforced by
//! construction here:
//!
//! 1. **The loop never awaits external I/O.** Every handler is a state transition
//!    plus a message send, and returns immediately.
//! 2. **Loading happens outside.** Resolving an identifier, decoding a track and
//!    establishing the voice connection are all done by the caller before
//!    [`Command::Patch`] arrives, so the actor only ever sees a finished result.
//! 3. **The actor does not touch audio data.** It starts and stops the engine and
//!    reads a counter.
//!
//! What is left is small enough to read as a transition table.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lavalink_protocol::filters::Filters;
use lavalink_protocol::message::{EmittedEvent, Message, TrackEndReason};
use lavalink_protocol::player::{JsonObject, Player, Track, VoiceState};
use lavalink_protocol::Omissible;
use tokio::sync::{mpsc, oneshot};

use crate::audio::ring::FrameCounters;
use crate::audio::{Engine, EngineEvent, PlayRequest};
use crate::player::state::{PlayerModel, VoiceConnection};
use crate::sink::Sink;

/// Bounded so a runaway producer applies backpressure to its caller rather than
/// growing without limit. REST awaits a slot for up to [`SEND_TIMEOUT`]; past
/// that the actor is considered wedged, which the caller turns into 503.
const COMMAND_CAPACITY: usize = 64;

/// How long [`PlayerHandle::send`] waits for room in a full queue before giving
/// up. The actor loop never awaits I/O, so a healthy actor drains a full queue
/// in microseconds — only a wedged one holds it open this long.
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the actor checks whether the current track has gone quiet.
const STUCK_CHECK_INTERVAL: Duration = Duration::from_millis(500);

/// Voice transitions are rare (only on an actual connect/reconnect/disconnect)
/// and never queue up behind each other in practice — this only has to be
/// bigger than "one", not big.
const VOICE_UPDATE_CAPACITY: usize = 16;

#[derive(Debug)]
pub enum Command {
    /// One `PATCH /v4/sessions/{id}/players/{guildId}`, applied in the original's
    /// order in a single step so no other command can interleave halfway.
    Patch(Box<PatchRequest>, oneshot::Sender<Player>),
    Snapshot(oneshot::Sender<Player>),
    /// Emit a `playerUpdate` to the session. Sent by the global tick, which does not
    /// read player state itself — asking the actor to publish keeps the actor the
    /// only reader of its own fields.
    EmitUpdate,
    /// The audio engine reported something.
    Engine(EngineEvent),
    Destroy(oneshot::Sender<()>),
}

#[derive(Debug, Default)]
pub struct PatchRequest {
    /// Already established by the caller if present, so recording it here cannot
    /// fail — the error was mapped to a status code before we got here.
    pub voice: Option<VoiceState>,
    pub paused: Omissible<bool>,
    pub user_data: Omissible<JsonObject>,
    pub volume: Omissible<i32>,
    pub position: Omissible<i64>,
    pub end_time: Omissible<Option<i64>>,
    pub filters: Omissible<Filters>,
    /// `None` means the request said nothing about the track.
    pub track: Option<TrackChange>,
    pub no_replace: bool,
}

#[derive(Debug)]
pub enum TrackChange {
    /// A resolved track, ready to hand to the engine.
    Play(Box<Track>),
    /// `encodedTrack: null` — an explicit stop (`PlayerRestHandler.kt:225`).
    Clear,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceUpdate {
    Connecting,
    Connected { ping_ms: i64 },
    Reconnecting,
    Disconnected,
    Closed {
        code: i32,
        by_remote: bool,
    },
}

/// A cheap, cloneable reference to a player.
#[derive(Debug, Clone)]
pub struct PlayerHandle {
    pub guild_id: u64,
    commands: mpsc::Sender<Command>,
    /// Separate from `commands`: a voice transition (the only writer of the
    /// connection cache) must not be able to lose its turn behind a burst of
    /// unrelated REST traffic sharing the same queue — see the channel's own
    /// docs on `PlayerActor`.
    voice_updates: mpsc::Sender<VoiceUpdate>,
    /// Written by the audio engine's *consuming* side and read here without a lock.
    /// Never written by the pump: the pump runs ahead by `frameBufferDurationMs`, so
    /// its production count is not a playback position.
    position_ms: Arc<AtomicI64>,
    /// Epoch ms when the current unbroken `Playing` period started, or `0` when not
    /// playing. Written only by the actor, in [`PlayerActor::sync_playing`]; read
    /// here without a lock, the same arrangement as `position_ms`.
    playing_since_ms: Arc<AtomicI64>,
    frames: Arc<FrameCounters>,
}

impl PlayerHandle {
    pub fn position_ms(&self) -> i64 {
        self.position_ms.load(Ordering::Relaxed)
    }

    /// `0` if not currently playing. Used both for `/v4/stats`' `playingPlayers`
    /// and, gated by how long ago this was, for `frameStats`' usability.
    pub fn playing_since_ms(&self) -> i64 {
        self.playing_since_ms.load(Ordering::Relaxed)
    }

    /// Frames sent/nulled since the last call. Draining resets both to zero, so
    /// call this at most once per stats tick per player.
    pub fn take_frame_stats(&self) -> (u32, u32) {
        self.frames.take()
    }

    pub async fn send(&self, command: Command) -> Result<(), PlayerGone> {
        match tokio::time::timeout(SEND_TIMEOUT, self.commands.send(command)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) | Err(_) => Err(PlayerGone),
        }
    }

    /// Non-blocking send, for callers that would rather skip than wait — the global
    /// tick, whose next round supersedes anything it drops.
    pub fn try_send(&self, command: Command) -> Result<(), PlayerGone> {
        self.commands.try_send(command).map_err(|_| PlayerGone)
    }

    /// Reports a voice transition. Non-blocking, like `try_send`: this is the
    /// same call `ActorNotifier` makes from songbird's own event dispatch,
    /// which must not block — but on its own channel, so it cannot be starved
    /// by whatever `commands` happens to be carrying at the same moment.
    pub fn send_voice(&self, update: VoiceUpdate) -> Result<(), PlayerGone> {
        self.voice_updates.try_send(update).map_err(|_| PlayerGone)
    }

    pub async fn snapshot(&self) -> Result<Player, PlayerGone> {
        let (tx, rx) = oneshot::channel();
        self.send(Command::Snapshot(tx)).await?;
        rx.await.map_err(|_| PlayerGone)
    }

    pub async fn patch(&self, request: PatchRequest) -> Result<Player, PlayerGone> {
        let (tx, rx) = oneshot::channel();
        self.send(Command::Patch(Box::new(request), tx)).await?;
        rx.await.map_err(|_| PlayerGone)
    }

    pub async fn destroy(&self) -> Result<(), PlayerGone> {
        let (tx, rx) = oneshot::channel();
        self.send(Command::Destroy(tx)).await?;
        rx.await.map_err(|_| PlayerGone)
    }
}

/// A slot holding the channel back to an actor, shared by everything that reports to
/// it.
///
/// The actor's sender only exists once [`PlayerActor::new`] has run, but the engine
/// and the voice connection have to be built first — the actor needs them. Rather
/// than construct the graph in two phases or hand out a reference to the actor, both
/// take this slot and it is filled in afterwards, exactly once — a `OnceLock`
/// rather than a `Mutex` says so directly, instead of leaving it to the "called
/// once, at construction" comment on [`Engine::attach`](crate::audio::Engine::attach).
pub type EventSlot = Arc<std::sync::OnceLock<mpsc::Sender<Command>>>;

/// As [`EventSlot`], but for voice transitions — `VoiceConnection` is built
/// before the actor exists, same as the engine, and needs somewhere to put
/// its sender until [`PlayerActor::new`] fills it in.
pub type VoiceUpdateSlot = Arc<std::sync::OnceLock<mpsc::Sender<VoiceUpdate>>>;

/// The actor is gone — destroyed, or its task died.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("player is no longer running")]
pub struct PlayerGone;

pub struct PlayerActor {
    model: PlayerModel,
    engine: Box<dyn Engine>,
    sink: Arc<Sink>,
    commands: mpsc::Receiver<Command>,
    /// Kept off `commands` on purpose — see `PlayerHandle::voice_updates`'s
    /// docs. `None` once the sender side is gone (a defunct `VoiceConnection`
    /// does not end the player), so `run`'s select loop stops polling it
    /// instead of spinning on a channel that will only ever report closed.
    voice_updates: mpsc::Receiver<VoiceUpdate>,
    voice_updates_closed: bool,
    position_ms: Arc<AtomicI64>,
    playing_since_ms: Arc<AtomicI64>,
    stuck_threshold: Duration,
    /// Set while a `TrackStuckEvent` has been emitted for the current track, so we
    /// report it once rather than every tick.
    stuck_reported: bool,
}

impl PlayerActor {
    /// Builds the actor and its handle. The caller spawns [`PlayerActor::run`].
    ///
    /// `voice_slot` is filled in here with the actor's own voice-update
    /// sender, the same deferred-fill pattern [`EventSlot`] uses — the caller
    /// builds `VoiceConnection` (which needs a sender) before it can build
    /// the actor (which is what creates one).
    pub fn new(
        guild_id: u64,
        engine: Box<dyn Engine>,
        sink: Arc<Sink>,
        stuck_threshold: Duration,
        voice_slot: VoiceUpdateSlot,
    ) -> (Self, PlayerHandle) {
        let (tx, rx) = mpsc::channel(COMMAND_CAPACITY);
        let (voice_tx, voice_rx) = mpsc::channel(VOICE_UPDATE_CAPACITY);
        let position_ms = engine.position_handle();
        let frames = engine.frame_counters();
        // Not from the engine: whether the player is playing is the actor's own
        // state, not the pipeline's.
        let playing_since_ms = Arc::new(AtomicI64::new(0));
        // The engine reports back as `Command::Engine`, so it never needs a
        // reference to the actor itself.
        engine.attach(tx.clone());
        let _ = voice_slot.set(voice_tx.clone());

        let handle = PlayerHandle {
            guild_id,
            commands: tx,
            voice_updates: voice_tx,
            position_ms: Arc::clone(&position_ms),
            playing_since_ms: Arc::clone(&playing_since_ms),
            frames,
        };
        let actor = Self {
            model: PlayerModel::new(guild_id),
            engine,
            sink,
            commands: rx,
            voice_updates: voice_rx,
            voice_updates_closed: false,
            position_ms,
            playing_since_ms,
            stuck_threshold,
            stuck_reported: false,
        };
        (actor, handle)
    }

    pub async fn run(mut self) {
        let mut stuck_check = tokio::time::interval(STUCK_CHECK_INTERVAL);
        stuck_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                command = self.commands.recv() => {
                    let Some(command) = command else { break };
                    if self.handle(command) == Flow::Stop {
                        break;
                    }
                }
                update = self.voice_updates.recv(), if !self.voice_updates_closed => {
                    match update {
                        Some(update) => {
                            self.apply_voice(update);
                            self.sync_playing();
                        }
                        None => self.voice_updates_closed = true,
                    }
                }
                _ = stuck_check.tick() => self.check_stuck(Instant::now()),
            }
        }

        self.engine.shutdown();
    }

    fn handle(&mut self, command: Command) -> Flow {
        let flow = match command {
            Command::Snapshot(reply) => {
                let _ = reply.send(self.snapshot());
                Flow::Continue
            }
            Command::EmitUpdate => {
                self.emit_player_update();
                Flow::Continue
            }
            Command::Patch(request, reply) => {
                self.apply_patch(*request);
                let _ = reply.send(self.snapshot());
                Flow::Continue
            }
            Command::Engine(event) => {
                self.apply_engine_event(event);
                Flow::Continue
            }
            Command::Destroy(reply) => {
                // Destroy wins over anything in flight. The track ends with no event,
                // matching the original, which drops the player without emitting on
                // `DELETE`.
                self.engine.stop();
                self.model.stop();
                let _ = reply.send(());
                Flow::Stop
            }
        };
        // Every path above can change `self.model.playback`, and none of them is
        // worth hunting down individually just to keep a second counter in step —
        // one resync after every command is cheaper to keep correct.
        self.sync_playing();
        flow
    }

    /// Keeps `playing_since_ms` in step with `self.model.playback`: stamped with
    /// the current time on the transition into `Playing`, held steady for as long
    /// as playback continues (so a burst of unrelated commands, e.g. repeated
    /// `EmitUpdate` ticks, does not keep bumping it forward), and cleared to `0`
    /// the moment playback stops.
    fn sync_playing(&mut self) {
        if self.model.playback.is_playing() {
            if self.playing_since_ms.load(Ordering::Relaxed) == 0 {
                self.playing_since_ms
                    .store(now_epoch_ms(), Ordering::Relaxed);
            }
        } else {
            self.playing_since_ms.store(0, Ordering::Relaxed);
        }
    }

    /// The order below is `PlayerRestHandler.kt:143-226` and is wire-visible: it
    /// decides, for instance, whether `position` in a play request seeks the old
    /// track or starts the new one at an offset.
    fn apply_patch(&mut self, request: PatchRequest) {
        let now = Instant::now();

        if let Some(voice) = request.voice {
            self.model.voice = voice;
        }

        // Several fields are applied here only when the request leaves the track
        // alone, and again further down when it does not.
        let track_untouched = request.track.is_none();

        if let Some(paused) = request.paused.take_if(track_untouched) {
            self.model.set_paused(paused, now);
            self.engine.set_paused(paused);
        }

        // Cloned because `userData` is consumed again further down, when the request
        // also sets a track — the two paths are mutually exclusive at run time but
        // both are reachable from here.
        if let Some(user_data) = request.user_data.clone().take_if(track_untouched) {
            if let Some(track) = self.model.track.as_mut() {
                track.user_data = user_data;
            }
        }

        // Volume is the exception: always applied immediately, whatever else the
        // request does.
        if let Omissible::Present(volume) = request.volume {
            self.model.set_volume(volume);
            self.engine.set_volume(self.model.volume);
        }

        if let Some(position) = request.position.take_if(track_untouched) {
            if self.model.track.is_some() {
                self.engine.seek(position);
                // The optimistic value goes out now: the original reports the
                // requested position rather than where the seek actually landed, and
                // the next playerUpdate corrects it. Reporting the true value here
                // would be more accurate and less compatible.
                self.position_ms.store(position, Ordering::Relaxed);
                self.emit_player_update();
            }
        }

        if let Some(end_time) = request.end_time.take_if(track_untouched) {
            self.model.end_time_ms = end_time;
            self.engine.set_end_time(end_time);
        }

        if let Omissible::Present(filters) = request.filters {
            self.model.filters = filters;
            self.engine.set_filters(&self.model.filters);
            self.emit_player_update();
        }

        match request.track {
            None => {}
            Some(TrackChange::Clear) => {
                self.stop_track(TrackEndReason::Stopped);
            }
            Some(TrackChange::Play(track)) => {
                // `noReplace` with something already playing: the request is dropped
                // and the current state returned, with a 200 rather than an error
                // (`:182-185`).
                if request.no_replace && self.model.track.is_some() {
                    return;
                }

                // Anything already playing ends as REPLACED before the new track
                // starts.
                if self.model.track.is_some() {
                    self.stop_track(TrackEndReason::Replaced);
                }

                // A play request with no `paused` field forces `false` (`:186`) —
                // note this ignores a `paused: true` sent alongside a track only in
                // the sense that it is applied *after*, not skipped.
                let paused = match request.paused {
                    Omissible::Present(paused) => paused,
                    Omissible::Omitted => false,
                };

                let position = request.position.into_option().unwrap_or(0);
                let end_time = request.end_time.into_option().flatten();
                let mut track = *track;
                if let Omissible::Present(user_data) = request.user_data {
                    track.user_data = user_data;
                }

                self.model.end_time_ms = end_time;
                self.model.play(track.clone(), paused, now);
                self.position_ms.store(position, Ordering::Relaxed);
                self.stuck_reported = false;

                self.engine.play(PlayRequest {
                    track: track.clone(),
                    start_position_ms: position,
                    end_time_ms: end_time,
                    paused,
                    volume: self.model.volume,
                    filters: self.model.filters.clone(),
                });

                self.emit(EmittedEvent::TrackStart {
                    guild_id: self.guild_id_string(),
                    track: Box::new(track),
                });
            }
        }
    }

    fn apply_voice(&mut self, update: VoiceUpdate) {
        match update {
            VoiceUpdate::Connecting => self.model.connection = VoiceConnection::Connecting,
            VoiceUpdate::Connected { ping_ms } => {
                self.model.connection = VoiceConnection::Connected;
                self.model.ping_ms = ping_ms;
            }
            VoiceUpdate::Reconnecting => self.model.connection = VoiceConnection::Reconnecting,
            VoiceUpdate::Disconnected => {
                self.model.connection = VoiceConnection::Disconnected;
                self.model.ping_ms = -1;
            }
            VoiceUpdate::Closed { code, by_remote } => {
                self.model.connection = VoiceConnection::Disconnected;
                self.model.ping_ms = -1;
                self.emit(EmittedEvent::WebSocketClosed {
                    guild_id: self.guild_id_string(),
                    code,
                    // The original forwards Discord's reason string; we do not get
                    // one from every path, so an empty string stands in — which is
                    // also what the original sends when koe reports none
                    // (`SocketContext.kt:224`).
                    reason: String::new(),
                    by_remote,
                });
            }
        }
        // Both the original and we push a fresh update after a voice transition, so
        // clients see `connected` flip without waiting for the next tick.
        self.emit_player_update();
    }

    fn apply_engine_event(&mut self, event: EngineEvent) {
        match event {
            EngineEvent::Progress => {
                self.model.last_progress = Some(Instant::now());
                self.stuck_reported = false;
            }
            EngineEvent::Finished => self.stop_track(TrackEndReason::Finished),
            EngineEvent::Failed { exception, started } => {
                let Some(track) = self.model.track.clone() else {
                    return;
                };
                self.emit(EmittedEvent::TrackException {
                    guild_id: self.guild_id_string(),
                    track: Box::new(track),
                    exception,
                });
                // A track that never produced audio ended as LOAD_FAILED; one that
                // died partway ended as FINISHED. Clients use this to decide whether
                // to advance the queue.
                self.stop_track(if started {
                    TrackEndReason::Finished
                } else {
                    TrackEndReason::LoadFailed
                });
            }
        }
    }

    fn check_stuck(&mut self, now: Instant) {
        if !self.model.playback.is_playing() || self.stuck_reported {
            return;
        }
        let Some(last_progress) = self.model.last_progress else {
            return;
        };
        if now.duration_since(last_progress) < self.stuck_threshold {
            return;
        }
        let Some(track) = self.model.track.clone() else {
            return;
        };

        self.stuck_reported = true;
        self.emit(EmittedEvent::TrackStuck {
            guild_id: self.guild_id_string(),
            track: Box::new(track),
            threshold_ms: self.stuck_threshold.as_millis() as i64,
        });
    }

    /// Ends the current track, emitting `TrackEndEvent` only if there was one.
    fn stop_track(&mut self, reason: TrackEndReason) {
        self.engine.stop();
        let track = self.model.stop();
        self.position_ms.store(0, Ordering::Relaxed);
        self.stuck_reported = false;

        if let Some(track) = track {
            self.emit(EmittedEvent::TrackEnd {
                guild_id: self.guild_id_string(),
                track: Box::new(track),
                reason,
            });
        }
    }

    fn snapshot(&self) -> Player {
        self.model.snapshot(self.position(), now_epoch_ms())
    }

    fn position(&self) -> i64 {
        self.position_ms.load(Ordering::Relaxed)
    }

    fn emit_player_update(&self) {
        let _ = self.sink.send(Message::PlayerUpdate {
            state: self.model.wire_state(self.position(), now_epoch_ms()),
            guild_id: self.guild_id_string(),
        });
    }

    fn emit(&self, event: EmittedEvent) {
        // A full sink means the client stopped reading; the websocket task notices
        // and closes the session. Nothing useful can be done from here.
        let _ = self.sink.send(Message::Event(event));
    }

    fn guild_id_string(&self) -> String {
        self.model.guild_id.to_string()
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Flow {
    Continue,
    Stop,
}

pub fn now_epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::testing::{EngineCall, RecordingEngine};
    use lavalink_protocol::player::TrackInfo;

    fn track(title: &str) -> Track {
        Track::new(
            "encoded".into(),
            TrackInfo {
                identifier: "id".into(),
                is_seekable: true,
                author: "author".into(),
                length: 10_000,
                is_stream: false,
                position: 0,
                title: title.into(),
                uri: None,
                source_name: "http".into(),
                artwork_url: None,
                isrc: None,
            },
        )
    }

    struct Harness {
        handle: PlayerHandle,
        sink: Arc<Sink>,
        engine: RecordingEngine,
    }

    impl Harness {
        fn start() -> Self {
            Self::start_with_stuck_threshold(Duration::from_secs(10))
        }

        fn start_with_stuck_threshold(stuck_threshold: Duration) -> Self {
            let sink = Arc::new(Sink::new());
            let engine = RecordingEngine::new();
            let (actor, handle) = PlayerActor::new(
                123,
                Box::new(engine.clone()),
                Arc::clone(&sink),
                stuck_threshold,
                Arc::new(std::sync::OnceLock::new()),
            );
            tokio::spawn(actor.run());
            Self {
                handle,
                sink,
                engine,
            }
        }

        /// Every message the sink has, in order.
        fn drain(&self) -> Vec<Message> {
            std::iter::from_fn(|| self.sink.try_recv()).collect()
        }

        fn events(&self) -> Vec<EmittedEvent> {
            self.drain()
                .into_iter()
                .filter_map(|message| match message {
                    Message::Event(event) => Some(event),
                    _ => None,
                })
                .collect()
        }

        /// Retries `snapshot()` until `until` holds.
        ///
        /// Needed whenever a test wants to observe the effect of a
        /// `send_voice` call: `voice_updates` and `commands` are separate
        /// channels now (see `PlayerHandle::voice_updates`'s docs), so
        /// nothing guarantees a voice update sent moments earlier has
        /// already been applied by the instant a specific `snapshot()`
        /// call's reply is captured — `tokio::select!` may service either
        /// channel first.
        async fn snapshot_until(&self, mut until: impl FnMut(&Player) -> bool) -> Player {
            for _ in 0..100 {
                let player = self.handle.snapshot().await.unwrap();
                if until(&player) {
                    return player;
                }
                tokio::task::yield_now().await;
            }
            panic!("condition was never satisfied within the retry budget");
        }
    }

    fn play(track: Track) -> PatchRequest {
        PatchRequest {
            track: Some(TrackChange::Play(Box::new(track))),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn playing_a_track_starts_the_engine_and_emits_track_start() {
        let harness = Harness::start();
        let player = harness.handle.patch(play(track("first"))).await.unwrap();

        assert_eq!(player.track.unwrap().info.title, "first");
        assert!(!player.paused);

        let events = harness.events();
        assert!(matches!(
            events.as_slice(),
            [EmittedEvent::TrackStart { .. }]
        ));
        assert!(harness
            .engine
            .calls()
            .iter()
            .any(|call| matches!(call, EngineCall::Play { .. })));
    }

    /// `PlayerRestHandler.kt:186`: a play request that says nothing about `paused`
    /// forces the player unpaused, even if it was paused before.
    #[tokio::test]
    async fn a_new_track_clears_a_previous_pause() {
        let harness = Harness::start();
        harness.handle.patch(play(track("first"))).await.unwrap();
        harness
            .handle
            .patch(PatchRequest {
                paused: Omissible::Present(true),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(harness.handle.snapshot().await.unwrap().paused);

        let player = harness.handle.patch(play(track("second"))).await.unwrap();
        assert!(!player.paused);
    }

    /// A play request with `paused: true` must reach the engine, not just the
    /// reported model — otherwise a client sees `"paused": true` while the track
    /// keeps playing audibly.
    #[tokio::test]
    async fn a_play_request_with_paused_pauses_the_engine() {
        let harness = Harness::start();
        let player = harness
            .handle
            .patch(PatchRequest {
                paused: Omissible::Present(true),
                track: Some(TrackChange::Play(Box::new(track("first")))),
                ..Default::default()
            })
            .await
            .unwrap();

        assert!(player.paused);
        assert!(harness.engine.calls().iter().any(|call| matches!(
            call,
            EngineCall::Play { paused: true, .. }
        )));
    }

    #[tokio::test]
    async fn replacing_a_track_ends_the_old_one_as_replaced_before_starting_the_new() {
        let harness = Harness::start();
        harness.handle.patch(play(track("first"))).await.unwrap();
        harness.handle.patch(play(track("second"))).await.unwrap();

        let reasons: Vec<_> = harness
            .events()
            .into_iter()
            .filter_map(|event| match event {
                EmittedEvent::TrackEnd { reason, track, .. } => Some((reason, track.info.title)),
                _ => None,
            })
            .collect();
        assert_eq!(
            reasons,
            vec![(TrackEndReason::Replaced, "first".to_owned())]
        );
    }

    #[tokio::test]
    async fn no_replace_drops_the_request_and_keeps_the_current_track() {
        let harness = Harness::start();
        harness.handle.patch(play(track("first"))).await.unwrap();

        let player = harness
            .handle
            .patch(PatchRequest {
                track: Some(TrackChange::Play(Box::new(track("second")))),
                no_replace: true,
                ..Default::default()
            })
            .await
            .unwrap();

        assert_eq!(player.track.unwrap().info.title, "first");
    }

    /// The three-state `Omissible` in action: an explicit null clears the track.
    #[tokio::test]
    async fn clearing_the_track_stops_and_emits_stopped() {
        let harness = Harness::start();
        harness.handle.patch(play(track("first"))).await.unwrap();

        let player = harness
            .handle
            .patch(PatchRequest {
                track: Some(TrackChange::Clear),
                ..Default::default()
            })
            .await
            .unwrap();

        assert!(player.track.is_none());
        assert!(harness.events().iter().any(|event| matches!(
            event,
            EmittedEvent::TrackEnd {
                reason: TrackEndReason::Stopped,
                ..
            }
        )));
    }

    #[tokio::test]
    async fn stopping_an_empty_player_emits_nothing() {
        let harness = Harness::start();
        harness
            .handle
            .patch(PatchRequest {
                track: Some(TrackChange::Clear),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(harness.events().is_empty());
    }

    /// The PATCH response reports the requested position, not where the seek
    /// actually landed.
    #[tokio::test]
    async fn seek_reports_the_requested_position_optimistically() {
        let harness = Harness::start();
        harness.handle.patch(play(track("first"))).await.unwrap();

        let player = harness
            .handle
            .patch(PatchRequest {
                position: Omissible::Present(42_000),
                ..Default::default()
            })
            .await
            .unwrap();

        assert_eq!(player.state.position, 42_000);
        assert!(harness
            .engine
            .calls()
            .contains(&EngineCall::Seek { position_ms: 42_000 }));
    }

    #[tokio::test]
    async fn seeking_an_empty_player_does_nothing() {
        let harness = Harness::start();
        let player = harness
            .handle
            .patch(PatchRequest {
                position: Omissible::Present(42_000),
                ..Default::default()
            })
            .await
            .unwrap();

        assert_eq!(player.state.position, 0);
        assert!(!harness
            .engine
            .calls()
            .iter()
            .any(|call| matches!(call, EngineCall::Seek { .. })));
    }

    /// Volume is applied even in a request that also sets a track — the one field
    /// that is not deferred (`PlayerRestHandler.kt:156`).
    #[tokio::test]
    async fn volume_applies_alongside_a_new_track() {
        let harness = Harness::start();
        let player = harness
            .handle
            .patch(PatchRequest {
                volume: Omissible::Present(50),
                track: Some(TrackChange::Play(Box::new(track("first")))),
                ..Default::default()
            })
            .await
            .unwrap();

        assert_eq!(player.volume, 50);
        assert!(harness
            .engine
            .calls()
            .contains(&EngineCall::SetVolume { volume: 50 }));
    }

    /// `position` in a play request is a start offset, not a seek on the old track.
    #[tokio::test]
    async fn position_with_a_new_track_starts_it_at_that_offset() {
        let harness = Harness::start();
        harness.handle.patch(play(track("first"))).await.unwrap();
        harness.engine.clear();

        let player = harness
            .handle
            .patch(PatchRequest {
                position: Omissible::Present(30_000),
                track: Some(TrackChange::Play(Box::new(track("second")))),
                ..Default::default()
            })
            .await
            .unwrap();

        assert_eq!(player.state.position, 30_000);
        assert!(
            !harness
                .engine
                .calls()
                .iter()
                .any(|call| matches!(call, EngineCall::Seek { .. })),
            "a play request must start the new track at the offset, not seek the old one"
        );
        assert!(harness.engine.calls().iter().any(|call| matches!(
            call,
            EngineCall::Play {
                start_position_ms: 30_000,
                ..
            }
        )));
    }

    #[tokio::test]
    async fn voice_events_update_the_cache_and_push_an_update() {
        let harness = Harness::start();
        harness
            .handle
            .send_voice(VoiceUpdate::Connected { ping_ms: 21 })
            .unwrap();

        let player = harness.snapshot_until(|player| player.state.connected).await;
        assert_eq!(player.state.ping, 21);

        assert!(harness
            .drain()
            .iter()
            .any(|message| matches!(message, Message::PlayerUpdate { .. })));
    }

    #[tokio::test]
    async fn a_closed_voice_connection_emits_websocket_closed() {
        let harness = Harness::start();
        harness
            .handle
            .send_voice(VoiceUpdate::Closed {
                code: 4006,
                by_remote: true,
            })
            .unwrap();
        // A sentinel sent right after, on the same channel: FIFO delivery
        // within one channel guarantees the actor applies `Closed` before
        // this, so once this lands, `Closed`'s emission is already in the
        // sink — unlike `state.connected`/`ping`, which `Closed` sets to the
        // same values a fresh player already starts with, so they can't
        // distinguish "applied" from "not yet applied".
        const SENTINEL_PING: i64 = 4_006_000;
        harness
            .handle
            .send_voice(VoiceUpdate::Connected {
                ping_ms: SENTINEL_PING,
            })
            .unwrap();
        harness
            .snapshot_until(|player| player.state.ping == SENTINEL_PING)
            .await;

        assert!(harness.events().iter().any(|event| matches!(
            event,
            EmittedEvent::WebSocketClosed {
                code: 4006,
                by_remote: true,
                ..
            }
        )));
    }

    #[tokio::test]
    async fn a_track_that_never_started_ends_as_load_failed() {
        let harness = Harness::start();
        harness.handle.patch(play(track("first"))).await.unwrap();

        harness
            .handle
            .send(Command::Engine(EngineEvent::Failed {
                exception: lavalink_protocol::Exception::common("nope", "cause"),
                started: false,
            }))
            .await
            .unwrap();
        harness.handle.snapshot().await.unwrap();

        let events = harness.events();
        assert!(events
            .iter()
            .any(|event| matches!(event, EmittedEvent::TrackException { .. })));
        assert!(events.iter().any(|event| matches!(
            event,
            EmittedEvent::TrackEnd {
                reason: TrackEndReason::LoadFailed,
                ..
            }
        )));
    }

    #[tokio::test]
    async fn a_finished_track_ends_as_finished() {
        let harness = Harness::start();
        harness.handle.patch(play(track("first"))).await.unwrap();
        harness
            .handle
            .send(Command::Engine(EngineEvent::Finished))
            .await
            .unwrap();

        let player = harness.handle.snapshot().await.unwrap();
        assert!(player.track.is_none());
        assert!(harness.events().iter().any(|event| matches!(
            event,
            EmittedEvent::TrackEnd {
                reason: TrackEndReason::Finished,
                ..
            }
        )));
    }

    #[tokio::test]
    async fn playing_since_is_set_while_playing_and_cleared_when_not() {
        let harness = Harness::start();
        assert_eq!(harness.handle.playing_since_ms(), 0);

        harness.handle.patch(play(track("first"))).await.unwrap();
        assert!(harness.handle.playing_since_ms() > 0);

        // Pausing stops it, even though the track is still loaded.
        harness
            .handle
            .patch(PatchRequest {
                paused: Omissible::Present(true),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(harness.handle.playing_since_ms(), 0);

        harness
            .handle
            .patch(PatchRequest {
                paused: Omissible::Present(false),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(harness.handle.playing_since_ms() > 0);

        harness
            .handle
            .patch(PatchRequest {
                track: Some(TrackChange::Clear),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(harness.handle.playing_since_ms(), 0);
    }

    /// An unrelated command mid-playback (the tick-driven `EmitUpdate` is the real
    /// example) must not push `playing_since_ms` forward — otherwise a busy player
    /// would never look "usable" for `frameStats`.
    #[tokio::test]
    async fn playing_since_does_not_advance_on_an_unrelated_command() {
        let harness = Harness::start();
        harness.handle.patch(play(track("first"))).await.unwrap();
        let first = harness.handle.playing_since_ms();

        harness.handle.snapshot().await.unwrap();
        harness.handle.send(Command::EmitUpdate).await.unwrap();
        harness.handle.snapshot().await.unwrap();

        assert_eq!(harness.handle.playing_since_ms(), first);
    }

    /// A full queue that never drains (a wedged actor) must not hang `send`
    /// forever — `patch_player` maps `PlayerGone` to 503, which is only
    /// reachable if `send` itself eventually gives up.
    #[tokio::test(start_paused = true)]
    async fn send_reports_the_player_gone_when_a_full_queue_never_drains() {
        let sink = Arc::new(Sink::new());
        let engine = RecordingEngine::new();
        let (_actor, handle) = PlayerActor::new(
            123,
            Box::new(engine),
            sink,
            Duration::from_secs(10),
            Arc::new(std::sync::OnceLock::new()),
        );
        // _actor.run() is deliberately never spawned, so nothing ever drains
        // the queue below — the same "wedged actor" shape as a real stall.

        for _ in 0..COMMAND_CAPACITY {
            handle.commands.try_send(Command::EmitUpdate).unwrap();
        }

        let result = tokio::time::timeout(Duration::from_secs(60), handle.send(Command::EmitUpdate))
            .await
            .expect("send should give up on its own well before this outer bound");
        assert_eq!(result, Err(PlayerGone));
    }

    /// The bug: voice updates used to share the general `commands` queue
    /// (`Command::Voice`), so `ActorNotifier`'s `try_send` could be dropped by
    /// a burst of REST traffic that happened to fill it — misreporting
    /// `connected: false` until some later, unrelated transition came along
    /// to correct it. On its own channel, a full `commands` queue must have
    /// no bearing on whether a voice update can still be delivered.
    #[tokio::test]
    async fn a_full_command_queue_does_not_block_a_voice_update() {
        let sink = Arc::new(Sink::new());
        let engine = RecordingEngine::new();
        let (_actor, handle) = PlayerActor::new(
            123,
            Box::new(engine),
            sink,
            Duration::from_secs(10),
            Arc::new(std::sync::OnceLock::new()),
        );
        // _actor.run() is deliberately never spawned, so `commands` fills
        // without ever draining.

        for _ in 0..COMMAND_CAPACITY {
            handle.try_send(Command::EmitUpdate).unwrap();
        }
        assert!(
            handle.try_send(Command::EmitUpdate).is_err(),
            "the commands queue should now be completely full"
        );

        assert!(
            handle
                .send_voice(VoiceUpdate::Connected { ping_ms: 21 })
                .is_ok(),
            "a full commands queue must not block a voice update on its own channel"
        );
    }

    #[tokio::test]
    async fn destroy_stops_the_engine_and_ends_the_actor() {
        let harness = Harness::start();
        harness.handle.patch(play(track("first"))).await.unwrap();
        harness.handle.destroy().await.unwrap();

        assert!(harness.engine.calls().contains(&EngineCall::Shutdown));
        // Destroy publishes no track event.
        assert!(!harness
            .events()
            .iter()
            .any(|event| matches!(event, EmittedEvent::TrackEnd { .. })));
        assert_eq!(harness.handle.snapshot().await, Err(PlayerGone));
    }

    /// `STUCK_CHECK_INTERVAL` is a real-time tick, not `tokio::time`, so these wait
    /// on the wall clock rather than pausing it — same trade `stream.rs`'s
    /// reconnect test makes.
    fn past_one_check_interval() -> Duration {
        STUCK_CHECK_INTERVAL + Duration::from_millis(150)
    }

    #[tokio::test]
    async fn a_track_with_no_progress_past_the_threshold_is_reported_stuck() {
        let harness = Harness::start_with_stuck_threshold(Duration::from_millis(1));
        harness.handle.patch(play(track("first"))).await.unwrap();

        tokio::time::sleep(past_one_check_interval()).await;
        harness.handle.snapshot().await.unwrap();

        assert!(harness.events().iter().any(|event| matches!(
            event,
            EmittedEvent::TrackStuck {
                threshold_ms: 1,
                ..
            }
        )));
    }

    #[tokio::test]
    async fn stuck_is_reported_only_once_per_track() {
        let harness = Harness::start_with_stuck_threshold(Duration::from_millis(1));
        harness.handle.patch(play(track("first"))).await.unwrap();

        // Two full check intervals with no progress: only the first should emit.
        tokio::time::sleep(past_one_check_interval() * 2).await;
        harness.handle.snapshot().await.unwrap();

        let stuck_count = harness
            .events()
            .iter()
            .filter(|event| matches!(event, EmittedEvent::TrackStuck { .. }))
            .count();
        assert_eq!(stuck_count, 1);
    }

    #[tokio::test]
    async fn progress_lets_a_track_be_reported_stuck_again_later() {
        let harness = Harness::start_with_stuck_threshold(Duration::from_millis(1));
        harness.handle.patch(play(track("first"))).await.unwrap();

        tokio::time::sleep(past_one_check_interval()).await;
        harness.handle.snapshot().await.unwrap();
        assert!(harness
            .events()
            .iter()
            .any(|event| matches!(event, EmittedEvent::TrackStuck { .. })));

        harness
            .handle
            .send(Command::Engine(EngineEvent::Progress))
            .await
            .unwrap();

        tokio::time::sleep(past_one_check_interval()).await;
        harness.handle.snapshot().await.unwrap();
        assert!(
            harness
                .events()
                .iter()
                .any(|event| matches!(event, EmittedEvent::TrackStuck { .. })),
            "progress should reset the stuck flag, letting it fire again"
        );
    }

    #[tokio::test]
    async fn a_paused_track_is_never_reported_stuck() {
        let harness = Harness::start_with_stuck_threshold(Duration::from_millis(1));
        harness.handle.patch(play(track("first"))).await.unwrap();
        harness
            .handle
            .patch(PatchRequest {
                paused: Omissible::Present(true),
                ..Default::default()
            })
            .await
            .unwrap();

        tokio::time::sleep(past_one_check_interval() * 2).await;
        harness.handle.snapshot().await.unwrap();

        assert!(!harness
            .events()
            .iter()
            .any(|event| matches!(event, EmittedEvent::TrackStuck { .. })));
    }

    #[tokio::test]
    async fn an_idle_player_is_never_reported_stuck() {
        let harness = Harness::start_with_stuck_threshold(Duration::from_millis(1));

        tokio::time::sleep(past_one_check_interval() * 2).await;
        harness.handle.snapshot().await.unwrap();

        assert!(harness.events().is_empty());
    }
}
