//! `GET /v4/websocket`.
//!
//! Two things differ from the original, both at the edges:
//!
//! * **`User-Id` is validated.** The original does
//!   `session.handshakeHeaders.getFirst("User-Id")!!.toLong()`
//!   (`SocketServer.kt:98`) — a missing header is a null assertion failure and a
//!   non-numeric one is a `NumberFormatException`, either way a 500-class crash
//!   during the handshake. A client cannot usefully observe a crash, so this is a
//!   400 with a message instead.
//! * **No servlet container in the send path.** The original reaches through Spring
//!   into Undertow's channel to write (`SocketContext.kt:164`), which ties the
//!   protocol to one server implementation. Here the sink is plain data and the
//!   writer task owns the socket.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{CloseFrame, Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use futures_util::{SinkExt as _, StreamExt as _};
use lavalink_protocol::message::Message;

use crate::error::ApiError;
use crate::session::Session;
use crate::state::AppState;

/// Sent when the client stops draining essential messages.
const CLOSE_POLICY_VIOLATION: u16 = 1008;

/// Sent to every connected client when the node is shutting down, so a restart
/// closes the socket cleanly instead of the connection just dying with the process.
const CLOSE_GOING_AWAY: u16 = 1001;

/// The point at which a client is considered unresponsive. Below the sink's own
/// capacity, so the session is closed deliberately rather than after messages have
/// already been refused.
const OVERFLOW_THRESHOLD: usize = 2048;

/// How long a single outbound write may go unacknowledged before the client is
/// considered unresponsive.
///
/// Without this, a client whose TCP receive window is stuck blocks this task
/// inside `writer.send` indefinitely — the `pending_essentials` check below
/// only runs after a write returns, so essentials keep accumulating in
/// `sink.rs` for the entire stall. Past `ESSENTIAL_CAPACITY` they start being
/// silently discarded (`SendError::Overflow`, ignored by every caller), which
/// is exactly the "never dropped" guarantee the essential lane exists to make.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let user_id = parse_user_id(&headers)?;
    let requested_session = header(&headers, "session-id");
    let client_name = header(&headers, "client-name");

    if client_name.is_none() {
        tracing::warn!(
            user_agent = header(&headers, "user-agent").unwrap_or_default(),
            "library developers: please send a Client-Name header"
        );
    }

    Ok(upgrade.on_upgrade(move |socket| {
        run(state, socket, user_id, requested_session, client_name)
    }))
}

