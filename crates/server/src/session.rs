//! The session registry.
//!
//! The original keeps two maps: `sessions` (a `ConcurrentHashMap`) and
//! `resumableSessions` (a plain `mutableMapOf`), touched from websocket callbacks,
//! a scheduler and the handshake thread (`SocketServer.kt:54,150,158,176,180`).
//! Only one of them is thread-safe, and a session's identity is spread across both.
//!
//! Here there is one map, and a session's lifecycle is one enum guarded by one lock:
//!
//! ```text
//! Open ──disconnect, resuming enabled──▶ Resumable{deadline} ──claim──▶ Open
//!   │                                          │
//!   └──disconnect, resuming disabled──▶ gone ◀──┴──deadline passes──
//! ```
//!
//! The second fix is [`SessionRegistry::claim_for_resume`]. The original's
//! `canResume` is a predicate that cancels the resume timeout as a side effect
//! (`SocketServer.kt:180`), so a handshake that then fails to complete leaves the
//! session with no timeout and no owner — a permanent leak. Here nothing is
//! cancelled by looking: ownership moves in a single compare-and-swap at the moment
//! the connection is established, and until that happens the deadline stands.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lavalink_protocol::message::Message;

use crate::player::PlayerHandle;
use crate::sink::{SendError, Sink};
use crate::voice::VoiceConnection;

/// The original's default, and what a session gets before any `PATCH /v4/sessions`.
const DEFAULT_RESUME_TIMEOUT_SECS: u64 = 60;

/// How long [`Session::shutdown`] waits on a single player's destroy before giving
/// up on it and moving on. `PlayerActor`'s own contract is that its loop never
/// awaits I/O, so a healthy destroy finishes near-instantly — this exists only to
/// cap the cost of an actor that never will.
const PLAYER_DESTROY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Open,
    Resumable { deadline: Instant },
}

pub struct Session {
    pub id: String,
    pub user_id: u64,
    pub client_name: Option<String>,
    pub sink: Arc<Sink>,
    resuming: AtomicBool,
    resume_timeout_secs: AtomicU64,
    /// One entry per guild: the player and the voice connection its engine was
    /// actually built with, inserted together in a single step (see
    /// [`Session::get_or_create_player`]) rather than as two independent maps.
    /// Two maps updated by two separate `entry().or_insert()` calls let two
    /// concurrent first-time requests for the same guild disagree about which of
    /// them won — the registered player's engine could end up holding a voice
    /// connection that was never the one `PATCH`'s `voice` field gets connected
    /// against, and the loser's actor (kept alive forever by its own engine's
    /// self-referencing event-channel sender) would leak permanently. One map
    /// filled in one step makes "who won" a single decision instead of two.
    guilds: Mutex<HashMap<u64, GuildPlayer>>,
    /// Flipped to `false` by [`Session::take_players`], under the same lock as
    /// `guilds`. Without it, a `get_or_create_player` racing a sweep-driven
    /// `take_players` on this same (still-`Arc`-alive) `Session` cannot tell "no
    /// players yet" from "just torn down" — an empty map looks the same either
    /// way — and would happily build a fresh player that nothing can ever reach
    /// again, because the session id that led here has already left the
    /// registry.
    alive: AtomicBool,
}

/// A guild's player, paired with the voice connection its engine holds.
struct GuildPlayer {
    handle: PlayerHandle,
    voice: Arc<VoiceConnection>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("id", &self.id)
            .field("user_id", &self.user_id)
            .field("client_name", &self.client_name)
            .finish_non_exhaustive()
    }
}

impl Session {
    fn new(id: String, user_id: u64, client_name: Option<String>) -> Self {
        Self {
            id,
            user_id,
            client_name,
            sink: Arc::new(Sink::new()),
            resuming: AtomicBool::new(false),
            resume_timeout_secs: AtomicU64::new(DEFAULT_RESUME_TIMEOUT_SECS),
            guilds: Mutex::new(HashMap::new()),
            alive: AtomicBool::new(true),
        }
    }

    pub fn resuming(&self) -> bool {
        self.resuming.load(Ordering::Relaxed)
    }

    pub fn set_resuming(&self, resuming: bool) {
        self.resuming.store(resuming, Ordering::Relaxed);
    }

    pub fn resume_timeout_secs(&self) -> u64 {
        self.resume_timeout_secs.load(Ordering::Relaxed)
    }

