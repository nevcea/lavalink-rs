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

## Development

`dev` is where active development happens; `main` only receives merges from
`dev` after testing. Commits follow [Conventional Commits](https://www.conventionalcommits.org/ko/v1.0.0/).
See `CLAUDE.md` for the full workflow.

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
```

## License

MIT, see `LICENSE.md`.
