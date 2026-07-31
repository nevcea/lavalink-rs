//! Shared server state.

use std::sync::Arc;
use std::time::{Duration, Instant};

use lavalink_protocol::info::{Git, Info, Version};

use crate::audio::stream::StreamOpener;
use crate::audio::{Engine, PipelineEngine};
use crate::config::Config;
use crate::loader::Loader;
use crate::player::{PlayerActor, PlayerHandle};
use crate::session::{Session, SessionRegistry};
use crate::stats::StatsCollector;
use crate::voice::VoiceConnection;

/// The crate's own build version. Used only for self-identification strings
/// (`Info.jvm`, `Info.lavaplayer`) — clients don't parse those beyond display.
const SEMVER: &str = env!("CARGO_PKG_VERSION");

/// What `/version` and `Info.version` report. Clients gate on `version.major < 4`
/// and refuse to connect below it, so this must track the Lavalink protocol this
/// node speaks, not the crate's own version. Reported as an exact `4.0.0` rather
/// than `CARGO_PKG_VERSION`: this node speaks the v4 wire protocol, but a
/// pre-release-shaped string (e.g. `4.0.0-rs.0.1.0`) sorts *below* `4.0.0` under
/// semver, and a client checking `>= 4.0.0` would reject it. `4.0.0` is the floor
/// of the wire contract this node implements and claims nothing added by later
/// 4.0.x releases.
const PROTOCOL_VERSION: &str = "4.0.0";

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub sessions: Arc<SessionRegistry>,
    pub loader: Arc<Loader>,
    pub stats: Arc<StatsCollector>,
    pub info: Arc<Info>,
    /// Opens byte streams at playback time. Shared by every player.
    pub opener: Arc<StreamOpener>,
    /// Filter names a `PATCH player` is rejected for naming. Fixed at startup, and
    /// the complement of `info.filters` — computed here once so the two cannot
    /// disagree, and so a request carrying filters does not rebuild the list.
    pub disabled_filters: Arc<[String]>,
}

impl AppState {
    pub fn new(
        config: Config,
        loader: Loader,
        opener: StreamOpener,
        started_at: Instant,
    ) -> Self {
        let config = Arc::new(config);
        let loader = Arc::new(loader);

        let info = Info {
            version: Version::from_semver(PROTOCOL_VERSION),
            // No build-time injection; the field exists because clients read it.
            build_time: 0,
            git: Git {
                branch: String::new(),
                commit: String::new(),
                commit_time: 0,
            },
            // There is no JVM and no lavaplayer. Reporting what we actually are is
            // more useful than an invented version string, and no client parses
            // these beyond displaying them.
            jvm: format!("none (lavalink-rs {SEMVER})"),
            lavaplayer: format!("none (lavalink-rs {SEMVER})"),
            source_managers: loader.source_names(),
            // Only what is really implemented and enabled.
            filters: config.enabled_filters(),
            plugins: Vec::new(),
        };

        Self {
            disabled_filters: config.disabled_filters().into(),
            config,
            sessions: Arc::new(SessionRegistry::new()),
            loader,
            stats: Arc::new(StatsCollector::new(started_at)),
            info: Arc::new(info),
            opener: Arc::new(opener),
        }
    }

