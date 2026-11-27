//! Scheduled and event-driven automations (`packages/schema/src/automation.ts`,
//! `GET /api/experimental/automation`).

use serde_json::{Map, Value};

/// Trigger schedule or event subscription (`Automation.Trigger`).
#[derive(Clone, Debug, PartialEq)]
pub enum AutomationTrigger {
    At { at: String, tz: Option<String> },
    Every { every: String, tz: Option<String> },
    Cron { cron: String, tz: Option<String> },
    Event { server: String, event: String, args: Option<Map<String, Value>> },
}

/// Destination of a prompt automation (`Automation.Target`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AutomationTarget {
    Session(String),
    Subagent(String),
}

/// Action executed when an automation fires (`Automation.Action`).
#[derive(Clone, Debug, PartialEq)]
pub enum AutomationAction {
    Prompt { prompt: String, to: AutomationTarget },
    Code { code: String, args: Option<Value> },
    Shell { command: String },
}

/// Lifecycle state (`Automation.Status`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AutomationStatus {
    Active,
    Paused,
    Done,
}

impl AutomationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Done => "done",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "active" => Some(Self::Active),
            "paused" => Some(Self::Paused),
            "done" => Some(Self::Done),
            _ => None,
        }
    }
}

/// Outcome of the last run (`Automation.Outcome`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AutomationOutcome {
    Fired,
    Late,
    Skipped,
    Disabled,
    Failed,
    Expired,
}

impl AutomationOutcome {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "fired" => Some(Self::Fired),
            "late" => Some(Self::Late),
            "skipped" => Some(Self::Skipped),
            "disabled" => Some(Self::Disabled),
            "failed" => Some(Self::Failed),
            "expired" => Some(Self::Expired),
            _ => None,
        }
    }
}

/// Creation and last-update timestamps in Unix milliseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AutomationTime {
    pub created: i64,
    pub updated: i64,
}

/// One automation record (`Automation.Info`).
#[derive(Clone, Debug, PartialEq)]
pub struct AutomationInfo {
    pub id: String,
    pub name: String,
    pub project_id: String,
    pub session_id: Option<String>,
    pub directory: String,
    pub trigger: AutomationTrigger,
    pub tz: String,
    pub action: AutomationAction,
    pub expires: Option<i64>,
    pub status: AutomationStatus,
    pub next_due_at: Option<i64>,
    pub run_count: u64,
    pub last_run_at: Option<i64>,
    pub last_outcome: Option<AutomationOutcome>,
    pub last_output: Option<String>,
    pub time: AutomationTime,
}

/// Parses the JSON array returned by `GET /api/experimental/automation` (`{ data: [...] }` or `[...]`).
pub fn parse_automations(v: &Value) -> Vec<AutomationInfo> {
    let items = v.get("data").and_then(Value::as_array).or_else(|| v.as_array());
    items.map(|arr| arr.iter().filter_map(parse_automation).collect()).unwrap_or_default()
}

/// Parses one `Automation.Info` object (unwrapping `{ data: ... }` when present).
pub fn parse_automation(v: &Value) -> Option<AutomationInfo> {
    let v = v.get("data").filter(|d| d.is_object()).unwrap_or(v);
    let id = v.get("id")?.as_str()?.to_owned();
    let name = v.get("name")?.as_str()?.to_owned();
    let project_id = v.get("projectID").and_then(Value::as_str).unwrap_or_default().to_owned();
    let session_id = v.get("sessionID").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);
    let directory = v.get("directory").and_then(Value::as_str).unwrap_or_default().to_owned();
    let trigger = parse_trigger(v.get("trigger")?)?;
    let tz = v.get("tz").and_then(Value::as_str).unwrap_or("UTC").to_owned();
    let action = parse_action(v.get("action")?)?;
    let status = v.get("status").and_then(Value::as_str).and_then(AutomationStatus::parse)?;
    let time_obj = v.get("time");
    let time = AutomationTime {
        created: time_obj.and_then(|t| t.get("created")).and_then(Value::as_i64).unwrap_or(0),
        updated: time_obj.and_then(|t| t.get("updated")).and_then(Value::as_i64).unwrap_or(0),
    };
    Some(AutomationInfo {
        id,
        name,
        project_id,
        session_id,
        directory,
        trigger,
        tz,
        action,
        expires: v.get("expires").and_then(Value::as_i64),
        status,
        next_due_at: v.get("nextDueAt").and_then(Value::as_i64),
        run_count: v.get("runCount").and_then(Value::as_u64).unwrap_or(0),
        last_run_at: v.get("lastRunAt").and_then(Value::as_i64),
        last_outcome: v.get("lastOutcome").and_then(Value::as_str).and_then(AutomationOutcome::parse),
        last_output: v.get("lastOutput").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned),
        time,
    })
}

fn parse_trigger(v: &Value) -> Option<AutomationTrigger> {
    let tz = v.get("tz").and_then(Value::as_str).map(str::to_owned);
    match v.get("type")?.as_str()? {
        "at" => Some(AutomationTrigger::At { at: v.get("at")?.as_str()?.to_owned(), tz }),
        "every" => Some(AutomationTrigger::Every { every: v.get("every")?.as_str()?.to_owned(), tz }),
        "cron" => Some(AutomationTrigger::Cron { cron: v.get("cron")?.as_str()?.to_owned(), tz }),
        "event" => Some(AutomationTrigger::Event {
            server: v.get("server")?.as_str()?.to_owned(),
            event: v.get("event")?.as_str()?.to_owned(),
            args: v.get("args").and_then(Value::as_object).filter(|m| !m.is_empty()).cloned(),
        }),
        _ => None,
    }
}

fn parse_action(v: &Value) -> Option<AutomationAction> {
    match v.get("type")?.as_str()? {
        "prompt" => {
            let prompt = v.get("prompt")?.as_str()?.to_owned();
            let to_obj = v.get("to")?;
            let to = if let Some(s) = to_obj.get("session").and_then(Value::as_str) {
                AutomationTarget::Session(s.to_owned())
            } else {
                AutomationTarget::Subagent(to_obj.get("subagent").and_then(Value::as_str)?.to_owned())
            };
            Some(AutomationAction::Prompt { prompt, to })
        }
        "code" => Some(AutomationAction::Code {
            code: v.get("code")?.as_str()?.to_owned(),
            args: v.get("args").filter(|a| !a.is_null()).cloned(),
        }),
        "shell" => Some(AutomationAction::Shell {
            command: v.get("command")?.as_str()?.to_owned(),
        }),
        _ => None,
    }
}
