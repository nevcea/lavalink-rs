//! Discord voice, via songbird's standalone driver.
//!
//! Implementing the voice transport ourselves was ruled out: gateway v8 state
//! machine, UDP IP discovery, RTP with negotiated SRTP modes, speaking payloads,
//! silence frames, DAVE — plus an open-ended commitment to follow a protocol Discord
//! keeps changing. Owning that while removing features to stay small is
//! self-contradictory, so it is bought rather than built.
//!
//! Only the driver half of songbird is used. Lavalink is given voice state and
//! voice server updates by its own client over REST, which is exactly what the
//! standalone driver takes, so no Discord gateway client is involved.
//!
//! Ownership
//!
//! The split is strict: the player actor owns player state, songbird owns
//! connection state. Nothing here asks the actor anything, and the actor never
//! queries a driver — it caches what arrives as VoiceUpdate events. connected
//! and ping in a playerUpdate therefore report what songbird observed, not what
//! we hope is true.

use std::num::NonZeroU64;
use std::sync::Arc;

use lavalink_protocol::player::VoiceState;
use songbird::error::ConnectionError;
use songbird::id::{ChannelId, GuildId, UserId};
use songbird::events::context_data::DisconnectReason;
use songbird::{Config, ConnectionInfo, CoreEvent, Driver, Event, EventContext, EventHandler};
use tokio::sync::Mutex;

use crate::player::{VoiceUpdate, VoiceUpdateSlot};

#[derive(Debug, thiserror::Error)]
pub enum VoiceError {
    #[error("the voice state is missing a channel id")]
    NoChannel,
    #[error("the voice state has an unusable id: {0}")]
    BadId(String),
    /// Boxed because songbird's error carries a large payload and this type is
    /// returned by value on every connect.
    #[error("failed to connect to voice server: {0}")]
    Connect(#[from] Box<ConnectionError>),
}

/// driver and current behind one lock, not two: connect() checks, dials
/// and records as a single critical section, so two concurrent connects for
/// the same guild can't interleave and leave current pointing at a state
/// the driver didn't actually end up with.
struct ConnectionState {
    driver: Driver,
    /// What the last successful connect used, so a repeated voice field with
    /// identical contents does not tear down a working connection.
    current: Option<VoiceState>,
}

/// One guild's voice connection.
pub struct VoiceConnection {
    state: Mutex<ConnectionState>,
    guild_id: u64,
    user_id: u64,
}

/// Whether requested differs from the voice state a connection was last built
/// with — a client re-sending its current voice state unchanged must not tear
/// down working audio to rebuild an identical connection.
fn needs_reconnect(current: Option<&VoiceState>, requested: &VoiceState) -> bool {
    current != Some(requested)
}

impl VoiceConnection {
    /// Builds a connection and subscribes the actor to songbird's events.
    ///
    /// The subscription is what keeps the actor's cache honest: every transition
    /// arrives as a message rather than being inferred.
    pub fn new(guild_id: u64, user_id: u64, voice_updates: VoiceUpdateSlot) -> Self {
        let mut driver = Driver::new(Config::default());

        for event in [
            CoreEvent::DriverConnect,
            CoreEvent::DriverReconnect,
            CoreEvent::DriverDisconnect,
        ] {
            driver.add_global_event(
                Event::Core(event),
                ActorNotifier {
                    voice_updates: Arc::clone(&voice_updates),
                },
            );
        }

        Self {
            state: Mutex::new(ConnectionState {
                driver,
                current: None,
            }),
            guild_id,
            user_id,
        }
    }

