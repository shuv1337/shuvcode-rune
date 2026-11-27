//! Session revert and fork operations on [`Server`] (`packages/app/src/session/{revert,fork}.ts`,
//! `packages/app/src/composer/submit.ts`).

use serde_json::Value;

use crate::client::{Prompt, Server};
use crate::wire_error::{WireError, WireKind};
use crate::model::{self, Delivery, SessionInfo};

impl Server {
    /// Marks `session` read up to `idle` (`POST /api/session/:id/view`, Desktop's `session.view`).
    pub fn view(&self, session: &str, idle: i64) -> Result<(), WireError> {
        self.write("POST", &format!("/api/session/{session}/view"), Some(serde_json::json!({ "idle": idle }))).map(|_| ())
    }

    /// Replaces `session`'s metadata (`PATCH /api/session/:id` with `{ metadata }`).
    pub fn set_metadata(&self, session: &str, metadata: Value) -> Result<(), WireError> {
        self.write("PATCH", &format!("/api/session/{session}"), Some(serde_json::json!({ "metadata": metadata }))).map(|_| ())
    }

    /// Archives `session` (`PATCH /api/session/:id` with `{ archived: true }`, Desktop's `session.update`).
    pub fn archive(&self, session: &str) -> Result<(), WireError> {
        self.write("PATCH", &format!("/api/session/{session}"), Some(serde_json::json!({ "archived": true }))).map(|_| ())
    }

    /// Brings `session` back from the archive (`PATCH /api/session/:id` with `{ archived: false }`).
    pub fn unarchive(&self, session: &str) -> Result<(), WireError> {
        self.write("PATCH", &format!("/api/session/{session}"), Some(serde_json::json!({ "archived": false }))).map(|_| ())
    }

    /// Waits for `session`'s active run to settle (`POST /api/experimental/session/:id/wait`).
    pub fn wait(&self, session: &str) -> Result<(), WireError> {
        self.write("POST", &format!("/api/experimental/session/{session}/wait"), None).map(|_| ())
    }

    /// Moves `session` to `directory` (`POST /api/session/:id/move`), waiting up to 30 s
    /// (Desktop's `WORKSPACE_MOVE_TIMEOUT_MS`).
    pub fn move_session(&self, session: &str, directory: &str) -> Result<(), WireError> {
        self.write_within("POST", &format!("/api/session/{session}/move"), Some(serde_json::json!({ "directory": directory })), std::time::Duration::from_secs(30)).map(|_| ())
    }

    /// Stages a revert at `message` (`POST /api/session/:id/revert/stage`).
    pub fn revert_stage(&self, session: &str, message: &str) -> Result<Value, WireError> {
        self.write("POST", &format!("/api/session/{session}/revert/stage"), Some(serde_json::json!({ "messageID": message })))
    }

    /// Desktop's rewind sequence: `interrupt`, `wait`, then `revert/stage` at `message`.
    pub fn revert_to(&self, session: &str, message: &str) -> Result<Value, WireError> {
        self.interrupt(session)?;
        self.wait(session)?;
        self.revert_stage(session, message)
    }

    /// Clears a staged revert (`DELETE /api/session/:id/revert`), restoring reverted messages.
    pub fn revert_clear(&self, session: &str) -> Result<(), WireError> {
        self.write("DELETE", &format!("/api/session/{session}/revert"), None).map(|_| ())
    }

    /// Commits a staged revert (`POST /api/session/:id/revert/commit`).
    pub fn revert_commit(&self, session: &str) -> Result<(), WireError> {
        self.write("POST", &format!("/api/session/{session}/revert/commit"), None).map(|_| ())
    }

    /// Desktop's take-back of an unanswered prompt after Stop (`session/revert.ts:293-304`): `wait`,
    /// since the interrupt is acknowledged before the run settles and staging a busy session
    /// fails; `revert/stage` at `message` with `files: false`, since the model called no tool
    /// and any file that changed meanwhile is the user's own edit; then `revert/commit`.
    pub fn take_back(&self, session: &str, message: &str) -> Result<(), WireError> {
        self.wait(session)?;
        let stage = serde_json::json!({ "messageID": message, "files": false });
        self.write("POST", &format!("/api/session/{session}/revert/stage"), Some(stage))?;
        self.revert_commit(session)
    }

    /// Sends `prompt` from a staged revert (`composer/submit.ts`): switches agent, commits the
    /// staged revert, switches model, then sends the prompt or command.
    pub fn send_from_revert(&self, session: &str, prompt: &Prompt) -> Result<Option<String>, WireError> {
        if prompt.delivery != Some(Delivery::Queue)
            && let Some(agent) = &prompt.agent
        {
            self.switch_agent(session, agent)?;
        }
        self.revert_commit(session)?;
        if prompt.delivery != Some(Delivery::Queue)
            && let Some(m) = &prompt.model
        {
            self.switch_model(session, m)?;
        }
        if prompt.command.is_some() {
            return self.write_bytes(&format!("/api/session/{session}/command"), prompt.command_body().to_string().into_bytes(), crate::client::Expect::Command).map(|_| None);
        }
        let prompt = self.with_mentions(None, session, prompt);
        let v = self.write_bytes(&format!("/api/session/{session}/prompt"), prompt.body_bytes(), crate::client::Expect::Prompt(prompt.id.as_deref()))?;
        Ok(v["data"]["id"].as_str().map(str::to_owned))
    }

    /// Forks `session` before `before` (`POST /api/session/:id/fork`).
    pub fn fork_session(&self, session: &str, before: Option<&str>) -> Result<SessionInfo, WireError> {
        let body = match before {
            Some(id) => serde_json::json!({ "before": id }),
            None => serde_json::json!({}),
        };
        let path = format!("/api/session/{session}/fork");
        let v = self.write("POST", &path, Some(body))?;
        let info = model::parse_session(v.get("data").unwrap_or(&v));
        if info.id.is_empty() { Err(WireError::new("POST", &path, WireKind::Shape("no session in the reply".into()))) } else { Ok(info) }
    }
}

