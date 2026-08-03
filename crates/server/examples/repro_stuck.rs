use lavalink_server::audio::stream::HttpMediaSource;
use symphonia::core::audio::{Audio as _, GenericAudioBufferRef};
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use std::time::Instant;

fn main() {
    let url = std::env::args().nth(1).expect("url");
    let source = HttpMediaSource::open(&url, None, None).expect("open");

    let stream = MediaSourceStream::new(Box::new(source), Default::default());
    let mut hint = Hint::new();
    hint.with_extension("m4a");
    let mut format = symphonia::default::get_probe()
        .probe(&hint, stream, FormatOptions::default(), MetadataOptions::default())
        .expect("probe");

    let (track_id, params) = format.tracks().iter().find_map(|t| match &t.codec_params {
        Some(CodecParameters::Audio(p)) => Some((t.id, p.clone())),
        _ => None,
    }).expect("audio track");

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .expect("decoder");

    let start = Instant::now();
    let mut packet_count = 0u64;
    let mut nonempty = 0u64;
    let mut empty = 0u64;
    loop {
        if start.elapsed().as_secs() > 8 { break; }
        match format.next_packet() {
            Ok(Some(packet)) => {
                packet_count += 1;
                if packet.track_id != track_id { continue; }
                match decoder.decode(&packet) {
                    Ok(decoded) => {
                        let generic: GenericAudioBufferRef = decoded;
                        let (frames, channels, variant) = match &generic {
                            GenericAudioBufferRef::F32(b) => (b.frames(), b.spec().channels().count(), "F32"),
                            GenericAudioBufferRef::F64(b) => (b.frames(), b.spec().channels().count(), "F64"),
                            GenericAudioBufferRef::S32(b) => (b.frames(), b.spec().channels().count(), "S32"),
                            GenericAudioBufferRef::S24(b) => (b.frames(), b.spec().channels().count(), "S24"),
                            GenericAudioBufferRef::S16(b) => (b.frames(), b.spec().channels().count(), "S16"),
                            GenericAudioBufferRef::S8(b) => (b.frames(), b.spec().channels().count(), "S8"),
                            GenericAudioBufferRef::U32(b) => (b.frames(), b.spec().channels().count(), "U32"),
                            GenericAudioBufferRef::U24(b) => (b.frames(), b.spec().channels().count(), "U24"),
                            GenericAudioBufferRef::U16(b) => (b.frames(), b.spec().channels().count(), "U16"),
                            GenericAudioBufferRef::U8(b) => (b.frames(), b.spec().channels().count(), "U8"),
                        };
                        if packet_count <= 5 {
                            eprintln!("packet {packet_count}: variant={variant} frames={frames} channels={channels}");
                        }
                        if frames == 0 { empty += 1; } else { nonempty += 1; }
                    }
                    Err(e) => eprintln!("decode error: {e}"),
                }
            }
            Ok(None) => break,
            Err(SymphoniaError::ResetRequired) => {}
            Err(e) => { eprintln!("next_packet error: {e}"); break; }
        }
    }
    eprintln!("packets={packet_count} nonempty_buffers={nonempty} empty_buffers={empty}");
}
