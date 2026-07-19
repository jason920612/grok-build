//! Compass — the mission map that gives agents long-horizon situational
//! awareness ("where am I in this task, and why does it exist").
//!
//! Current models keep no internal model of a long-running task's arc: they
//! optimize the current turn, declare goals done without perspective, and
//! cannot tell "physically waiting" from "stuck". The compass externalizes
//! that missing model into a data structure the framework maintains and
//! feeds back:
//!
//! - **Mission map** (`mission.json`, next to `blackboard.jsonl`, shared by
//!   the whole session tree): north star + why, phases with status
//!   (pending/active/waiting/done), open questions, assumptions, risks.
//!   Self-reported via `map_update`, mechanically validated: phase
//!   completion requires evidence (spot-checked), timestamps are
//!   framework-stamped, only one phase can be active.
//! - **Orientation** ([`OrientationReminder`]): a throttled "you are here"
//!   block injected after tool calls — current phase, elapsed vs estimate,
//!   idea-box count, and metacognitive drift signals. During a `waiting`
//!   phase it reinforces that waiting is valid work instead of nagging.
//! - **Stuck detection** ([`StuckTracker`]): consecutive same-class failures
//!   of the same tool are a mechanical impasse signal. Episode 1 appends
//!   divergence guidance to the error (soft). A repeat episode arms a hard
//!   gate (guardrail `stuck_gate`) that refuses further mutation/execution
//!   until the agent posts a reflective board entry. Wait/poll tools are
//!   NEVER fingerprinted — polling is not a loop.
//!
//! The map and orientation are permanent cognitive capability (strong
//! models benefit too); only the stuck hard gate is a removable guardrail.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::types::output::ToolOutput;
use crate::types::tool::{Reminder, ToolKind, ToolNamespace};

use super::blackboard::{BlackboardCfg, EntryKind, spot_check_evidence};

// ── Mission model ───────────────────────────────────────────────────────

/// Lifecycle of one mission phase.
///
/// `Waiting` is a first-class state, not a gap: while the active phase is
/// waiting, orientation reinforces the wait and stuck detection stays quiet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PhaseStatus {
    Pending,
    Active,
    /// Physically blocked on an external process (cron, long build, another
    /// party). Correct behavior is to do nothing and process results later.
    Waiting,
    Done,
}

impl PhaseStatus {
    pub const fn tag(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Waiting => "waiting",
            Self::Done => "done",
        }
    }
}

/// One phase/milestone of the mission.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Phase {
    pub title: String,
    pub status: PhaseStatus,
    /// Model's own duration estimate — the reference for drift signals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub est_minutes: Option<u64>,
    /// Framework-stamped when the phase first becomes active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_ts: Option<String>,
    /// Framework-stamped when the phase is marked done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_ts: Option<String>,
    /// Evidence backing completion (required for `done`, spot-checked).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
    /// Free-form context, e.g. what a waiting phase is waiting on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The whole mission map (`mission.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mission {
    /// The end state this whole task exists to reach.
    pub north_star: String,
    /// WHY the task exists — the defense against superficially "done" work.
    pub why: String,
    #[serde(default)]
    pub phases: Vec<Phase>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_questions: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assumptions: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub risks: Vec<String>,
    pub created_ts: String,
    pub updated_ts: String,
}

impl Mission {
    /// Index of the single active-or-waiting phase, if any.
    pub fn current_phase(&self) -> Option<usize> {
        self.phases
            .iter()
            .position(|p| matches!(p.status, PhaseStatus::Active | PhaseStatus::Waiting))
    }

    /// True when the mission's current phase is a declared physical wait.
    pub fn is_waiting(&self) -> bool {
        self.current_phase()
            .is_some_and(|i| self.phases[i].status == PhaseStatus::Waiting)
    }
}

// ── Storage ─────────────────────────────────────────────────────────────

/// The mission map lives next to the blackboard so the whole session tree
/// shares one map with zero extra plumbing. `None` when no blackboard is
/// configured (hosts that did not opt into the collaboration discipline).
pub fn mission_path(res: &crate::types::resources::Resources) -> Option<PathBuf> {
    let cfg = res.get::<BlackboardCfg>()?;
    Some(cfg.path.parent()?.join("mission.json"))
}

fn read_mission_sync(path: &Path) -> std::io::Result<Option<Mission>> {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    fs2::FileExt::lock_shared(&file)?;
    let mut raw = String::new();
    let result = file.read_to_string(&mut raw);
    let _ = fs2::FileExt::unlock(&file);
    result?;
    Ok(serde_json::from_str(&raw).ok())
}

