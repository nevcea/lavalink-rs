//! The v4 REST surface.

pub mod info;
pub mod player;
pub mod session;
pub mod track;

use axum::http::{Method, Uri};
use axum::routing::{get, patch, post};
use axum::Router;

use crate::error::ApiError;
use crate::state::AppState;

pub fn router(state: AppState) -> Router {
    let v4 = Router::new()
        .route("/v4/info", get(info::info))
        .route("/v4/stats", get(info::stats))
        .route("/v4/websocket", get(crate::ws::handler))
        .route("/v4/loadtracks", get(track::load_tracks))
        .route("/v4/decodetrack", get(track::decode_track))
        .route("/v4/decodetracks", post(track::decode_tracks))
        .route("/v4/sessions/{session_id}", patch(session::update))
        .route(
            "/v4/sessions/{session_id}/players",
            get(player::list_players),
        )
        .route(
            "/v4/sessions/{session_id}/players/{guild_id}",
            get(player::get_player)
                .patch(player::patch_player)
                .delete(player::delete_player),
        )
        // Route planning belongs to the IP-rotation feature, which is out of scope.
        // The original's own behaviour with no route planner configured (the only
        // state this node can ever be in) is what's matched here, not a made-up
        // "not implemented" status: `getStatus` returns 204 with no body, and both
        // `POST` handlers throw `RoutePlannerDisabledException`, a plain 500
        // (`RoutePlannerRestHandler.kt`).
        .route("/v4/routeplanner/status", get(route_planner_status))
        .route(
            "/v4/routeplanner/free/address",
            post(route_planner_disabled),
        )
        .route("/v4/routeplanner/free/all", post(route_planner_disabled));

    Router::new()
        .route("/version", get(info::version))
        .merge(v4)
        // Both must come after every `.route`/`.merge` above: `fallback` only
        // covers paths that match nothing at all, and `method_not_allowed_fallback`
        // installs itself on every `MethodRouter` registered so far, so it has to
        // run last to reach all of them. Both replace axum's default empty-body
        // 404/405 with the same `Error` JSON shape a client gets from a stale
        // session or player id.
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::auth::require_password,
        ))
        .layer(axum::middleware::from_fn(crate::error::fill_error_path))
        .with_state(state)
}

async fn route_planner_status() -> axum::http::StatusCode {
    axum::http::StatusCode::NO_CONTENT
}

async fn route_planner_disabled() -> ApiError {
    ApiError::new(
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        "Can't access disabled route planner",
    )
}

async fn not_found(uri: Uri) -> ApiError {
    ApiError::no_such_route(uri.path())
}

async fn method_not_allowed(method: Method, uri: Uri) -> ApiError {
    ApiError::method_not_allowed(&method, uri.path())
}

/// Parses a snowflake path segment.
///
/// The original types the parameter as `Long` and lets Spring reject anything else
/// with a 400; doing it by hand keeps the error body ours.
pub fn parse_guild_id(raw: &str) -> Result<u64, ApiError> {
    raw.parse::<u64>()
        .map_err(|_| ApiError::bad_request(format!("Invalid guild id: {raw}")))
}

