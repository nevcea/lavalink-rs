//! REST error responses.
//!
//! Clients parse the body, so it has to be the original's shape — including path,
//! which a handler does not naturally know. Rather than thread the request URI
//! through every handler, a handler raises an ApiError and a middleware
//! (fill_error_path) renders it once, where the path is in scope.

use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts, Path, Query, Request};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use lavalink_protocol::http::Error as ErrorBody;
use serde::de::DeserializeOwned;
use tracing::Instrument;

use crate::player::now_epoch_ms;

#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    /// A load failure as a 400, preferring the exception's own message and
    /// falling back to its cause — the original never surfaces an empty one.
    /// This is for loadAudioItem's FriendlyException, which
    /// PlayerRestHandler.kt/AudioLoaderRestHandler.kt explicitly catch and
    /// turn into a 400 — not for decodeTrack, which throws a plain
    /// IllegalStateException/IllegalArgumentException neither handler
    /// catches, so it falls through to Spring's uncaught-exception 500 (see
    /// Self::decode_failed).
    pub fn from_exception(exception: lavalink_protocol::Exception) -> Self {
        Self::bad_request(exception.message.unwrap_or(exception.cause))
    }

    /// A decodeTrack failure (bad base64, mismatched track version, missing
    /// source manager) as a 500 — util.kt's decodeTrack throws a plain
    /// IllegalStateException/IllegalArgumentException, which no upstream
    /// handler catches, so it falls through to Spring's uncaught-exception
    /// path rather than the FriendlyException-only 400 Self::from_exception
    /// models.
    pub fn decode_failed(exception: lavalink_protocol::Exception) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            exception.message.unwrap_or(exception.cause),
        )
    }

    /// The original's wording, which some clients match on
    /// (util.kt:126).
    pub fn session_not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "Session not found")
    }

    /// util.kt:129.
    pub fn player_not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "Player not found")
    }

    /// A path that matches no route at all, so a client sees the same Error
    /// shape it gets from a stale session or player id, rather than axum's
    /// built-in empty-body 404.
    pub fn no_such_route(path: &str) -> Self {
        Self::new(StatusCode::NOT_FOUND, format!("No route found for {path}"))
    }

    /// A path that matches a route, but not with this HTTP method.
    ///
    /// This replaces axum's built-in 405 with the same Error shape, at the cost
    /// of the Allow header axum's default would have set (Spring sets one too,
    /// but nothing here reads it back out to reconstruct it from route
    /// registration).
    pub fn method_not_allowed(method: &axum::http::Method, path: &str) -> Self {
        Self::new(
            StatusCode::METHOD_NOT_ALLOWED,
            format!("Request method '{method}' is not supported for {path}"),
        )
    }

    /// The player actor is wedged or gone; the caller should retry.
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, message)
    }

    /// trace is Some only when the request asked for ?trace=true
    /// (wants_trace) — a stack trace is a JVM artefact this port has none of,
    /// so the message stands in for it. What trace=true actually toggles on
    /// the wire is the field's presence, and that much is reproduced exactly.
    fn body(&self, path: &str, trace: Option<String>) -> ErrorBody {
        ErrorBody {
            timestamp: now_epoch_ms(),
            status: self.status.as_u16(),
            error: self.status.canonical_reason().unwrap_or("Error").to_owned(),
            trace,
            message: self.message.clone(),
            path: path.to_owned(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // Deliberately bodyless. The path is not knowable here, and
        // [fill_error_path] — the outermost layer in rest::router, so nothing
        // reaches a client without passing through it — re-renders every response
        // carrying this extension once it does know. Rendering a body here as well
        // would serialize an ErrorBody and immediately throw it away on every
        // error the node returns.
        let mut response = self.status.into_response();
        response.extensions_mut().insert(self);
        response
    }
}

/// Whether the request asked for a trace via ?trace=true, matching the
/// original's @RequestParam boolean parsing closely enough for the only
/// value any real client sends.
fn wants_trace(query: Option<&str>) -> bool {
    query
        .unwrap_or("")
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .any(|(key, value)| key == "trace" && value.eq_ignore_ascii_case("true"))
}

/// Renders error responses, with the request path filled in.
///
/// ApiError's own IntoResponse leaves the body empty for this to fill, so this
/// is where an error response actually gets its JSON — not a second rendering of one.
pub async fn fill_error_path(request: Request, next: Next) -> Response {
    // The Uri, not the path: this runs on every request and only the error branch
    // below ever reads it, so allocating the path here spent a String per request
    // to serve the responses that don't have one. http::Uri is Bytes-backed, so
    // keeping the whole thing is a refcount bump.
    let method = request.method().clone();
    let uri = request.uri().clone();
    let trace_requested = wants_trace(uri.query());
    let span = tracing::debug_span!("http.request", %method, path = uri.path());
    let started = std::time::Instant::now();
    let mut response = next.run(request).instrument(span.clone()).await;

    // Taken out rather than cloned: this is the outermost layer, so nothing after it
    // reads the extension, and the error owns a String that would be copied for no
    // one on every error response.
    let error = response.extensions_mut().remove::<ApiError>();
    {
        let _entered = span.enter();
        log_response(response.status(), started.elapsed(), error.as_ref());
    }
    let Some(error) = error else { return response };
    let trace = trace_requested.then(|| error.message.clone());
    (error.status, Json(error.body(uri.path(), trace))).into_response()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponseLogLevel {
    Debug,
    Warn,
    Error,
}

fn response_log_level(status: StatusCode) -> ResponseLogLevel {
    if status.is_server_error() {
        ResponseLogLevel::Error
    } else if status.is_client_error() {
        ResponseLogLevel::Warn
    } else {
        ResponseLogLevel::Debug
    }
}

fn log_response(status: StatusCode, elapsed: std::time::Duration, error: Option<&ApiError>) {
    let error = error.map(|error| crate::logging::safe_error(format!("{error:?}")));
    let level = response_log_level(status);
    let status = status.as_u16();
    let elapsed_ms = elapsed.as_millis();
    match level {
        ResponseLogLevel::Debug => {
            tracing::debug!(status, elapsed_ms, error = ?error, "request completed")
        }
        ResponseLogLevel::Warn => {
            tracing::warn!(status, elapsed_ms, error = ?error, "request completed")
        }
        ResponseLogLevel::Error => {
            tracing::error!(status, elapsed_ms, error = ?error, "request completed")
        }
    }
}

/// axum::Json, but a malformed or wrongly-typed body becomes an ApiError
/// instead of axum's own plain-text rejection.
///
/// Without this, a request body that fails to deserialize skips ApiError
/// entirely — fill_error_path only rewrites a response that carries one
/// (response.extensions().get::<ApiError>()), so the client would see a
/// completely different shape (no path, no timestamp) from every other
/// error this API returns. The status is flattened to 400 for everything
/// except a missing Content-Type, which stays 415 — axum's own default for
/// that case, and the one the rest of this API's status codes don't override.
pub struct ValidatedJson<T>(pub T);

impl<S, T> FromRequest<S> for ValidatedJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        Json::<T>::from_request(req, state)
            .await
            .map(|Json(value)| Self(value))
            .map_err(|rejection: JsonRejection| match rejection {
                JsonRejection::MissingJsonContentType(_) => {
                    ApiError::new(StatusCode::UNSUPPORTED_MEDIA_TYPE, rejection.body_text())
                }
                _ => ApiError::bad_request(rejection.body_text()),
            })
    }
}