fn write_mission_sync(path: &Path, mission: &Mission) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(path)?;
    fs2::FileExt::lock_exclusive(&file)?;
    let result = (|| {
        let json = serde_json::to_string_pretty(mission).map_err(std::io::Error::other)?;
        file.set_len(0)?;
        let mut f = &file;
        std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(0))?;
        f.write_all(json.as_bytes())?;
        f.flush()
    })();
    let _ = fs2::FileExt::unlock(&file);
    result
}

pub async fn read_mission(path: PathBuf) -> std::io::Result<Option<Mission>> {
    tokio::task::spawn_blocking(move || read_mission_sync(&path))
        .await
        .map_err(|e| std::io::Error::other(format!("mission read task panicked: {e}")))?
}

async fn write_mission(path: PathBuf, mission: Mission) -> std::io::Result<()> {
    tokio::task::spawn_blocking(move || write_mission_sync(&path, &mission))
        .await
        .map_err(|e| std::io::Error::other(format!("mission write task panicked: {e}")))?
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn minutes_since(ts: &str) -> Option<u64> {
    let then = chrono::DateTime::parse_from_rfc3339(ts).ok()?;
    let mins = (chrono::Utc::now() - then.with_timezone(&chrono::Utc)).num_minutes();
    Some(mins.max(0) as u64)
}

// ── Rendering ───────────────────────────────────────────────────────────

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

/// Full multi-line rendering used by `map_read` and `map_update` acks.
fn render_mission(mission: &Mission, idea_count: usize) -> String {
    let mut out = format!(
        "MISSION MAP\nNorth star: {}\nWhy: {}\n",
        mission.north_star, mission.why
    );
    if mission.phases.is_empty() {
        out.push_str("Phases: none yet — add them with map_update(addPhases).\n");
    } else {
        out.push_str("Phases:\n");
        for (i, p) in mission.phases.iter().enumerate() {
            let mut line = format!("  {}. [{}] {}", i + 1, p.status.tag(), p.title);
            match p.status {
                PhaseStatus::Active | PhaseStatus::Waiting => {
                    if let Some(started) = &p.started_ts
                        && let Some(mins) = minutes_since(started)
                    {
                        line.push_str(&format!(" — {mins}m elapsed"));
                        if let Some(est) = p.est_minutes {
                            line.push_str(&format!(" (est {est}m)"));
                        }
                    }
                }
                PhaseStatus::Done => {
                    if !p.evidence.is_empty() {
                        line.push_str(&format!(" — evidence: {}", p.evidence.join(" | ")));
                    }
                }
                PhaseStatus::Pending => {}
            }
            if let Some(note) = &p.note {
                line.push_str(&format!("\n     note: {note}"));
            }
            out.push_str(&line);
            out.push('\n');
        }
    }
    let list = |label: &str, items: &[String], out: &mut String| {
        if !items.is_empty() {
            out.push_str(&format!("{label}:\n"));
            for (i, q) in items.iter().enumerate() {
                out.push_str(&format!("  {}. {}\n", i + 1, q));
            }
        }
    };
    list("Open questions", &mission.open_questions, &mut out);
    list("Assumptions", &mission.assumptions, &mut out);
    list("Risks", &mission.risks, &mut out);
    if idea_count > 0 {
        out.push_str(&format!(
            "Idea box: {idea_count} unverified ideas (board_read kind=idea) — consult at decision points.\n"
        ));
    }
    out
}

/// Count idea-box entries on the shared board (best-effort; 0 on error).
async fn idea_count(res: &crate::types::resources::SharedResources) -> usize {
    let path = {
        let r = res.lock().await;
        r.get::<BlackboardCfg>().map(|c| c.path.clone())
    };
    let Some(path) = path else { return 0 };
    super::blackboard::read_entries_for_compass(path)
        .await
        .map(|entries| entries.iter().filter(|e| e.kind == EntryKind::Idea).count())
        .unwrap_or(0)
}

// ── map_update ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MapUpdateInput {
    #[schemars(
        description = "The end state this task exists to reach. Required on the first update that creates the map."
    )]
    #[serde(default)]
    pub north_star: Option<String>,

    #[schemars(
        description = "WHY the task exists — what the result is for. Required on the first update that creates the map."
    )]
    #[serde(default)]
    pub why: Option<String>,

    #[schemars(description = "Phase titles to append to the map (created as pending).")]
    #[serde(default)]
    pub add_phases: Vec<String>,

    #[schemars(description = "1-based index of the phase a status/estimate/note change targets.")]
    #[serde(default)]
    pub phase: Option<usize>,

    #[schemars(
        description = "New status for the targeted phase: pending | active | waiting | done. active demotes any other active phase; waiting REQUIRES note (what you are waiting on); done REQUIRES evidence."
    )]
    #[serde(default)]
    pub status: Option<PhaseStatus>,

    #[schemars(description = "Your duration estimate in minutes for the targeted phase.")]
    #[serde(default)]
    pub est_minutes: Option<u64>,

    #[schemars(
        description = "Evidence for marking a phase done: file:line locations, commands with observed output. Spot-checked against the filesystem."
    )]
    #[serde(default)]
    pub evidence: Vec<String>,

    #[schemars(
        description = "Context note for the targeted phase, e.g. what a waiting phase waits on."
    )]
    #[serde(default)]
    pub note: Option<String>,

    #[schemars(description = "Open questions to add to the map.")]
    #[serde(default)]
    pub add_questions: Vec<String>,

    #[schemars(description = "Assumptions you are working under, to add to the map.")]
    #[serde(default)]
    pub add_assumptions: Vec<String>,

    #[schemars(description = "Risks to add to the map.")]
    #[serde(default)]
    pub add_risks: Vec<String>,

    #[schemars(description = "1-based indices of open questions to remove (resolved).")]
    #[serde(default)]
    pub resolve_questions: Vec<usize>,

    #[schemars(description = "1-based indices of assumptions to remove (no longer held).")]
    #[serde(default)]
    pub resolve_assumptions: Vec<usize>,

    #[schemars(description = "1-based indices of risks to remove (mitigated).")]
    #[serde(default)]
    pub resolve_risks: Vec<usize>,
}

