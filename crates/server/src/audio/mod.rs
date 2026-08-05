//! The audio pipeline.
//!
//! ```text
//! [pump: CPU-bound, no deadline]                [send: O(1), 20ms deadline]
//! source → decode → resample → filter ──▶ ring ──▶ mixer pulls, encodes Opus
//! ```
//!
//! # Why filtering costs us seeking
//!
//! Running our own DSP means giving the voice layer raw PCM, which it then treats as
//! a live stream and cannot seek; using its own seekable input would leave nowhere to
//! put a filter. Filters won, so seeking is reimplemented in [`pump`]: it seeks the
//! demuxer, discards the buffered audio and rebases the position counter. Precision
//! therefore follows the container — exact where there is an index, approximate where
//! the duration was guessed, refused on a live stream.
//!
//! Two properties of the send side are constraints, not decisions:
//!
//! * **The mixer pulls.** It reads a `MediaSource` on its own clock; there is no API
//!   for handing it finished frames. So the ring's read end is what we expose.
//! * **The mixer encodes.** Opus passthrough needs track volume to be exactly 1.0,
//!   and `volume` is a filter we must support, so passthrough is off. There is no
//!   `encode.rs` here because encoding is not ours.
//!
//! And the position counter is advanced by the **consuming** side, inside the ring's
//! read — the pump runs a whole buffer ahead, so its output is not a playback
//! position.
//!
//! # What is untested here
//!
//! Seek precision on real containers, and the thread and CPU cost of several players
//! at once, both need a live Discord voice channel to measure. The pieces that can be
//! tested without one — the ring's isolation and accounting, the resampler, the
//! filters, the failure model — are, in their own modules.

pub mod engine;
pub mod filter;
pub mod pump;
pub mod resample;
pub mod ring;
pub mod source;
pub mod stream;

use std::sync::atomic::AtomicI64;
use std::sync::Arc;

use lavalink_protocol::filters::Filters;
use lavalink_protocol::player::Track;
use lavalink_protocol::Exception;
use tokio::sync::mpsc;

/// One track's worth of instructions for the pipeline.
#[derive(Debug, Clone)]
pub struct PlayRequest {
    pub track: Track,
    pub start_position_ms: i64,
    pub end_time_ms: Option<i64>,
    pub paused: bool,
    /// The player's `volume` field (0..=1000), not the `volume` filter.
    pub volume: i32,
    pub filters: Filters,
}

/// What the pipeline tells the actor. Deliberately small: the actor owns player
/// state, so the engine reports facts about audio, not state transitions.
#[derive(Debug, Clone)]
pub enum EngineEvent {
    /// Audio was produced. Resets the stuck timer.
    Progress,
    /// The track reached its end, or its configured `endTime`.
    Finished,
    /// The track failed. `started` distinguishes "never produced audio"
    /// (`loadFailed`) from "died partway through" (`finished`).
    Failed { exception: Exception, started: bool },
}

/// The pipeline, as the actor sees it.
///
/// Every method returns immediately — the actor must not block, so anything slow is
/// the implementation's job to defer. Results come back as [`EngineEvent`]s.
pub trait Engine: Send + Sync + 'static {
    /// The shared playback position in milliseconds.
    ///
    /// Written by the consuming side of the pipeline and read by the actor and the
    /// global tick without a lock. Single writer, so no coordination is needed.
    fn position_handle(&self) -> Arc<AtomicI64>;

    /// Frames sent/nulled, for `/v4/stats`' `frameStats`. Defaulted to an
    /// unshared, always-empty counter so [`testing::RecordingEngine`] does not
    /// need one.
    fn frame_counters(&self) -> Arc<ring::FrameCounters> {
        Arc::default()
    }

    /// Hands the engine a channel for reporting back. Called once, at construction.
    ///
    /// Its own channel, not the actor's general command queue: a terminal event
    /// that loses its slot to a burst of REST traffic is a player wedged forever
    /// — see `PlayerHandle::engine_events`.
    fn attach(&self, _events: mpsc::Sender<EngineEvent>) {}

    fn play(&self, request: PlayRequest);
    fn stop(&self);
    fn set_paused(&self, paused: bool);
    fn seek(&self, position_ms: i64);
    fn set_volume(&self, volume: i32);
    fn set_filters(&self, filters: &Filters);
    fn set_end_time(&self, end_time_ms: Option<i64>);
    /// Releases everything. The actor calls this exactly once, as it exits.
    fn shutdown(&self);
}