fn parse_user_id(headers: &HeaderMap) -> Result<u64, ApiError> {
    let raw = header(headers, "user-id")
        .ok_or_else(|| ApiError::bad_request("The User-Id header is required"))?;
    raw.parse::<u64>()
        .map_err(|_| ApiError::bad_request(format!("The User-Id header is not a snowflake: {raw}")))
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

async fn run(
    state: AppState,
    socket: WebSocket,
    user_id: u64,
    requested_session: Option<String>,
    client_name: Option<String>,
) {
    // Ownership of a resumable session transfers here and nowhere else — one atomic
    // step, at the moment the connection actually exists.
    let resumed = requested_session
        .as_deref()
        .and_then(|id| state.sessions.claim_for_resume(id, Instant::now()));

    let (session, resumed) = match resumed {
        Some(session) => {
            tracing::info!(session = %session.id, "resumed session");
            (session, true)
        }
        None => {
            if let Some(requested) = requested_session {
                tracing::info!(
                    requested = %requested,
                    "the requested session could not be resumed; starting a new one"
                );
            }
            (state.sessions.open(user_id, client_name), false)
        }
    };

    // ready is the first thing on the wire, before any queued replay: a
    // resumed session's essential lane can already hold a backlog from the
    // reconnect window, and plain send would put this behind it.
    let _ = session.send_first(Message::Ready {
        resumed,
        session_id: session.id.clone(),
    });

    if resumed {
        emit_fresh_updates(&session);
    }

    let destroyed = pump(&state, &session, socket).await;

    // The socket is gone. Either the session waits to be resumed or it is over.
    // destroyed is checked first: pump already tore the session down (write
    // timeout / overflow), so on_disconnect finding nothing left in the
    // registry means exactly that, not "went Resumable" — the two share the
    // same None otherwise.
    if destroyed {
        tracing::info!(session = %session.id, "session closed");
    } else if let Some(session) = state.sessions.on_disconnect(&session.id, Instant::now()) {
        tracing::info!(session = %session.id, "session closed");
        session.shutdown().await;
    } else {
        tracing::info!(
            session = %session.id,
            timeout = session.resume_timeout_secs(),
            "session can be resumed"
        );
    }
}

/// Drives the socket until it closes. Returns whether it destroyed the
/// session itself (essential-queue overflow) rather than leaving that decision
/// to the caller.
async fn pump(state: &AppState, session: &Arc<Session>, socket: WebSocket) -> bool {
    let (mut writer, mut reader) = socket.split();
    let mut shutdown = state.shutdown.clone();
    // A session resumed with a backlog already over OVERFLOW_THRESHOLD (built up
    // while nobody was connected to read it) must get a chance to drain it
    // rather than being 1008-closed on its first loop iteration, before the
    // client has had any chance to prove it isn't draining. Enforcement only
    // arms once the backlog has been observed under threshold at least once.
    let mut overflow_armed = session.sink.pending_essentials() < OVERFLOW_THRESHOLD;

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                tracing::info!(session = %session.id, "node is shutting down; closing the session");
                let _ = tokio::time::timeout(
                    WRITE_TIMEOUT,
                    writer.send(WsMessage::Close(Some(CloseFrame {
                        code: CLOSE_GOING_AWAY,
                        reason: "node is shutting down".into(),
                    }))),
                )
                .await;
                return false;
            }

            // v4 has no client-to-server messages; the original logs and ignores
            // them (SocketServer.kt:172). We still have to read, or close frames
            // and pings never arrive.
            incoming = reader.next() => {
                match incoming {
                    Some(Ok(WsMessage::Close(_))) | None => break,
                    Some(Ok(WsMessage::Text(_) | WsMessage::Binary(_))) => {
                        tracing::warn!(
                            session = %session.id,
                            "Lavalink v4 does not accept websocket messages; use the REST API"
                        );
                    }
                    // Ping/pong are handled by the transport.
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        tracing::debug!(session = %session.id, %error, "websocket read failed");
                        break;
                    }
                }
            }

            outgoing = session.sink.recv() => {
                let Some(message) = outgoing else { break };
                let payload = match serde_json::to_string(&message) {
                    Ok(payload) => payload,
                    Err(error) => {
                        // Serializing our own DTOs cannot fail in practice; if it
                        // does, dropping one message beats dropping the session.
                        tracing::error!(%error, "could not serialize an outgoing message");
                        continue;
                    }
                };
                match tokio::time::timeout(
                    WRITE_TIMEOUT,
                    writer.send(WsMessage::Text(payload.into())),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) => {
                        restore_undelivered(session, message);
                        break;
                    }
                    Err(_elapsed) => {
                        // The client stopped acknowledging writes at the transport
                        // level — could be a misbehaving client or just a stalled
                        // TCP receive window, so this is treated as an ordinary
                        // disconnect (break, let the caller's on_disconnect decide
                        // resumable vs. destroy) rather than assumed malicious the
                        // way an overflowing queue below is.
                        tracing::warn!(
                            session = %session.id,
                            "client did not acknowledge a write in time; closing the session"
                        );
                        restore_undelivered(session, message);
                        break;
                    }
                }
            }
        }

        if overflow_closes(&mut overflow_armed, session.sink.pending_essentials()) {
            tracing::warn!(
                session = %session.id,
                "client is not draining events; closing the session"
            );
            let _ = tokio::time::timeout(
                WRITE_TIMEOUT,
                writer.send(WsMessage::Close(Some(CloseFrame {
                    code: CLOSE_POLICY_VIOLATION,
                    reason: "event queue overflow".into(),
                }))),
            )
            .await;
            state.sessions.destroy(&session.id).await;
            return true;
        }
    }

    false
}

/// Sends `Command::EmitUpdate` to every player in `session`, non-blocking.
///
/// `SocketContext.kt:193`: after replaying its queue, the original sends a
/// fresh `playerUpdate` for every player unconditionally, right at resume —
/// not waiting for the next periodic tick. Our sink drops snapshot messages
/// entirely while paused (`Sink::send`'s docs), so without this a resumed
/// client sees nothing for a guild whose state didn't happen to change since
/// disconnecting, and stale-until-corrected state for one that did, for up to
/// `playerUpdateInterval` (5s default).
///
/// `try_send`, not an awaited `send`: this must not hold up completing the
/// handshake behind a busy actor, and a skipped one is superseded by the next
/// periodic tick regardless — the same trade `ticker.rs`'s own use of
/// `EmitUpdate` makes.
fn emit_fresh_updates(session: &Session) {
    for player in session.players() {
        let _ = player.try_send(crate::player::Command::EmitUpdate);
    }
}

/// Puts an essential `message` that `recv()` already dequeued back at the front
/// of the essential lane, after the write that was going to deliver it failed or
/// timed out.
///
/// Without this, `recv()`'s `try_recv` (`sink.rs`'s `essential.pop_front()`)
/// already removed the message from the queue before the write that was
/// supposed to deliver it ever ran — so a write failure here lost it silently,
/// even though this is exactly the abnormal-disconnect case resume exists to
/// recover from.
///
/// A snapshot message (`playerUpdate`/`stats`, identified by
/// [`Message::coalesce_key`] being `Some`) is not restored: the caller's
/// `on_disconnect` unconditionally clears the snapshot lane
/// (`Sink::pause`), so putting one back here would just be thrown away a
/// moment later, and the next tick regenerates a fresher one regardless.
fn restore_undelivered(session: &Session, message: Message) {
    if message.coalesce_key().is_none() {
        let _ = session.sink.send_first(message);
    }
}