/// Update the mission map (self-report with mechanical validation).
#[derive(Debug, Default)]
pub struct MapUpdateTool;

impl crate::types::tool_metadata::ToolMetadata for MapUpdateTool {
    fn kind(&self) -> ToolKind {
        ToolKind::MapUpdate
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        r#"Update the mission map — the shared long-horizon picture of this task: north star, why it exists, phases with status, open questions, assumptions, risks.

Create the map early (northStar + why + addPhases) for any task with more than a couple of steps. Keep it honest as you go: mark the phase you are working on active, mark a phase waiting (with note = what you wait on) when physically blocked on an external process, and mark it done ONLY with evidence — completion claims are spot-checked. Timestamps and elapsed times are maintained by the framework, not you."#
    }
}

impl xai_tool_runtime::Tool for MapUpdateTool {
    type Args = MapUpdateInput;
    type Output = ToolOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new("map_update").expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &::xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            "map_update",
            crate::types::tool_metadata::ToolMetadata::description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: false,
            tool_scope: Some(xai_tool_protocol::ToolScope::Write),
            ..Default::default()
        }
    }

    #[tracing::instrument(name = "tool.map_update", skip_all)]
    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: MapUpdateInput,
    ) -> Result<ToolOutput, xai_tool_runtime::ToolError> {
        use crate::types::tool_metadata::shared_resources;
        let resources = shared_resources(&ctx)?;

        let (path, cwd) = {
            let res = resources.lock().await;
            let path = mission_path(&res).ok_or_else(|| {
                xai_tool_runtime::ToolError::custom(
                    "map_update",
                    "The mission map is not configured for this session (no blackboard)"
                        .to_string(),
                )
            })?;
            (
                path,
                res.get::<crate::types::resources::Cwd>().map(|c| c.0.clone()),
            )
        };

        let existing = read_mission(path.clone()).await.map_err(|e| {
            xai_tool_runtime::ToolError::custom(
                "map_update",
                format!("failed to read mission map {}: {e}", path.display()),
            )
        })?;

        let mut mission = match existing {
            Some(m) => m,
            None => {
                let (Some(ns), Some(why)) = (input.north_star.clone(), input.why.clone()) else {
                    return Err(xai_tool_runtime::ToolError::invalid_arguments(
                        "No mission map exists yet: the first map_update must set northStar \
                         (the end state) and why (what the result is for)."
                            .to_string(),
                    ));
                };
                Mission {
                    north_star: ns,
                    why,
                    phases: Vec::new(),
                    open_questions: Vec::new(),
                    assumptions: Vec::new(),
                    risks: Vec::new(),
                    created_ts: now_rfc3339(),
                    updated_ts: now_rfc3339(),
                }
            }
        };
        if let Some(ns) = &input.north_star {
            mission.north_star = ns.clone();
        }
        if let Some(why) = &input.why {
            mission.why = why.clone();
        }

        for title in &input.add_phases {
            if !title.trim().is_empty() {
                mission.phases.push(Phase {
                    title: title.clone(),
                    status: PhaseStatus::Pending,
                    est_minutes: None,
                    started_ts: None,
                    completed_ts: None,
                    evidence: Vec::new(),
                    note: None,
                });
            }
        }

        let mut phase_transition = None;
        if input.status.is_some() || input.est_minutes.is_some() || input.note.is_some() {
            let idx = match input.phase {
                Some(n) if n >= 1 && n <= mission.phases.len() => n - 1,
                Some(n) => {
                    return Err(xai_tool_runtime::ToolError::invalid_arguments(format!(
                        "phase {n} does not exist (map has {} phases).",
                        mission.phases.len()
                    )));
                }
                // Untargeted status/note changes default to the current phase.
                None => mission.current_phase().ok_or_else(|| {
                    xai_tool_runtime::ToolError::invalid_arguments(
                        "No active phase to target — pass phase (1-based index).".to_string(),
                    )
                })?,
            };

            if let Some(status) = input.status {
                match status {
                    PhaseStatus::Done => {
                        let evidence_ok = !input.evidence.iter().all(|e| e.trim().is_empty())
                            || !mission.phases[idx].evidence.is_empty();
                        if !evidence_ok {
                            return Err(xai_tool_runtime::ToolError::invalid_arguments(
                                "Marking a phase done requires evidence (file:line, command + \
                                 observed output). Verify first, then complete."
                                    .to_string(),
                            ));
                        }
                        if crate::guardrails::guardrails().evidence_spot_check {
                            let evidence = input.evidence.clone();
                            let cwd = cwd.clone();
                            let check = tokio::task::spawn_blocking(move || {
                                spot_check_evidence(&evidence, cwd.as_deref())
                            })
                            .await
                            .map_err(|e| {
                                xai_tool_runtime::ToolError::custom(
                                    "map_update",
                                    format!("evidence check task panicked: {e}"),
                                )
                            })?;
                            if let Err(msg) = check {
                                return Err(xai_tool_runtime::ToolError::invalid_arguments(msg));
                            }
                        }
                        mission.phases[idx].completed_ts = Some(now_rfc3339());
                    }
                    PhaseStatus::Waiting => {
                        if input.note.is_none() && mission.phases[idx].note.is_none() {
                            return Err(xai_tool_runtime::ToolError::invalid_arguments(
                                "Marking a phase waiting requires note = what you are waiting \
                                 on (so orientation can protect the wait)."
                                    .to_string(),
                            ));
                        }
                        if mission.phases[idx].started_ts.is_none() {
                            mission.phases[idx].started_ts = Some(now_rfc3339());
                        }
                    }
                    PhaseStatus::Active => {
                        // Only one phase runs at a time.
                        for (i, p) in mission.phases.iter_mut().enumerate() {
                            if i != idx
                                && matches!(p.status, PhaseStatus::Active | PhaseStatus::Waiting)
                            {
                                p.status = PhaseStatus::Pending;
                            }
                        }
                        if mission.phases[idx].started_ts.is_none() {
                            mission.phases[idx].started_ts = Some(now_rfc3339());
                        }
                    }
                    PhaseStatus::Pending => {}
                }
                if mission.phases[idx].status != status {
                    phase_transition = Some((mission.phases[idx].title.clone(), status));
                }
                mission.phases[idx].status = status;
            }
            if let Some(est) = input.est_minutes {
                mission.phases[idx].est_minutes = Some(est);
            }
            if let Some(note) = &input.note {
                mission.phases[idx].note = Some(note.clone());
            }
            if !input.evidence.is_empty() {
                mission.phases[idx]
                    .evidence
                    .extend(input.evidence.iter().filter(|e| !e.trim().is_empty()).cloned());
            }
        }

        let extend = |target: &mut Vec<String>, items: &[String]| {
            target.extend(items.iter().filter(|s| !s.trim().is_empty()).cloned());
        };
        let resolve = |target: &mut Vec<String>, indices: &[usize]| {
            let mut sorted: Vec<usize> = indices
                .iter()
                .copied()
                .filter(|&i| i >= 1 && i <= target.len())
                .collect();
            sorted.sort_unstable_by(|a, b| b.cmp(a));
            sorted.dedup();
            for i in sorted {
                target.remove(i - 1);
            }
        };
        extend(&mut mission.open_questions, &input.add_questions);
        extend(&mut mission.assumptions, &input.add_assumptions);
        extend(&mut mission.risks, &input.add_risks);
        resolve(&mut mission.open_questions, &input.resolve_questions);
        resolve(&mut mission.assumptions, &input.resolve_assumptions);
        resolve(&mut mission.risks, &input.resolve_risks);

        mission.updated_ts = now_rfc3339();
        write_mission(path.clone(), mission.clone())
            .await
            .map_err(|e| {
                xai_tool_runtime::ToolError::custom(
                    "map_update",
                    format!("failed to write mission map {}: {e}", path.display()),
                )
            })?;

        // Phase transitions are the natural decision points where the silent
        // idea box surfaces.
        let ideas = if phase_transition.is_some() {
            idea_count(&resources).await
        } else {
            0
        };
        let mut msg = format!("Mission map updated.\n\n{}", render_mission(&mission, ideas));
        if let Some((title, status)) = phase_transition {
            msg.push_str(&format!("\nPhase transition: '{title}' -> {}.", status.tag()));
            if ideas > 0 {
                msg.push_str(&format!(
                    " Decision point: {ideas} ideas are in the idea box (board_read kind=idea) — \
                     worth a look before committing to the next approach. They are unverified \
                     suggestions, not tasks."
                ));
            }
        }
        Ok(ToolOutput::Text(msg.into()))
    }
}

