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
  setsid nohup env "$@" \
    "$PTY" run -W 160 -H 45 -n "$name" -t 3600 --force -- \
    "$BIN" --always-approve </dev/null >"/tmp/probe-$name.log" 2>&1 &
  sleep 12
  "$PTY" send -n "$name" -e "/goal $goal"
  sleep 3
}

model_label() {
  local name="$1"
  "$PTY" screen -n "$name" 2>/dev/null \
    | grep -o 'Grok [^·(]*([a-z]*)' | tail -1 | tr -d ' ' | tr '()' '-' | sed 's/-$//'
}

# find_session <marker> — newest session whose history mentions the marker
# and that has a goal dir.
find_session() {
  local marker="$1" d S=""
  for d in "$SESS_ROOT"/*/*/; do
    if [ -d "$d/goal" ] && grep -q "$marker" "$d/chat_history.jsonl" 2>/dev/null; then
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
