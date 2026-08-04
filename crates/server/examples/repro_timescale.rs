//! Standalone listening/timing harness for `signalsmith-stretch`, the stretcher
//! behind `filter::TimescaleFilter`.
//!
//! Runs outside the pump and the ring on purpose: it is faster to iterate on
//! chunk size, speed and pitch here than to spin up a whole player, and the WAV it
//! writes is what actually answers "does this sound right" — a question the unit
//! tests in `filter.rs` deliberately don't try to answer (see
//! `timescale_speed_changes_frame_count`'s doc comment).
//!
//! ```sh
//! cargo run -p lavalink-server --release --example repro_timescale -- --speed 1.5 --pitch 2.0 out.wav
//! ```
//!
//! Generates a 3-second 440Hz+880Hz stereo test tone, runs it through `Stretch` in
//! 1024-frame chunks (the same order of magnitude as one `pump` read), and writes the
//! result as a 16-bit PCM WAV for listening. Prints per-chunk timing so the CPU cost
//! is visible before it is a player.

use std::f32::consts::PI;
use std::fs::File;
use std::io::{BufWriter, Write as _};
use std::time::Instant;

use signalsmith_stretch::Stretch;

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u32 = 2;
const DURATION_SECS: u32 = 3;
const CHUNK_FRAMES: usize = 1024;

fn main() {
    let mut speed = 1.5_f32;
    let mut pitch_semitones = 0.0_f32;
    let mut out_path = "timescale_prototype.wav".to_string();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--speed" => speed = args.next().expect("--speed needs a value").parse().unwrap(),
            "--pitch" => {
                pitch_semitones = args.next().expect("--pitch needs a value").parse().unwrap()
            }
            path => out_path = path.to_string(),
        }
    }

    println!(
        "speed={speed}x pitch={pitch_semitones:+.1}st chunk={CHUNK_FRAMES} frames -> {out_path}"
    );

    let input = test_tone();

    let mut stretch = Stretch::preset_default(CHANNELS, SAMPLE_RATE);
    if pitch_semitones != 0.0 {
        stretch.set_transpose_factor_semitones(pitch_semitones, None);
    }
    println!(
        "input_latency={} output_latency={} frames",
        stretch.input_latency(),
        stretch.output_latency()
    );

    let mut output: Vec<f32> = Vec::new();
    let mut in_buf = vec![0.0_f32; CHUNK_FRAMES * CHANNELS as usize];
    let mut out_buf = vec![0.0_f32; 0];

    let mut total_process_time = std::time::Duration::ZERO;
    let mut chunks = 0u32;

    for chunk in input.chunks(CHUNK_FRAMES * CHANNELS as usize) {
        in_buf.truncate(chunk.len());
        in_buf.copy_from_slice(chunk);

        let in_frames = in_buf.len() / CHANNELS as usize;
        let out_frames = ((in_frames as f32) / speed).round() as usize;
        out_buf.resize(out_frames * CHANNELS as usize, 0.0);

        let start = Instant::now();
        stretch.process(&in_buf, out_buf.as_mut_slice());
        total_process_time += start.elapsed();
        chunks += 1;

        output.extend_from_slice(&out_buf);
    }

    // Drain the pipeline's internal delay so the tail of the tone isn't lost.
    let mut flush_buf = vec![0.0_f32; stretch.output_latency() * CHANNELS as usize];
    stretch.flush(flush_buf.as_mut_slice());
    output.extend_from_slice(&flush_buf);

    println!(
        "{chunks} chunks, {:.3}ms total process() time ({:.4}ms/chunk avg) for {:.1}s of audio",
        total_process_time.as_secs_f64() * 1000.0,
        total_process_time.as_secs_f64() * 1000.0 / chunks as f64,
        DURATION_SECS,
    );

    write_wav(&out_path, &output, SAMPLE_RATE, CHANNELS as u16);
    println!("wrote {out_path} ({} frames)", output.len() / CHANNELS as usize);
}

/// 440Hz + 880Hz stereo tone (left/right slightly detuned), so both pitch shift and
/// stereo image survive stretching are audible on playback.
fn test_tone() -> Vec<f32> {
    let frames = (SAMPLE_RATE * DURATION_SECS) as usize;
    let mut samples = Vec::with_capacity(frames * CHANNELS as usize);
    for i in 0..frames {
        let t = i as f32 / SAMPLE_RATE as f32;
        let left = 0.2 * (2.0 * PI * 440.0 * t).sin() + 0.1 * (2.0 * PI * 880.0 * t).sin();
        let right = 0.2 * (2.0 * PI * 441.0 * t).sin() + 0.1 * (2.0 * PI * 879.0 * t).sin();
        samples.push(left);
        samples.push(right);
    }
    samples
}

/// Minimal 16-bit PCM WAV writer — not worth a dependency for one throwaway harness.
fn write_wav(path: &str, samples: &[f32], sample_rate: u32, channels: u16) {
    let mut w = BufWriter::new(File::create(path).expect("create wav"));
    let data_bytes = (samples.len() * 2) as u32;
    let byte_rate = sample_rate * channels as u32 * 2;
    let block_align = channels * 2;

    w.write_all(b"RIFF").unwrap();
    w.write_all(&(36 + data_bytes).to_le_bytes()).unwrap();
    w.write_all(b"WAVE").unwrap();
    w.write_all(b"fmt ").unwrap();
    w.write_all(&16u32.to_le_bytes()).unwrap();
    w.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
    w.write_all(&channels.to_le_bytes()).unwrap();
    w.write_all(&sample_rate.to_le_bytes()).unwrap();
    w.write_all(&byte_rate.to_le_bytes()).unwrap();
    w.write_all(&block_align.to_le_bytes()).unwrap();
    w.write_all(&16u16.to_le_bytes()).unwrap(); // bits per sample
    w.write_all(b"data").unwrap();
    w.write_all(&data_bytes.to_le_bytes()).unwrap();

    for &s in samples {
        let clamped = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        w.write_all(&clamped.to_le_bytes()).unwrap();
    }
}
