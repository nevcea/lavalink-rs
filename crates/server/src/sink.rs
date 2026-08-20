//! The outbound WebSocket queue and its backpressure policy.
//!
//! The original writes straight into the Undertow channel with no bound
//! (SocketContext.kt:164) and queues resume events in an unbounded
//! ConcurrentLinkedQueue (:74). Both are unbounded in the direction that hurts:
//! a client that stops reading grows the server's memory until something breaks.
//!
//! Here the queue is bounded, and messages are split into two lanes:
//!
//! • Essential — ready and every event. Mutual order preserved, and not dropped
//!   in the sense that matters for a connected client: ws.rs closes the session
//!   with 1008 once 2048 are outstanding, well under this lane's own 4096 cap,
//!   so a connected client that is actually draining never sees one lost. A
//!   resumable (detached) session has no WebSocket for anything to notice that
//!   on, so it only gets caught at the next per-second sweep tick
//!   (SessionRegistry::sweep_expired polling is_overflowing) — every producer
//!   here discards a SendError::Overflow (send's own Err case), so an essential
//!   really can be silently lost for up to that one tick if a detached
//!   session's queue crosses 4096 between sweeps.
//! • Snapshots — playerUpdate and stats. Coalesced by key: an unsent update
//!   for a guild is replaced by the newer one. Nothing is lost that the next message
//!   does not already carry.
//!
//! Essentials are drained before snapshots, so a burst of updates cannot delay an
//! event. The reverse ordering (a playerUpdate overtaking a TrackStart) is the
//! one clients would notice.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;

use lavalink_protocol::message::{CoalesceKey, Message};
use tokio::sync::Notify;

/// How many essential messages may be outstanding before we give up on the client.
///
/// Generous: a healthy client drains continuously, and the only way to reach this
/// is a client that has stopped reading entirely.
///
/// pub(crate) rather than private: ws.rs's overflow_closes needs the exact
/// value it shares with Sink::send, so a connected client that reaches actual
/// data loss (this cap, not just crate::ws's own lower OVERFLOW_THRESHOLD)
/// is closed unconditionally rather than only when the grace-period-after-resume
/// logic happens to have armed.
pub(crate) const ESSENTIAL_CAPACITY: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendError {
    /// The client is not consuming essential messages. Caller closes with 1008.
    Overflow,
    /// The sink was closed; the message is discarded.
    Closed,
}

#[derive(Debug, Default)]
struct Inner {
    essential: VecDeque<Message>,
    /// Keyed by Message::coalesce_key, rendered to an owned key so the map can
    /// outlive the borrow.
    snapshots: BTreeMap<SnapshotKey, Message>,
    /// Set while the session is Resumable: nothing is written, essentials
    /// accumulate for replay, snapshots are dropped entirely.
    paused: bool,
    closed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum SnapshotKey {
    Stats,
    PlayerUpdate(String),
}

impl SnapshotKey {
    fn of(message: &Message) -> Option<Self> {
        match message.coalesce_key()? {
            CoalesceKey::Stats => Some(SnapshotKey::Stats),
            CoalesceKey::PlayerUpdate(guild) => Some(SnapshotKey::PlayerUpdate(guild.to_owned())),
        }
    }
}

#[derive(Debug, Default)]
pub struct Sink {
    inner: Mutex<Inner>,
    notify: Notify,
}

impl Sink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn send(&self, message: Message) -> Result<(), SendError> {
        {
            let mut inner = self.lock();
            if inner.closed {
                return Err(SendError::Closed);
            }

            match SnapshotKey::of(&message) {
                Some(key) => {
                    // While paused, snapshots are not queued at all — a genuine
                    // divergence from the original, which queues every payload
                    // (playerUpdate/Stats included) in an unbounded
                    // resumeEventQueue and replays it verbatim on resume before
                    // sending one fresh update per player (SocketContext.kt:193).
                    // Queuing a growing backlog of stale positions is exactly the
                    // unbounded growth this module's docs reject for essentials,
                    // and a stale playerUpdate is strictly less useful than the
                    // fresh one that follows it — dropping it loses nothing a
                    // client can act on. What a client actually observes still
                    // matches: ws.rs::run sends that same fresh per-player update
                    // at resume, so the original's replay is redundant work we
                    // skip, not a wire-visible difference.
                    if inner.paused {
                        return Ok(());
                    }
                    inner.snapshots.insert(key, message);
                }
                None => {
                    if inner.essential.len() >= ESSENTIAL_CAPACITY {
                        return Err(SendError::Overflow);
                    }
                    inner.essential.push_back(message);
                }
            }
        }
        self.notify.notify_one();
        Ok(())
    }

