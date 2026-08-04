//! A Discord bot whose only purpose is to exercise this node against real Discord.
//!
//! The node has never completed a voice handshake — everything up to the point where
//! bytes leave for Discord is tested, and nothing past it. This bot closes that loop:
//! it is a v4 client like Wavelink or Lavalink.py, driving the node over REST and
//! reading back what the node sends over its websocket.
//!
//! # Why songbird is here again, and differently
//!
//! The server links songbird's `driver` and no gateway. This links its `gateway` and
//! no driver — the two halves of the same library on opposite sides of the same
//! connection. That is the actual Lavalink architecture: the *client* asks Discord to
//! move the bot into a voice channel and receives the credentials, then hands them to
//! the *node*, which is what actually speaks voice. Nothing here touches audio.

mod node;
mod node_ws;

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use lavalink_protocol::filters::{Band, Filters, LowPass};
use lavalink_protocol::player::{Player, PlayerUpdate, PlayerUpdateTrack, VoiceState};
use lavalink_protocol::{LoadResult, Omissible, Track};
use serenity::all::{
    CommandInteraction, CommandOptionType, Context, CreateCommand, CreateCommandOption,
    EditInteractionResponse, EventHandler, GatewayIntents, GuildId, Interaction, Ready,
    ResolvedOption, ResolvedValue, ShardManager,
};
use serenity::{async_trait, Client};
use songbird::{SerenityInit, Songbird};
use tokio::sync::Mutex;

use node::{Node, NodeError};

struct Handler {
    node: Node,
    songbird: Arc<Songbird>,
    /// The guild slash commands are registered to — guild-scoped registration is
    /// live immediately, unlike global commands, which can take up to an hour to
    /// propagate. Fine for a bot that only ever runs against one test server.
    guild: GuildId,
    /// Filters are cumulative on the wire — a `PATCH` carrying only `equalizer`
    /// replaces the whole chain — so the last state sent is kept per guild and
    /// re-sent in full. Without this, `/lowpass` would silently undo `/eq`.
    filters: Mutex<HashMap<u64, Filters>>,
    /// Set once `main` has the built [`Client`] in hand — it does not exist yet when
    /// `Handler` is constructed. `/ping` reads the shard's heartbeat latency from it.
    shard_manager: Arc<OnceLock<Arc<ShardManager>>>,
    /// `ready` fires on every IDENTIFY, not just the first — a failed session resume
    /// re-triggers it. Without this guard, each re-identify would spawn another
    /// websocket task, register another session on the node, and re-register the
    /// same slash commands.
    ws_started: OnceLock<()>,
}

#[async_trait]
impl EventHandler for Handler {
    /// The node websocket cannot open before this point: it needs the bot's user id
    /// as a header, and that is not known until Discord says so.
    async fn ready(&self, ctx: Context, ready: Ready) {
        let user_id = ready.user.id.get();
        tracing::info!(user = %ready.user.name, user_id, "logged in");

        if self.ws_started.set(()).is_err() {
            tracing::debug!("re-identify — node websocket task already running");
            return;
        }

        if let Err(error) = self.guild.set_commands(&ctx.http, commands()).await {
            tracing::error!(%error, "failed to register slash commands");
        }

        let session = self.node.session_slot();
        let (host, password) = (self.node.host.clone(), self.node.password.clone());
        tokio::spawn(node_ws::run(host, password, user_id, session));
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let Interaction::Command(command) = interaction else {
            return;
        };

        // Node REST calls can outlast the 3s Discord gives an initial response, so
        // acknowledge first and deliver the real answer as an edit.
        if let Err(error) = command.defer(&ctx.http).await {
            tracing::warn!(%error, "failed to defer interaction");
            return;
        }

        let reply = self
            .dispatch(&ctx, &command)
            .await
            .unwrap_or_else(|error| format!("`{error}`"));

        if let Err(error) =
            command.edit_response(&ctx.http, EditInteractionResponse::new().content(reply)).await
        {
            tracing::warn!(%error, "failed to edit interaction response");
        }
    }
}

