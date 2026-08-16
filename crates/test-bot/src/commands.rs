//! Slash command registration and typed option lookup.
//!
//! `Handler::dispatch` (in `main.rs`) is what runs a command; this module is only
//! what declares one to Discord and reads its arguments back out.

use serenity::all::{CommandOptionType, CreateCommand, CreateCommandOption, ResolvedOption, ResolvedValue};

/// Every slash command this bot registers, guild-scoped in `ready`.
pub fn commands() -> Vec<CreateCommand> {
    use CommandOptionType::{Integer, Number, String as Str};

    vec![
        CreateCommand::new("join").description("move the bot into your voice channel"),
        CreateCommand::new("leave").description("destroy the player and leave"),
        CreateCommand::new("play").description("load and play a track").add_option(
            CreateCommandOption::new(Str, "query", "url, ytsearch:…, scsearch:…, or a local path")
                .required(true),
        ),
        CreateCommand::new("search").description("load without playing").add_option(
            CreateCommandOption::new(Str, "query", "same syntax as /play").required(true),
        ),
        CreateCommand::new("stop").description("stop the current track"),
        CreateCommand::new("pause").description("pause playback"),
        CreateCommand::new("resume").description("resume playback"),
        CreateCommand::new("seek").description("seek to a position").add_option(
            CreateCommandOption::new(Number, "seconds", "position in seconds").required(true),
        ),
        CreateCommand::new("volume").description("set player volume").add_option(
            CreateCommandOption::new(Integer, "amount", "0-1000")
                .required(true)
                .min_int_value(0)
                .max_int_value(1000),
        ),
        CreateCommand::new("np").description("now playing"),
        CreateCommand::new("players").description("node-wide player list"),
        CreateCommand::new("eq")
            .description("set an equalizer band")
            .add_option(
                CreateCommandOption::new(Integer, "band", "0-14")
                    .required(true)
                    .min_int_value(0)
                    .max_int_value(14),
            )
            .add_option(CreateCommandOption::new(Number, "gain", "-0.25 to 1.0").required(true)),
        CreateCommand::new("lowpass").description("set the low-pass filter").add_option(
            CreateCommandOption::new(Number, "smoothing", "smoothing factor").required(true),
        ),
        CreateCommand::new("karaoke")
            .description("set karaoke (vocal removal) level")
            .add_option(
                CreateCommandOption::new(Number, "level", "0 = normal, 1 = max vocal removal")
                    .required(true),
            ),
        CreateCommand::new("timescale")
            .description("set speed/pitch/rate (each defaults to 1.0 if omitted)")
            .add_option(CreateCommandOption::new(Number, "speed", "1.0 = normal"))
            .add_option(CreateCommandOption::new(Number, "pitch", "1.0 = normal"))
            .add_option(CreateCommandOption::new(Number, "rate", "1.0 = normal")),
        CreateCommand::new("tremolo")
            .description("set tremolo (amplitude wobble)")
            .add_option(CreateCommandOption::new(Number, "frequency", "Hz, > 0").required(true))
            .add_option(CreateCommandOption::new(Number, "depth", "0 to 1").required(true)),
        CreateCommand::new("vibrato")
            .description("set vibrato (pitch wobble)")
            .add_option(CreateCommandOption::new(Number, "frequency", "0 to 14 Hz").required(true))
            .add_option(CreateCommandOption::new(Number, "depth", "0 to 1").required(true)),
        CreateCommand::new("rotation").description("set 8D audio rotation speed").add_option(
            CreateCommandOption::new(Number, "hz", "rotation speed, e.g. 0.2").required(true),
        ),
        CreateCommand::new("distortion").description("set distortion amount").add_option(
            CreateCommandOption::new(Number, "scale", "1.0 = clean, higher = more distorted")
                .required(true),
        ),
        CreateCommand::new("channelmix")
            .description("crossfeed between left/right channels")
            .add_option(
                CreateCommandOption::new(Number, "crossfeed", "0 = normal stereo, 1 = fully swapped")
                    .required(true),
            ),
        CreateCommand::new("clearfilters").description("clear all filters"),
        CreateCommand::new("filters").description("show the current filter chain"),
        CreateCommand::new("ping").description("gateway latency"),
        CreateCommand::new("info").description("node version and capabilities"),
        CreateCommand::new("stats").description("node-wide stats"),
    ]
}

pub fn opt_str<'a>(options: &[ResolvedOption<'a>], name: &str) -> Option<&'a str> {
    options.iter().find(|o| o.name == name).and_then(|o| match o.value {
        ResolvedValue::String(s) => Some(s),
        _ => None,
    })
}

pub fn opt_i64(options: &[ResolvedOption<'_>], name: &str) -> Option<i64> {
    options.iter().find(|o| o.name == name).and_then(|o| match o.value {
        ResolvedValue::Integer(v) => Some(v),
        _ => None,
    })
}

pub fn opt_f64(options: &[ResolvedOption<'_>], name: &str) -> Option<f64> {
    options.iter().find(|o| o.name == name).and_then(|o| match o.value {
        ResolvedValue::Number(v) => Some(v),
        _ => None,
    })
}
