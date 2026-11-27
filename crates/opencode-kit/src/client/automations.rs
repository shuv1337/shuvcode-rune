//! Experimental automation endpoints (`packages/app/src/automations/source.ts`,
//! `packages/app/e2e/utils/mock-api.ts`):
//! `GET /api/experimental/automation`, `POST /api/experimental/automation/:id/{pause,resume}`,
//! `DELETE /api/experimental/automation/:id`, and `GET /api/command?location[directory]=…`.

use super::{Server, encode_component};
use crate::model::{self, AutomationInfo};
use crate::wire_error::{WireError, WireKind};

/// Outcome of calling an experimental automation route that older servers may not implement.
#[derive(Clone, Debug, PartialEq)]
pub enum AutomationEndpoint<T> {
    Supported(T),
    Unsupported,
}

/// Whether `err` indicates the server does not expose `/api/experimental/automation*`.
pub fn is_automation_endpoint_unsupported(err: &WireError) -> bool {
    matches!(err.kind, WireKind::Status { status: 404 | 405 | 501, .. })
}

impl Server {
    /// `GET /api/experimental/automation` (`experimental.automation.list`).
    pub fn list_automations(&self) -> Result<AutomationEndpoint<Vec<AutomationInfo>>, WireError> {
        match self.get("/api/experimental/automation") {
            Ok(v) => Ok(AutomationEndpoint::Supported(model::parse_automations(&v))),
            Err(e) if is_automation_endpoint_unsupported(&e) => Ok(AutomationEndpoint::Unsupported),
            Err(e) => Err(e),
        }
    }

    /// `POST /api/experimental/automation/:id/pause` (`experimental.automation.pause`).
    pub fn pause_automation(&self, id: &str) -> Result<Option<AutomationInfo>, WireError> {
        let path = format!("/api/experimental/automation/{}/pause", encode_component(id));
        self.write("POST", &path, None).map(|v| model::parse_automation(&v))
    }

    /// `POST /api/experimental/automation/:id/resume` (`experimental.automation.resume`).
    pub fn resume_automation(&self, id: &str) -> Result<Option<AutomationInfo>, WireError> {
        let path = format!("/api/experimental/automation/{}/resume", encode_component(id));
        self.write("POST", &path, None).map(|v| model::parse_automation(&v))
    }

    /// `DELETE /api/experimental/automation/:id` (`experimental.automation.delete`, 204).
    pub fn delete_automation(&self, id: &str) -> Result<(), WireError> {
        let path = format!("/api/experimental/automation/{}", encode_component(id));
        self.write("DELETE", &path, None).map(|_| ())
    }

    /// Whether `GET /api/command?location[directory]=…` lists a command named `name`
    /// (used by Automations to check whether `/automation` is enabled in `directory`).
    pub fn has_slash_command(&self, directory: &str, name: &str) -> Result<bool, WireError> {
        let path = format!("/api/command?location%5Bdirectory%5D={}", encode_component(directory));
        let v = self.get(&path)?;
        let items = v.get("data").and_then(serde_json::Value::as_array).or_else(|| v.as_array());
        Ok(items.is_some_and(|arr| {
            arr.iter().any(|c| c.get("name").and_then(serde_json::Value::as_str) == Some(name))
        }))
    }
}
