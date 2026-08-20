use serde::{Deserialize, Serialize};

use crate::omissible::Omissible;

/// The REST error body.
///
/// Shaped like Spring Boot's default error response because that is what clients
/// parse — including error, the HTTP status reason phrase, and path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Error {
    /// Milliseconds since the epoch.
    pub timestamp: i64,
    pub status: u16,
    /// The status reason phrase, e.g. "Bad Request".
    pub error: String,
    /// Only populated when the request carried trace=true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<String>,
    pub message: String,
    pub path: String,
}

/// PATCH /v4/sessions/{id} response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub resuming: bool,
    /// Seconds. Named timeout on the wire despite the unit-suffixed field name in
    /// the original.
    #[serde(rename = "timeout")]
    pub timeout_seconds: i64,
}

/// PATCH /v4/sessions/{id} request body.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionUpdate {
    #[serde(default, skip_serializing_if = "Omissible::is_omitted")]
    pub resuming: Omissible<bool>,
    #[serde(
        rename = "timeout",
        default,
        skip_serializing_if = "Omissible::is_omitted"
    )]
    pub timeout_seconds: Omissible<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_is_dropped_when_absent() {
        let error = Error {
            timestamp: 1_667_857_581_613,
            status: 404,
            error: "Not Found".to_owned(),
            trace: None,
            message: "Session not found".into(),
            path: "/v4/sessions/xyz/players/123".into(),
        };

        let json = serde_json::to_value(&error).unwrap();
        assert!(json.get("trace").is_none());
        assert_eq!(json["error"], "Not Found");
        assert_eq!(json["status"], 404);
    }

    #[test]
    fn session_uses_the_short_timeout_key() {
        let session = Session {
            resuming: true,
            timeout_seconds: 60,
        };
        assert_eq!(
            serde_json::to_string(&session).unwrap(),
            r#"{"resuming":true,"timeout":60}"#
        );
    }

    #[test]
    fn partial_update_leaves_the_other_field_alone() {
        let update: SessionUpdate = serde_json::from_str(r#"{"resuming":true}"#).unwrap();
        assert_eq!(update.resuming, Omissible::Present(true));
        assert_eq!(update.timeout_seconds, Omissible::Omitted);
    }
}
