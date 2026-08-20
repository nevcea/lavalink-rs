//! Node statistics.
//!
//! Note the asymmetry in how frameStats is reported, which is load-bearing for
//! clients and was itself a breaking change once (docs/changelog/v3.md:65):
//!
//! • GET /v4/stats (StatsData) — the key is omitted entirely
//!   (docs/api/rest.md:989).
//! • WebSocket stats op (StatsEvent) — the key is always present, null
//!   when the session has no usable frame data.
//!
//! Two types rather than one flag, so the difference cannot be lost at a call site.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatsData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_stats: Option<FrameStats>,
    pub players: i32,
    pub playing_players: i32,
    pub uptime: i64,
    pub memory: Memory,
    pub cpu: Cpu,
}

/// The stats WebSocket payload. Identical to StatsData except that
/// frameStats is never dropped.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatsEvent {
    pub frame_stats: Option<FrameStats>,
    pub players: i32,
    pub playing_players: i32,
    pub uptime: i64,
    pub memory: Memory,
    pub cpu: Cpu,
}

impl StatsEvent {
    /// Attaches per-session frame stats to the node-wide snapshot.
    ///
    /// The node-wide half is computed once per tick and broadcast; only
    /// frame_stats differs per session.
    pub fn from_node(stats: StatsData, frame_stats: Option<FrameStats>) -> Self {
        Self {
            frame_stats,
            players: stats.players,
            playing_players: stats.playing_players,
            uptime: stats.uptime,
            memory: stats.memory,
            cpu: stats.cpu,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameStats {
    pub sent: i32,
    pub nulled: i32,
    /// Expected minus actual, so it goes negative when we over-deliver.
    pub deficit: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Memory {
    pub free: i64,
    pub used: i64,
    pub allocated: i64,
    pub reservable: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cpu {
    pub cores: i32,
    /// 0.0..=1.0
    pub system_load: f64,
    /// 0.0..=1.0
    pub lavalink_load: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    const NODE: StatsData = StatsData {
        frame_stats: None,
        players: 1,
        playing_players: 1,
        uptime: 123,
        memory: Memory {
            free: 1,
            used: 2,
            allocated: 3,
            reservable: 4,
        },
        cpu: Cpu {
            cores: 8,
            system_load: 0.5,
            lavalink_load: 0.25,
        },
    };

    #[test]
    fn rest_stats_omit_absent_frame_stats() {
        let json = serde_json::to_value(NODE).unwrap();
        assert!(
            json.get("frameStats").is_none(),
            "GET /v4/stats must drop the key, not send null: {json}"
        );
    }

    #[test]
    fn websocket_stats_keep_null_frame_stats() {
        let json = serde_json::to_value(StatsEvent::from_node(NODE, None)).unwrap();
        assert!(json["frameStats"].is_null());
    }
}
