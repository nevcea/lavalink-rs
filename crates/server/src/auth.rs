//! Password check.
//!
//! The original compares with == (RequestAuthorizationFilter.kt:26), which on
//! the JVM short-circuits at the first differing character. That leaks the password
//! prefix-by-prefix to anyone who can time responses. The comparison here is
//! constant time.
//!
//! Status codes follow the original exactly: a missing header is 401, a wrong one is
//! 403. That is an unusual split — 403 normally means "authenticated but not
//! allowed" — but clients distinguish them, so it stays.

use axum::extract::{Request, State};
use axum::http::{header::AUTHORIZATION, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use subtle::ConstantTimeEq;

use crate::error::ApiError;
use crate::state::AppState;

pub async fn require_password(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let provided = request.headers().get(AUTHORIZATION);

    match provided {
        None => Err(ApiError::new(StatusCode::UNAUTHORIZED, "Unauthorized")),
        Some(provided)
            if matches(
                provided.as_bytes(),
                state.config.lavalink.server.password.as_bytes(),
            ) =>
        {
            Ok(next.run(request).await)
        }
        Some(_) => Err(ApiError::new(StatusCode::FORBIDDEN, "Forbidden")),
    }
}

/// Constant time in the length of the shorter input.
///
/// The lengths themselves are compared normally: whether two byte strings are
/// the same length is not secret in any useful way, and pretending otherwise
/// would mean hashing, which is more machinery than this needs.
///
/// Takes raw bytes, not &str: an Authorization header is present here
/// whether or not its bytes happen to be valid UTF-8 (RFC 7230 allows
/// obs-text, and the JVM decodes it as Latin-1 rather than rejecting it),
/// so comparing bytes is what keeps a non-UTF-8 header "present but wrong"
/// (403) instead of HeaderValue::to_str() silently failing and this
/// treating it as "absent" (401).
fn matches(provided: &[u8], expected: &[u8]) -> bool {
    if provided.len() != expected.len() {
        return false;
    }
    provided.ct_eq(expected).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_right_password_is_accepted() {
        assert!(matches(b"youshallnotpass", b"youshallnotpass"));
    }

    #[test]
    fn a_wrong_password_is_rejected() {
        assert!(!matches(b"youshallnotpasx", b"youshallnotpass"));
        assert!(!matches(b"", b"youshallnotpass"));
        assert!(!matches(b"youshallnotpass ", b"youshallnotpass"));
    }

    #[test]
    fn a_shared_prefix_is_still_rejected() {
        assert!(!matches(b"you", b"youshallnotpass"));
        assert!(!matches(b"youshallnotpassword", b"youshallnotpass"));
    }

    /// The bug this fix targets: a non-UTF-8 (but present) Authorization
    /// header must still compare as "wrong", not be silently treated as
    /// bytes that happen not to decode to anything comparable.
    #[test]
    fn non_utf8_bytes_are_compared_like_any_other_wrong_password() {
        assert!(!matches(&[0xff, 0xfe, 0xfd], b"youshallnotpass"));
    }
}
