//! Blackboard — a shared, append-only ledger of verified facts for
//! multi-agent collaboration.
//!
//! All agents in a session tree (root session + every subagent) share one
//! `blackboard.jsonl` file. Agents post findings, test results, claims,
//! questions, decisions, and corrections; other agents see new entries via
//! the cross-cutting [`BlackboardDigestReminder`] after their next tool call.
//!
//! Core semantics ("verify-first" discipline):
//! - Before starting work, an agent reads the board (`board_read`) to learn
//!   what teammates already verified.
//! - Facts not on the board must be verified with tools, then posted with
//!   evidence (`board_post`).
//! - When reality contradicts a board entry, the agent posts a `correction`
//!   referencing the stale entry, which is pushed to all other agents.
//!
//! Storage: JSONL, one entry per line, append-only. Concurrent writers are
//! serialized with an exclusive advisory file lock (`fs2`); readers take a
//! shared lock. The file is human-readable — the whole meeting transcript
//! can be inspected with any text editor.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::types::output::ToolOutput;
use crate::types::resources::State;
use crate::types::tool::{Reminder, ToolKind, ToolNamespace};

/// Where the shared board lives and who this agent is on it.
///
/// Ephemeral resource (not serialized): inserted into `Resources` at agent
/// build time. The root session derives the path from its session folder;
/// subagents inherit the parent's path so the whole tree shares one board.
#[derive(Debug, Clone)]
pub struct BlackboardCfg {
    pub path: PathBuf,
    /// Author name stamped on entries posted by this agent,
    /// e.g. `"main"` or `"explore#a1b2c3"`.
    pub author: String,
}

/// Per-session digest cursor: how many board entries this agent has seen.
/// New entries beyond the cursor are surfaced by [`BlackboardDigestReminder`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BoardCursor {
    pub seen: u64,
}

crate::register_resource!("grok_build", "Blackboard", BoardCursor);

/// The category of a board entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    /// A verified fact about the current state of the world/codebase.
    Finding,
    /// The outcome of running tests/commands (include the actual output).
    TestResult,
    /// An assertion that is not yet independently verified. Requires evidence.
    Claim,
    /// A question addressed to other agents.
    Question,
    /// A decision taken (design choice, approach). Requires evidence/rationale.
    Decision,
    /// The board (or a teammate's entry) contradicts observed reality.
    /// Requires evidence and should set `reply_to` to the stale entry.
    Correction,
    /// A broad goal open for proposals (posted by the orchestrator/user).
    Direction,
    /// A candidate's plan for a direction: approach, first milestone, and
    /// VERIFIABLE success criteria (exact commands). Requires evidence and
    /// `reply_to` = the direction. The accepted proposal's author becomes
    /// the leader accountable for the outcome.
    Proposal,
    /// The outcome of executing an accepted proposal, judged by running its
    /// own success criteria verbatim. Requires evidence, `reply_to` = the
    /// proposal, and an explicit `outcome` — which mechanically drives
    /// roster personnel actions (promotion / elimination).
    Verdict,
}

impl EntryKind {
    pub const fn tag(&self) -> &'static str {
        match self {
            Self::Finding => "finding",
            Self::TestResult => "test_result",
            Self::Claim => "claim",
            Self::Question => "question",
            Self::Decision => "decision",
            Self::Correction => "correction",
            Self::Direction => "direction",
            Self::Proposal => "proposal",
            Self::Verdict => "verdict",
        }
    }

    /// Kinds that assert something about reality must carry evidence.
    const fn requires_evidence(&self) -> bool {
        matches!(
            self,
            Self::Finding
                | Self::TestResult
                | Self::Claim
                | Self::Correction
                | Self::Proposal
                | Self::Verdict
        )
    }
}

/// Explicit outcome carried by `verdict` entries — the input to the
/// mechanical personnel machinery, deliberately not inferred from prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VerdictOutcome {
    Success,
    Failure,
}

/// One line in `blackboard.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardEntry {
    pub id: String,
    /// RFC 3339 UTC timestamp.
    pub ts: String,
    pub author: String,
    pub kind: EntryKind,
    /// Short topic key so agents can filter, e.g. `"auth"`, `"build"`.
    pub topic: String,
    pub body: String,
    /// Evidence references: `file:line`, command + output snippets, URLs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
    /// Id of the entry this one replies to (discussion threads, corrections).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    /// Explicit outcome — present on `verdict` entries only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<VerdictOutcome>,
}

impl BoardEntry {
    /// Single-line rendering used in tool output and digests.
    fn render(&self, index: u64) -> String {
        let mut line = format!(
            "[{}] {} by {} on '{}': {}",
            index,
            self.kind.tag(),
            self.author,
            self.topic,
            self.body
        );
        if let Some(ref parent) = self.reply_to {
            line.push_str(&format!(" (reply to {parent})"));
        }
        if !self.evidence.is_empty() {
            line.push_str(&format!("\n    evidence: {}", self.evidence.join(" | ")));
        }
        line
    }
}

// ── Storage ─────────────────────────────────────────────────────────────

