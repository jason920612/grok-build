//! Guardrails — the removable "weak-model babysitting" layer, collected
//! behind one switchboard.
//!
//! Every mechanism here exists because current models take the path of
//! least resistance (blocking turns instead of waiting, declaring done
//! without verifying, fabricating citations). None of them should be
//! load-bearing for a sufficiently disciplined model — so they are all
//! toggleable from one place and can be retired wholesale:
//!
//! - `GROK_GUARDRAILS=0` (or `off`) disables every guardrail at once.
//! - `GROK_GUARDRAIL_<NAME>=0|1` overrides one flag (e.g.
//!   `GROK_GUARDRAIL_FRESH_EVIDENCE=0`).
//! - Legacy alias: `GROK_VERIFY_FIRST=0` still disables `verify_first`.
//!
//! Call sites consult [`guardrails()`] and stay one-line checks, so
//! deleting a guardrail later means deleting its flag and its single
//! branch — no archaeology.

use std::sync::OnceLock;

/// Snapshot of every guardrail toggle, resolved once per process.
#[derive(Debug, Clone, Copy)]
pub struct Guardrails {
    /// Planning/delegation tools refused until the agent observed current
    /// state this turn (dispatch layer; needs a blackboard-configured
    /// session).
    pub verify_first: bool,
    /// `update_goal(completed: true)` refused while the latest edit has
    /// no later execute call.
    pub fresh_evidence: bool,
    /// `run_terminal_cmd` forces auto-backgrounding while the goal loop
    /// is active so foreground blocking cannot starve the goal loop.
    pub goal_auto_background: bool,
    /// Blocking task-output waits are capped much shorter while the goal
    /// loop is active (declare `waiting_on` instead of holding the turn).
    pub goal_block_cap: bool,
    /// `board_post` spot-checks `file:line` evidence citations against
    /// the real filesystem.
    pub evidence_spot_check: bool,
    /// A repeat stuck episode (same tool failing with the same error class,
    /// twice over the threshold) hard-blocks mutation/execution until the
    /// agent posts a reflective board entry. Soft stuck guidance (appended
    /// to the triggering error) is permanent capability and not gated here.
    pub stuck_gate: bool,
    /// When a mission phase activates with no verified reconnaissance on
    /// the board, `map_update`'s ack includes a ready-to-sign scout order
    /// draft. Elicits delegation from models whose post-training tendency
    /// is to do everything solo — the model stays the commander (execute,
    /// adapt, or decline); pure advisory text, no gate.
    pub adjutant: bool,
    /// Structural prompt-injection defense on the tool-return path:
    /// neutralize forged harness markers in all tool output, and fence
    /// external-source payloads (web/read) with an unforgeable provenance
    /// delimiter + data anchor. Counters the model's own trust in tool
    /// returns being turned against it.
    pub antiinjection: bool,
}

impl Default for Guardrails {
    fn default() -> Self {
        Self {
            verify_first: true,
            fresh_evidence: true,
            goal_auto_background: true,
            goal_block_cap: true,
            evidence_spot_check: true,
            stuck_gate: true,
            adjutant: true,
            antiinjection: true,
        }
    }
}

fn env_flag(name: &str) -> Option<bool> {
    match std::env::var(name).ok()?.to_ascii_lowercase().as_str() {
        "0" | "off" | "false" | "no" => Some(false),
        "1" | "on" | "true" | "yes" => Some(true),
        _ => None,
    }
}

fn resolve_from_env() -> Guardrails {
    let master = env_flag("GROK_GUARDRAILS");
    let base = match master {
        Some(false) => Guardrails {
            verify_first: false,
            fresh_evidence: false,
            goal_auto_background: false,
            goal_block_cap: false,
            evidence_spot_check: false,
            stuck_gate: false,
            adjutant: false,
            antiinjection: false,
        },
        _ => Guardrails::default(),
    };
    let mut g = base;
    if let Some(v) = env_flag("GROK_GUARDRAIL_VERIFY_FIRST") {
        g.verify_first = v;
    }
    // Legacy alias predating the switchboard.
    if let Some(v) = env_flag("GROK_VERIFY_FIRST") {
        g.verify_first = v;
    }
    if let Some(v) = env_flag("GROK_GUARDRAIL_FRESH_EVIDENCE") {
        g.fresh_evidence = v;
    }
    if let Some(v) = env_flag("GROK_GUARDRAIL_GOAL_AUTO_BACKGROUND") {
        g.goal_auto_background = v;
    }
    if let Some(v) = env_flag("GROK_GUARDRAIL_GOAL_BLOCK_CAP") {
        g.goal_block_cap = v;
    }
    if let Some(v) = env_flag("GROK_GUARDRAIL_EVIDENCE_SPOT_CHECK") {
        g.evidence_spot_check = v;
    }
    if let Some(v) = env_flag("GROK_GUARDRAIL_STUCK_GATE") {
        g.stuck_gate = v;
    }
    if let Some(v) = env_flag("GROK_GUARDRAIL_ADJUTANT") {
        g.adjutant = v;
    }
    if let Some(v) = env_flag("GROK_GUARDRAIL_ANTIINJECTION") {
        g.antiinjection = v;
    }
    g
}

static GUARDRAILS: OnceLock<Guardrails> = OnceLock::new();

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static TEST_OVERRIDE: std::cell::Cell<Option<Guardrails>> =
        const { std::cell::Cell::new(None) };
}

/// The process-wide guardrail switchboard.
pub fn guardrails() -> Guardrails {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(g) = TEST_OVERRIDE.with(|c| c.get()) {
        return g;
    }
    *GUARDRAILS.get_or_init(resolve_from_env)
}

/// Per-thread override for tests; cleared on guard drop.
#[cfg(any(test, feature = "test-support"))]
pub fn set_guardrails_for_test(g: Guardrails) -> TestGuardrailsGuard {
    TEST_OVERRIDE.with(|c| c.set(Some(g)));
    TestGuardrailsGuard
}

#[cfg(any(test, feature = "test-support"))]
pub struct TestGuardrailsGuard;

#[cfg(any(test, feature = "test-support"))]
impl Drop for TestGuardrailsGuard {
    fn drop(&mut self) {
        TEST_OVERRIDE.with(|c| c.set(None));
    }
}

/// Cap on a single blocking task-output wait while the goal loop is
/// active: long enough for short commands to finish inline, short enough
/// that the agent cannot camp the turn — it should declare
/// `update_goal(waiting_on: ...)` and let completion wake it.
pub const GOAL_WAIT_BLOCK_CAP: std::time::Duration = std::time::Duration::from_secs(60);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_all_on_and_test_override_works() {
        let _g = set_guardrails_for_test(Guardrails {
            verify_first: false,
            ..Guardrails::default()
        });
        assert!(!guardrails().verify_first);
        assert!(guardrails().fresh_evidence);
    }
}