/// Whether a session that now has `pending` essential messages queued should be
/// closed as a policy violation, given whether enforcement is currently `armed`
/// (mutated in place).
///
/// Essentials backing up ordinarily means a connected client is not reading. But
/// a session resumed after a long detach can start already over
/// [`OVERFLOW_THRESHOLD`] on a backlog that piled up while nobody was connected
/// to drain it — punishing that on the very first check would close a client
/// that never had a chance to prove it isn't draining. So enforcement starts
/// disarmed whenever the backlog is already over threshold, and arms itself the
/// first time it is observed under threshold; only a backlog that regrows past
/// threshold after that is a client that truly isn't keeping up.
///
/// The grace period above only ever waives [`OVERFLOW_THRESHOLD`], never
/// [`crate::sink::ESSENTIAL_CAPACITY`] itself: past that point `Sink::send` is
/// already returning `SendError::Overflow` and silently dropping messages
/// (every caller discards that error), so a backlog that never dips back under
/// `OVERFLOW_THRESHOLD` — staying disarmed forever under the logic above — would
/// otherwise keep losing events indefinitely with nothing to ever close it.
fn overflow_closes(armed: &mut bool, pending: usize) -> bool {
    if pending >= crate::sink::ESSENTIAL_CAPACITY {
        return true;
    }
    if pending >= OVERFLOW_THRESHOLD {
        *armed
    } else {
        *armed = true;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::track;
    use lavalink_protocol::message::EmittedEvent;
    use lavalink_protocol::player::PlayerState;

    fn event(guild: &str) -> Message {
        Message::Event(EmittedEvent::TrackStart {
            guild_id: guild.to_owned(),
            track: Box::new(track("t")),
        })
    }

    fn update(guild: &str, position: i64) -> Message {
        Message::PlayerUpdate {
            state: PlayerState {
                time: 0,
                position,
                connected: true,
                ping: 1,
            },
            guild_id: guild.to_owned(),
        }
    }

    fn dummy_session() -> std::sync::Arc<Session> {
        crate::session::SessionRegistry::new().open(1, None)
    }

    /// `sink` is the session's own — matching `AppState::player`'s production
    /// wiring — so an event a spawned actor emits (here, the `PlayerUpdate`
    /// `Command::EmitUpdate` triggers) lands where a test reading the
    /// session's sink can see it.
    fn dummy_pair(
        guild_id: u64,
        sink: std::sync::Arc<crate::sink::Sink>,
    ) -> (crate::player::PlayerHandle, std::sync::Arc<crate::voice::VoiceConnection>) {
        crate::testing::dummy_pair(guild_id, sink)
    }

    /// The bug this guards: `recv()` already removed the message from the
    /// essential lane before a write that fails or times out ever runs, so
    /// without restoring it the message that was supposed to be delivered (or
    /// replayed after a resume) is silently gone.
    #[test]
    fn restoring_an_undelivered_essential_puts_it_back_ahead_of_the_rest() {
        let session = dummy_session();
        session.send(event("1")).unwrap();
        session.send(event("2")).unwrap();

        // What ws.rs's pump() does with the message recv() just handed it,
        // once the write for it failed.
        restore_undelivered(&session, event("0"));

        assert_eq!(session.sink.try_recv(), Some(event("0")));
        assert_eq!(session.sink.try_recv(), Some(event("1")));
        assert_eq!(session.sink.try_recv(), Some(event("2")));
    }

    /// A snapshot message is not restored: `on_disconnect`'s `sink.pause()`
    /// clears the snapshot lane unconditionally right after, so putting one
    /// back here would only be thrown away a moment later.
    #[test]
    fn restoring_an_undelivered_snapshot_is_a_no_op() {
        let session = dummy_session();
        restore_undelivered(&session, update("1", 42));
        assert_eq!(session.sink.try_recv(), None);
    }

    /// The bug this guards: `Sink::send` drops every `playerUpdate` while
    /// paused (see its own docs on why), so a resumed session's sink starts
    /// back up with nothing queued for any player whose state didn't happen
    /// to change while detached — a client would see no update at all until
    /// the next periodic tick, up to `playerUpdateInterval` (5s default)
    /// later. `SocketContext.kt:193` closes exactly this gap in the original
    /// by sending one immediately for every player on resume.
    #[tokio::test]
    async fn resuming_emits_a_fresh_update_for_every_player() {
        let session = dummy_session();
        session
            .get_or_create_player(1, || dummy_pair(1, std::sync::Arc::clone(&session.sink)))
            .unwrap();
        session
            .get_or_create_player(2, || dummy_pair(2, std::sync::Arc::clone(&session.sink)))
            .unwrap();

        // A paused sink is what a real resume transitions out of; snapshots
        // sent while paused (nothing here, since nothing played) would have
        // been dropped either way.
        session.sink.pause();
        session.sink.resume();

        emit_fresh_updates(&session);

        let mut guilds_seen = Vec::new();
        for _ in 0..100 {
            match session.sink.try_recv() {
                Some(Message::PlayerUpdate { guild_id, .. }) => guilds_seen.push(guild_id),
                Some(_) => {}
                None => {
                    if guilds_seen.len() == 2 {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            }
        }
        guilds_seen.sort();
        assert_eq!(guilds_seen, vec!["1".to_owned(), "2".to_owned()]);
    }

    use axum::http::{HeaderName, HeaderValue};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn a_valid_user_id_is_accepted() {
        let headers = headers(&[("user-id", "170939974227541002")]);
        assert_eq!(parse_user_id(&headers).unwrap(), 170_939_974_227_541_002);
    }

    /// The original crashes on both of these.
    #[test]
    fn a_missing_user_id_is_a_bad_request() {
        let error = parse_user_id(&HeaderMap::new()).unwrap_err();
        assert_eq!(error.status, axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn a_non_numeric_user_id_is_a_bad_request() {
        let headers = headers(&[("user-id", "not-a-snowflake")]);
        let error = parse_user_id(&headers).unwrap_err();
        assert_eq!(error.status, axum::http::StatusCode::BAD_REQUEST);
        assert!(error.message.contains("not-a-snowflake"));
    }

    #[test]
    fn headers_are_matched_case_insensitively() {
        let headers = headers(&[("User-Id", "1"), ("Client-Name", "Wavelink/3.0")]);
        assert_eq!(parse_user_id(&headers).unwrap(), 1);
        assert_eq!(header(&headers, "client-name").as_deref(), Some("Wavelink/3.0"));
    }

    // -- overflow_closes ----------------------------------------------------------

    #[test]
    fn an_armed_session_over_threshold_closes() {
        let mut armed = true;
        assert!(overflow_closes(&mut armed, OVERFLOW_THRESHOLD));
    }

    #[test]
    fn a_connected_session_under_threshold_never_closes_and_stays_armed() {
        let mut armed = true;
        assert!(!overflow_closes(&mut armed, OVERFLOW_THRESHOLD - 1));
        assert!(armed);
    }

    /// The bug this guards: a session resumed with a pre-existing backlog above
    /// threshold (built up while nobody was connected to drain it) must not be
    /// closed on the very first check — it never had a chance to prove it isn't
    /// draining live traffic.
    #[test]
    fn a_resumed_backlog_already_over_threshold_is_not_immediately_closed() {
        let mut armed = false; // what pump() computes when resuming over threshold
        assert!(!overflow_closes(&mut armed, OVERFLOW_THRESHOLD + 500));
        assert!(!armed, "still disarmed until the backlog is seen under threshold");
    }

    /// Once the backlog has drained under threshold at least once, enforcement is
    /// live: a client that lets it regrow past threshold after that really is
    /// failing to keep up.
    #[test]
    fn enforcement_arms_after_the_backlog_drains_and_then_catches_regrowth() {
        let mut armed = false;
        assert!(!overflow_closes(&mut armed, OVERFLOW_THRESHOLD + 500));

        // The backlog drains below threshold: this is what arms enforcement.
        assert!(!overflow_closes(&mut armed, OVERFLOW_THRESHOLD - 1));
        assert!(armed);

        // It grows back past threshold: now it closes.
        assert!(overflow_closes(&mut armed, OVERFLOW_THRESHOLD));
    }

    /// The bug this guards: a backlog resumed above `OVERFLOW_THRESHOLD` that
    /// never dips back under it (a client slow enough to stay over the grace
    /// threshold but not slow enough to stop entirely) left `armed` `false`
    /// forever under the old logic — so it was never closed even once
    /// `Sink::send` started silently dropping messages at
    /// `ESSENTIAL_CAPACITY`. The hard cap must fire regardless of `armed`.
    #[test]
    fn a_backlog_that_never_arms_still_closes_once_data_is_actually_being_lost() {
        let mut armed = false; // never observed under OVERFLOW_THRESHOLD
        assert!(
            !overflow_closes(&mut armed, crate::sink::ESSENTIAL_CAPACITY - 1),
            "still under the point where Sink::send starts dropping messages"
        );
        assert!(
            overflow_closes(&mut armed, crate::sink::ESSENTIAL_CAPACITY),
            "past this point Sink::send is already discarding essentials"
        );
    }
}
