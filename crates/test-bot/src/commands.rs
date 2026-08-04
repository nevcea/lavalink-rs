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
