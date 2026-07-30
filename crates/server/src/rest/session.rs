//! `PATCH /v4/sessions/{sessionId}`.

use axum::extract::{Path, State};
use axum::Json;
use lavalink_protocol::session::{Session, SessionUpdate};
use lavalink_protocol::Omissible;

use crate::error::{ApiError, ValidatedJson};
use crate::state::AppState;

pub async fn update(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    ValidatedJson(update): ValidatedJson<SessionUpdate>,
) -> Result<Json<Session>, ApiError> {
    let session = crate::rest::session(&state, &session_id)?;

    // Both fields are independently omissible: sending only one leaves the other
    // as it was.
    if let Omissible::Present(resuming) = update.resuming {
        session.set_resuming(resuming);
    }
    if let Omissible::Present(timeout) = update.timeout_seconds {
        session.set_resume_timeout_secs(timeout.max(0) as u64);
    }

    Ok(Json(Session {
        resuming: session.resuming(),
        timeout_seconds: session.resume_timeout_secs() as i64,
    }))
}
