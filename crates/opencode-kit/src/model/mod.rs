//! OpenCode's data as this app sees it: projects, sessions and transcripts, parsed leniently
//! from the server's JSON (`/api/project`, `/api/session`, `/api/session/:id/message`) and
//! kept current by the same events `/api/event` streams.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use serde_json::Value;

mod attachment;
mod automation;
mod background_result;
mod catalog;
mod compaction;
mod console_extensions;
mod inbox;
mod job;
mod notice;
mod parse;
mod pending;
mod plugin;
pub mod preview;
mod tool;
mod transcript;
pub mod turn;

pub use attachment::{Attachment, base64_decode, media_hash, parse_attachment, prompt_extras_generation, set_user_files, user_files};
pub use automation::{AutomationAction, AutomationInfo, AutomationOutcome, AutomationStatus, AutomationTarget, AutomationTime, AutomationTrigger, parse_automation, parse_automations};
pub use background_result::{BackgroundResult, BackgroundResultKind, parse_background_result};
pub use catalog::{AttachmentInput, Model, Speed};
pub(crate) use attachment::extras_changed;
use attachment::parse_attachments;
pub use compaction::{Compaction, CompactionStatus, background_compaction};
pub use console_extensions::{CONSOLE_URL, ConsoleMcp, ConsoleMcpLinks, ConsoleWorkspace, console_mcp_links, console_mcps, console_skill_link, console_workspace};
pub use inbox::{Delivery, Inbox, InboxChange, InboxItem, NoticeEffect, NoticeEvent, NoticeState, inbox_payload, parse_inbox_item, step_notice};
pub use job::{JobInfo, JobKind, JobKindFilter, JobListQuery, JobLoadPhase, JobStatus, JobTime, JobTracker, JobTrackerEffect, JobTrackerEvent, is_job_event, merge_job_list, parse_job, parse_job_updated, parse_jobs};
pub use notice::{Finished, Notice, NoticeKind, Outcome, Retry, StepError, is_notice_event, parse_inbox_notice};
pub use parse::{parse_message, parse_messages, parse_projects, parse_session, parse_sessions};
use parse::tool_title;
pub use pending::{Field, FieldKind, FieldOption, Pending, parse_form, parse_permission};
pub use plugin::{PluginFeatures, PluginInfo, PluginSource, PluginState, SkillInfo, parse_plugin, parse_plugins, parse_skill, parse_skills, plugin_labels};
pub use tool::{shell_message, tool_error, tool_error_metadata, tool_output, tool_ran};
use tool::{keep_tool_error, keep_tool_output, keep_tool_ran, replace_tool_metadata};
pub use transcript::{Transcript, UNSEEN};
pub use crate::project::{LocationInfo, ProjectMeta, add_project_sandbox, parse_location, parse_managed_worktrees, parse_project, parse_worktrees, project_git, project_meta, project_sandboxes, project_sandboxes_version, set_project_sandboxes};
pub use crate::quote::{CommentOrigin, FileComment, NoteComment, PromptComment, Quote, set_user_comments, user_comments};

#[derive(Clone, Debug)]
pub struct Project {
    pub id: String,
    pub name: String,
    /// The project's root folder.
    pub directory: String,
    pub active: i64,
    pub icon: ProjectIcon,
}

/// The project's `icon`: a discovered image (`url`, usually a data URL), a chosen one
/// (`override`) or a chosen avatar colour. Empty strings when unset.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProjectIcon {
    pub url: String,
    pub override_url: String,
    pub color: String,
}

impl ProjectIcon {
    pub fn parse(v: &Value) -> ProjectIcon {
        ProjectIcon { url: s(&v["url"]), override_url: s(&v["override"]), color: s(&v["color"]) }
    }

    /// The image to show for project `id`, as Desktop picks it (`getProjectAvatarSource`,
    /// `project-avatar.tsx:78-86`): a chosen image, else none when a colour is chosen, else
    /// OpenCode's favicon for OpenCode's own repo, else the discovered one.
    pub fn source(&self, id: &str) -> Option<&str> {
        if !self.override_url.is_empty() {
            return Some(&self.override_url);
        }
        if !self.color.is_empty() {
            return None;
        }
        if OPENCODE_PROJECT_IDS.contains(&id) {
            return Some(OPENCODE_FAVICON);
        }
        (!self.url.is_empty()).then_some(self.url.as_str())
    }
}