/// Append one entry under an exclusive advisory lock.
fn append_entry_sync(path: &Path, entry: &BoardEntry) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    fs2::FileExt::lock_exclusive(&file)?;
    let result = (|| {
        let mut line = serde_json::to_string(entry).map_err(std::io::Error::other)?;
        line.push('\n');
        file.write_all(line.as_bytes())?;
        file.flush()
    })();
    let _ = fs2::FileExt::unlock(&file);
    result
}

/// Read every entry under a shared advisory lock. Unparseable lines are
/// skipped (a torn write must not brick the whole board).
fn read_entries_sync(path: &Path) -> std::io::Result<Vec<BoardEntry>> {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    fs2::FileExt::lock_shared(&file)?;
    let mut raw = String::new();
    let result = file.read_to_string(&mut raw);
    let _ = fs2::FileExt::unlock(&file);
    result?;
    Ok(raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<BoardEntry>(l).ok())
        .collect())
}

async fn append_entry(path: PathBuf, entry: BoardEntry) -> std::io::Result<()> {
    tokio::task::spawn_blocking(move || append_entry_sync(&path, &entry))
        .await
        .map_err(|e| std::io::Error::other(format!("blackboard write task panicked: {e}")))?
}

async fn read_entries(path: PathBuf) -> std::io::Result<Vec<BoardEntry>> {
    tokio::task::spawn_blocking(move || read_entries_sync(&path))
        .await
        .map_err(|e| std::io::Error::other(format!("blackboard read task panicked: {e}")))?
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn board_cfg_or_missing(
    resources: &crate::types::resources::Resources,
    tool: &str,
) -> Result<BlackboardCfg, xai_tool_runtime::ToolError> {
    resources.get::<BlackboardCfg>().cloned().ok_or_else(|| {
        xai_tool_runtime::ToolError::custom(
            tool,
            "Blackboard is not configured for this session".to_string(),
        )
    })
}

// ── Evidence spot-check ─────────────────────────────────────────────────

/// How many parsable `file:line` references get spot-checked per post.
const EVIDENCE_SPOT_CHECKS: usize = 2;
/// Files larger than this are not line-counted (existence check only).
const EVIDENCE_MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// A `path:N` / `path:N-M` reference extracted from an evidence string.
struct EvidenceRef {
    raw: String,
    path: String,
    line: u64,
}

/// Extract a leading `file:line` reference from one evidence string, if the
/// first token looks like one. Only strings that *claim* a file location are
/// candidates — command output, URLs, and prose pass through unchecked.
fn parse_evidence_ref(evidence: &str) -> Option<EvidenceRef> {
    let token = evidence.split_whitespace().next()?;
    let token = token.trim_end_matches([',', ';', '.', ')']);
    if token.contains("://") {
        return None; // URL
    }
    let (path, suffix) = token.rsplit_once(':')?;
    if path.is_empty() || !(path.contains('/') || path.contains('\\') || path.contains('.')) {
        return None; // not path-shaped (e.g. "note:" prefixes)
    }
    let start = suffix.split('-').next()?;
    let line = start.parse::<u64>().ok()?;
    if line == 0 {
        return None;
    }
    Some(EvidenceRef {
        raw: token.to_string(),
        path: path.to_string(),
        line,
    })
}

/// Spot-check up to [`EVIDENCE_SPOT_CHECKS`] file references against the real
/// filesystem: the file must exist and contain the cited line. This catches
/// fabricated citations, not wrong conclusions — an agent can still cite a
/// real line and misread it, but it cannot invent locations.
fn spot_check_evidence(
    evidence: &[String],
    cwd: Option<&Path>,
) -> Result<(), String> {
    let mut checked = 0usize;
    for ev in evidence {
        if checked >= EVIDENCE_SPOT_CHECKS {
            break;
        }
        let Some(eref) = parse_evidence_ref(ev) else {
            continue;
        };
        checked += 1;
        let candidate = Path::new(&eref.path);
        let resolved = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            match cwd {
                Some(base) => base.join(candidate),
                None => continue, // no cwd to resolve against — skip
            }
        };
        let meta = match std::fs::metadata(&resolved) {
            Ok(m) => m,
            Err(_) => {
                return Err(format!(
                    "evidence cites '{}' but {} does not exist. Cite locations you actually \
                     read — fabricated references are rejected.",
                    eref.raw,
                    resolved.display()
                ));
            }
        };
        if !meta.is_file() || meta.len() > EVIDENCE_MAX_FILE_BYTES {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(&resolved) {
            let line_count = content.lines().count() as u64;
            if eref.line > line_count {
                return Err(format!(
                    "evidence cites '{}' but {} has only {} lines. Cite locations you \
                     actually read — fabricated references are rejected.",
                    eref.raw,
                    resolved.display(),
                    line_count
                ));
            }
        }
    }
    Ok(())
}

// ── board_post ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BoardPostInput {
    #[schemars(
        description = "Entry kind: finding | test_result | claim | question | decision | correction. finding/test_result/claim/correction REQUIRE evidence."
    )]
    pub kind: EntryKind,

    #[schemars(
        description = "Short topic key other agents can filter by, e.g. 'build', 'auth-flow', 'flaky-test'."
    )]
    pub topic: String,

    #[schemars(description = "The content of the entry. Be specific and factual.")]
    pub body: String,

    #[schemars(
        description = "Evidence references backing this entry: file:line locations, commands with their observed output, URLs. Required for finding/test_result/claim/correction."
    )]
    #[serde(default)]
    pub evidence: Vec<String>,

    #[schemars(
        description = "Optional id of the board entry this replies to (discussion thread / the stale entry a correction fixes / the direction a proposal answers / the proposal a verdict judges)."
    )]
    #[serde(default)]
    pub reply_to: Option<String>,

    #[schemars(
        description = "REQUIRED for kind=verdict, forbidden otherwise: success | failure. Drives automatic roster personnel actions (the judged proposal's author is promoted or eliminated)."
    )]
    #[serde(default)]
    pub outcome: Option<VerdictOutcome>,
}

