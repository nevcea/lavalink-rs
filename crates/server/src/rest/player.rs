//! Player endpoints, and in particular `PATCH`, which is where most of the v4
//! surface lives.
//!
//! Validation order and error wording follow `PlayerRestHandler.kt` literally.
//! Clients match on some of these strings, and the order decides which of two
//! simultaneous mistakes gets reported.
//!
//! Everything slow happens *here*, not in the actor: resolving an identifier and
//! decoding an encoded track are done first, and the actor receives a finished
//! [`PatchRequest`]. That is also what removes the original's worst lock —
//! `synchronized(player)` wrapped around a blocking voice connect.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use lavalink_protocol::player::{Player, PlayerUpdate, PlayerUpdateTrack, Players, VoiceState};
use lavalink_protocol::{LoadResult, Omissible, Track};
use serde::Deserialize;

use crate::error::{ApiError, ValidatedJson, ValidatedPath, ValidatedQuery};
use crate::player::{PatchRequest, TrackChange};
use crate::rest::{parse_guild_id, session};
use crate::session::PLAYER_DESTROY_TIMEOUT;
use crate::state::AppState;

pub async fn list_players(
    State(state): State<AppState>,
    ValidatedPath(session_id): ValidatedPath<String>,
) -> Result<Json<Players>, ApiError> {
    let session = session(&state, &session_id)?;

    // A player whose actor has gone is simply not listed, rather than failing
    // the whole request.
    let snapshots = futures_util::future::join_all(
        session.players().into_iter().map(|handle| async move { handle.snapshot().await }),
    )
    .await;
    let players = snapshots.into_iter().filter_map(Result::ok).collect();

    Ok(Json(Players(players)))
}

pub async fn get_player(
    State(state): State<AppState>,
    ValidatedPath((session_id, guild_id)): ValidatedPath<(String, String)>,
) -> Result<Json<Player>, ApiError> {
    let session = session(&state, &session_id)?;
    let guild_id = parse_guild_id(&guild_id)?;

    // GET does not create a player — the original's 404 here is the difference
    // between "no player" and "an idle player", and clients use it.
    let handle = session
        .player(guild_id)
        .ok_or_else(ApiError::player_not_found)?;

    handle
        .snapshot()
        .await
        .map(Json)
        .map_err(|_| ApiError::player_not_found())
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PatchQuery {
    #[serde(default)]
    no_replace: bool,
}

/// `path`, `query` and `body` are wrapped in `Result` and resolved in that
/// order inside the body, rather than let axum short-circuit on whichever
/// fails first in declaration order. axum requires the body-consuming
/// extractor to be declared last, but the original resolves `@RequestBody`
/// first (`PlayerRestHandler.kt`'s parameter order) — this module's own docs
/// on why that order is wire-visible. Declaring them plainly would report a
/// bad guild id even when the body is *also* malformed, which is not what a
/// client of the original sees.
pub async fn patch_player(
    State(state): State<AppState>,
    path: Result<ValidatedPath<(String, String)>, ApiError>,
    query: Result<ValidatedQuery<PatchQuery>, ApiError>,
    body: Result<ValidatedJson<PlayerUpdate>, ApiError>,
) -> Result<Json<Player>, ApiError> {
    let ValidatedJson(update) = body?;
    let ValidatedPath((session_id, guild_id)) = path?;
    let ValidatedQuery(query) = query?;

    let session = session(&state, &session_id)?;
    let guild_id = parse_guild_id(&guild_id)?;

    let track_fields = resolve_track_fields(update.track, update.encoded_track, update.identifier)?;

    if let Omissible::Present(filters) = &update.filters {
        let invalid = filters.validate(&state.disabled_filters);
        if !invalid.is_empty() {
            return Err(ApiError::bad_request(format!(
                "Following filters are disabled in the config: {}",
                invalid.join(", ")
            )));
        }
    }

    if let Omissible::Present(voice) = &update.voice {
        validate_voice(voice)?;
    }

    if let Omissible::Present(Some(end_time)) = update.end_time {
        if end_time <= 0 {
            return Err(ApiError::bad_request("End time must be greater than 0"));
        }
    }

    // The voice connection comes from this same call, not a second
    // session.voice(guild_id) lookup: a teardown landing between two
    // independent lookups could make the second one silently see a different
    // (or torn-down) guild than the first, which is exactly the race
    // AppState::player's pairing exists to close.
    let (handle, connection) = state
        .player(&session, guild_id)
        .ok_or_else(player_gone)?;

    let wants_track_change = track_fields.encoded.is_present() || track_fields.identifier.is_present();

    // noReplace with something already playing drops the request before a
    // resolution is even attempted (`PlayerRestHandler.kt`: the check runs
    // before `decodeTrack`/`loadAudioItem` are called), not merely after
    // resolving succeeds or fails. A snapshot is a cheap round trip through
    // the actor's own queue, not a blocking wait, so this does not reintroduce
    // the slow-source-blocks-the-guild problem resolving-before-touching-the-
    // actor exists to avoid.
    let dropped_by_no_replace = wants_track_change
        && query.no_replace
        && handle
            .snapshot()
            .await
            .is_ok_and(|player| player.track.is_some());

    // Resolving happens before the actor sees a track change, so a slow source
    // cannot hold up anything else in this guild.
    let track = if dropped_by_no_replace {
        None
    } else {
        match &track_fields.encoded {
            Omissible::Present(Some(encoded)) => Some(TrackChange::Play(Box::new(
                state
                    .loader
                    .decode(encoded)
                    .map_err(ApiError::decode_failed)?,
            ))),
            Omissible::Present(None) => Some(TrackChange::Clear),
            Omissible::Omitted => match &track_fields.identifier {
                Omissible::Present(identifier) => {
                    Some(TrackChange::Play(Box::new(load_one(&state, identifier).await?)))
                }
                Omissible::Omitted => None,
            },
        }
    };

    // Connecting happens here, awaited, before the actor is told anything, so a
    // failure can become a status code. The original wraps this in
    // .exceptionally { throw … } and then .join()s it, so the intended 500 is
    // buried inside a CompletionException and the client sees something else.
    if let Omissible::Present(voice) = &update.voice {
        connection.connect(voice).await.map_err(|error| {
            tracing::warn!(guild_id, %error, "voice connection failed");
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to connect to voice server",
            )
        })?;

        // A DELETE landing between state.player() above and this await already
        // tore this guild's player down — engine.shutdown() calls voice.leave() —
        // and nothing owns the connection this connect just re-established.
        // Leave again rather than hand a patch to an actor no longer registered.
        //
        // Checked by identity, not just presence: a fresh player (and voice
        // connection) can already have been built in its place by the time this
        // await returns, and session.player(guild_id).is_none() alone can't tell
        // that apart from "still ours" — it would leave this stale connection
        // IDENTIFY'd to Discord while a different, freshly-registered connection
        // is also live for the guild, racing or knocking out the real one.
        if !is_current_connection(&session, guild_id, &connection) {
            connection.leave().await;
            return Err(player_gone());
        }
    }

    let request = PatchRequest {
        voice: update.voice.into_option(),
        paused: update.paused,
        user_data: track_fields.user_data,
        volume: update.volume,
        position: update.position,
        end_time: update.end_time,
        filters: update.filters,
        track,
        no_replace: query.no_replace,
    };

    handle
        .patch(request)
        .await
        .map(Json)
        .map_err(|_| player_gone())
}

