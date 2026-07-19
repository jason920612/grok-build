//! Repro: OrientationReminder must nudge after enough calls with no map.
//!
//! The state is self-counting and persisted (State<OrientationState>) —
//! regression coverage for the live failure where the ephemeral
//! `EvidenceTimeline` counter was reset by shell harness rebuilds and the
//! nudge never fired in a real goal session.

use xai_grok_tools::implementations::grok_build::blackboard::BlackboardCfg;
use xai_grok_tools::implementations::grok_build::compass::OrientationReminder;
use xai_grok_tools::types::output::ToolOutput;
use xai_grok_tools::types::resources::Resources;
use xai_grok_tools::types::template_renderer::TemplateRenderer;
use xai_grok_tools::types::tool::{Reminder, ToolKind};

#[tokio::test]
async fn nudges_when_no_mission_map_after_enough_calls() {
    let tmp = tempfile::tempdir().unwrap();
    let mut res = Resources::new();
    res.insert(BlackboardCfg {
        path: tmp.path().join("blackboard.jsonl"),
        author: "main".to_string(),
    });
    res.insert(TemplateRenderer::new(
        [(ToolKind::MapUpdate, "map_update".to_string())].into(),
        Default::default(),
    ));
    let shared = res.into_shared();
    let out = ToolOutput::Text("ok".into());

    let mut nudge_at = None;
    for i in 1..=12u64 {
        let reminders = OrientationReminder
            .collect_reminders(shared.clone(), &out)
            .await;
        if reminders.iter().any(|r| r.contains("No mission map")) {
            nudge_at = Some(i);
            break;
        }
    }
    assert_eq!(nudge_at, Some(8), "nudge must fire exactly at the threshold");

    // And only once.
    let again = OrientationReminder
        .collect_reminders(shared.clone(), &out)
        .await;
    assert!(
        !again.iter().any(|r| r.contains("No mission map")),
        "nudge must be one-time"
    );
}

#[tokio::test]
async fn no_nudge_without_map_update_in_toolset() {
    let tmp = tempfile::tempdir().unwrap();
    let mut res = Resources::new();
    res.insert(BlackboardCfg {
        path: tmp.path().join("blackboard.jsonl"),
        author: "explore#abc".to_string(),
    });
    // No TemplateRenderer with MapUpdate: a subagent without the tool must
    // never be told to call it.
    let shared = res.into_shared();
    let out = ToolOutput::Text("ok".into());
    for _ in 0..20 {
        let reminders = OrientationReminder
            .collect_reminders(shared.clone(), &out)
            .await;
        assert!(
            !reminders.iter().any(|r| r.contains("No mission map")),
            "must not nudge an agent that lacks map_update"
        );
    }
}