/// Post an entry to the shared blackboard.
#[derive(Debug, Default)]
pub struct BoardPostTool;

impl crate::types::tool_metadata::ToolMetadata for BoardPostTool {
    fn kind(&self) -> ToolKind {
        ToolKind::BoardPost
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        r#"Post an entry to the shared team blackboard — the append-only ledger of verified facts all agents in this session share.

Post when you: verified something teammates may rely on (finding), ran tests/commands (test_result), made an assertion you could not fully verify (claim), need input (question), settled an approach (decision), or observed reality contradicting an existing board entry (correction — set replyTo to the stale entry's id).

finding/test_result/claim/correction REQUIRE evidence (file:line, command output, URLs). Unverified opinions without evidence are rejected."#
    }
}

impl xai_tool_runtime::Tool for BoardPostTool {
    type Args = BoardPostInput;
    type Output = ToolOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new("board_post").expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &::xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            "board_post",
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

    #[tracing::instrument(name = "tool.board_post", skip_all, fields(kind = ?input.kind, topic = %input.topic))]
    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: BoardPostInput,
    ) -> Result<ToolOutput, xai_tool_runtime::ToolError> {
        use crate::types::tool_metadata::shared_resources;
        let resources = shared_resources(&ctx)?;

        if input.kind.requires_evidence()
            && input.evidence.iter().all(|e| e.trim().is_empty())
        {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(format!(
                "A '{}' entry requires evidence. Verify first (read the file, run the command), \
                 then post with evidence like 'src/auth.rs:42' or 'cargo test auth:: -> 5 passed'.",
                input.kind.tag()
            )));
        }
        if input.body.trim().is_empty() {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(
                "Entry body must not be empty".to_string(),
            ));
        }
        match input.kind {
            EntryKind::Verdict => {
                if input.outcome.is_none() {
                    return Err(xai_tool_runtime::ToolError::invalid_arguments(
                        "A verdict requires an explicit outcome (success | failure) — it drives \
                         automatic personnel actions and is never inferred from prose."
                            .to_string(),
                    ));
                }
                if input.reply_to.is_none() {
                    return Err(xai_tool_runtime::ToolError::invalid_arguments(
                        "A verdict must set replyTo to the proposal it judges.".to_string(),
                    ));
                }
            }
            EntryKind::Proposal => {
                if input.reply_to.is_none() {
                    return Err(xai_tool_runtime::ToolError::invalid_arguments(
                        "A proposal must set replyTo to the direction it answers.".to_string(),
                    ));
                }
            }
            _ if input.outcome.is_some() => {
                return Err(xai_tool_runtime::ToolError::invalid_arguments(
                    "outcome is only valid on kind=verdict.".to_string(),
                ));
            }
            _ => {}
        }

        let (cfg, cwd) = {
            let res = resources.lock().await;
            (
                board_cfg_or_missing(&res, "board_post")?,
                res.get::<crate::types::resources::Cwd>().map(|c| c.0.clone()),
            )
        };

        if input.kind.requires_evidence() {
            let evidence = input.evidence.clone();
            let check = tokio::task::spawn_blocking(move || {
                spot_check_evidence(&evidence, cwd.as_deref())
            })
            .await
            .map_err(|e| {
                xai_tool_runtime::ToolError::custom(
                    "board_post",
                    format!("evidence check task panicked: {e}"),
                )
            })?;
            if let Err(msg) = check {
                return Err(xai_tool_runtime::ToolError::invalid_arguments(msg));
            }
        }

        let entry = BoardEntry {
            id: uuid::Uuid::now_v7().simple().to_string(),
            ts: now_rfc3339(),
            author: cfg.author.clone(),
            kind: input.kind,
            topic: input.topic,
            body: input.body,
            evidence: input
                .evidence
                .into_iter()
                .filter(|e| !e.trim().is_empty())
                .collect(),
            reply_to: input.reply_to,
            outcome: input.outcome,
        };
        let entry_id = entry.id.clone();
        let verdict_reply_to = matches!(entry.kind, EntryKind::Verdict)
            .then(|| entry.reply_to.clone())
            .flatten();

        append_entry(cfg.path.clone(), entry).await.map_err(|e| {
            xai_tool_runtime::ToolError::custom(
                "board_post",
                format!("failed to write blackboard {}: {e}", cfg.path.display()),
            )
        })?;

        // Posting counts as having seen your own entry: advance the cursor
        // past the end so the digest never echoes an agent's own post back.
        let entries = read_entries(cfg.path.clone()).await.unwrap_or_default();
        {
            let mut res = resources.lock().await;
            let cursor = res.get_or_default::<State<BoardCursor>>();
            cursor.0.seen = entries.len() as u64;
        }

        // Mechanical personnel action: a verdict on a proposal promotes or
        // eliminates the proposal's author on the roster. Reality decides —
        // no agent gets to vote on the consequence.
        let mut personnel_note = String::new();
        if let Some(proposal_id) = verdict_reply_to {
            let roster_path = {
                let res = resources.lock().await;
                res.get::<super::roster::RosterCfg>().map(|c| c.path.clone())
            };
            if let Some(roster_path) = roster_path
                && let Some(proposal) = entries
                    .iter()
                    .find(|e| e.id == proposal_id && e.kind == EntryKind::Proposal)
            {
                let won = input.outcome == Some(VerdictOutcome::Success);
                let author = proposal.author.clone();
                let direction = proposal.topic.clone();
                let vid = entry_id.clone();
                let action = tokio::task::spawn_blocking(move || {
                    super::roster::apply_verdict(&roster_path, &author, won, &direction, &vid)
                })
                .await
                .map_err(|e| xai_tool_runtime::ToolError::custom("board_post", e.to_string()))?
                .map_err(|e| {
                    xai_tool_runtime::ToolError::custom(
                        "board_post",
                        format!("verdict recorded but roster update failed: {e}"),
                    )
                })?;
                use super::roster::PersonnelAction;
                personnel_note = match action {
                    PersonnelAction::Promoted { name, new_rank } => format!(
                        "\nPersonnel: {name} promoted to rank {new_rank} (subagent quota now {}).",
                        super::roster::subagent_quota_for_rank(new_rank)
                    ),
                    PersonnelAction::Eliminated { name } => format!(
                        "\nPersonnel: {name} ELIMINATED from the roster (one strike). Post their \
                         failure analysis as a finding, and refill the roster via roster_add by \
                         mutating a winner's style."
                    ),
                    PersonnelAction::NoMatch => String::new(),
                };
            }
        }

        Ok(ToolOutput::Text(
            format!("Posted to blackboard (id: {entry_id}). Teammates will see it after their next tool call.{personnel_note}").into(),
        ))
    }
}

