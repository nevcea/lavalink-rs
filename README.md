# lavalink-rs

A small Lavalink v4 compatible audio node, written in Rust. The wire protocol
(REST/WS behavior, status codes, event sequences) matches the original
Lavalink v4 server; everything else — concurrency, resource ownership — is
rebuilt. See `CLAUDE.md` for the architecture and `MAINTENANCE.md` for which
parts of the v4 feature set are deliberately not implemented, and why.

## Crates

- `crates/protocol` (`lavalink-protocol`) — Lavalink v4 wire protocol DTOs and
  the lavaplayer `encodedTrack` codec. No server logic, no async, no I/O — the
  reusable library half of this workspace. Pull it in as a git dependency if
  you just need the protocol types:

  ```toml
  [dependencies]
  lavalink-protocol = { git = "https://github.com/nevcea/lavalink-rs", package = "lavalink-protocol" }
  ```

- `crates/server` (`lavalink-server`) — the audio node binary itself.
- `crates/test-bot` (`lavalink-test-bot`) — a Discord bot that drives the node
  over its v4 API, for end-to-end testing. See `crates/test-bot/README.md`.

## Features

- REST + WS surface matching Lavalink v4: loading, playing, filters, player
  updates, session resuming.
- Sources: `http`, `local`, and `getyarn.io` built in; `youtube`, `soundcloud`,
  `bandcamp`, `deezer` via `yt-dlp` (auto-disabled if `yt-dlp` isn't on `PATH`).
- Filters: 9 of the 10 v4 filters (volume, equalizer, karaoke, tremolo,
  vibrato, distortion, rotation, channelMix, lowPass). `timescale` is the one
  deliberate omission — see `MAINTENANCE.md` for that and everything else
  the node knowingly doesn't implement.

## Requirements

- Rust 1.75+
- A C compiler and CMake, to build the vendored `libopus` (pulled in
  transitively through `songbird`)
- [`yt-dlp`](https://github.com/yt-dlp/yt-dlp) on `PATH`, optional — only
  needed for the youtube/soundcloud/bandcamp/deezer sources. Detected once at
  startup; if it's missing, those sources are disabled rather than failing
  the boot.

## Running the node

```sh
cp application.yml.example application.yml
# edit application.yml, then:
cargo run -p lavalink-server --release
```

See `application.yml.example` for the full set of options.

## CI

`.github/workflows/ci.yml` runs `cargo test --workspace` and
`cargo clippy --workspace --all-targets -- -D warnings` on every push to `dev`
and every PR. Dependency updates (`Cargo.toml` and Actions versions) are
proposed weekly via `.github/dependabot.yml`.

## Connecting a client

By default the node listens on port `2333` (`server.port`) with REST under
`/v4/` and WebSocket at `/v4/websocket`, authenticated via the `Authorization`
header set to `lavalink.server.password` (`youshallnotpass` in the example
config). This is the same contract as upstream Lavalink v4, so any existing
v4 client library works against this node unmodified.

## License

MIT, see `LICENSE.md`.
