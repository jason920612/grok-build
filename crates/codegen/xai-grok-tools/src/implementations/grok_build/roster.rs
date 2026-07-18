//! Personnel roster — evolutionary selection over agent personas.
//!
//! A workspace keeps a persistent roster of named personas, each carrying a
//! methodology style, a rank, and a track record. Directions are executed by
//! whichever persona's proposal wins; reality (a `verdict` board entry with
//! an explicit outcome) then drives *mechanical* personnel actions:
//!
//! - success → rank +1 (rank gates real privileges: rank 0 cannot command
//!   subagents — enforced at spawn time in the shell)
//! - failure → eliminated (archived with its record; one strike)
//!
//! What is being selected is not "talent" — every persona runs on the same
//! model. Selection pressure acts on *methodology styles and accumulated
//! priors*: styles that empirically work in this workspace survive and gain
//! scope; styles that fail are retired, and the orchestrator refills the
//! roster by mutating winners (`roster_add`).
//!
//! Storage: `<cwd>/.grok/roster.json`, fs2-locked, human-readable.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::types::output::ToolOutput;
use crate::types::tool::{ToolKind, ToolNamespace};

/// Where the roster lives for this session tree. Ephemeral resource,
/// inserted at agent build time next to `BlackboardCfg`.
#[derive(Debug, Clone)]
pub struct RosterCfg {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PersonaStatus {
    Active,
    Eliminated,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackRecordEntry {
    pub ts: String,
    /// Topic of the direction this outcome belongs to.
    pub direction: String,
    pub won: bool,
    /// Board entry id of the verdict.
    pub verdict_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Persona {
    pub name: String,
    /// Methodology style injected as persona instructions when spawned.
    pub style: String,
    pub rank: u32,
    pub status: PersonaStatus,
    #[serde(default)]
    pub record: Vec<TrackRecordEntry>,
    /// Provenance: seed persona or mutation of a winner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub born_from: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Roster {
    pub personas: Vec<Persona>,
}

/// Outcome of a mechanical personnel action, for surfacing in tool output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersonnelAction {
    Promoted { name: String, new_rank: u32 },
    Eliminated { name: String },
    /// Verdict didn't map to a roster persona (e.g. author was "main").
    NoMatch,
}

/// The six seed personas: methodology styles deliberately spread apart so
/// selection has genuine diversity to act on.
fn seed_personas() -> Vec<Persona> {
    let styles: [(&str, &str); 6] = [
        (
            "risk-first-rhea",
            "You attack the riskiest unknown first. Before proposing, identify the \
             assumption most likely to sink the direction and design the first milestone \
             to test exactly that. You would rather kill a bad direction in one day than \
             polish a doomed one for a week.",
        ),
        (
            "mvp-milo",
            "You ship the smallest end-to-end working slice first, always. Your proposals \
             cut scope aggressively: one vertical path through the whole system, working \
             and verified, before any breadth. Feature-completeness comes from iterating \
             on something real.",
        ),
        (
            "test-first-tessa",
            "You define the verification before the implementation. Your proposals lead \
             with the exact commands and assertions that will prove success, then work \
             backwards. You never claim done without the test output in hand.",
        ),
        (
            "architecture-arlo",
            "You map the existing structure before touching it. Your proposals show where \
             the change belongs in the current design, which boundaries it respects, and \
             what it deliberately does not touch. You favor the smallest structural \
             change that fits the system's own grain.",
        ),
        (
            "iterate-io",
            "You optimize for feedback-loop speed. Your proposals maximize the number of \
             build-run-observe cycles per hour: instrument early, run constantly, let \
             observed behavior redirect the plan. Plans are hypotheses, runs are data.",
        ),
        (
            "deletion-dara",
            "You look for what can be deleted or avoided before what must be built. Your \
             proposals question the requirement itself first, reuse existing machinery \
             second, and write new code only third. The best diff is the smallest one \
             that verifiably meets the goal.",
        ),
    ];
    styles
        .into_iter()
        .map(|(name, style)| Persona {
            name: name.to_string(),
            style: style.to_string(),
            rank: 0,
            status: PersonaStatus::Active,
            record: Vec::new(),
            born_from: None,
        })
        .collect()
}

/// How many subagents a persona of `rank` may command. Rank 0 personas
/// cannot spawn at all — enforced by stripping the task tool at spawn.
pub fn subagent_quota_for_rank(rank: u32) -> u32 {
    match rank {
        0 => 0,
        1 => 2,
        2 => 4,
        _ => 8,
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Load the roster only if the file exists — used by spawn-time gates that
/// must not create `.grok/roster.json` as a side effect in workspaces that
/// never opted into the meritocracy flow.
pub fn load_if_exists(path: &Path) -> Option<Roster> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Load the roster, seeding the default personas when the file is missing.
pub fn load_or_seed(path: &Path) -> std::io::Result<Roster> {
    match std::fs::File::open(path) {
        Ok(mut f) => {
            fs2::FileExt::lock_shared(&f)?;
            let mut raw = String::new();
            let read = f.read_to_string(&mut raw);
            let _ = fs2::FileExt::unlock(&f);
            read?;
            serde_json::from_str(&raw).map_err(std::io::Error::other)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let roster = Roster {
                personas: seed_personas(),
            };
            save(path, &roster)?;
            Ok(roster)
        }
        Err(e) => Err(e),
    }
}

pub fn save(path: &Path, roster: &Roster) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    fs2::FileExt::lock_exclusive(&f)?;
    let result = (|| {
        let raw = serde_json::to_string_pretty(roster).map_err(std::io::Error::other)?;
        f.write_all(raw.as_bytes())?;
        f.flush()
    })();
    let _ = fs2::FileExt::unlock(&f);
    result
}

/// Look up an ACTIVE persona by name, or by a blackboard author string of
/// the form `"{persona}#{session_prefix}"`.
pub fn find_active<'a>(roster: &'a Roster, author: &str) -> Option<&'a Persona> {
    let name = author.split('#').next().unwrap_or(author);
    roster
        .personas
        .iter()
        .find(|p| p.status == PersonaStatus::Active && p.name == name)
}

/// Apply the mechanical personnel action for a verdict: promote on success,
/// eliminate on failure (one strike). `author` is the proposal author
/// (persona name or `persona#prefix` board author). Persists on change.
pub fn apply_verdict(
    path: &Path,
    author: &str,
    won: bool,
    direction: &str,
    verdict_id: &str,
) -> std::io::Result<PersonnelAction> {
    let mut roster = load_or_seed(path)?;
    let name = author.split('#').next().unwrap_or(author).to_string();
    let Some(p) = roster
        .personas
        .iter_mut()
        .find(|p| p.status == PersonaStatus::Active && p.name == name)
    else {
        return Ok(PersonnelAction::NoMatch);
    };
    p.record.push(TrackRecordEntry {
        ts: now_rfc3339(),
        direction: direction.to_string(),
        won,
        verdict_id: verdict_id.to_string(),
    });
    let action = if won {
        p.rank += 1;
        PersonnelAction::Promoted {
            name: p.name.clone(),
            new_rank: p.rank,
        }
    } else {
        p.status = PersonaStatus::Eliminated;
        PersonnelAction::Eliminated { name: p.name.clone() }
    };
    save(path, &roster)?;
    Ok(action)
}

fn cfg_or_missing(
    resources: &crate::types::resources::Resources,
    tool: &str,
) -> Result<RosterCfg, xai_tool_runtime::ToolError> {
    resources.get::<RosterCfg>().cloned().ok_or_else(|| {
        xai_tool_runtime::ToolError::custom(
            tool,
            "Roster is not configured for this session".to_string(),
        )
    })
}

// ── roster_list ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RosterListInput {
    #[schemars(description = "Include eliminated personas and their records.")]
    #[serde(default)]
    pub include_eliminated: bool,
}