// ── board_read ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BoardReadInput {
    #[schemars(description = "Only show entries whose topic contains this string.")]
    #[serde(default)]
    pub topic: Option<String>,

    #[schemars(
        description = "Only show entries of this kind: finding | test_result | claim | question | decision | correction."
    )]
    #[serde(default)]
    pub kind: Option<EntryKind>,

    #[schemars(description = "Only show entries by this author.")]
    #[serde(default)]
    pub author: Option<String>,

    #[schemars(description = "Show at most the newest N matching entries (default 50).")]
    #[serde(default)]
    pub limit: Option<usize>,

    #[schemars(
        description = "Also show entries that a later correction superseded (hidden by default)."
    )]
    #[serde(default)]
    pub include_superseded: bool,
}

/// Read the shared blackboard (optionally filtered).
#[derive(Debug, Default)]
pub struct BoardReadTool;

impl crate::types::tool_metadata::ToolMetadata for BoardReadTool {
    fn kind(&self) -> ToolKind {
        ToolKind::BoardRead
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        r#"Read the shared team blackboard — the ledger of facts already verified by agents in this session.

ALWAYS read the board before planning or starting work: a teammate may already have verified what you need, or corrected an assumption you hold. Filter by topic/kind/author when the board is large. Reading also marks entries as seen for you."#
    }
}

impl xai_tool_runtime::Tool for BoardReadTool {
    type Args = BoardReadInput;
    type Output = ToolOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new("board_read").expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &::xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            "board_read",
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

