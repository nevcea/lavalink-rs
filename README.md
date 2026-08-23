# lavalink-rs

A from-scratch Rust port of the Lavalink v4 audio node. REST responses,
websocket messages, status codes, omitted fields, and event order follow
upstream; the server and audio pipeline are native Rust.

This project is suitable for development and compatibility testing, but
operators should review [the known and deliberate differences](MAINTENANCE.md)
before treating it as a drop-in production node.

## Features

- Lavalink v4 REST and websocket APIs, including session resuming and the
  `encodedTrack` codec
- Per-guild players with play, stop, pause, seek, volume, voice updates, and
  player destruction
- All ten v4 filters: volume, equalizer, karaoke, timescale, tremolo, vibrato,
  distortion, rotation, channel mix, and low-pass
- HTTP, local-file, and getyarn.io clip loading
- YouTube, SoundCloud, Bandcamp, and Deezer loading through `yt-dlp`
- Node/player statistics and Lavalink-shaped error responses
- Graceful shutdown and per-track panic isolation

Plugins, route planning/IP rotation, Twitch, Vimeo, Nico Nico Douga, and some
configuration keys are intentionally unsupported. Unsupported sources and
filters are omitted from `/v4/info`; [MAINTENANCE.md](MAINTENANCE.md) records
the exact behavior of every gap.

## Architecture

```text
Lavalink client
   │ REST commands + voice credentials
   │ websocket events
   ▼
session registry ──▶ one actor per guild ──▶ player engine
                                              │
                                              ▼
                         source → decode → resample → filters
                                              │
                                              ▼
                                      ring → mixer → Opus → Discord
```

The CPU-bound pump decodes ahead into a bounded ring. The mixer consumes the
ring and advances audible position. One actor per guild owns player state but
does no network or audio work, isolating slow or failed tracks from other
players.

## Workspace

| Crate | Purpose |
|---|---|
| [`lavalink-protocol`](crates/protocol) | Wire DTOs and the standalone lavaplayer `encodedTrack` codec. |
| [`lavalink-server`](crates/server) | The REST/websocket node, sessions, players, sources, and audio pipeline. |
| [`lavalink-test-bot`](crates/test-bot) | A small Discord client for live, end-to-end voice and event testing. |

To use only the protocol crate from Git:

```toml
lavalink-protocol = { git = "https://github.com/nevcea/lavalink-rs", package = "lavalink-protocol" }
```

## Requirements

