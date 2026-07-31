//! Node statistics.
//!
//! The original builds a `StatsCollector` task per session and each one samples the
//! CPU independently (`SocketContext.kt:99-100`, `StatsCollector.kt`). The node-wide
//! numbers are the same for everyone, so this computes them once per tick and every
//! session gets the same snapshot; only `frameStats` is per-session.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use lavalink_protocol::stats::{Cpu, FrameStats, Memory, StatsData};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use crate::player::PlayerHandle;
use crate::session::Session;

/// Frames expected in one stats tick from a continuously-playing player: 50 fps
/// (one `FRAME_SAMPLES` buffer is 20ms) times a 60s tick. Named for lavaplayer's
/// own `AudioLossCounter.EXPECTED_PACKET_COUNT_PER_MIN`, which this equals because
/// our tick interval (`ticker::STATS_INTERVAL`) is also 60s.
const EXPECTED_FRAMES_PER_TICK: u64 = 3_000;

/// How long a player must have been playing continuously before its frame counts
/// are trusted for the aggregate — otherwise a track that started seconds before
/// this tick reports a deficit that looks like near-total loss.
///
/// The original's `AudioLossCounter.isDataUsable` is more elaborate (a rolling
/// per-minute window, plus a 100ms grace period across a track switch); this is
/// the coarser equivalent that follows from tracking one `playing_since`
/// timestamp per player rather than a minute-bucketed history. Both answer the
/// same question: has this player been producing frames for the whole window the
/// stats claim to cover?
pub const FRAME_STATS_USABLE_AFTER_MS: i64 = 60_000;

/// Averages `frameStats` over every "usable" player, the same way
/// `StatsCollector.retrieveStats` does: usable players' sent/nulled counts are
/// summed and divided by the usable player count, and `deficit` is how many of
/// the expected frames per player never arrived, also averaged. Unusable players
/// are excluded from the average entirely, but the caller must still have drained
/// their counters (see [`crate::audio::ring::FrameCounters::take`]).
///
/// `None` when there are no usable players — the original does not divide by
/// zero either, and a session with nothing playing has no frame data to report.
pub fn frame_stats(samples: impl Iterator<Item = (u32, u32, bool)>) -> Option<FrameStats> {
    let (mut sent, mut nulled, mut usable_players) = (0u64, 0u64, 0u64);
    for (player_sent, player_nulled, usable) in samples {
        if !usable {
            continue;
        }
        sent += u64::from(player_sent);
        nulled += u64::from(player_nulled);
        usable_players += 1;
    }

    if usable_players == 0 {
        return None;
    }

    let expected = usable_players * EXPECTED_FRAMES_PER_TICK;
    let deficit = expected as i64 - (sent + nulled) as i64;

    Some(FrameStats {
        sent: (sent / usable_players) as i32,
        nulled: (nulled / usable_players) as i32,
        deficit: (deficit / usable_players as i64) as i32,
    })
}

pub struct StatsCollector {
    started_at: Instant,
    system: Mutex<System>,
    pid: Pid,
    cores: i32,
}

impl StatsCollector {
    pub fn new(started_at: Instant) -> Self {
        let mut system = System::new();
        system.refresh_memory();
        let cores = system.physical_core_count().unwrap_or(1) as i32;

        Self {
            started_at,
            system: Mutex::new(system),
            pid: Pid::from_u32(std::process::id()),
            cores,
        }
    }

    /// Samples the node. Called once per stats tick, not once per session.
    pub fn sample(&self, players: i32, playing_players: i32) -> StatsData {
        let mut system = crate::lock(&self.system);
        system.refresh_cpu_usage();
        system.refresh_memory();
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[self.pid]),
            // Do not prune processes that vanished: we only ever ask about our own.
            false,
            ProcessRefreshKind::new().with_cpu().with_memory(),
        );

        let process = system.process(self.pid);

        // The original reports JVM heap numbers. There is no heap here, so these are
        // the process's own memory: `used` is RSS, and `allocated`/`reservable` are
        // the machine's total, which is the honest analogue of "how much more could
        // this process take". Clients display these; none of them branch on the
        // relationship between the four.
        let total = system.total_memory() as i64;
        let used = process.map(|process| process.memory() as i64).unwrap_or(0);
        let memory = Memory {
            free: (total - used).max(0),
            used,
            allocated: total,
            reservable: total,
        };

        let cpu = Cpu {
            cores: self.cores,
            system_load: (system.global_cpu_usage() as f64 / 100.0).clamp(0.0, 1.0),
            lavalink_load: process
                .map(|process| {
                    // sysinfo reports process CPU as a percentage of one core;
                    // Lavalink's `lavalinkLoad` is a fraction of the whole machine.
                    (process.cpu_usage() as f64 / 100.0 / self.cores.max(1) as f64).clamp(0.0, 1.0)
                })
                .unwrap_or(0.0),
        };

        StatsData {
            // Always None here: `GET /v4/stats` omits the key entirely, and the
            // websocket event attaches the per-session value itself.
            frame_stats: None,
            players,
            playing_players,
            uptime: self.started_at.elapsed().as_millis() as i64,
            memory,
            cpu,
        }
    }
}