/// Whether `connection` is still the one registered for `guild_id` — pulled
/// out of `patch_player`'s post-connect re-check so the race it guards
/// against (see the comment at its call site) is directly testable without a
/// live voice connection or HTTP round trip.
fn is_current_connection(
    session: &crate::session::Session,
    guild_id: u64,
    connection: &std::sync::Arc<crate::voice::VoiceConnection>,
) -> bool {
    matches!(session.voice(guild_id), Some(current) if std::sync::Arc::ptr_eq(&current, connection))
}

fn player_gone() -> ApiError {
    ApiError::unavailable("The player is not accepting commands")
}

/// 204, and this one really is 204 — unlike `PATCH`, whose `@ResponseStatus`
/// annotation on the original is dead code because the method returns a
/// `ResponseEntity.ok()`. We follow the observed behaviour of each, not the
/// annotations.
pub async fn delete_player(
    State(state): State<AppState>,
    ValidatedPath((session_id, guild_id)): ValidatedPath<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let session = session(&state, &session_id)?;
    let guild_id = parse_guild_id(&guild_id)?;

    // Removed whether or not the actor acknowledges, because dropping the last
    // PlayerHandle is itself a teardown: the destroy channel is held only by the
    // handle, so the actor's own recv reports every sender gone and it runs
    // engine.shutdown() on its way out. Keeping the entry on a failed destroy
    // looked like the careful choice, but wedged the guild instead — every later
    // PATCH found the same unresponsive handle and answered 503 for good, while
    // the client had already been told 204 here.
    if let Some(handle) = session.remove_player(guild_id) {
        // Bounded for the same reason Session::shutdown bounds its own destroys:
        // the result is already ignored (removal above is the teardown that
        // counts), so this wait is a courtesy, and an unbounded courtesy is a
        // hung request every time a client retries the DELETE.
        let _ = tokio::time::timeout(PLAYER_DESTROY_TIMEOUT, handle.destroy()).await;
    }

    // Deleting a player that is not there succeeds; the original's destroyPlayer
    // is a no-op in that case.
    Ok(StatusCode::NO_CONTENT)
}

