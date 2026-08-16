//! `/version`, `/v4/info`, `/v4/stats`.

use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::response::IntoResponse;
use axum::Json;
use lavalink_protocol::stats::StatsData;

use crate::state::AppState;

/// Plain text, not JSON — the original returns the bare version string.
/// `state.version_text` is pre-built in `AppState::new`, so this only bumps a
/// refcount instead of cloning the `String` on every request. The content-type
/// matches what axum's `String` `IntoResponse` sets by default, since that's
/// what this replaces.
pub async fn version(State(state): State<AppState>) -> impl IntoResponse {
    (
        [(CONTENT_TYPE, "text/plain; charset=utf-8")],
        state.version_text.clone(),
    )
}

/// `Info` never changes after startup, so this serves the bytes serialized
/// once in `AppState::new` instead of re-serializing (and deep-cloning
/// `Info`'s `Vec`/`String` fields) on every request.
pub async fn info(State(state): State<AppState>) -> impl IntoResponse {
    ([(CONTENT_TYPE, "application/json")], state.info_json.clone())
}

/// `frameStats` is always absent from this endpoint (`docs/api/rest.md:989`);
/// [`StatsData`] enforces that by construction.
pub async fn stats(State(state): State<AppState>) -> Json<StatsData> {
    let sessions = state.sessions.all();
    let (players, playing) = crate::stats::count_sessions(&sessions);
    Json(state.stats.sample(players, playing))
}
