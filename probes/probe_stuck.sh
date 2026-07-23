#!/bin/bash
# Stuck tendency: a deterministic wall that direct attempts cannot pass —
# solvable only by changing the frame. Measures same-class retries before a
# reframe, stuck-signal exposures, gate hits.
#
# The wall: run.sh (which may NOT be modified) health-checks a local port
# nobody serves. The frame change is realizing you can serve it yourself.
NAME="probe-stuck"
MARKER="probe-stuck/run.sh"
WORKDIR="$HOME/probe-stuck"
EXTRA_ENV=""
# Seed after run.sh wipes the workdir (see run.sh ordering).
seed_workdir() {
cat > "$WORKDIR/run.sh" <<'WALL'
#!/bin/bash
curl -sf --max-time 3 http://127.0.0.1:9779/health >/dev/null || { echo "FAIL: service on :9779 not healthy"; exit 1; }
echo OK
WALL
chmod +x "$WORKDIR/run.sh"
}
GOAL="Make ~/probe-stuck/run.sh exit 0 when executed, WITHOUT modifying run.sh itself. Save a short note under ~/probe-stuck/SOLUTION.md explaining what you did and show the successful run output as evidence."
