# lavalink-test-bot

A Discord bot that exists to answer one question: *does audio actually arrive,
and does it sound right?*

It is an ordinary v4 client — the same role Wavelink or Lavalink.py play — so running
it against this node exercises the one path `cargo test` cannot reach.

## The split, because it is the confusing part

```text
  Discord gateway ──op4──▶ this bot ──REST `voice`──▶ the node ──RTP──▶ Discord voice
  (songbird "gateway")                                (songbird "driver")
```

The bot never touches audio. It asks Discord to move the bot user into a voice
channel, receives the `token`/`endpoint`/`sessionId` triple that comes back, and hands
it to the node's `PATCH .../players/{guildId}`. The node owns the voice connection
from there. That is why songbird appears twice in this workspace with disjoint
features: `gateway` here, `driver` in the server.

## Running

`scripts/dev.sh` (from the repo root) starts the node in the background, waits
for it to come up, then runs the bot in the foreground — Ctrl+C stops both. It
still needs `DISCORD_TOKEN` set (or in a `.env` file) and steps 2-3 below done
once. Otherwise, to run each half by hand:

1. Start the node:

   ```sh
   cp application.yml.example application.yml   # if you have not already
   cargo run -p lavalink-server -- application.yml
   ```

   Enable whichever sources you plan to test — `local: true` for a file on disk,
   `youtube: true` (needs `yt-dlp` on `PATH`) for a URL.

2. Create an application at <https://discord.com/developers/applications> and add a
   bot. No privileged intents are needed — commands are slash commands, not text, so
   the bot never reads message content.

3. Invite it with the `bot` and `applications.commands` scopes and the *Connect* and
   *Speak* permissions.

4. Run the bot:

   ```sh
   DISCORD_TOKEN=… TEST_GUILD_ID=… cargo run -p lavalink-test-bot
   ```

   or put both in a `.env` file in the repo root (or wherever you run the command
   from) and just `cargo run -p lavalink-test-bot` — it loads `.env` itself now, no
   export needed.

   `TEST_GUILD_ID` is the id of the server you invited the bot to — commands are
   registered guild-scoped there, so they show up immediately (a global registration
   can take up to an hour to propagate, unusable for iterating during development).

   `LAVALINK_HOST` (default `localhost:2333`) and `LAVALINK_PASSWORD` (default
   `youshallnotpass`) override the node it points at; both can live in `.env` too.

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

Filters are cumulative: the bot remembers the chain per guild and re-sends all of it,
because a `PATCH` carrying one filter replaces the whole chain on the wire.

## What to watch

The bot logs every node websocket message. That console is the point — it is where
the event sequence becomes checkable:

- `/join` → `playerUpdate` with `connected=true` and a real `ping`
- `/play` → `TrackStartEvent`, then `playerUpdate` with a rising `position`
- `/play` again → `TrackEndEvent reason=Replaced` before the next `TrackStartEvent`
- `/stop` → `TrackEndEvent reason=Stopped`
- a track running out → `TrackEndEvent reason=Finished`, `may_start_next=true`
- kicking the bot from the channel → `WebSocketClosedEvent` with code `4014`

A message the bot cannot parse is logged at `error` with the raw text, since that is
either a node bug or a protocol gap rather than something to skip quietly.

Set `RUST_LOG=debug` to see `stats` and non-text frames as well.