/// The three fields that can name a track, reconciled from the v4 shape and the two
/// deprecated top-level fields.
#[derive(Debug)]
struct TrackFields {
    encoded: Omissible<Option<String>>,
    identifier: Omissible<String>,
    user_data: Omissible<lavalink_protocol::player::JsonObject>,
}

fn resolve_track_fields(
    track: Omissible<PlayerUpdateTrack>,
    encoded_track: Omissible<Option<String>>,
    identifier: Omissible<String>,
) -> Result<TrackFields, ApiError> {
    let legacy_present = encoded_track.is_present() || identifier.is_present();

    if track.is_present() && legacy_present {
        return Err(ApiError::bad_request(
            "Cannot specify both track and encodedTrack/identifier",
        ));
    }

    let track = match track {
        Omissible::Present(track) => track,
        Omissible::Omitted if legacy_present => PlayerUpdateTrack {
            encoded: encoded_track,
            identifier,
            user_data: Omissible::Omitted,
        },
        Omissible::Omitted => PlayerUpdateTrack::default(),
    };

    if track.encoded.is_present() && track.identifier.is_present() {
        return Err(ApiError::bad_request(
            "Cannot specify both encodedTrack and identifier",
        ));
    }

    Ok(TrackFields {
        encoded: track.encoded,
        identifier: track.identifier,
        user_data: track.user_data,
    })
}

/// Discord sometimes sends a partial voice server update with no endpoint; the
/// original rejects the whole request rather than half-applying it.
fn validate_voice(voice: &VoiceState) -> Result<(), ApiError> {
    let blank = voice.token.trim().is_empty()
        || voice.endpoint.trim().is_empty()
        || voice.session_id.trim().is_empty()
        || voice
            .channel_id
            .as_ref()
            .is_none_or(|id| id.trim().is_empty());

    if blank {
        return Err(ApiError::bad_request(
            "token, endpoint, sessionId and channelId must be provided in voice state",
        ));
    }
    Ok(())
}

