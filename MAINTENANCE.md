# Maintenance notes

This document records intentional gaps and divergences from upstream Lavalink.
Read it before changing behavior that looks incomplete: unsupported features
are omitted from `/v4/info`, refused explicitly, or matched to upstream's
disabled behavior instead of being approximated silently.

## Compatibility baseline

Last reviewed on 2026-08-23 against upstream tag `4.2.2`. The observable
changes since 4.0.8 are accounted for as follows; dependency-only, JVM image,
Spring Cloud, and plugin-manager changes do not apply to this Rust node.

| Upstream release | Observable change | This node |
|---|---|---|
| 4.0.8 | Non-allocating frames; clean shutdown | The pump/ring reuse their buffers, and shutdown closes websocket sessions. |
| 4.1.0 | Cause stack, request timeouts, metrics, filter defaults, CPU polling, voice race | Implemented and covered by focused tests; the one unmappable pool timeout remains documented below. |
| 4.1.2 | `beforeRequest` logging | Not modelled; request logging is controlled by `RUST_LOG`. |
| 4.2.0 | DAVE `channelId`; SoundCloud preview filtering | DAVE and `channelId` are implemented; the unavailable yt-dlp preview signal is documented below. |
| 4.1.1, 4.2.1, 4.2.2 | Voice-library fixes and DAVE library updates | Supplied by songbird 0.6; no additional wire or configuration shape. |

## Performance evidence gate

An optimization needs measured evidence. Compare baseline and candidate three
times with an existing Criterion benchmark, or the smallest new benchmark that
covers the path. Use the same host, toolchain, lockfile, power settings, and
input. Every comparison must show `p < 0.05`, at least a 5% improvement in the
median point estimate, and no significant regression in related cases. Record
the environment, commands, and medians in the PR. Audio-path changes also need
a real Discord voice check through `scripts/dev.sh`.

Release-level parity with upstream is measured separately by
`benchmarks/compare/run.py` on a dedicated Linux host. `prepare` pins and
verifies the Lavalink 4.2.2 JAR and builds deterministic WAV/FLAC/M4A fixtures;
`all --server-cpus <set> --driver-cpus <set>` runs the paired audio, HTTP,
deadline and RSS gate and writes raw JSON plus a Markdown summary under
`target/compare`. Run `python3 benchmarks/compare/run.py --self-test` after
changing the runner. Attach generated results to the PR; do not commit
machine-specific output or run the noisy comparison in shared CI.

## Route planning / IP rotation — not implemented

Route planning belongs to the out-of-scope IP-rotation feature. The routes
match upstream with no planner configured: `GET /v4/routeplanner/status`
returns `204 No Content`, while both `POST` free routes return a plain `500`
with `Can't access disabled route planner`. See
`crates/server/src/rest/mod.rs` and upstream's `RoutePlannerRestHandler.kt`.

## Twitch, Vimeo, Nico Nico Douga — not implemented

Live checks with yt-dlp 2026.07.04 could not produce media this node can
reliably play. These sources remain disabled rather than advertising partial
support.