    /// Connects, or reconnects if the voice server details changed.
    ///
    /// The original tears down and rebuilds whenever any field differs
    /// (PlayerRestHandler.kt:115-127); the same comparison is here, because a
    /// client that re-sends an unchanged voice state expects its audio to keep
    /// playing.
    ///
    /// Held as one lock for the whole check-dial-record sequence: a second
    /// connect() racing this one on the same guild waits for the lock rather
    /// than reading a current that this call hasn't written yet.
    pub async fn connect(&self, voice: &VoiceState) -> Result<(), VoiceError> {
        let mut state = self.state.lock().await;
        if !needs_reconnect(state.current.as_ref(), voice) {
            return Ok(());
        }

        let info = self.connection_info(voice)?;
        // A failed attempt still tears down whatever songbird had before it, so
        // current must not keep pointing at that now-dead connection — otherwise
        // a client retrying with the same (last known-good) voice state would hit
        // needs_reconnect == false and never call driver.connect again,
        // stranding the guild disconnected until a byte-for-byte different voice
        // state happens to arrive.
        if let Err(error) = state.driver.connect(info).await {
            state.current = None;
            return Err(VoiceError::Connect(Box::new(error)));
        }
        state.current = Some(voice.clone());
        Ok(())
    }

    fn connection_info(&self, voice: &VoiceState) -> Result<ConnectionInfo, VoiceError> {
        let channel_id = voice
            .channel_id
            .as_deref()
            .ok_or(VoiceError::NoChannel)?
            .parse::<NonZeroU64>()
            .map_err(|error| VoiceError::BadId(error.to_string()))?;

        Ok(ConnectionInfo {
            channel_id: ChannelId::from(channel_id),
            // Discord sends the endpoint without a scheme and sometimes with a
            // port suffix; songbird wants it exactly as delivered.
            endpoint: voice.endpoint.clone(),
            guild_id: GuildId::from(
                NonZeroU64::new(self.guild_id).ok_or_else(|| VoiceError::BadId("guild 0".into()))?,
            ),
            session_id: voice.session_id.clone(),
            token: voice.token.clone(),
            user_id: UserId::from(
                NonZeroU64::new(self.user_id).ok_or_else(|| VoiceError::BadId("user 0".into()))?,
            ),
        })
    }

    pub async fn play(&self, input: songbird::input::Input) -> songbird::tracks::TrackHandle {
        self.state.lock().await.driver.play_only_input(input)
    }

    /// Serializes the current-track check with mixer replacement. A superseded
    /// input must not reach play_only_input, because that call stops the newer
    /// track already in the mixer.
    pub(crate) async fn play_if(
        &self,
        input: songbird::input::Input,
        should_play: impl FnOnce() -> bool,
    ) -> Option<songbird::tracks::TrackHandle> {
        let mut state = self.state.lock().await;
        should_play().then(|| state.driver.play_only_input(input))
    }

    /// play_if for a preconfigured Track, used when pause, volume and event
    /// handlers must be installed before a lazy input can finish immediately.
    pub(crate) async fn play_track_if(
        &self,
        track: songbird::tracks::Track,
        should_play: impl FnOnce() -> bool,
    ) -> Option<songbird::tracks::TrackHandle> {
        let mut state = self.state.lock().await;
        should_play().then(|| state.driver.play_only(track))
    }

    pub async fn stop(&self) {
        self.state.lock().await.driver.stop();
    }

