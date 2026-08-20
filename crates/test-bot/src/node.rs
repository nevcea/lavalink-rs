//! The node's v4 REST API from a client's perspective.
//!
//! Deliberately thin: every method is one request and the bodies are
//! lavalink_protocol types, so a field this node gets wrong shows up as a
//! deserialization failure here rather than as a silently ignored key. That is the
//! purpose of the bot: a second implementation reads what the first writes.

use std::sync::Arc;

use lavalink_protocol::player::{Player, PlayerUpdate, Players};
use lavalink_protocol::{Info, LoadResult, Omissible, Session, SessionUpdate, StatsData};
use reqwest::{Method, StatusCode};
use tokio::sync::RwLock;

#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    /// The node answered, and said no. Kept separate from a transport failure
    /// because the body is the node's own error DTO and worth showing verbatim.
    #[error("node returned {status}: {body}")]
    Status { status: StatusCode, body: String },
    /// /join has not run yet, so there is no session to address a player to.
    #[error("no session yet — the node websocket has not sent `ready`")]
    NoSession,
    /// The command was wrong, not the node. Carried here so the dispatch table has
    /// one error type rather than two that both end up as the same reply.
    #[error("{0}")]
    Usage(String),
}

/// A handle on one node.
///
/// The session ID arrives asynchronously over the WebSocket (op: "ready"), so it
/// lives behind a lock shared by the WebSocket task and command handlers.
#[derive(Clone)]
pub struct Node {
    client: reqwest::Client,
    pub(crate) host: String,
    pub(crate) password: String,
    session_id: Arc<RwLock<Option<String>>>,
}

impl Node {
    pub fn new(host: &str, password: &str) -> Self {
        Self {
            client: reqwest::Client::new(),
            host: host.to_owned(),
            password: password.to_owned(),
            session_id: Arc::new(RwLock::new(None)),
        }
    }

    pub(crate) async fn current_session_id(&self) -> Option<String> {
        self.session_id.read().await.clone()
    }

    pub(crate) async fn set_session_id(&self, session_id: String) {
        *self.session_id.write().await = Some(session_id);
    }

    pub async fn session_id(&self) -> Result<String, NodeError> {
        self.current_session_id().await.ok_or(NodeError::NoSession)
    }

    /// Sends a request and checks the status, leaving the body's shape (or absence)
    /// to the caller — a DELETE answers 204 with nothing, which send cannot
    /// share since it always decodes JSON.
    async fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&impl serde::Serialize>,
    ) -> Result<reqwest::Response, NodeError> {
        let mut request = self
            .client
            .request(method, format!("http://{}{path}", self.host))
            .header("Authorization", &self.password)
            .query(query);
        if let Some(body) = body {
            request = request.json(body);
        }

        let response = request.send().await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(NodeError::Status { status, body });
        }
        Ok(response)
    }

    async fn send<T: serde::de::DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&impl serde::Serialize>,
    ) -> Result<T, NodeError> {
        Ok(self.request(method, path, query, body).await?.json().await?)
    }

    pub async fn info(&self) -> Result<Info, NodeError> {
        self.send(Method::GET, "/v4/info", &[], NO_BODY).await
    }

    pub async fn stats(&self) -> Result<StatsData, NodeError> {
        self.send(Method::GET, "/v4/stats", &[], NO_BODY).await
    }

    pub(crate) async fn enable_resuming(&self, session_id: &str) -> Result<Session, NodeError> {
        let update = SessionUpdate {
            resuming: Omissible::Present(true),
            timeout_seconds: Omissible::Present(60),
        };
        self.send(
            Method::PATCH,
            &format!("/v4/sessions/{session_id}"),
            &[],
            Some(&update),
        )
        .await
    }

    /// GET /v4/loadtracks. The identifier is whatever the user typed — a URL, a
    /// path, or a ytsearch:/scsearch: query.
    pub async fn load_tracks(&self, identifier: &str) -> Result<LoadResult, NodeError> {
        self.send(
            Method::GET,
            "/v4/loadtracks",
            &[("identifier", identifier)],
            NO_BODY,
        )
        .await
    }

    pub async fn update_player(
        &self,
        guild_id: u64,
        update: &PlayerUpdate,
        no_replace: bool,
    ) -> Result<Player, NodeError> {
        let session = self.session_id().await?;
        let path = format!("/v4/sessions/{session}/players/{guild_id}");
        self.send(
            Method::PATCH,
            &path,
            &[("noReplace", if no_replace { "true" } else { "false" })],
            Some(update),
        )
        .await
    }

    pub async fn player(&self, guild_id: u64) -> Result<Player, NodeError> {
        let session = self.session_id().await?;
        self.send(
            Method::GET,
            &format!("/v4/sessions/{session}/players/{guild_id}"),
            &[],
            NO_BODY,
        )
        .await
    }

    pub async fn players(&self) -> Result<Players, NodeError> {
        let session = self.session_id().await?;
        self.send(
            Method::GET,
            &format!("/v4/sessions/{session}/players"),
            &[],
            NO_BODY,
        )
        .await
    }

    /// DELETE answers 204 with no body, so it does not go through send.
    pub async fn destroy_player(&self, guild_id: u64) -> Result<(), NodeError> {
        let session = self.session_id().await?;
        self.request(
            Method::DELETE,
            &format!("/v4/sessions/{session}/players/{guild_id}"),
            &[],
            NO_BODY,
        )
        .await?;
        Ok(())
    }
}

/// None needs a type for the generic body parameter, and () serializes to
/// null rather than to nothing — this names the absent body once instead of
/// spelling the turbofish at every call site.
const NO_BODY: Option<&()> = None;