#[derive(Debug, Default)]
pub struct RosterListTool;

impl crate::types::tool_metadata::ToolMetadata for RosterListTool {
    fn kind(&self) -> ToolKind {
        ToolKind::RosterList
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        "List the personnel roster: active personas with their methodology style, rank \
         (rank 0 cannot command subagents), and win/loss record. Consult it before \
         selecting a proposal (track record is a tiebreaker) and before spawning a \
         leader (the leader must be the accepted proposal's author)."
    }
}

impl xai_tool_runtime::Tool for RosterListTool {
    type Args = RosterListInput;
    type Output = ToolOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new("roster_list").expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &::xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            "roster_list",
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

    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: RosterListInput,
    ) -> Result<ToolOutput, xai_tool_runtime::ToolError> {
        use crate::types::tool_metadata::shared_resources;
        let resources = shared_resources(&ctx)?;
        let cfg = {
            let res = resources.lock().await;
            cfg_or_missing(&res, "roster_list")?
        };
        let path = cfg.path.clone();
        let roster = tokio::task::spawn_blocking(move || load_or_seed(&path))
            .await
            .map_err(|e| xai_tool_runtime::ToolError::custom("roster_list", e.to_string()))?
            .map_err(|e| {
                xai_tool_runtime::ToolError::custom(
                    "roster_list",
                    format!("failed to load roster {}: {e}", cfg.path.display()),
                )
            })?;
        let mut out = String::from("Personnel roster:\n");
        for p in &roster.personas {
            if p.status == PersonaStatus::Eliminated && !input.include_eliminated {
                continue;
            }
            let (wins, losses) = p
                .record
                .iter()
                .fold((0, 0), |(w, l), r| if r.won { (w + 1, l) } else { (w, l + 1) });
            out.push_str(&format!(
                "- {} [{} rank {} | {}W-{}L | quota {} subagents]\n    style: {}\n",
                p.name,
                match p.status {
                    PersonaStatus::Active => "active,",
                    PersonaStatus::Eliminated => "ELIMINATED,",
                },
                p.rank,
                wins,
                losses,
                subagent_quota_for_rank(p.rank),
                p.style,
            ));
            if let Some(ref origin) = p.born_from {
                out.push_str(&format!("    born from: {origin}\n"));
            }
        }
        Ok(ToolOutput::Text(out.into()))
    }
}