    #[tracing::instrument(name = "tool.board_read", skip_all)]
    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: BoardReadInput,
    ) -> Result<ToolOutput, xai_tool_runtime::ToolError> {
        use crate::types::tool_metadata::shared_resources;
        let resources = shared_resources(&ctx)?;

        let cfg = {
            let res = resources.lock().await;
            board_cfg_or_missing(&res, "board_read")?
        };

        let entries = read_entries(cfg.path.clone()).await.map_err(|e| {
            xai_tool_runtime::ToolError::custom(
                "board_read",
                format!("failed to read blackboard {}: {e}", cfg.path.display()),
            )
        })?;
        let total = entries.len() as u64;

        // Reading the board catches the agent up on everything.
        {
            let mut res = resources.lock().await;
            let cursor = res.get_or_default::<State<BoardCursor>>();
            cursor.0.seen = total;
        }

        // A correction supersedes the entry it replies to: superseded facts
        // are dead by default, not merely flagged, so agents can't keep
        // building on them by skimming the board.
        let superseded: std::collections::HashSet<&str> = entries
            .iter()
            .filter(|e| e.kind == EntryKind::Correction)
            .filter_map(|e| e.reply_to.as_deref())
            .collect();
        let hidden_superseded = entries
            .iter()
            .filter(|e| superseded.contains(e.id.as_str()))
            .count();

        let limit = input.limit.unwrap_or(50).max(1);
        let matching: Vec<String> = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| input.include_superseded || !superseded.contains(e.id.as_str()))
            .filter(|(_, e)| {
                input
                    .topic
                    .as_deref()
                    .is_none_or(|t| e.topic.contains(t))
                    && input.kind.is_none_or(|k| e.kind == k)
                    && input
                        .author
                        .as_deref()
                        .is_none_or(|a| e.author == a)
            })
            .map(|(i, e)| {
                let mut line = e.render(i as u64 + 1);
                if superseded.contains(e.id.as_str()) {
                    line.push_str(" [superseded by a correction]");
                }
                line
            })
            .collect();

        let shown = matching.len().min(limit);
        let text = if matching.is_empty() {
            if total == 0 {
                "The blackboard is empty. You are the first: verify the current state with tools and post what you find (board_post) so teammates can build on it.".to_string()
            } else {
                format!("No entries match the filter ({total} entries on the board in total).")
            }
        } else {
            let listed = matching[matching.len() - shown..].join("\n");
            let mut header = format!(
                "Blackboard ({} matching of {} total, showing newest {})",
                matching.len(),
                total,
                shown
            );
            if !input.include_superseded && hidden_superseded > 0 {
                header.push_str(&format!(
                    ", {hidden_superseded} superseded hidden — includeSuperseded to view"
                ));
            }
            format!("{header}:\n{listed}")
        };

        Ok(ToolOutput::Text(text.into()))
    }
}

// ── Digest reminder ─────────────────────────────────────────────────────

/// Cross-cutting reminder: after any tool call, surface board entries this
/// agent has not seen yet (excluding its own posts). This is what makes the
/// blackboard an asynchronous meeting — agents keep working and get new
/// entries pushed into their context as they appear.
///
/// Broadcast-tax control: with N agents posting, naive push is O(N²) tokens.
/// Corrections are always delivered in full; everything else is budgeted to
/// [`DIGEST_MAX_ENTRIES`] newest entries per digest, with the overflow
/// collapsed to a topic list + a pointer to `board_read`.
///
/// Hard consequence of corrections: receiving one resets this agent's
/// [`VerifyGate`], so its next planning/delegation call is refused until it
/// has re-observed the world — "please re-verify" is enforced, not advisory.
#[derive(Debug, Default)]
pub struct BlackboardDigestReminder;

/// Max non-correction entries shown per digest.
const DIGEST_MAX_ENTRIES: usize = 5;

#[async_trait::async_trait]
impl Reminder for BlackboardDigestReminder {
    async fn collect_reminders(
        &self,
        resources: crate::types::resources::SharedResources,
        _tool_output: &ToolOutput,
    ) -> Vec<String> {
        let (cfg, seen) = {
            let mut res = resources.lock().await;
            let Some(cfg) = res.get::<BlackboardCfg>().cloned() else {
                return vec![];
            };
            let cursor = res.get_or_default::<State<BoardCursor>>();
            (cfg, cursor.0.seen)
        };

        let entries = match read_entries(cfg.path.clone()).await {
            Ok(e) => e,
            Err(_) => return vec![],
        };
        let total = entries.len() as u64;
        if total <= seen {
            return vec![];
        }

        let fresh: Vec<(usize, &BoardEntry)> = entries
            .iter()
            .enumerate()
            .skip(seen as usize)
            .filter(|(_, e)| e.author != cfg.author)
            .collect();

        let has_correction = fresh
            .iter()
            .any(|(_, e)| e.kind == EntryKind::Correction);

        {
            let mut res = resources.lock().await;
            let cursor = res.get_or_default::<State<BoardCursor>>();
            cursor.0.seen = total;
            // A correction invalidates something this agent may be relying
            // on: force re-verification before its next plan/delegation.
            if has_correction {
                res.get_or_default::<VerifyGate>().verified = false;
            }
        }

        if fresh.is_empty() {
            return vec![];
        }

        // Corrections always shown in full; the rest budgeted, newest last.
        let (corrections, others): (Vec<_>, Vec<_>) = fresh
            .iter()
            .partition(|(_, e)| e.kind == EntryKind::Correction);
        let overflow = others.len().saturating_sub(DIGEST_MAX_ENTRIES);
        let mut lines: Vec<String> = corrections
            .iter()
            .map(|(i, e)| format!("!! {}", e.render(*i as u64 + 1)))
            .collect();
        lines.extend(
            others
                .iter()
                .skip(overflow)
                .map(|(i, e)| e.render(*i as u64 + 1)),
        );

        let mut msg = format!(
            "New blackboard entries from teammates ({}):\n{}",
            fresh.len(),
            lines.join("\n")
        );
        if overflow > 0 {
            let mut topics: Vec<&str> = others
                .iter()
                .take(overflow)
                .map(|(_, e)| e.topic.as_str())
                .collect();
            topics.dedup();
            msg.push_str(&format!(
                "\n(+{} older entries not shown, topics: {} — use board_read to catch up)",
                overflow,
                topics.join(", ")
            ));
        }
        if has_correction {
            msg.push_str(
                "\nEntries marked '!!' are corrections: something you may believe is stale. \
                 Your verify-first gate has been reset — re-verify the affected facts before \
                 planning or delegating again.",
            );
        }
        msg.push_str(
            "\nIf any entry affects your current work, act on it; reply or challenge with board_post (replyTo).",
        );
        vec![msg]
    }
}

