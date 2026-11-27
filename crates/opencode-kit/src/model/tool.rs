//! What a tool call keeps in its `metadata` besides the server's own: its error message, when it
//! began running, and its text result, kept across the server's later metadata updates.

use serde_json::Value;

use super::ToolStatus;

/// Where a failed tool call's error message is kept in its `metadata`.
const TOOL_ERROR: &str = "opencode-kit.error";

/// Where a tool call's start, in Unix milliseconds, is kept in its `metadata`.
const TOOL_RAN: &str = "opencode-kit.ran";

/// Where a finished tool call's text result is kept in its `metadata`.
const TOOL_OUTPUT: &str = "opencode-kit.output";

/// A failed tool call's error message, from its `metadata`.
pub fn tool_error(metadata: &Value) -> Option<&str> {
    metadata[TOOL_ERROR].as_str()
}

/// The `metadata` of a call that failed with `message`, for a failed call made up to show an
/// error card.
pub fn tool_error_metadata(message: &str) -> Value {
    serde_json::json!({ TOOL_ERROR: message })
}

/// Keeps `error`'s message (an `{ message }` object or a string) in `metadata`.
pub(super) fn keep_tool_error(metadata: &mut Value, error: &Value) {
    let Some(message) = error["message"].as_str().or(error.as_str()) else { return };
    object(metadata)[TOOL_ERROR] = message.into();
}

/// When a tool call began running (`time.ran`, else `time.created`), once a read has said.
pub fn tool_ran(metadata: &Value) -> Option<i64> {
    metadata[TOOL_RAN].as_i64()
}

pub(super) fn keep_tool_ran(metadata: &mut Value, time: &Value) {
    let Some(ran) = time["ran"].as_i64().or(time["created"].as_i64()) else { return };
    object(metadata)[TOOL_RAN] = ran.into();
}

/// A tool call's text result: what a running call has streamed so far (`metadata.output`), or
/// the text items of what a finished one returned, else what it streamed before it ended.
pub fn tool_output<'a>(status: &ToolStatus, metadata: &'a Value) -> Option<&'a str> {
    match status {
        ToolStatus::Running => metadata["output"].as_str(),
        _ => metadata[TOOL_OUTPUT].as_str().or_else(|| metadata["output"].as_str()),
    }
}

/// Keeps the text items of a tool result's `content` in `metadata`, dropping any duplicate
/// streamed `metadata["output"]` now superseded by the finished result (`tool_output` reads
/// `TOOL_OUTPUT` first for completed calls).
pub(super) fn keep_tool_output(metadata: &mut Value, content: &Value) {
    let texts: Vec<&str> = content.as_array().into_iter().flatten().filter(|c| c["type"] == "text").filter_map(|c| c["text"].as_str()).collect();
    if texts.is_empty() {
        return;
    }
    let m = object(metadata);
    m[TOOL_OUTPUT] = texts.join("\n").into();
    if let Some(obj) = m.as_object_mut() {
        obj.remove("output");
    }
}

/// Where a composer shell command's call is marked in its `metadata` (see [`shell_message`]).
const SHELL_MESSAGE: &str = "opencode-kit.shellMessage";

/// Whether a `shell` call is a shell command run from the composer (a `shell` message), which
/// Desktop draws as the call with an error card under it when the command didn't succeed. Its
/// `metadata` then holds the message's `status` (`running`, `exited`, `timeout`, `killed`) and
/// `exit`, as Desktop hands them to its shell renderer.
pub fn shell_message(metadata: &Value) -> bool {
    metadata[SHELL_MESSAGE] == true
}

/// A `shell` message (`SessionMessageShell`) as the one-call step Desktop's
/// `SessionShellMessage` renders: the `shell` call of its command, running until the command
/// ends, with its output and how it ended in the call's `metadata`.
pub(super) fn shell_step(id: &str, m: &Value) -> super::Assistant {
    let status = m["status"].as_str().unwrap_or("running");
    let mut metadata = serde_json::json!({
        "shellID": m["shellID"],
        "status": status,
        "exit": m["exit"],
        "truncated": m["output"]["truncated"],
        "output": m["output"]["output"].as_str().unwrap_or(""),
        SHELL_MESSAGE: true,
    });
    // `currentToolFailed` reads a timed-out shell from `timeout`.
    if status == "timeout" {
        metadata["timeout"] = true.into();
    }
    let input = serde_json::json!({ "command": m["command"].as_str().unwrap_or("") });
    super::Assistant {
        id: id.to_owned(),
        completed: m["time"]["completed"].as_i64(),
        created: m["time"]["created"].as_i64().unwrap_or(0),
        parts: vec![super::Part::Tool {
            id: id.to_owned(),
            name: "shell".into(),
            status: if status == "running" { ToolStatus::Running } else { ToolStatus::Completed },
            title: super::tool_title("shell", &input),
            input,
            files: Vec::new(),
            metadata,
        }],
        ..Default::default()
    }
}

/// Swaps in the server's newer `metadata`, keeping what this crate keeps there: when the call
/// began running, and its result and error, which a late progress event mustn't wipe.
pub(super) fn replace_tool_metadata(metadata: &mut Value, new: &Value) {
    let kept: Vec<(&str, Value)> = [TOOL_RAN, TOOL_OUTPUT, TOOL_ERROR].into_iter().map(|k| (k, metadata[k].clone())).filter(|(_, v)| !v.is_null()).collect();
    *metadata = new.clone();
    if kept.is_empty() {
        return;
    }
    let m = object(metadata);
    for (k, v) in kept {
        m[k] = v;
    }
}

/// `metadata` as an object, made one when it isn't.
fn object(metadata: &mut Value) -> &mut Value {
    if !metadata.is_object() {
        *metadata = Value::Object(Default::default());
    }
    metadata
}
