//! Player state and its transitions.
//!
//! Two state machines live here and they deliberately do not talk to each other:
//!
//! * [`Playback`] — ours. The actor is the sole authority.
//! * [`VoiceConnection`] — not ours. It is a *cache* of what the voice layer last
//!   told us, updated only by events. Nothing here ever asks the voice layer a
//!   question, which is what makes the read path side-effect free: the original's
//!   `sendPlayerUpdate` calls `getMediaConnection`, which creates a connection if
//!   none exists, so merely reporting state changes it.
//!
//! Illegal transitions are rejected internally, but that rejection never reaches the
//! client — a path the original silently no-ops stays a silent no-op here.

use std::time::Instant;

use lavalink_protocol::filters::Filters;
use lavalink_protocol::player::{Player, PlayerState as WirePlayerState, Track, VoiceState};

/// What the player is doing. `Idle` and `Stopped` look identical on the wire
/// (`track: null`) but differ in whether a track has ever run, which decides
/// whether stopping should emit a `TrackEndEvent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Playback {
    /// Created, never given a track.
    Idle,
    /// An identifier is being resolved. Holds nothing but a cancellation token —
    /// the load itself happens outside the actor.
    Loading,
    Playing,
    Paused,
    /// Had a track; does not any more.
    Stopped,
}

impl Playback {
    pub fn is_playing(self) -> bool {
        matches!(self, Playback::Playing)
    }

    /// Whether a track is currently loaded, i.e. whether `track` is non-null.
    pub fn has_track(self) -> bool {
        matches!(self, Playback::Playing | Playback::Paused)
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
    /// What `playerUpdate.state.connected` reports.
    pub fn is_connected(self) -> bool {
        matches!(self, VoiceConnection::Connected)
    }
}

/// Everything the actor owns about one guild's player.
#[derive(Debug)]
pub struct PlayerModel {
    pub guild_id: u64,
    pub playback: Playback,
    pub track: Option<Track>,
    /// 0..=1000, as the original's `AudioPlayer.volume`.
    pub volume: i32,
    pub filters: Filters,
    pub end_time_ms: Option<i64>,
    /// Voice server details as last accepted from the client. Reported verbatim in
    /// `GET player`, independent of whether the connection actually came up.
    pub voice: VoiceState,
    /// Cached from voice-layer events; never queried.
    pub connection: VoiceConnection,
    pub ping_ms: i64,
    /// When the current track last produced audio, for `TrackStuckEvent`.
    pub last_progress: Option<Instant>,
}

impl PlayerModel {
    pub fn new(guild_id: u64) -> Self {
        Self {
            guild_id,
            playback: Playback::Idle,
            track: None,
            volume: 100,
            filters: Filters::default(),
            end_time_ms: None,
            voice: VoiceState::default(),
            connection: VoiceConnection::Disconnected,
            // -1 is what the original reports with no connection
            // (`SocketServer.kt:77`).
            ping_ms: -1,
            last_progress: None,
        }
    }

    /// Starts a new track.
    ///
    /// `paused` is passed explicitly because the caller has already applied the
    /// rule that a play request with no `paused` field forces `false`
    /// (`PlayerRestHandler.kt:186`) — encoding it here would hide a wire-visible
    /// decision inside the model.
    pub fn play(&mut self, track: Track, paused: bool, now: Instant) {
        self.track = Some(track);
        self.playback = if paused {
            Playback::Paused
        } else {
            Playback::Playing
        };
        self.last_progress = Some(now);
    }

    /// Clears the current track. Returns whether there was one to clear, which is
    /// also whether a `TrackEndEvent` is owed.
    pub fn stop(&mut self) -> bool {
        let had_track = self.track.is_some();
        self.track = None;
        self.end_time_ms = None;
        self.playback = if had_track {
            Playback::Stopped
        } else {
            Playback::Idle
        };
        self.last_progress = None;
        had_track
    }

    /// Applies `paused`. A no-op unless a track is loaded, matching the original,
    /// where pausing an empty player sets a flag nothing reads.
    pub fn set_paused(&mut self, paused: bool, now: Instant) {
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

    /// Assembles the `GET player` / `playerUpdate` view.
    ///
    /// Player fields come from this model, voice fields from the event cache — the
    /// same split the original gets from koe (`util.kt:91-113`), minus the query.
    pub fn snapshot(&self, position_ms: i64, now_epoch_ms: i64) -> Player {
        Player {
            guild_id: self.guild_id.to_string(),
            track: self.track.clone().map(|mut track| {
                // The reported track carries the live position, not the one it was
                // decoded with.
                track.info.position = position_ms;
                track
            }),
            volume: self.volume,
            paused: matches!(self.playback, Playback::Paused),
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
    use lavalink_protocol::player::TrackInfo;

    fn track() -> Track {
        Track::new(
            "encoded".into(),
            TrackInfo {
                identifier: "id".into(),
                is_seekable: true,
                author: "author".into(),
                length: 10_000,
                is_stream: false,
                position: 0,
                title: "title".into(),
                uri: None,
                source_name: "http".into(),
                artwork_url: None,
                isrc: None,
            },
        )
    }

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
        assert!(!model.stop());
        assert_eq!(model.playback, Playback::Idle);
    }

    #[test]
    fn stopping_a_playing_player_owes_an_event_and_lands_in_stopped() {
        let mut model = model();
        model.play(track(), false, Instant::now());
        assert!(model.stop());
        assert_eq!(model.playback, Playback::Stopped);
        assert!(model.track.is_none());
    }

    #[test]
    fn pausing_an_empty_player_changes_nothing() {
        let mut model = model();
        model.set_paused(true, Instant::now());
        assert_eq!(model.playback, Playback::Idle);
    }

    #[test]
    fn pause_and_resume_round_trip() {
        let mut model = model();
        let now = Instant::now();
        model.play(track(), false, now);

        model.set_paused(true, now);
        assert_eq!(model.playback, Playback::Paused);
        assert!(model.snapshot(0, 1).paused);

        model.set_paused(false, now);
        assert_eq!(model.playback, Playback::Playing);
    }

    /// `Playing` while the voice connection is down is legal. Frames keep being
    /// produced and discarded, and position keeps advancing — same as the original.
    #[test]
    fn playing_while_disconnected_is_legal() {
        let mut model = model();
        model.play(track(), false, Instant::now());
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
        model.play(track(), false, Instant::now());
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
        model.play(track(), false, now);
        model.play(track(), true, now);
        assert_eq!(model.playback, Playback::Paused);
        assert!(model.playback.has_track());
    }
}
