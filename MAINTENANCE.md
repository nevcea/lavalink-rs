# Maintenance notes

Why this node's advertised feature set stops where it does. Each of these is a
deliberate omission, checked at compile time or surfaced to clients honestly
(`/v4/info`, a 400, or a 501) rather than approximated or stubbed silently.

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

An optimization is not merged from profiling intuition alone. Use an existing
Criterion benchmark, or add the smallest focused one when the changed path is
not covered, then compare baseline and candidate independently three times on
the same host, toolchain, lockfile, power settings, and input. All three
comparisons must report `p < 0.05` and at least a 5% improvement in Criterion's
median point estimate, with no statistically significant regression in related
cases. Record the commands, environment, and all before/after medians in the PR;
discard the optimization if it misses the gate.
Audio-path changes additionally require a real `scripts/dev.sh` Discord voice
check regardless of benchmark results.

Release-level parity with upstream is measured separately by
`benchmarks/compare/run.py` on a dedicated Linux host. `prepare` pins and
verifies the Lavalink 4.2.2 JAR and builds deterministic WAV/FLAC/M4A fixtures;
`all --server-cpus <set> --driver-cpus <set>` runs the paired audio, HTTP,
deadline and RSS gate and writes raw JSON plus a Markdown summary under
`target/compare`. Attach those files to the performance PR rather than committing
machine-specific results. Shared CI runs only the runner's
self-test, never the noisy comparison itself.

## Route planning / IP rotation — not implemented

Route planning belongs to the IP-rotation feature, which is out of scope for
this node. Rather than invent a status code for "not implemented," this node
matches what a real Lavalink node with no route planner configured already
does — the only state a real node's `AbstractRoutePlanner?` is ever `null`,
and the only state this node can ever be in — checked against
`RoutePlannerRestHandler.kt`: `GET /v4/routeplanner/status` returns `204 No
Content` with no body, and both `POST /v4/routeplanner/free/address` and
`POST /v4/routeplanner/free/all` throw `RoutePlannerDisabledException`, a
plain `500` with the message "Can't access disabled route planner". See
`crates/server/src/rest/mod.rs`.

## Twitch, Vimeo, Nico Nico Douga — not implemented

All three were attempted and rolled back after testing against the live sites
with yt-dlp 2026.07.04, because each fails the same check: the identifier would
load successfully (metadata resolves) and then fail at playback, which is
exactly the "advertises something that can't work" failure this project refuses
to ship — the same reasoning `timescale` and `resamplingQuality` get, applied to
a source instead of a filter.

- **Twitch** is HLS-only audio; `--get-url` returns an `.m3u8` playlist, and
  symphonia (this node's only decoder) has no HLS/MPEG-TS demuxer to read it.
- **Nico Nico Douga**, tested directly (`yt-dlp -J -f '<this node's FORMAT
  string>' https://www.nicovideo.jp/watch/sm9`), resolves to
  `"protocol": "m3u8_native"` for every audio format Nico's own delivery CDN
  ("domand") offers — the same HLS problem as Twitch, not a format-selection
  fix away. This wasn't obvious going in (lavaplayer's own `NicoAudioTrack`
  predates Nico's HLS-only migration), so `SourceKind::Nico` and a `SimpleSource`
  manager were written and then removed once this was confirmed.
- **Vimeo** fails differently: `yt-dlp -J https://vimeo.com/<id>` currently
  returns `ERROR: [vimeo] <id>: Failed to fetch macos OAuth token: HTTP Error
  401: Unauthorized`, with yt-dlp warning that "the extractor is attempting
  impersonation, but no impersonate target is available" — Vimeo's anti-bot
  gate now requires yt-dlp's browser-impersonation feature (`curl_cffi` and a
  target browser profile), which this node's yt-dlp integration does not detect,
  install, or manage, and which is not present on a stock yt-dlp install.
  Whether a successfully-impersonated extraction would even return a
  non-HLS audio format could not be verified without that dependency, so this is
  refused on the more conservative of two failures rather than shipped on an
  unverified hope.

A future `youtube-source`-style plugin architecture (see "Plugins", below) would
be the right place to pick these back up, the same way upstream itself now
distributes its actively-maintained YouTube support as a plugin rather than in
the core.