    /// Sends an essential message ahead of anything already queued.
    ///
    /// For ready: it must be the first frame a (re)connecting client reads, even
    /// when a resumed session's essential lane already holds a replayed backlog —
    /// send would put it behind that backlog instead.
    pub fn send_first(&self, message: Message) -> Result<(), SendError> {
        debug_assert!(
            message.coalesce_key().is_none(),
            "send_first is for the essential lane only"
        );
        {
            let mut inner = self.lock();
            if inner.closed {
                return Err(SendError::Closed);
            }
            if inner.essential.len() >= ESSENTIAL_CAPACITY {
                return Err(SendError::Overflow);
            }
            inner.essential.push_front(message);
        }
        self.notify.notify_one();
        Ok(())
    }

    /// Takes the next message to write, or None if there is nothing pending or the
    /// sink is paused or closed.
    pub fn try_recv(&self) -> Option<Message> {
        let mut inner = self.lock();
        if inner.paused || inner.closed {
            return None;
        }
        if let Some(message) = inner.essential.pop_front() {
            return Some(message);
        }
        // pop_first, not keys().next().cloned() then remove: the latter cloned
        // the key String and searched the map twice to take the entry it had just
        // found. Same entry either way — both take the first in BTreeMap order.
        inner.snapshots.pop_first().map(|(_, message)| message)
    }

    /// Waits until Self::try_recv may return something. Cancellation-safe.
    pub async fn recv(&self) -> Option<Message> {
        loop {
            if let Some(message) = self.try_recv() {
                return Some(message);
            }
            if self.lock().closed {
                return None;
            }
            self.notify.notified().await;
        }
    }

    /// Enters the Resumable state: stop writing, keep essentials for replay.
    pub fn pause(&self) {
        let mut inner = self.lock();
        inner.paused = true;
        inner.snapshots.clear();
    }

    /// Leaves the Resumable state. Queued essentials are replayed in order by the
    /// writer task that picks up next.
    pub fn resume(&self) {
        {
            let mut inner = self.lock();
            inner.paused = false;
        }
        self.notify.notify_one();
    }

    pub fn close(&self) {
        {
            let mut inner = self.lock();
            inner.closed = true;
            inner.essential.clear();
            inner.snapshots.clear();
        }
        // Both calls, not just one: notify_waiters only reaches a Notified future
        // already registered (polled at least once), so a recv caller between its
        // closed check and its await point would never see this wakeup and hang
        // forever. notify_one stores a permit for the next await to consume
        // immediately, closing that gap — one writer task per sink, so one stored
        // permit covers it.
        self.notify.notify_waiters();
        self.notify.notify_one();
    }

    pub fn is_paused(&self) -> bool {
        self.lock().paused
    }

    /// Number of queued essential messages — used by tests and by the resume-queue
    /// bound.
    pub fn pending_essentials(&self) -> usize {
        self.lock().essential.len()
    }

