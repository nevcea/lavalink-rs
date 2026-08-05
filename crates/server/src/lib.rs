//! A small Lavalink v4 compatible audio node.
//!
//! # What this is
//!
//! A port of Lavalink v4's server behaviour, with the wire kept identical and the
//! internals rebuilt. The governing rule is that anything a client can observe —
//! response bodies, status codes, event sequences — follows the original, including
//! where the original looks accidental. Improvements are confined to what a client
//! cannot see: concurrency, resource ownership, and the handful of places where the
//! original simply crashes.
//!
//! # What is fixed, and where to look
//!
//! | | |
//! |---|---|
//! | one session registry instead of two half-safe maps | [`session`] |
//! | resume ownership taken atomically, deadlines never orphaned | [`session`] |
//! | no blocking work under a per-guild lock | [`player::actor`] |
//! | voice and engine reports cannot be starved by REST traffic | [`player::actor`] |
//! | reads do not mutate state | [`player::state`] |
//! | bounded, prioritised outbound queue | [`sink`] |
//! | node-wide ticks instead of per-session schedulers | [`ticker`] |
//! | single-flight, cached loading off the async threads | [`loader`] |
//! | constant-time password comparison | [`auth`] |
//! | `User-Id` validated instead of asserted | [`ws`] |
//!
//! # What is not implemented
//!
//! Plugins, route planning and IP rotation. Each is advertised honestly rather than
//! stubbed: `/v4/info` lists only what really runs, and a request naming a filter
//! this node does not have is rejected with the original's 400. `MAINTENANCE.md`
//! records why for each. `timescale` used to belong on this list — see
//! `audio::filter`'s module docs for why it does not any more.

// The default limit overflows when the trait solver checks auto-trait bounds
// (Send/Sync) on the full REST router as one type — AppState pulls in
// songbird's connection/error types transitively, and that chain runs deep
// enough during a Router::oneshot call (used by rest::tests) to need more
// headroom than 128 gives.
#![recursion_limit = "256"]

pub mod audio;
pub mod auth;
pub mod config;
pub mod error;
pub mod loader;
pub mod player;
pub mod rest;
pub mod session;
pub mod sink;
pub mod state;
pub mod stats;
pub mod ticker;
pub mod voice;
pub mod ws;

pub use config::Config;
pub use state::AppState;

/// Locks a mutex, ignoring poisoning.
///
/// Every mutex in this crate guards a plain container with no invariant a panic
/// could half-break, so carrying on beats taking the whole node down with it.
pub(crate) fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Fixtures shared across this crate's test modules.
///
/// Not to be confused with [`audio::testing`], which holds fake collaborators for
/// the player actor. This is only for values that several unrelated modules need to
/// construct and none of them care about.
#[cfg(test)]
pub mod testing {
    use std::sync::{Arc, OnceLock};
    use std::time::Duration;

    use lavalink_protocol::player::{Track, TrackInfo};

    use crate::audio::testing::RecordingEngine;
    use crate::player::{PlayerActor, PlayerHandle, VoiceUpdateSlot};
    use crate::sink::Sink;
    use crate::voice::VoiceConnection;

    /// A fully-populated track, for tests that need one but do not care what is in
    /// it. `title` is the one field callers routinely tell apart, so it is the only
    /// parameter; anything asserting on the rest should build its own literal and
    /// say why.
    pub fn track(title: &str) -> Track {
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

    /// A player actor + voice connection pair, wired the way `AppState::player`
    /// wires production ones. `sink` is a parameter rather than always built
    /// fresh, so a test can inspect the messages the spawned actor emits through
    /// it (matching the session's own sink, in `AppState::player`'s wiring).
    pub fn dummy_pair(guild_id: u64, sink: Arc<Sink>) -> (PlayerHandle, Arc<VoiceConnection>) {
        let voice_updates: VoiceUpdateSlot = Arc::new(OnceLock::new());
        let voice = Arc::new(VoiceConnection::new(guild_id, 1, Arc::clone(&voice_updates)));
        let (actor, handle) = PlayerActor::new(
            guild_id,
            Box::new(RecordingEngine::new()),
            sink,
            Duration::from_secs(10),
            voice_updates,
        );
        tokio::spawn(actor.run());
        (handle, voice)
    }
}
