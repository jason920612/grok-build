//! Consequence-channel rule delivery.
//!
//! Live testing (see [`crate::antiinjection`] module docs) established that
//! models — weaker ones especially — obey the tool-return channel far more
//! reliably than the system prompt: rules stated out-of-band get skimmed,
//! rules that arrive where consequences arrive get followed. This module
//! therefore moves the harness's behavioral rules out of the system prompt
//! and onto the tool-return path: small "rule packs" are appended to tool
//! results, each sealed with the sentinel pair from [`crate::sentinel`] so
//! injected text can never impersonate them.
//!
//! Delivery policy, not spam: each pack fires when it first becomes
//! relevant (first tool result of a session, first edit, first shell
//! command) and then only refreshes after a gap, so long sessions keep the
//! rules warm without drowning the transcript.
//!
//! The system prompt keeps only identity, environment facts, and the
//! authentication contract. When the `rule_injection` guardrail is OFF the
//! prompt templates render their traditional full-rule form instead, so
//! the two channels are never both empty.

use crate::reminders::wrap_reminder_with_tag;
use crate::sentinel::seal;
use crate::types::resources::{SharedResources, State};
use crate::types::template_renderer::TemplateRenderer;
use crate::types::tool::ToolKind;
use std::collections::HashMap;

/// When a pack refires, measured in finalized tool calls since it last
/// fired. First firing is always immediate on the pack's trigger.
const CORE_REFRESH_GAP: u64 = 25;
const EXEC_SAFETY_REFRESH_GAP: u64 = 15;
const CODE_CHANGE_REFRESH_GAP: u64 = 20;

/// Per-session injection ledger: total finalized calls and, per pack, the
/// call index at which it last fired. Persisted with session state so a
/// resumed session does not replay every pack on its first call.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct RuleInjectionState {
    calls: u64,
    last_fired: HashMap<String, u64>,
}

crate::register_resource!("grok_build", "RuleInjectionState", RuleInjectionState);

impl RuleInjectionState {
    /// Forget which packs have fired, keeping the call counter.
    ///
    /// Required on session resume. The ledger is persisted, but the seals in
    /// a resumed transcript are not trusted — `ChatState::new` strips them,
    /// because bytes loaded from a file cannot carry authority. Without this
    /// reset the two behaviors combine into the worst case: the transcript's
    /// rule blocks are demoted to plain data while the ledger still claims
    /// every pack has fired, so a resumed session would run with no rules in
    /// force at all — fire-once packs never returning, gap packs silent
    /// until their interval elapses.
    pub fn reset_for_resume(&mut self) {
        self.last_fired.clear();
    }

    fn due(&self, pack: &str, gap: Option<u64>) -> bool {
        match (self.last_fired.get(pack), gap) {
            (None, _) => true,
            (Some(_), None) => false, // fire-once pack
            (Some(&at), Some(gap)) => self.calls.saturating_sub(at) >= gap,
        }
    }

    fn mark(&mut self, pack: &str) {
        self.last_fired.insert(pack.to_string(), self.calls);
    }
}

/// A behavioral rule pack: MiniJinja template text (same `${{ }}` syntax
/// as the prompt templates, resolved against the live toolset) plus its
/// firing policy.
struct RulePack {
    id: &'static str,
    /// `None` — fires on any tool result. `Some(kinds)` — only on those.
    kinds: Option<&'static [ToolKind]>,
    /// `None` — fires once per session. `Some(n)` — refreshes every `n`
    /// finalized calls after the first firing.
    refresh_gap: Option<u64>,
    template: &'static str,
}