// ── map_read ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MapReadInput {}

/// Read the mission map.
#[derive(Debug, Default)]
pub struct MapReadTool;

impl crate::types::tool_metadata::ToolMetadata for MapReadTool {
    fn kind(&self) -> ToolKind {
        ToolKind::MapRead
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        "Read the mission map: the long-horizon picture of this task — north star, why it \
         exists, phases with status and elapsed time, open questions, assumptions, risks, and \
         the idea-box count. Read it when (re)orienting: after a wake-up, before choosing the \
         next phase, or when unsure where you are in the task."
    }
}

impl xai_tool_runtime::Tool for MapReadTool {
    type Args = MapReadInput;
    type Output = ToolOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new("map_read").expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &::xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            "map_read",
            crate::types::tool_metadata::ToolMetadata::description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: true,
            tool_scope: Some(xai_tool_protocol::ToolScope::Read),
            ..Default::default()
        }
    }

    #[tracing::instrument(name = "tool.map_read", skip_all)]
    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        _input: MapReadInput,
    ) -> Result<ToolOutput, xai_tool_runtime::ToolError> {
        use crate::types::tool_metadata::shared_resources;
        let resources = shared_resources(&ctx)?;
        let path = {
            let res = resources.lock().await;
            mission_path(&res).ok_or_else(|| {
                xai_tool_runtime::ToolError::custom(
                    "map_read",
                    "The mission map is not configured for this session (no blackboard)"
                        .to_string(),
                )
            })?
        };
        let mission = read_mission(path).await.map_err(|e| {
            xai_tool_runtime::ToolError::custom("map_read", e.to_string())
        })?;
        let Some(mission) = mission else {
            return Ok(ToolOutput::Text(
                "No mission map yet. Create one with map_update: set northStar (the end \
                 state), why (what the result is for), and addPhases (the milestones you \
                 foresee)."
                    .into(),
            ));
        };
        let ideas = idea_count(&resources).await;
        Ok(ToolOutput::Text(render_mission(&mission, ideas).into()))
    }
}

