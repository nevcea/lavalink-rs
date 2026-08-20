//! The node's WebSocket, from the client side.
//!
//! It provides the session ID needed to address players and exposes the node's event
//! sequence. Every message is logged because the test is concerned with everything
//! the node sends.

use std::time::Duration;

use futures_util::StreamExt;
use lavalink_protocol::{EmittedEvent, Message};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;

use crate::node::Node;

/// Connects and reads until the socket closes, then retries.
///
/// Reconnecting rather than exiting is what makes the bot usable while the node is
/// being restarted, which during testing is most of the time. The session id is
/// retained so the next connection can ask the node to resume it; a fresh Ready
/// replaces it when the node no longer has that session.
pub async fn run(
    node: Node,
    user_id: u64,
) {
    loop {
        if let Err(error) = connect_once(&node, user_id).await {
            tracing::warn!(%error, "node websocket dropped");
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn connect_once(
    node: &Node,
    user_id: u64,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let session_id = node.current_session_id().await;
    let request = websocket_request(
        &node.host,
        &node.password,
        user_id,
        session_id.as_deref(),
    )?;
    let (mut stream, _) = tokio_tungstenite::connect_async(request).await?;
    tracing::info!(resuming = session_id.is_some(), "connected to the node websocket");

    while let Some(frame) = stream.next().await {
        let text = match frame? {
            WsMessage::Text(text) => text,
            WsMessage::Close(frame) => {
                tracing::warn!(?frame, "node closed the websocket");
                break;
            }
            // Ping/pong are handled by the library; anything else is not something
            // this protocol uses, so seeing one is itself worth reporting.
            other => {
                tracing::debug!(?other, "ignoring non-text frame");
                continue;
            }
        };

        match serde_json::from_str::<Message>(&text) {
            Ok(message) => handle(message, node).await?,
            // Not a warning to be tidy: a message this bot cannot parse is either a
            // node bug or a protocol gap, and the raw text is the evidence.
            Err(error) => tracing::error!(%error, raw = %text, "unparseable node message"),
        }
    }

    Ok(())
}

fn websocket_request(
    host: &str,
    password: &str,
    user_id: u64,
    session_id: Option<&str>,
) -> Result<
    tokio_tungstenite::tungstenite::http::Request<()>,
    Box<dyn std::error::Error + Send + Sync>,
> {
    let mut request = format!("ws://{host}/v4/websocket").into_client_request()?;
    let headers = request.headers_mut();
    headers.insert("Authorization", password.parse()?);
    headers.insert("User-Id", user_id.to_string().parse()?);
    headers.insert(
        "Client-Name",
        concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION")).parse()?,
    );
    if let Some(session_id) = session_id {
        headers.insert("Session-Id", session_id.parse()?);
    }
    Ok(request)
}

async fn handle(
    message: Message,
    node: &Node,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match message {
        Message::Ready { resumed, session_id } => {
            tracing::info!(resumed, session_id, "ready");
            node.set_session_id(session_id.clone()).await;
            node.enable_resuming(&session_id).await?;
        }
        Message::PlayerUpdate { state, guild_id } => {
            tracing::info!(
                guild = %guild_id,
                position = state.position,
                connected = state.connected,
                ping = state.ping,
                "playerUpdate"
            );
        }
        Message::Stats(stats) => {
            tracing::debug!(
                players = stats.players,
                playing = stats.playing_players,
                frame_stats = ?stats.frame_stats,
                "stats"
            );
        }
        Message::Event(event) => log_event(event),
    }
    Ok(())
}

/// Events are logged at info with their distinguishing field spelled out, because
/// the sequence — and the reason on a TrackEnd in particular — is what the
/// manual verification in the README compares against.
fn log_event(event: EmittedEvent) {
    match event {
        EmittedEvent::TrackStart { guild_id, track } => {
            tracing::info!(guild = %guild_id, title = %track.info.title, "TrackStartEvent");
        }
        EmittedEvent::TrackEnd {
            guild_id,
            track,
            reason,
        } => {
            tracing::info!(
                guild = %guild_id,
                title = %track.info.title,
                ?reason,
                may_start_next = reason.may_start_next(),
                "TrackEndEvent"
            );
        }
        EmittedEvent::TrackException {
            guild_id,
            track,
            exception,
        } => {
            tracing::error!(
                guild = %guild_id,
                title = %track.info.title,
                author = %track.info.author,
                source = %track.info.source_name,
                identifier = %track.info.identifier,
                ?exception,
                "TrackExceptionEvent"
            );
        }
        EmittedEvent::TrackStuck {
            guild_id,
            track,
            threshold_ms,
        } => {
            tracing::error!(
                guild = %guild_id,
                title = %track.info.title,
                threshold_ms,
                "TrackStuckEvent"
            );
        }
        EmittedEvent::WebSocketClosed {
            guild_id,
            code,
            reason,
            by_remote,
        } => {
            // 4006 and 4014 are the two a client is expected to recover from by
            // re-sending its voice state, so they are called out rather than left
            // in the general event log.
            tracing::warn!(
                guild = %guild_id,
                code,
                reason,
                by_remote,
                recoverable = matches!(code, 4006 | 4014),
                "WebSocketClosedEvent"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_known_session_is_requested_on_reconnect() {
        let request = websocket_request("localhost:2333", "secret", 42, Some("session-1")).unwrap();
        assert_eq!(request.headers()["Session-Id"], "session-1");
    }

    #[test]
    fn a_first_connection_has_no_session_header() {
        let request = websocket_request("localhost:2333", "secret", 42, None).unwrap();
        assert!(!request.headers().contains_key("Session-Id"));
    }
}