impl Handler {
    async fn dispatch(&self, ctx: &Context, command: &CommandInteraction) -> Result<String, NodeError> {
        let Some(guild_id) = command.guild_id else {
            return Ok("guild channels only".into());
        };
        let guild = guild_id.get();
        let options = command.data.options();

        match command.data.name.as_str() {
            "ping" => Ok(self.gateway_latency(ctx).await),
            "info" => {
                let info = self.node.info().await?;
                Ok(format!(
                    "version `{}`, sources {:?}, filters {:?}",
                    info.version.semver, info.source_managers, info.filters
                ))
            }
            "stats" => {
                let stats = self.node.stats().await?;
                Ok(format!(
                    "players {} ({} playing), uptime {}ms, mem used {}B",
                    stats.players, stats.playing_players, stats.uptime, stats.memory.used
                ))
            }
            "join" => self.join(ctx, command, guild).await,
            "leave" => self.leave(guild).await,
            "play" => self.play(guild, opt_str(&options, "query").unwrap_or_default()).await,
            "search" => self.search(opt_str(&options, "query").unwrap_or_default()).await,
            "stop" => {
                // `Present(None)` on the track is the stop signal — an omitted track
                // would mean "leave whatever is playing alone".
                self.patch_with(guild, |u| {
                    u.track = Omissible::Present(PlayerUpdateTrack {
                        encoded: Omissible::Present(None),
                        ..Default::default()
                    });
                })
                .await?;
                Ok("stopped".into())
            }
            name @ ("pause" | "resume") => {
                let paused = name == "pause";
                self.patch_with(guild, |u| u.paused = Omissible::Present(paused)).await?;
                Ok(if paused { "paused".into() } else { "resumed".into() })
            }
            "seek" => {
                let seconds = opt_f64(&options, "seconds").ok_or_else(|| bad("seek <seconds>"))?;
                self.patch_with(guild, |u| u.position = Omissible::Present((seconds * 1000.0) as i64))
                    .await?;
                Ok(format!("seeked to {seconds}s"))
            }
            "volume" => {
                let volume =
                    opt_i64(&options, "amount").ok_or_else(|| bad("volume <0-1000>"))? as i32;
                self.patch_with(guild, |u| u.volume = Omissible::Present(volume)).await?;
                Ok(format!("volume {volume}"))
            }
            "np" => {
                let player = self.node.player(guild).await?;
                Ok(match player.track {
                    Some(track) => format!(
                        "**{}** — {}\n`{}ms / {}ms` paused={} connected={} ping={}",
                        track.info.title,
                        track.info.author,
                        player.state.position,
                        track.info.length,
                        player.paused,
                        player.state.connected,
                        player.state.ping,
                    ),
                    None => "nothing playing".into(),
                })
            }
            // The node-wide view, which is what the ten-player resource measurement
            // needs — `/np` only ever describes the guild it was typed in.
            "players" => {
                let players = self.node.players().await?;
                if players.0.is_empty() {
                    return Ok("no players".into());
                }
                let lines = players
                    .0
                    .iter()
                    .map(|player| {
                        format!(
                            "`{}` {} pos={}ms connected={}",
                            player.guild_id,
                            player.track.as_ref().map_or("—", |t| t.info.title.as_str()),
                            player.state.position,
                            player.state.connected,
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(format!("{} players\n{lines}", players.0.len()))
            }
            "eq" => {
                let usage = || bad("eq <band> <gain>");
                let band = opt_i64(&options, "band").ok_or_else(usage)? as i32;
                let gain = opt_f64(&options, "gain").ok_or_else(usage)? as f32;
                self.apply_filters(guild, |filters| {
                    // Replacing an existing entry rather than appending: the node
                    // applies the last value for a band, but sending duplicates
                    // makes the echoed `filters` object unreadable while testing.
                    let mut bands = match filters.equalizer.clone() {
                        Omissible::Present(bands) => bands,
                        Omissible::Omitted => Vec::new(),
                    };
                    match bands.iter_mut().find(|entry| entry.band == band) {
                        Some(entry) => entry.gain = gain,
                        None => bands.push(Band { band, gain }),
                    }
                    filters.equalizer = Omissible::Present(bands);
                })
                .await
            }
            "lowpass" => {
                let smoothing =
                    opt_f64(&options, "smoothing").ok_or_else(|| bad("lowpass <smoothing>"))? as f32;
                self.apply_filters(guild, |filters| {
                    filters.low_pass = Omissible::Present(Some(LowPass { smoothing }));
                })
                .await
            }
            "clearfilters" => self.apply_filters(guild, |filters| *filters = Filters::default()).await,
            "filters" => {
                let player = self.node.player(guild).await?;
                Ok(format!("```json\n{}\n```", pretty(&player.filters)))
            }
            name => Ok(format!("unknown command `{name}`")),
        }
    }

    /// Moves the bot into the caller's voice channel and hands the resulting
    /// credentials to the node.
    ///
    /// The two steps are the whole architecture in miniature: songbird performs the
    /// gateway op4 and waits for Discord's voice state + voice server updates, and
    /// the node is then *told* the result. It never talks to the gateway itself.
    async fn join(
        &self,
        ctx: &Context,
        command: &CommandInteraction,
        guild: u64,
    ) -> Result<String, NodeError> {
        let channel = ctx
            .cache
            .guild(guild)
            .and_then(|g| {
                g.voice_states
                    .get(&command.user.id)
                    .and_then(|state| state.channel_id)
            })
            .ok_or_else(|| bad("join a voice channel first"))?;

        let (info, _call) = self
            .songbird
            .join_gateway(GuildId::new(guild), channel)
            .await
            .map_err(|error| bad(format!("gateway join failed: {error}")))?;

        self.patch_with(guild, |u| {
            u.voice = Omissible::Present(VoiceState {
                token: info.token,
                endpoint: info.endpoint,
                session_id: info.session_id,
                channel_id: Some(channel.get().to_string()),
            });
        })
        .await?;

        Ok(format!("joined <#{channel}> — node has the voice credentials"))
    }

    /// Destroys the player before leaving, in that order.
    ///
    /// The reverse leaves the node holding a voice connection Discord has already
    /// torn down, which surfaces as a `WebSocketClosedEvent` the client never asked
    /// for — worth avoiding here so that any such event during testing is real.
    async fn leave(&self, guild: u64) -> Result<String, NodeError> {
        self.node.destroy_player(guild).await?;
        self.filters.lock().await.remove(&guild);
        let _ = self.songbird.leave(GuildId::new(guild)).await;
        Ok("left".into())
    }

    async fn play(&self, guild: u64, identifier: &str) -> Result<String, NodeError> {
        if identifier.is_empty() {
            return Err(bad("play <url | ytsearch:query | scsearch:query>"));
        }

        let (track, note) = match self.node.load_tracks(identifier).await? {
            LoadResult::Track(track) => (*track, String::new()),
            LoadResult::Search(mut tracks) if !tracks.is_empty() => {
                (tracks.remove(0), format!(" (1 of {} matches)", tracks.len() + 1))
            }
            LoadResult::Playlist(playlist) => {
                // `selectedTrack` is -1 when the playlist names no entry point, and
                // an out-of-range index is a node bug rather than something to
                // paper over — so it is clamped only for the negative case.
                let index = usize::try_from(playlist.info.selected_track).unwrap_or(0);
                let count = playlist.tracks.len();
                let track = playlist
                    .tracks
                    .into_iter()
                    .nth(index)
                    .ok_or_else(|| bad("playlist had no playable track"))?;
                (track, format!(" (from playlist **{}**, {count} tracks)", playlist.info.name))
            }
            LoadResult::Search(_) | LoadResult::Empty => return Ok("no matches".into()),
            LoadResult::Error(exception) => {
                return Ok(format!(
                    "load failed ({:?}): {}",
                    exception.severity,
                    exception.message.unwrap_or(exception.cause)
                ))
            }
        };

        let summary = describe(&track);
        self.patch_with(guild, |u| {
            u.track = Omissible::Present(PlayerUpdateTrack {
                encoded: Omissible::Present(Some(track.encoded)),
                ..Default::default()
            });
        })
        .await?;

        Ok(format!("playing {summary}{note}"))
    }

    /// Loads without playing, so a source can be checked before a voice channel is
    /// involved — which is most of what `loadtracks` verification needs.
    async fn search(&self, identifier: &str) -> Result<String, NodeError> {
        let result = self.node.load_tracks(identifier).await?;
        Ok(match result {
            LoadResult::Track(track) => format!("`track` — {}", describe(&track)),
            LoadResult::Search(tracks) => {
                format!("`search` — {} results\n{}", tracks.len(), summarize(tracks.iter()))
            }
            LoadResult::Playlist(playlist) => format!(
                "`playlist` — **{}** ({} tracks, selected {})\n{}",
                playlist.info.name,
                playlist.tracks.len(),
                playlist.info.selected_track,
                summarize(playlist.tracks.iter()),
            ),
            LoadResult::Empty => "`empty`".into(),
            LoadResult::Error(exception) => format!("`error` — {exception:?}"),
        })
    }

    /// Mutates the remembered filter chain for a guild and re-sends all of it — a
    /// `PATCH` carrying only one filter key replaces the whole chain server-side.
    async fn apply_filters(
        &self,
        guild: u64,
        f: impl FnOnce(&mut Filters),
    ) -> Result<String, NodeError> {
        let filters = {
            let mut cache = self.filters.lock().await;
            let filters = cache.entry(guild).or_default();
            f(filters);
            filters.clone()
        };

        let player = self.patch_with(guild, |u| u.filters = Omissible::Present(filters)).await?;
        Ok(format!("```json\n{}\n```", pretty(&player.filters)))
    }

    async fn patch_with(&self, guild: u64, f: impl FnOnce(&mut PlayerUpdate)) -> Result<Player, NodeError> {
        let mut update = PlayerUpdate::default();
        f(&mut update);
        self.node.update_player(guild, &update, false).await
    }

    /// The Discord gateway heartbeat latency for the shard this interaction arrived
    /// on — the same number the client logs on every `Ready`/`Resumed`, surfaced on
    /// demand.
    async fn gateway_latency(&self, ctx: &Context) -> String {
        let Some(shard_manager) = self.shard_manager.get().cloned() else {
            return "pong! (shard manager not ready yet)".into();
        };
        let latency = shard_manager
            .runners
            .lock()
            .await
            .get(&ctx.shard_id)
            .and_then(|runner| runner.latency);

        match latency {
            Some(latency) => format!("pong! gateway latency {}ms", latency.as_millis()),
            // Not yet measured — the first heartbeat ack has not come back.
            None => "pong! (gateway latency not measured yet)".into(),
        }
    }
}

/// Every slash command this bot registers, guild-scoped in `ready`.
fn commands() -> Vec<CreateCommand> {
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

fn opt_str<'a>(options: &[ResolvedOption<'a>], name: &str) -> Option<&'a str> {
    options.iter().find(|o| o.name == name).and_then(|o| match o.value {
        ResolvedValue::String(s) => Some(s),
        _ => None,
    })
}

fn opt_i64(options: &[ResolvedOption<'_>], name: &str) -> Option<i64> {
    options.iter().find(|o| o.name == name).and_then(|o| match o.value {
        ResolvedValue::Integer(v) => Some(v),
        _ => None,
    })
}

fn opt_f64(options: &[ResolvedOption<'_>], name: &str) -> Option<f64> {
    options.iter().find(|o| o.name == name).and_then(|o| match o.value {
        ResolvedValue::Number(v) => Some(v),
        _ => None,
    })
}

fn describe(track: &Track) -> String {
    format!(
        "**{}** — {} `[{}] {}ms`",
        track.info.title, track.info.author, track.info.source_name, track.info.length
    )
}

fn summarize<'a>(tracks: impl Iterator<Item = &'a Track>) -> String {
    tracks.take(5).map(describe).collect::<Vec<_>>().join("\n")
}

fn pretty(filters: &Filters) -> String {
    serde_json::to_string_pretty(filters).unwrap_or_else(|error| error.to_string())
}

/// A user mistake, shaped like a node error so the dispatch table has one error type.
fn bad(message: impl Into<String>) -> NodeError {
    NodeError::Usage(message.into())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Loads `.env` from the current directory if present; a real env var always
    // wins over one set there. Missing entirely is fine — just falls through to
    // `DISCORD_TOKEN` below, which reports the actual problem.
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,lavalink_test_bot=debug".into()),
        )
        .init();

    let token = std::env::var("DISCORD_TOKEN")
        .map_err(|_| "DISCORD_TOKEN is not set — see crates/test-bot/README.md")?;
    let guild = std::env::var("TEST_GUILD_ID")
        .map_err(|_| "TEST_GUILD_ID is not set — see crates/test-bot/README.md")?
        .parse::<u64>()
        .map_err(|_| "TEST_GUILD_ID must be a guild id (a number)")?;
    let host = std::env::var("LAVALINK_HOST").unwrap_or_else(|_| "localhost:2333".into());
    let password = std::env::var("LAVALINK_PASSWORD").unwrap_or_else(|_| "youshallnotpass".into());

    let songbird = Songbird::serenity();
    // `Handler` is moved into the builder below, before the `Client` (and its
    // `shard_manager`) exists — shared so it can be filled in afterwards.
    let shard_manager_slot = Arc::new(OnceLock::new());
    let handler = Handler {
        node: Node::new(&host, &password),
        songbird: Arc::clone(&songbird),
        guild: GuildId::new(guild),
        filters: Mutex::new(HashMap::new()),
        shard_manager: Arc::clone(&shard_manager_slot),
        ws_started: OnceLock::new(),
    };

    // `GUILD_VOICE_STATES` is what makes the caller's current voice channel
    // knowable. Slash commands need no message-content access, so unlike the old
    // `!`-prefix commands, nothing here requires the privileged `MESSAGE_CONTENT`
    // intent.
    let intents = GatewayIntents::GUILDS | GatewayIntents::GUILD_VOICE_STATES;

    let mut client = Client::builder(&token, intents)
        .event_handler(handler)
        .register_songbird_with(songbird)
        .await?;
    let _ = shard_manager_slot.set(Arc::clone(&client.shard_manager));

    tracing::info!(%host, "starting; node websocket opens once Discord confirms login");
    client.start().await?;
    Ok(())
}
