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
  `bandcamp`/`deezer` via `yt-dlp` if it's on `PATH`. All 10 filters
  implemented, `timescale` included — see `MAINTENANCE.md` for the one place
  it deliberately diverges from the original's algorithm.
- `crates/test-bot` (`lavalink-test-bot`) — Discord bot for end-to-end
  testing, see its README.

## Requirements

- Rust — see `rust-version` in `Cargo.toml` for the minimum
- A C compiler + CMake (vendored `libopus` via `songbird`)
- A C++ compiler + `libclang` (`signalsmith-stretch`'s `cc`+`bindgen` build,
  for the `timescale` filter)
- [`yt-dlp`](https://github.com/yt-dlp/yt-dlp) on `PATH`, optional — enables
  the youtube/soundcloud/bandcamp/deezer sources; detected once at startup,
  auto-disabled and dropped from `/v4/info` if missing rather than failing
  the boot

## Running

```sh
cp application.yml.example application.yml
# the node refuses to start with an empty lavalink.server.password (the
# example ships "youshallnotpass" — change it for anything but local use).
# Toggle sources under lavalink.server.sources to enable the ones you need
# (all default off except http).
cargo run -p lavalink-server --release
```

The node listens on port `2333` by default (`server.port` in the yml). Check
it came up:

```sh
curl -H "Authorization: youshallnotpass" http://localhost:2333/v4/info
```

(`youshallnotpass` is the example config's default password — use whatever
you set). REST lives under `/v4/`, the WS session endpoint is
`/v4/websocket`, auth is the `Authorization` header on both — the same
contract as upstream Lavalink, so any existing v4 client library
(Lavalink.py, Wavelink, Shoukaku, ...) works against this node unmodified,
no code changes on the bot side.

### Docker

```sh
docker build -t lavalink-rs .
docker run -p 2333:2333 -v "$(pwd)/application.yml:/app/application.yml" lavalink-rs
```

No config is baked into the image — mount your own `application.yml` as
above (`yt-dlp` is preinstalled in the image, so the four sources that need
it work out of the box once enabled in the config).

### Testing

```sh
cargo test --workspace       # unit tests, no external services needed
cargo clippy --workspace --all-targets -- -D warnings
```

`cargo test` covers DSP math, the wire protocol, and pipeline logic, but not
real audio over a live voice connection — that needs `crates/test-bot`
driving a real Discord voice channel; see its README before touching the
audio pipeline.

## License

MIT, see `LICENSE.md`.
