//! The buffer between the pump and the send path.
//!
//! This is the isolation device. The pump does the expensive work — demux, decode,
//! resample, filter — and has no deadline; the send path has a hard 20ms deadline
//! and does nothing but copy. Neither can make the other late, because they only
//! meet here.
//!
//! ```text
//! [pump: CPU-bound, no deadline]                 [send: O(1), 20ms deadline]
//! source → decode → filter → f32 PCM ──▶ ring ──▶ mixer pulls via Read
//! ```
//!
//! Two consequences worth stating, because they are what the design buys:
//!
//! * A pump that falls behind starves **its own** ring. The reader gets silence and
//!   counts a nulled frame — the same accounting as the original's
//!   `AudioLossCounter`. Other players are untouched.
//! * A pump that runs ahead blocks on a full ring, so it can never be more than
//!   `frameBufferDurationMs` ahead of playback.
//!
//! # Position
//!
//! The position counter is advanced **here, on the read side**, not by the pump.
//! The pump is up to a whole buffer ahead of what anyone can hear, so its
//! progress is not a playback position. There is exactly one reader, so the counter
//! has one writer and needs no coordination.

use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::lock;

/// Discord's sample rate. Everything is resampled to this before it reaches the ring.
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;

/// One Discord frame: 20ms of stereo audio.
pub const FRAME_SAMPLES: usize = (SAMPLE_RATE as usize / 50) * CHANNELS;

/// How long a producer waits for space before re-checking whether it was stopped.
/// Only [`RingWriter::write`] uses it; the pump polls on its own `COMMAND_POLL`.
#[cfg(test)]
const PRODUCER_POLL: Duration = Duration::from_millis(100);

/// Frames sent and frames nulled, for `/v4/stats`' `frameStats`.
///
/// Lives outside [`Shared`] and is handed in from the caller rather than created
/// per ring, because a ring is rebuilt on every new track
/// (`PipelineEngine::play`) but the original's `AudioLossCounter` is a per-player
/// counter that survives a track change — a quick switch between tracks does not
/// reset it. Sharing one `Arc` across every ring a player creates reproduces that.
#[derive(Debug, Default)]
pub struct FrameCounters {
    sent: AtomicU32,
    nulled: AtomicU32,
    /// Samples handed to `record_sent`/`record_nulled` that did not complete a
    /// whole [`FRAME_SAMPLES`]-sized frame by themselves — carried into the next
    /// call rather than truncated away, the same shape as [`Shared`]'s own
    /// `remainder_samples`. Without this, a `read()` counts as one frame
    /// whatever its actual size: a ring drained in many small partial reads (a
    /// starved buffer nearly caught up, or `out.len() < FRAME_SAMPLES * 4`)
    /// would report far more frames than the 20ms of audio it actually moved.
    sent_remainder: AtomicUsize,
    nulled_remainder: AtomicUsize,
}

impl FrameCounters {
    /// Counts `samples` toward `sent`, in units of whole [`FRAME_SAMPLES`] frames,
    /// carrying any leftover forward.
    fn record_sent(&self, samples: usize) {
        Self::record(&self.sent, &self.sent_remainder, samples);
    }

    /// As [`Self::record_sent`], for silence handed back on a starved read.
    fn record_nulled(&self, samples: usize) {
        Self::record(&self.nulled, &self.nulled_remainder, samples);
    }

    fn record(counter: &AtomicU32, remainder: &AtomicUsize, samples: usize) {
        let total = remainder.load(Ordering::Relaxed) + samples;
        if let Ok(frames) = u32::try_from(total / FRAME_SAMPLES) {
            counter.fetch_add(frames, Ordering::Relaxed);
        }
        remainder.store(total % FRAME_SAMPLES, Ordering::Relaxed);
    }

    /// Reads and resets both counters. `/v4/stats` ticks drain every player's
    /// counters every tick regardless of whether that player's data ends up
    /// "usable" for the aggregate — matching the original, which resets on
    /// its own minute boundary independent of whether it was queried.
    pub fn take(&self) -> (u32, u32) {
        (
            self.sent.swap(0, Ordering::Relaxed),
            self.nulled.swap(0, Ordering::Relaxed),
        )
    }
}

#[derive(Debug)]
struct Shared {
    /// Interleaved stereo samples. A deque rather than a `Vec` because the reader
    /// takes from the front every 20ms, and shifting a multi-second buffer down on
    /// each frame would cost more than the decoding does.
    buffer: Mutex<VecDeque<f32>>,
    /// Signalled on both ends: producers wait for space, consumers never block.
    space: Condvar,
    capacity: usize,
    /// The producer has delivered the whole track.
    finished: AtomicBool,
    /// The ring is being torn down; producers stop, consumers report end of stream.
    closed: AtomicBool,
    /// Frames (not samples) handed to the reader since the last seek.
    consumed_frames: AtomicI64,
    /// A sample handed to the reader that did not complete a stereo frame by
    /// itself (0 or 1, since `CHANNELS` is 2) — carried into the next call to
    /// [`Shared::advance_frames`] instead of being silently dropped by that
    /// call's own truncating division. Reset alongside `consumed_frames`,
    /// since a seek discards whatever it was counting toward.
    remainder_samples: AtomicI64,
    /// Playback position at the last seek, in milliseconds.
    base_position_ms: AtomicI64,
    /// Shared with the actor — and, because the engine hands the same handle to
    /// every ring it builds for a player, with every *other* ring that player has
    /// ever had. See `owns_position`.
    position_ms: Arc<AtomicI64>,
    /// Whether this ring is still the one entitled to write `position_ms`.
    ///
    /// Cleared by [`RingWriter::detach_position`] when the engine supersedes this
    /// ring. A superseded `RingReader` stays alive inside songbird's `Input` until
    /// the mixer gets round to dropping it, and any read it services meanwhile —
    /// including a starved one, which refreshes the position too — would otherwise
    /// write *this* track's base and consumed frames over the live one's.
    ///
    /// A `Mutex`, not an `AtomicBool`: `refresh_position` holds it across its whole
    /// check-then-store, and `detach_position` takes it too, so the two can never
    /// interleave. Two independent atomics let a `refresh_position` pass the check
    /// a moment before `detach_position` flipped it, then get preempted before its
    /// store — long enough for the engine to build the replacement ring and write
    /// the new track's position, which this call's now-stale store would then
    /// overwrite.
    owns_position: Mutex<bool>,
    frames: Arc<FrameCounters>,
    /// The position a seek in flight will land on, or `-1` while none is
    /// pending. Ports lavaplayer's own `LocalAudioTrackExecutor.queuedSeek`:
    /// its `getPosition()` reports this unconditionally while it is set,
    /// falling back to the last real frame's timecode only once the seek has
    /// actually been applied (`queuedSeek.set(-1)` in `applySeekState`, run
    /// on the playback thread) — not before.
    ///
    /// Set synchronously by [`RingWriter::begin_seek`] the moment the engine
    /// accepts a seek, ahead of the pump ever seeing the command. Without
    /// this, `refresh_position` below kept computing from the *old* base for
    /// as long as the pump took to notice the command (up to `COMMAND_POLL`
    /// on a full ring) and buffered pre-seek audio kept draining — so a
    /// client watching `playerUpdate` saw the position it was just told to
    /// expect regress toward wherever the stale audio happened to be, then
    /// jump forward again once the seek landed.
    pending_seek_ms: AtomicI64,
}