// ── roster_add ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RosterAddInput {
    #[schemars(description = "Unique kebab-case persona name, e.g. 'profile-first-pia'.")]
    pub name: String,

    #[schemars(
        description = "Methodology style paragraph — HOW this persona approaches work. Make it genuinely different from existing active personas; when replacing an eliminated one, mutate a winner's style rather than cloning it."
    )]
    pub style: String,

    #[schemars(
        description = "Provenance note, e.g. 'mutation of mvp-milo after risk-first-rhea eliminated'."
    )]
    #[serde(default)]
    pub born_from: Option<String>,
}

#[derive(Debug, Default)]
pub struct RosterAddTool;

impl crate::types::tool_metadata::ToolMetadata for RosterAddTool {
    fn kind(&self) -> ToolKind {
        ToolKind::RosterAction
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        "Register a new persona on the personnel roster (rank 0, empty record). Use when \
         the active roster runs thin after eliminations. Derive new styles by mutating \
         the styles of high-rank winners and mixing in a genuinely new angle — do not \
         clone an existing persona or resurrect an eliminated one unchanged. Promotion \
         and elimination are automatic (driven by verdict board entries) and are NOT \
         performed with this tool."
    }
}

impl xai_tool_runtime::Tool for RosterAddTool {
    type Args = RosterAddInput;
    type Output = ToolOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new("roster_add").expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &::xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            "roster_add",
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

    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: RosterAddInput,
    ) -> Result<ToolOutput, xai_tool_runtime::ToolError> {
        use crate::types::tool_metadata::shared_resources;
        let name = input.name.trim().to_string();
        if name.is_empty() || input.style.trim().is_empty() {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(
                "Persona name and style must not be empty".to_string(),
            ));
        }
        let resources = shared_resources(&ctx)?;
        let cfg = {
            let res = resources.lock().await;
            cfg_or_missing(&res, "roster_add")?
        };
        let path = cfg.path.clone();
        let style = input.style.clone();
        let born_from = input.born_from.clone();
        let result = tokio::task::spawn_blocking(move || -> std::io::Result<Result<u32, String>> {
            let mut roster = load_or_seed(&path)?;
            if roster.personas.iter().any(|p| p.name == name) {
                return Ok(Err(format!(
                    "persona '{name}' already exists on the roster (eliminated names stay \
                     reserved — pick a new name)"
                )));
            }
            roster.personas.push(Persona {
                name,
                style,
                rank: 0,
                status: PersonaStatus::Active,
                record: Vec::new(),
                born_from,
            });
            let active = roster
                .personas
                .iter()
                .filter(|p| p.status == PersonaStatus::Active)
                .count() as u32;
            save(&path, &roster)?;
            Ok(Ok(active))
        })
        .await
        .map_err(|e| xai_tool_runtime::ToolError::custom("roster_add", e.to_string()))?
        .map_err(|e| xai_tool_runtime::ToolError::custom("roster_add", e.to_string()))?;
        match result {
            Ok(active) => Ok(ToolOutput::Text(
                format!(
                    "Persona '{}' registered at rank 0 ({active} active on the roster).",
                    input.name.trim()
                )
                .into(),
            )),
            Err(msg) => Err(xai_tool_runtime::ToolError::invalid_arguments(msg)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_load_apply_verdict_cycle() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".grok/roster.json");

        let roster = load_or_seed(&path).unwrap();
        assert_eq!(roster.personas.len(), 6);
        assert!(roster.personas.iter().all(|p| p.rank == 0));

        // Win: promoted, record appended.
        let action = apply_verdict(&path, "mvp-milo#019f6d", true, "auth-flow", "v1").unwrap();
        assert_eq!(
            action,
            PersonnelAction::Promoted {
                name: "mvp-milo".into(),
                new_rank: 1
            }
        );

        // Loss: eliminated on one strike.
        let action = apply_verdict(&path, "risk-first-rhea", false, "db-migration", "v2").unwrap();
        assert_eq!(
            action,
            PersonnelAction::Eliminated {
                name: "risk-first-rhea".into()
            }
        );

        let roster = load_or_seed(&path).unwrap();
        let milo = roster.personas.iter().find(|p| p.name == "mvp-milo").unwrap();
        assert_eq!(milo.rank, 1);
        assert_eq!(milo.record.len(), 1);
        let rhea = roster
            .personas
            .iter()
            .find(|p| p.name == "risk-first-rhea")
            .unwrap();
        assert_eq!(rhea.status, PersonaStatus::Eliminated);

        // Eliminated persona no longer matches; unknown author no-ops.
        assert_eq!(
            apply_verdict(&path, "risk-first-rhea", true, "x", "v3").unwrap(),
            PersonnelAction::NoMatch
        );
        assert_eq!(
            apply_verdict(&path, "main", true, "x", "v4").unwrap(),
            PersonnelAction::NoMatch
        );
    }

    #[test]
    fn quota_ladder() {
        assert_eq!(subagent_quota_for_rank(0), 0);
        assert_eq!(subagent_quota_for_rank(1), 2);
        assert_eq!(subagent_quota_for_rank(2), 4);
        assert_eq!(subagent_quota_for_rank(9), 8);
    }

    #[test]
    fn find_active_matches_board_author_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("roster.json");
        let roster = load_or_seed(&path).unwrap();
        assert!(find_active(&roster, "test-first-tessa#abc123").is_some());
        assert!(find_active(&roster, "nobody#abc123").is_none());
    }
}
