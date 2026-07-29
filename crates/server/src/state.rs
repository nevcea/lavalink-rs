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
            config,
            sessions: Arc::new(SessionRegistry::new()),
            loader,
            stats: Arc::new(StatsCollector::new(started_at)),
            info: Arc::new(info),
            opener: Arc::new(opener),
        }
    }

    /// Returns the guild's player, creating and spawning it if there is none.
    ///
    /// Construction happens outside the registry's lock and the actor task is
    /// spawned before insertion, so nothing runs while the map is held. If two
    /// callers race, one handle wins and the loser's actor exits when its unused
    /// sender drops.
    pub fn player(&self, session: &Arc<Session>, guild_id: u64) -> PlayerHandle {
        if let Some(existing) = session.player(guild_id) {
            return existing;
        }

        // The engine and the voice connection both report to an actor that does not
        // exist yet, so they share a slot that `PlayerActor::new` fills in.
        let events = crate::player::EventSlot::default();
        let voice = Arc::new(VoiceConnection::new(
            guild_id,
            session.user_id,
            Arc::clone(&events),
        ));
        let engine: Box<dyn Engine> = Box::new(PipelineEngine::new(
            guild_id,
            self.config.lavalink.server.frame_buffer_duration_ms,
            Arc::clone(&voice),
            Arc::clone(&self.opener),
            events,
            tokio::runtime::Handle::current(),
        ));

        let stuck_threshold =
            Duration::from_millis(self.config.lavalink.server.track_stuck_threshold_ms);
        let (actor, handle) =
            PlayerActor::new(guild_id, engine, Arc::clone(&session.sink), stuck_threshold);
        tokio::spawn(actor.run());

        session.insert_voice(guild_id, voice);
        session.insert_player(guild_id, handle)
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

        let first = state.player(&session, 123);
        let second = state.player(&session, 123);
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
}