impl Shared {
    fn refresh_position(&self) {
        let owns = lock(&self.owns_position);
        if !*owns {
            return;
        }
        let pending = self.pending_seek_ms.load(Ordering::Relaxed);
        if pending != -1 {
            self.position_ms.store(pending, Ordering::Relaxed);
            return;
        }
        let frames = self.consumed_frames.load(Ordering::Relaxed);
        let elapsed_ms = frames * 1000 / i64::from(SAMPLE_RATE);
        let base = self.base_position_ms.load(Ordering::Relaxed);
        self.position_ms.store(base + elapsed_ms, Ordering::Relaxed);
    }

    /// Retires the announcement for `position_ms`, leaving a newer one alone.
    ///
    /// Both callers ([`RingWriter::reset`] and [`RingWriter::cancel_seek`]) are
    /// finishing one specific seek, and both run on the pump thread long after
    /// the engine announced it — so the value they are retiring is only theirs
    /// if it is still the one they were given.
    fn clear_pending_seek(&self, position_ms: i64) {
        let _ = self.pending_seek_ms.compare_exchange(
            position_ms,
            -1,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }

    /// Advances `consumed_frames` by however many whole frames `samples` (plus
    /// any sample left over from the previous call) makes up, carrying a new
    /// leftover forward rather than truncating it away — `samples` is not
    /// guaranteed to be a multiple of `CHANNELS` on its own, since a partial
    /// drain near a starved or nearly-empty buffer can hand over an odd count.
    fn advance_frames(&self, samples: i64) {
        let total = self.remainder_samples.load(Ordering::Relaxed) + samples;
        self.consumed_frames
            .fetch_add(total / CHANNELS as i64, Ordering::Relaxed);
        self.remainder_samples
            .store(total % CHANNELS as i64, Ordering::Relaxed);
    }
}

/// Creates a ring holding `buffer_ms` of audio, and its two ends.
///
/// `frames` is owned by the caller (see [`FrameCounters`]'s docs for why) rather
/// than created here.
pub fn channel(
    buffer_ms: u32,
    position_ms: Arc<AtomicI64>,
    frames: Arc<FrameCounters>,
) -> (RingWriter, RingReader) {
    let capacity_frames = (SAMPLE_RATE as usize * buffer_ms.max(20) as usize) / 1000;
    let shared = Arc::new(Shared {
        buffer: Mutex::new(VecDeque::with_capacity(capacity_frames * CHANNELS)),
        space: Condvar::new(),
        capacity: capacity_frames * CHANNELS,
        finished: AtomicBool::new(false),
        closed: AtomicBool::new(false),
        consumed_frames: AtomicI64::new(0),
        remainder_samples: AtomicI64::new(0),
        base_position_ms: AtomicI64::new(0),
        position_ms,
        owns_position: Mutex::new(true),
        frames,
        pending_seek_ms: AtomicI64::new(-1),
    });

    (
        RingWriter {
            shared: Arc::clone(&shared),
        },
        RingReader {
            shared,
            leftover: Vec::new(),
        },
    )
}

/// The pump's end.
#[derive(Debug, Clone)]
pub struct RingWriter {
    shared: Arc<Shared>,
}

impl RingWriter {
    /// Appends samples, blocking while the ring is full.
    ///
    /// Test-only. The pump cannot park on a full ring — it has commands to drain —
    /// so it drives [`Self::try_write`] and [`Self::wait_for_space`] itself
    /// (`pump::write_interruptibly`). This is the same loop without the interruption,
    /// which is all a test that just wants the samples in there needs.
    #[cfg(test)]
    pub fn write(&self, mut samples: &[f32]) -> bool {
        while !samples.is_empty() {
            let (written, closed) = self.try_write(samples);
            if closed {
                return false;
            }
            samples = &samples[written..];
            if samples.is_empty() {
                break;
            }
            if !self.wait_for_space(PRODUCER_POLL) {
                return false;
            }
        }
        true
    }

    /// Appends as many of `samples` as currently fit, without blocking.
    ///
    /// Returns how many samples were written and whether the ring is closed.
    /// Pairs with [`Self::wait_for_space`]: a caller that needs to check for
    /// other work between attempts (rather than parking the way [`Self::write`]
    /// does) calls this first, and only waits when it comes back short.
    pub fn try_write(&self, samples: &[f32]) -> (usize, bool) {
        if self.shared.closed.load(Ordering::Relaxed) {
            return (0, true);
        }
        let mut buffer = lock(&self.shared.buffer);
        let room = self.shared.capacity.saturating_sub(buffer.len());
        let take = room.min(samples.len());
        // extend(&[f32]) and not extend(iter().copied()): only `Extend<&T> where
        // T: Copy specialises to a slice copy, and the f32`-yielding form falls
        // back to appending one element at a time with a wraparound check each. This
        // runs with the lock held and the reader is on a 20ms deadline behind it, so
        // the shape of this copy is the reader's problem too, not just the pump's.
        buffer.extend(&samples[..take]);
        (take, false)
    }

    /// Waits up to `timeout` for room in the ring or for it to close, whichever
    /// comes first. Returns whether the ring is still open — `false` means stop,
    /// same as [`Self::write`]'s return value.
    pub fn wait_for_space(&self, timeout: Duration) -> bool {
        if self.shared.closed.load(Ordering::Relaxed) {
            return false;
        }
        let buffer = lock(&self.shared.buffer);
        if buffer.len() < self.shared.capacity {
            return true;
        }
        let _ = self
            .shared
            .space
            .wait_timeout(buffer, timeout)
            .unwrap_or_else(|e| e.into_inner());
        !self.shared.closed.load(Ordering::Relaxed)
    }

