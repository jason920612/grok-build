//! Incubation — the session's "default mode network".
//!
//! Human cognition alternates between a task-positive mode (focused work)
//! and the default mode network (mind-wandering, incubation) — and most
//! divergent insight comes from the latter. Agent harnesses only ever run
//! the focused mode; this module adds the other half: when the session
//! enters a declared wait, a cheap READ-ONLY subagent is spawned in the
//! background to wander over the mission map, the blackboard, and the
//! workspace, and drop at most a few `idea` entries into the silent idea
//! box.
//!
//! Two triggers share this module (and one rate limiter):
//! - the shell's goal wait window (`update_goal(waiting_on: ...)`), and
//! - `map_update` marking a phase `waiting` (works outside goal mode too).
//!
//! Non-negotiable containment (so incubation can never recreate the
//! goal-system nagging pathology on a legitimately waiting agent):
//! - The incubation agent is mechanically read-only
//!   (`SubagentCapabilityMode::ReadOnly`) — it cannot mutate anything.
//! - Its completion is never surfaced to the main model
//!   (`surface_completion: false`); its only artifact is board entries.
//! - `idea` entries are excluded from digests and default board reads —
//!   they surface at decision points, not on wake-up.
//! - Only a wait expected to be long triggers it, rate-limited per session.
//!
//! Escape hatch with a deliberately high bar: if incubation finds concrete
//! grounds that the WAIT PREMISE ITSELF is wrong, it may post a `question`
//! (which does digest) — phrased as a question, never a directive.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use crate::implementations::grok_build::task::types::{
    SubagentEvent, SubagentRequest, SubagentRuntimeOverrides,
};

/// Same role subagent type as the other harness-internal roles.
const INCUBATION_SUBAGENT_TYPE: &str = "general-purpose";
const INCUBATION_DESCRIPTION: &str = "Incubation: background divergent thinking during a wait";

/// Waits shorter than this don't incubate (not enough time to think).
const MIN_WAIT_SECS: u64 = 120;
/// At most one incubation run per session per this interval.
const MIN_INTERVAL: Duration = Duration::from_secs(3600);

