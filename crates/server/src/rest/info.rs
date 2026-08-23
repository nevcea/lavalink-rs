//! /version, /v4/info, /v4/stats, and Prometheus metrics.

use std::collections::HashSet;
use std::fmt::Write as _;

use axum::extract::{Query, State};
use axum::http::header::CONTENT_TYPE;
use axum::response::IntoResponse;
use axum::Json;
use lavalink_protocol::stats::StatsData;

use crate::state::AppState;

/// Plain text, not JSON — the original returns the bare version string.
/// state.version_text is pre-built in AppState::new, so requests share its bytes.
/// The content-type matches what axum's String IntoResponse sets by default.
pub async fn version(State(state): State<AppState>) -> impl IntoResponse {
    ([(CONTENT_TYPE, "text/plain; charset=utf-8")], state.version_text)
}

/// Info never changes after startup, so this serves the bytes serialized
/// once in AppState::new instead of re-serializing (and deep-cloning
/// Info's Vec/String fields) on every request.
pub async fn info(State(state): State<AppState>) -> impl IntoResponse {
    ([(CONTENT_TYPE, "application/json")], state.info_json)
}

/// frameStats is always absent from this endpoint (docs/api/rest.md:989);
/// StatsData enforces that by construction.
pub async fn stats(State(state): State<AppState>) -> Json<StatsData> {
    let sessions = state.sessions.all();
    let (players, playing) = crate::stats::count_sessions(&sessions);
    Json(state.stats.sample_async(players, playing).await)
}

/// Prometheus text exposition for the portable Lavalink-specific gauges added
/// upstream in 4.1.0. JVM, GC, and logback collectors have no Rust equivalent.
pub async fn metrics(
    State(state): State<AppState>,
    Query(params): Query<Vec<(String, String)>>,
) -> impl IntoResponse {
    let included: HashSet<String> = params
        .into_iter()
        .filter_map(|(name, value)| (name == "name[]").then_some(value))
        .collect();
    let sessions = state.sessions.all();
    let (players, playing) = crate::stats::count_sessions(&sessions);
    let stats = state.stats.sample_async(players, playing).await;

    (
        [(CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        prometheus_text(stats, &included),
    )
}

fn prometheus_text(stats: StatsData, included: &HashSet<String>) -> String {
    let metrics = [
        (
            "lavalink_players_total",
            "Total number of players connected.",
            f64::from(stats.players),
        ),
        (
            "lavalink_playing_players_total",
            "Number of players currently playing audio.",
            f64::from(stats.playing_players),
        ),
        (
            "lavalink_uptime_milliseconds",
            "Uptime of the node in milliseconds.",
            stats.uptime as f64,
        ),
        (
            "lavalink_memory_free_bytes",
            "Memory statistics in bytes. (Free)",
            stats.memory.free as f64,
        ),
        (
            "lavalink_memory_used_bytes",
            "Memory statistics in bytes. (Used)",
            stats.memory.used as f64,
        ),
        (
            "lavalink_memory_allocated_bytes",
            "Memory statistics in bytes. (Allocated)",
            stats.memory.allocated as f64,
        ),
        (
            "lavalink_memory_reservable_bytes",
            "Memory statistics in bytes. (Reservable)",
            stats.memory.reservable as f64,
        ),
        (
            "lavalink_cpu_cores",
            "CPU statistics. (Cores)",
            f64::from(stats.cpu.cores),
        ),
        (
            "lavalink_cpu_system_load_percentage",
            "CPU statistics. (System Load)",
            stats.cpu.system_load,
        ),
        (
            "lavalink_cpu_lavalink_load_percentage",
            "CPU statistics. (LL Load)",
            stats.cpu.lavalink_load,
        ),
    ];

    let mut output = String::new();
    for (name, help, value) in metrics {
        if !included.is_empty() && !included.contains(name) {
            continue;
        }
        writeln!(output, "# HELP {name} {help}").expect("writing to a String cannot fail");
        writeln!(output, "# TYPE {name} gauge").expect("writing to a String cannot fail");
        writeln!(output, "{name} {value:?}").expect("writing to a String cannot fail");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use lavalink_protocol::stats::{Cpu, Memory};

    const STATS: StatsData = StatsData {
        frame_stats: None,
        players: 2,
        playing_players: 1,
        uptime: 123,
        memory: Memory {
            free: 10,
            used: 20,
            allocated: 30,
            reservable: 40,
        },
        cpu: Cpu {
            cores: 8,
            system_load: 0.5,
            lavalink_load: 0.25,
        },
    };

    #[test]
    fn prometheus_text_matches_the_upstream_lavalink_collector() {
        let body = prometheus_text(STATS, &HashSet::new());
        assert_eq!(
            body,
            "# HELP lavalink_players_total Total number of players connected.\n\
# TYPE lavalink_players_total gauge\n\
lavalink_players_total 2.0\n\
# HELP lavalink_playing_players_total Number of players currently playing audio.\n\
# TYPE lavalink_playing_players_total gauge\n\
lavalink_playing_players_total 1.0\n\
# HELP lavalink_uptime_milliseconds Uptime of the node in milliseconds.\n\
# TYPE lavalink_uptime_milliseconds gauge\n\
lavalink_uptime_milliseconds 123.0\n\
# HELP lavalink_memory_free_bytes Memory statistics in bytes. (Free)\n\
# TYPE lavalink_memory_free_bytes gauge\n\
lavalink_memory_free_bytes 10.0\n\
# HELP lavalink_memory_used_bytes Memory statistics in bytes. (Used)\n\
# TYPE lavalink_memory_used_bytes gauge\n\
lavalink_memory_used_bytes 20.0\n\
# HELP lavalink_memory_allocated_bytes Memory statistics in bytes. (Allocated)\n\
# TYPE lavalink_memory_allocated_bytes gauge\n\
lavalink_memory_allocated_bytes 30.0\n\
# HELP lavalink_memory_reservable_bytes Memory statistics in bytes. (Reservable)\n\
# TYPE lavalink_memory_reservable_bytes gauge\n\
lavalink_memory_reservable_bytes 40.0\n\
# HELP lavalink_cpu_cores CPU statistics. (Cores)\n\
# TYPE lavalink_cpu_cores gauge\n\
lavalink_cpu_cores 8.0\n\
# HELP lavalink_cpu_system_load_percentage CPU statistics. (System Load)\n\
# TYPE lavalink_cpu_system_load_percentage gauge\n\
lavalink_cpu_system_load_percentage 0.5\n\
# HELP lavalink_cpu_lavalink_load_percentage CPU statistics. (LL Load)\n\
# TYPE lavalink_cpu_lavalink_load_percentage gauge\n\
lavalink_cpu_lavalink_load_percentage 0.25\n"
        );
    }

    #[test]
    fn prometheus_name_filter_keeps_registry_order_and_drops_unknown_names() {
        let included = [
            "unknown".to_owned(),
            "lavalink_cpu_cores".to_owned(),
            "lavalink_players_total".to_owned(),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            prometheus_text(STATS, &included),
            "# HELP lavalink_players_total Total number of players connected.\n\
# TYPE lavalink_players_total gauge\n\
lavalink_players_total 2.0\n\
# HELP lavalink_cpu_cores CPU statistics. (Cores)\n\
# TYPE lavalink_cpu_cores gauge\n\
lavalink_cpu_cores 8.0\n"
        );

        assert!(prometheus_text(STATS, &["unknown".to_owned()].into_iter().collect()).is_empty());
    }
}
