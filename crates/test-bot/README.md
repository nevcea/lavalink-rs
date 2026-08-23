# lavalink-test-bot

A development-only Discord bot for testing voice handshakes, audible playback,
and websocket behavior that unit tests cannot cover.

It behaves like a Lavalink v4 client: REST loads and player updates, Discord
voice credentials forwarded to the node, and every node websocket message
logged. The bot never processes audio itself.

## How it fits together

```text
Discord gateway ── voice state/server updates ──▶ test bot
                                                      │
                                               REST `voice` update
                                                      │
                                                      ▼
Discord voice ◀──────────── RTP/Opus ─────────── Lavalink node
                                                      │
test bot ◀──────────── websocket events ──────────────┘
```

The bot collects Discord's `token`, `endpoint`, and `sessionId`, then sends them
to `PATCH /v4/sessions/{sessionId}/players/{guildId}`. The node owns the voice
connection from that point.

## Prerequisites

- A running lavalink-rs node and its password
- A Discord application with a bot user
- A private Discord server with a voice channel
- The same native build tools required by the workspace; see the
  [root README](../../README.md#requirements)

Use a dedicated token and private guild: the bot registers development
commands directly in that guild and logs node events.

## Discord setup

1. Create an application at the
   [Discord Developer Portal](https://discord.com/developers/applications),
   then add a bot user.
2. Copy or reset the bot token and store it only in the environment or a local
   `.env`.
3. Invite the bot with the `bot` and `applications.commands` scopes. Grant at
   least View Channel, Connect, and Speak in the test voice channel.
4. Enable Discord Developer Mode, right-click the test server, and copy its ID.
   This is `TEST_GUILD_ID`.

No privileged gateway intents are required. Guild commands are registered when
the bot becomes ready and normally appear immediately.

## Environment

The binary loads `.env` from the current directory, then reads:

| Variable | Required | Default | Purpose |
|---|---:|---|---|
| `DISCORD_TOKEN` | yes | — | Discord bot token. |
| `TEST_GUILD_ID` | yes | — | Guild where slash commands are registered. |
| `LAVALINK_HOST` | no | `localhost:2333` | Node host and port, without an `http://` prefix. |
| `LAVALINK_PASSWORD` | no | `youshallnotpass` | Value sent in the node's `Authorization` header. |
| `RUST_LOG` | no | `info,lavalink_test_bot=debug` | Log filter. |

A repo-root `.env` for local development can look like this:

```dotenv
DISCORD_TOKEN=replace-me
TEST_GUILD_ID=123456789012345678
LAVALINK_HOST=localhost:2333
LAVALINK_PASSWORD=youshallnotpass
RUST_LOG=info
```

The root `.gitignore` excludes `.env` and `application.yml`.

## Running both processes

From the repository root, create `application.yml` and enable the source under
test. For example, `local: true` permits file paths; `youtube: true` permits
YouTube URLs and searches when `yt-dlp` is available.

On Bash, the helper starts a release node, waits for its TCP port, and runs the
bot:

```sh
scripts/dev.sh
```

After both binaries have been built, restart without rebuilding:

```bash
scripts/dev.sh --no-build
```

The helper reads the repo-root `.env`; Ctrl+C stops both processes. It expects
`LAVALINK_HOST` as `host:port` and requires Bash's `/dev/tcp`. Use the manual
workflow on other shells.

## Running each process manually

Terminal 1:

```sh
cargo run -p lavalink-server -- application.yml
```

Terminal 2 on Bash:

```sh
export DISCORD_TOKEN='…'
export TEST_GUILD_ID='…'
cargo run -p lavalink-test-bot
```

If `.env` contains the required values, the second terminal only needs
`cargo run -p lavalink-test-bot` from the repository root.

Startup is ready when the bot logs into Discord, connects to the node websocket,
and receives a session ID. The bot enables a 60-second resume window; after a
brief disconnect, the next ready message should show `resumed=true` with the
same ID. Commands issued earlier may fail with `no session yet`.

## Commands

### Voice and playback

| Command | What it exercises |
|---|---|
| `/join` | Moves the bot to the caller's voice channel and sends the voice credentials to the node. |
| `/leave` | Deletes the guild player and leaves voice. |
| `/play <query>` | Loads the first result and starts it. Accepts a URL, `ytsearch:…`, `scsearch:…`, or a local path. |
| `/search <query>` | Loads and displays a result without requiring a voice connection. |
| `/stop` | Stops the current track. |
| `/pause`, `/resume` | Toggles the player's paused state. |
| `/seek <seconds>` | Seeks to an absolute position. |
| `/volume <0-1000>` | Sets the Lavalink player volume. |
| `/np` | Shows this guild's current track and player state. |
| `/players` | Lists every player in the current node session. |

Use `/search` to isolate source loading from Discord voice before debugging
`/join` or playback.

### Filters

| Command | Main input |
|---|---|
| `/eq <band> <gain>` | Band `0-14`, gain `-0.25` to `1.0`. |
| `/lowpass <smoothing>` | Low-pass smoothing value. |
| `/karaoke <level>` | `0` is unchanged; `1` is maximum configured vocal removal. |
| `/timescale [speed] [pitch] [rate]` | Each value defaults to `1.0`. |
| `/tremolo <frequency> <depth>` | Frequency in Hz and depth `0-1`. |
| `/vibrato <frequency> <depth>` | Frequency `0-14` Hz and depth `0-1`. |
| `/rotation <hz>` | Stereo rotation speed. |
| `/distortion <scale>` | `1.0` is the command's clean baseline; larger values increase the effect. |
| `/channelmix <crossfeed>` | `0` keeps normal stereo; `1` swaps the channels. |
| `/filters` | Shows the filter state returned by the node. |
| `/clearfilters` | Clears the complete filter chain. |

Filters are cumulative: because Lavalink replaces the full filter object on
each update, the bot retains and resends the guild's current chain.

### Diagnostics

| Command | Output |
|---|---|
| `/ping` | Discord gateway heartbeat latency. It does not measure node REST or voice latency. |
| `/info` | Node version, advertised sources, and filters. |
| `/stats` | Node player count, uptime, and memory counters. |

## Recommended smoke test

1. Run `/info` and confirm the source and filters under test are advertised.
2. Run `/search <query>` and confirm loading returns the expected track.
3. Join a voice channel and run `/join`; watch for `connected=true` and a real
   ping in a `playerUpdate`.
4. Run `/play <query>`; listen for clean audio and confirm position increases
   in subsequent `playerUpdate` messages.
5. Run `/pause`, `/resume`, `/seek`, and `/volume`, checking both the audible
   result and `/np`.
6. Apply the changed filters one at a time, inspect `/filters`, then use
   `/clearfilters`.
7. Run `/play` again while a track is active, then `/stop` and `/leave`, and
   verify the event order below.
8. Interrupt the node websocket for less than 60 seconds and verify the bot
   reconnects with `resumed=true` without losing the player.

Repeat audio checks with the changed source, codec, or container. For
concurrency work, use multiple guilds or bot instances; one guild cannot expose
cross-player stalls.

## Expected websocket events

The bot logs every node websocket text message. Useful checkpoints are:

- `/join` produces a `playerUpdate` with `connected=true` and a non-negative
  voice ping.
- `/play` produces `TrackStartEvent`, followed by updates with a rising
  `position`.
- Replacing a track produces `TrackEndEvent reason=Replaced` before the next
  `TrackStartEvent`.
- `/stop` produces `TrackEndEvent reason=Stopped`.
- Natural completion produces `TrackEndEvent reason=Finished` with
  `may_start_next=true`.
- Removing the bot from the voice channel produces `WebSocketClosedEvent`
  with code `4014`.

Unparseable node messages are logged at `error` with their raw text and indicate
a protocol mismatch or node defect.

## Troubleshooting

| Symptom | Check |
|---|---|
| Commands do not appear | Confirm `TEST_GUILD_ID`, the `applications.commands` invite scope, and the command-registration error in the bot log. |
| `/join` says to join voice first | The command uses the caller's current cached voice state; join the channel, wait briefly, and retry. |
| `/join` fails with permissions | Grant the bot View Channel, Connect, and Speak in that channel. |
| Node returns 401/403 | Match `LAVALINK_PASSWORD` to `lavalink.server.password`; do not include a URL scheme in `LAVALINK_HOST`. |
| Source is absent from `/info` | Enable it in `application.yml`; for YouTube, SoundCloud, Bandcamp, or Deezer, also verify `yt-dlp --version`. Restart after changing sources. |
| `/search` works but `/play` does not | Check that `/join` reached `connected=true`, then inspect node logs for stream/decoder errors. |
| Player position rises but nothing is audible | Check Discord output device/volume, server mute/deafen state, bot Speak permission, and node voice logs. |
| Bot connects before the node | Leave it running; the websocket task retries. Commands need a ready node session before they can use REST player routes. |

The bot enables its own debug logs by default. If node logs are insufficient,
run the node with `RUST_LOG=info,lavalink_server=debug`.