    /// Whether the essential queue is at ESSENTIAL_CAPACITY — the same
    /// condition that makes send start returning SendError::Overflow.
    ///
    /// While a WebSocket is attached, an overflowing sink is noticed and the
    /// session is closed with 1008 (ws.rs's pump). A session in
    /// SessionState::Resumable has no WebSocket to notice it, so nobody would
    /// otherwise react — SessionRegistry::sweep_expired polls this instead, to
    /// give an overflowing resumable session the same fate a connected one gets,
    /// rather than silently dropping every essential message past the cap for
    /// the rest of the resume window.
    pub fn is_overflowing(&self) -> bool {
        self.lock().essential.len() >= ESSENTIAL_CAPACITY
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        crate::lock(&self.inner)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use lavalink_protocol::message::{EmittedEvent, Message};
    use crate::testing::track;
    use lavalink_protocol::player::PlayerState;

    fn update(guild: &str, position: i64) -> Message {
        Message::PlayerUpdate {
            state: PlayerState {
                time: 0,
                position,
                connected: true,
                ping: 1,
            },
            guild_id: guild.to_owned(),
        }
    }

    fn event(guild: &str) -> Message {
        Message::Event(EmittedEvent::TrackStart {
            guild_id: guild.to_owned(),
            track: Box::new(track("t")),
        })
    }

    #[test]
    fn snapshots_coalesce_per_guild() {
        let sink = Sink::new();
        sink.send(update("1", 100)).unwrap();
        sink.send(update("1", 200)).unwrap();
        sink.send(update("2", 300)).unwrap();

        let mut positions: Vec<i64> = Vec::new();
        while let Some(Message::PlayerUpdate { state, .. }) = sink.try_recv() {
            positions.push(state.position);
        }
        // The stale update for guild 1 is gone; guild 2 is untouched.
        positions.sort_unstable();
        assert_eq!(positions, vec![200, 300]);
    }

    /// The bug this guards: Message::Ready sent through plain send lands behind
    /// whatever essentials a resumed session already queued for replay, so a
    /// reconnecting client reads its backlog before sessionId/resumed.
    #[test]
    fn send_first_jumps_ahead_of_an_existing_backlog() {
        let sink = Sink::new();
        sink.send(event("1")).unwrap();
        sink.send(event("2")).unwrap();

        sink.send_first(event("ready")).unwrap();

        assert_eq!(sink.try_recv(), Some(event("ready")));
        assert_eq!(sink.try_recv(), Some(event("1")));
        assert_eq!(sink.try_recv(), Some(event("2")));
    }

    #[test]
    fn essentials_are_never_coalesced_and_keep_their_order() {
        let sink = Sink::new();
        sink.send(event("1")).unwrap();
        sink.send(event("2")).unwrap();

        let first = sink.try_recv().unwrap();
        let second = sink.try_recv().unwrap();
        assert_eq!(first, event("1"));
        assert_eq!(second, event("2"));
        assert_eq!(sink.try_recv(), None);
    }

    #[test]
    fn essentials_are_drained_before_snapshots() {
        let sink = Sink::new();
        sink.send(update("1", 1)).unwrap();
        sink.send(event("1")).unwrap();

        assert!(matches!(sink.try_recv(), Some(Message::Event(_))));
        assert!(matches!(sink.try_recv(), Some(Message::PlayerUpdate { .. })));
    }

    #[test]
    fn a_client_that_stops_reading_overflows_instead_of_growing() {
        let sink = Sink::new();
        for _ in 0..ESSENTIAL_CAPACITY {
            sink.send(event("1")).unwrap();
        }
        assert_eq!(sink.send(event("1")), Err(SendError::Overflow));
    }

    #[test]
    fn is_overflowing_reports_the_same_threshold_send_enforces() {
        let sink = Sink::new();
        assert!(!sink.is_overflowing());

        for _ in 0..ESSENTIAL_CAPACITY {
            sink.send(event("1")).unwrap();
        }
        assert!(sink.is_overflowing());
    }

    #[test]
    fn snapshots_never_overflow_however_many_arrive() {
        let sink = Sink::new();
        for position in 0..ESSENTIAL_CAPACITY as i64 * 4 {
            sink.send(update("1", position)).unwrap();
        }
        assert_eq!(sink.pending_essentials(), 0);
    }

    #[test]
    fn pausing_keeps_events_and_drops_snapshots() {
        let sink = Sink::new();
        sink.send(update("1", 1)).unwrap();
        sink.pause();

        sink.send(event("1")).unwrap();
        sink.send(update("1", 2)).unwrap();
        assert_eq!(sink.try_recv(), None, "nothing is written while paused");

        sink.resume();
        assert_eq!(sink.try_recv(), Some(event("1")));
        assert_eq!(sink.try_recv(), None, "the stale update was not replayed");
    }

    #[test]
    fn closing_discards_everything() {
        let sink = Sink::new();
        sink.send(event("1")).unwrap();
        sink.close();
        assert_eq!(sink.try_recv(), None);
        assert_eq!(sink.send(event("1")), Err(SendError::Closed));
    }

    /// close used to call only notify_waiters, which only reaches a Notified
    /// future that has already been polled — a recv caller between its closed
    /// check (reading false) and registering its await point would never be
    /// woken by that call, and nothing wakes it after (send returns Err(Closed)
    /// before its own notify_one). That caller hangs forever. close now also
    /// calls notify_one, which stores a permit regardless of whether anyone is
    /// waiting yet, closing the gap.
    ///
    /// The actual race window is a few instructions wide with no await point in
    /// it, so this soak does not reliably reproduce the old hang on its own —
    /// confirmed by temporarily dropping the notify_one call and rerunning this
    /// exact test, which still passed. What it does verify: recv concurrent with
    /// close, across many interleavings on a real multi-thread runtime, always
    /// returns rather than hanging within the timeout — a basic correctness
    /// property worth pinning even without a guaranteed repro of the narrow bug.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_recv_racing_close_is_not_a_permanent_hang() {
        for _ in 0..200 {
            let sink = Arc::new(Sink::new());
            let reader = tokio::spawn({
                let sink = Arc::clone(&sink);
                async move { sink.recv().await }
            });
            // Give recv a chance to reach try_recv/the closed check before close
            // runs, without pinning the interleaving down further.
            tokio::task::yield_now().await;
            sink.close();

            let result = tokio::time::timeout(std::time::Duration::from_secs(2), reader).await;
            assert!(result.is_ok(), "recv() did not return after close()");
            assert_eq!(result.unwrap().unwrap(), None);
        }
    }
}