    /// The track is fully delivered. The reader drains what is left, then reports
    /// end of stream.
    pub fn finish(&self) {
        self.shared.finished.store(true, Ordering::Release);
    }

    /// Gives up this ring's claim on the shared position counter.
    ///
    /// Called by the engine the moment it supersedes this ring, which it does
    /// synchronously — before the replacement ring exists. Everything after that
    /// point is the next track's to report, even though this ring's reader is
    /// still alive inside songbird's `Input` until the mixer drops it. See
    /// [`Shared::owns_position`].
    pub fn detach_position(&self) {
        *lock(&self.shared.owns_position) = false;
    }

    /// Discards buffered audio and restarts the position counter at `position_ms`.
    ///
    /// Called by the pump after it has seeked the decoder: the buffer holds audio
    /// from *before* the seek, which must not be played. Also the point where a
    /// seek that [`Self::begin_seek`] announced has actually landed: `base` is
    /// rebased to the same target `pending_seek_ms` was already reporting, so
    /// clearing it here (after the rebase, before `refresh_position` runs) hands
    /// position reporting back to the normal frame-tracked path without the
    /// value it reports ever changing.
    ///
    /// Clears only the announcement this call is landing, hence the
    /// compare-exchange: the engine announces on its own thread, so a second seek
    /// can be accepted between the pump seeking the decoder and arriving here.
    /// An unconditional clear would drop that newer target and report `base +
    /// elapsed` until the pump worked through the second seek's own I/O — the
    /// regress-then-jump this field exists to prevent. A failed exchange means
    /// the pending value belongs to a later seek (or is already `-1`, as on
    /// `open`'s initial reset), and either way is not ours to clear.
    pub fn reset(&self, position_ms: i64) {
        let mut buffer = lock(&self.shared.buffer);
        buffer.clear();
        self.shared.finished.store(false, Ordering::Release);
        self.shared.consumed_frames.store(0, Ordering::Relaxed);
        self.shared.remainder_samples.store(0, Ordering::Relaxed);
        self.shared
            .base_position_ms
            .store(position_ms, Ordering::Relaxed);
        self.shared.clear_pending_seek(position_ms);
        self.shared.refresh_position();
        self.shared.space.notify_all();
    }

    /// Announces a seek that is about to be handed to the pump, before it has
    /// been applied — see [`Shared::pending_seek_ms`]. Called by the engine
    /// synchronously, in the same call that queues the command, so there is no
    /// window where the command is in flight but nothing yet reflects it.
    pub fn begin_seek(&self, position_ms: i64) {
        self.shared
            .pending_seek_ms
            .store(position_ms, Ordering::Relaxed);
    }

    /// Cancels a seek [`Self::begin_seek`] announced but that never landed —
    /// the pump found the target unseekable and the decoder kept going from
    /// wherever it actually was, the same outcome the pump's own `seek` falls
    /// back to on failure (`pump.rs`'s `State::seek`). Position reporting
    /// must return to that real, unmoved position instead of holding at a
    /// target that is never going to arrive.
    ///
    /// Takes the target it is cancelling for the same reason [`Self::reset`]
    /// does: a seek accepted while this one was failing is still going to land,
    /// and cancelling *it* would report a position the client was never told to
    /// expect.
    pub fn cancel_seek(&self, position_ms: i64) {
        self.shared.clear_pending_seek(position_ms);
    }

    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Relaxed)
    }

    /// Wakes a pump parked in [`Self::wait_for_space`], without changing
    /// anything it is waiting for.
    ///
    /// Called by the engine alongside a command send (`Seek`, `SetVolume`,
    /// `SetFilters`, `SetEndTime`) so a pump that is currently blocked on a
    /// full ring — the common case in steady playback, since decode outruns
    /// real time — checks its command queue immediately instead of only at
    /// the next `COMMAND_POLL` tick, up to 100ms later. Reuses the same
    /// `space` condvar `reset` already notifies on, so this does not add a
    /// second wakeup mechanism to the ring.
    pub fn wake(&self) {
        self.shared.space.notify_all();
    }
}

/// The send path's end: a byte stream of little-endian `f32` samples, which is the
/// format the voice mixer's raw adapter expects.
#[derive(Debug)]
pub struct RingReader {
    shared: Arc<Shared>,
    /// Bytes of a sample that did not fit in the caller's buffer last time. A read
    /// is not guaranteed to land on a 4-byte boundary.
    leftover: Vec<u8>,
}

impl RingReader {
    /// Frames delivered and frames missed since the last call, for `/v4/stats`.
    pub fn take_frame_stats(&self) -> (u32, u32) {
        self.shared.frames.take()
    }

    pub fn close(&self) {
        self.shared.closed.store(true, Ordering::Relaxed);
        self.shared.space.notify_all();
    }

}

impl Drop for RingReader {
    fn drop(&mut self) {
        self.close();
    }
}