    pub fn set_resume_timeout_secs(&self, seconds: u64) {
        self.resume_timeout_secs.store(seconds, Ordering::Relaxed);
    }

    pub fn send(&self, message: Message) -> Result<(), SendError> {
        self.sink.send(message)
    }

    pub fn player(&self, guild_id: u64) -> Option<PlayerHandle> {
        self.lock_guilds().get(&guild_id).map(|guild| guild.handle.clone())
    }

    pub fn players(&self) -> Vec<PlayerHandle> {
        self.lock_guilds()
            .values()
            .map(|guild| guild.handle.clone())
            .collect()
    }

    /// Returns the guild's (player, voice) pair, building and inserting it with
    /// `build` if there is none yet.
    ///
    /// `build` runs at most once per guild, under the same lock that checks for
    /// and inserts the entry: a race between two first-time callers for the same
    /// guild is resolved as a single winner for *both* the player and the voice
    /// connection together, rather than as two independent decisions that could
    /// disagree about who won (see [`GuildPlayer`]'s docs for what that used to
    /// cost). The original builds the player *inside* `computeIfAbsent` and fires
    /// `onNewPlayer` from the mapping function (`SocketContext.kt:109-113`), which
    /// runs arbitrary listener code while holding the map's bin lock — reentrancy
    /// there is undefined behaviour for `ConcurrentHashMap`. Here construction is
    /// cheap and side-effect free (spawning the actor task is all `build` does
    /// beyond constructing values), so nothing unbounded runs under the lock.
    ///
    /// Returns `None` without calling `build` if the session has already been
    /// torn down by [`Session::take_players`] — otherwise a `PATCH` that is slow
    /// to reach this call (e.g. stuck resolving an identifier) could race a
    /// resume-deadline sweep and build a player into a session nothing can find
    /// again, since only the registry's session id, not this `Arc`, is what a
    /// later request can look sessions up by.
    pub fn get_or_create_player(
        &self,
        guild_id: u64,
        build: impl FnOnce() -> (PlayerHandle, Arc<VoiceConnection>),
    ) -> Option<(PlayerHandle, Arc<VoiceConnection>)> {
        let mut guilds = self.lock_guilds();
        if !self.alive.load(Ordering::Relaxed) {
            return None;
        }
        let guild = guilds.entry(guild_id).or_insert_with(|| {
            let (handle, voice) = build();
            GuildPlayer { handle, voice }
        });
        Some((guild.handle.clone(), Arc::clone(&guild.voice)))
    }

    pub fn voice(&self, guild_id: u64) -> Option<Arc<VoiceConnection>> {
        self.lock_guilds().get(&guild_id).map(|guild| Arc::clone(&guild.voice))
    }

    pub fn remove_player(&self, guild_id: u64) -> Option<PlayerHandle> {
        self.lock_guilds().remove(&guild_id).map(|guild| guild.handle)
    }

    /// Empties the guild map and marks the session dead to
    /// [`Session::get_or_create_player`], both under the same lock so the two
    /// can't race into a player neither state can see.
    pub fn take_players(&self) -> Vec<PlayerHandle> {
        let mut guilds = self.lock_guilds();
        self.alive.store(false, Ordering::Relaxed);
        std::mem::take(&mut *guilds)
            .into_values()
            .map(|guild| guild.handle)
            .collect()
    }

    /// Destroys every player the session holds, then closes its sink. The one
    /// teardown routine, shared by an explicit close (`SessionRegistry::destroy`)
    /// and a resume-deadline sweep (`ticker::shutdown_session`) — both need every
    /// actor, voice connection and pump thread gone, not just the session's own
    /// bookkeeping removed.
    ///
    /// Each player's destroy runs concurrently with the rest and is bounded by
    /// `PLAYER_DESTROY_TIMEOUT`, so one wedged actor can only ever cost this
    /// session that much time — not stall its own siblings, and not stall
    /// `ticker::sweep_tick`'s loop, which calls this once per expired session with
    /// nothing else bounding how long any single call may run.
    pub async fn shutdown(&self) {
        let destroys = self.take_players().into_iter().map(|player| async move {
            let _ = tokio::time::timeout(PLAYER_DESTROY_TIMEOUT, player.destroy()).await;
        });
        futures_util::future::join_all(destroys).await;
        self.sink.close();
    }

