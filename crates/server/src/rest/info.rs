//! `/version`, `/v4/info`, `/v4/stats`.

use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::response::IntoResponse;
use axum::Json;
use lavalink_protocol::stats::StatsData;

use crate::state::AppState;

/// Plain text, not JSON — the original returns the bare version string.
pub async fn version(State(state): State<AppState>) -> String {
    state.info.version.semver.clone()
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
    let (players, playing) = crate::stats::count(&crate::stats::rosters(&sessions));
    Json(state.stats.sample(players, playing))
}
