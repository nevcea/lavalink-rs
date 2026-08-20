//! GET /v4/loadtracks result.
//!
//! Modeled by hand rather than with #[serde(tag, content)] because the original
//! always emits the data key — including "data": null for empty
//! (docs/api/rest.md:200-204, LoadResultSerializerTest.kt:97-102). A content
//! tagged enum would drop the key for the unit variant, changing the wire shape.

// The derive macros and the traits share these names; importing from the crate root
// brings both, which the manual impls below and the derives above each need.
use serde::de::Deserializer;
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::player::{JsonObject, Track};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResultStatus {
    Track,
    Playlist,
    Search,
    /// Serialized as "empty", not "none".
    #[serde(rename = "empty")]
    None,
    Error,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LoadResult {
    Track(Box<Track>),
    Playlist(Playlist),
    Search(Vec<Track>),
    /// No matches. Also what an unsupported source URL yields — HTTP 200 with
    /// loadType: "empty" rather than an error.
    Empty,
    /// Loading failed. Still returned with HTTP 200.
    Error(Exception),
}

impl LoadResult {
    pub fn load_type(&self) -> ResultStatus {
        match self {
            LoadResult::Track(_) => ResultStatus::Track,
            LoadResult::Playlist(_) => ResultStatus::Playlist,
            LoadResult::Search(_) => ResultStatus::Search,
            LoadResult::Empty => ResultStatus::None,
            LoadResult::Error(_) => ResultStatus::Error,
        }
    }
}

impl Serialize for LoadResult {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut s = serializer.serialize_struct("LoadResult", 2)?;
        s.serialize_field("loadType", &self.load_type())?;
        match self {
            LoadResult::Track(track) => s.serialize_field("data", track)?,
            LoadResult::Playlist(playlist) => s.serialize_field("data", playlist)?,
            LoadResult::Search(tracks) => s.serialize_field("data", tracks)?,
            LoadResult::Empty => s.serialize_field("data", &Value::Null)?,
            LoadResult::Error(exception) => s.serialize_field("data", exception)?,
        }
        s.end()
    }
}

impl<'de> Deserialize<'de> for LoadResult {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Helper {
            load_type: ResultStatus,
            #[serde(default)]
            data: Value,
        }

        // A generic function rather than a closure: each arm below needs a different
        // return type, and a closure would be monomorphized to whichever one the
        // compiler saw first.
        fn from<T, E>(value: Value) -> Result<T, E>
        where
            T: serde::de::DeserializeOwned,
            E: serde::de::Error,
        {
            serde_json::from_value(value).map_err(E::custom)
        }

        let helper = Helper::deserialize(deserializer)?;
        let data = helper.data;

        Ok(match helper.load_type {
            ResultStatus::Track => LoadResult::Track(Box::new(from(data)?)),
            ResultStatus::Playlist => LoadResult::Playlist(from(data)?),
            ResultStatus::Search => LoadResult::Search(from(data)?),
            ResultStatus::None => LoadResult::Empty,
            ResultStatus::Error => LoadResult::Error(from(data)?),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Playlist {
    pub info: PlaylistInfo,
    #[serde(default)]
    pub plugin_info: JsonObject,
    pub tracks: Vec<Track>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistInfo {
    pub name: String,
    /// -1 when the playlist has no selected track.
    pub selected_track: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Exception {
    pub message: Option<String>,
    pub severity: Severity,
    pub cause: String,
    /// Present only when the server was built with stack traces; the original always
    /// emits it, so we do too (empty string when we have nothing to say).
    #[serde(default)]
    pub cause_stack_trace: String,
}

impl Exception {
    /// We never have a stack trace to give — there is no JVM here — so the field is
    /// always the empty string rather than an invented one.
    pub fn new(
        severity: Severity,
        message: impl Into<String>,
        cause: impl Into<String>,
    ) -> Self {
        Self {
            message: Some(message.into()),
            severity,
            cause: cause.into(),
            cause_stack_trace: String::new(),
        }
    }

    /// The everyday case: something outside us went wrong and we know what.
    pub fn common(message: impl Into<String>, cause: impl Into<String>) -> Self {
        Self::new(Severity::Common, message, cause)
    }

    /// Ours to answer for: decoder blew up, encoder failed, pump panicked.
    pub fn fault(message: impl Into<String>, cause: impl Into<String>) -> Self {
        Self::new(Severity::Fault, message, cause)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Common,
    Suspicious,
    Fault,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_keeps_the_null_data_key() {
        let json = serde_json::to_string(&LoadResult::Empty).unwrap();
        assert_eq!(json, r#"{"loadType":"empty","data":null}"#);

        let parsed: LoadResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, LoadResult::Empty);
    }

    #[test]
    fn search_data_is_a_bare_array() {
        let json = serde_json::to_string(&LoadResult::Search(vec![])).unwrap();
        assert_eq!(json, r#"{"loadType":"search","data":[]}"#);
    }

    #[test]
    fn error_round_trips() {
        let json = r#"{
          "loadType": "error",
          "data": {
            "message": "The uploader has not made this video available in your country.",
            "severity": "common",
            "cause": "FriendlyException: This video is not available in your country.",
            "causeStackTrace": "FriendlyException: ...\n\nblabla"
          }
        }"#;

        let parsed: LoadResult = serde_json::from_str(json).unwrap();
        let LoadResult::Error(exception) = &parsed else {
            panic!("expected an error result, got {parsed:?}");
        };
        assert_eq!(exception.severity, Severity::Common);

        let round_tripped: LoadResult =
            serde_json::from_str(&serde_json::to_string(&parsed).unwrap()).unwrap();
        assert_eq!(round_tripped, parsed);
    }

    #[test]
    fn playlist_round_trips() {
        let json = r#"{
          "loadType": "playlist",
          "data": {
            "info": { "name": "Example YouTube Playlist", "selectedTrack": 3 },
            "pluginInfo": {},
            "tracks": []
          }
        }"#;

        let parsed: LoadResult = serde_json::from_str(json).unwrap();
        let LoadResult::Playlist(playlist) = &parsed else {
            panic!("expected a playlist result, got {parsed:?}");
        };
        assert_eq!(playlist.info.selected_track, 3);
        assert!(playlist.tracks.is_empty());
    }
}
