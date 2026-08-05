//! Three-state field wrapper matching the original `protocol/.../omissible.kt`.
//!
//! The distinction that must survive the port:
//!
//! | state | JSON | meaning |
//! |---|---|---|
//! | [`Omissible::Omitted`] | key absent | leave untouched |
//! | `Present(None)` | `"field": null` | explicit clear (e.g. `encodedTrack: null` = stop) |
//! | `Present(Some(v))` | `"field": v` | set to `v` |
//!
//! `Option<Option<T>>` cannot express this under serde, because a missing field and
//! an explicit `null` both deserialize to the outer `None`. Hence the custom type:
//! `Deserialize` is only invoked when the key is present, so `#[serde(default)]`
//! supplies `Omitted` for absence and everything else lands in `Present`.

use serde::de::{Deserialize, Deserializer};
use serde::ser::{Error as _, Serialize, Serializer};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Omissible<T> {
    /// The default, and the reason `#[serde(default)]` on every field is enough to
    /// distinguish an absent key from an explicit null.
    #[default]
    Omitted,
    Present(T),
}

impl<T> Omissible<T> {
    pub fn is_omitted(&self) -> bool {
        matches!(self, Omissible::Omitted)
    }

    pub fn is_present(&self) -> bool {
        matches!(self, Omissible::Present(_))
    }

    /// The present value, or `None` when omitted. Mirrors Kotlin `ifPresent`.
    pub fn into_option(self) -> Option<T> {
        match self {
            Omissible::Omitted => None,
            Omissible::Present(v) => Some(v),
        }
    }

    /// Kotlin `takeIfPresent { predicate }`: present *and* the predicate holds.
    ///
    /// The original uses this to make `paused`/`position`/`endTime`/`userData` apply
    /// only when no new track is being set (`PlayerRestHandler.kt:145,151,161,169`).
    pub fn take_if(self, condition: bool) -> Option<T> {
        if condition {
            self.into_option()
        } else {
            None
        }
    }
}

impl<T: Serialize> Serialize for Omissible<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Omissible::Present(v) => v.serialize(serializer),
            // Unreachable in practice: every Omissible field carries
            // skip_serializing_if = "Omissible::is_omitted". Erroring rather than
            // emitting null keeps a missed attribute loud instead of silently
            // turning "untouched" into "explicit clear" on the wire.
            Omissible::Omitted => Err(S::Error::custom(
                "Omissible::Omitted cannot be serialized; the field needs \
                 #[serde(skip_serializing_if = \"Omissible::is_omitted\")]",
            )),
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Omissible<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        T::deserialize(deserializer).map(Omissible::Present)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct Probe {
        #[serde(default, skip_serializing_if = "Omissible::is_omitted")]
        field: Omissible<Option<String>>,
    }

    #[test]
    fn three_states_are_distinguishable() {
        let omitted: Probe = serde_json::from_str("{}").unwrap();
        assert_eq!(omitted.field, Omissible::Omitted);

        let null: Probe = serde_json::from_str(r#"{"field":null}"#).unwrap();
        assert_eq!(null.field, Omissible::Present(None));

        let set: Probe = serde_json::from_str(r#"{"field":"x"}"#).unwrap();
        assert_eq!(set.field, Omissible::Present(Some("x".into())));
    }

    #[test]
    fn omitted_round_trips_as_an_absent_key() {
        let probe = Probe {
            field: Omissible::Omitted,
        };
        assert_eq!(serde_json::to_string(&probe).unwrap(), "{}");
    }

    #[test]
    fn explicit_null_round_trips_as_null() {
        let probe = Probe {
            field: Omissible::Present(None),
        };
        assert_eq!(serde_json::to_string(&probe).unwrap(), r#"{"field":null}"#);
    }
}
