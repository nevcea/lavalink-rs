use serde::{Deserialize, Serialize};

use crate::omissible::Omissible;

/// `PATCH /v4/sessions/{id}` response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub resuming: bool,
    /// Seconds. Named `timeout` on the wire despite the unit-suffixed field name in
    /// the original.
    #[serde(rename = "timeout")]
    pub timeout_seconds: i64,
}

/// `PATCH /v4/sessions/{id}` request body.
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
