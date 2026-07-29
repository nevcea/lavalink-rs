//! Global periodic work.
//!
//! The original starts a scheduled executor *per session* plus a two-thread
//! `player-update` pool per session (`SocketContext.kt:80,101`). Nothing about the
//! work needs that: the schedule is the same for every session, so there are three
//! tasks here for the whole node regardless of how many clients connect.
//!
//! Claiming this is "fewer threads" would be guessing — JVM executors create core
//! threads lazily, so the original's count depends on load. The claim is structural:
//! there is no per-session scheduler to grow.

use std::sync::Arc;
use std::time::{Duration, Instant};

use lavalink_protocol::message::Message;
use lavalink_protocol::stats::StatsEvent;

use crate::player::Command;
use crate::state::AppState;

/// The original's stats interval (`SocketContext.kt:100`).
const STATS_INTERVAL: Duration = Duration::from_secs(60);

/// How often expired resume deadlines are collected. Fine-grained enough that a
/// session is not kept alive noticeably past its timeout.
const SWEEP_INTERVAL: Duration = Duration::from_secs(1);

/// Spawns the node's periodic tasks. They run until the process exits.
pub fn spawn(state: AppState) {
    tokio::spawn(player_update_tick(state.clone()));
    tokio::spawn(stats_tick(state.clone()));
    tokio::spawn(sweep_tick(state));
}

async fn player_update_tick(state: AppState) {
    let period = Duration::from_secs(state.config.lavalink.server.player_update_interval);
    let mut interval = tokio::time::interval(period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        interval.tick().await;
        for session in state.sessions.all() {
            // A paused session is dropping snapshots anyway; skip the work.
            if session.sink.is_paused() {
                continue;
            }
            for player in session.players() {
                // `try_send`: if an actor's queue is full it is busy with real work,
                // and a skipped update is replaced by the next tick.
                let _ = player.try_send(Command::EmitUpdate);
            }
        }
    }
}

async fn stats_tick(state: AppState) {
    let mut interval = tokio::time::interval(STATS_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        interval.tick().await;

        let sessions = state.sessions.all();
        let players = crate::stats::count_players(&sessions);
        let playing = crate::stats::count_playing(&sessions);

        // Sampled once for the whole node, then shared by every session.
        let node = state.stats.sample(players, playing);
        let now_ms = crate::player::now_epoch_ms();
        for session in sessions {
            // Per session, not per node: the original's `StatsCollector.retrieveStats`
            // takes a `SocketContext` and only aggregates that session's own players,
            // unlike `players`/`playingPlayers`/`cpu`/`memory` above, which are the
            // same for everyone. Every player's counters are drained here regardless
            // of usability — see `FrameCounters::take`'s docs — so a player that sits
            // just under the usability threshold does not have frames silently pile up
            // for next tick.
            let samples: Vec<_> = session
                .players()
                .iter()
                .map(|player| {
                    let (sent, nulled) = player.take_frame_stats();
                    let since = player.playing_since_ms();
                    let usable =
                        since != 0 && since <= now_ms - crate::stats::FRAME_STATS_USABLE_AFTER_MS;
                    (sent, nulled, usable)
                })
                .collect();
            let frame_stats = crate::stats::frame_stats(samples.into_iter());

            let _ = session.send(Message::Stats(StatsEvent::from_node(node, frame_stats)));
        }
    }
}

async fn sweep_tick(state: AppState) {
    let mut interval = tokio::time::interval(SWEEP_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        interval.tick().await;
        for session in state.sessions.sweep_expired(Instant::now()) {
            tracing::info!(session = %session.id, "resume window expired");
            shutdown_session(&session).await;
        }
    }
}

/// Tears a session down: every player destroyed, then the sink closed.
pub async fn shutdown_session(session: &Arc<crate::session::Session>) {
    for player in session.take_players() {
        let _ = player.destroy().await;
    }
    session.sink.close();
}
