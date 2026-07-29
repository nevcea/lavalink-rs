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
use std::time::Instant;

use axum::extract::ws::{CloseFrame, Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use futures_util::{SinkExt as _, StreamExt as _};
use lavalink_protocol::message::Message;

use crate::error::ApiError;
use crate::session::Session;
use crate::state::AppState;
use crate::ticker::shutdown_session;

/// Sent when the client stops draining essential messages.
const CLOSE_POLICY_VIOLATION: u16 = 1008;

/// The point at which a client is considered unresponsive. Below the sink's own
/// capacity, so the session is closed deliberately rather than after messages have
/// already been refused.
const OVERFLOW_THRESHOLD: usize = 2048;

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
        .and_then(|id| state.sessions.claim_for_resume(id));

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

    // `ready` is the first thing on the wire, before any queued replay.
    let _ = session.send(Message::Ready {
        resumed,
        session_id: session.id.clone(),
    });

    pump(&state, &session, socket).await;

    // The socket is gone. Either the session waits to be resumed or it is over.
    if let Some(session) = state.sessions.on_disconnect(&session.id, Instant::now()) {
        tracing::info!(session = %session.id, "session closed");
        shutdown_session(&session).await;
    } else {
        tracing::info!(
            session = %session.id,
            timeout = session.resume_timeout_secs(),
            "session can be resumed"
        );
    }
}

/// Drives the socket until it closes.
async fn pump(state: &AppState, session: &Arc<Session>, socket: WebSocket) {
    let (mut writer, mut reader) = socket.split();

    loop {
        tokio::select! {
            // v4 has no client-to-server messages; the original logs and ignores
            // them (`SocketServer.kt:172`). We still have to read, or close frames
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
                if writer.send(WsMessage::Text(payload.into())).await.is_err() {
                    break;
                }
            }
        }

        // Essentials backing up means the client is not reading. Closing is the
        // honest outcome — the alternative is dropping events, which leaves the
        // client's view of its queue permanently wrong.
        if session.sink.pending_essentials() >= OVERFLOW_THRESHOLD {
            tracing::warn!(
                session = %session.id,
                "client is not draining events; closing the session"
            );
            let _ = writer
                .send(WsMessage::Close(Some(CloseFrame {
                    code: CLOSE_POLICY_VIOLATION,
                    reason: "event queue overflow".into(),
                })))
                .await;
            state.sessions.destroy(&session.id);
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
