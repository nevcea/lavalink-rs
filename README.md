# lavalink-rs

A Lavalink v4 compatible audio node, written in Rust — the wire protocol
matches upstream exactly, everything else is rebuilt. See `CLAUDE.md` for
architecture, `MAINTENANCE.md` for what's deliberately not implemented.

## Crates

- `crates/protocol` (`lavalink-protocol`) — wire DTOs + the lavaplayer
  `encodedTrack` codec, no server logic/async/I/O, usable standalone:
  `lavalink-protocol = { git = "https://github.com/nevcea/lavalink-rs", package = "lavalink-protocol" }`
- `crates/server` (`lavalink-server`) — the node binary. REST + WS matching
  Lavalink v4 (loading, playing, filters, player updates, resuming).
  Sources: `http`/`local`/`getyarn.io` built in, `youtube`/`soundcloud`/
  `bandcamp`/`deezer` via `yt-dlp` if it's on `PATH`. Filters: 9 of 10 —
  `timescale` is the deliberate omission, see `MAINTENANCE.md`.
- `crates/test-bot` (`lavalink-test-bot`) — Discord bot for end-to-end
  testing, see its README.

## Running

```sh
cp application.yml.example application.yml
# edit application.yml, then:
cargo run -p lavalink-server --release
```

Needs a C compiler + CMake (vendored `libopus`); `yt-dlp` on `PATH` is
optional. Listens on port `2333`, REST under `/v4/`, WS at `/v4/websocket`,
auth via the `Authorization` header — same contract as upstream, so any v4
client library works unmodified.

## License

MIT, see `LICENSE.md`.
