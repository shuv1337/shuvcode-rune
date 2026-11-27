//! Deterministic scenes and Storybook story for the Automations screen (`--scene automations`).

use opencode_kit::model::{
    AutomationAction, AutomationInfo, AutomationOutcome, AutomationStatus, AutomationTarget,
    AutomationTime, AutomationTrigger,
};
use serde_json::{Map, Value};

use crate::stories::{Story, Trigger, replay, stage};
use crate::{App, Route};

use super::source;

const VARIANTS: &[(&str, usize)] = &[
    ("Default", 0),
    ("Empty", 1),
    ("Setting off", 2),
    ("Delete confirm", 3),
];

const KEYS: &[Trigger] = &[
    replay(open),
    Trigger {
        key: 'd',
        label: "Toggle Completed group",
        run: |s, _| {
            let st = s.kernel.slots.get_mut(&super::AUTOMATIONS);
            st.show_done = !st.show_done;
        },
    },
];

fn sample_rows(now_ms: i64) -> Vec<AutomationInfo> {
    let base_dir = "/workspace/lumen/release";
    let mut event_args = Map::new();
    event_args.insert("environment".into(), Value::String("staging".into()));
    vec![
        AutomationInfo {
            id: "aut_deploy".into(),
            name: "Release health watch".into(),
            project_id: "proj_lumen".into(),
            session_id: Some("ses_fx0001aaaaaaaaaaaaaaaaaa".into()),
            directory: base_dir.into(),
            trigger: AutomationTrigger::Event {
                server: "deployments".into(),
                event: "deployment.finished".into(),
                args: Some(event_args),
            },
            tz: "UTC".into(),
            action: AutomationAction::Code {
                code: "console.log(`Deployment status: ${event.data.status}`)\nif (event.data.status === \"failed\")\n  return { send: { prompt: \"Review the failed deployment.\" } }".into(),
                args: None,
            },
            expires: None,
            status: AutomationStatus::Active,
            next_due_at: None,
            run_count: 3,
            last_run_at: Some(now_ms - 10 * 60_000),
            last_outcome: Some(AutomationOutcome::Fired),
            last_output: Some("Deployment status: succeeded".into()),
            time: AutomationTime { created: now_ms - 86_400_000, updated: now_ms - 60_000 },
        },
        AutomationInfo {
            id: "aut_brief".into(),
            name: "Morning release brief".into(),
            project_id: "proj_lumen".into(),
            session_id: Some("ses_fx0001aaaaaaaaaaaaaaaaaa".into()),
            directory: base_dir.into(),
            trigger: AutomationTrigger::Cron { cron: "0 9 * * 1-5".into(), tz: None },
            tz: "UTC".into(),
            action: AutomationAction::Prompt {
                prompt: "Summarize release activity since yesterday.".into(),
                to: AutomationTarget::Session("ses_fx0003aaaaaaaaaaaaaaaaaa".into()),
            },
            expires: None,
            status: AutomationStatus::Active,
            next_due_at: Some(now_ms + 86_400_000),
            run_count: 1,
            last_run_at: Some(now_ms - 86_400_000),
            last_outcome: Some(AutomationOutcome::Fired),
            last_output: None,
            time: AutomationTime { created: now_ms - 172_800_000, updated: now_ms - 120_000 },
        },
        AutomationInfo {
            id: "aut_smoke".into(),
            name: "Nightly smoke test".into(),
            project_id: "proj_harbor".into(),
            session_id: Some("ses_fx0006aaaaaaaaaaaaaaaaaa".into()),
            directory: "/workspace/harbor".into(),
            trigger: AutomationTrigger::Cron { cron: "0 2 * * *".into(), tz: None },
            tz: "UTC".into(),
            action: AutomationAction::Shell { command: "bun run test:smoke --reporter=dot".into() },
            expires: None,
            status: AutomationStatus::Active,
            next_due_at: Some(now_ms + 2 * 86_400_000),
            run_count: 12,
            last_run_at: Some(now_ms - 3_600_000),
            last_outcome: Some(AutomationOutcome::Fired),
            last_output: Some("Exit code: 0\n42 pass, 0 fail".into()),
            time: AutomationTime { created: now_ms - 259_200_000, updated: now_ms - 180_000 },
        },
        AutomationInfo {
            id: "aut_docs".into(),
            name: "Documentation review".into(),
            project_id: "proj_lumen".into(),
            session_id: Some("ses_fx0001aaaaaaaaaaaaaaaaaa".into()),
            directory: base_dir.into(),
            trigger: AutomationTrigger::Every { every: "1 day".into(), tz: None },
            tz: "UTC".into(),
            action: AutomationAction::Prompt {
                prompt: "Review the documentation changes.".into(),
                to: AutomationTarget::Subagent("reviewer".into()),
            },
            expires: None,
            status: AutomationStatus::Paused,
            next_due_at: None,
            run_count: 0,
            last_run_at: None,
            last_outcome: None,
            last_output: None,
            time: AutomationTime { created: now_ms - 345_600_000, updated: now_ms - 240_000 },
        },
        AutomationInfo {
            id: "aut_done".into(),
            name: "Pre-cut readiness check".into(),
            project_id: "proj_lumen".into(),
            session_id: None,
            directory: base_dir.into(),
            trigger: AutomationTrigger::At { at: "2026-11-26T09:00".into(), tz: None },
            tz: "UTC".into(),
            action: AutomationAction::Shell { command: "bun scripts/check-release.ts".into() },
            expires: None,
            status: AutomationStatus::Done,
            next_due_at: None,
            run_count: 1,
            last_run_at: Some(now_ms - 432_000_000),
            last_outcome: Some(AutomationOutcome::Expired),
            last_output: None,
            time: AutomationTime { created: now_ms - 518_400_000, updated: now_ms - 432_000_000 },
        },
    ]
}

