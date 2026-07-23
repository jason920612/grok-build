#!/bin/bash
# Tendency probe orchestrator. Usage:
#   bash probes/run.sh [probe ...]         # default: all
#   RUNS=3 OBS_MINUTES=15 bash probes/run.sh delegation
set -u
cd "$(dirname "$0")"
source ./lib.sh

RUNS="${RUNS:-1}"
ALL=(delegation waiting honesty stuck maintenance)
PROBES=("$@"); [ ${#PROBES[@]} -eq 0 ] && PROBES=("${ALL[@]}")
mkdir -p results
STAMP=$(date +%Y%m%dT%H%M%S)
COMBINED="results/profile-$STAMP.json"
echo "[" > "$COMBINED"
first=1

for probe in "${PROBES[@]}"; do
  [ -f "probe_$probe.sh" ] || { echo "unknown probe: $probe" >&2; continue; }
  for run in $(seq 1 "$RUNS"); do
    echo "=== probe:$probe run:$run (window ${OBS_MINUTES}m) ==="
    # Each probe script defines: NAME, MARKER, WORKDIR, GOAL, and optionally EXTRA_ENV.
    NAME=""; MARKER=""; WORKDIR=""; GOAL=""; EXTRA_ENV=""
    unset -f seed_workdir 2>/dev/null
    source "./probe_$probe.sh"
    # Clean slate FIRST, then let a probe seed fixture files (payload files,
    # deterministic walls). Ordering matters: seeding must survive the wipe.
    rm -rf "$WORKDIR"; mkdir -p "$WORKDIR"
    declare -f seed_workdir >/dev/null && seed_workdir
    # shellcheck disable=SC2086
    launch "$NAME" "$GOAL" $EXTRA_ENV
    MODEL=$(model_label "$NAME"); MODEL="${MODEL:-unknown}"
    S=$(observe "$MARKER")
    if [ -z "$S" ]; then
      echo "  !! session never materialized" >&2
      teardown "$NAME"; continue
    fi
    OUT="results/$MODEL-$probe-$STAMP-r$run.json"
    { echo "{\"probe\":\"$probe\",\"model\":\"$MODEL\",\"run\":$run,\"metrics\":";
      bash ./extract.sh "$S" "$WORKDIR"; echo "}"; } > "$OUT"
    echo "  -> $OUT"
    cat "$OUT"
    [ $first -eq 0 ] && echo "," >> "$COMBINED"; first=0
    cat "$OUT" >> "$COMBINED"
    teardown "$NAME"
  done
done
echo "]" >> "$COMBINED"
echo "profile: $COMBINED"
