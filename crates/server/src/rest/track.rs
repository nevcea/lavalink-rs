//! loadtracks and decodetrack(s).

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use lavalink_protocol::player::{EncodedTracks, Track, Tracks};
use lavalink_protocol::LoadResult;
use serde::Deserialize;

use crate::error::{ApiError, ValidatedJson, ValidatedQuery};
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct LoadQuery {
    identifier: String,
}

/// Always 200, even when loading fails — the failure travels in the body as
/// loadType: "error". Clients rely on this: a non-200 is treated as the node being
/// broken rather than the track being bad.
pub async fn load_tracks(
    State(state): State<AppState>,
    ValidatedQuery(query): ValidatedQuery<LoadQuery>,
) -> Json<Arc<LoadResult>> {
    tracing::info!(identifier = %query.identifier, "loading");
    // Serialized straight out of the Arc (serde's rc feature) rather than
    // unwrapped into an owned LoadResult: unwrapping is exactly the deep copy
    // the loader stopped making.
    Json(state.loader.load(&query.identifier).await)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DecodeQuery {
    /// The v4 name.
    encoded_track: Option<String>,
    /// The v3 name, still accepted.
    track: Option<String>,
}

pub async fn decode_track(
    State(state): State<AppState>,
    ValidatedQuery(query): ValidatedQuery<DecodeQuery>,
) -> Result<Json<Track>, ApiError> {
    let encoded = query
        .encoded_track
        .or(query.track)
        .ok_or_else(|| ApiError::bad_request("No track to decode provided"))?;

    state
        .loader
        .decode(&encoded)
        .map(Json)
        .map_err(ApiError::decode_failed)
}

pub async fn decode_tracks(
    State(state): State<AppState>,
    ValidatedJson(encoded): ValidatedJson<EncodedTracks>,
) -> Result<Json<Tracks>, ApiError> {
    if encoded.0.is_empty() {
        return Err(ApiError::bad_request("No tracks to decode provided"));
    }

    let tracks = encoded
        .0
        .iter()
        .map(|encoded| {
            state
                .loader
                .decode(encoded)
                .map_err(ApiError::decode_failed)
        })
        .collect::<Result<Vec<Track>, ApiError>>()?;

    Ok(Json(Tracks(tracks)))
}