    pub async fn leave(&self) {
        let mut state = self.state.lock().await;
        state.driver.stop();
        state.driver.leave();
        state.current = None;
    }
}

/// Forwards songbird's connection events to the player actor.
struct ActorNotifier {
    voice_updates: VoiceUpdateSlot,
}

#[async_trait::async_trait]
impl EventHandler for ActorNotifier {
    async fn act(&self, context: &EventContext<'_>) -> Option<Event> {
        let update = match context {
            // songbird's docs on CoreEvent::DriverReconnect: it "fires when this
            // driver successfully reconnects after a network error" — both
            // firing sites are the Ok arm of a connect attempt. It's a success
            // event, not "reconnecting in progress", so it's reported the same
            // way an initial connect is. Mapping it to Reconnecting left
            // connected false (ping stuck at -1) for the rest of the session
            // after every network blip that recovered on its own, since songbird
            // never fires DriverConnect again on the same session.
            EventContext::DriverConnect(_) | EventContext::DriverReconnect(_) => {
                Some(VoiceUpdate::Connected {
                    // songbird has no latency measurement at connect time. -1 is
                    // the original's "not measured" value, not 0, and clients
                    // display it.
                    ping_ms: -1,
                })
            }
            EventContext::DriverDisconnect(data) => Some(disconnect_update(data.reason)),
            _ => None,
        };

        if let Some(update) = update {
            let voice_updates = self.voice_updates.get().cloned();
            if let Some(voice_updates) = voice_updates {
                // On its own channel, not the general command queue, precisely so
                // a burst of REST traffic sharing that queue can't fill it out
                // from under a voice transition — the case this used to drop in
                // (see git history) and misreport connected until some later,
                // unrelated transition happened to come along and correct it.
                // Only a wedged actor (never draining anything) can still fill
                // this one, which is what try_send still allows dropping.
                if let Err(error) = voice_updates.try_send(update) {
                    tracing::warn!(
                        error_debug = ?error,
                        error_display = %error,
                        "dropped a voice state transition"
                    );
                }
            }
        }

        None
    }
}

/// Maps songbird's disconnect reason to what the actor reports.
fn disconnect_update(reason: Option<DisconnectReason>) -> VoiceUpdate {
    match reason {
        // Discord closed the socket. The code is the point of the event — 4006 and
        // 4014 in particular are what drive a client to re-send its voice state —
        // so it is forwarded rather than flattened.
        Some(DisconnectReason::WsClosed(code)) => VoiceUpdate::Closed {
            code: code.map_or(0, |code| code as i32),
            by_remote: true,
        },
        // Everything else — requested, an ordinary teardown, a failed attempt — is
        // reported the same way. None of them are a WebSocket close, and emitting
        // WebSocketClosedEvent for one would have clients trying to recover from
        // something they asked for, or from a failure the code cannot describe.
        _ => VoiceUpdate::Disconnected,
    }
}

/// Convenience alias for what an engine holds.
pub type SharedVoice = Arc<VoiceConnection>;

#[cfg(test)]
mod tests {
    use super::*;
    use songbird::input::RawAdapter;
    use std::io::Cursor;

    fn voice_state(token: &str, endpoint: &str, session: &str, channel: &str) -> VoiceState {
        VoiceState {
            token: token.into(),
            endpoint: endpoint.into(),
            session_id: session.into(),
            channel_id: Some(channel.into()),
        }
    }

    fn voice_updates() -> VoiceUpdateSlot {
        Arc::new(std::sync::OnceLock::new())
    }

    #[tokio::test]
    async fn a_superseded_input_never_reaches_the_mixer() {
        let connection = VoiceConnection::new(123, 456, voice_updates());
        let input = RawAdapter::new(Cursor::new(Vec::<u8>::new()), 48_000, 2).into();
        assert!(connection.play_if(input, || false).await.is_none());
    }

    // -- needs_reconnect --------------------------------------------------------

    #[test]
    fn an_unchanged_voice_state_does_not_need_a_reconnect() {
        let state = voice_state("t", "e", "s", "c");
        assert!(!needs_reconnect(Some(&state), &state.clone()));
    }

    #[test]
    fn no_prior_state_needs_a_reconnect() {
        let state = voice_state("t", "e", "s", "c");
        assert!(needs_reconnect(None, &state));
    }

    #[test]
    fn a_changed_token_needs_a_reconnect() {
        let current = voice_state("t1", "e", "s", "c");
        let requested = voice_state("t2", "e", "s", "c");
        assert!(needs_reconnect(Some(&current), &requested));
    }

    #[test]
    fn a_changed_endpoint_needs_a_reconnect() {
        let current = voice_state("t", "e1", "s", "c");
        let requested = voice_state("t", "e2", "s", "c");
        assert!(needs_reconnect(Some(&current), &requested));
    }

    #[test]
    fn a_changed_session_id_needs_a_reconnect() {
        let current = voice_state("t", "e", "s1", "c");
        let requested = voice_state("t", "e", "s2", "c");
        assert!(needs_reconnect(Some(&current), &requested));
    }