- Rust 1.95 or newer
- A C compiler and CMake for the vendored Opus library
- A C++ compiler and `libclang` for the timescale filter
- [`yt-dlp`](https://github.com/yt-dlp/yt-dlp) 2026.08.19 or newer on `PATH`, optional

`yt-dlp` is detected at startup. If missing, its four dependent sources are
disabled and omitted from `/v4/info`. YouTube also needs a JavaScript runtime;
Deno is yt-dlp's preferred default.

After updating yt-dlp, verify a full download rather than relying on `--test`,
which only fetches the beginning of the file:

```sh
yt-dlp --no-playlist -f "bestaudio[acodec=aac]/bestaudio[ext=m4a]/bestaudio[acodec!=opus]/bestaudio/best" \
  -o '/tmp/ytdlp-check.%(ext)s' 'https://www.youtube.com/watch?v=S0G-sVyabT0'
```

## Quick start

1. Copy the example configuration:

   ```sh
   cp application.yml.example application.yml
   ```

2. Change `lavalink.server.password`; `youshallnotpass` is for local
   development only.

3. Enable the sources you need under `lavalink.server.sources`. Only `http`
   is enabled in the example.

4. Start the node:

   ```sh
   cargo run -p lavalink-server --release -- application.yml
   ```

5. Verify it from another terminal:

   ```sh
   curl -H "Authorization: youshallnotpass" http://localhost:2333/v4/info
   ```

The node listens on `0.0.0.0:2333` by default. REST endpoints live under
`/v4/`; the websocket is `/v4/websocket`. Both use the `Authorization` header.
A configured, non-empty Prometheus path is anonymous, matching upstream.

The server accepts the configuration path as its first argument and otherwise
looks for `application.yml` in the current directory. Logging is controlled by
`RUST_LOG`, for example:

```sh
RUST_LOG=info,lavalink_server=debug cargo run -p lavalink-server -- application.yml
```

## Configuration

[`application.yml.example`](application.yml.example) is the configuration
reference. The main settings are:

| Setting | Meaning |
|---|---|
| `server.address`, `server.port` | Listen address and port. |
| `lavalink.server.password` | Shared secret required outside the configured Prometheus path. It must not be empty. |
| `lavalink.server.sources` | Source managers to enable. Enabled managers are registered in a fixed precedence order because the first matching source wins. |
| `youtubeSearchEnabled`, `soundcloudSearchEnabled` | Whether the corresponding search prefixes are claimed. Direct URLs remain available when the source is enabled. |
| `filters` | Filters advertised by `/v4/info` and accepted by player updates. |
| `resamplingQuality` | `LOW`, `MEDIUM`, or `HIGH`; higher tiers trade CPU for a band-limited resampler. |
| `frameBufferDurationMs` | Decoded audio buffered per player. |
| `trackStuckThresholdMs` | Silence duration before a `TrackStuckEvent`. |
| `playerUpdateInterval` | Seconds between websocket `playerUpdate` messages. |
| `httpConfig` | Optional proxy used by blocking HTTP requests and `yt-dlp`. |
| `timeouts` | Connect and idle-read timeouts for media requests. |
| `metrics.prometheus` | Optional Prometheus endpoint for the ten Lavalink-specific gauges. |

Unknown upstream keys are accepted so an existing Lavalink configuration can
be reused, but unimplemented keys have no effect. Check
[MAINTENANCE.md](MAINTENANCE.md) rather than assuming that an accepted key is
active.

## Sources

| Source | Extra requirement | Notes |
|---|---|---|
| `http` | None | Enabled in the example. Can fetch private/LAN addresses; see Security. |
| `local` | None | Allows arbitrary local file reads to authenticated clients. Off by default. |
| `getyarn` | None | Loads short clips from getyarn.io. |
| `youtube` | `yt-dlp` | Supports direct URLs and `ytsearch:`. `ytmsearch:` is deliberately refused. |
| `soundcloud` | `yt-dlp` | Supports direct URLs and `scsearch:`. |
| `bandcamp` | `yt-dlp` | Direct URL loading. |
| `deezer` | `yt-dlp` for playback | Metadata comes from Deezer; playback substitutes a YouTube match. |

## Security notes

- Do not commit `application.yml`, `.env`, bot tokens, or production passwords.
- Treat the node password as an administrative credential. Authenticated
  clients can create sessions and players without per-client quotas.
- The HTTP source can request loopback, private-network, and link-local URLs.
  Configure `httpConfig` or disable the source when clients are not fully
  trusted.
- The local source can read any path available to the node process. Keep it
  disabled unless that access is intentional.
- Bind to a private interface or put TLS and network access control in front of
  the node when it is reachable outside a trusted network.

The complete reasoning for these trust-boundary choices is in
[MAINTENANCE.md](MAINTENANCE.md#post-auth-resource-limits-and-source-reach--deliberately-absent).

## Development

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Run a focused server test by module or name:

```sh
cargo test -p lavalink-server config::
```

Server benchmarks live under `crates/server/benches`:

```sh
cargo bench -p lavalink-server --bench pipeline
```

Unit tests cannot verify audible playback, Discord voice handshakes, seek
accuracy, or live multi-player behavior. Audio-path changes also require the
[test bot](crates/test-bot/README.md) and a real Discord voice channel.

Do not run repository-wide `cargo fmt`; match the surrounding hand formatting
and keep DSP tables comparable with their Java sources. See
[AGENTS.md](AGENTS.md) for contributor rules.

## License

MIT. See [LICENSE.md](LICENSE.md).
