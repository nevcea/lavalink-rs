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
    players: Mutex<HashMap<u64, PlayerHandle>>,
    /// Kept alongside the players so a `PATCH` carrying a `voice` field can await
    /// the connection *before* the actor is told about it — which is what lets a
    /// failure become a status code instead of being swallowed.
    voices: Mutex<HashMap<u64, Arc<VoiceConnection>>>,
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
            players: Mutex::new(HashMap::new()),
            voices: Mutex::new(HashMap::new()),
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
        self.lock_players().get(&guild_id).cloned()
    }

    pub fn players(&self) -> Vec<PlayerHandle> {
        self.lock_players().values().cloned().collect()
    }

    /// Inserts a player if the guild has none, returning the handle either way.
    ///
    /// The original builds the player *inside* `computeIfAbsent` and fires
    /// `onNewPlayer` from the mapping function (`SocketContext.kt:109-113`), which
    /// runs arbitrary listener code while holding the map's bin lock — reentrancy
    /// there is undefined behaviour for `ConcurrentHashMap`. Here construction is
    /// cheap and side-effect free (spawning the actor task is the caller's job
    /// before it gets here), so nothing runs under the lock.
    pub fn insert_player(&self, guild_id: u64, handle: PlayerHandle) -> PlayerHandle {
        self.lock_players()
            .entry(guild_id)
            .or_insert(handle)
            .clone()
    }

    pub fn insert_voice(&self, guild_id: u64, voice: Arc<VoiceConnection>) {
        self.lock_voices().entry(guild_id).or_insert(voice);
    }

    pub fn voice(&self, guild_id: u64) -> Option<Arc<VoiceConnection>> {
        self.lock_voices().get(&guild_id).cloned()
    }

    pub fn remove_player(&self, guild_id: u64) -> Option<PlayerHandle> {
        self.lock_voices().remove(&guild_id);
        self.lock_players().remove(&guild_id)
    }

    pub fn take_players(&self) -> Vec<PlayerHandle> {
        self.lock_voices().clear();
        std::mem::take(&mut *self.lock_players())
            .into_values()
            .collect()
    }

    fn lock_players(&self) -> std::sync::MutexGuard<'_, HashMap<u64, PlayerHandle>> {
        crate::lock(&self.players)
    }

    fn lock_voices(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Arc<VoiceConnection>>> {
        crate::lock(&self.voices)
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
    /// Returns `None` when the id is unknown or the session is currently open — in
    /// which case its resume deadline, if any, is left running untouched.
    pub fn claim_for_resume(&self, id: &str) -> Option<Arc<Session>> {
        let mut sessions = self.lock();
        let entry = sessions.get_mut(id)?;
        if !matches!(entry.state, SessionState::Resumable { .. }) {
            return None;
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

    /// Removes sessions whose resume deadline has passed. Called from the global
    /// tick, which replaces the original's per-session scheduled executor.
    pub fn sweep_expired(&self, now: Instant) -> Vec<Arc<Session>> {
        let mut sessions = self.lock();
        let expired: Vec<String> = sessions
            .iter()
            .filter(|(_, entry)| match entry.state {
                SessionState::Resumable { deadline } => deadline <= now,
                SessionState::Open => false,
            })
            .map(|(id, _)| id.clone())
            .collect();

        expired
            .into_iter()
            .filter_map(|id| sessions.remove(&id))
            .map(|entry| entry.session)
            .collect()
    }

    /// Unconditional removal, for an explicit close or shutdown.
    pub fn destroy(&self, id: &str) -> Option<Arc<Session>> {
        let session = self.lock().remove(id).map(|entry| entry.session)?;
        session.sink.close();
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

        let claimed = registry.claim_for_resume(&session.id).unwrap();
        assert!(Arc::ptr_eq(&claimed, &session));
        assert_eq!(registry.state(&session.id), Some(SessionState::Open));
        assert!(!session.sink.is_paused());
    }

    /// Only one connection can take a resumable session.
    #[test]
    fn a_session_can_only_be_claimed_once() {
        let registry = SessionRegistry::new();
        let session = resumable_session(&registry);

        assert!(registry.claim_for_resume(&session.id).is_some());
        assert!(registry.claim_for_resume(&session.id).is_none());
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

    #[test]
    fn a_claimed_session_no_longer_expires() {
        let registry = SessionRegistry::new();
        let session = resumable_session(&registry);
        let claimed_at = Instant::now();

        registry.claim_for_resume(&session.id).unwrap();

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
}
