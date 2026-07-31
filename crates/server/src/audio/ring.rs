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
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
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
}

impl FrameCounters {
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
    /// Shared with the actor.
    position_ms: Arc<AtomicI64>,
    frames: Arc<FrameCounters>,
}

impl Shared {
    fn refresh_position(&self) {
        let frames = self.consumed_frames.load(Ordering::Relaxed);
        let elapsed_ms = frames * 1000 / i64::from(SAMPLE_RATE);
        let base = self.base_position_ms.load(Ordering::Relaxed);
        self.position_ms.store(base + elapsed_ms, Ordering::Relaxed);
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
        frames,
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
#[derive(Debug)]
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
        // `extend(&[f32])` and not `extend(iter().copied())`: only `Extend<&T> where
        // T: Copy` specialises to a slice copy, and the `f32`-yielding form falls
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

    /// Discards buffered audio and restarts the position counter at `position_ms`.
    ///
    /// Called by the pump after it has seeked the decoder: the buffer holds audio
    /// from *before* the seek, which must not be played.
    pub fn reset(&self, position_ms: i64) {
        let mut buffer = lock(&self.shared.buffer);
        buffer.clear();
        self.shared.finished.store(false, Ordering::Release);
        self.shared.consumed_frames.store(0, Ordering::Relaxed);
        self.shared.remainder_samples.store(0, Ordering::Relaxed);
        self.shared
            .base_position_ms
            .store(position_ms, Ordering::Relaxed);
        self.shared.refresh_position();
        self.shared.space.notify_all();
    }

    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Relaxed)
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
            // and a position that stalled during a stutter would be wrong. This
            // happens before the buffer lock is released — matching the section
            // `RingWriter::reset` holds the same lock across — so a concurrent
            // seek can never have its rebase clobbered by a read that started
            // before it, or vice versa.
            let silence = out.len().min(FRAME_SAMPLES * 4);
            self.shared.advance_frames((silence / 4) as i64);
            self.shared.refresh_position();
            drop(buffer);
            // Starved: the pump has not kept up. Hand back silence rather than
            // blocking the mixer, and account for it as a lost frame — exactly what
            // the original counts as `nulled`. No producer can be waiting on space
            // here (the buffer was empty), so there is nothing to `notify_all`.
            self.shared.frames.nulled.fetch_add(1, Ordering::Relaxed);
            out[..silence].fill(0);
            return Ok(silence);
        }

        let take = wanted_samples.min(buffer.len());

        // The common case: `wanted_samples = out.len() / 4`, so `take * 4 <=
        // out.len()` always holds and every drained sample is written straight into
        // `out` with nothing left over. The exception is `out.len() < 4` (the
        // `.max(1)` above), where a single sample can't fit — that's the one case
        // still routed through `leftover`.
        let written = if out.len() >= 4 {
            // Copied off `as_slices` rather than straight through `drain`: a
            // `VecDeque`'s drain iterator carries the wraparound check into every
            // step, so the conversion loop stays scalar however simple its body is.
            // Over a contiguous `&[f32]` the same `to_le_bytes`-and-write is a shape
            // the optimiser can widen — on a little-endian target it is a memcpy.
            // Two segments because the head may sit anywhere in the ring; a ring that
            // has been playing for any length of time is always split.
            let (front, back) = buffer.as_slices();
            let from_front = take.min(front.len());
            let mut written = 0;
            for source in [&front[..from_front], &back[..take - from_front]] {
                for (chunk, sample) in out[written..].chunks_exact_mut(4).zip(source) {
                    chunk.copy_from_slice(&sample.to_le_bytes());
                }
                written += source.len() * 4;
            }
            // Drops the samples just copied. `Drain` removes its range on drop, so
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

        self.shared.frames.sent.fetch_add(1, Ordering::Relaxed);

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

impl symphonia::core::io::MediaSource for RingReader {
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
            // moved off zero, `as_slices` reports a non-empty second segment.
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

    #[test]
    fn a_starved_ring_yields_silence_and_counts_a_nulled_frame() {
        let (_writer, mut reader, _) = ring(1000);

        let samples = read_samples(&mut reader, FRAME_SAMPLES);
        assert_eq!(samples.len(), FRAME_SAMPLES);
        assert!(samples.iter().all(|sample| *sample == 0.0));

        let (sent, nulled) = reader.take_frame_stats();
        assert_eq!((sent, nulled), (0, 1));
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
}
