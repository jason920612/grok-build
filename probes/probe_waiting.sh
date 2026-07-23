#!/bin/bash
# Waiting tendency: real timed gaps. Measures declaration latency (cap-note
# exposures before the first waiting declaration), polling wakes, incubation.
NAME="probe-wait"
MARKER="Keelung"
WORKDIR="$HOME/probe-wait"
EXTRA_ENV="GROK_INCUBATION_MIN_WAIT_SECS=60 GROK_INCUBATION_MIN_INTERVAL_SECS=240"
GOAL="Collect real weather observations for Keelung from wttr.in (curl https://wttr.in/Keelung?format=j1): at least 3 samples spaced about 4 minutes apart, then write a one-page trend summary under ~/probe-wait/ citing the sampled numbers. Verify the summary exists before finishing."