// ── Verify-before-plan gate ─────────────────────────────────────────────

/// Per-turn gate state: has this agent verified anything (read a file,
/// searched, read the board, run a command) since the current user turn
/// started? The dispatch layer refuses planning/delegation tools
/// (`todo_write`, `task`, `exit_plan_mode`) until it has.
///
/// GUARANTEE BOUNDARY: the gate proves an observation *happened* this
/// turn, not that it was relevant or understood — reading one unrelated
/// file satisfies it. It is a floor against planning blind, defeatable by
/// minimal compliance; relevance is enforced only by the prompt
/// discipline and by teammates challenging unsupported posts.
///
/// Ephemeral resource: reset by the session host at each user-turn start,
/// and by [`BlackboardDigestReminder`] when a correction arrives.
#[derive(Debug, Clone, Default)]
pub struct VerifyGate {
    pub verified: bool,
}

/// Tool kinds that count as verifying the current state of the world.
pub fn kind_verifies(kind: ToolKind) -> bool {
    matches!(
        kind,
        ToolKind::Read
            | ToolKind::Search
            | ToolKind::ListDir
            | ToolKind::Lsp
            | ToolKind::Execute
            | ToolKind::WebSearch
            | ToolKind::WebFetch
            | ToolKind::MemorySearch
            | ToolKind::MemoryGet
            | ToolKind::BackgroundTaskAction
            | ToolKind::Monitor
            | ToolKind::BoardRead
    )
}

/// Tool kinds gated behind prior verification.
pub fn kind_requires_verification(kind: ToolKind) -> bool {
    matches!(kind, ToolKind::Plan | ToolKind::Task | ToolKind::ExitPlan)
}