    /// Returns the guild's player and its voice connection, creating and spawning
    /// them if there is none.
    ///
    /// Construction happens inside `build`, which `Session::get_or_create_player`
    /// runs at most once per guild under its own lock — nothing in `build` awaits,
    /// so nothing blocks while it is held. This is what keeps a race between two
    /// first-time callers for the same guild from registering a player from one
    /// caller alongside a voice connection from the other: see
    /// `Session::get_or_create_player`'s docs for what that used to cost.
    ///
    /// The pair is returned together, not just the handle, so a caller can't
    /// re-derive the voice connection with a second, independent `session.voice`
    /// lookup later — doing so would reopen the exact race this pairing exists to
    /// close, since a session teardown between the two lookups would silently
    /// return a different (or no) answer the second time.
    ///
    /// Returns `None` if the session was torn down (resume deadline swept, or an
    /// overflowing sink closed) while this call was reaching the actor build —
    /// see `Session::get_or_create_player`'s docs.
    pub fn player(
        &self,
        session: &Arc<Session>,
        guild_id: u64,
    ) -> Option<(PlayerHandle, Arc<VoiceConnection>)> {
        let config = Arc::clone(&self.config);
        let opener = Arc::clone(&self.opener);
        let sink = Arc::clone(&session.sink);
        let user_id = session.user_id;
        let runtime = tokio::runtime::Handle::current();

        session.get_or_create_player(guild_id, move || {
            // The engine and the voice connection both report to an actor that
            // does not exist yet, so they share slots that `PlayerActor::new`
            // fills in — one per channel, since voice transitions get their own
            // (see `PlayerHandle::voice_updates`'s docs).
            let events = crate::player::EventSlot::default();
            let voice_updates = crate::player::VoiceUpdateSlot::default();
            let voice = Arc::new(VoiceConnection::new(
                guild_id,
                user_id,
                Arc::clone(&voice_updates),
            ));
            let engine: Box<dyn Engine> = Box::new(PipelineEngine::new(
                guild_id,
                config.lavalink.server.frame_buffer_duration_ms,
                Arc::clone(&voice),
                opener,
                events,
                runtime,
            ));

            let stuck_threshold =
                Duration::from_millis(config.lavalink.server.track_stuck_threshold_ms);
            let (actor, handle) =
                PlayerActor::new(guild_id, engine, sink, stuck_threshold, voice_updates);
            tokio::spawn(actor.run());

            (handle, voice)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> AppState {
        let mut config = Config::default();
        config.lavalink.server.password = "test".into();
        AppState::new(
            config,
            Loader::new(Vec::new()),
            StreamOpener::default(),
            Instant::now(),
        )
    }

    #[test]
    fn info_reports_the_v4_wire_protocol_not_the_crate_version() {
        let state = state();
        // Clients gate on `version.major < 4` and refuse to connect below it.
        assert_eq!(state.info.version.semver, "4.0.0");
        assert_eq!(state.info.version.major, 4);
    }

    #[test]
    fn info_advertises_only_implemented_filters() {
        let state = state();
        assert_eq!(
            state.info.filters,
            vec![
                "volume",
                "equalizer",
                "karaoke",
                "tremolo",
                "vibrato",
                "distortion",
                "rotation",
                "channelMix",
                "lowPass",
            ]
        );
        assert!(state.info.plugins.is_empty());
    }

    #[test]
    fn info_advertises_only_configured_sources() {
        let state = state();
        assert!(state.info.source_managers.is_empty());
    }

    #[tokio::test]
    async fn asking_for_a_player_twice_gives_the_same_one() {
        let state = state();
        let session = state.sessions.open(1, None);

        let (first, _voice) = state.player(&session, 123).unwrap();
        let (second, _voice) = state.player(&session, 123).unwrap();
        assert_eq!(first.guild_id, second.guild_id);
        assert_eq!(session.players().len(), 1);
    }

    #[tokio::test]
    async fn different_guilds_get_different_players() {
        let state = state();
        let session = state.sessions.open(1, None);

        state.player(&session, 1);
        state.player(&session, 2);
        assert_eq!(session.players().len(), 2);
    }

    /// The same call that returns the handle also returns its voice connection —
    /// a second, independent lookup of the connection by guild id is exactly the
    /// TOCTOU this pairing exists to prevent, which is why no such accessor exists
    /// on `Session` (see `AppState::player`'s doc comment).
    #[tokio::test]
    async fn player_returns_its_own_voice_connection_paired() {
        let state = state();
        let session = state.sessions.open(1, None);

        let (handle, voice) = state.player(&session, 123).unwrap();
        // Looked up again through the same call, which is the only way to reach a
        // registered voice connection — there is deliberately no accessor that
        // returns one on its own.
        let (_, again) = state.player(&session, 123).unwrap();
        assert_eq!(Arc::as_ptr(&voice), Arc::as_ptr(&again));
        assert_eq!(handle.guild_id, 123);
    }
}
