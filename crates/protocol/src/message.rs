//! Outbound websocket messages.
//!
//! Two layers of internal tagging, matching the original's two
//! `JsonContentPolymorphicSerializer`s: `op` selects the message, and for
//! `op: "event"` the `type` field selects the event. Both tags sit at the top level
//! of the same object — an event is `{"op":"event","type":"TrackStartEvent",...}`,
//! not a nested payload.

use serde::{Deserialize, Serialize};

use crate::load_result::Exception;
use crate::player::{PlayerState, Track};
use crate::stats::StatsEvent;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op")]
pub enum Message {
    #[serde(rename = "ready")]
    #[serde(rename_all = "camelCase")]
    Ready { resumed: bool, session_id: String },

    #[serde(rename = "playerUpdate")]
    #[serde(rename_all = "camelCase")]
    PlayerUpdate { state: PlayerState, guild_id: String },

    #[serde(rename = "stats")]
    Stats(StatsEvent),

    #[serde(rename = "event")]
    Event(EmittedEvent),
}

impl Message {
    /// Coalescing key for discardable messages: an unsent `playerUpdate` for a guild
    /// is replaced by a newer one for the same guild. `playerUpdate` and `stats` are
    /// snapshots — a newer one supersedes an older one, so coalescing loses nothing.
    /// Events (`None` here) are history: dropping one would desynchronise the
    /// client's queue, so a client too slow to keep up with events gets closed
    /// instead of having one skipped.
    pub fn coalesce_key(&self) -> Option<CoalesceKey<'_>> {
        match self {
            Message::PlayerUpdate { guild_id, .. } => Some(CoalesceKey::PlayerUpdate(guild_id)),
            Message::Stats(_) => Some(CoalesceKey::Stats),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CoalesceKey<'a> {
    PlayerUpdate(&'a str),
    Stats,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum EmittedEvent {
    #[serde(rename = "TrackStartEvent")]
    #[serde(rename_all = "camelCase")]
    TrackStart { guild_id: String, track: Box<Track> },

    #[serde(rename = "TrackEndEvent")]
    #[serde(rename_all = "camelCase")]
    TrackEnd {
        guild_id: String,
        track: Box<Track>,
        reason: TrackEndReason,
    },

    #[serde(rename = "TrackExceptionEvent")]
    #[serde(rename_all = "camelCase")]
    TrackException {
        guild_id: String,
        track: Box<Track>,
        exception: Exception,
    },

    #[serde(rename = "TrackStuckEvent")]
    #[serde(rename_all = "camelCase")]
    TrackStuck {
        guild_id: String,
        track: Box<Track>,
        threshold_ms: i64,
    },

    #[serde(rename = "WebSocketClosedEvent")]
    #[serde(rename_all = "camelCase")]
    WebSocketClosed {
        guild_id: String,
        code: i32,
        reason: String,
        by_remote: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TrackEndReason {
    /// The track ran out, or ended because of an exception.
    Finished,
    /// The track never produced audio.
    LoadFailed,
    Stopped,
    Replaced,
    Cleanup,
}

impl TrackEndReason {
    /// Whether the client should start the next track on receiving this.
    ///
    /// Clients branch on this to drive their queue, so the mapping is copied from
    /// the original enum's `mayStartNext` rather than re-reasoned.
    pub fn may_start_next(self) -> bool {
        matches!(self, TrackEndReason::Finished | TrackEndReason::LoadFailed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::TrackInfo;

    fn track() -> Box<Track> {
        Box::new(Track::new(
            "encoded".into(),
            TrackInfo {
                identifier: "id".into(),
                is_seekable: true,
                author: "author".into(),
                length: 1,
                is_stream: false,
                position: 0,
                title: "title".into(),
                uri: None,
                source_name: "http".into(),
                artwork_url: None,
                isrc: None,
            },
        ))
    }

    #[test]
    fn ready_shape() {
        let message = Message::Ready {
            resumed: false,
            session_id: "abc".into(),
        };
        assert_eq!(
            serde_json::to_string(&message).unwrap(),
            r#"{"op":"ready","resumed":false,"sessionId":"abc"}"#
        );
    }

    #[test]
    fn events_carry_both_tags_at_the_top_level() {
        let message = Message::Event(EmittedEvent::TrackEnd {
            guild_id: "123".into(),
            track: track(),
            reason: TrackEndReason::LoadFailed,
        });

        let json = serde_json::to_value(&message).unwrap();
        assert_eq!(json["op"], "event");
        assert_eq!(json["type"], "TrackEndEvent");
        assert_eq!(json["guildId"], "123");
        assert_eq!(json["reason"], "loadFailed");

        let parsed: Message = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, message);
    }

    #[test]
    fn websocket_closed_round_trips() {
        let message = Message::Event(EmittedEvent::WebSocketClosed {
            guild_id: "123".into(),
            code: 4006,
            reason: "Session is no longer valid.".into(),
            by_remote: true,
        });
        let json = serde_json::to_string(&message).unwrap();
        assert_eq!(serde_json::from_str::<Message>(&json).unwrap(), message);
    }

    #[test]
    fn only_snapshots_have_a_coalesce_key() {
        let update = Message::PlayerUpdate {
            state: PlayerState {
                time: 1,
                position: 2,
                connected: true,
                ping: 3,
            },
            guild_id: "123".into(),
        };
        assert_eq!(update.coalesce_key(), Some(CoalesceKey::PlayerUpdate("123")));

        let event = Message::Event(EmittedEvent::TrackStart {
            guild_id: "123".into(),
            track: track(),
        });
        assert_eq!(event.coalesce_key(), None);
    }
}