// ── Orientation reminder ────────────────────────────────────────────────

/// Throttle state for [`OrientationReminder`] (ephemeral, per agent).
#[derive(Debug, Clone, Default)]
pub struct OrientationState {
    /// `EvidenceTimeline.seq` at the last emitted orientation block.
    pub last_emit_seq: u64,
    /// Unix seconds of the last emitted block.
    pub last_emit_unix: u64,
    /// Whether the one-time "no mission map" nudge fired.
    pub nudged_missing: bool,
}

/// Emit an orientation block at most every this many tool calls…
const ORIENT_EVERY_CALLS: u64 = 12;
/// …or this many seconds, whichever comes first.
const ORIENT_EVERY_SECS: u64 = 300;
/// Nudge the agent to create a map after this many calls without one.
const NUDGE_AFTER_CALLS: u64 = 8;

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Cross-cutting "you are here" reminder.
///
/// Fires after tool calls (goal loop or not), heavily throttled. Content is
/// orientation, not pressure: current phase, elapsed vs estimate, next
/// milestone, idea-box count, and mechanical metacognition (over-estimate
/// drift). During a `waiting` phase it explicitly reinforces that waiting
/// is valid work — the compass protects legitimate waits instead of
/// recreating goal-system nagging.
#[derive(Debug, Default)]
pub struct OrientationReminder;

