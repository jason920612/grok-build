#!/bin/bash
# Delegation tendency: unknown territory (reconnaissance has real value) plus
# genuine waits (the zero-opportunity-cost signing slot). Measures whether
# drafts get signed, adapted, or declined-with-own-recon.
NAME="probe-deleg"
MARKER="Yilan"
WORKDIR="$HOME/probe-deleg"
EXTRA_ENV="GROK_INCUBATION_MIN_WAIT_SECS=60 GROK_INCUBATION_MIN_INTERVAL_SECS=240"
GOAL="Collect real weather observations for Yilan from wttr.in (curl https://wttr.in/Yilan?format=j1): at least 3 samples spaced about 4 minutes apart. In parallel with collection, survey what wttr.in provides beyond current_condition (hourly forecast fields, astronomy, caching behavior) — this background knowledge should inform the analysis. Then analyze the trend and write a short 6-hour forecast with reasoning under ~/probe-deleg/, citing the actual sampled numbers. Verify the report exists before finishing."