    fn lock_guilds(&self) -> std::sync::MutexGuard<'_, HashMap<u64, GuildPlayer>> {
        crate::lock(&self.guilds)
    }
}

#[derive(Debug)]
struct Entry {
    session: Arc<Session>,
    state: SessionState,
}

#[derive(Default)]
pub struct SessionRegistry {
    sessions: Mutex<HashMap<String, Entry>>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a new open session under a freshly generated id.
    pub fn open(&self, user_id: u64, client_name: Option<String>) -> Arc<Session> {
        let mut sessions = self.lock();
        let id = loop {
            let candidate = generate_session_id();
            if !sessions.contains_key(&candidate) {
                break candidate;
            }
        };

        let session = Arc::new(Session::new(id.clone(), user_id, client_name));
        sessions.insert(
            id,
            Entry {
                session: Arc::clone(&session),
                state: SessionState::Open,
            },
        );
        session
    }

    /// Takes ownership of a resumable session, in one atomic step.
    ///
    /// Returns `None` when the id is unknown, the session is currently open, or
    /// its deadline has already passed — in the first two cases its resume
    /// deadline, if any, is left running untouched; in the last, the entry is
    /// left for `sweep_expired` to remove rather than raced against it here.
    /// Without this check, a resume landing between two sweep ticks (up to
    /// `ticker::SWEEP_INTERVAL` late) could succeed after the deadline the client
    /// was promised — `sweep_expired` only runs once a second, so it is not a
    /// substitute for checking `now` at the moment of the claim itself.
    pub fn claim_for_resume(&self, id: &str, now: Instant) -> Option<Arc<Session>> {
        let mut sessions = self.lock();
        let entry = sessions.get_mut(id)?;
        match entry.state {
            SessionState::Resumable { deadline } if deadline > now => {}
            _ => return None,
        }
        entry.state = SessionState::Open;
        let session = Arc::clone(&entry.session);
        session.sink.resume();
        Some(session)
    }

    /// Any registered session, open or awaiting resume.
    ///
    /// REST requests are served in either state: the session is alive, only its
    /// websocket is gone.
    pub fn get(&self, id: &str) -> Option<Arc<Session>> {
        self.lock().get(id).map(|entry| Arc::clone(&entry.session))
    }

    pub fn state(&self, id: &str) -> Option<SessionState> {
        self.lock().get(id).map(|entry| entry.state)
    }

    pub fn all(&self) -> Vec<Arc<Session>> {
        self.lock()
            .values()
            .map(|entry| Arc::clone(&entry.session))
            .collect()
    }

    /// Handles a websocket closing.
    ///
    /// Returns the session to shut down, or `None` if it went to `Resumable` and
    /// should be left alone until it is claimed or expires.
    pub fn on_disconnect(&self, id: &str, now: Instant) -> Option<Arc<Session>> {
        let mut sessions = self.lock();
        let entry = sessions.get_mut(id)?;
        if !matches!(entry.state, SessionState::Open) {
            return None;
        }

        if entry.session.resuming() {
            let timeout = Duration::from_secs(entry.session.resume_timeout_secs());
            entry.state = SessionState::Resumable {
                deadline: now + timeout,
            };
            entry.session.sink.pause();
            return None;
        }

        sessions.remove(id).map(|entry| entry.session)
    }

    /// Removes sessions whose resume deadline has passed, or whose essential
    /// queue has overflowed while waiting to be resumed. Called from the global
    /// tick, which replaces the original's per-session scheduled executor.
    ///
    /// A connected session that stops draining essentials is caught by `ws.rs`'s
    /// `pump`, which closes it with 1008 the moment its sink overflows. A
    /// `Resumable` session has no websocket for anything to notice that on, so
    /// without this it would just keep silently dropping essential messages
    /// (`Sink::send`'s `SendError::Overflow`) for the rest of the resume window
    /// — defeating the reason resume exists. Treating an overflowing sink the
    /// same as an expired deadline here gives it the same fate a connected
    /// session gets, instead of a silent, unbounded event gap.
    ///
    /// Scans and removes as two separate critical sections rather than one held
    /// across the whole pass — every other session lookup on the node (every REST
    /// request, every websocket handshake) shares this same registry lock, so
    /// holding it for an uninterrupted O(sessions) scan plus O(expired) removal
    /// once a second stalls all of them for that whole span. Splitting leaves a
    /// gap between deciding a session is expired and removing it, so
    /// [`Self::remove_if_still_expired`] re-checks the same condition under its
    /// own lock before removing — otherwise a resume that legitimately claims a
    /// session in that gap (turning it `Open`) would be undone by a removal
    /// decided against the stale, no-longer-true `Resumable` state.
    pub fn sweep_expired(&self, now: Instant) -> Vec<Arc<Session>> {
        let expired: Vec<String> = {
            let sessions = self.lock();
            sessions
                .iter()
                .filter(|(_, entry)| Self::is_expired(entry, now))
                .map(|(id, _)| id.clone())
                .collect()
        };

        expired
            .iter()
            .filter_map(|id| self.remove_if_still_expired(id, now))
            .collect()
    }

