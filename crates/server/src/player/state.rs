//! Player state and its transitions.
//!
//! Two state machines live here and they deliberately do not talk to each other:
//!
//! • Playback — ours. The actor is the sole authority.
//! • VoiceConnection — not ours. It is a cache of what the voice layer last
//!   told us, updated only by events. Nothing here ever asks the voice layer a
//!   question, which is what makes the read path side-effect free: the original's
//!   sendPlayerUpdate calls getMediaConnection, which creates a connection if
//!   none exists, so merely reporting state changes it.
//!
//! Illegal transitions are rejected internally, but that rejection never reaches the
//! client — a path the original silently no-ops stays a silent no-op here.

use std::time::Instant;

use lavalink_protocol::filters::Filters;
use lavalink_protocol::player::{Player, PlayerState as WirePlayerState, Track, VoiceState};

/// What the player is doing. Idle and Stopped look identical on the wire
/// (track: null) but differ in whether a track has ever run, which decides
/// whether stopping should emit a TrackEndEvent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Playback {
    /// Created, never given a track.
    Idle,
    Playing,
    Paused,
    /// Had a track; does not any more.
    Stopped,
}

impl Playback {
    pub fn is_playing(self) -> bool {
        matches!(self, Playback::Playing)
    }
}

/// A cache of the voice layer's own state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VoiceConnection {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    Reconnecting,
}

impl VoiceConnection {
    /// What playerUpdate.state.connected reports.
    pub fn is_connected(self) -> bool {
        matches!(self, VoiceConnection::Connected)
    }
}

/// Everything the actor owns about one guild's player.
#[derive(Debug)]
pub struct PlayerModel {
    pub guild_id: u64,
    /// guild_id formatted once, not on every snapshot() — the wire field is a
    /// string but the model's own identity is the u64, and snapshot() runs on
    /// every playerUpdate tick.
    guild_id_str: String,
    pub playback: Playback,
    pub track: Option<Track>,
    /// What PATCH's paused field last requested, tracked independently of
    /// playback/track — the original's AudioPlayer.paused is its own flag,
    /// not derived from whether a track happens to be loaded, so PATCH
    /// {"paused": true} against an empty player still reports back "paused":
    /// true there. playback alone cannot represent that: a trackless player
    /// can never be Playback::Paused, since Playback also drives
    /// is_playing()/stuck-detection, which really do depend on a track
    /// actually running.
    paused: bool,
    /// 0..=1000, as the original's AudioPlayer.volume.
    pub volume: i32,
    pub filters: Filters,
    pub end_time_ms: Option<i64>,
    /// Voice server details as last accepted from the client. Reported verbatim in
    /// GET player, independent of whether the connection actually came up.
    pub voice: VoiceState,
    /// Cached from voice-layer events; never queried.
    pub connection: VoiceConnection,
    pub ping_ms: i64,
    /// When the current track last produced audio, for TrackStuckEvent.
    pub last_progress: Option<Instant>,
}

impl PlayerModel {
    pub fn new(guild_id: u64) -> Self {
        Self {
            guild_id,
            guild_id_str: guild_id.to_string(),
            playback: Playback::Idle,
            track: None,
            paused: false,
            volume: 100,
            filters: Filters::default(),
            end_time_ms: None,
            voice: VoiceState::default(),
            connection: VoiceConnection::Disconnected,
            // -1 is what the original reports with no connection
            // (SocketServer.kt:77).
            ping_ms: -1,
            last_progress: None,
        }
    }

    /// Starts a new track.
    ///
    /// paused is passed explicitly because the caller has already applied the
    /// rule that a play request with no paused field forces false
    /// (PlayerRestHandler.kt:186) — encoding it here would hide a wire-visible
    /// decision inside the model.
    pub fn play(&mut self, track: Track, paused: bool, now: Instant) {
        self.track = Some(track);
        self.paused = paused;
        self.playback = if paused {
            Playback::Paused
        } else {
            Playback::Playing
        };
        self.last_progress = Some(now);
    }

    /// Clears the current track and hands it back, None if there was none — which
    /// is also whether a TrackEndEvent is owed.
    ///
    /// Returns the track rather than a bool because the caller needs it for that
    /// event: taking it out here means the common path (stopping a player with no
    /// track) moves nothing, where cloning track before the call cloned a whole
    /// Track — eight strings and two JSON objects — on every stop, including the
    /// ones that turn out to owe no event at all.
    pub fn stop(&mut self) -> Option<Track> {
        let track = self.track.take();
        self.end_time_ms = None;
        self.playback = if track.is_some() {
            Playback::Stopped
        } else {
            Playback::Idle
        };
        self.last_progress = None;
        track
    }

    /// Applies paused. The flag itself is recorded unconditionally — the
    /// original's AudioPlayer.setPaused is not gated on a track being
    /// loaded — but playback only transitions while a track is actually
    /// running, since that is what is_playing()/stuck-detection depend on.
    pub fn set_paused(&mut self, paused: bool, now: Instant) {
        self.paused = paused;
        match (self.playback, paused) {
            (Playback::Playing, true) => self.playback = Playback::Paused,
            (Playback::Paused, false) => {
                self.playback = Playback::Playing;
                // Resuming restarts the stuck clock; the gap while paused is not
                // the track failing to produce audio.
                self.last_progress = Some(now);
            }
            _ => {}
        }
    }

    /// The original clamps volume to 0..=1000 rather than rejecting it.
    pub fn set_volume(&mut self, volume: i32) {
        self.volume = volume.clamp(0, 1000);
    }

