# lavalink-test-bot

A Discord bot that answers one question: *does audio actually arrive, and
does it sound right?* It's an ordinary v4 client — the same role Wavelink or
Lavalink.py play — exercising the one path `cargo test` can't reach.

## The split

```text
  Discord gateway ──op4──▶ this bot ──REST `voice`──▶ the node ──RTP──▶ Discord voice
  (songbird "gateway")                                (songbird "driver")
```

The bot never touches audio. It asks Discord to move the bot user into a
voice channel, gets back a `token`/`endpoint`/`sessionId` triple, and hands
it to the node's `PATCH .../players/{guildId}` — the node owns the voice
connection from there. That's why songbird appears twice in this workspace
with disjoint features: `gateway` here, `driver` in the server.

## Running

`scripts/dev.sh` (repo root) starts the node in the background, waits for it
to come up, then runs the bot in the foreground — Ctrl+C stops both. Needs
`DISCORD_TOKEN` set (or in `.env`) and steps 2-3 below done once. To run each
half by hand instead:

1. `cp application.yml.example application.yml` (once), then
   `cargo run -p lavalink-server -- application.yml`. Enable whichever
   sources you're testing — `local: true` for a file on disk, `youtube: true`
   (needs `yt-dlp` on `PATH`) for a URL.
2. Create an application + bot at
   <https://discord.com/developers/applications> (no privileged intents —
   commands are slash commands) and invite it with `bot` +
   `applications.commands` scopes and *Connect*/*Speak* permissions.
3. `DISCORD_TOKEN=… TEST_GUILD_ID=… cargo run -p lavalink-test-bot`, or put
   both in a repo-root `.env` and just `cargo run -p lavalink-test-bot`.
   `TEST_GUILD_ID` is the server you invited it to — guild-scoped commands
   show up immediately (global registration can take an hour, unusable for
   iterating). `LAVALINK_HOST` (`localhost:2333`) and `LAVALINK_PASSWORD`
   (`youshallnotpass`) override the node it points at; both can live in
   `.env` too.

## Commands

| | |
|---|---|
| `/join` / `/leave` | move the bot; hand the credentials to the node |
| `/play <query>` | load and play — a url, `ytsearch:…`, `scsearch:…`, or a local path |
| `/search <query>` | load without playing — checks a source without a voice channel |
| `/stop` `/pause` `/resume` `/seek <seconds>` `/volume <0-1000>` | player control |
| `/np` | this guild's player |
| `/players` | every player on the node |
| `/eq <band 0-14> <gain>` `/lowpass <smoothing>` `/clearfilters` `/filters` | the DSP chain |
| `/ping` | gateway (Discord) latency, from serenity's shard heartbeat |
| `/info` `/stats` | node identity and counters |

Filters are cumulative: the bot remembers the chain per guild and re-sends
all of it, since a `PATCH` carrying one filter replaces the whole chain on
the wire.

## What to watch

The bot logs every node websocket message — that's where the event sequence
becomes checkable:

- `/join` → `playerUpdate` with `connected=true` and a real `ping`
- `/play` → `TrackStartEvent`, then `playerUpdate` with a rising `position`
- `/play` again → `TrackEndEvent reason=Replaced` before the next `TrackStartEvent`
- `/stop` → `TrackEndEvent reason=Stopped`
- a track running out → `TrackEndEvent reason=Finished`, `may_start_next=true`
- kicking the bot from the channel → `WebSocketClosedEvent` with code `4014`

A message the bot can't parse is logged at `error` with the raw text — a
node bug or protocol gap, not something to skip quietly.

Set `RUST_LOG=debug` to see `stats` and non-text frames as well.