/// axum::extract::Query, but a missing required field or an unparsable value
/// becomes an ApiError instead of axum's own plain-text rejection — see
/// ValidatedJson, which exists for the same reason on the body side.
pub struct ValidatedQuery<T>(pub T);

impl<S, T> FromRequestParts<S> for ValidatedQuery<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Query::<T>::from_request_parts(parts, state)
            .await
            .map(|Query(value)| Self(value))
            .map_err(|rejection: QueryRejection| ApiError::bad_request(rejection.body_text()))
    }
}

/// axum::extract::Path, but a rejection (a non-UTF8 segment, a type mismatch)
/// becomes an ApiError instead of axum's own plain-text rejection — see
/// ValidatedJson, which exists for the same reason on the body side.
pub struct ValidatedPath<T>(pub T);

impl<S, T> FromRequestParts<S> for ValidatedPath<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Path::<T>::from_request_parts(parts, state)
            .await
            .map(|Path(value)| Self(value))
            .map_err(|rejection: PathRejection| ApiError::bad_request(rejection.body_text()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_body_matches_the_originals_shape() {
        let error = ApiError::session_not_found();
        let body = error.body("/v4/sessions/xyz/players", None);

        assert_eq!(body.status, 404);
        assert_eq!(body.error, "Not Found");
        assert_eq!(body.message, "Session not found");
        assert_eq!(body.path, "/v4/sessions/xyz/players");
        assert!(body.timestamp > 0);

        let json = serde_json::to_value(&body).unwrap();
        assert!(json.get("trace").is_none());
    }

    #[test]
    fn the_error_survives_in_the_response_extensions() {
        let response = ApiError::bad_request("nope").into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let error = response.extensions().get::<ApiError>().unwrap();
        assert_eq!(error.message, "nope");
    }

    #[test]
    fn trace_is_present_only_when_requested() {
        assert!(!wants_trace(None));
        assert!(!wants_trace(Some("")));
        assert!(!wants_trace(Some("trace=false")));
        assert!(!wants_trace(Some("identifier=foo")));
        assert!(wants_trace(Some("trace=true")));
        assert!(wants_trace(Some("identifier=foo&trace=true")));
        assert!(wants_trace(Some("trace=TRUE")));
    }

    #[test]
    fn response_statuses_choose_an_operator_visible_level() {
        assert_eq!(response_log_level(StatusCode::OK), ResponseLogLevel::Debug);
        assert_eq!(
            response_log_level(StatusCode::BAD_REQUEST),
            ResponseLogLevel::Warn
        );
        assert_eq!(
            response_log_level(StatusCode::INTERNAL_SERVER_ERROR),
            ResponseLogLevel::Error
        );
    }

    #[test]
    fn a_requested_trace_carries_the_errors_message() {
        let error = ApiError::session_not_found();
        let body = error.body("/v4/sessions/xyz/players", Some(error.message.clone()));

        assert_eq!(body.trace.as_deref(), Some("Session not found"));
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["trace"], "Session not found");
    }
}