/// Resolves a session id, or reports the original's 404.
///
/// Serves sessions awaiting resume as well as open ones: such a session is alive,
/// only its websocket is gone.
pub fn session(
    state: &AppState,
    session_id: &str,
) -> Result<std::sync::Arc<crate::session::Session>, ApiError> {
    state
        .sessions
        .get(session_id)
        .ok_or_else(ApiError::session_not_found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, Request as HttpRequest, StatusCode};
    use tower::ServiceExt;

    #[test]
    fn guild_ids_must_be_snowflakes() {
        assert_eq!(parse_guild_id("123").unwrap(), 123);
        assert!(parse_guild_id("abc").is_err());
        assert!(parse_guild_id("-1").is_err());
        assert!(parse_guild_id("").is_err());
    }

    fn test_state() -> AppState {
        let mut config = crate::config::Config::default();
        config.lavalink.server.password = "test".into();
        crate::state::AppState::new(
            config,
            crate::loader::Loader::new(Vec::new()),
            crate::audio::stream::StreamOpener::default(),
            std::time::Instant::now(),
        )
    }

    async fn body_json(response: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// An unmatched path with no credentials must fail on auth before it ever
    /// reaches the fallback — the auth layer wraps the fallback router too.
    #[tokio::test]
    async fn an_unknown_path_without_a_password_is_401_not_404() {
        let app = router(test_state());
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/v4/nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    /// An authenticated request to an unmatched path gets the Lavalink `Error`
    /// JSON shape, not axum's built-in empty-body 404.
    #[tokio::test]
    async fn an_unknown_path_gets_the_lavalink_error_shape() {
        let app = router(test_state());
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/v4/nope")
                    .header(header::AUTHORIZATION, "test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = body_json(response).await;
        assert_eq!(body["status"], 404);
        assert_eq!(body["error"], "Not Found");
        assert_eq!(body["path"], "/v4/nope");
    }

    /// `?trace=true` must populate a non-null `trace` field — dropped
    /// entirely otherwise, per `the_body_matches_the_originals_shape` in
    /// `error.rs`.
    #[tokio::test]
    async fn a_trace_query_param_populates_the_trace_field() {
        let app = router(test_state());
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/v4/nope?trace=true")
                    .header(header::AUTHORIZATION, "test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = body_json(response).await;
        assert!(body["trace"].is_string(), "expected a trace, got {body:?}");
    }

    /// A known path with the wrong HTTP method gets the same `Error` shape as a
    /// 405, not axum's default bare status line.
    #[tokio::test]
    async fn a_known_path_with_the_wrong_method_gets_the_lavalink_error_shape() {
        let app = router(test_state());
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .method(Method::POST)
                    .uri("/v4/info")
                    .header(header::AUTHORIZATION, "test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        let body = body_json(response).await;
        assert_eq!(body["status"], 405);
        assert_eq!(body["error"], "Method Not Allowed");
        assert_eq!(body["path"], "/v4/info");
    }

    /// A request axum's own extractors reject (a missing required query
    /// parameter, here) must still get the Lavalink `Error` JSON shape, the
    /// same as every other error this API returns — not axum's own bare
    /// plain-text rejection body, which carries no `path` and a status
    /// `fill_error_path` never gets a chance to normalize.
    #[tokio::test]
    async fn a_missing_required_query_param_gets_the_lavalink_error_shape() {
        let app = router(test_state());
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/v4/loadtracks")
                    .header(header::AUTHORIZATION, "test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_json(response).await;
        assert_eq!(body["status"], 400);
        assert_eq!(body["path"], "/v4/loadtracks");
    }

    /// Same as above, for a malformed request body instead of a missing query
    /// parameter.
    #[tokio::test]
    async fn a_malformed_json_body_gets_the_lavalink_error_shape() {
        let app = router(test_state());
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .method(Method::PATCH)
                    .uri("/v4/sessions/whatever")
                    .header(header::AUTHORIZATION, "test")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{not json"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_json(response).await;
        assert_eq!(body["status"], 400);
        assert_eq!(body["path"], "/v4/sessions/whatever");
    }

    /// A JSON body sent with no `Content-Type` header must keep axum's own 415
    /// for that specific case, not the flat 400 every other body rejection
    /// gets here — the same status Spring's default gives it, which is what a
    /// client of the original would see.
    #[tokio::test]
    async fn a_json_body_with_no_content_type_is_415_not_400() {
        let app = router(test_state());
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .method(Method::PATCH)
                    .uri("/v4/sessions/whatever")
                    .header(header::AUTHORIZATION, "test")
                    .body(Body::from(r#"{"resuming":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let body = body_json(response).await;
        assert_eq!(body["status"], 415);
        assert_eq!(body["path"], "/v4/sessions/whatever");
    }

    /// A path segment axum's own `Path` extractor rejects (invalid UTF-8, here)
    /// must still get the Lavalink `Error` JSON shape — see
    /// [`crate::error::ValidatedPath`], which exists for the same reason
    /// [`ValidatedJson`](crate::error::ValidatedJson) does on the body side.
    #[tokio::test]
    async fn a_non_utf8_path_segment_gets_the_lavalink_error_shape() {
        let app = router(test_state());
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/v4/sessions/%ff/players")
                    .header(header::AUTHORIZATION, "test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_json(response).await;
        assert_eq!(body["status"], 400);
        assert_eq!(body["path"], "/v4/sessions/%ff/players");
    }

    /// `RoutePlannerRestHandler.kt::getStatus` returns 204 with no body when no
    /// route planner is configured, which is the only state this node is ever
    /// in — not a 404 or 501.
    #[tokio::test]
    async fn route_planner_status_is_204_with_no_route_planner_configured() {
        let app = router(test_state());
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/v4/routeplanner/status")
                    .header(header::AUTHORIZATION, "test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(bytes.is_empty());
    }

    /// Both free-address endpoints throw `RoutePlannerDisabledException` in the
    /// original, a plain 500 — not the Lavalink `Error` shape's own 501.
    #[tokio::test]
    async fn route_planner_free_endpoints_are_500_with_no_route_planner_configured() {
        for path in [
            "/v4/routeplanner/free/address",
            "/v4/routeplanner/free/all",
        ] {
            let app = router(test_state());
            let response = app
                .oneshot(
                    HttpRequest::builder()
                        .method(Method::POST)
                        .uri(path)
                        .header(header::AUTHORIZATION, "test")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
            let body = body_json(response).await;
            assert_eq!(body["message"], "Can't access disabled route planner");
        }
    }
}
