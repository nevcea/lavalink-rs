use serde::{Deserialize, Serialize};

/// The REST error body.
///
/// Shaped like Spring Boot's default error response because that is what clients
/// parse — including `error`, the HTTP status *reason phrase*, and `path`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Error {
    /// Milliseconds since the epoch.
    pub timestamp: i64,
    pub status: u16,
    /// The status reason phrase, e.g. `"Bad Request"`.
    pub error: String,
    /// Only populated when the request carried `trace=true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<String>,
    pub message: String,
    pub path: String,
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
}