impl Read for RingReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }

        if !self.leftover.is_empty() {
            let take = self.leftover.len().min(out.len());
            out[..take].copy_from_slice(&self.leftover[..take]);
            self.leftover.drain(..take);
            return Ok(take);
        }

        if self.shared.closed.load(Ordering::Relaxed) {
            // End of stream. The mixer stops the track on this.
            return Ok(0);
        }

        let wanted_samples = (out.len() / 4).max(1);
        let mut buffer = lock(&self.shared.buffer);

        if buffer.is_empty() {
            if self.shared.finished.load(Ordering::Acquire) {
                return Ok(0);
            }
            // Silence still advances playback: the listener heard 20ms of nothing,
            // and a stalled position during a stutter would be wrong. Done before
            // the buffer lock is released — matching the section RingWriter::reset
            // holds the same lock across — so a concurrent seek's rebase can't be
            // clobbered by a read that started before it, or vice versa.
            //
            // Counted in samples like wanted_samples/take below, and written to out
            // the same two ways: whole 4-byte samples straight in, with a leftover
            // partial sample (only when out.len() < 4) routed through leftover.
            // Silence used to just fill out.len().min(FRAME_SAMPLES * 4) bytes
            // outright, which wasn't a multiple of 4 whenever out.len() wasn't —
            // permanently misaligning every later read by the short fraction,
            // since nothing carried the remainder forward.
            let silence_samples = wanted_samples.min(FRAME_SAMPLES);
            self.shared.advance_frames(silence_samples as i64);
            self.shared.refresh_position();
            drop(buffer);
            // Starved: the pump has not kept up. Hand back silence rather than
            // blocking the mixer, and account for it as a lost frame — exactly what
            // the original counts as nulled. No producer can be waiting on space
            // here (the buffer was empty), so there is nothing to notify_all.
            self.shared.frames.record_nulled(silence_samples);
            let written = if out.len() >= 4 {
                let silence_bytes = silence_samples * 4;
                out[..silence_bytes].fill(0);
                silence_bytes
            } else {
                let bytes = [0u8; 4];
                let written = bytes.len().min(out.len());
                out[..written].copy_from_slice(&bytes[..written]);
                self.leftover.extend_from_slice(&bytes[written..]);
                written
            };
            return Ok(written);
        }

        let take = wanted_samples.min(buffer.len());

        // The common case: wanted_samples = out.len() / 4, so take * 4 <= out.len()
        // always holds today, and every drained sample is written straight into out
        // with nothing left over. That bound comes from how wanted_samples is
        // computed, not from anything the zip loop below checks — it sums
        // source.len() * 4 into written unconditionally, trusting the bound rather
        // than verifying it, so a future change letting take * 4 exceed out.len()
        // would silently under-report written (zip stops at whichever side runs
        // out first) instead of panicking. The exception today is out.len() < 4
        // (the .max(1) above), where a single sample can't fit — routed through
        // leftover instead.
        let written = if out.len() >= 4 {
            // Copied off as_slices rather than through drain directly: a
            // VecDeque's drain iterator carries the wraparound check into every
            // step, keeping the conversion loop scalar however simple its body
            // is. Over a contiguous &[f32] the same to_le_bytes-and-write is a
            // shape the optimiser can widen — on little-endian, a memcpy. Two
            // segments because the head may sit anywhere in the ring; a ring
            // that's been playing any length of time is always split.
            let (front, back) = buffer.as_slices();
            let from_front = take.min(front.len());
            let mut written = 0;
            for source in [&front[..from_front], &back[..take - from_front]] {
                for (chunk, sample) in out[written..].chunks_exact_mut(4).zip(source) {
                    chunk.copy_from_slice(&sample.to_le_bytes());
                }
                written += source.len() * 4;
            }
            // Drops the samples just copied. Drain removes its range on drop, so
            // this is the removal — nothing is left to iterate.
            buffer.drain(..take);
            written
        } else {
            let sample = buffer.pop_front().expect("buffer is non-empty here");
            let bytes = sample.to_le_bytes();
            let written = bytes.len().min(out.len());
            out[..written].copy_from_slice(&bytes[..written]);
            self.leftover.extend_from_slice(&bytes[written..]);
            written
        };
        // Position bookkeeping happens before the buffer lock is released — see
        // the starved branch above for why.
        self.shared.advance_frames(take as i64);
        self.shared.refresh_position();
        drop(buffer);
        self.shared.space.notify_all();

        self.shared.frames.record_sent(take);

        Ok(written)
    }
}

impl Seek for RingReader {
    /// The ring is a live stream. Seeking is the pump's job — it rebuilds its
    /// decoder and calls [`RingWriter::reset`] — so the mixer must never try.
    fn seek(&mut self, _from: SeekFrom) -> io::Result<u64> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the ring is a live stream; seeking happens in the pump",
        ))
    }
}

// Implemented against songbird's own vendored symphonia-core (currently 0.5.x,
// re-exported as songbird::input::core), not this crate's own symphonia
// dependency — RawAdapter requires the trait from songbird's copy specifically,
// and the two are unrelated crate instances once their versions diverge.
impl songbird::input::core::io::MediaSource for RingReader {
    /// Always false, and this is load-bearing.
    ///
    /// The mixer wraps this in its raw adapter and would otherwise be entitled to
    /// seek it — but the bytes behind this reader are produced on demand and are
    /// gone once read. Seeking is the pump's job, and saying so here is what stops
    /// the two mechanisms fighting.
    fn is_seekable(&self) -> bool {
        false
    }

    /// Unknown: this is a live stream, however long the track turns out to be.
    fn byte_len(&self) -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(buffer_ms: u32) -> (RingWriter, RingReader, Arc<AtomicI64>) {
        let position = Arc::new(AtomicI64::new(0));
        let (writer, reader) = channel(
            buffer_ms,
            Arc::clone(&position),
            Arc::new(FrameCounters::default()),
        );
        (writer, reader, position)
    }