## `ytmsearch:` (YouTube Music search) — not implemented

Checked against yt-dlp 2026.07.04, live: `music.youtube.com/search?q=...` returns
`MPREb_*` browse/album ids with `title: null`, and the `#songs`-scoped search
variant returns `entries: []`. Neither is a playable song track, so there is
nothing for yt-dlp to hand back here.

Rather than map `ytmsearch:` onto a plain `ytsearch:` — which would return real
results, just the wrong ones (different ranking, different titles, fan uploads,
under a prefix a client used specifically to ask for something else) —
`YouTubeSource` claims the prefix and refuses it with an error. This makes
`loadTracks` report `loadType: "error"` with a message naming the limitation,
distinguishable from `"empty"` ("no results"), the same way route planning
answers 501 instead of 404. See `crates/server/src/audio/source/youtube.rs`.

## `timeouts.connectionRequestTimeoutMs` — not implemented

Apache's `connectionRequestTimeoutMs` bounds how long a request waits for a
connection to become free *from a bounded connection pool*. `reqwest`'s
connection pool has no such bound — a request that can't reuse a pooled
connection simply opens a new one — so there is nothing here for this key to
time out. `connectTimeoutMs` (the TCP handshake) and `socketTimeoutMs` (the
idle-read stall threshold) are both modelled, in `crates/server/src/config.rs`'s
`Timeouts` and used by `audio/stream.rs`'s `StreamOpener`; this third key is
unmappable, not merely unimplemented.

## `bufferDurationMs` — not implemented

This is lavaplayer's own decode-side buffer: how far `AudioPlayerManager` reads
and holds PCM ahead of what it hands the caller, internal to a component this
node does not have. It is not the same knob as `frameBufferDurationMs`, which
this node does model (`crates/server/src/config.rs`'s `Timeouts`-adjacent
fields, consumed by `audio/ring.rs`) and which governs the actual
pump-to-mixer buffer in this pipeline. There is no lavaplayer decode stage
here for `bufferDurationMs` to size, so the key is accepted and ignored like
any other unmodelled one, the same way `opusEncodingQuality` is unmappable
rather than merely unimplemented.

## `useSeekGhosting` — not implemented

Upstream's lavaplayer synthesizes silence frames while a seek re-buffers, so a
client hears quiet instead of a stall during the gap. This node has no
equivalent toggle, but it is not missing the underlying behavior: `ring.rs`
already hands the mixer nulled frames whenever the pump has not produced real
ones yet — the same mechanism a starved pump uses generally, not a seek
special case — and does so unconditionally rather than behind a switch. So a
seek here never stalls the output either; there is simply no config key
because there is no code path that needs to be turned off.

## `soundcloudFilterOutPreviewTracks` — not implemented

This key tells upstream to skip tracks SoundCloud marks with a `policy: SNIP`
API response — a signal that the "full" stream is actually a 30-second preview
gated behind a paid account. Checked directly against yt-dlp 2026.07.04's own
SoundCloud extraction output (`yt-dlp -J <soundcloud-url> | jq keys`): there is
no `policy`, `snippet`, or equivalent field anywhere in it. This node's `Video`
struct (`crates/server/src/audio/source/ytdlp.rs`) is built entirely from what
yt-dlp reports, so there is no signal to filter on — not a missing filter, but
a missing input to one.

## `opusEncodingQuality` — not implemented

Not reachable through songbird 0.6 at all. `Driver` exposes only `set_bitrate`
(never called — every player gets the crate default, 128 kbps stereo); `Config`
covers `crypto_mode`, `decode_mode`, `playout_buffer_length` and similar, but
nothing that reaches the Opus encoder's own complexity setting. lavaplayer's
`opusEncodingQuality` sets libopus's complexity 0..10 directly; matching it here
would need a patched or forked songbird, a materially larger undertaking than
the other unmapped keys in this document.

## `WebSocketClosedEvent.reason` — always empty

Upstream's `WsEventHandler.gatewayClosed(code, reason, byRemote)` in
`SocketContext.kt` gets a real `reason: String?` from Koe (its JVM voice
library) and forwards it verbatim, defaulting to `""` only when Koe itself
has none. This node always sends `""`.

