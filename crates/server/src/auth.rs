//! Password check.
//!
//! The original compares with `==` (`RequestAuthorizationFilter.kt:26`), which on
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
    let provided = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok());

    match provided {
        None => Err(ApiError::new(StatusCode::UNAUTHORIZED, "Unauthorized")),
        Some(provided) if matches(provided, &state.config.lavalink.server.password) => {
            Ok(next.run(request).await)
        }
        Some(_) => Err(ApiError::new(StatusCode::FORBIDDEN, "Forbidden")),
    }
}

/// Constant time in the length of the shorter input.
///
/// The lengths themselves are compared normally: whether two strings are the same
/// length is not secret in any useful way, and pretending otherwise would mean
/// hashing, which is more machinery than this needs.
fn matches(provided: &str, expected: &str) -> bool {
    if provided.len() != expected.len() {
        return false;
    }
    provided.as_bytes().ct_eq(expected.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_right_password_is_accepted() {
        assert!(matches("youshallnotpass", "youshallnotpass"));
    }

    #[test]
    fn a_wrong_password_is_rejected() {
        assert!(!matches("youshallnotpasx", "youshallnotpass"));
        assert!(!matches("", "youshallnotpass"));
        assert!(!matches("youshallnotpass ", "youshallnotpass"));
    }

    #[test]
    fn a_shared_prefix_is_still_rejected() {
        assert!(!matches("you", "youshallnotpass"));
        assert!(!matches("youshallnotpassword", "youshallnotpass"));
    }
}