/// The rejection message returned when the gate blocks a call.
pub fn gate_rejection_message(tool_name: &str) -> String {
    format!(
        "'{tool_name}' blocked by the verify-first policy: you have not verified the current \
         state yet this turn. Before planning or delegating, first (1) read the shared \
         blackboard (board_read) to see what teammates already verified, and (2) check the \
         actual state with tools (read files, search, run commands). Then retry '{tool_name}'."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::resources::Resources;
    use crate::types::tool_metadata::test_ctx;

    fn cfg_in(dir: &Path, author: &str) -> BlackboardCfg {
        BlackboardCfg {
            path: dir.join("blackboard.jsonl"),
            author: author.to_string(),
        }
    }

    fn post_input(kind: EntryKind, topic: &str, body: &str, evidence: &[&str]) -> BoardPostInput {
        BoardPostInput {
            kind,
            topic: topic.into(),
            body: body.into(),
            evidence: evidence.iter().map(|s| s.to_string()).collect(),
            reply_to: None,
            outcome: None,
        }
    }

    fn text_of(output: ToolOutput) -> String {
        match output {
            ToolOutput::Text(t) => t.text,
            other => panic!("expected Text output, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn post_then_read_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let mut resources = Resources::new();
        resources.insert(cfg_in(tmp.path(), "tester"));
        let shared = resources.into_shared();

        let out = xai_tool_runtime::Tool::run(
            &BoardPostTool,
            test_ctx(shared.clone()),
            post_input(
                EntryKind::Finding,
                "build",
                "workspace builds clean",
                &["cargo build -> Finished dev profile"],
            ),
        )
        .await
        .unwrap();
        assert!(text_of(out).contains("Posted to blackboard"));

        let out = xai_tool_runtime::Tool::run(
            &BoardReadTool,
            test_ctx(shared.clone()),
            BoardReadInput {
                topic: None,
                kind: None,
                author: None,
                limit: None,
                include_superseded: false,
            },
        )
        .await
        .unwrap();
        let text = text_of(out);
        assert!(text.contains("workspace builds clean"), "got: {text}");
        assert!(text.contains("cargo build -> Finished"), "got: {text}");
    }

    #[tokio::test]
    async fn claim_without_evidence_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let mut resources = Resources::new();
        resources.insert(cfg_in(tmp.path(), "tester"));
        let shared = resources.into_shared();

        let err = xai_tool_runtime::Tool::run(
            &BoardPostTool,
            test_ctx(shared),
            post_input(EntryKind::Claim, "auth", "login is broken", &[]),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("requires evidence"), "got: {err}");
    }

    #[tokio::test]
    async fn question_without_evidence_allowed() {
        let tmp = tempfile::tempdir().unwrap();
        let mut resources = Resources::new();
        resources.insert(cfg_in(tmp.path(), "tester"));
        let shared = resources.into_shared();

        xai_tool_runtime::Tool::run(
            &BoardPostTool,
            test_ctx(shared),
            post_input(EntryKind::Question, "auth", "which auth flow do we use?", &[]),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn read_with_filters() {
        let tmp = tempfile::tempdir().unwrap();
        let mut resources = Resources::new();
        resources.insert(cfg_in(tmp.path(), "tester"));
        let shared = resources.into_shared();

        for (kind, topic, body) in [
            (EntryKind::Finding, "build", "builds fine"),
            (EntryKind::TestResult, "tests", "3 failures in auth"),
            (EntryKind::Finding, "auth", "uses jwt"),
        ] {
            xai_tool_runtime::Tool::run(
                &BoardPostTool,
                test_ctx(shared.clone()),
                post_input(kind, topic, body, &["x:1"]),
            )
            .await
            .unwrap();
        }

        let out = xai_tool_runtime::Tool::run(
            &BoardReadTool,
            test_ctx(shared.clone()),
            BoardReadInput {
                topic: Some("auth".into()),
                kind: None,
                author: None,
                limit: None,
                include_superseded: false,
            },
        )
        .await
        .unwrap();
        let text = text_of(out);
        assert!(text.contains("uses jwt"));
        assert!(!text.contains("builds fine"));
    }

    #[tokio::test]
    async fn digest_surfaces_only_foreign_unseen_entries() {
        let tmp = tempfile::tempdir().unwrap();

        // Agent A posts.
        let mut res_a = Resources::new();
        res_a.insert(cfg_in(tmp.path(), "agent-a"));
        let shared_a = res_a.into_shared();
        xai_tool_runtime::Tool::run(
            &BoardPostTool,
            test_ctx(shared_a.clone()),
            post_input(EntryKind::Finding, "db", "schema has 3 tables", &["schema.sql:1"]),
        )
        .await
        .unwrap();

        // Agent B (separate session resources, same board file) gets a digest.
        let mut res_b = Resources::new();
        res_b.insert(cfg_in(tmp.path(), "agent-b"));
        let shared_b = res_b.into_shared();
        let dummy = ToolOutput::Text("x".into());
        let reminders = BlackboardDigestReminder
            .collect_reminders(shared_b.clone(), &dummy)
            .await;
        assert_eq!(reminders.len(), 1);
        assert!(reminders[0].contains("schema has 3 tables"));

        // Second digest: nothing new.
        let reminders = BlackboardDigestReminder
            .collect_reminders(shared_b, &dummy)
            .await;
        assert!(reminders.is_empty());

        // Agent A never sees its own post.
        let reminders = BlackboardDigestReminder
            .collect_reminders(shared_a, &dummy)
            .await;
        assert!(reminders.is_empty());
    }

    #[tokio::test]
    async fn correction_flagged_in_digest() {
        let tmp = tempfile::tempdir().unwrap();
        let mut res_a = Resources::new();
        res_a.insert(cfg_in(tmp.path(), "agent-a"));
        let shared_a = res_a.into_shared();
        xai_tool_runtime::Tool::run(
            &BoardPostTool,
            test_ctx(shared_a),
            post_input(
                EntryKind::Correction,
                "build",
                "board says build is green but main fails to link",
                &["cargo build -> linker error in xai-grok-shell"],
            ),
        )
        .await
        .unwrap();

        let mut res_b = Resources::new();
        res_b.insert(cfg_in(tmp.path(), "agent-b"));
        let reminders = BlackboardDigestReminder
            .collect_reminders(res_b.into_shared(), &ToolOutput::Text("x".into()))
            .await;
        assert_eq!(reminders.len(), 1);
        assert!(reminders[0].contains("!!"));
        assert!(reminders[0].contains("corrections"));
    }

    #[test]
    fn gate_kind_classification() {
        assert!(kind_verifies(ToolKind::Read));
        assert!(kind_verifies(ToolKind::BoardRead));
        assert!(kind_verifies(ToolKind::Execute));
        assert!(!kind_verifies(ToolKind::Edit));
        assert!(!kind_verifies(ToolKind::Task));

        assert!(kind_requires_verification(ToolKind::Plan));
        assert!(kind_requires_verification(ToolKind::Task));
        assert!(kind_requires_verification(ToolKind::ExitPlan));
        assert!(!kind_requires_verification(ToolKind::Read));
        assert!(!kind_requires_verification(ToolKind::Edit));
    }

    fn raw_entry(id: &str, author: &str, kind: EntryKind, topic: &str, body: &str) -> BoardEntry {
        BoardEntry {
            id: id.into(),
            ts: now_rfc3339(),
            author: author.into(),
            kind,
            topic: topic.into(),
            body: body.into(),
            evidence: vec![],
            reply_to: None,
            outcome: None,
        }
    }

    #[tokio::test]
    async fn evidence_citing_missing_file_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let mut resources = Resources::new();
        resources.insert(cfg_in(tmp.path(), "tester"));
        resources.insert(crate::types::resources::Cwd(tmp.path().to_path_buf()));
        let shared = resources.into_shared();

        let err = xai_tool_runtime::Tool::run(
            &BoardPostTool,
            test_ctx(shared),
            post_input(
                EntryKind::Finding,
                "auth",
                "made up",
                &["src/ghost.rs:42 totally real"],
            ),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("does not exist"), "got: {err}");
    }

    #[tokio::test]
    async fn evidence_citing_line_past_eof_rejected_and_valid_line_accepted() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/real.rs"), "line1\nline2\nline3\n").unwrap();
        let mut resources = Resources::new();
        resources.insert(cfg_in(tmp.path(), "tester"));
        resources.insert(crate::types::resources::Cwd(tmp.path().to_path_buf()));
        let shared = resources.into_shared();

        let err = xai_tool_runtime::Tool::run(
            &BoardPostTool,
            test_ctx(shared.clone()),
            post_input(EntryKind::Finding, "t", "x", &["src/real.rs:99 phantom"]),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("only 3 lines"), "got: {err}");

        xai_tool_runtime::Tool::run(
            &BoardPostTool,
            test_ctx(shared),
            post_input(EntryKind::Finding, "t", "x", &["src/real.rs:2 line2 content"]),
        )
        .await
        .unwrap();
    }

    #[test]
    fn evidence_parser_ignores_non_path_refs() {
        assert!(parse_evidence_ref("cargo test -> 5 passed").is_none());
        assert!(parse_evidence_ref("https://example.com/a:1").is_none());
        assert!(parse_evidence_ref("note: something").is_none());
        let r = parse_evidence_ref("src/a.rs:12-14 def foo").unwrap();
        assert_eq!(r.path, "src/a.rs");
        assert_eq!(r.line, 12);
    }

    #[tokio::test]
    async fn superseded_entries_hidden_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("blackboard.jsonl");
        append_entry_sync(&path, &raw_entry("e1", "a", EntryKind::Finding, "db", "3 tables"))
            .unwrap();
        let mut correction =
            raw_entry("e2", "b", EntryKind::Correction, "db", "actually 4 tables");
        correction.reply_to = Some("e1".into());
        correction.evidence = vec!["schema checked".into()];
        append_entry_sync(&path, &correction).unwrap();

        let mut resources = Resources::new();
        resources.insert(cfg_in(tmp.path(), "reader"));
        let shared = resources.into_shared();

        let read = |include| BoardReadInput {
            topic: None,
            kind: None,
            author: None,
            limit: None,
            include_superseded: include,
        };
        let out = xai_tool_runtime::Tool::run(&BoardReadTool, test_ctx(shared.clone()), read(false))
            .await
            .unwrap();
        let text = text_of(out);
        assert!(!text.contains("3 tables"), "superseded must be hidden: {text}");
        assert!(text.contains("superseded hidden"), "got: {text}");

        let out = xai_tool_runtime::Tool::run(&BoardReadTool, test_ctx(shared), read(true))
            .await
            .unwrap();
        let text = text_of(out);
        assert!(text.contains("3 tables"), "includeSuperseded must show it: {text}");
        assert!(text.contains("[superseded by a correction]"), "got: {text}");
    }

    #[tokio::test]
    async fn digest_budgets_noncorrections_and_lists_overflow_topics() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("blackboard.jsonl");
        for i in 0..8 {
            append_entry_sync(
                &path,
                &raw_entry(
                    &format!("e{i}"),
                    "poster",
                    EntryKind::Finding,
                    &format!("topic{i}"),
                    &format!("body {i}"),
                ),
            )
            .unwrap();
        }
        let mut res = Resources::new();
        res.insert(cfg_in(tmp.path(), "reader"));
        let reminders = BlackboardDigestReminder
            .collect_reminders(res.into_shared(), &ToolOutput::Text("x".into()))
            .await;
        assert_eq!(reminders.len(), 1);
        let msg = &reminders[0];
        // newest 5 shown, 3 oldest collapsed
        assert!(msg.contains("body 7") && msg.contains("body 3"), "got: {msg}");
        assert!(!msg.contains("body 2"), "oldest must be collapsed: {msg}");
        assert!(msg.contains("+3 older entries not shown"), "got: {msg}");
        assert!(msg.contains("topic0"), "overflow topics listed: {msg}");
    }

    #[tokio::test]
    async fn correction_in_digest_resets_verify_gate() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("blackboard.jsonl");
        let mut correction =
            raw_entry("c1", "poster", EntryKind::Correction, "db", "stale fact");
        correction.evidence = vec!["x".into()];
        append_entry_sync(&path, &correction).unwrap();

        let mut res = Resources::new();
        res.insert(cfg_in(tmp.path(), "reader"));
        res.insert(VerifyGate { verified: true });
        let shared = res.into_shared();
        let reminders = BlackboardDigestReminder
            .collect_reminders(shared.clone(), &ToolOutput::Text("x".into()))
            .await;
        assert!(reminders[0].contains("gate has been reset"));
        let res = shared.lock().await;
        assert!(
            !res.get::<VerifyGate>().unwrap().verified,
            "correction must reset the verify gate"
        );
    }

    #[test]
    fn skips_corrupt_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("blackboard.jsonl");
        let good = BoardEntry {
            id: "1".into(),
            ts: now_rfc3339(),
            author: "a".into(),
            kind: EntryKind::Finding,
            topic: "t".into(),
            body: "b".into(),
            evidence: vec![],
            reply_to: None,
            outcome: None,
        };
        let mut content = serde_json::to_string(&good).unwrap();
        content.push('\n');
        content.push_str("{ torn line\n");
        std::fs::write(&path, content).unwrap();

        let entries = read_entries_sync(&path).unwrap();
        assert_eq!(entries.len(), 1);
    }
}