- **Twitch** is HLS-only audio; `--get-url` returns an `.m3u8` playlist, and
  symphonia (this node's only decoder) has no HLS/MPEG-TS demuxer to read it.
- **Nico Nico Douga** resolves to
  `"protocol": "m3u8_native"` for every audio format Nico's own delivery CDN
  offers, so it has the same HLS limitation as Twitch.
- **Vimeo** fails differently: `yt-dlp -J https://vimeo.com/<id>` currently
  requires yt-dlp browser impersonation (`curl_cffi` and a target browser
  profile). This node does not install or manage that optional environment,
  and a usable non-HLS result has not been verified.

These sources belong in a future plugin system rather than core.

## `ytmsearch:` (YouTube Music search) — not implemented

With yt-dlp 2026.07.04, `music.youtube.com/search?q=...` returns unplayable
`MPREb_*` browse/album IDs, while the `#songs` variant returns no entries.

Mapping `ytmsearch:` to `ytsearch:` would silently change ranking and result
semantics. `YouTubeSource` therefore claims the prefix and returns
`loadType: "error"`, distinct from an empty search. See
`crates/server/src/audio/source/youtube.rs`.

## `timeouts.connectionRequestTimeoutMs` — not implemented

This upstream key limits waits for a bounded Apache connection pool. `reqwest`
has no equivalent bound: a request opens a new connection when it cannot reuse
one. `connectTimeoutMs` and `socketTimeoutMs` are implemented in
`crates/server/src/config.rs`; `connectionRequestTimeoutMs` has no Rust-side
operation to control.

## `bufferDurationMs` — not implemented

This sizes lavaplayer's internal decode buffer, a component this node does not
have. It is not `frameBufferDurationMs`, which sizes the implemented
pump-to-mixer ring in `crates/server/src/audio/ring.rs`. The key is accepted for
configuration compatibility but has no effect.

## `useSeekGhosting` — not implemented

Upstream emits silence while a seek re-buffers. This node's ring already emits
silence whenever the pump has no frame ready, including during seeks, and does
so unconditionally. The behavior exists; only the toggle does not.

## `soundcloudFilterOutPreviewTracks` — not implemented

Upstream filters SoundCloud tracks marked `policy: SNIP`. yt-dlp 2026.07.04
does not expose that policy or an equivalent field, so this node has no signal
to filter on. See `crates/server/src/audio/source/ytdlp.rs`.

## `opusEncodingQuality` — not implemented

Songbird 0.6 does not expose libopus's complexity setting. Matching
lavaplayer's 0–10 `opusEncodingQuality` would require a patched or forked
songbird. Players therefore use songbird's default encoder settings.

## `WebSocketClosedEvent.reason` — always empty

Upstream forwards the voice websocket close reason. Songbird 0.6 discards the
reason when converting its internal close frame to the public
`DisconnectReason`; `crates/server/src/voice.rs` receives only the code. This
node therefore sends `""`. Preserving the reason requires a patched or forked
songbird.

## `Session-Resumed`-style backpressure — a deliberate divergence, not a gap

Upstream's resumable event queue is unbounded. This node bounds essential
outbound messages and closes an overflowing session with websocket code
`1008`, preventing an indefinitely paused client from consuming unbounded
memory. See `crates/server/src/sink.rs` and `ws.rs`.

## Plugins — not implemented

No plugin loading mechanism exists. `Info.plugins` is always reported as an
empty array, matching what actually runs.

## Tremolo's LFO phase wraps — a deliberate divergence

lavadsp accumulates tremolo phase in an unwrapped `f32`; after long playback,
precision loss slows and eventually freezes the LFO. `TremoloFilter` wraps the
phase with `rem_euclid`, as vibrato already does. This changes audible output
only after the upstream defect appears and does not change the wire contract.

## `timescale` filter — a different algorithm family, not a port

Upstream wraps SoundTouch, a C++ WSOLA time-stretcher with no Rust port. This
node uses `signalsmith-stretch`, a phase-vocoder implementation selected by
listening and CPU checks (`cargo run -p lavalink-server --release --example
repro_timescale`). The algorithm differs, but the control semantics match:
`combined_speed = speed * rate` and `combined_pitch = pitch * rate`.

Unlike other filters, `timescale` can change the frame count. The pump therefore
accepts resized filter output instead of assuming an in-place transform.
Building it requires a C++ compiler and `libclang` for generated FFI bindings.

## `/metrics` — Lavalink gauges implemented, JVM gauges unavailable

This node implements all ten portable `lavalink_*` gauge families for players,
uptime, memory, and CPU, including Prometheus 0.0.4 text, `name[]` filtering,
the configurable endpoint, and upstream auth behavior. Values share the
`StatsCollector` snapshot used by `/v4/stats`.

The rest of upstream's registry is still JVM-specific:
`DefaultExports.initialize()` exposes HotSpot memory, GC, thread, classloader,
and process data; `InstrumentedAppender` exposes logback events; and
`GcNotificationListener` observes JVM GC pauses. Inventing those series for a
Rust process would reuse upstream names with different semantics, so they stay
absent rather than misleading operators.

Two unusual upstream defaults are preserved. An enabled empty endpoint is
served at `/metrics` but requires the password; a configured but disabled
endpoint reaches the anonymous 404 fallback.

## `TrackEndReason::Cleanup` — refused, variant kept

Upstream emits `CLEANUP` when lavaplayer's timer finds a track whose frames are
no longer consumed. This pipeline has no consumption-side timer: when Songbird
stops reading, the ring fills and the production-side stuck check emits
`TrackStuckEvent` after `trackStuckThresholdMs`.

The `Cleanup` enum variant remains so protocol clients can deserialize it, but
this node never emits it. Adding a second liveness timer only to reproduce the
label would change the pipeline rather than fix a missing mapping.

## Post-auth resource limits and source reach — deliberately absent

The trust boundary is the node password (`crate::auth::require_password`,
applied to the whole router in `crates/server/src/rest/mod.rs`), except for the
configured Prometheus path documented above. Like upstream, this node assumes
authenticated clients are trusted operators. The following behaviors remain
for compatibility and must be controlled at deployment boundaries.

**No per-client resource quota.** Axum's 2 MiB `Json` limit bounds JSON request
bodies, but sessions, players, `userData`, and `pluginFilters` are uncapped.
The loader cache is bounded by entry count and TTL, not bytes.

**The HTTP source can reach private addresses.** It matches by scheme and uses
reqwest's redirect behavior without per-hop host filtering. Loopback, private,
and link-local targets are reachable during load and playback. Use the
`httpConfig` proxy or disable the source for untrusted clients; the content-type
denylist is not an SSRF control.

**The local source can read any process-readable path.** It has no configured
root and is off by default. `Loader::decode` rejects tracks whose `sourceName`
is not registered, so a crafted token cannot bypass `sources.local: false`.

**`encodedTrack` decoding is structurally bounded.** Lengths, remaining input,
and versions are checked before slicing or allocating. Semantic fields such as
`length`, `position`, `identifier`, and `uri` remain client-provided, matching
upstream; the registered `sourceName` check is therefore required.

**`frameStats.deficit` is approximate and may be negative.** It compares a
fixed 3,000 expected frames with a skipped-tick timer rather than upstream's
rolling `AudioLossCounter`. Late ticks and players starting mid-window can skew
one report. See `crates/server/src/stats.rs`.

**`pluginFilters` has a different, unreachable shape.** Upstream places plugin
filters as arbitrary top-level keys inside `filters`; this node nests them under
`pluginFilters`. With no plugin system, no supported filter can exercise the
difference. See `crates/protocol/src/filters.rs`.

## Formatting

This codebase is hand-formatted; do not run repository-wide `cargo fmt`.
In particular, `COEFFICIENTS_48000` in `audio/filter.rs` is intentionally
aligned with lavaplayer's `Equalizer.java`. If the repository is ever
reformatted, protect that table with `#[rustfmt::skip]` first.
