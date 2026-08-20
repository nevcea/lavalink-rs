#!/usr/bin/env bash
# Starts the node in the background, waits for it to come up, then runs
# test-bot in the foreground. Ctrl+C stops both.
set -euo pipefail
cd "$(dirname "$0")/.."

no_build=false
if [ "${1:-}" = "--no-build" ]; then
  no_build=true
  shift
fi
if [ "$#" -ne 0 ]; then
  echo "usage: scripts/dev.sh [--no-build]" >&2
  exit 2
fi

[ -f .env ] && set -a && source .env && set +a

: "${DISCORD_TOKEN:?DISCORD_TOKEN is not set (export it or put it in .env) — see crates/test-bot/README.md}"
: "${TEST_GUILD_ID:?TEST_GUILD_ID is not set (export it or put it in .env) — see crates/test-bot/README.md}"
: "${LAVALINK_HOST:=localhost:2333}"
host="${LAVALINK_HOST%%:*}"
port="${LAVALINK_HOST##*:}"

suffix=
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) suffix=.exe ;;
esac
target_dir="${CARGO_TARGET_DIR:-target}"
server="$target_dir/release/lavalink-server$suffix"
bot="$target_dir/debug/lavalink-test-bot$suffix"

if ! "$no_build"; then
  cargo build --release -p lavalink-server
  cargo build -p lavalink-test-bot
fi
[ -x "$server" ] && [ -x "$bot" ] || {
  echo "built server/test-bot not found; run scripts/dev.sh once without --no-build" >&2
  exit 1
}

"$server" application.yml &
node_pid=$!
trap 'kill "$node_pid" 2>/dev/null' EXIT

# No deadline: startup time varies with the host and enabled sources.
echo "waiting for node at $LAVALINK_HOST ..."
until (exec 3<>"/dev/tcp/$host/$port") 2>/dev/null; do
  kill -0 "$node_pid" 2>/dev/null || { echo "node exited before coming up" >&2; exit 1; }
  sleep 1
done
echo "node is up"

"$bot"
