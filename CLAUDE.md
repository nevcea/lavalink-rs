# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A from-scratch Rust port of Lavalink v4 (the JVM audio node most Discord music bots
talk to). The governing rule, stated in `crates/server/src/lib.rs`: **anything a
client can observe over the wire — response bodies, status codes, event
sequences — matches the original exactly, including where the original looks
accidental.** Improvements are confined to what a client cannot see: concurrency,
resource ownership, and places where the original simply crashes. Read
`MAINTENANCE.md` before "fixing" anything that looks like a bug or an omission —
most of them are deliberate and already explained there (unimplemented filters/keys,
refused sources, a documented divergence in tremolo's LFO). If you can't find a
reason there, it's more likely an actual bug.

## Git workflow

One long-lived branch, **`dev`** — it's also the repo's default branch on
GitHub. Commit here directly; there is no separate `main` to merge into or
protect. Before committing anything that touches the audio path, run
`cargo test --workspace` and, when relevant, a `test-bot` pass per its README.

### Commit messages — Conventional Commits

Every commit follows [Conventional Commits v1.0.0](https://www.conventionalcommits.org/ko/v1.0.0/):

```
<type>[optional scope]: <description>

[optional body]

[optional footer(s)]
```

- `type` is one of: `feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`,
  `build`, `ci`, `chore`, `revert`. Lowercase, English, matching the spec exactly
  — these are what tooling (changelog generators, semver bumps) parses on.
- `scope` is optional, parenthesized, names the affected area (e.g. `feat(audio):`,
  `fix(session):`, `docs(readme):`).
- `description` can be Korean or English — the spec doesn't constrain it.
- A breaking change is marked either with `!` after the type/scope
  (`feat(config)!: ...`) or a `BREAKING CHANGE:` footer, and bumps the next major
  version under semver — use only for changes that actually break something a
  client or operator depends on (a wire-visible behavior change, a config key
  removal), not internal refactors.
- Multiple unrelated changes are multiple commits, not one commit with a
  compound type.
- Keep each commit small enough to review on its own, and each commit
  independently understandable and revertible — a split that leaves an
  intermediate commit non-compiling or meaningless on its own is not the goal,
  just noise. Even one logical fix should be split where it has separable
  parts that meet that bar — e.g. a helper/primitive added in one commit, then
  the commit that puts it to use — rather than landing as one large diff. A
  commit that touches unrelated files or mixes a refactor with a behavior
  change is a sign it should be split further.

## Workspace layout

- `crates/protocol` (`lavalink-protocol`) — pure wire types and the lavaplayer
  `encodedTrack` codec. No server logic, no async, no I/O. This is the reusable
  library half of the workspace; anything that just needs to speak v4 to/from a
  node depends on this crate alone.
- `crates/server` (`lavalink-server`) — the audio node itself: REST/WS surface,
  player actors, the audio pipeline, source managers.
- `crates/test-bot` (`lavalink-test-bot`) — an integration harness, not a feature
  bot: a Discord bot that drives a running node as an ordinary v4 client, for
  exercising the one path unit tests can't reach: real audio into a real Discord
  voice channel. See `crates/test-bot/README.md` for setup and the event sequence
  to watch for per command.

## Requirements

- Rust 1.95+ (`rust-version` in the workspace `Cargo.toml`)
- A C compiler and CMake, to build the vendored `libopus` (pulled in
  transitively through `songbird`)
- A C++ compiler and `libclang` (e.g. `libclang-dev` on Debian/Ubuntu), for
  `signalsmith-stretch`'s `cc`+`bindgen` build (the `timescale` filter — see
  `MAINTENANCE.md`)
- [`yt-dlp`](https://github.com/yt-dlp/yt-dlp) on `PATH`, optional — only needed
  for the youtube/soundcloud/bandcamp/deezer sources. Detected once at startup
  (`main.rs`); if it's missing, those sources are disabled rather than failing
  the boot.

## Commands

```sh
cargo build --workspace
cargo test --workspace                       # all unit tests (397 across the tree)
cargo test -p lavalink-server config::        # one module, e.g. config tests
cargo test -p lavalink-server some_test_name  # one test by name substring
cargo bench -p lavalink-server --bench filter # also: resample, pipeline
cargo clippy --workspace --all-targets
cargo run -p lavalink-server --release -- application.yml   # path arg optional, defaults to ./application.yml
```

`cp application.yml.example application.yml` first — the node refuses to start with
an empty password (`Config::validate`).

**Do not run `cargo fmt` (or `rustfmt`) across the tree.** This codebase is
hand-formatted deliberately (see `MAINTENANCE.md`'s "Formatting" section); a
full-tree `cargo fmt` produces ~84 diff hunks and destroys at least one table
(`filter.rs`'s `COEFFICIENTS_48000`) that's intentionally kept diffable against
the original Java source. Whatever file you touch — including a brand new one
— match its surrounding style by hand instead.

`.github/workflows/ci.yml` runs `cargo test --workspace` and `cargo clippy
--workspace --all-targets -- -D warnings` on push to `dev` and on PRs — no
`rustfmt.toml`/`clippy.toml` exist in the repo, so run `cargo test` and
`cargo clippy` yourself before calling something done rather than waiting on CI.

## Architecture

### The audio pipeline (`crates/server/src/audio/`)

```text
[pump: CPU-bound, no deadline]                [send: O(1), 20ms deadline]
source → decode → resample → filter ──▶ ring ──▶ mixer pulls, encodes Opus
```

- **`pump.rs`** — the decode loop, on its own OS thread (not the tokio blocking
  pool, since it lives for a whole track). Demuxes with symphonia, resamples,
  applies filters, writes PCM into the ring. Also owns seeking: since songbird's
  mixer treats the ring as a live stream it can't seek, so the pump seeks the
  *demuxer* directly, discards buffered audio, and rebases the position counter.
- **`ring.rs`** — the isolation boundary between the pump and playback. A pump
  that falls behind only starves its own ring (reader sees nulled frames, counted
  for `/v4/stats`); it can never make another player's audio late. The position
  counter is advanced on the **read** side, not by the pump, because the pump runs
  a whole `frameBufferDurationMs` ahead of what's audible.
- **`engine.rs`** — assembles pump + ring + songbird's `RawAdapter` into one
  pipeline per track; the only surface the player actor calls into, and every
  method on it is non-blocking (flip an atomic, send on a channel, or spawn).
  Wraps the pump thread's run loop in `catch_unwind`, turning one track's panic
  into a `Failed` event instead of taking the node down — the reason the
  workspace `Cargo.toml`'s release profile keeps `panic = "unwind"` rather than
  the smaller/faster `"abort"`.
- **`filter.rs`** — the DSP chain, ports of specific lavaplayer/lavadsp
  implementations where coefficients and update-loop shape are part of the
  contract, not just the algorithm. `timescale` is the one filter with no port
  (needs a WSOLA time-stretcher; see `MAINTENANCE.md`).
- **`resample.rs`** — Catmull-Rom interpolation to 48kHz stereo, a deliberate
  trade against a windowed-sinc resampler (see module docs / `MAINTENANCE.md`'s
  `resamplingQuality` entry).
- **`stream.rs`** / **`source/`** — opens bytes per source (http, local, youtube,
  soundcloud, bandcamp, deezer via yt-dlp; see `main.rs::source_managers` for
  registration order, which matters: first manager to claim an identifier wins).

### The player actor (`crates/server/src/player/actor.rs`)

One actor per guild, replacing the original's `synchronized(player)` + blocking
`.join()` on a voice connection (which stalls every other request for that guild).
Three rules enforced by construction: the actor loop never awaits I/O, loading
happens entirely before a `Command::Patch` arrives, and the actor never touches
audio data — only starts/stops the engine and reads counters. `PatchRequest`
application order in `apply_patch` mirrors the original's handler line-for-line and
is wire-visible (it decides whether `position` in a play request seeks the old
track or offsets the new one).

### Everything else in `crates/server/src/`

- **`state.rs`** — `AppState`, built once in `main.rs`; `AppState::player`
  gets-or-spawns a guild's actor via `Session::get_or_create_player`.
- **`session.rs`** — one registry, one lock, one `Open ↔ Resumable{deadline}` state
  machine per session — replacing the original's two half-safe maps and a resume
  handshake that can leak an uncancelled timeout. Within a session,
  `Session::get_or_create_player` registers a guild's player and its voice
  connection as one atomic entry (`GuildPlayer`), not two independently-updated
  maps — two racing first-time requests for the same guild used to be able to
  register a player from one and a voice connection built by the other, since
  each was a separate `entry().or_insert()`.
- **`sink.rs`** — the outbound WS queue: bounded, two lanes (essential
  events/`ready`, never dropped vs. coalesced `playerUpdate`/`stats` snapshots,
  latest-wins). A client that stops draining essentials gets closed with 1008
  rather than silently growing server memory.
- **`voice.rs`** — songbird's standalone `driver` only, no gateway client (Lavalink
  receives voice state/server updates over REST from its own caller, which is
  exactly the driver's input shape). Strict split: the player actor owns player
  state, songbird owns connection state; `voice.rs` only caches `VoiceUpdate` events
  as they arrive.
- **`ticker.rs`** — three node-wide periodic tasks (player updates, stats, resume
  sweep) instead of a scheduler per session.
- **`loader.rs`** — track-identifier resolution: off the async runtime (blocking
  pool), single-flight per identifier, 60s TTL cache on success/empty only (never
  caches failures).
- **`auth.rs`** — constant-time password comparison (the original's `==` is a JVM
  timing side channel).
- **`error.rs`** — `ApiError` + a middleware that fills in the response's `path`
  field once, centrally, since handlers don't naturally have the request URI.
- **`rest/`** — the v4 HTTP surface; `rest/mod.rs::router` is the full route table.

### Testing

Unit tests live inline (`#[cfg(test)] mod tests`) next to the code they cover —
`config.rs`, `state.rs`, `filter.rs`, `ring.rs`, etc. Benches under
`crates/server/benches/` cover the DSP chain, resampler, and full pipeline cost.
What unit tests and benches *can't* cover — real seek precision on real containers,
multi-player CPU cost against a live Discord voice channel — is called out
explicitly in `audio/mod.rs`'s module docs and is `test-bot`'s job instead.