    /// Assembles the GET player / playerUpdate view.
    ///
    /// Player fields come from this model, voice fields from the event cache — the
    /// same split the original gets from koe (util.kt:91-113), minus the query.
    pub fn snapshot(&self, position_ms: i64, now_epoch_ms: i64) -> Player {
        Player {
            guild_id: self.guild_id_str.clone(),
            track: self.track.clone().map(|mut track| {
                // The reported track carries the live position, not the one it was
                // decoded with.
                track.info.position = position_ms;
                track
            }),
            volume: self.volume,
            paused: self.paused,
            state: self.wire_state(position_ms, now_epoch_ms),
            voice: self.voice.clone(),
            filters: self.filters.clone(),
        }
    }

    pub fn wire_state(&self, position_ms: i64, now_epoch_ms: i64) -> WirePlayerState {
        WirePlayerState {
            time: now_epoch_ms,
            position: if self.track.is_some() { position_ms } else { 0 },
            connected: self.connection.is_connected(),
            ping: if self.connection.is_connected() {
                self.ping_ms
            } else {
                -1
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::track;

    fn model() -> PlayerModel {
        PlayerModel::new(123)
    }

    #[test]
    fn a_new_player_is_idle_and_disconnected() {
        let model = model();
        assert_eq!(model.playback, Playback::Idle);
        assert_eq!(model.connection, VoiceConnection::Disconnected);

        let snapshot = model.snapshot(0, 1);
        assert!(snapshot.track.is_none());
        assert!(!snapshot.state.connected);
        assert_eq!(snapshot.state.ping, -1);
        assert_eq!(snapshot.volume, 100);
    }

    #[test]
    fn stopping_an_idle_player_owes_no_event() {
        let mut model = model();
        assert!(model.stop().is_none());
        assert_eq!(model.playback, Playback::Idle);
    }

    #[test]
    fn stopping_a_playing_player_owes_an_event_and_lands_in_stopped() {
        let mut model = model();
        model.play(track("title"), false, Instant::now());
        // Handed back, not just signalled: the caller emits TrackEndEvent with it.
        assert!(model.stop().is_some());
        assert_eq!(model.playback, Playback::Stopped);
        assert!(model.track.is_none());
    }

    /// playback cannot represent "paused" without a track — it stays Idle,
    /// since is_playing()/stuck-detection genuinely depend on a track
    /// running — but the wire paused field must still report back what was
    /// requested, matching the original's AudioPlayer.paused, which is not
    /// gated on a loaded track. This used to report false here (derived
    /// purely from playback == Paused, which an empty player can never be),
    /// diverging from the original for a PATCH {"paused": true} sent before
    /// any track was ever played.
    #[test]
    fn pausing_an_empty_player_still_reports_paused_on_the_wire() {
        let mut model = model();
        model.set_paused(true, Instant::now());
        assert_eq!(model.playback, Playback::Idle);
        assert!(model.snapshot(0, 1).paused);

        model.set_paused(false, Instant::now());
        assert!(!model.snapshot(0, 1).paused);
    }

    /// The flag survives a track ending: the original's AudioPlayer.paused
    /// is an independent switch, not reset by onTrackEnd, so a player
    /// paused mid-track and then stopped stays reported as paused until a
    /// client explicitly unpauses it.
    #[test]
    fn stopping_a_paused_player_does_not_clear_the_flag() {
        let mut model = model();
        model.play(track("title"), false, Instant::now());
        model.set_paused(true, Instant::now());

        model.stop();

        assert_eq!(model.playback, Playback::Stopped);
        assert!(model.snapshot(0, 1).paused);
    }

    #[test]
    fn pause_and_resume_round_trip() {
        let mut model = model();
        let now = Instant::now();
        model.play(track("title"), false, now);

        model.set_paused(true, now);
        assert_eq!(model.playback, Playback::Paused);
        assert!(model.snapshot(0, 1).paused);

        model.set_paused(false, now);
        assert_eq!(model.playback, Playback::Playing);
    }

    /// Playing while the voice connection is down is legal. Frames keep being
    /// produced and discarded, and position keeps advancing — same as the original.
    #[test]
    fn playing_while_disconnected_is_legal() {
        let mut model = model();
        model.play(track("title"), false, Instant::now());
        model.connection = VoiceConnection::Disconnected;

        assert_eq!(model.playback, Playback::Playing);
        let snapshot = model.snapshot(5_000, 1);
        assert_eq!(snapshot.state.position, 5_000);
        assert!(!snapshot.state.connected);
    }

    #[test]
    fn ping_is_minus_one_until_connected() {
        let mut model = model();
        model.ping_ms = 42;

        model.connection = VoiceConnection::Reconnecting;
        assert_eq!(model.wire_state(0, 1).ping, -1);

        model.connection = VoiceConnection::Connected;
        assert_eq!(model.wire_state(0, 1).ping, 42);
    }

    #[test]
    fn position_is_reported_as_zero_without_a_track() {
        let model = model();
        // Even if the engine's counter still holds a stale value.
        assert_eq!(model.wire_state(9_999, 1).position, 0);
    }

    #[test]
    fn the_reported_track_carries_the_live_position() {
        let mut model = model();
        model.play(track("title"), false, Instant::now());
        let snapshot = model.snapshot(4_200, 1);
        assert_eq!(snapshot.track.unwrap().info.position, 4_200);
    }

    #[test]
    fn volume_is_clamped_rather_than_rejected() {
        let mut model = model();
        model.set_volume(5_000);
        assert_eq!(model.volume, 1_000);
        model.set_volume(-1);
        assert_eq!(model.volume, 0);
    }

    #[test]
    fn replacing_a_track_keeps_the_player_loaded() {
        let mut model = model();
        let now = Instant::now();
        model.play(track("title"), false, now);
        model.play(track("title"), true, now);
        assert_eq!(model.playback, Playback::Paused);
        assert!(model.track.is_some());
    }
}
