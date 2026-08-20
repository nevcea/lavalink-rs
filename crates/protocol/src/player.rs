use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::filters::Filters;
use crate::omissible::Omissible;

/// A JSON object field that is always emitted, defaulting to {}.
///
/// The original marks pluginInfo/userData @EncodeDefault, so they appear even
/// when empty. Clients read them unconditionally.
pub type JsonObject = Map<String, Value>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Player {
    pub guild_id: String,
    pub track: Option<Track>,
    pub volume: i32,
    pub paused: bool,
    pub state: PlayerState,
    pub voice: VoiceState,
    pub filters: Filters,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Track {
    pub encoded: String,
    pub info: TrackInfo,
    #[serde(default)]
    pub plugin_info: JsonObject,
    #[serde(default)]
    pub user_data: JsonObject,
}

impl Track {
    /// Builds a track carrying the empty pluginInfo our sources always report: we
    /// ship no plugins, so it is {} exactly like the original's built-in sources.
    pub fn new(encoded: String, info: TrackInfo) -> Self {
        Self {
            encoded,
            info,
            plugin_info: JsonObject::new(),
            user_data: JsonObject::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackInfo {
    pub identifier: String,
    pub is_seekable: bool,
    pub author: String,
    pub length: i64,
    pub is_stream: bool,
    pub position: i64,
    pub title: String,
    /// Nullable, never absent.
    pub uri: Option<String>,
    /// "http" / "local" / "youtube" — clients branch on this string.
    pub source_name: String,
    pub artwork_url: Option<String>,
    pub isrc: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceState {
    pub token: String,
    pub endpoint: String,
    pub session_id: String,
    pub channel_id: Option<String>,
}

impl Default for VoiceState {
    /// What the original reports for a player with no voice server info yet:
    /// empty strings, null channel (util.kt:106-111).
    fn default() -> Self {
        Self {
            token: String::new(),
            endpoint: String::new(),
            session_id: String::new(),
            channel_id: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerState {
    /// Unix epoch milliseconds.
    pub time: i64,
    /// Milliseconds into the track — not the same clock as time.
    pub position: i64,
    pub connected: bool,
    /// -1 when there is no voice connection.
    pub ping: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerUpdateTrack {
    #[serde(default, skip_serializing_if = "Omissible::is_omitted")]
    pub encoded: Omissible<Option<String>>,
    #[serde(default, skip_serializing_if = "Omissible::is_omitted")]
    pub identifier: Omissible<String>,
    #[serde(default, skip_serializing_if = "Omissible::is_omitted")]
    pub user_data: Omissible<JsonObject>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerUpdate {
    /// Deprecated in v4 in favour of PlayerUpdateTrack::encoded, still accepted.
    #[serde(default, skip_serializing_if = "Omissible::is_omitted")]
    pub encoded_track: Omissible<Option<String>>,
    /// Deprecated in v4 in favour of PlayerUpdateTrack::identifier.
    #[serde(default, skip_serializing_if = "Omissible::is_omitted")]
    pub identifier: Omissible<String>,
    #[serde(default, skip_serializing_if = "Omissible::is_omitted")]
    pub track: Omissible<PlayerUpdateTrack>,
    #[serde(default, skip_serializing_if = "Omissible::is_omitted")]
    pub position: Omissible<i64>,
    #[serde(default, skip_serializing_if = "Omissible::is_omitted")]
    pub end_time: Omissible<Option<i64>>,
    #[serde(default, skip_serializing_if = "Omissible::is_omitted")]
    pub volume: Omissible<i32>,
    #[serde(default, skip_serializing_if = "Omissible::is_omitted")]
    pub paused: Omissible<bool>,
    #[serde(default, skip_serializing_if = "Omissible::is_omitted")]
    pub filters: Omissible<Filters>,
    #[serde(default, skip_serializing_if = "Omissible::is_omitted")]
    pub voice: Omissible<VoiceState>,
}

/// GET /v4/sessions/{id}/players returns a bare array, not an object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Players(pub Vec<Player>);

/// POST /v4/decodetracks request body: a bare array of encoded strings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EncodedTracks(pub Vec<String>);

/// POST /v4/decodetracks response body: a bare array.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Tracks(pub Vec<Track>);

#[cfg(test)]
mod tests {
    use super::*;

    const TRACK_JSON: &str = r#"{
      "encoded": "QAAAjQIAJVJpY2sgQXN0bGV5IC0gTmV2ZXIgR29ubmEgR2l2ZSBZb3UgVXAADlJpY2tBc3RsZXlWRVZPAAAAAAADPCAAC2RRdzR3OVdnWGNRAAEAK2h0dHBzOi8vd3d3LnlvdXR1YmUuY29tL3dhdGNoP3Y9ZFF3NHc5V2dYY1EAB3lvdXR1YmUAAAAAAAAAAA==",
      "info": {
        "identifier": "dQw4w9WgXcQ",
        "isSeekable": true,
        "author": "RickAstleyVEVO",
        "length": 212000,
        "isStream": false,
        "position": 0,
        "title": "Rick Astley - Never Gonna Give You Up",
        "uri": "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
        "sourceName": "youtube",
        "artworkUrl": null,
        "isrc": null
      },
      "pluginInfo": {},
      "userData": {}
    }"#;

    #[test]
    fn track_round_trips() {
        let track: Track = serde_json::from_str(TRACK_JSON).unwrap();
        assert_eq!(track.info.identifier, "dQw4w9WgXcQ");
        assert_eq!(track.info.length, 212_000);
        assert_eq!(track.info.artwork_url, None);
        assert!(track.plugin_info.is_empty());

        let value: serde_json::Value = serde_json::to_value(&track).unwrap();
        // Nullable fields are emitted as null, never dropped.
        assert!(value["info"]["isrc"].is_null());
        assert_eq!(value["pluginInfo"], serde_json::json!({}));
        assert_eq!(value["userData"], serde_json::json!({}));
    }

    #[test]
    fn empty_update_leaves_everything_omitted() {
        let update: PlayerUpdate = serde_json::from_str("{}").unwrap();
        assert_eq!(update, PlayerUpdate::default());
        assert_eq!(serde_json::to_string(&update).unwrap(), "{}");
    }

    #[test]
    fn update_distinguishes_null_track_from_absent_track() {
        let clear: PlayerUpdate = serde_json::from_str(r#"{"track":{"encoded":null}}"#).unwrap();
        let encoded = match clear.track {
            Omissible::Present(t) => t.encoded,
            Omissible::Omitted => panic!("track should be present"),
        };
        // Present(None) is the "stop" signal the original spells ?: player.stop().
        assert_eq!(encoded, Omissible::Present(None));

        let untouched: PlayerUpdate = serde_json::from_str(r#"{"track":{}}"#).unwrap();
        let encoded = match untouched.track {
            Omissible::Present(t) => t.encoded,
            Omissible::Omitted => panic!("track should be present"),
        };
        assert_eq!(encoded, Omissible::Omitted);
    }
}
