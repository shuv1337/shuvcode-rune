//! Multi-server Automations polling, /automation capability check, and pause/resume/delete
//! mutations (packages/app/src/automations/source.ts, model.ts, opencode#66393).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use opencode_kit::client::{AutomationEndpoint, Server};
use opencode_kit::model::{AutomationAction, AutomationInfo, AutomationStatus};
use opencode_kit::servers::ServerKey;
use rune::Tx;

use crate::{App, Route};

use super::format::{compare_automations, trigger_label};
use super::{AUTOMATIONS, DeleteModal};

const OPEN_INTERVAL_SECS: f64 = 5.0;
const BG_INTERVAL_SECS: f64 = 30.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Pause,
    Resume,
    Delete,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum FetchPhase {
    #[default]
    Idle,
    Pending,
    Loaded { last_at: f64 },
    Unsupported,
    Failed { last_at: f64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmdOffer {
    Pending,
    Offered,
    Absent,
}

#[derive(Clone, Debug, Default)]
pub struct ServerState {
    pub label: String,
    pub server: Option<Server>,
    pub phase: FetchPhase,
    pub rows: Vec<AutomationInfo>,
    pub commands: HashMap<String, CmdOffer>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum MutatePhase {
    #[default]
    Idle,
    Pending { key: String, op: Operation },
}

pub enum BgResult {
    List { server: ServerKey, res: Result<AutomationEndpoint<Vec<AutomationInfo>>, String> },
    Command { server: ServerKey, directory: String, offered: bool },
    Mutated { server: ServerKey, id: String, op: Operation, res: Result<Option<Box<AutomationInfo>>, String> },
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub key: String,
    pub server: ServerKey,
    pub server_label: String,
    pub project_name: String,
    pub creator: Option<String>,
    pub recipient: Option<String>,
    pub trigger: String,
    pub row: AutomationInfo,
}

pub fn entry_key(server: &ServerKey, id: &str) -> String {
    format!("{}\0{id}", server.as_str())
}

pub fn entries(s: &App) -> Vec<Entry> {
    let st = s.kernel.slots.get(&AUTOMATIONS);
    let active_srv = crate::servers::runtime::active(s);
    let mut out = Vec::new();
    for (srv_key, srv) in &st.servers {
        let on_active = srv_key == active_srv;
        for row in &srv.rows {
            let project_name = if on_active {
                s.projects
                    .iter()
                    .find(|p| p.id == row.project_id && (row.project_id != "global" || p.directory == row.directory))
                    .map(|p| p.name.clone())
            } else {
                None
            }
            .unwrap_or_else(|| {
                if row.project_id.is_empty() || row.project_id == "global" {
                    row.directory
                        .rsplit(['/', '\\'])
                        .find(|seg| !seg.is_empty())
                        .unwrap_or(&row.directory)
                        .to_owned()
                } else {
                    row.project_id.clone()
                }
            });
            let session_name = |sid: &str| -> String {
                if on_active
                    && let Some(info) = s.session(sid)
                    && !info.title.trim().is_empty()
                {
                    return info.title.clone();
                }
                if let Some(tab) = s.tabs.iter().find(|t| !t.is_draft() && t.server == *srv_key && t.id == sid)
                    && !tab.title.trim().is_empty()
                {
                    return tab.title.clone();
                }
                sid.to_owned()
            };
            let creator = row.session_id.as_deref().map(session_name);
            let recipient = match &row.action {
                AutomationAction::Prompt { to: opencode_kit::model::AutomationTarget::Session(sid), .. } => {
                    Some(session_name(sid))
                }
                _ => None,
            };
            out.push(Entry {
                key: entry_key(srv_key, &row.id),
                server: srv_key.clone(),
                server_label: srv.label.clone(),
                project_name,
                creator,
                recipient,
                trigger: trigger_label(row),
                row: row.clone(),
            });
        }
    }
    out.sort_by(|a, b| compare_automations(&a.row, &b.row));
    out
}

pub fn filtered_entries(s: &App, all: &[Entry]) -> Vec<Entry> {
    let needle = s.kernel.slots.get(&AUTOMATIONS).query.text.trim().to_lowercase();
    if needle.is_empty() {
        return all.to_vec();
    }
    all.iter()
        .filter(|e| {
            let mut hay = String::new();
            let mut push = |part: &str| {
                if !part.is_empty() {
                    hay.push(' ');
                    hay.push_str(part);
                }
            };
            push(&e.row.name);
            push(&e.row.id);
            push(&e.trigger);
            push(&e.project_name);
            push(&e.row.directory);
            if let Some(c) = &e.creator { push(c); }
            if let Some(r) = &e.recipient { push(r); }
            push(&e.server_label);
            if let opencode_kit::model::AutomationTrigger::Event { server, .. } = &e.row.trigger {
                push(server);
            }
            match &e.row.action {
                AutomationAction::Prompt { prompt, .. } => push(prompt),
                AutomationAction::Code { code, .. } => push(code),
                AutomationAction::Shell { command } => push(command),
            }
            hay.to_lowercase().contains(&needle)
        })
        .cloned()
        .collect()
}

pub fn selected_entry(s: &App, filtered: &[Entry]) -> Option<Entry> {
    let sel = s.kernel.slots.get(&AUTOMATIONS).selected.as_deref();
    sel.and_then(|k| filtered.iter().find(|e| e.key == k).cloned())
        .or_else(|| filtered.first().cloned())
}

pub fn multi_server(s: &App) -> bool {
    s.kernel.slots.get(&AUTOMATIONS).servers.len() > 1
}

pub fn multi_project(all: &[Entry]) -> bool {
    let set: HashSet<&str> = all.iter().map(|e| e.project_name.as_str()).collect();
    set.len() > 1
}

pub fn is_loading(s: &App) -> bool {
    s.kernel.slots.get(&AUTOMATIONS).servers.values().any(|srv| matches!(srv.phase, FetchPhase::Pending))
}

pub fn is_failed(s: &App) -> bool {
    let st = s.kernel.slots.get(&AUTOMATIONS);
    st.servers.values().any(|srv| {
        matches!(srv.phase, FetchPhase::Failed { .. })
            && srv.commands.values().any(|c| *c == CmdOffer::Offered)
    })
}

pub fn draft_target(s: &App, selected: Option<&Entry>) -> Option<(ServerKey, String)> {
    let st = s.kernel.slots.get(&AUTOMATIONS);
    if let Some(entry) = selected {
        let srv = st.servers.get(&entry.server)?;
        if srv.commands.get(&entry.row.directory) == Some(&CmdOffer::Offered) {
            return Some((entry.server.clone(), entry.row.directory.clone()));
        }
        return None;
    }
    let active = crate::servers::runtime::active(s);
    let pick = |srv: &ServerState| -> Option<String> {
        srv.commands.iter().find(|(_, c)| **c == CmdOffer::Offered).map(|(d, _)| d.clone())
    };
    if let Some(dir) = st.servers.get(active).and_then(pick) {
        return Some((active.clone(), dir));
    }
    st.servers.iter().find_map(|(k, srv)| pick(srv).map(|d| (k.clone(), d)))
}

pub fn mutate(s: &mut App, entry: &Entry, op: Operation, tx: &mut Tx) {
    let st = s.kernel.slots.get(&AUTOMATIONS);
    if !matches!(st.mutate, MutatePhase::Idle) {
        return;
    }
    if st.fixture_mode {
        apply_fixture_mutation(s, &entry.server, &entry.row.id, op);
        tx.instant();
        return;
    }
    let Some(server) = st.servers.get(&entry.server).and_then(|v| v.server.clone()) else { return };
    let (srv_key, id, key) = (entry.server.clone(), entry.row.id.clone(), entry.key.clone());
    let results = {
        let st = s.kernel.slots.get_mut(&AUTOMATIONS);
        st.mutate = MutatePhase::Pending { key, op };
        Arc::clone(&st.results)
    };
    let waker = (*s.waker).clone();
    if rune::task::spawn("automation-mutate", move || {
        let res = match op {
            Operation::Pause => server.pause_automation(&id).map(|o| o.map(Box::new)).map_err(|e| e.to_string()),
            Operation::Resume => server.resume_automation(&id).map(|o| o.map(Box::new)).map_err(|e| e.to_string()),
            Operation::Delete => server.delete_automation(&id).map(|()| None).map_err(|e| e.to_string()),
        };
        if let Ok(mut q) = results.lock() {
            q.push(BgResult::Mutated { server: srv_key, id, op, res });
        }
        if let Some(w) = waker {
            w();
        }
    })
    .is_err()
    {
        s.kernel.slots.get_mut(&AUTOMATIONS).mutate = MutatePhase::Idle;
    }
}

fn apply_fixture_mutation(s: &mut App, server: &ServerKey, id: &str, op: Operation) {
    let now_ms = *s.now_ms;
    let st = s.kernel.slots.get_mut(&AUTOMATIONS);
    let Some(srv) = st.servers.get_mut(server) else { return };
    match op {
        Operation::Pause => {
            if let Some(r) = srv.rows.iter_mut().find(|r| r.id == id) {
                r.status = AutomationStatus::Paused;
                r.time.updated = now_ms;
            }
        }
        Operation::Resume => {
            if let Some(r) = srv.rows.iter_mut().find(|r| r.id == id) {
                r.status = AutomationStatus::Active;
                r.time.updated = now_ms;
            }
        }
        Operation::Delete => {
            srv.rows.retain(|r| r.id != id);
        }
    }
    st.delete_modal = DeleteModal::Closed;
}

pub fn poll(s: &mut App, now: f64, tx: &mut Tx) -> bool {
    let mut changed = false;

    let landed: Vec<BgResult> = {
        let st = s.kernel.slots.untracked(&AUTOMATIONS);
        st.results.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default()
    };
    for msg in landed {
        changed = true;
        match msg {
            BgResult::List { server, res } => {
                let st = s.kernel.slots.get_mut(&AUTOMATIONS);
                if let Some(srv) = st.servers.get_mut(&server) {
                    match res {
                        Ok(AutomationEndpoint::Supported(rows)) => {
                            srv.rows = rows;
                            srv.phase = FetchPhase::Loaded { last_at: now };
                        }
                        Ok(AutomationEndpoint::Unsupported) => {
                            srv.rows.clear();
                            srv.phase = FetchPhase::Unsupported;
                        }
                        Err(_) => {
                            srv.phase = FetchPhase::Failed { last_at: now };
                        }
                    }
                }
            }
            BgResult::Command { server, directory, offered } => {
                let st = s.kernel.slots.get_mut(&AUTOMATIONS);
                if let Some(srv) = st.servers.get_mut(&server) {
                    srv.commands.insert(directory, if offered { CmdOffer::Offered } else { CmdOffer::Absent });
                }
            }
            BgResult::Mutated { server, id, op, res } => {
                {
                    let st = s.kernel.slots.get_mut(&AUTOMATIONS);
                    st.mutate = MutatePhase::Idle;
                }
                match res {
                    Ok(updated) => {
                        let st = s.kernel.slots.get_mut(&AUTOMATIONS);
                        if let Some(srv) = st.servers.get_mut(&server) {
                            match op {
                                Operation::Delete => srv.rows.retain(|r| r.id != id),
                                Operation::Pause | Operation::Resume => {
                                    if let Some(row) = updated {
                                        if let Some(slot) = srv.rows.iter_mut().find(|r| r.id == id) {
                                            *slot = *row;
                                        }
                                    } else if let Some(slot) = srv.rows.iter_mut().find(|r| r.id == id) {
                                        slot.status = if op == Operation::Pause {
                                            AutomationStatus::Paused
                                        } else {
                                            AutomationStatus::Active
                                        };
                                    }
                                }
                            }
                            srv.phase = FetchPhase::Idle;
                        }
                    }
                    Err(err) => {
                        crate::status_toast::fail(s, opencode_i18n::t!("common.requestFailed"), &err);
                    }
                }
            }
        }
    }

    let all = entries(s);
    let filtered = filtered_entries(s, &all);
    let want_sel = selected_entry(s, &filtered).map(|e| e.key);
    if s.kernel.slots.untracked(&AUTOMATIONS).selected != want_sel {
        s.kernel.slots.get_mut(&AUTOMATIONS).selected = want_sel;
        changed = true;
    }

    if s.kernel.slots.untracked(&AUTOMATIONS).fixture_mode {
        return changed;
    }
    if !s.settings.automations() {
        if !s.kernel.slots.untracked(&AUTOMATIONS).servers.is_empty() {
            let st = s.kernel.slots.get_mut(&AUTOMATIONS);
            st.servers.clear();
            changed = true;
        }
        return changed;
    }

    let listed = crate::servers::runtime::listed();
    let open = *s.route == Route::Automations && !s.settings_open();
    let interval = if open { OPEN_INTERVAL_SECS } else { BG_INTERVAL_SECS };
    let active_dir = match &*s.route {
        Route::Session { id, .. } => s.session(id).map(|i| i.directory.clone()),
        Route::Draft(_) => Some(s.draft_dir().to_owned()),
        Route::Home | Route::Automations => None,
    };
    let tab_dirs: Vec<(ServerKey, String)> = s
        .tabs
        .iter()
        .filter(|t| !t.directory.is_empty())
        .map(|t| (t.server.clone(), t.directory.clone()))
        .collect();
    let proj_dirs: Vec<String> = if open {
        s.used_projects().iter().map(|p| p.directory.clone()).collect()
    } else {
        Vec::new()
    };
    let active_srv = crate::servers::runtime::active(s).clone();

    let mut to_fetch: Vec<(ServerKey, Server)> = Vec::new();
    let mut to_check_cmd: Vec<(ServerKey, Server, String)> = Vec::new();

    {
        let st = s.kernel.slots.get_mut(&AUTOMATIONS);
        let live_keys: HashSet<ServerKey> = listed.iter().filter(|l| !l.signed_out).map(|l| l.key.clone()).collect();
        st.servers.retain(|k, _| live_keys.contains(k));

        for item in &listed {
            if item.signed_out {
                continue;
            }
            let srv = st.servers.entry(item.key.clone()).or_default();
            srv.label = item.name.clone();
            srv.server = Some(item.server.clone());

            let offered = srv.commands.values().any(|c| *c == CmdOffer::Offered);
            let due = match srv.phase {
                FetchPhase::Idle => true,
                FetchPhase::Loaded { last_at } if (!srv.rows.is_empty() || offered) && now - last_at >= interval => true,
                FetchPhase::Failed { last_at } if offered && now - last_at >= interval => true,
                _ => false,
            };
            if due {
                srv.phase = FetchPhase::Pending;
                to_fetch.push((item.key.clone(), item.server.clone()));
            }

            let mut dirs = Vec::new();
            if item.key == active_srv {
                if let Some(d) = &active_dir {
                    dirs.push(d.clone());
                }
                dirs.extend(proj_dirs.iter().cloned());
            }
            if open {
                for (tk, td) in &tab_dirs {
                    if *tk == item.key {
                        dirs.push(td.clone());
                    }
                }
                for row in &srv.rows {
                    if !row.directory.is_empty() {
                        dirs.push(row.directory.clone());
                    }
                }
            }
            for d in dirs {
                if !d.is_empty() && !srv.commands.contains_key(&d) {
                    srv.commands.insert(d.clone(), CmdOffer::Pending);
                    to_check_cmd.push((item.key.clone(), item.server.clone(), d));
                }
            }
        }
    }

    let results = Arc::clone(&s.kernel.slots.untracked(&AUTOMATIONS).results);
    let waker = (*s.waker).clone();

    for (key, server) in to_fetch {
        let (results, waker, key_back) = (Arc::clone(&results), waker.clone(), key.clone());
        if rune::task::spawn("automations-list", move || {
            let res = server.list_automations().map_err(|e| e.to_string());
            if let Ok(mut q) = results.lock() {
                q.push(BgResult::List { server: key, res });
            }
            if let Some(w) = waker {
                w();
            }
        })
        .is_err()
        && let Some(srv) = s.kernel.slots.get_mut(&AUTOMATIONS).servers.get_mut(&key_back)
        {
            srv.phase = FetchPhase::Idle;
        }
    }

    for (key, server, directory) in to_check_cmd {
        let (results, waker, key_back, dir_back) = (Arc::clone(&results), waker.clone(), key.clone(), directory.clone());
        if rune::task::spawn("automations-cmd", move || {
            let offered = server.has_slash_command(&directory, "automation").unwrap_or(false);
            if let Ok(mut q) = results.lock() {
                q.push(BgResult::Command { server: key, directory, offered });
            }
            if let Some(w) = waker {
                w();
            }
        })
        .is_err()
        && let Some(srv) = s.kernel.slots.get_mut(&AUTOMATIONS).servers.get_mut(&key_back)
        {
            srv.commands.remove(&dir_back);
        }
    }

    if !s.kernel.slots.untracked(&AUTOMATIONS).servers.is_empty() {
        tx.wake_after(std::time::Duration::from_secs_f64(interval));
    }

    changed
}

pub fn seed_fixture(s: &mut App, server_label: &str, rows: Vec<AutomationInfo>, offered_dirs: &[&str]) {
    let key = crate::servers::runtime::active(s).clone();
    let mut commands = HashMap::new();
    for d in offered_dirs {
        commands.insert((*d).to_owned(), CmdOffer::Offered);
    }
    let mut servers = BTreeMap::new();
    servers.insert(
        key,
        ServerState {
            label: server_label.to_owned(),
            server: None,
            phase: FetchPhase::Loaded { last_at: *s.clock },
            rows,
            commands,
        },
    );
    let st = s.kernel.slots.get_mut(&AUTOMATIONS);
    st.fixture_mode = true;
    st.servers = servers;
    st.query = Default::default();
    st.selected = None;
    st.show_done = false;
    st.mutate = MutatePhase::Idle;
    st.delete_modal = DeleteModal::Closed;
}