Checked against the vendored `songbird 0.6.0` source directly
(`~/.cargo/registry/.../songbird-0.6.0/src`): `songbird::ws::Error::WsClosed`
briefly holds tungstenite's full `CloseFrame` — code *and* reason text — but
`events/context/data/disconnect.rs`'s `From<&WsError> for DisconnectReason`
keeps only `frame.code` when building the public `DisconnectReason::WsClosed
(Option<VoiceCloseCode>)` variant, discarding `frame.reason` at that
conversion. `crates/server/src/voice.rs` only ever receives this
already-stripped `DisconnectReason` via songbird's `EventContext::
DriverDisconnect` — there is no lower-level hook that still has the string.
Same class of gap as `opusEncodingQuality` above: matching this would need a
patched or forked songbird, not a change on this node's side.

## `Session-Resumed`-style backpressure — a deliberate divergence, not a gap

`ws.rs`'s `OVERFLOW_THRESHOLD`/`ESSENTIAL_CAPACITY` logic closes a session
with `1008` once its outbound essential-message backlog crosses a bound, and
`sink.rs`'s queues are capacity-limited to match. Checked against upstream's
`SocketContext.kt`: `resumeEventQueue` is a bare `ConcurrentLinkedQueue<
String>` with no size cap and no code path that ever closes a slow-draining
client — a paused session's queue grows without bound for as long as the
session stays resumable. This node's bound is a genuine, deliberate safety
fix for that unbounded growth (already reasoned about in `sink.rs`'s and
`ws.rs`'s own doc comments), not an accidental behavior gap — recorded here
so it reads the same way the rest of this document's intentional
divergences do.

## Plugins — not implemented

No plugin loading mechanism exists. `Info.plugins` is always reported as an
empty array, matching what actually runs.

## Tremolo's LFO phase wraps — a deliberate divergence

lavadsp's `VectorSupport.tremolo` accumulates its phase in an unwrapped `float`
and never resets it (`phase += 2*PI/sampleRate*frequency`, forever). At typical
tremolo frequencies the increment falls below the `f32` ULP after a few minutes
of continuous playback, so on a real Lavalink node the tremolo LFO's rate visibly
slows and then freezes partway through a long track. This node wraps the phase
with `rem_euclid` instead (`crates/server/src/audio/filter.rs`'s `TremoloFilter`),
matching the pattern already used for vibrato and the `f64` phase already used
for rotation.

This is called out specifically because it is the one place in the DSP chain
where this node's output does *not* match the original's, even though the
governing rule elsewhere is wire-for-wire fidelity "including where it looks
accidental". The distinction: fidelity is preserved for anything a client can
observe over the protocol — status codes, field presence, event sequences,
volume/distortion curves that are part of the wire's numeric contract. LFO phase
is not observable or branched on by any client; it only ever produces a defect
(the tremolo silently stopping). Reproducing it would buy nothing but a bug.

## `timescale` filter — a different algorithm family, not a port

Every other filter in `crates/server/src/audio/filter.rs` is a port of a
specific lavaplayer/lavadsp implementation, coefficients and update-loop shape
included. `timescale` can't be one: lavadsp's `TimescalePcmAudioFilter` is a
JNI wrapper around **SoundTouch**, a large C++ WSOLA time-stretcher with no
Rust implementation to port. Hand-rolling WSOLA from scratch (period
detection, overlap-add) risked exactly the "approximation that's easy to ship
and hard to notice as wrong" failure this module's own history already has one
example of — reason enough that this stayed refused (400, absent from
`IMPLEMENTED_FILTERS`) for a long time.

It's implemented now, but with `signalsmith-stretch` (a `cc`+`bindgen` Rust
wrapper around Signalsmith's phase-vocoder stretcher) standing in for
SoundTouch — a different algorithm family, evaluated and chosen on its own
listening/CPU merits (`cargo run -p lavalink-server --release --example
repro_timescale`) rather than because a phase vocoder and WSOLA are
interchangeable. It is the DSP chain's second acknowledged divergence, next to
tremolo's LFO above, and for the same reason: nothing about *which*
time-stretching algorithm ran is observable over the wire, only that `speed`/
`pitch`/`rate` did what they said. That combination rule is ported faithfully
from SoundTouch's three independent controls even though the stretcher behind
it isn't: `rate` scales both tempo and pitch together, `speed` moves tempo
alone, `pitch` moves pitch alone — `combined_speed = speed * rate`,
`combined_pitch = pitch * rate`.

This also makes `timescale` the one filter whose `process` changes the number
of frames per channel instead of transforming them in place — nothing in
`AudioFilter`'s signature prevented that (each channel is already a resizable
`Vec<f32>`), but `pump::filter_interleaved` had to stop assuming the chain's
output is the same length as its input, writing into an owned scratch buffer
instead of back into the caller's fixed-size slice.

Building `signalsmith-stretch` needs a C++ compiler and `libclang` (`bindgen`
generates its FFI bindings at build time) on top of this crate's other build
requirements — worth knowing before "why won't this compile on a fresh
machine" turns into a longer search.

## `/metrics` — Lavalink gauges implemented, JVM gauges unavailable

Upstream 4.1.0 added `LavalinkStatsCollector`, invalidating this document's old
conclusion that the endpoint had no Lavalink-specific series. This node now
matches all ten portable `lavalink_*` gauge families for players, uptime,
memory, and CPU, including Prometheus 0.0.4 text shape, `name[]` filtering, the
configurable endpoint, and its auth behavior. The values come from the same
`StatsCollector` snapshot as `/v4/stats`, so the two surfaces cannot drift.

The rest of upstream's registry is still JVM-specific:
`DefaultExports.initialize()` exposes HotSpot memory, GC, thread, classloader,
and process data; `InstrumentedAppender` exposes logback events; and
`GcNotificationListener` observes JVM GC pauses. Inventing those series for a
Rust process would reuse upstream names with different semantics, so they stay
absent rather than misleading operators.

Two unusual defaults are preserved because clients and scrape configuration can
observe them. The properties object defaults to `enabled: false`, `endpoint:
""`, while the enabled controller falls back to `/metrics` when the endpoint is
empty. The auth interceptor, however, exempts only the non-empty configured
endpoint and does so regardless of `enabled`. Thus an enabled empty endpoint is
served at `/metrics` but still requires the password, while a configured but
disabled endpoint reaches the anonymous 404 fallback.

## `TrackEndReason::Cleanup` — refused, variant kept

Upstream emits `CLEANUP` from lavaplayer's own cleanup sweep: when nobody has
pulled audio frames from a track for its cleanup threshold — concretely, the
voice connection is gone but the player still holds a track — lavaplayer notices
on a timer and ends the track with this reason.

This node's pipeline reaches a *different* observable state in that situation,
not the same one under a different name. Nothing here polls "has anyone read
from this player lately": `songbird` simply stops pulling frames, the ring fills
up and the pump's writer blocks on it (`crates/server/src/audio/ring.rs`), and
the actor's own liveness check is `trackStuckThresholdMs` on the *producing*
side — it fires `TrackStuckEvent`, not `TrackEnd(reason=CLEANUP)`, and on a
different clock (the configured stuck threshold, not upstream's fixed cleanup
interval). Faking `CLEANUP` here would mean bolting a second,
consumption-side liveness timer onto a pipeline whose stuck detection is
deliberately production-side — a real architectural addition to match a label,
not a bug fix.

So it stays undocumented behavior turned into documented behavior: the
`Cleanup` variant remains in `lavalink_protocol::message`'s `TrackEndReason` (so
a client's deserializer never sees an unknown variant), but nothing in this
node emits it. A disconnected voice channel with a still-loaded track surfaces
as a stuck-track event instead, on the stuck-threshold clock, not upstream's
cleanup clock.

## Post-auth resource limits and source reach — deliberately absent

Every entry here has been raised as a bug at least once, and each is either
upstream parity or already guarded elsewhere. Recorded so the next reviewer
does not re-derive it.

The trust boundary is the node password (`crate::auth::require_password`,
applied to the whole router in `crates/server/src/rest/mod.rs`), except for the
configured Prometheus path documented above. Lavalink's threat model is a
trusted operator running a node for a trusted bot; anything below is reachable
only by someone who already holds that password, and upstream grants them the
same reach. Adding a limit upstream does not have would be a wire divergence,
which the governing rule in `crates/server/src/lib.rs` does not allow.

**No request, memory or cache byte budget.** There is no `DefaultBodyLimit`
layer; axum's implicit 2 MiB inside `Json` (and so `ValidatedJson`) is the only
bound on a body. Sessions (`session::SessionRegistry::open`) and players per
session (`Session::get_or_create_player`) are uncapped, as are `userData` and
`pluginFilters` blobs, which are held per player for its lifetime. The loader
cache is capped at `MAX_CACHE_ENTRIES` entries with a `CACHE_TTL` sweep, not at
a byte count — an entry can be a whole playlist. Filling it needs 10 000
distinct *successful* loads inside one 60s window through
`MAX_CONCURRENT_LOADS` yt-dlp spawns, which is not a reachable shape.

**The HTTP source will fetch anything.** `HttpSource::matches` is
scheme-only, and neither client sets a redirect policy, so `reqwest`'s default
follows up to ten hops with no per-hop host check. Loopback, RFC1918 and
link-local targets are all reachable, at load time and again at playback time
via a hand-built `encodedTrack`'s `uri`. Upstream has the same property. A
blocklist would break LAN-hosted media, which is a legitimate deployment; the
documented mitigation is the `httpConfig` proxy
(`application.yml.example`), and the content-type denylist in
`source/http.rs` is a load-result filter, not a security control.

**The local source will read any path.** No canonicalisation, no root
directory, no `..` check — already stated at the top of
`crates/server/src/audio/source/local.rs` and in `application.yml.example`.
Off by default, same as upstream. The one bypass that *is* closed:
`Loader::decode` refuses an `encodedTrack` whose `sourceName` is not a
registered manager, so a crafted token cannot reach the local reader on a node
with `sources.local: false`.

**`encodedTrack` decoding is already bounded.** Size mismatch, the
minimum-eight-byte split, and the track version on both sides are all checked,
and `java_io::DataInput::take` validates against the remaining input before
slicing — so a `read_utf` length prefix that lies allocates nothing. There is
no panic path in the decoder. What it deliberately does *not* validate is
semantics: `length`, `position`, `identifier` and `uri` are whatever the token
says, which is what makes the `sourceName` check above load-bearing.

**`frameStats`' `deficit` is an approximation, and can go negative.**
`EXPECTED_FRAMES_PER_TICK` is a fixed 3 000 against a tick whose
`MissedTickBehavior` is `Skip`, so a late tick covers more than 60s of frames
and reports a negative deficit; and the usability gate ("started at least 60s
ago") is a cutoff rather than upstream's rolling `AudioLossCounter` window, so
a player whose start does not align with the tick boundary reports a large
false deficit for one window. `crates/server/src/stats.rs` documents the
approximation; the tick-skew case is the same approximation seen from the
other side, not a separate defect.

**`pluginFilters` has the wrong wire shape, unobservably.** Upstream v4
carries plugin filters as arbitrary *top-level* keys inside `filters`; this
node nests them under a `pluginFilters` key and omits it when empty. Nothing
can exercise the difference — this node ships no plugins (see below), so no
plugin filter name exists for a client to send, and an unknown top-level key is
dropped either way. Left as-is rather than restructuring `Filters` for a path
with no reachable caller; `crates/protocol/src/filters.rs` says the same at the
field.

## Formatting

This codebase is hand-formatted, and `cargo fmt` is deliberately not run.
`cargo fmt --all -- --check` currently produces 202 diff hunks across nearly
every file (and grows as the tree does — re-run the check before trusting
this number), and there is not one `#[rustfmt::skip]` in the tree — none of
that churn was ever accepted. The clearest casualty would be
`crates/server/src/audio/filter.rs`'s `COEFFICIENTS_48000`: fifteen aligned
one-line literals, kept at lavaplayer's own printed precision *specifically to
stay diffable against `Equalizer.java`*, which `rustfmt` would reflow into
roughly 75 lines and make un-diffable. One line of documentation here beats
hundreds of hunks of churn. If this codebase is ever reformatted,
`#[rustfmt::skip]` should go on that table first, on its own, before anything
else moves.
