# lavalink-test-bot

A development-only Discord bot for answering the questions unit tests cannot:
does the node complete a real Discord voice handshake, does audio arrive, and
does the result sound right?

The bot is an ordinary Lavalink v4 client. It loads tracks and updates players
through REST, forwards Discord voice credentials to the node, and logs every
node websocket message. It does not decode, mix, or transmit audio itself.

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

The bot uses Songbird's gateway half to request a voice-channel move and
correlate Discord's `token`, `endpoint`, and `sessionId`. It sends that triple
to `PATCH /v4/sessions/{sessionId}/players/{guildId}`. The server uses
Songbird's driver half and owns the voice connection from then on.

## Prerequisites

- A running lavalink-rs node and its password
- A Discord application with a bot user
- A private Discord server where you can invite the bot and join a voice
  channel
- The same native build tools required by the workspace; see the
  [root README](../../README.md#requirements)

Do not use a production bot token or public guild for routine testing. The bot
registers development commands directly in one guild and logs node events.

## Discord setup

1. Create an application at the
   [Discord Developer Portal](https://discord.com/developers/applications),
   then add a bot user.
2. Copy or reset the bot token. Store it only in your environment or a local
   `.env`; never commit it.
3. Invite the bot with the `bot` and `applications.commands` scopes. Grant at
   least View Channel, Connect, and Speak in the test voice channel.
4. Enable Discord Developer Mode, right-click the test server, and copy its ID.
   This is `TEST_GUILD_ID`.

No privileged gateway intents are needed. Commands are registered to the test
guild when the bot becomes ready, so they normally appear immediately.

## Environment

The binary loads a `.env` from the current working directory before reading
these variables:

| Variable | Required | Default | Purpose |
|---|---:|---|---|
| `DISCORD_TOKEN` | yes | — | Discord bot token. |
| `TEST_GUILD_ID` | yes | — | Guild where slash commands are registered. |
| `LAVALINK_HOST` | no | `localhost:2333` | Node host and port, without an `http://` prefix. |
| `LAVALINK_PASSWORD` | no | `youshallnotpass` | Value sent in the node's `Authorization` header. |
| `RUST_LOG` | no | `info,lavalink_test_bot=debug` | Log filter. The default includes stats and non-text websocket frames from this crate. |

A repo-root `.env` for local development can look like this:

```dotenv
DISCORD_TOKEN=replace-me
TEST_GUILD_ID=123456789012345678
LAVALINK_HOST=localhost:2333
LAVALINK_PASSWORD=youshallnotpass
RUST_LOG=info
```

The root `.gitignore` excludes `.env` and `application.yml`; still check staged
files before committing.

## Running both processes

From the repository root, first create `application.yml` and enable the source
you plan to test. For example, `local: true` permits a file path and
`youtube: true` permits YouTube URLs/searches when `yt-dlp` is installed.

On Bash, the helper starts a release node in the background, waits for its TCP
port, and runs the bot in the foreground:

```sh
scripts/dev.sh
```

It reads the repo-root `.env`. Pressing Ctrl+C stops the bot and the background
node. The helper expects a simple `host:port` in `LAVALINK_HOST` and uses Bash's
`/dev/tcp`; use the manual workflow on shells that do not provide it.

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

Successful startup has three visible milestones: the bot logs into Discord,
connects to the node websocket, and receives a ready message with a session ID.
It enables a 60-second resume window automatically; after a brief node websocket
disconnect, the next ready log should contain `resumed=true` and the same session
ID. Command registration is silent unless it fails. A slash command issued before
the first ready message may fail with "no session yet"; retry after the ready log.

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

`/search` is the fastest way to isolate loading from Discord voice. If it
fails, fix the source or node configuration before debugging `/join` or audio.

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

Filters are cumulative in this bot. Lavalink replaces the complete filter
object on each update, so the bot remembers the current per-guild chain and
re-sends it when one filter changes.

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

For audio-pipeline work, repeat with the source/codec/container that changed.
For concurrency work, use multiple guilds or bot instances and inspect
`/players` plus `/stats`; one guild alone cannot expose cross-player stalls.

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

An unparseable node message is logged at `error` with its raw text. Treat that
as a protocol mismatch or node defect, not as harmless debug noise.

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

The bot's default filter already enables its own debug logs. Use
`RUST_LOG=info,lavalink_server=debug` on the node when the normal node logs do
not identify the failing boundary.
