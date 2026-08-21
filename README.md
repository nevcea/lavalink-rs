# lavalink-rs

A from-scratch Rust port of the Lavalink v4 audio node. The public contract —
REST responses, websocket messages, status codes, field omission, and event
ordering — follows upstream Lavalink, while the server, player model, and audio
pipeline are native Rust.

This project is suitable for development and compatibility testing, but
operators should review [the known and deliberate differences](MAINTENANCE.md)
before treating it as a drop-in production node.

## What is included

- Lavalink v4 REST and websocket APIs, including session resuming and the
  `encodedTrack` codec
- Per-guild players with play, stop, pause, seek, volume, voice updates, and
  player destruction
- All ten v4 filters: volume, equalizer, karaoke, timescale, tremolo, vibrato,
  distortion, rotation, channel mix, and low-pass
- HTTP and local-file loading, plus getyarn.io clips
- YouTube, SoundCloud, Bandcamp, and Deezer loading through `yt-dlp`
- Node/player statistics and Lavalink-shaped error responses
- Graceful shutdown and per-track panic isolation

Plugins, IP rotation/route planning, Twitch, Vimeo, Nico Nico Douga, and a few
configuration keys are intentionally not implemented. Unsupported capabilities
are not advertised in `/v4/info`; the rationale and exact observable behavior
are recorded in [MAINTENANCE.md](MAINTENANCE.md).

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

The CPU-bound audio pump decodes ahead into a bounded ring. The mixer consumes
from that ring and advances the audible position. A guild actor owns player
state and coordinates the pipeline without doing network or audio work itself.
These boundaries keep a slow or failed track from blocking unrelated players.

## Workspace

| Crate | Purpose |
|---|---|
| [`lavalink-protocol`](crates/protocol) | Wire DTOs and the lavaplayer `encodedTrack` codec. It has no server logic, async work, or I/O and can be used independently. |
| [`lavalink-server`](crates/server) | The REST/websocket node, sessions, players, sources, and audio pipeline. |
| [`lavalink-test-bot`](crates/test-bot) | A small Discord client for live, end-to-end voice and event testing. |

To use only the protocol crate from Git:

```toml
lavalink-protocol = { git = "https://github.com/nevcea/lavalink-rs", package = "lavalink-protocol" }
```

## Requirements

- Rust 1.95 or newer
- A C compiler and CMake, used to build the vendored Opus library
- A C++ compiler and `libclang`, used by the timescale filter's
  `signalsmith-stretch` bindings
- [`yt-dlp`](https://github.com/yt-dlp/yt-dlp) 2026.08.19 or newer on `PATH`, optional

`yt-dlp` is detected once during startup. If it is missing, the four dependent
sources are disabled and omitted from `/v4/info` instead of preventing the node
from starting. For YouTube, keep a supported JavaScript runtime on `PATH`; Deno
is yt-dlp's default and highest-priority runtime.

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

2. Change `lavalink.server.password`. The example value,
   `youshallnotpass`, is for local development only.

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

The default bind address is `0.0.0.0:2333`. REST endpoints live under `/v4/`,
the client websocket is `/v4/websocket`, and both use the `Authorization`
header. Existing Lavalink v4 clients use the same connection details they use
for an upstream node.

The server accepts the configuration path as its first argument and otherwise
looks for `application.yml` in the current directory. Logging is controlled by
`RUST_LOG`, for example:

```sh
RUST_LOG=info,lavalink_server=debug cargo run -p lavalink-server -- application.yml
```

## Configuration

[`application.yml.example`](application.yml.example) documents every supported
setting and the security implications of source options. The main groups are:

| Setting | Meaning |
|---|---|
| `server.address`, `server.port` | Listen address and port. |
| `lavalink.server.password` | Shared secret required on every route. It must not be empty. |
| `lavalink.server.sources` | Source managers to enable. Enabled managers are registered in a fixed precedence order because the first matching source wins. |
| `youtubeSearchEnabled`, `soundcloudSearchEnabled` | Whether the corresponding search prefixes are claimed. Direct URLs remain available when the source is enabled. |
| `filters` | Filters advertised by `/v4/info` and accepted by player updates. |
| `resamplingQuality` | `LOW`, `MEDIUM`, or `HIGH`; higher tiers trade CPU for a band-limited resampler. |
| `frameBufferDurationMs` | Decoded audio buffered per player. |
| `trackStuckThresholdMs` | Silence duration before a `TrackStuckEvent`. |
| `playerUpdateInterval` | Seconds between websocket `playerUpdate` messages. |
| `httpConfig` | Optional proxy used by blocking HTTP requests and `yt-dlp`. |
| `timeouts` | Connect and idle-read timeouts for media requests. |

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

## Docker

Build the image and mount a configuration file at `/app/application.yml`:

```sh
docker build -t lavalink-rs .
docker run --rm -p 2333:2333 \
  -v "$(pwd)/application.yml:/app/application.yml:ro" \
  lavalink-rs
```

No configuration or password is baked into the image. The runtime image does
include `yt-dlp`, so its source managers work once enabled in the mounted file.
The process runs as UID/GID `10001`; mounted configuration and local-source files
must be readable by that user.

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

## Development and verification

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

Unit tests cover protocol shapes, loading logic, DSP math, and pipeline
coordination. They cannot verify audible playback, Discord voice handshakes,
seek accuracy, or live multi-player behavior. Audio-path changes also need the
[test bot](crates/test-bot/README.md) and a real Discord voice channel.

This repository is deliberately hand-formatted. Do not run repository-wide
`cargo fmt`; match the surrounding style and keep DSP tables comparable with
their Java sources. Contributor conventions and compatibility constraints are
summarized in [AGENTS.md](AGENTS.md).

## License

MIT. See [LICENSE.md](LICENSE.md).
