//! The node's websocket, from the client side.
//!
//! Two jobs. It is where the session id comes from — no player can be addressed
//! before `ready` arrives — and it is where the event sequence the node claims to
//! produce becomes visible. Every message is logged rather than filtered, because
//! the thing being tested is precisely what the node chooses to send.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use lavalink_protocol::{EmittedEvent, Message};
use tokio::sync::RwLock;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;

/// Connects and reads until the socket closes, then retries.
///
/// Reconnecting rather than exiting is what makes the bot usable while the node is
/// being restarted, which during testing is most of the time. The session id is
/// cleared on disconnect so a command cannot address a session the node has
/// forgotten.
pub async fn run(
    host: String,
    password: String,
    user_id: u64,
    session: Arc<RwLock<Option<String>>>,
) {
    loop {
        if let Err(error) = connect_once(&host, &password, user_id, &session).await {
            tracing::warn!(%error, "node websocket dropped");
        }
        *session.write().await = None;
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn connect_once(
    host: &str,
    password: &str,
    user_id: u64,
    session: &Arc<RwLock<Option<String>>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut request = format!("ws://{host}/v4/websocket").into_client_request()?;
    let headers = request.headers_mut();
    headers.insert("Authorization", password.parse()?);
    headers.insert("User-Id", user_id.to_string().parse()?);
    // The node logs a warning when this is missing, and asking for it while not
    // sending it would be rude.
    headers.insert(
        "Client-Name",
        concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION")).parse()?,
    );

    let (stream, _) = tokio_tungstenite::connect_async(request).await?;
    tracing::info!("connected to the node websocket");
    let (_write, mut read) = stream.split();

    while let Some(frame) = read.next().await {
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
            Ok(message) => handle(message, session).await,
            // Not a warning to be tidy: a message this bot cannot parse is either a
            // node bug or a protocol gap, and the raw text is the evidence.
            Err(error) => tracing::error!(%error, raw = %text, "unparseable node message"),
        }
    }

    Ok(())
}

async fn handle(message: Message, session: &Arc<RwLock<Option<String>>>) {
    match message {
        Message::Ready { resumed, session_id } => {
            tracing::info!(resumed, session_id, "ready");
            *session.write().await = Some(session_id);
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
                "stats"
            );
        }
        Message::Event(event) => log_event(event),
    }
}

/// Events are logged at info with their distinguishing field spelled out, because
/// the sequence — and the `reason` on a `TrackEnd` in particular — is what the
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
            // for the reader to recognise.
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
