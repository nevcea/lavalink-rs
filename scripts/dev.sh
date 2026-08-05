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

echo "waiting for node at $LAVALINK_HOST ..."
for _ in $(seq 1 60); do
  kill -0 "$node_pid" 2>/dev/null || { echo "node exited before coming up" >&2; exit 1; }
  (exec 3<>"/dev/tcp/$host/$port") 2>/dev/null && break
  sleep 1
done
(exec 3<>"/dev/tcp/$host/$port") 2>/dev/null || { echo "node didn't come up within 60s" >&2; exit 1; }
echo "node is up"

cargo run -p lavalink-test-bot
