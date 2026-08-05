# Maintenance notes

Why this node's advertised feature set stops where it does. Each of these is a
deliberate omission, checked at compile time or surfaced to clients honestly
(`/v4/info`, a 400, or a 501) rather than approximated or stubbed silently.

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

## `/metrics` — not implemented

Checked against upstream's `PrometheusMetrics.java`: it registers exactly three
things — an `InstrumentedAppender` (logback event counters), Prometheus's own
`DefaultExports.initialize()` (JVM hotspot: heap, GC, thread counts), and a
histogram of GC pause times. **Zero Lavalink-specific series.** There is no JVM
here, so every metric that endpoint would expose is unimplementable by
definition — building a `/metrics` that instead exports `lavalink_players` or
similar would be a new endpoint wearing upstream's name, not a port of it.

Upstream's default is `enabled: false`, `endpoint: ""`, and its auth exemption
keys off the *configured endpoint string* — so a disabled upstream node also
just 404s there. This node's unconfigured route falls through to the same
Lavalink-shaped 404 (see the router fallback in `crates/server/src/rest/mod.rs`),
which means the disabled case is already matched exactly, at zero code. The
honest replacement for what an operator would actually want from `/metrics` is
`/v4/stats`, which — since `frameStats` and `playingPlayers` are now wired up —
carries every number a Rust node can produce truthfully.

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

## Formatting

This codebase is hand-formatted, and `cargo fmt` is deliberately not run.
`cargo fmt --all -- --check` currently produces 84 diff hunks across nearly
every file, and there is not one `#[rustfmt::skip]` in the tree — none of that
churn was ever accepted. The clearest casualty would be
`crates/server/src/audio/filter.rs`'s `COEFFICIENTS_48000`: fifteen aligned
one-line literals, kept at lavaplayer's own printed precision *specifically to
stay diffable against `Equalizer.java`*, which `rustfmt` would reflow into
roughly 75 lines and make un-diffable. One line of documentation here beats 84
hunks of churn. If this codebase is ever reformatted, `#[rustfmt::skip]` should
go on that table first, on its own, before anything else moves.