#[async_trait::async_trait]
impl Reminder for OrientationReminder {
    async fn collect_reminders(
        &self,
        resources: crate::types::resources::SharedResources,
        _tool_output: &ToolOutput,
    ) -> Vec<String> {
        let (path, seq, state) = {
            let mut res = resources.lock().await;
            let Some(path) = mission_path(&res) else {
                return vec![];
            };
            let seq = res
                .get::<super::blackboard::EvidenceTimeline>()
                .map(|t| t.seq)
                .unwrap_or(0);
            let state = res.get_or_default::<OrientationState>().clone();
            (path, seq, state)
        };

        let now = unix_now();
        let due_by_calls = seq.saturating_sub(state.last_emit_seq) >= ORIENT_EVERY_CALLS;
        let due_by_time =
            state.last_emit_unix > 0 && now.saturating_sub(state.last_emit_unix) >= ORIENT_EVERY_SECS;
        // First emission waits for the call threshold; afterwards time also counts.
        if !due_by_calls && !due_by_time {
            return vec![];
        }

        let mission = match read_mission(path).await {
            Ok(m) => m,
            Err(_) => return vec![],
        };

        let Some(mission) = mission else {
            // One-time nudge: a session doing real work with no map at all.
            if state.nudged_missing || seq < NUDGE_AFTER_CALLS {
                return vec![];
            }
            let mut res = resources.lock().await;
            let st = res.get_or_default::<OrientationState>();
            st.nudged_missing = true;
            st.last_emit_seq = seq;
            st.last_emit_unix = now;
            return vec![
                "[compass] No mission map exists for this task yet. If this task has more \
                 than a couple of steps, spend one call on map_update: set northStar (the \
                 end state), why (what the result is for), and addPhases. The map is what \
                 keeps you oriented when the task gets long."
                    .to_string(),
            ];
        };

        {
            let mut res = resources.lock().await;
            let st = res.get_or_default::<OrientationState>();
            st.last_emit_seq = seq;
            st.last_emit_unix = now;
        }

        let done = mission
            .phases
            .iter()
            .filter(|p| p.status == PhaseStatus::Done)
            .count();
        let total = mission.phases.len();
        let mut msg = format!(
            "[compass] Mission: {} ({done}/{total} phases done)",
            truncate_chars(&mission.north_star, 100)
        );

        match mission.current_phase() {
            Some(idx) => {
                let p = &mission.phases[idx];
                let elapsed = p.started_ts.as_deref().and_then(minutes_since);
                match p.status {
                    PhaseStatus::Waiting => {
                        msg.push_str(&format!(
                            "\nPhase {}/{} '{}' is WAITING on: {}{}. Waiting is valid work — \
                             do not invent busywork to fill it; when the result arrives, \
                             process it first.",
                            idx + 1,
                            total,
                            p.title,
                            p.note.as_deref().unwrap_or("(unspecified)"),
                            elapsed
                                .map(|m| format!(" ({m}m elapsed)"))
                                .unwrap_or_default(),
                        ));
                    }
                    _ => {
                        msg.push_str(&format!("\nCurrent phase {}/{}: '{}'", idx + 1, total, p.title));
                        if let Some(m) = elapsed {
                            msg.push_str(&format!(" — {m}m elapsed"));
                            if let Some(est) = p.est_minutes {
                                msg.push_str(&format!(" (your estimate: {est}m)"));
                                if m > est.saturating_mul(2) {
                                    msg.push_str(
                                        ". You are far past your own estimate — reassess: is \
                                         the approach right, is the phase bigger than mapped, \
                                         or should it be split? Update the map to match \
                                         reality.",
                                    );
                                }
                            }
                        }
                        if let Some(next) = mission
                            .phases
                            .iter()
                            .find(|p| p.status == PhaseStatus::Pending)
                        {
                            msg.push_str(&format!("\nNext up: '{}'.", next.title));
                        }
                    }
                }
            }
            None if total > 0 && done < total => {
                msg.push_str(
                    "\nNo phase is marked active — mark the one you are actually working on \
                     (map_update) so the map stays true.",
                );
            }
            None => {}
        }
        if !mission.open_questions.is_empty() {
            msg.push_str(&format!(
                "\nOpen questions: {}.",
                mission.open_questions.len()
            ));
        }
        let ideas = idea_count(&resources).await;
        if ideas > 0 {
            msg.push_str(&format!(
                " Idea box: {ideas} (board_read kind=idea — reference material, not a task \
                 queue)."
            ));
        }
        vec![msg]
    }
}

// ── Stuck detection ─────────────────────────────────────────────────────

/// Consecutive same-class failures before a stuck episode fires.
pub const STUCK_THRESHOLD: u32 = 3;

/// Mechanical impasse detector (ephemeral resource).
///
/// Human analogy: impasse-driven restructuring — when direct attempts keep
/// failing the same way, the productive move is changing the frame, not
/// pushing harder. Fingerprints are (tool, normalized error class); a
/// success of the same tool clears its fingerprints ("the wall broke").
#[derive(Debug, Clone, Default)]
pub struct StuckTracker {
    /// Consecutive failures per (tool name, error-class hash).
    pub counts: std::collections::HashMap<(String, u64), u32>,
    /// How many stuck episodes each fingerprint has produced.
    pub episodes: std::collections::HashMap<(String, u64), u32>,
    /// Hard gate: mutation/execution refused until a reflective board post.
    pub gate_armed: bool,
    /// Human-readable description of the fingerprint that armed the gate.
    pub gate_reason: String,
}