/// OpenCode's own repo (`project-avatar.tsx:72-76`): its ids hash the origin remote, V1's the
/// root commit. The repo holds no icon file the server would find, so Desktop names one.
const OPENCODE_PROJECT_IDS: [&str; 3] =
    ["012780c4098d08caa4ea8c479ed0a4690489f38d", "476fbfe6befa9e63ddd7f9ae71655999a0104256", "4b0ea68d7af9a6031a7ffda7ad66e0cb83315750"];

/// The drawing of `https://opencode.ai/favicon.svg` (`packages/ui/src/assets/favicon/favicon.svg`),
/// which Desktop loads for those ids: carried here, so it shows on the first frame and offline.
const OPENCODE_FAVICON: &str = "data:image/svg+xml;utf8,<svg width=\"512\" height=\"512\" viewBox=\"0 0 512 512\" fill=\"none\" xmlns=\"http://www.w3.org/2000/svg\"><rect width=\"512\" height=\"512\" fill=\"#131010\"/><path d=\"M320 224V352H192V224H320Z\" fill=\"#5A5858\"/><path fill-rule=\"evenodd\" clip-rule=\"evenodd\" d=\"M384 416H128V96H384V416ZM320 160H192V352H320V160Z\" fill=\"white\"/></svg>";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
}

impl Tokens {
    pub(super) fn parse(v: &Value) -> Tokens {
        let n = |v: &Value| v.as_u64().unwrap_or(0);
        Tokens {
            input: n(&v["input"]),
            output: n(&v["output"]),
            cache_read: n(&v["cache"]["read"]),
            cache_write: n(&v["cache"]["write"]),
            reasoning: n(&v["reasoning"]),
        }
    }
    /// Tokens in the context window for this step.
    pub fn context(&self) -> u64 {
        self.input + self.cache_read + self.cache_write + self.output
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelRef {
    pub id: String,
    pub provider: String,
    pub variant: String,
}

impl ModelRef {
    pub(super) fn parse(v: &Value) -> ModelRef {
        ModelRef {
            id: s(&v["id"]),
            provider: s(&v["providerID"]),
            variant: s(&v["variant"]),
        }
    }

    /// The catalog's key for it: `provider:id` ([`Model::key`]).
    pub fn key(&self) -> String {
        format!("{}:{}", self.provider, self.id)
    }

    /// "claude-opus-5-5" → "Claude Opus 5.5".
    pub fn display(&self) -> String {
        let mut out = Vec::new();
        let parts: Vec<&str> = self.id.split('-').collect();
        let mut i = 0;
        while i < parts.len() {
            let p = parts[i];
            if p.chars().all(|c| c.is_ascii_digit()) {
                let mut num = p.to_owned();
                while i + 1 < parts.len() && parts[i + 1].chars().all(|c| c.is_ascii_digit()) && parts[i + 1].len() <= 2 {
                    num.push('.');
                    num.push_str(parts[i + 1]);
                    i += 1;
                }
                out.push(num);
            } else if p.eq_ignore_ascii_case("gpt") {
                out.push("GPT".into());
            } else {
                out.push(capitalize(p));
            }
            i += 1;
        }
        if out.is_empty() { "Default model".into() } else { out.join(" ") }
    }
}

pub fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map_or(String::new(), |f| f.to_uppercase().collect::<String>() + c.as_str())
}

#[derive(Clone, Debug, Default)]
pub struct SessionInfo {
    pub id: String,
    pub parent: Option<String>,
    pub project: String,
    pub title: String,
    pub directory: String,
    pub updated: i64,
    pub model: ModelRef,
    pub cost: f64,
    /// When the session last went idle and when someone last viewed it (`time.idle`,
    /// `time.viewed`): unread while it went idle after the last view.
    pub idle: Option<i64>,
    pub viewed: Option<i64>,
    /// When its latest run started (`time.busy`).
    pub busy: Option<i64>,
    /// The session's agent, when it was created (`time.created`), and how its last run ended
    /// (`outcome`: "succeeded", "failed", "interrupted").
    pub agent: String,
    pub created: i64,
    pub outcome: Option<String>,
    /// The message a staged revert rewinds to (`revert.messageID`): it and everything after it is hidden.
    pub revert: Option<String>,
    /// When it was archived (`time.archived`).
    pub archived: Option<i64>,
    /// The session's `metadata`, which the server copies onto every child it starts.
    pub metadata: Value,
    /// Whether a desktop holds the session's browser.
    pub browser: BrowserGrant,
}

/// Whether a desktop holds a session's browser: the server writes a grant into the session's
/// `permissions` (`browser` `*` `allow`) while a desktop is attached to it and revokes it when
/// that attachment ends (`packages/plugin-browser/src/connection.ts`; event `session.permissions`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BrowserGrant {
    /// Details this window has not read from the server (a saved row, a placeholder): they say
    /// nothing about a grant (`packages/gui-plugins/src/browser/prompt-gate.ts` `unheld`).
    #[default]
    Unread,
    /// A desktop is attached: this window or another.
    Held,
    /// No desktop is attached.
    Free,
}