/// How one track's pump ended.
#[derive(Debug)]
pub enum PumpOutcome {
    /// Reached the end of the stream, or the configured `endTime`.
    Finished,
    /// Asked to stop. The actor already knows, so no event follows.
    Stopped,
    /// `started` is whether any audio ever reached the ring, which decides between
    /// `loadFailed` and `finished` on the resulting `TrackEndEvent`.
    Failed { exception: Exception, started: bool },
}

pub use engine::PipelineEngine;

#[cfg(test)]
pub mod testing {
    use std::sync::atomic::AtomicI64;
    use std::sync::{Arc, Mutex};

    use lavalink_protocol::filters::Filters;
    use tokio::sync::mpsc;

    use super::{Engine, EngineEvent, PlayRequest};

    /// A record of one call into an [`Engine`].
    #[derive(Debug, Clone, PartialEq)]
    pub enum EngineCall {
        Play {
            identifier: String,
            start_position_ms: i64,
            paused: bool,
        },
        Stop,
        SetPaused {
            paused: bool,
        },
        Seek {
            position_ms: i64,
        },
        SetVolume {
            volume: i32,
        },
        SetFilters,
        SetEndTime {
            end_time_ms: Option<i64>,
        },
        Shutdown,
    }

    /// An engine that records what it was asked to do. Cloning shares the log, so a
    /// test can hold one while the actor owns another.
    #[derive(Debug, Clone, Default)]
    pub struct RecordingEngine {
        calls: Arc<Mutex<Vec<EngineCall>>>,
        position_ms: Arc<AtomicI64>,
        events: Arc<Mutex<Option<mpsc::Sender<EngineEvent>>>>,
    }

    impl RecordingEngine {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn calls(&self) -> Vec<EngineCall> {
            self.calls.lock().unwrap().clone()
        }

        pub fn clear(&self) {
            self.calls.lock().unwrap().clear();
        }

        /// The channel back to the actor, for tests that want to inject an event
        /// the way a real pipeline would.
        pub fn events(&self) -> Option<mpsc::Sender<EngineEvent>> {
            self.events.lock().unwrap().clone()
        }

        fn record(&self, call: EngineCall) {
            self.calls.lock().unwrap().push(call);
        }
    }

    impl Engine for RecordingEngine {
        fn position_handle(&self) -> Arc<AtomicI64> {
            Arc::clone(&self.position_ms)
        }

        fn attach(&self, events: mpsc::Sender<EngineEvent>) {
            *self.events.lock().unwrap() = Some(events);
        }

        fn play(&self, request: PlayRequest) {
            self.record(EngineCall::Play {
                identifier: request.track.info.identifier,
                start_position_ms: request.start_position_ms,
                paused: request.paused,
            });
        }

        fn stop(&self) {
            self.record(EngineCall::Stop);
        }

        fn set_paused(&self, paused: bool) {
            self.record(EngineCall::SetPaused { paused });
        }

        fn seek(&self, position_ms: i64) {
            self.record(EngineCall::Seek { position_ms });
        }

        fn set_volume(&self, volume: i32) {
            self.record(EngineCall::SetVolume { volume });
        }

        fn set_filters(&self, _filters: &Filters) {
            self.record(EngineCall::SetFilters);
        }

        fn set_end_time(&self, end_time_ms: Option<i64>) {
            self.record(EngineCall::SetEndTime { end_time_ms });
        }

        fn shutdown(&self) {
            self.record(EngineCall::Shutdown);
        }
    }
}