/// Tool kinds excluded from stuck fingerprinting.
///
/// Wait/poll/observe/coordination tools never loop-count: repeated polling
/// during a legitimate wait is CORRECT behavior, not an impasse — this is
/// the hard separation between "waiting" and "stuck".
pub fn kind_exempt_from_stuck(kind: ToolKind) -> bool {
    matches!(
        kind,
        ToolKind::BackgroundTaskAction
            | ToolKind::WaitTasksAction
            | ToolKind::KillTaskAction
            | ToolKind::Monitor
            | ToolKind::GoalUpdate
            | ToolKind::BoardRead
            | ToolKind::BoardPost
            | ToolKind::MapRead
            | ToolKind::MapUpdate
            | ToolKind::RosterList
            | ToolKind::RosterAction
            | ToolKind::AskUser
            | ToolKind::Plan
            | ToolKind::EnterPlan
            | ToolKind::ExitPlan
    )
}

/// Tool kinds refused while the stuck hard gate is armed. Observation,
/// board, and map tools stay open — reflection is the way out.
pub fn kind_blocked_by_stuck_gate(kind: ToolKind) -> bool {
    matches!(
        kind,
        ToolKind::Edit
            | ToolKind::Write
            | ToolKind::Delete
            | ToolKind::Move
            | ToolKind::Execute
            | ToolKind::Task
    )
}

/// Normalize an error message into a stable class: first line, lowercased,
/// digits stripped (line numbers, addresses, pids), truncated. Two failures
/// with the same class are "hitting the same wall".
pub fn error_class(message: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let normalized: String = message
        .lines()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .chars()
        .filter(|c| !c.is_ascii_digit())
        .take(160)
        .collect();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    normalized.hash(&mut hasher);
    hasher.finish()
}

/// Divergence guidance appended to the error of the failure that triggers a
/// stuck episode. Episode 1 is advisory; a repeat episode arms the hard gate
/// (guardrail `stuck_gate`).
pub fn stuck_guidance(tool_name: &str, episode: u32, gate_armed: bool) -> String {
    let mut msg = format!(
        "\n\n[compass] Stuck signal: '{tool_name}' has now failed {STUCK_THRESHOLD} times in a \
         row with the same class of error. Repeating the approach harder is unlikely to work. \
         Change the frame before continuing: (1) restate what the error actually says vs what \
         you assumed; (2) list 2-3 genuinely different approaches (different tool, different \
         layer, different decomposition — what would this look like in another domain?); (3) \
         check the idea box (board_read kind=idea) for existing alternatives."
    );
    if gate_armed {
        msg.push_str(
            "\nThis is a REPEAT stuck episode at the same wall, so the stuck gate is now \
             armed: editing/executing/delegating is refused until you post your analysis to \
             the blackboard (board_post kind=question or decision — what failed, why, and \
             which different approach you will try). Reflection is mandatory, not optional.",
        );
    } else if episode > 1 {
        msg.push_str(&format!("\n(Stuck episode #{episode} at this wall.)"));
    }
    msg
}

/// Rejection message while the stuck hard gate is armed.
pub fn stuck_gate_rejection(tool_name: &str, reason: &str) -> String {
    format!(
        "'{tool_name}' blocked by the stuck gate: repeated identical failures ({reason}) with \
         no change of approach. Post your analysis to the blackboard first (board_post \
         kind=question or decision: what failed, why, and which DIFFERENT approach you will \
         try) — that unlocks the gate. Observation tools (read/search/board/map) remain \
         available."
    )
}