    /// Removes `id` only if it is still expired under `now` at the moment of
    /// removal — see [`Self::sweep_expired`]'s docs for why a stale decision from
    /// an earlier scan cannot be trusted on its own.
    fn remove_if_still_expired(&self, id: &str, now: Instant) -> Option<Arc<Session>> {
        let mut sessions = self.lock();
        match sessions.get(id) {
            Some(entry) if Self::is_expired(entry, now) => {
                sessions.remove(id).map(|entry| entry.session)
            }
            _ => None,
        }
    }

    fn is_expired(entry: &Entry, now: Instant) -> bool {
        match entry.state {
            SessionState::Resumable { deadline } => {
                deadline <= now || entry.session.sink.is_overflowing()
            }
            SessionState::Open => false,
        }
    }

    /// Unconditional removal, for an explicit close or shutdown.
    ///
    /// Tears down every player the session holds before returning: this is the
    /// only teardown path a still-connected websocket has (`ws.rs`'s overflow
    /// close), so it must do the full job a resume-deadline sweep does —
    /// otherwise a session's actors, voice connections and pump threads outlive
    /// the session id that was the only way to reach them.
    pub async fn destroy(&self, id: &str) -> Option<Arc<Session>> {
        let session = self.lock().remove(id).map(|entry| entry.session)?;
        session.shutdown().await;
        Some(session)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        crate::lock(&self.sessions)
    }
}

