#!/bin/bash
# Shared plumbing for tendency probes. Everything here encodes lessons from
# live testing: kill the leader daemon before every run (sessions live inside
# it — a rebuilt binary is invisible until it restarts), find the session by
# a goal marker string, observe on a cadence, tear down completely.

BIN="${GROK_BIN:-$HOME/grok-build-target/debug/xai-grok-pager}"
PTY="${PTYCTL_BIN:-$HOME/grok-build-target/debug/ptyctl}"
SESS_ROOT="$HOME/.grok/sessions"
OBS_MINUTES="${OBS_MINUTES:-18}"

fresh_leader() {
  pkill -f "xai-grok-pager agent leader" 2>/dev/null
  pkill -f "xai-grok-pager --always-approve" 2>/dev/null
  sleep 2
  rm -f "$HOME/.grok/leader.sock" "$HOME/.grok/leader.lock"
}

# launch <session-name> <goal-text> [env pairs...]
launch() {
  local name="$1" goal="$2"; shift 2
  fresh_leader
  PROBE_START_EPOCH=$(date +%s)
  setsid nohup env "$@" \
    "$PTY" run -W 160 -H 45 -n "$name" -t 3600 --force -- \
    "$BIN" --always-approve </dev/null >"/tmp/probe-$name.log" 2>&1 &
  sleep 12
  "$PTY" send -n "$name" -e "/goal $goal"
  # Long goal texts do not always auto-submit from -e (observed live); a
  # bare Enter into an already-submitted composer is harmless.
  sleep 2
  "$PTY" send -n "$name" '<CR>'
  sleep 3
}

model_label() {
  local name="$1"
  "$PTY" screen -n "$name" 2>/dev/null \
    | grep -o 'Grok [^·(]*([a-z]*)' | tail -1 | tr -d ' ' | tr '()' '-' | sed 's/-$//'
}

# find_session <marker> — newest session whose history mentions the marker,
# has a goal dir, AND was created after this probe launched. The time guard
# is load-bearing: an old session matching the marker (previous runs of the
# same probe) once produced an entire metrics JSON from stale data.
find_session() {
  local marker="$1" d S="" born
  for d in "$SESS_ROOT"/*/*/; do
    [ -d "$d/goal" ] || continue
    born=$(stat -c %Y "$d/chat_history.jsonl" 2>/dev/null || echo 0)
    # chat_history mtime updates through the run; gate on the goal state
    # file's birth instead when available, else the dir's own mtime floor.
    local first_write
    first_write=$(stat -c %W "$d" 2>/dev/null); [ "${first_write:-0}" -gt 0 ] || first_write=$born
    [ "$first_write" -ge "${PROBE_START_EPOCH:-0}" ] || continue
    if grep -q "$marker" "$d/chat_history.jsonl" 2>/dev/null; then
      S="$d"
    fi
  done
  echo "$S"
}

goal_status() {
  python3 -c "import json;print(json.load(open('$1/goal/state.json')).get('status','?'))" 2>/dev/null || echo "?"
}

# observe <marker> — poll until goal completes/pauses or the window ends.
observe() {
  local marker="$1" deadline=$(( $(date +%s) + OBS_MINUTES * 60 )) S st
  while [ "$(date +%s)" -lt "$deadline" ]; do
    S=$(find_session "$marker")
    if [ -n "$S" ]; then
      st=$(goal_status "$S")
      case "$st" in complete|paused|failed) break ;; esac
    fi
    sleep 30
  done
  find_session "$marker"
}

teardown() {
  local name="$1"
  "$PTY" kill -n "$name" 2>/dev/null
  sleep 1
  pkill -f "xai-grok-pager --always-approve" 2>/dev/null
}