    fn read_samples(reader: &mut RingReader, count: usize) -> Vec<f32> {
        let mut bytes = vec![0u8; count * 4];
        let read = reader.read(&mut bytes).unwrap();
        bytes[..read]
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes(chunk.try_into().unwrap()))
            .collect()
    }

    #[test]
    fn samples_come_out_in_order_as_little_endian_floats() {
        let (writer, mut reader, _) = ring(1000);
        writer.write(&[0.5, -0.5, 0.25, -0.25]);

        assert_eq!(read_samples(&mut reader, 4), vec![0.5, -0.5, 0.25, -0.25]);
    }

    /// A caller whose buffer is smaller than one sample still makes progress: the
    /// remainder is held and handed over on the next read rather than being lost.
    #[test]
    fn a_read_smaller_than_one_sample_resumes_where_it_left_off() {
        let (writer, mut reader, _) = ring(1000);
        writer.write(&[1.0, 2.0]);

        let mut first = [0u8; 3];
        assert_eq!(reader.read(&mut first).unwrap(), 3);

        let mut rest = [0u8; 1];
        assert_eq!(reader.read(&mut rest).unwrap(), 1);

        let sample = [first[0], first[1], first[2], rest[0]];
        assert_eq!(f32::from_le_bytes(sample), 1.0);

        // The second sample is untouched and still queued.
        assert_eq!(read_samples(&mut reader, 1), vec![2.0]);
    }

    /// Whole-sample reads are the common case and are never split.
    /// A read copies out of the deque's two contiguous segments and stitches them
    /// into the caller's buffer, so the offset arithmetic between the two has to be
    /// right. A freshly filled ring is one segment and would never exercise the
    /// seam; a ring that has been playing for any length of time always straddles
    /// it, so this walks the head all the way around several times.
    #[test]
    fn samples_stay_in_order_when_the_buffer_wraps() {
        let (writer, mut reader, _position) = ring(20);
        const CHUNK: usize = 500;
        const PREFILL: usize = 900;

        // A reserve the reader never catches up with, so the head keeps walking
        // forward instead of meeting the tail and starting over at zero every round
        // — which is also what a healthy pump keeps the ring in.
        let mut next = 0usize;
        let prefill: Vec<f32> = (0..PREFILL).map(|i| i as f32).collect();
        assert_eq!(writer.try_write(&prefill).0, PREFILL);
        next += PREFILL;

        let mut wrapped = false;
        let mut got = Vec::new();
        for _ in 0..20 {
            let chunk: Vec<f32> = (next..next + CHUNK).map(|i| i as f32).collect();
            assert_eq!(writer.try_write(&chunk).0, CHUNK);
            next += CHUNK;
            // Checked while the samples are still in the ring: once the head has
            // moved off zero, as_slices reports a non-empty second segment.
            wrapped |= !lock(&reader.shared.buffer).as_slices().1.is_empty();
            got.extend(read_samples(&mut reader, CHUNK));
        }

        assert!(wrapped, "the head never wrapped — the seam was never exercised");
        let expected: Vec<f32> = (0..got.len()).map(|i| i as f32).collect();
        assert_eq!(got, expected, "samples came back out of order across the seam");
    }

    #[test]
    fn a_read_of_several_samples_returns_them_whole() {
        let (writer, mut reader, _) = ring(1000);
        writer.write(&[1.0, 2.0, 3.0]);

        let mut out = [0u8; 12];
        assert_eq!(reader.read(&mut out).unwrap(), 12);
        let samples: Vec<f32> = out
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes(chunk.try_into().unwrap()))
            .collect();
        assert_eq!(samples, vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn position_advances_with_what_the_reader_consumed() {
        let (writer, mut reader, position) = ring(1000);
        // One second of stereo audio.
        writer.write(&vec![0.0; SAMPLE_RATE as usize * CHANNELS]);
        assert_eq!(
            position.load(Ordering::Relaxed),
            0,
            "production alone must not move the position"
        );

        // Consume half a second.
        let half = SAMPLE_RATE as usize / 2 * CHANNELS;
        let mut consumed = 0;
        while consumed < half {
            consumed += read_samples(&mut reader, half - consumed).len();
        }

        assert_eq!(position.load(Ordering::Relaxed), 500);
    }

    /// The bug: a partial drain whose sample count isn't a multiple of
    /// `CHANNELS` used to silently drop the odd leftover from
    /// `consumed_frames` via truncating division — individually tiny (half a
    /// frame, a fraction of a millisecond) but one-directional and never
    /// corrected, so it compounds over a long-running stream into an
    /// observable amount of drift.
    #[test]
    fn a_partial_drains_remainder_is_not_silently_lost_over_many_reads() {
        let (writer, mut reader, position) = ring(1000);

        // Each cycle writes and drains 4 samples (2 stereo frames) split
        // across two reads of 3 and 1 samples — both counts odd, so both
        // truncate on their own if the remainder isn't carried between them.
        // Without the fix this nets exactly 1 counted frame per cycle
        // instead of 2 — half the audio actually delivered.
        for _ in 0..480 {
            writer.try_write(&[1.0, 2.0, 3.0]);
            read_samples(&mut reader, 3);
            writer.try_write(&[4.0]);
            read_samples(&mut reader, 1);
        }

        // 480 cycles * 2 frames = 960 frames = 20ms at 48kHz, not the 10ms a
        // remainder dropped every cycle would report.
        assert_eq!(position.load(Ordering::Relaxed), 20);
    }

    /// The bug this guards: before the fix, `frameStats.sent` counted one frame
    /// per `read()` call regardless of its size, so draining the same audio
    /// across many small reads (a short read near the ring's own capacity, or a
    /// consumer that just doesn't ask for a whole frame at once) inflated the
    /// count far past what was actually delivered.
    #[test]
    fn frame_stats_count_samples_delivered_not_read_calls() {
        let (writer, mut reader, _) = ring(4000);
        writer.try_write(&vec![1.0; FRAME_SAMPLES * 3]);

        let mut delivered = 0;
        while delivered < FRAME_SAMPLES * 3 {
            let chunk = read_samples(&mut reader, 10);
            assert!(!chunk.is_empty(), "ran out of buffered audio early");
            delivered += chunk.len();
        }

        let (sent, nulled) = reader.take_frame_stats();
        assert_eq!(
            (sent, nulled),
            (3, 0),
            "three frames' worth of samples were delivered across many small reads"
        );
    }

    #[test]
    fn a_starved_ring_yields_silence_and_counts_a_nulled_frame() {
        let (_writer, mut reader, _) = ring(1000);

        let samples = read_samples(&mut reader, FRAME_SAMPLES);
        assert_eq!(samples.len(), FRAME_SAMPLES);
        assert!(samples.iter().all(|sample| *sample == 0.0));

        let (sent, nulled) = reader.take_frame_stats();
        assert_eq!((sent, nulled), (0, 1));
    }

    /// The bug this guards: a starved read used to hand back exactly
    /// out.len().min(FRAME_SAMPLES * 4) bytes with no alignment check, so an
    /// out whose length was not a multiple of 4 got a silence fill that was
    /// not a whole number of samples either — and nothing carried the leftover
    /// fraction forward, unlike every other partial-sample case in this file.
    /// Every subsequent read would then start mid-sample, permanently. A
    /// starved read must always return a multiple of 4 (when out is at least
    /// 4 bytes; the sub-4-byte case already routes through leftover the same
    /// as a normal read does), and real audio read right after one must still
    /// decode to the exact values written, not something shifted.
    #[test]
    fn a_starved_read_stays_sample_aligned_with_an_odd_sized_buffer() {
        let (writer, mut reader, _) = ring(1000);

        let mut out = vec![0u8; 4001];
        let written = reader.read(&mut out).unwrap();
        assert_eq!(written % 4, 0, "a starved read must return a whole number of samples");

        writer.write(&[0.5, -0.5, 0.25, -0.25]);
        assert_eq!(read_samples(&mut reader, 4), vec![0.5, -0.5, 0.25, -0.25]);
    }

    #[test]
    fn silence_from_starvation_still_advances_the_position() {
        let (_writer, mut reader, position) = ring(1000);
        read_samples(&mut reader, FRAME_SAMPLES);
        assert_eq!(position.load(Ordering::Relaxed), 20);
    }

    #[test]
    fn a_finished_and_drained_ring_reports_end_of_stream() {
        let (writer, mut reader, _) = ring(1000);
        writer.write(&[1.0, 2.0]);
        writer.finish();

        assert_eq!(read_samples(&mut reader, 2), vec![1.0, 2.0]);

        let mut out = [0u8; 64];
        assert_eq!(reader.read(&mut out).unwrap(), 0);
    }

    #[test]
    fn finishing_does_not_discard_what_is_still_buffered() {
        let (writer, mut reader, _) = ring(1000);
        writer.write(&[1.0; 100]);
        writer.finish();
        assert_eq!(read_samples(&mut reader, 100).len(), 100);
    }

    #[test]
    fn a_seek_reset_drops_stale_audio_and_rebases_the_position() {
        let (writer, mut reader, position) = ring(1000);
        writer.write(&vec![1.0; SAMPLE_RATE as usize * CHANNELS]);
        read_samples(&mut reader, FRAME_SAMPLES);
        assert!(position.load(Ordering::Relaxed) > 0);

        writer.reset(42_000);
        assert_eq!(position.load(Ordering::Relaxed), 42_000);

        // Nothing pre-seek survives: the next read starves rather than replaying.
        let samples = read_samples(&mut reader, 8);
        assert!(samples.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn the_position_continues_from_the_seek_target() {
        let (writer, mut reader, position) = ring(1000);
        writer.reset(10_000);
        writer.write(&vec![0.0; SAMPLE_RATE as usize * CHANNELS]);

        let quarter = SAMPLE_RATE as usize / 4 * CHANNELS;
        let mut consumed = 0;
        while consumed < quarter {
            consumed += read_samples(&mut reader, quarter - consumed).len();
        }

        assert_eq!(position.load(Ordering::Relaxed), 10_250);
    }

    /// Ports lavaplayer's own `LocalAudioTrackExecutor.getPosition()`, which
    /// reports `queuedSeek` unconditionally while a seek is pending, not the
    /// last real frame's timecode. Without `begin_seek`, a read landing after
    /// the announcement but before `reset` actually rebases the ring reports
    /// whatever the still-buffered pre-seek audio's trajectory says instead —
    /// the position-regresses-then-jumps bug this pins.
    #[test]
    fn a_pending_seek_is_reported_immediately_and_holds_until_it_lands() {
        let (writer, mut reader, position) = ring(1000);
        writer.write(&vec![1.0; SAMPLE_RATE as usize * CHANNELS]);
        read_samples(&mut reader, FRAME_SAMPLES);
        let before_seek = position.load(Ordering::Relaxed);
        assert!(before_seek > 0);

        // The engine announces the seek before the pump has done anything —
        // no reset yet, so the ring's own base/consumed_frames are still
        // exactly where they were.
        writer.begin_seek(90_000);
        assert_eq!(
            position.load(Ordering::Relaxed),
            before_seek,
            "announcing alone does not move the counter; a read has to happen"
        );

        // A read of still-buffered pre-seek audio must not regress the
        // reported position back toward its own trajectory.
        read_samples(&mut reader, FRAME_SAMPLES);
        assert_eq!(position.load(Ordering::Relaxed), 90_000);

        // Only once the pump's reset actually lands does the ring go back to
        // tracking frames for real — from the target, not from zero.
        writer.reset(90_000);
        assert_eq!(position.load(Ordering::Relaxed), 90_000);
        read_samples(&mut reader, FRAME_SAMPLES);
        assert!(position.load(Ordering::Relaxed) > 90_000);
    }

    /// The pump finds the source unseekable and never calls `reset`; position
    /// reporting must give up on the target and go back to reality instead of
    /// holding at a seek that is never going to land.
    #[test]
    fn cancelling_a_seek_returns_position_reporting_to_reality() {
        let (writer, mut reader, position) = ring(1000);
        writer.write(&vec![1.0; SAMPLE_RATE as usize * CHANNELS]);
        read_samples(&mut reader, FRAME_SAMPLES);
        let real_position = position.load(Ordering::Relaxed);

        writer.begin_seek(90_000);
        read_samples(&mut reader, FRAME_SAMPLES);
        assert_eq!(position.load(Ordering::Relaxed), 90_000);

        writer.cancel_seek(90_000);
        read_samples(&mut reader, FRAME_SAMPLES);
        assert!(
            position.load(Ordering::Relaxed) > real_position,
            "reporting must resume tracking the real, unmoved position"
        );
        assert!(position.load(Ordering::Relaxed) < 90_000);
    }

    /// Scrubbing a seek bar sends seeks faster than the pump lands them. The
    /// engine announces the second one while the pump is still finishing the
    /// first, so the first's `reset` used to clear the second's announcement and
    /// report `base + elapsed` for however long the second seek's own I/O took —
    /// an HTTP `Range` re-request, so hundreds of ms of the very regress-then-jump
    /// `pending_seek_ms` exists to prevent.
    #[test]
    fn landing_a_seek_does_not_retire_a_newer_one_announced_behind_it() {
        let (writer, mut reader, position) = ring(1000);
        writer.write(&vec![1.0; SAMPLE_RATE as usize * CHANNELS]);
        read_samples(&mut reader, FRAME_SAMPLES);

        writer.begin_seek(30_000);
        // The engine accepts the second seek before the pump has finished the
        // first — both announcements happen on its thread, not the pump's.
        writer.begin_seek(90_000);

        // The pump now lands the *first* seek.
        writer.reset(30_000);

        assert_eq!(
            position.load(Ordering::Relaxed),
            90_000,
            "the newer seek's announcement must survive the older one landing"
        );
        read_samples(&mut reader, FRAME_SAMPLES);
        assert_eq!(
            position.load(Ordering::Relaxed),
            90_000,
            "and must keep holding until the seek the client was told about lands"
        );

        writer.reset(90_000);
        assert_eq!(position.load(Ordering::Relaxed), 90_000);
        read_samples(&mut reader, FRAME_SAMPLES);
        assert!(position.load(Ordering::Relaxed) > 90_000);
    }

    /// The same hazard on the failure path: an unseekable target cancels its own
    /// announcement, not the one queued behind it.
    #[test]
    fn cancelling_a_failed_seek_does_not_retire_a_newer_one() {
        let (writer, mut reader, position) = ring(1000);
        writer.write(&vec![1.0; SAMPLE_RATE as usize * CHANNELS]);
        read_samples(&mut reader, FRAME_SAMPLES);

        writer.begin_seek(30_000);
        writer.begin_seek(90_000);
        writer.cancel_seek(30_000);

        read_samples(&mut reader, FRAME_SAMPLES);
        assert_eq!(
            position.load(Ordering::Relaxed),
            90_000,
            "a failed seek must not cancel the one announced after it"
        );
    }

    #[test]
    fn a_closed_ring_stops_the_producer_rather_than_wedging_it() {
        let (writer, reader, _) = ring(20);
        reader.close();

        // Far more than the ring can hold: without the close check this blocks.
        assert!(!writer.write(&vec![0.0; SAMPLE_RATE as usize * CHANNELS]));
        assert!(writer.is_closed());
    }

    #[test]
    fn dropping_the_reader_releases_the_producer() {
        let (writer, reader, _) = ring(20);
        drop(reader);
        assert!(!writer.write(&vec![0.0; SAMPLE_RATE as usize * CHANNELS]));
    }

    /// The point of the whole arrangement: the pump cannot run further ahead than
    /// the configured buffer, so memory is bounded and so is seek latency — and it
    /// resumes rather than wedging once the reader catches up.
    ///
    /// The reader must drain to end of stream, not to a sample count. Stopping early
    /// leaves the producer parked on a full ring with nobody to wake it, which is
    /// exactly the deadlock the buffer bound would otherwise hide.
    #[test]
    fn the_producer_is_capped_at_the_buffer_size_and_resumes() {
        let (writer, mut reader, _) = ring(100);
        let capacity_samples = (SAMPLE_RATE as usize / 10) * CHANNELS;
        let written = capacity_samples * 3;

        let producer = std::thread::spawn(move || {
            // Three times what the ring holds, so the producer has to block at least
            // twice and be woken again.
            writer.write(&vec![0.25; written]);
            writer.finish();
        });

        // Only real audio is counted: a starved read yields zeroes, and how many of
        // those appear depends on scheduling.
        let mut delivered = 0;
        let mut reads = 0;
        loop {
            let samples = read_samples(&mut reader, FRAME_SAMPLES);
            if samples.is_empty() {
                break;
            }
            delivered += samples.iter().filter(|s| **s == 0.25).count();

            reads += 1;
            assert!(reads < 10_000, "the reader is not making progress");
        }

        producer.join().unwrap();
        assert_eq!(delivered, written, "every sample must survive the trip");
    }

    /// `wake` is what cuts a command's latency on a full ring down from
    /// `COMMAND_POLL` to near-immediate — see `PipelineEngine::send_to_pump`.
    /// This pins the mechanism it relies on: a producer parked in
    /// `wait_for_space` returns as soon as `wake` is called, well before the
    /// timeout it was given.
    #[test]
    fn wake_returns_a_parked_producer_before_its_timeout() {
        let (writer, _reader, _) = ring(20);
        let capacity_samples = (SAMPLE_RATE as usize / 50) * CHANNELS;
        // Fill the ring completely so the next wait actually parks.
        let (written, _closed) = writer.try_write(&vec![0.0; capacity_samples]);
        assert_eq!(written, capacity_samples, "the ring must be full for this test");

        let long_timeout = Duration::from_secs(10);
        let waiting_writer = writer.clone();
        let waiter = std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let open = waiting_writer.wait_for_space(long_timeout);
            (open, started.elapsed())
        });

        // Give the thread above a chance to actually enter the wait before
        // waking it, without pinning the interleaving down further.
        std::thread::sleep(Duration::from_millis(50));
        writer.wake();

        let (open, elapsed) = waiter.join().unwrap();
        assert!(open, "the ring was never closed");
        assert!(
            elapsed < long_timeout / 2,
            "wake should return the waiter almost immediately, took {elapsed:?}"
        );
    }

    /// The bug: the engine hands the same `position_ms` handle to every ring it
    /// builds for a player, so a superseded ring shares the live one's counter. Its
    /// reader stays alive inside songbird's `Input` until the mixer drops it, and
    /// any read it services in that window — a starved one included, since that
    /// refreshes the position too — wrote the *old* track's base and consumed
    /// frames over the new track's. Once the engine has moved on, the old ring must
    /// not touch the counter again.
    #[test]
    fn a_detached_ring_stops_writing_the_shared_position() {
        let (writer, mut reader, position) = ring(1000);
        writer.write(&vec![0.25; SAMPLE_RATE as usize * CHANNELS]);
        read_samples(&mut reader, FRAME_SAMPLES);
        assert!(
            position.load(Ordering::Relaxed) > 0,
            "the live ring should be reporting its own position"
        );

        // What stop_active does the moment this ring is superseded, before the
        // replacement ring exists.
        writer.detach_position();
        // The replacement's position, as the engine and the new ring write it.
        position.store(7_000, Ordering::Relaxed);

        read_samples(&mut reader, FRAME_SAMPLES);
        assert_eq!(
            position.load(Ordering::Relaxed),
            7_000,
            "a superseded ring must not report over the live track's position"
        );
    }

    /// `detach_position` (the engine, synchronously, before the replacement ring
    /// exists) must never lose a race against a `refresh_position` already in
    /// flight on this ring's reader: by the time `detach_position` returns, no
    /// read still in flight when it was called may write `position_ms` afterward.
    /// Regression test for the race where the two were separate atomics with no
    /// lock tying the check in `refresh_position` to its own store, letting a
    /// `detach_position` land in the gap between them and get overwritten by a
    /// read that read `true` a moment before.
    ///
    /// Mirrors how the real caller uses these two: `detach_position` returns
    /// before the replacement ring is even created, so nothing the old ring's
    /// reader does is allowed to land after that point.
    #[test]
    fn a_racing_detach_and_read_never_clobbers_a_later_write() {
        for _ in 0..2_000 {
            let (writer, mut reader, position) = ring(1000);
            writer.write(&vec![0.0; SAMPLE_RATE as usize * CHANNELS]);

            let keep_reading = Arc::new(AtomicBool::new(true));
            let reader_thread = std::thread::spawn({
                let keep_reading = Arc::clone(&keep_reading);
                move || {
                    let mut out = vec![0u8; FRAME_SAMPLES * 4];
                    // Real audio, then starvation once it runs out — either kind of
                    // read still refreshes the position, so the loop keeps racing
                    // detach_position for as long as the main thread wants.
                    while keep_reading.load(Ordering::Relaxed) {
                        let _ = reader.read(&mut out);
                    }
                }
            });

            writer.detach_position();
            // What the engine does immediately after detaching, before the
            // replacement ring is created: nothing from the old ring may still be
            // in flight past this point, so this write must be the last one seen.
            position.store(7_000, Ordering::Relaxed);
            keep_reading.store(false, Ordering::Relaxed);
            reader_thread.join().unwrap();

            assert_eq!(
                position.load(Ordering::Relaxed),
                7_000,
                "a detached ring's in-flight read overwrote the live track's position"
            );
        }
    }

    /// A concurrent `reset` (the pump, after a seek) must never have its rebase
    /// clobbered by a `read` (the mixer) that was already in flight, and vice
    /// versa: whichever one runs must see a consistent, non-interleaved
    /// `consumed_frames`/`base_position_ms` pair. Regression test for the race
    /// where `read` wrote its bookkeeping after releasing the buffer lock,
    /// letting a `reset` land in between and get partially overwritten.
    #[test]
    fn a_racing_reset_and_read_never_corrupt_the_position() {
        for _ in 0..2_000 {
            let (writer, mut reader, position) = ring(1000);
            writer.write(&vec![0.0; SAMPLE_RATE as usize * CHANNELS]);

            let reader_thread = std::thread::spawn(move || {
                let mut out = vec![0u8; FRAME_SAMPLES * 4];
                let _ = reader.read(&mut out);
            });

            writer.reset(42_000);
            reader_thread.join().unwrap();

            // Whichever ran first or second, the reported position must be
            // exactly the seek target or the target plus one read's worth of
            // audio — never anything else, and never negative or wildly off.
            let observed = position.load(Ordering::Relaxed);
            assert!(
                observed == 42_000 || observed == 42_020,
                "position corrupted by a reset/read race: {observed}"
            );
        }
    }

    /// What `PipelineEngine::play()` actually relies on for `frameStats` to survive
    /// a skip/replace: `frames` is handed in by the caller and `channel()` never
    /// makes its own, so the same `Arc<FrameCounters>` can be threaded through a
    /// fresh ring for every new track (`ring::FrameCounters`'s docs). This is that
    /// mechanism in isolation, without a real engine or songbird.
    #[test]
    fn frame_counters_survive_a_new_ring_for_a_replaced_track() {
        let frames = Arc::new(FrameCounters::default());
        let position = Arc::new(AtomicI64::new(0));

        let (writer_a, mut reader_a) =
            channel(1000, Arc::clone(&position), Arc::clone(&frames));
        writer_a.write(&vec![0.0; FRAME_SAMPLES]);
        assert_eq!(read_samples(&mut reader_a, FRAME_SAMPLES).len(), FRAME_SAMPLES);
        drop(reader_a); // the track ends / is replaced, same as `PipelineEngine::stop_active`

        let (writer_b, mut reader_b) =
            channel(1000, Arc::clone(&position), Arc::clone(&frames));
        writer_b.write(&vec![0.0; FRAME_SAMPLES]);
        assert_eq!(read_samples(&mut reader_b, FRAME_SAMPLES).len(), FRAME_SAMPLES);

        let (sent, nulled) = frames.take();
        assert_eq!(
            sent, 2,
            "a new ring for a replaced track must not reset the shared counter"
        );
        assert_eq!(nulled, 0);
    }

    /// A starved read on the *second* ring must still add to the same counter as a
    /// healthy read on the first — nothing about switching rings should reset which
    /// counter a starvation is charged to.
    #[test]
    fn a_starved_read_on_a_later_ring_still_charges_the_shared_counter() {
        let frames = Arc::new(FrameCounters::default());
        let position = Arc::new(AtomicI64::new(0));

        let (writer_a, mut reader_a) =
            channel(1000, Arc::clone(&position), Arc::clone(&frames));
        writer_a.write(&vec![0.0; FRAME_SAMPLES]);
        read_samples(&mut reader_a, FRAME_SAMPLES);
        drop(reader_a);

        // Ring B has nothing written to it: every read starves.
        let (_writer_b, mut reader_b) =
            channel(1000, Arc::clone(&position), Arc::clone(&frames));
        read_samples(&mut reader_b, FRAME_SAMPLES);

        let (sent, nulled) = frames.take();
        assert_eq!(sent, 1, "ring A's healthy read");
        assert_eq!(nulled, 1, "ring B's starved read, on the same shared counter");
    }

    /// The bug this guards (#9): songbird builds its codec registry with
    /// `symphonia::default::register_enabled_codecs`, and its own `symphonia`
    /// dependency enables **no features at all** — by design, so the downstream
    /// crate picks the codec set. Which means the PCM decoder that songbird's
    /// `RawAdapter` path needs (`RawReader` declares `CODEC_TYPE_PCM_F32LE`) only
    /// exists in that registry if *we* enable `pcm` on the same symphonia version
    /// songbird resolved. That happened by accident for as long as our own
    /// symphonia was 0.5.x too, and stopped the instant we moved to 0.6: two
    /// versions are two unrelated crates, so nothing unified any more.
    ///
    /// Nothing about it fails loudly. `LiveInput::promote` just finds no decoder
    /// for the track, the mixer never pulls a frame, the ring fills, the pump
    /// parks on a full ring — and because the position counter is advanced on the
    /// *read* side, `position` sits at 0 until `TrackStuckEvent` fires. That is
    /// the whole of #9, and it is invisible to every test that stops at the ring.
    ///
    /// So this asserts the one thing that actually matters at the boundary: the
    /// registry songbird will use at runtime can decode what we hand it.
    #[test]
    fn songbird_can_promote_the_ring_into_a_playable_input() {
        use songbird::input::codecs::{get_codec_registry, get_probe};
        use songbird::input::{Input, RawAdapter};

        let (writer, reader, _position) = ring(1000);
        writer.write(&vec![0.0; FRAME_SAMPLES * 2]);

        let input: Input = RawAdapter::new(reader, SAMPLE_RATE, CHANNELS as u32).into();
        let Input::Live(live, _) = input else {
            panic!("a RawAdapter is already a live input");
        };

        let promoted = live.promote(get_codec_registry(), get_probe());
        assert!(
            promoted.is_ok(),
            "songbird cannot decode our raw f32 PCM: {:?} — its registry is missing \
             the symphonia `pcm` codec, see this test's docs",
            promoted.err()
        );
    }
}
