//! REST error responses.
//!
//! Clients parse the body, so it has to be the original's shape — including `path`,
//! which a handler does not naturally know. Rather than thread the request URI
//! through every handler, a handler raises an [`ApiError`] and a middleware
//! ([`fill_error_path`]) renders it once, where the path is in scope.

use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts, Query, Request};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use lavalink_protocol::error::Error as ErrorBody;
use serde::de::DeserializeOwned;

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

    /// A load or decode failure as a 400, preferring the exception's own message and
    /// falling back to its cause — the original never surfaces an empty one.
    pub fn from_exception(exception: lavalink_protocol::Exception) -> Self {
        Self::bad_request(exception.message.unwrap_or(exception.cause))
    }

    /// The original's wording, which some clients match on
    /// (`util.kt:126`).
    pub fn session_not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "Session not found")
    }

    /// `util.kt:129`.
    pub fn player_not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "Player not found")
    }

    /// A path that matches no route at all, so a client sees the same `Error`
    /// shape it gets from a stale session or player id, rather than axum's
    /// built-in empty-body 404.
    pub fn no_such_route(path: &str) -> Self {
        Self::new(StatusCode::NOT_FOUND, format!("No route found for {path}"))
    }

    /// A path that matches a route, but not with this HTTP method.
    ///
    /// This replaces axum's built-in 405 with the same `Error` shape, at the cost
    /// of the `Allow` header axum's default would have set (Spring sets one too,
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

    fn body(&self, path: &str) -> ErrorBody {
        ErrorBody {
            timestamp: now_epoch_ms(),
            status: self.status.as_u16(),
            error: self.status.canonical_reason().unwrap_or("Error").to_owned(),
            // Stack traces are a JVM artefact; `trace=true` has nothing to return
            // here, so the key stays absent rather than carrying a fake.
            trace: None,
            message: self.message.clone(),
            path: path.to_owned(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // Rendered without a path; the middleware replaces this once it knows one.
        let mut response = (self.status, Json(self.body(""))).into_response();
        response.extensions_mut().insert(self);
        response
    }
}

/// Re-renders error responses with the request path filled in.
pub async fn fill_error_path(request: Request, next: Next) -> Response {
    let path = request.uri().path().to_owned();
    let response = next.run(request).await;

    let Some(error) = response.extensions().get::<ApiError>().cloned() else {
        return response;
    };
    (error.status, Json(error.body(&path))).into_response()
}

/// `axum::Json`, but a malformed or wrongly-typed body becomes an [`ApiError`]
/// instead of axum's own plain-text rejection.
///
/// Without this, a request body that fails to deserialize skips `ApiError`
/// entirely — `fill_error_path` only rewrites a response that carries one
/// (`response.extensions().get::<ApiError>()`), so the client would see a
/// completely different shape (no `path`, no `timestamp`) from every other
/// error this API returns, plus whatever status axum's `JsonRejection` happens
/// to pick (400 for bad syntax, 415 for a missing content type, 422 for a
/// type mismatch) rather than the flat 400 the rest of this API uses.
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
            .map_err(|rejection: JsonRejection| ApiError::bad_request(rejection.body_text()))
    }
}

/// `axum::extract::Query`, but a missing required field or an unparsable value
/// becomes an [`ApiError`] instead of axum's own plain-text rejection — see
/// [`ValidatedJson`], which exists for the same reason on the body side.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_body_matches_the_originals_shape() {
        let error = ApiError::session_not_found();
        let body = error.body("/v4/sessions/xyz/players");

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
}