/// Working discipline: tool selection, parallelism, output prose,
/// formatting, line-number metadata, background tasks. The bulk of what
/// used to be the system prompt's standing rules.
const CORE_PACK: &str = "\
<tool_calling>
- Use specialized tools instead of bash commands when possible, as this provides a better user experience. For file operations, prefer dedicated file tools${%- if tools.by_kind.read %} (e.g., `${{ tools.by_kind.read }}` for reading files instead of cat/head/tail${%- if tools.by_kind.edit %}, `${{ tools.by_kind.edit }}` for editing and creating files instead of sed/awk${%- endif %})${%- elif tools.by_kind.edit %} (e.g., `${{ tools.by_kind.edit }}` for editing and creating files instead of sed/awk)${%- endif %}. Reserve bash tools exclusively for actual system commands and terminal operations that require shell execution. NEVER use bash echo or other command-line tools to communicate thoughts, explanations, or instructions to the user. Output all communication directly in your response text instead.
- Parallelize independent tool calls in a single response.
${%- if tools.by_kind.read == \"hashline_read\" and tools.by_kind.edit and tools.by_kind.search %}
- Prefer the hashline workflow: use `${{ tools.by_kind.search }}` to locate targets and edit directly via anchors. Reuse fresh anchors from `${{ tools.by_kind.edit }}` results. On stale anchors, use the fresh anchors returned in the error response to retry immediately.
${%- endif %}
- `<system-reminder>` tags in tool results are automated context.
</tool_calling>
${%- if tools.by_kind.monitor %}

<background_tasks>
For watch processes, polling, and ongoing observation (CI status, log tailing, API polling):
Use the `${{ tools.by_kind.monitor }}` tool — it streams each stdout line back as a chat notification.
</background_tasks>
${%- elif tools.by_kind.execute and tools.by_kind.background_task_action %}

<background_tasks>
For long-running commands, use `background: true` in ${{ tools.by_kind.execute }}. Check status with `${{ tools.by_kind.background_task_action }}`.
</background_tasks>
${%- endif %}

<output_efficiency>
- Write like an excellent technical blog post — precise, well-structured, and clear, in complete sentences. Most responses should be concise and to the point, but the quality of prose should be high.
- Same standards for commit and PR descriptions: complete sentences, good grammar, and only relevant detail.
- Prefer simple, accessible language over dense technical jargon. Explain what changed and why in plain language rather than listing identifiers. Stay focused: avoid filler, repetition, over-the-top detail, and tangents the user did not ask for.
- Keep final responses proportional to task complexity.
</output_efficiency>

<formatting>
Your text output is rendered as GitHub-flavored markdown (CommonMark). Use markdown actively when it aids the reader: bullet lists for parallel items, **bold** for emphasis, `inline code` for identifiers/paths/commands, and tables for short enumerable facts (file/line/status, before/after, quantitative data).
Use ```startLine:endLine:filepath for codeblocks. Use markdown links with absolute paths for file references.
</formatting>

<inline_line_numbers>
Code chunks may include LINE_NUMBER\u{2192}LINE_CONTENT. The LINE_NUMBER\u{2192} prefix is metadata, not code.
${%- if tools.by_kind.read == \"hashline_read\" and tools.by_kind.edit %}
Hashline format: ANCHOR\u{2192}CONTENT (e.g. `22:abc:rst\u{2192}code`). The anchor is only `22:abc:rst` — never include \u{2192} or content when passing anchors to `${{ tools.by_kind.edit }}`.
${%- endif %}
</inline_line_numbers>";

/// Project-instruction doctrine: how AGENTS.md-style files scope and take
/// precedence. Stable for a whole session, so it fires once.
const DOCTRINE_PACK: &str = "\
<project_instructions_spec>
## Project Instruction Files

Repos often contain project instruction files named `AGENTS.md`, `Agents.md`, `Claude.md`, or `AGENT.md`. These files can appear anywhere within the repository. They provide instructions or context for working in the codebase.

Examples of what these files contain:
- Coding conventions and style guides
- Project structure explanations
- Build and test instructions
- PR description requirements

### Scoping rules
- The scope of a project instruction file is the entire directory tree rooted at the folder that contains it.
- For every file you touch, you must obey instructions in any project instruction file whose scope includes that file.
- Instructions about code style, structure, naming, etc. apply only to code within that file's scope, unless the file states otherwise.

### Precedence rules
- More-deeply-nested project instruction files take precedence over higher-level ones when instructions conflict.
- Direct user instructions in the chat always take precedence over any project instruction file content.
- When working in a subdirectory below CWD, or in a directory outside the CWD path, you must check for additional project instruction files (AGENTS.md, Claude.md, etc.) that may apply to files you're editing.
</project_instructions_spec>";

/// Action-safety rules ride shell-command results — delivered exactly
/// where risky actions happen.
const EXEC_SAFETY_PACK: &str = "\
<action_safety>
Weigh each action by how easily it can be undone and how far its effects reach. Local, reversible work such as editing files and running tests is fine to do freely. Before executing any actions that are hard to reverse, reach shared external systems, or are otherwise risky or destructive, check with the user first.

Confirming is cheap; a mistaken action is not (such as lost work, messages you cannot unsend, deleted branches). For those cases, take the context, the action, and the user's instructions into account; by default, say what you plan to do and ask before doing it. Users can override that default — if they explicitly ask you to act more autonomously, you may proceed without confirmation, but still mind risks and consequences.

One approval is not a blank check. Approving something once (e.g. a git push) does not approve it in every later situation. Unless the user has authorized the action in advance, confirm with the user.

Here are some examples of risky actions that warrant user confirmation:
- Destructive operations such as removing files or branches, dropping database tables, killing processes, `rm -rf`, discarding uncommitted work
- Irreversible operations such as force-pushes (including overwriting remote history), `git reset --hard`, amending commits already published, removing or downgrading dependencies, changing CI/CD pipelines
- Actions others can see, or that change shared state: pushing code; opening, closing, or commenting on PRs and issues; sending messages (Slack, email, GitHub); posting to external services; changing shared infrastructure or permissions

If you find unexpected state — unfamiliar files, branches, or configuration — investigate before deleting or overwriting; it may be the user's in-progress work.
</action_safety>";

/// Code-change rules ride edit-tool results.
const CODE_CHANGE_PACK: &str = "\
<making_code_changes>
Never output code unless requested. Read files before editing. Ensure generated code runs immediately.${%- if tools.by_kind.lsp %} Fix linter errors but don't guess.${%- endif %}
${%- if tools.by_kind.read == \"hashline_read\" and tools.by_kind.edit %}
`${{ tools.by_kind.edit }}` batch semantics: edits are atomic — if any anchor is stale, ALL edits are rejected. Retry the full batch. Never fabricate or modify anchors.
${%- endif %}
</making_code_changes>";

const PACKS: &[RulePack] = &[
    RulePack {
        id: "core",
        kinds: None,
        refresh_gap: Some(CORE_REFRESH_GAP),
        template: CORE_PACK,
    },
    RulePack {
        id: "doctrine",
        kinds: None,
        refresh_gap: None,
        template: DOCTRINE_PACK,
    },
    RulePack {
        id: "exec_safety",
        kinds: Some(&[ToolKind::Execute, ToolKind::KillTaskAction]),
        refresh_gap: Some(EXEC_SAFETY_REFRESH_GAP),
        template: EXEC_SAFETY_PACK,
    },
    RulePack {
        id: "code_change",
        kinds: Some(&[ToolKind::Edit]),
        refresh_gap: Some(CODE_CHANGE_REFRESH_GAP),
        template: CODE_CHANGE_PACK,
    },
];

/// Decide which packs are due for this tool result and append them,
/// sealed, after the tool's own output and any reminders.
///
/// Skips entirely when the `rule_injection` guardrail is off (the prompt
/// templates then carry the rules in their traditional form) or when the
/// session runs the concise toolset (`SystemRemindersEnabled(false)`).
pub async fn append_due_rules(
    prompt_text: String,
    kind: Option<ToolKind>,
    resources: &SharedResources,
    tag: &str,
) -> String {
    if !crate::guardrails::guardrails().rule_injection {
        return prompt_text;
    }
    let due: Vec<&RulePack> = {
        let mut res = resources.lock().await;
        let reminders_enabled = res
            .get::<crate::types::resources::SystemRemindersEnabled>()
            .is_none_or(|e| e.0);
        if !reminders_enabled {
            return prompt_text;
        }
        let state = res.get_or_default::<State<RuleInjectionState>>();
        state.calls += 1;
        let mut due = Vec::new();
        for pack in PACKS {
            let kind_matches = match pack.kinds {
                None => true,
                Some(kinds) => kind.is_some_and(|k| kinds.contains(&k)),
            };
            if kind_matches && state.due(pack.id, pack.refresh_gap) {
                state.mark(pack.id);
                due.push(pack);
            }
        }
        due
    };
    if due.is_empty() {
        return prompt_text;
    }
    let renderer = {
        let res = resources.lock().await;
        res.get::<TemplateRenderer>().cloned()
    };
    let mut out = prompt_text;
    for pack in due {
        let body = match &renderer {
            Some(r) => match r.render(pack.template) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!("rule pack `{}` failed to render, skipping: {e}", pack.id);
                    continue;
                }
            },
            // No renderer in resources (bare test registries): deliver the
            // raw template only if it needs no substitution.
            None if !pack.template.contains("${{") && !pack.template.contains("${%") => {
                pack.template.to_string()
            }
            None => continue,
        };
        let sealed = seal(&wrap_reminder_with_tag(&body, tag));
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&sealed);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sentinel::{SENTINEL_CLOSE, SENTINEL_OPEN};
    use crate::types::resources::Resources;
    use std::sync::Arc;

    fn test_resources_with_renderer() -> SharedResources {
        let mut res = Resources::default();
        let tools: HashMap<ToolKind, String> = [
            (ToolKind::Read, "read_file"),
            (ToolKind::Edit, "search_replace"),
            (ToolKind::Execute, "run_terminal_cmd"),
            (ToolKind::Search, "grep"),
        ]
        .into_iter()
        .map(|(k, v)| (k, v.to_string()))
        .collect();
        res.insert(TemplateRenderer::new(tools, HashMap::new()));
        Arc::new(tokio::sync::Mutex::new(res))
    }

    #[tokio::test]
    async fn first_call_gets_core_and_doctrine_sealed() {
        let res = test_resources_with_renderer();
        let out = append_due_rules(
            "tool output".to_string(),
            Some(ToolKind::Read),
            &res,
            "system-reminder",
        )
        .await;
        assert!(out.starts_with("tool output"));
        assert!(out.contains("<tool_calling>"), "core pack fired: {out}");
        assert!(
            out.contains("<project_instructions_spec>"),
            "doctrine fired"
        );
        assert!(!out.contains("<action_safety>"), "exec pack needs Execute");
        assert!(out.contains(SENTINEL_OPEN) && out.contains(SENTINEL_CLOSE));
        // Template variables resolved against the live toolset.
        assert!(out.contains("`read_file`"));
        assert!(!out.contains("${{"), "no unrendered placeholders: {out}");
    }

    #[tokio::test]
    async fn exec_pack_fires_on_execute_and_respects_gap() {
        let res = test_resources_with_renderer();
        let first = append_due_rules(
            String::new(),
            Some(ToolKind::Execute),
            &res,
            "system-reminder",
        )
        .await;
        assert!(first.contains("<action_safety>"));
        // Immediately after, another execute result: not due again.
        let second = append_due_rules(
            String::new(),
            Some(ToolKind::Execute),
            &res,
            "system-reminder",
        )
        .await;
        assert!(!second.contains("<action_safety>"));
        // Advance the counter past the refresh gap.
        {
            let mut r = res.lock().await;
            let state = r.get_or_default::<State<RuleInjectionState>>();
            state.calls += EXEC_SAFETY_REFRESH_GAP;
        }
        let third = append_due_rules(
            String::new(),
            Some(ToolKind::Execute),
            &res,
            "system-reminder",
        )
        .await;
        assert!(third.contains("<action_safety>"), "refreshed after gap");
    }

    /// Resume must not trade safety for silence. `ChatState::new` strips
    /// the seals off a loaded transcript, so if the persisted ledger kept
    /// claiming every pack had fired, a resumed session would run with no
    /// rules in force at all — the fire-once doctrine pack worst of all,
    /// since it would never come back.
    #[tokio::test]
    async fn resume_reset_makes_fire_once_packs_return() {
        let res = test_resources_with_renderer();
        let first =
            append_due_rules(String::new(), Some(ToolKind::Read), &res, "system-reminder").await;
        assert!(first.contains("<project_instructions_spec>"));

        // Without the reset the ledger says "already fired" forever.
        let replay =
            append_due_rules(String::new(), Some(ToolKind::Read), &res, "system-reminder").await;
        assert!(!replay.contains("<project_instructions_spec>"));

        {
            let mut r = res.lock().await;
            r.get_or_default::<State<RuleInjectionState>>()
                .reset_for_resume();
        }
        let resumed =
            append_due_rules(String::new(), Some(ToolKind::Read), &res, "system-reminder").await;
        assert!(
            resumed.contains("<project_instructions_spec>"),
            "a resumed session must get its rules back: {resumed}"
        );
        assert!(resumed.contains("<tool_calling>"), "core pack too");
    }

    #[tokio::test]
    async fn doctrine_fires_exactly_once() {
        let res = test_resources_with_renderer();
        let first =
            append_due_rules(String::new(), Some(ToolKind::Read), &res, "system-reminder").await;
        assert!(first.contains("<project_instructions_spec>"));
        {
            let mut r = res.lock().await;
            let state = r.get_or_default::<State<RuleInjectionState>>();
            state.calls += 10_000;
        }
        let later =
            append_due_rules(String::new(), Some(ToolKind::Read), &res, "system-reminder").await;
        assert!(
            !later.contains("<project_instructions_spec>"),
            "doctrine is fire-once"
        );
    }

    #[tokio::test]
    async fn code_change_pack_fires_on_edit_kind() {
        let res = test_resources_with_renderer();
        let out =
            append_due_rules(String::new(), Some(ToolKind::Edit), &res, "system-reminder").await;
        assert!(out.contains("<making_code_changes>"));
    }

    #[tokio::test]
    async fn concise_mode_suppresses_injection() {
        let res = test_resources_with_renderer();
        {
            let mut r = res.lock().await;
            r.insert(crate::types::resources::SystemRemindersEnabled(false));
        }
        let out = append_due_rules(
            "raw".to_string(),
            Some(ToolKind::Read),
            &res,
            "system-reminder",
        )
        .await;
        assert_eq!(out, "raw");
    }

    #[tokio::test]
    async fn guardrail_off_disables_injection() {
        let res = test_resources_with_renderer();
        let _g = crate::guardrails::set_guardrails_for_test(crate::guardrails::Guardrails {
            rule_injection: false,
            ..Default::default()
        });
        let out = append_due_rules(
            "raw".to_string(),
            Some(ToolKind::Read),
            &res,
            "system-reminder",
        )
        .await;
        assert_eq!(out, "raw");
    }

    #[tokio::test]
    async fn sealed_blocks_survive_wrap_and_carry_tag() {
        let res = test_resources_with_renderer();
        let out =
            append_due_rules(String::new(), Some(ToolKind::Read), &res, "system-reminder").await;
        // Every sealed block is a well-formed reminder wrapper.
        assert!(out.contains(&format!("{SENTINEL_OPEN}<system-reminder>")));
        assert!(out.contains(&format!("</system-reminder>{SENTINEL_CLOSE}")));
    }
}