/// Resolves an identifier to exactly one track, with the original's messages for
/// each way that can fail (`PlayerRestHandler.kt:196-203`).
async fn load_one(state: &AppState, identifier: &str) -> Result<Track, ApiError> {
    // Matched by reference and cloned per arm: the result is shared with the cache
    // and with anyone else waiting on the same load, and the two arms that need
    // owned data need one track or one exception — never the whole playlist a
    // move-out would have copied.
    match &*state.loader.load(identifier).await {
        LoadResult::Track(track) => Ok((**track).clone()),
        LoadResult::Empty => Err(ApiError::bad_request("No matches found for identifier")),
        LoadResult::Playlist(_) | LoadResult::Search(_) => Err(ApiError::bad_request(
            "Cannot play a playlist or search result",
        )),
        LoadResult::Error(exception) => Err(ApiError::from_exception(exception.clone())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(json: &str) -> PlayerUpdate {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn the_modern_and_legacy_track_fields_cannot_be_mixed() {
        let update = update(r#"{"track":{"identifier":"x"},"identifier":"y"}"#);
        let error =
            resolve_track_fields(update.track, update.encoded_track, update.identifier)
                .unwrap_err();
        assert_eq!(error.message, "Cannot specify both track and encodedTrack/identifier");
    }

    #[test]
    fn encoded_and_identifier_cannot_be_mixed() {
        let update = update(r#"{"track":{"encoded":"a","identifier":"b"}}"#);
        let error =
            resolve_track_fields(update.track, update.encoded_track, update.identifier)
                .unwrap_err();
        assert_eq!(error.message, "Cannot specify both encodedTrack and identifier");
    }

    #[test]
    fn the_legacy_fields_are_still_honoured() {
        let update = update(r#"{"encodedTrack":"abc"}"#);
        let fields =
            resolve_track_fields(update.track, update.encoded_track, update.identifier).unwrap();
        assert_eq!(fields.encoded, Omissible::Present(Some("abc".into())));
        assert!(fields.identifier.is_omitted());
    }

    /// The distinction the three-state wrapper exists for: an explicit null is a
    /// stop request, an absent field is "leave the track alone".
    #[test]
    fn a_null_encoded_track_is_distinguishable_from_an_absent_one() {
        let clear_update = update(r#"{"track":{"encoded":null}}"#);
        let cleared = resolve_track_fields(
            clear_update.track,
            clear_update.encoded_track,
            clear_update.identifier,
        )
        .unwrap();
        assert_eq!(cleared.encoded, Omissible::Present(None));

        let empty_update = update("{}");
        let untouched = resolve_track_fields(
            empty_update.track,
            empty_update.encoded_track,
            empty_update.identifier,
        )
        .unwrap();
        assert!(untouched.encoded.is_omitted());
    }

    #[test]
    fn a_partial_voice_state_is_rejected() {
        let voice = |json: &str| serde_json::from_str::<VoiceState>(json).unwrap();

        assert!(validate_voice(&voice(
            r#"{"token":"t","endpoint":"e","sessionId":"s","channelId":"c"}"#
        ))
        .is_ok());

        for partial in [
            r#"{"token":"","endpoint":"e","sessionId":"s","channelId":"c"}"#,
            r#"{"token":"t","endpoint":"","sessionId":"s","channelId":"c"}"#,
            r#"{"token":"t","endpoint":"e","sessionId":"","channelId":"c"}"#,
            r#"{"token":"t","endpoint":"e","sessionId":"s","channelId":null}"#,
            r#"{"token":"t","endpoint":"e","sessionId":"s","channelId":"  "}"#,
        ] {
            assert!(
                validate_voice(&voice(partial)).is_err(),
                "should have been rejected: {partial}"
            );
        }
    }

    #[test]
    fn no_replace_defaults_to_false() {
        assert!(!PatchQuery::default().no_replace);
        let explicit: PatchQuery = serde_json::from_str(r#"{"noReplace":true}"#).unwrap();
        assert!(explicit.no_replace);
    }

    fn dummy_pair(
        guild_id: u64,
    ) -> (crate::player::PlayerHandle, std::sync::Arc<crate::voice::VoiceConnection>) {
        crate::testing::dummy_pair(guild_id, std::sync::Arc::new(crate::sink::Sink::new()))
    }

    /// `patch_player`'s post-connect re-check exists for exactly this: a `DELETE`
    /// racing a slow `PATCH` builds a fresh player (and voice connection) for the
    /// guild before the slow `PATCH`'s own `connect().await` returns. Presence
    /// alone (`session.player(guild_id).is_some()`) can't tell "still ours" apart
    /// from "replaced", which is why the check compares by pointer instead — this
    /// exercises `is_current_connection` the same way `patch_player` does, without
    /// a live voice connection or HTTP round trip.
    /// `noReplace` must stop a resolution attempt from ever starting, not just
    /// discard its result: `PlayerRestHandler.kt` checks `noReplace &&
    /// player.track != null` before calling `decodeTrack`/`loadAudioItem` at
    /// all, so a client sending `noReplace=true` against a player that's
    /// already playing gets `200` with the unchanged player even when the
    /// `identifier` it sent would fail to resolve. `test_state()`'s loader has
    /// no source managers registered, so any `load_one` call here would fail —
    /// this only passes if resolution is skipped outright.
    #[tokio::test]
    async fn no_replace_skips_resolution_entirely_when_already_playing() {
        let mut config = crate::config::Config::default();
        config.lavalink.server.password = "test".into();
        let state = crate::state::AppState::new(
            config,
            crate::loader::Loader::new(Vec::new()),
            crate::audio::stream::StreamOpener::default(),
            std::time::Instant::now(),
            tokio::sync::watch::channel(()).1,
        );
        let session = state.sessions.open(1, None);
        let guild_id = 55;

        session
            .get_or_create_player(guild_id, || dummy_pair(guild_id))
            .unwrap();
        let (handle, _) = state.player(&session, guild_id).unwrap();
        handle
            .patch(PatchRequest {
                track: Some(TrackChange::Play(Box::new(crate::testing::track("first")))),
                ..Default::default()
            })
            .await
            .unwrap();

        let update: PlayerUpdate = serde_json::from_str(r#"{"identifier":"unresolvable"}"#).unwrap();
        let result = patch_player(
            State(state.clone()),
            Ok(ValidatedPath((session.id.clone(), guild_id.to_string()))),
            Ok(ValidatedQuery(PatchQuery { no_replace: true })),
            Ok(ValidatedJson(update)),
        )
        .await
        .expect("a dropped noReplace request must not surface a resolution error");

        assert_eq!(result.0.track.unwrap().info.title, "first");
    }

    #[tokio::test]
    async fn is_current_connection_rejects_a_connection_replaced_by_a_racing_delete() {
        let session = crate::session::SessionRegistry::new().open(1, None);
        let guild_id = 9;

        let (_handle, stale) = session.get_or_create_player(guild_id, || dummy_pair(guild_id)).unwrap();
        assert!(is_current_connection(&session, guild_id, &stale));

        session.remove_player(guild_id);
        let (_handle, fresh) = session.get_or_create_player(guild_id, || dummy_pair(guild_id)).unwrap();

        assert!(
            !is_current_connection(&session, guild_id, &stale),
            "the stale connection must no longer read as current"
        );
        assert!(is_current_connection(&session, guild_id, &fresh));
    }
}