/// 16 characters from `[a-z0-9]`, as the original generates
/// (`SocketServer.kt:57,88`). Clients treat it as opaque, but keeping the alphabet
/// avoids surprising anything that validates it.
fn generate_session_id() -> String {
    use rand::Rng as _;
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    (0..16)
        .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    use crate::audio::testing::{EngineCall, RecordingEngine};
    use crate::player::PlayerActor;
    use crate::voice::VoiceConnection;

    fn dummy_pair(guild_id: u64) -> (PlayerHandle, Arc<VoiceConnection>) {
        let events: crate::player::EventSlot = Arc::new(std::sync::OnceLock::new());
        let voice = Arc::new(VoiceConnection::new(guild_id, 1, Arc::clone(&events)));
        let (actor, handle) = PlayerActor::new(
            guild_id,
            Box::new(RecordingEngine::new()),
            Arc::new(crate::sink::Sink::new()),
            Duration::from_secs(10),
        );
        tokio::spawn(actor.run());
        (handle, voice)
    }

    /// The race `AppState::player` used to be exposed to: two first-time callers
    /// for the same guild each building their own (player, voice) pair, with the
    /// registered player possibly ending up paired with a voice connection built
    /// by the *other* caller. `get_or_create_player` closes this by making "who
    /// won" one decision instead of two — `build` runs at most once per guild, and
    /// every caller gets back the exact pair that was inserted.
    #[tokio::test]
    async fn get_or_create_player_builds_at_most_once_and_returns_one_pair() {
        let session = Session::new("s".into(), 1, None);
        let build_calls = Arc::new(AtomicUsize::new(0));
        let guild = 42;

        let build = |calls: &Arc<AtomicUsize>| {
            let calls = Arc::clone(calls);
            move || {
                calls.fetch_add(1, Ordering::SeqCst);
                dummy_pair(guild)
            }
        };

        let (handle_1, voice_1) =
            session.get_or_create_player(guild, build(&build_calls)).unwrap();
        let (handle_2, voice_2) =
            session.get_or_create_player(guild, build(&build_calls)).unwrap();

        assert_eq!(
            build_calls.load(Ordering::SeqCst),
            1,
            "a second caller for the same guild must not build its own pair"
        );
        assert!(
            Arc::ptr_eq(&voice_1, &voice_2),
            "every caller must get back the same voice connection as the registered player"
        );
        assert_eq!(handle_1.guild_id, handle_2.guild_id);
        assert!(Arc::ptr_eq(&session.voice(guild).unwrap(), &voice_1));
    }

    /// The race a slow `PATCH` could hit: `take_players` (a resume sweep or an
    /// overflow close) runs while the request is still resolving a track, then
    /// the request reaches `get_or_create_player`. It must not build a player
    /// into a session nothing can look up again instead of reporting failure.
    #[tokio::test]
    async fn get_or_create_player_refuses_after_take_players() {
        let session = Session::new("s".into(), 1, None);
        let build_calls = Arc::new(AtomicUsize::new(0));
        let guild = 7;

        assert!(session.take_players().is_empty());

        let calls = Arc::clone(&build_calls);
        let result = session.get_or_create_player(guild, move || {
            calls.fetch_add(1, Ordering::SeqCst);
            dummy_pair(guild)
        });

        assert!(result.is_none(), "a torn-down session must not build a new player");
        assert_eq!(build_calls.load(Ordering::SeqCst), 0, "build must not run either");
    }

    fn resumable_session(registry: &SessionRegistry) -> Arc<Session> {
        let session = registry.open(1, None);
        session.set_resuming(true);
        session.set_resume_timeout_secs(60);
        assert!(registry.on_disconnect(&session.id, Instant::now()).is_none());
        session
    }

    #[test]
    fn ids_are_sixteen_lowercase_alphanumerics() {
        let id = generate_session_id();
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
    }

    #[test]
    fn a_session_without_resuming_is_destroyed_on_disconnect() {
        let registry = SessionRegistry::new();
        let session = registry.open(1, None);
        let id = session.id.clone();

        assert!(registry.on_disconnect(&id, Instant::now()).is_some());
        assert!(registry.get(&id).is_none());
    }

    #[test]
    fn a_resuming_session_survives_disconnect_and_can_be_claimed() {
        let registry = SessionRegistry::new();
        let session = resumable_session(&registry);

        assert!(matches!(
            registry.state(&session.id),
            Some(SessionState::Resumable { .. })
        ));
        assert!(session.sink.is_paused());

        let claimed = registry.claim_for_resume(&session.id, Instant::now()).unwrap();
        assert!(Arc::ptr_eq(&claimed, &session));
        assert_eq!(registry.state(&session.id), Some(SessionState::Open));
        assert!(!session.sink.is_paused());
    }

    /// Only one connection can take a resumable session.
    #[test]
    fn a_session_can_only_be_claimed_once() {
        let registry = SessionRegistry::new();
        let session = resumable_session(&registry);

        assert!(registry.claim_for_resume(&session.id, Instant::now()).is_some());
        assert!(registry.claim_for_resume(&session.id, Instant::now()).is_none());
    }

    /// A resume arriving after its deadline must be rejected even if the
    /// per-second `sweep_expired` tick hasn't gotten to it yet — otherwise a
    /// client could resume up to `ticker::SWEEP_INTERVAL` late.
    #[test]
    fn a_claim_past_its_own_deadline_is_rejected_even_if_unswept() {
        let registry = SessionRegistry::new();
        let session = registry.open(1, None);
        session.set_resuming(true);
        session.set_resume_timeout_secs(60);

        let disconnected_at = Instant::now();
        registry.on_disconnect(&session.id, disconnected_at);

        // Just past the deadline, but nothing has swept it yet: still in the map.
        let past_deadline = disconnected_at + Duration::from_secs(61);
        assert!(registry.get(&session.id).is_some());

        assert!(
            registry.claim_for_resume(&session.id, past_deadline).is_none(),
            "a resume past its deadline must not succeed just because the sweep hasn't run yet"
        );
        assert!(
            matches!(
                registry.state(&session.id),
                Some(SessionState::Resumable { .. })
            ),
            "a rejected late claim must leave the entry for sweep_expired, not touch it"
        );
    }

    /// A failed claim attempt must not cancel the deadline.
    #[test]
    fn looking_at_a_session_does_not_disarm_its_deadline() {
        let registry = SessionRegistry::new();
        let session = registry.open(1, None);
        session.set_resuming(true);
        session.set_resume_timeout_secs(60);

        let disconnected_at = Instant::now();
        registry.on_disconnect(&session.id, disconnected_at);

        // A handshake that inspects the session and then goes nowhere.
        assert!(registry.get(&session.id).is_some());
        assert!(registry.state(&session.id).is_some());

        let expired = registry.sweep_expired(disconnected_at + Duration::from_secs(61));
        assert_eq!(expired.len(), 1, "the session must still expire on time");
        assert!(registry.get(&session.id).is_none());
    }

    #[test]
    fn sweeping_leaves_open_and_unexpired_sessions_alone() {
        let registry = SessionRegistry::new();
        let open = registry.open(1, None);
        let waiting = resumable_session(&registry);

        let expired = registry.sweep_expired(Instant::now());
        assert!(expired.is_empty());
        assert!(registry.get(&open.id).is_some());
        assert!(registry.get(&waiting.id).is_some());
    }

    /// While `Resumable`, nothing has a websocket to notice an overflowing sink
    /// the way `ws.rs`'s `pump` does for a connected one — without this, an
    /// overflowing resumable session would just keep dropping essential messages
    /// silently until its deadline, however far off that still is.
    #[test]
    fn an_overflowing_resumable_session_is_swept_before_its_deadline() {
        use lavalink_protocol::message::{EmittedEvent, Message};
        use lavalink_protocol::player::{Track, TrackInfo};

        let registry = SessionRegistry::new();
        let session = registry.open(1, None);
        session.set_resuming(true);
        session.set_resume_timeout_secs(3600); // far from expiring on its own
        registry.on_disconnect(&session.id, Instant::now());

        let event = || {
            Message::Event(EmittedEvent::TrackStart {
                guild_id: "1".into(),
                track: Box::new(Track::new(
                    "e".into(),
                    TrackInfo {
                        identifier: "i".into(),
                        is_seekable: true,
                        author: "a".into(),
                        length: 1,
                        is_stream: false,
                        position: 0,
                        title: "t".into(),
                        uri: None,
                        source_name: "http".into(),
                        artwork_url: None,
                        isrc: None,
                    },
                )),
            })
        };
        while !session.sink.is_overflowing() {
            let _ = session.sink.send(event());
        }

        let expired = registry.sweep_expired(Instant::now());
        assert_eq!(expired.len(), 1, "an overflowing resumable session must be swept");
        assert!(registry.get(&session.id).is_none());
    }

    /// The race `sweep_expired`'s two-phase split opens up: an overflowing
    /// session (deadline still far off, so `claim_for_resume` has no reason of
    /// its own to reject it) is decided expired by the scan phase, then
    /// successfully claimed for resume — turning it `Open` — before the
    /// matching removal runs. Unlike a deadline-expired session (whose claim
    /// would fail on `claim_for_resume`'s own deadline check), nothing else
    /// protects an overflow-expired one, so the removal must re-check expiry
    /// itself rather than trust the scan's now-stale verdict.
    #[test]
    fn a_session_resumed_between_scan_and_removal_is_not_torn_down() {
        use lavalink_protocol::message::{EmittedEvent, Message};
        use lavalink_protocol::player::{Track, TrackInfo};

        let registry = SessionRegistry::new();
        let session = registry.open(1, None);
        session.set_resuming(true);
        session.set_resume_timeout_secs(3600); // far from expiring on its own
        registry.on_disconnect(&session.id, Instant::now());

        let event = || {
            Message::Event(EmittedEvent::TrackStart {
                guild_id: "1".into(),
                track: Box::new(Track::new(
                    "e".into(),
                    TrackInfo {
                        identifier: "i".into(),
                        is_seekable: true,
                        author: "a".into(),
                        length: 1,
                        is_stream: false,
                        position: 0,
                        title: "t".into(),
                        uri: None,
                        source_name: "http".into(),
                        artwork_url: None,
                        isrc: None,
                    },
                )),
            })
        };
        while !session.sink.is_overflowing() {
            let _ = session.sink.send(event());
        }

        let now = Instant::now();
        // What `sweep_expired`'s scan phase would have decided moments earlier:
        // this session is expired (by overflow, not deadline).
        assert!(registry
            .lock()
            .get(&session.id)
            .is_some_and(|entry| SessionRegistry::is_expired(entry, now)));

        // The claim that lands in the gap between scan and removal. Succeeds
        // because the deadline itself is nowhere near `now`.
        assert!(registry.claim_for_resume(&session.id, now).is_some());
        assert_eq!(registry.state(&session.id), Some(SessionState::Open));

        // The stale removal decision must see the session is `Open` now and
        // leave it alone, instead of tearing down a session just resumed.
        assert!(registry.remove_if_still_expired(&session.id, now).is_none());
        assert!(registry.get(&session.id).is_some());
        assert_eq!(registry.state(&session.id), Some(SessionState::Open));
    }

    #[test]
    fn a_claimed_session_no_longer_expires() {
        let registry = SessionRegistry::new();
        let session = resumable_session(&registry);
        let claimed_at = Instant::now();

        registry.claim_for_resume(&session.id, claimed_at).unwrap();

        let expired = registry.sweep_expired(claimed_at + Duration::from_secs(3600));
        assert!(expired.is_empty());
        assert!(registry.get(&session.id).is_some());
    }

    #[test]
    fn rest_requests_still_resolve_a_resumable_session() {
        let registry = SessionRegistry::new();
        let session = resumable_session(&registry);
        assert!(registry.get(&session.id).is_some());
    }

    #[test]
    fn disconnecting_a_session_that_is_already_waiting_is_a_no_op() {
        let registry = SessionRegistry::new();
        let session = resumable_session(&registry);
        assert!(registry.on_disconnect(&session.id, Instant::now()).is_none());
        assert!(matches!(
            registry.state(&session.id),
            Some(SessionState::Resumable { .. })
        ));
    }

    /// The bug this fix targets: `shutdown` used to await each player's
    /// `destroy()` in sequence with nothing bounding either call, so one wedged
    /// actor (here, one whose run loop is never spawned, so its command channel
    /// is never drained) blocked every sibling's destroy too — and, since
    /// `ticker::sweep_tick` calls this once per expired session with nothing else
    /// bounding it, would have stalled sweeping for the whole node forever.
    #[tokio::test(start_paused = true)]
    async fn shutdown_does_not_let_one_wedged_player_block_its_siblings() {
        let session = Session::new("s".into(), 1, None);

        // A player whose actor loop never runs: `destroy()`'s command send
        // succeeds (there's room in the channel) but its oneshot reply never
        // arrives, so it can only ever be recovered by `PLAYER_DESTROY_TIMEOUT`.
        let wedged_events: crate::player::EventSlot = Arc::new(std::sync::OnceLock::new());
        let wedged_voice = Arc::new(VoiceConnection::new(1, 1, Arc::clone(&wedged_events)));
        let (_wedged_actor, wedged_handle) = PlayerActor::new(
            1,
            Box::new(RecordingEngine::new()),
            Arc::new(crate::sink::Sink::new()),
            Duration::from_secs(10),
        );
        session
            .get_or_create_player(1, || (wedged_handle, wedged_voice))
            .unwrap();

        // A healthy player alongside it, whose engine is inspected afterward to
        // confirm its destroy actually completed rather than being starved by
        // the wedged one.
        let healthy_engine = RecordingEngine::new();
        let healthy_events: crate::player::EventSlot = Arc::new(std::sync::OnceLock::new());
        let healthy_voice = Arc::new(VoiceConnection::new(2, 1, Arc::clone(&healthy_events)));
        let (healthy_actor, healthy_handle) = PlayerActor::new(
            2,
            Box::new(healthy_engine.clone()),
            Arc::new(crate::sink::Sink::new()),
            Duration::from_secs(10),
        );
        tokio::spawn(healthy_actor.run());
        session
            .get_or_create_player(2, || (healthy_handle, healthy_voice))
            .unwrap();

        let shutdown = tokio::time::timeout(Duration::from_secs(30), session.shutdown()).await;
        assert!(
            shutdown.is_ok(),
            "shutdown must finish within PLAYER_DESTROY_TIMEOUT even with one wedged player, \
             not hang forever"
        );
        assert!(
            healthy_engine.calls().contains(&EngineCall::Shutdown),
            "a healthy sibling's destroy must still complete, not be starved by the wedged one"
        );
    }
}
