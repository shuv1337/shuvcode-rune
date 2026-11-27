//! Requests off the UI thread: `Server:RequestAsync`, `Http:FetchAsync`, and Workspace's files
//! and downloads. Each yields its task and resumes it with the answer.

use std::rc::Rc;
use std::time::Duration;

use mlua::{FromLua, Lua, MultiValue, Table, Value};
use opencode_plugin_spec as spec;
use rune_luau::{Completer, Data, Host};

use super::{async_method, json_to_data, map, method};
use crate::App;
use crate::plugin_host::cx;
use crate::plugin_host::plugin::Half;
use crate::plugin_host::vm::Vmx;

pub fn build(lua: &Lua, vmx: &Rc<Vmx>, name: &str, t: &Table) -> mlua::Result<()> {
    match name {
        "Http" => async_method(lua, vmx, t, "FetchAsync", "Http.FetchAsync"),
        _ => {
            async_method(lua, vmx, t, "ReadFileAsync", "Workspace.ReadFileAsync")?;
            async_method(lua, vmx, t, "SaveDownloadAsync", "Workspace.SaveDownloadAsync")?;
            // The apps that open a path, where the server the window works against shares this
            // machine's files (`session.server.local`); none on another machine's.
            method(lua, t, "OpenInApps", |lua, ()| {
                super::with_app("Workspace:OpenInApps", |app| {
                    let apps: Vec<Data> = crate::open_in::apps_for_plugin(app).into_iter().map(|(id, name)| map([("id", Data::String(id.to_owned())), ("name", Data::String(name.to_owned()))])).collect();
                    mlua::IntoLua::into_lua(Data::List(apps), lua)
                })
            })?;
            let q = vmx.half.id.clone();
            method(lua, t, "OpenIn", move |_, (app, path): (String, String)| {
                cx::ask(&q, cx::Request::OpenIn { app, path });
                Ok(())
            })?;
            method(lua, t, "Relative", |_, (path, directory): (String, String)| {
                let Some(rel) = crate::tool_open::relative(&path, &directory) else {
                    return Ok(None);
                };
                // On local live servers, require OS-level `verify_within_root` as final authority
                // (blocking symlink/junction escapes, non-fixed drives, and path differentials).
                let verified = super::with_app("Workspace:Relative", |app| {
                    #[cfg(not(target_arch = "wasm32"))]
                    if app.can_reveal_outside() && app.live() {
                        let abs = opencode_kit::path::absolute(&directory, &rel);
                        return Ok(browser_policy::verify_within_root(&abs, &directory, false).is_some());
                    }
                    let _ = app;
                    Ok(true)
                })?;
                Ok(verified.then_some(rel))
            })?;
            super::lazy_signals(lua, vmx, t, "Workspace", &["FileChanged"])
        }
    }
}

pub fn asyncs(host: &Host<'_>, t: &Table, half: &Rc<Half>) -> mlua::Result<()> {
    let app_half = half.kind == crate::plugin_host::plugin::Kind::App;
    t.raw_set("Server.RequestAsync", host.async_function("Server:RequestAsync", move |lua, a, done| request(lua, a, done, app_half))?)?;
    t.raw_set("Http.FetchAsync", host.async_function("Http:FetchAsync", fetch)?)?;
    t.raw_set("Workspace.ReadFileAsync", host.async_function("Workspace:ReadFileAsync", read_file)?)?;
    t.raw_set("Workspace.SaveDownloadAsync", host.async_function("Workspace:SaveDownloadAsync", save_download)?)?;
    Ok(())
}

fn args<T: mlua::FromLuaMulti>(lua: &Lua, args: MultiValue) -> mlua::Result<T> {
    T::from_lua_multi(args, lua)
}

/// `Server:RequestAsync(method, path, body?)`: on the server the window works with now, the
/// routed tab's (`local:`, and every request of the app half, asks the app's own); resumes with a
/// `ServerResponse`.
fn request(lua: &Lua, a: MultiValue, done: Completer, app_half: bool) -> mlua::Result<()> {
    let (method, path, body): (String, String, Value) = args(lua, a)?;
    let path = if app_half && !path.starts_with("local:") { format!("local:{path}") } else { path };
    let body = match body {
        Value::Nil => None,
        other => Some(Data::from_lua(other, lua)?.to_json()),
    };
    cx::with_app(|app| {
        // A window without a server (a fixture) runs no jobs: answer at once rather than never. A
        // `local:` request goes to the app's own server, and without one it would wait on those jobs.
        let local = path.starts_with("local:");
        if !app.live() && (!local || crate::servers::runtime::own_server().is_none()) {
            let error = if local { "This app has no local server" } else { "This window has no server" };
            done.complete(map([("ok", Data::Bool(false)), ("status", Data::Integer(0)), ("text", Data::String(String::new())), ("error", Data::String(error.into()))]));
            return;
        }
        let send = move |srv: &opencode_kit::client::Server, path: &str| {
            let answer = if method.eq_ignore_ascii_case("GET") {
                srv.get_text_ready(path).map(|text| (serde_json::from_str(&text).ok(), text)).map_err(|e| e.to_string())
            } else {
                let method = match method.to_ascii_uppercase().as_str() {
                    "PATCH" => "PATCH",
                    "PUT" => "PUT",
                    "DELETE" => "DELETE",
                    _ => "POST",
                };
                srv.call_within(method, path, body, Duration::from_secs(600)).map(|v| {
                    let text = v.to_string();
                    (Some(v), text)
                })
            };
            done.complete(match answer {
                Ok((json, text)) => map([("ok", Data::Bool(true)), ("status", Data::Integer(200)), ("json", json.as_ref().map_or(Data::Nil, json_to_data)), ("text", Data::String(text))]),
                Err(e) => map([("ok", Data::Bool(false)), ("status", Data::Integer(0)), ("text", Data::String(String::new())), ("error", Data::String(e))]),
            });
        };
        match (path.strip_prefix("local:"), crate::servers::runtime::own_server()) {
            (Some(local), Some(own)) => {
                let local = local.to_owned();
                crate::plugin_host::tasks::run(move || send(&own, &local));
            }
            (local, _) => {
                let path = local.map_or(path.clone(), str::to_owned);
                app.job(move |srv| {
                    send(srv, &path);
                    Box::new(|_: &mut App, _: &mut rune::Tx| {}) as crate::jobs::Apply
                });
            }
        }
    })
    .ok_or_else(|| mlua::Error::runtime("Server:RequestAsync needs a window"))?;
    Ok(())
}