/// Board-post kinds whose posting counts as reflection and disarms the gate.
pub fn board_kind_disarms_stuck(kind_str: &str) -> bool {
    matches!(kind_str, "question" | "decision" | "direction" | "correction")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::resources::Resources;
    use crate::types::tool_metadata::test_ctx;

    fn resources_with_board(dir: &Path) -> Resources {
        let mut res = Resources::new();
        res.insert(BlackboardCfg {
            path: dir.join("blackboard.jsonl"),
            author: "tester".to_string(),
        });
        res
    }

    fn text_of(output: ToolOutput) -> String {
        match output {
            ToolOutput::Text(t) => t.text,
            other => panic!("expected Text output, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn first_update_requires_north_star_and_why() {
        let tmp = tempfile::tempdir().unwrap();
        let shared = resources_with_board(tmp.path()).into_shared();
        let err = xai_tool_runtime::Tool::run(
            &MapUpdateTool,
            test_ctx(shared),
            MapUpdateInput {
                add_phases: vec!["build".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("northStar"), "err: {err}");
    }

    #[tokio::test]
    async fn create_activate_and_wait_flow() {
        let tmp = tempfile::tempdir().unwrap();
        let shared = resources_with_board(tmp.path()).into_shared();

        let out = xai_tool_runtime::Tool::run(
            &MapUpdateTool,
            test_ctx(shared.clone()),
            MapUpdateInput {
                north_star: Some("weather forecaster live".into()),
                why: Some("user needs planning data".into()),
                add_phases: vec!["collect".into(), "analyze".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(text_of(out).contains("1. [pending] collect"));

        // Activate phase 1.
        xai_tool_runtime::Tool::run(
            &MapUpdateTool,
            test_ctx(shared.clone()),
            MapUpdateInput {
                phase: Some(1),
                status: Some(PhaseStatus::Active),
                est_minutes: Some(30),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // Waiting without a note is refused.
        let err = xai_tool_runtime::Tool::run(
            &MapUpdateTool,
            test_ctx(shared.clone()),
            MapUpdateInput {
                phase: Some(1),
                status: Some(PhaseStatus::Waiting),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("waiting"), "err: {err}");

        // Waiting with a note works; untargeted update hits the current phase.
        let out = xai_tool_runtime::Tool::run(
            &MapUpdateTool,
            test_ctx(shared.clone()),
            MapUpdateInput {
                status: Some(PhaseStatus::Waiting),
                note: Some("cron samples every 30m".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let text = text_of(out);
        assert!(text.contains("[waiting] collect"), "text: {text}");
        assert!(text.contains("cron samples"), "text: {text}");

        let mission = read_mission(tmp.path().join("mission.json"))
            .await
            .unwrap()
            .unwrap();
        assert!(mission.is_waiting());
        assert!(mission.phases[0].started_ts.is_some(), "framework stamps start");
    }

    #[tokio::test]
    async fn done_requires_evidence_and_only_one_active() {
        let tmp = tempfile::tempdir().unwrap();
        let shared = resources_with_board(tmp.path()).into_shared();
        xai_tool_runtime::Tool::run(
            &MapUpdateTool,
            test_ctx(shared.clone()),
            MapUpdateInput {
                north_star: Some("ship".into()),
                why: Some("because".into()),
                add_phases: vec!["a".into(), "b".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let err = xai_tool_runtime::Tool::run(
            &MapUpdateTool,
            test_ctx(shared.clone()),
            MapUpdateInput {
                phase: Some(1),
                status: Some(PhaseStatus::Done),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("evidence"), "err: {err}");

        for i in [1usize, 2] {
            xai_tool_runtime::Tool::run(
                &MapUpdateTool,
                test_ctx(shared.clone()),
                MapUpdateInput {
                    phase: Some(i),
                    status: Some(PhaseStatus::Active),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }
        let mission = read_mission(tmp.path().join("mission.json"))
            .await
            .unwrap()
            .unwrap();
        let active = mission
            .phases
            .iter()
            .filter(|p| p.status == PhaseStatus::Active)
            .count();
        assert_eq!(active, 1, "activating phase 2 demotes phase 1");
        assert_eq!(mission.current_phase(), Some(1));
    }

    #[tokio::test]
    async fn fabricated_done_evidence_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let mut res = resources_with_board(tmp.path());
        res.insert(crate::types::resources::Cwd(tmp.path().to_path_buf()));
        let shared = res.into_shared();
        xai_tool_runtime::Tool::run(
            &MapUpdateTool,
            test_ctx(shared.clone()),
            MapUpdateInput {
                north_star: Some("ship".into()),
                why: Some("because".into()),
                add_phases: vec!["a".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let err = xai_tool_runtime::Tool::run(
            &MapUpdateTool,
            test_ctx(shared),
            MapUpdateInput {
                phase: Some(1),
                status: Some(PhaseStatus::Done),
                evidence: vec!["src/does_not_exist.rs:42".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("does not exist"), "err: {err}");
    }

    #[test]
    fn error_class_ignores_digits_and_later_lines() {
        let a = error_class("error[E0308]: mismatched types at line 42\nnote: ...");
        let b = error_class("error[E0308]: mismatched types at line 97\nother trailing");
        assert_eq!(a, b);
        assert_ne!(a, error_class("permission denied"));
    }

    #[test]
    fn stuck_exemptions_split_waiting_from_stuck() {
        assert!(kind_exempt_from_stuck(ToolKind::BackgroundTaskAction));
        assert!(kind_exempt_from_stuck(ToolKind::Monitor));
        assert!(!kind_exempt_from_stuck(ToolKind::Execute));
        assert!(kind_blocked_by_stuck_gate(ToolKind::Execute));
        assert!(!kind_blocked_by_stuck_gate(ToolKind::Read));
        assert!(board_kind_disarms_stuck("question"));
        assert!(!board_kind_disarms_stuck("finding"));
    }
}