/// Every session's player roster, in the same order as `sessions`.
///
/// Taking a roster is not free — [`Session::players`] locks that session's guild map
/// and clones a handle per player — so callers that need more than one number out of
/// the same set of players collect once and pass the result around. The stats tick
/// needs three (`players`, `playingPlayers`, and each session's frame samples) and
/// used to walk for each of them.
pub fn rosters(sessions: &[Arc<Session>]) -> Vec<Vec<PlayerHandle>> {
    sessions.iter().map(|session| session.players()).collect()
}

/// Total players, and of those the ones actually playing.
///
/// `playingPlayers` is the original's `context.playingPlayers.size`, which filters on
/// `player.isPlaying` with no minute-window gate (unlike `frameStats`' usability
/// check). Both counts live here, and both callers ([`crate::ticker`]'s stats tick and
/// `GET /v4/stats`) go through it, so the two cannot disagree about what the node is
/// running.
pub fn count(rosters: &[Vec<PlayerHandle>]) -> (i32, i32) {
    let players = rosters.iter().map(Vec::len).sum::<usize>() as i32;
    let playing = rosters
        .iter()
        .flatten()
        .filter(|player| player.playing_since_ms() != 0)
        .count() as i32;
    (players, playing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionRegistry;

    #[test]
    fn counting_no_sessions_reports_no_players() {
        assert_eq!(count(&[]), (0, 0));
    }

    #[test]
    fn player_counts_are_summed_across_sessions() {
        let registry = SessionRegistry::new();
        let sessions = vec![registry.open(1, None), registry.open(2, None)];

        // A session with no players still counts as a session, not as a player.
        assert_eq!(count(&rosters(&sessions)), (0, 0));
    }

    #[test]
    fn no_usable_players_reports_no_frame_stats() {
        assert!(frame_stats(std::iter::empty()).is_none());
        assert!(frame_stats(std::iter::once((3_000, 0, false))).is_none());
    }

    #[test]
    fn frame_stats_are_averaged_not_summed_across_usable_players() {
        // Two fully healthy players: each sent every expected frame.
        let stats = frame_stats(
            vec![(3_000, 0, true), (3_000, 0, true)].into_iter(),
        )
        .unwrap();

        assert_eq!(stats.sent, 3_000, "averaged, not the sum of both players");
        assert_eq!(stats.nulled, 0);
        assert_eq!(stats.deficit, 0);
    }

    #[test]
    fn unusable_players_are_excluded_from_the_average_but_do_not_prevent_one() {
        // One healthy player and one that just started (unusable, and its counts
        // would look like catastrophic loss if it were included).
        let stats = frame_stats(
            vec![(3_000, 0, true), (10, 0, false)].into_iter(),
        )
        .unwrap();

        assert_eq!(stats.sent, 3_000);
        assert_eq!(stats.deficit, 0);
    }

    #[test]
    fn a_starved_player_reports_a_positive_deficit() {
        // Half the expected frames arrived; the other half were neither sent nor
        // nulled at all (a stalled pump, not just silence).
        let stats = frame_stats(std::iter::once((1_500, 0, true))).unwrap();
        assert_eq!(stats.deficit, 1_500);
    }

    #[test]
    fn a_player_that_never_produces_silence_frames_can_report_a_negative_deficit() {
        // More frames arrived than the nominal per-tick expectation — the ring
        // does not clamp to it, so neither does the aggregate.
        let stats = frame_stats(std::iter::once((4_000, 0, true))).unwrap();
        assert!(stats.deficit < 0, "deficit was {}", stats.deficit);
    }

    #[test]
    fn uptime_is_measured_from_the_start_instant() {
        let collector = StatsCollector::new(Instant::now());
        let stats = collector.sample(3, 1);

        assert_eq!(stats.players, 3);
        assert_eq!(stats.playing_players, 1);
        assert!(stats.uptime >= 0);
        // `GET /v4/stats` drops the key entirely rather than sending null.
        assert!(stats.frame_stats.is_none());
    }
}
