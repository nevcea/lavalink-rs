# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A from-scratch Rust port of Lavalink v4 (the JVM audio node most Discord music
bots talk to).

## First files to read

- `crates/server/src/lib.rs` — the governing rule (below) plus a table of
  what was fixed and where.
- `MAINTENANCE.md` — every deliberately-missing feature, and why.
- The `## Architecture` section below, for how the audio pipeline and player
  actor fit together before touching either.

## Non-negotiable rules

- **Wire compatibility.** Anything a client can observe over the wire —
  response bodies, status codes, event sequences — matches the original
  exactly, even where the original looks accidental. Timing/latency is not
  observable this way and is fair game (that's the whole point of the
  concurrency rework); only order and content are fixed. Improvements are
  confined to what a client can't see: concurrency, resource ownership,
  crash-prone paths.
- **Read `MAINTENANCE.md` before "fixing" anything that looks like a bug or
  omission** — most are deliberate; if it's not explained there, it's
  probably real.
- **Never run `cargo fmt` across the tree** (see `## Commands` below).
- **Run `test-bot` before merging an audio-path change**, not just
  `cargo test --workspace`. Unit tests and benches cover DSP math and
  pipeline cost in isolation; they can't catch a real seek landing wrong or
  audio breaking up under a live voice connection — only a real Discord
  voice channel does (see `### Testing` under `## Architecture`).
  `scripts/dev.sh` starts the node then `test-bot` against it in one step
  (needs `DISCORD_TOKEN`/`TEST_GUILD_ID`, see `crates/test-bot/README.md`).

## Git workflow

One branch, `dev` — also GitHub's default; commit directly, no `main` to
protect.

Commits follow [Conventional Commits](https://www.conventionalcommits.org/):
`<type>[scope]: <description>` (types: `feat|fix|docs|style|refactor|perf|
test|build|ci|chore|revert`; description in English). `!` /
`BREAKING CHANGE:` only for actual wire/config breaks, not internal
refactors. Split unrelated changes into separate, independently-revertible
commits — but don't fragment one mechanical pass (e.g. a repeated wording
fix) into several just to keep them small.

## Workspace layout

- `crates/protocol` (`lavalink-protocol`) — wire DTOs + the lavaplayer
  `encodedTrack` codec; no server logic, async, or I/O.
- `crates/server` (`lavalink-server`) — the audio node: REST/WS, player
  actors, audio pipeline, source managers.
- `crates/test-bot` (`lavalink-test-bot`) — a Discord bot that drives a
  running node as an ordinary v4 client, for real audio into a real voice
  channel (see its README).

## Requirements

- Rust version — see `rust-version` in the workspace `Cargo.toml`
- A C compiler + CMake (vendored `libopus` via `songbird`)
- A C++ compiler + `libclang` (`signalsmith-stretch`'s `cc`+`bindgen` build,
  for the `timescale` filter)
- [`yt-dlp`](https://github.com/yt-dlp/yt-dlp) on `PATH`, optional — enables
  the youtube/soundcloud/bandcamp/deezer sources; auto-disabled if missing.

## Commands

```sh
cargo build --workspace
cargo test --workspace
cargo test -p lavalink-server config::        # one module
cargo test -p lavalink-server some_test_name  # one test by name substring
cargo bench -p lavalink-server --bench <name>  # see crates/server/benches/
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p lavalink-server --release -- application.yml   # path arg optional
```

`cp application.yml.example application.yml` first — the node refuses to
start with an empty password.

Never run `cargo fmt` across the tree (see `## Non-negotiable rules` above) —
this codebase is hand-formatted and destroys a table kept intentionally
diffable against the original Java source; see `MAINTENANCE.md`'s
"Formatting" for why. Match whatever file you touch by hand.

## Architecture

### Audio pipeline (`crates/server/src/audio/`)

```text
[pump: CPU-bound, no deadline]                [send: O(1), 20ms deadline]
source → decode → resample → filter ──▶ ring ──▶ mixer pulls, encodes Opus
```

- `pump.rs` — decode loop on its own OS thread (a whole track's lifetime,
  not the blocking pool). Demuxes with symphonia, resamples, filters, writes
  into the ring. Seeks the demuxer directly, since songbird's mixer can't
  seek a live ring.
- `ring.rs` — isolation boundary between pump and playback; a slow pump only
  starves its own ring (nulled frames, counted for `/v4/stats`), never
  another player's. Position advances on read, since the pump runs ahead of
  what's audible.
- `engine.rs` — assembles pump + ring + songbird's `RawAdapter` per track;
  the only surface the player actor calls, all non-blocking. Wraps the pump
  thread in `catch_unwind` (hence `panic = "unwind"`) so a panic ends one
  track, not the node.
- `filter.rs` — DSP chain, ports of lavaplayer/lavadsp (coefficients and
  update-loop shape are part of the contract). `timescale` uses a different
  algorithm family (signalsmith-stretch, not SoundTouch/WSOLA) — see
  `MAINTENANCE.md`.
- `resample.rs` — Catmull-Rom to 48kHz stereo for `LOW`,
  `rubato::SincFixedIn` for `MEDIUM`/`HIGH` — see module docs.
- `stream.rs` / `source/` — opens bytes per source; `main.rs::source_managers`
  sets registration order (first match wins).

### Player actor (`crates/server/src/player/actor.rs`)

One actor per guild, replacing the original's `synchronized(player)` +
blocking `.join()`. The loop never awaits I/O, loading finishes before a
`Command::Patch` arrives, and the actor never touches audio data — only
starts/stops the engine and reads counters. `apply_patch`'s field order
mirrors the original's handler line-for-line and is wire-visible.

### Everything else in `crates/server/src/`

- `state.rs` / `session.rs` — `AppState` and the session registry: one lock,
  one `Open ↔ Resumable{deadline}` state machine per session, guild player +
  voice connection registered atomically.
- `sink.rs` — outbound WS queue, two lanes (essentials never dropped,
  `playerUpdate`/`stats` coalesced latest-wins); a client that stops
  draining essentials is closed with 1008.
- `voice.rs` — songbird's driver only, no gateway client; the actor owns
  player state, songbird owns connection state.
- `ws.rs` — `/v4/websocket` handler; validates `User-Id` as a 400 instead of
  the original's handshake crash.
- `ticker.rs` — three node-wide periodic tasks (player updates, stats,
  resume sweep), not a scheduler per session.
- `loader.rs` — identifier resolution off the async runtime, single-flight,
  TTL-cached (`CACHE_TTL`) on success/empty only.
- `rest/` — the v4 HTTP surface; `rest/mod.rs::router` is the full route
  table.

### Testing

Unit tests live inline (`#[cfg(test)] mod tests`) next to the code they
cover. Benches under `crates/server/benches/` cover the DSP chain,
resampler, and pipeline cost. What tests/benches can't reach — real seek
precision, multi-player CPU under a live voice channel — is `test-bot`'s job
(`audio/mod.rs`'s module docs).