fn min_wait_secs() -> u64 {
    std::env::var("GROK_INCUBATION_MIN_WAIT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(MIN_WAIT_SECS)
}

fn min_interval() -> Duration {
    std::env::var("GROK_INCUBATION_MIN_INTERVAL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(MIN_INTERVAL)
}

fn enabled() -> bool {
    !matches!(
        std::env::var("GROK_INCUBATION").ok().as_deref(),
        Some("0") | Some("off") | Some("false")
    )
}

/// Per-session last-run stamps. Module-local static keyed by session id so
/// both triggers (goal wait, map_update waiting) share one budget without
/// threading state through either host.
static LAST_RUN: LazyLock<parking_lot::Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| parking_lot::Mutex::new(HashMap::new()));

/// Gate + rate limit. Records the run when it returns true.
pub fn should_incubate(session_id: &str, wait_secs: u64) -> bool {
    if !enabled() || wait_secs < min_wait_secs() {
        return false;
    }
    let mut last = LAST_RUN.lock();
    if last
        .get(session_id)
        .is_some_and(|t| t.elapsed() < min_interval())
    {
        return false;
    }
    last.insert(session_id.to_string(), Instant::now());
    true
}

/// The mind-wandering protocol prompt.
fn incubation_prompt(waiting_on: &str, wait_secs: u64, mission_path: Option<&str>) -> String {
    let map_step = match mission_path {
        Some(p) => format!(
            "Read the mission map at {p} (read_file; it may not exist yet) and the shared \
             blackboard (board_read with includeIdeas: true) to absorb where the team is."
        ),
        None => "Read the shared blackboard (board_read with includeIdeas: true) to absorb \
                 where the team is."
            .to_string(),
    };
    format!(
        r#"You are the team's incubation agent — its default mode network. The main agent is in a declared wait ("{waiting_on}", ~{wait_secs}s window), so you think in the background while nothing else happens. You are read-only.

Protocol:
1. {map_step} Skim the workspace where useful; do not re-verify what the board already settled.
2. Wander deliberately around the mission's current bottleneck and next phases:
   - analogies: what known problem is this isomorphic to, and how was THAT solved?
   - inversions: which standing assumption, if wrong, changes the plan most? What would falsify it cheaply?
   - simplifications: what would a 10x simpler approach look like? What could be deleted instead of built?
   - blind spots: which risk or second-order effect is nobody tracking?
3. Post AT MOST 3 of your best thoughts as board_post kind=idea (topic = the phase or bottleneck each addresses; body = the idea plus how to cheaply test it). Quality over quantity — an obvious or generic idea is worse than none. Ideas land in the silent idea box; nobody is forced to act on them.
4. ONLY IF you find concrete grounds that the wait premise itself is wrong (what "{waiting_on}" waits for is already finished, impossible, or unnecessary), post kind=question explaining the evidence — that is the only entry that interrupts the team. Never use it for ordinary ideas.

Do not modify anything. Do not spawn subagents. End with a one-line summary of what you posted."#
    )
}

/// Fire-and-forget spawn of the incubation subagent. The result is consumed
/// by a detached task purely for tracing — board entries are the artifact.
#[allow(clippy::too_many_arguments)]
pub fn spawn_incubation(
    event_tx: tokio::sync::mpsc::UnboundedSender<SubagentEvent>,
    parent_session_id: String,
    parent_prompt_id: Option<String>,
    cwd: Option<String>,
    waiting_on: String,
    wait_secs: u64,
    mission_path: Option<String>,
) {
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let id = uuid::Uuid::now_v7().to_string();
    let request = SubagentRequest {
        id: id.clone(),
        prompt: incubation_prompt(&waiting_on, wait_secs, mission_path.as_deref()),
        description: INCUBATION_DESCRIPTION.to_string(),
        subagent_type: INCUBATION_SUBAGENT_TYPE.to_string(),
        parent_session_id,
        parent_prompt_id,
        resume_from: None,
        cwd,
        runtime_overrides: SubagentRuntimeOverrides {
            capability_mode: Some(xai_tool_types::SubagentCapabilityMode::ReadOnly),
            ..Default::default()
        },
        run_in_background: false,
        // Harness-internal: the waiting model must never be woken or
        // notified by incubation — its ideas wait silently in the idea box.
        surface_completion: false,
        fork_context: false,
        result_tx,
    };
    if event_tx
        .send(SubagentEvent::Spawn(Box::new(request)))
        .is_err()
    {
        tracing::debug!("incubation: subagent coordinator channel closed; skipping");
        return;
    }
    tracing::info!(subagent_id = %id, waiting_on = %waiting_on, "incubation agent spawned");
    tokio::spawn(async move {
        match result_rx.await {
            Ok(res) if res.success => {
                tracing::info!(subagent_id = %id, turns = res.turns, "incubation agent completed")
            }
            Ok(res) => {
                tracing::info!(subagent_id = %id, error = ?res.error, "incubation agent failed")
            }
            Err(_) => tracing::debug!(subagent_id = %id, "incubation result channel dropped"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_and_threshold() {
        let sid = format!("test-session-{}", std::process::id());
        assert!(!should_incubate(&sid, 30), "short waits never incubate");
        assert!(should_incubate(&sid, 600), "long wait incubates");
        assert!(
            !should_incubate(&sid, 600),
            "second run within the interval is rate-limited"
        );
    }

    #[test]
    fn prompt_contains_containment_rules() {
        let p = incubation_prompt("cron samples", 300, Some("/tmp/mission.json"));
        assert!(p.contains("AT MOST 3"));
        assert!(p.contains("kind=idea"));
        assert!(p.contains("read-only"));
        assert!(p.contains("/tmp/mission.json"));
        assert!(p.contains("Do not spawn subagents"));
    }
}