impl BrowserGrant {
    /// What a session's `permissions`, as the server sends them, say: it leaves them out of a
    /// session that has none.
    pub fn of(permissions: &Value) -> Self {
        let grant = |rule: &Value| rule["action"] == "browser" && rule["resource"] == "*" && rule["effect"] == "allow";
        if permissions.as_array().is_some_and(|rules| rules.iter().any(grant)) { Self::Held } else { Self::Free }
    }
}

impl SessionInfo {
    /// `metadata.seerProject`: the project session this one is, or was started under, and when
    /// it was marked.
    pub fn seer_project(&self) -> Option<(&str, i64)> {
        let m = &self.metadata["seerProject"];
        Some((m["id"].as_str()?, m["since"].as_i64()?))
    }

    /// When it was last active: `time.updated`, else `time.created` (`updated ?? created`).
    pub fn active_ms(&self) -> i64 {
        if self.updated > 0 { self.updated } else { self.created }
    }

    pub fn unread(&self) -> bool {
        self.idle.is_some_and(|i| self.viewed.is_none_or(|v| i > v))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ToolStatus {
    Pending,
    Running,
    Completed,
    Error,
}

#[derive(Clone, Debug)]
pub enum Part {
    Text { text: String },
    Reasoning { text: String, started: i64, completed: Option<i64> },
    /// `files`: what the tool returned besides text, such as the image a `read` loaded.
    /// `metadata`: the tool's own state (a subagent call's `sessionID`).
    /// A failed call keeps its error there too: see [`tool_error`]; and when it began running:
    /// see [`tool_ran`].
    Tool { id: String, name: String, status: ToolStatus, title: String, input: Value, files: Vec<Attachment>, metadata: Value },
}

/// The child session a subagent call runs in, once the call names it.
pub fn subagent_session<'a>(input: &'a Value, metadata: &'a Value) -> Option<&'a str> {
    metadata["sessionID"].as_str().or_else(|| input["sessionID"].as_str()).filter(|s| !s.is_empty())
}

/// A message's model (`{ id, providerID, variant }`), when it names one.
pub(super) fn model_of(v: &Value) -> Option<ModelRef> {
    let m = ModelRef::parse(v);
    (!m.id.is_empty() || !m.provider.is_empty()).then_some(m)
}

/// User messages' creation times, by message id, as [`user_files`] keeps their files.
fn user_created_store() -> &'static Mutex<HashMap<String, i64>> {
    static STORE: OnceLock<Mutex<HashMap<String, i64>>> = OnceLock::new();
    STORE.get_or_init(Default::default)
}

/// What the stores beside `Message::User` keep by message id (its files, creation times, mentions,
/// path attachments, comments), kept only for the messages `keep` names: an app that lets go of a
/// transcript calls it, so those go too. A message parsed again fills them again.
pub fn retain_user_messages(keep: &dyn Fn(&str) -> bool) {
    let mut dropped = attachment::retain_user_files(keep);
    {
        let mut created = user_created_store().lock().unwrap_or_else(PoisonError::into_inner);
        let before = created.len();
        created.retain(|id, _| keep(id));
        dropped |= created.len() != before;
    }
    crate::mention::retain_user_messages(keep);
    crate::quote::retain_user_comments(keep);
    dropped |= inbox::retain_inbox_payloads(keep);
    if dropped {
        // What was let go of a projection may have read: it projects again.
        extras_changed();
    }
}

/// When user message `id` was sent (ms), once known: a reply's duration runs from it.
pub fn user_created(id: &str) -> Option<i64> {
    user_created_store().lock().unwrap_or_else(PoisonError::into_inner).get(id).copied()
}

/// Records when user message `id` was sent (ms).
pub fn set_user_created(id: &str, at: i64) {
    user_created_store().lock().unwrap_or_else(PoisonError::into_inner).insert(id.to_owned(), at);
}

#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum Message {
    User { id: String, text: String },
    Assistant(Assistant),
    /// A shell command run from the composer (`!`): `text` reads `$ command`, `failed` when it
    /// timed out or exited non-zero, and `step` holds it as the `shell` call Desktop draws it as
    /// (`SessionShellMessage`): its command, output and how it ended (see [`shell_message`]).
    Note { id: String, text: String, failed: bool, step: Box<Assistant> },
    Compaction(Compaction),
    /// A switched agent or model, a move, updated instructions, a restart.
    Notice(Notice),
}

