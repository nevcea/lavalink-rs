# Repository Guidelines

## Project Scope & Non-Negotiable Rules

This repository is a from-scratch Rust port of the Lavalink v4 audio node. Preserve everything clients can observe: response bodies, status codes, wire shapes, and event content/order must match upstream, even when upstream behavior looks accidental. Internal timing, concurrency, ownership, and crash isolation may improve without changing that contract.

Before changing an apparent bug or omission, read `MAINTENANCE.md`; many differences are deliberate. Before touching server behavior, also read `crates/server/src/lib.rs`. Never run repository-wide `cargo fmt`: the project is hand-formatted, and DSP tables intentionally remain comparable with their Java sources. Match the surrounding file manually.

## Project Structure & Architecture

- `crates/protocol` (`lavalink-protocol`) contains wire DTOs and the standalone `encodedTrack` codec. Keep server logic, async work, and I/O out.
- `crates/server` (`lavalink-server`) contains REST/WS handling, sessions, guild player actors, sources, and the audio pipeline.
- `crates/test-bot` (`lavalink-test-bot`) drives a running node through Discord for end-to-end audio checks.
- Unit tests live inline in `#[cfg(test)] mod tests`; server benchmarks live in `crates/server/benches/`.

The audio path is `source -> decode -> resample -> filter -> ring -> mixer/Opus`. The CPU-bound pump owns decoding and writes ahead; the ring isolates each player and advances audible position on reads. One actor per guild controls player state and must not await I/O or process audio. Source registration order in `main.rs::source_managers` is significant because the first match wins. Preserve `panic = "unwind"`: `engine.rs` catches a failed track's panic so it cannot terminate the node.

## Build, Test, and Development Commands

- `cargo build --workspace` builds all crates.
- `cargo test --workspace` runs service-free unit tests.
- `cargo test -p lavalink-server config::` runs one module; substitute a test-name substring for a narrower run.
- `cargo clippy --workspace --all-targets -- -D warnings` runs the CI lint gate.
- `cargo bench -p lavalink-server --bench pipeline` runs a named server benchmark.
- `cargo run -p lavalink-server --release -- application.yml` starts the node.
- `scripts/dev.sh` starts the node and test bot; it requires `DISCORD_TOKEN` and `TEST_GUILD_ID`.

The toolchain needs Rust 1.95+, a C compiler and CMake for Opus, plus a C++ compiler and `libclang` for the timescale filter. `yt-dlp` is optional. Copy `application.yml.example` to `application.yml` and set a non-empty password before running.

## Coding Style & Naming

Use four-space indentation, `snake_case` functions/modules, `CamelCase` types, and `SCREAMING_SNAKE_CASE` constants. Keep changes focused within the owning crate. Do not reorder wire-visible patch application, source registration, or event delivery for stylistic reasons. Add dependencies or abstractions only when existing code or the standard library cannot solve the problem directly.

## Testing Guidelines

Add focused `#[test]` or `#[tokio::test]` cases beside changed code, with behavior-oriented snake-case names. CI requires workspace tests, Clippy, Rust 1.95 compatibility, and dependency auditing; there is no numeric coverage threshold. Audio-path changes additionally require `scripts/dev.sh` and a real Discord voice channel because unit tests cannot validate audible playback, seek accuracy, or live multi-player behavior.

## Commits & Pull Requests

The default branch is `dev`. Use Conventional Commits: `<type>[scope]: <English description>`, for example `fix(server): preserve websocket event order`. Allowed types include `feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`, `build`, `ci`, `chore`, and `revert`. Reserve `!` or `BREAKING CHANGE:` for real wire/config breaks. Split unrelated changes, but keep a single mechanical pass together.

Pull requests should explain user-visible behavior, link relevant issues, call out wire/config effects, and list commands and live-audio checks performed. All CI jobs must pass before merge.

## Security & Configuration

Never commit `application.yml`, `.env`, Discord tokens, or production passwords. The example password is local-only. Document new configuration keys in `application.yml.example`.
