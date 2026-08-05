#!/usr/bin/env bash
# Starts the node in the background, waits for it to come up, then runs
# test-bot in the foreground. Ctrl+C stops both.
set -euo pipefail
cd "$(dirname "$0")/.."

[ -f .env ] && set -a && source .env && set +a

: "${DISCORD_TOKEN:?DISCORD_TOKEN is not set (export it or put it in .env) — see crates/test-bot/README.md}"
: "${TEST_GUILD_ID:?TEST_GUILD_ID is not set (export it or put it in .env) — see crates/test-bot/README.md}"
: "${LAVALINK_HOST:=localhost:2333}"
host="${LAVALINK_HOST%%:*}"
port="${LAVALINK_HOST##*:}"

cargo run -p lavalink-server --release -- application.yml &
node_pid=$!
trap 'kill "$node_pid" 2>/dev/null' EXIT

# No deadline: a cold --release build can outrun any fixed timeout.
echo "waiting for node at $LAVALINK_HOST ..."
until (exec 3<>"/dev/tcp/$host/$port") 2>/dev/null; do
  kill -0 "$node_pid" 2>/dev/null || { echo "node exited before coming up" >&2; exit 1; }
  sleep 1
done
echo "node is up"

cargo run -p lavalink-test-bot
