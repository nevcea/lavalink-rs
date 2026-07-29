# lavalink-rs

A small Lavalink v4 compatible audio node, written in Rust.

## Crates

- `crates/protocol` (`lavalink-protocol`) — Lavalink v4 wire protocol DTOs and
  the lavaplayer `encodedTrack` codec. This is the reusable library; pull it
  in as a git dependency if you just need the protocol types:

  ```toml
  [dependencies]
  lavalink-protocol = { git = "https://github.com/<you>/lavalink-rs", package = "lavalink-protocol" }
  ```

- `crates/server` (`lavalink-server`) — the audio node binary itself.
- `crates/test-bot` (`lavalink-test-bot`) — a Discord bot that drives the node
  over its v4 API, for end-to-end testing.

## Running the node

```sh
cp application.yml.example application.yml
# edit application.yml, then:
cargo run -p lavalink-server --release
```

See `application.yml.example` for the full set of options, and
`MAINTENANCE.md` for which parts of the Lavalink v4 feature set are
deliberately not implemented and why.

## License

MIT, see `LICENSE`.