fn setup_base(s: &mut App, enabled: bool) {
    crate::scenes::open_scene(s);
    s.finish_stream();
    crate::scenes::side_tabs(s);
    s.settings.automations = Some(enabled);
    *s.route = Route::Automations;
}

fn setup_default(s: &mut App) {
    setup_base(s, true);
    let now_ms = *s.now_ms;
    let rows = sample_rows(now_ms);
    source::seed_fixture(s, "release-runner-03", rows, &["/workspace/lumen/release", "/workspace/harbor"]);
}

fn setup_empty(s: &mut App) {
    setup_base(s, true);
    source::seed_fixture(s, "release-runner-03", Vec::new(), &["/workspace/lumen/release"]);
}

fn setup_off(s: &mut App) {
    setup_base(s, false);
    source::seed_fixture(s, "release-runner-03", Vec::new(), &[]);
}

fn open(s: &mut App, _play: &mut rune::Play<'_, App>) {
    match *s.fixture_run.variant {
        1 => setup_empty(s),
        2 => setup_off(s),
        3 => {
            setup_default(s);
            let entries = source::entries(s);
            if let Some(first) = entries.first() {
                super::ask_delete(s, first.key.clone(), first.server.clone(), first.row.id.clone(), first.row.name.clone());
            }
        }
        _ => setup_default(s),
    }
}

rune_register::register!(crate::stories::STORY_LIST: Story = [
    Story {
        path: "Shell/Automations",
        scene: |s| {
            setup_default(s);
            stage(s, |s, p| super::view::page(s, p, [980.0, 640.0]));
        },
        knobs: &[],
        keys: KEYS,
        variants: VARIANTS,
    },
]);

rune_register::register!(crate::scenes::SCENE_LIST: crate::scenes::Scene = [
    ("automations", setup_default),
    ("automations-empty", setup_empty),
    ("automations-off", setup_off),
]);