    #[test]
    fn a_changed_channel_needs_a_reconnect() {
        let current = voice_state("t", "e", "s", "c1");
        let requested = voice_state("t", "e", "s", "c2");
        assert!(needs_reconnect(Some(&current), &requested));
    }

    // -- disconnect_update --------------------------------------------------------

    #[test]
    fn a_remote_close_reports_the_code_and_by_remote() {
        let update = disconnect_update(Some(DisconnectReason::WsClosed(
            songbird::model::FromPrimitive::from_u16(4006),
        )));
        assert_eq!(
            update,
            VoiceUpdate::Closed {
                code: 4006,
                by_remote: true,
            }
        );
    }

    #[test]
    fn a_close_with_no_code_reports_zero() {
        let update = disconnect_update(Some(DisconnectReason::WsClosed(None)));
        assert_eq!(
            update,
            VoiceUpdate::Closed {
                code: 0,
                by_remote: true,
            }
        );
    }

    #[test]
    fn a_requested_disconnect_is_not_reported_as_a_websocket_close() {
        assert_eq!(
            disconnect_update(Some(DisconnectReason::Requested)),
            VoiceUpdate::Disconnected
        );
    }

    #[test]
    fn no_reason_is_treated_as_an_ordinary_disconnect() {
        assert_eq!(disconnect_update(None), VoiceUpdate::Disconnected);
    }

    #[test]
    fn a_transient_io_failure_is_not_mistaken_for_a_websocket_close() {
        assert_eq!(
            disconnect_update(Some(DisconnectReason::Io)),
            VoiceUpdate::Disconnected
        );
    }

    // -- connection_info ----------------------------------------------------------

    #[tokio::test]
    async fn a_complete_voice_state_builds_connection_info() {
        let connection = VoiceConnection::new(123, 456, voice_updates());
        let info = connection
            .connection_info(&voice_state("t", "e", "s", "789"))
            .unwrap();
        assert_eq!(info.channel_id.0.get(), 789);
        assert_eq!(info.guild_id.0.get(), 123);
        assert_eq!(info.user_id.0.get(), 456);
        assert_eq!(info.endpoint, "e");
        assert_eq!(info.session_id, "s");
        assert_eq!(info.token, "t");
    }

    #[tokio::test]
    async fn a_missing_channel_id_is_rejected() {
        let connection = VoiceConnection::new(123, 456, voice_updates());
        let mut voice = voice_state("t", "e", "s", "c");
        voice.channel_id = None;
        assert!(matches!(
            connection.connection_info(&voice),
            Err(VoiceError::NoChannel)
        ));
    }

    #[tokio::test]
    async fn a_non_numeric_channel_id_is_rejected() {
        let connection = VoiceConnection::new(123, 456, voice_updates());
        assert!(matches!(
            connection.connection_info(&voice_state("t", "e", "s", "not-a-number")),
            Err(VoiceError::BadId(_))
        ));
    }

    #[tokio::test]
    async fn guild_id_zero_is_rejected_even_with_a_valid_voice_state() {
        let connection = VoiceConnection::new(0, 456, voice_updates());
        assert!(matches!(
            connection.connection_info(&voice_state("t", "e", "s", "789")),
            Err(VoiceError::BadId(_))
        ));
    }

    #[tokio::test]
    async fn user_id_zero_is_rejected_even_with_a_valid_voice_state() {
        let connection = VoiceConnection::new(123, 0, voice_updates());
        assert!(matches!(
            connection.connection_info(&voice_state("t", "e", "s", "789")),
            Err(VoiceError::BadId(_))
        ));
    }

    // -- connect --------------------------------------------------------------

    /// connect() validates before it ever touches the driver, so a malformed
    /// voice state fails fast instead of hanging on a network attempt.
    #[tokio::test]
    async fn connecting_with_an_invalid_voice_state_fails_before_touching_the_driver() {
        let connection = VoiceConnection::new(123, 456, voice_updates());
        let mut voice = voice_state("t", "e", "s", "c");
        voice.channel_id = None;
        assert!(matches!(
            connection.connect(&voice).await,
            Err(VoiceError::NoChannel)
        ));
    }
}