/// `Http:FetchAsync(request)`: any URL, off the UI thread; resumes with an `HttpResponse`.
fn fetch(lua: &Lua, a: MultiValue, done: Completer) -> mlua::Result<()> {
    let (req,): (Value,) = args(lua, a)?;
    let req: spec::HttpRequest = Data::from_lua(req, lua)?.decode().map_err(|e| mlua::Error::runtime(format!("HttpRequest: {e}")))?;
    crate::plugin_host::tasks::run(move || done.complete(send(&req)));
    Ok(())
}

fn send(r: &spec::HttpRequest) -> Data {
    let timeout = Duration::from_secs_f64(r.timeout.unwrap_or(30.0).max(0.1));
    let agent: ureq::Agent = net_trust::agent_config().timeout_global(Some(timeout)).http_status_as_error(false).build().into();
    let method = r.method.map_or("GET", |m| m.as_str());
    let mut req = ureq::http::Request::builder().method(method).uri(r.url.as_str());
    for (k, v) in r.headers.iter().flatten() {
        req = req.header(k.as_str(), v.as_str());
    }
    if let Some(auth) = &r.basic_auth {
        use base64::Engine as _;
        let token = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", auth.user, auth.password));
        req = req.header("authorization", format!("Basic {token}"));
    }
    let body = match (&r.json, &r.body) {
        (Some(json), _) => {
            req = req.header("content-type", "application/json");
            Some(json.to_string())
        }
        (None, Some(body)) => Some(body.clone()),
        _ => None,
    };
    let sent = match body {
        Some(body) => req.body(body).map(|req| agent.run(req)),
        None => req.body(()).map(|req| agent.run(req)),
    };
    let failed = |e: String| map([("ok", Data::Bool(false)), ("status", Data::Integer(0)), ("headers", Data::Map(Default::default())), ("text", Data::String(String::new())), ("error", Data::String(e))]);
    let mut resp = match sent {
        Ok(Ok(resp)) => resp,
        Ok(Err(e)) => return failed(e.to_string()),
        Err(e) => return failed(e.to_string()),
    };
    let status = resp.status().as_u16();
    let headers = resp.headers().iter().map(|(k, v)| (k.as_str().to_owned(), Data::String(v.to_str().unwrap_or_default().to_owned()))).collect();
    match resp.body_mut().with_config().limit(16 << 20).read_to_string() {
        Ok(text) => {
            let json = serde_json::from_str::<serde_json::Value>(&text).ok();
            map([
                ("ok", Data::Bool((200..300).contains(&status))),
                ("status", Data::Integer(i64::from(status))),
                ("headers", Data::Map(headers)),
                ("json", json.as_ref().map_or(Data::Nil, json_to_data)),
                ("text", Data::String(text)),
            ])
        }
        Err(e) => failed(e.to_string()),
    }
}

fn read_file(lua: &Lua, a: MultiValue, done: Completer) -> mlua::Result<()> {
    let (path,): (String,) = args(lua, a)?;
    crate::plugin_host::tasks::run(move || {
        done.complete(match std::fs::read_to_string(&path) {
            Ok(text) => map([("text", Data::String(text))]),
            Err(e) => map([("error", Data::String(format!("{path}: {e}")))]),
        });
    });
    Ok(())
}

fn save_download(lua: &Lua, a: MultiValue, done: Completer) -> mlua::Result<()> {
    let (name, content): (String, String) = args(lua, a)?;
    let persists = cx::with_app(App::persists).unwrap_or(false);
    let saved = if persists { opencode_kit::client::save_download(&name, &content) } else { Err("This window doesn't save files".to_owned()) };
    done.complete(match saved {
        Ok(path) => map([("path", Data::String(path))]),
        Err(e) => map([("error", Data::String(e))]),
    });
    Ok(())
}