/// Provider-supplied phase label on an assistant text part (`content.native.phase`,
/// anomalyco/opencode#64917): takes precedence over tool order when grouping commentary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TextPhase {
    Commentary,
    FinalAnswer,
}

impl TextPhase {
    pub fn parse(v: &Value) -> Option<Self> {
        match v.as_str()? {
            "commentary" => Some(Self::Commentary),
            "final_answer" => Some(Self::FinalAnswer),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Assistant {
    pub id: String,
    pub completed: Option<i64>,
    pub parts: Vec<Part>,
    /// Optional provider `native.phase` labels by `parts` index (empty unless the provider sent one).
    pub text_phases: Vec<(usize, TextPhase)>,
    pub cost: f64,
    /// What the step reported as it finished (`tokens`); none while it streams, or when it
    /// ended without them.
    pub tokens: Option<Tokens>,
    pub error: Option<String>,
    /// The error's details, beside its message in `error`.
    pub failure: Option<StepError>,
    /// The server is waiting to try this step again.
    pub retry: Option<Retry>,
    /// When it started, or 0 when unknown.
    pub created: i64,
    /// When the provider streamed its first output token (`time.firstToken`); none before then,
    /// or for steps recorded before the server kept it.
    pub first_token: Option<i64>,
    /// When the provider's response body ended (`time.streamed`), before its tools ran.
    pub streamed: Option<i64>,
    /// The model that ran it (`model` in `/api` and `session.step.started`), as Desktop's reply
    /// footer names it; `None` until known.
    pub model: Option<ModelRef>,
    /// The agent that ran it, as the footer names it when agents are shown.
    pub agent: Option<String>,
    /// The files the step changed, by path (`snapshot.files`), for its turn's Changed files.
    pub changed: Vec<String>,
}

impl Assistant {
    pub fn text_phase(&self, part: usize) -> Option<TextPhase> {
        self.text_phases.iter().find_map(|&(idx, phase)| (idx == part).then_some(phase))
    }

    pub fn set_text_phase(&mut self, part: usize, phase: TextPhase) {
        if let Some((_, existing)) = self.text_phases.iter_mut().find(|(idx, _)| *idx == part) {
            *existing = phase;
        } else {
            self.text_phases.push((part, phase));
        }
    }
}

impl Message {
    pub fn id(&self) -> &str {
        match self {
            Message::User { id, .. } | Message::Note { id, .. } => id,
            Message::Assistant(a) => &a.id,
            Message::Compaction(c) => &c.id,
            Message::Notice(n) => &n.id,
        }
    }
}

pub(super) fn s(v: &Value) -> String {
    v.as_str().unwrap_or("").to_owned()
}

/// A JSON array's strings.
pub(super) fn strings(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_owned)).collect()
}

pub(super) fn i(v: &Value) -> i64 {
    v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)).unwrap_or(0)
}

pub fn tokens_short(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}
