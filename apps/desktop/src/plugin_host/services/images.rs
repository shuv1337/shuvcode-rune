//! `Images` and `ui.image`: images a plugin loads from a source or hands over, held by the
//! window's store (`plugin_host::images`), which reads and decodes them off the UI thread and
//! steps each one's load machine in the host's poll. A render only reads where one stands
//! (`Dep::Image`), so a surface at rest enters no VM; as an image lands or fails, the surfaces
//! that drew it run again.

use std::rc::Rc;

use mlua::{Lua, Table, Value};
use rune::{El, SurfaceFit, Transition, el};
use rune_luau::NodeArgs;

use super::{method, record, with_app};
use crate::App;
use crate::plugin_host::images::load::{self, Load, Source};
use crate::plugin_host::cx::{self, Dep};
use crate::plugin_host::sources::Origin;
use crate::plugin_host::vm::Vmx;

pub fn build(lua: &Lua, vmx: &Rc<Vmx>, t: &Table) -> mlua::Result<()> {
    let plugin = vmx.half.id.clone();
    method(lua, t, "Load", move |_, src: String| with_app("Images:Load", |app| Ok(load_src(app, &plugin, &src))))?;
    let plugin = vmx.half.id.clone();
    method(lua, t, "State", move |lua, id: u32| {
        cx::read(Dep::Image(plugin.clone(), id));
        with_app("Images:State", |app| {
            let images = app.plugins.shared.images.borrow();
            let load = images.get(&plugin, id);
            record(lua, 4, |o| match load {
                Some(Load::Loaded { size: [w, h], .. }) => {
                    o.str(c"status", "loaded");
                    o.num(c"width", f64::from(*w));
                    o.num(c"height", f64::from(*h));
                }
                Some(Load::Failed(why)) => {
                    o.str(c"status", "failed");
                    o.str(c"error", why);
                }
                Some(other) => o.str(c"status", other.status()),
                None => {
                    o.str(c"status", "failed");
                    o.str(c"error", "no such image");
                }
            })
        })
    })?;
    let plugin = vmx.half.id.clone();
    method(lua, t, "VideoFailed", move |_, src: String| {
        cx::read(Dep::Video(plugin.clone(), src.clone()));
        with_app("Images:VideoFailed", |app| Ok(app.plugins.shared.images.borrow().video_failed(&plugin, &src)))
    })?;
    let plugin = vmx.half.id.clone();
    method(lua, t, "FromEncoded", move |_, data: Value| {
        let bytes = bytes_of(&data).ok_or_else(|| mlua::Error::runtime("Images:FromEncoded takes a buffer or a string"))?;
        with_app("Images:FromEncoded", |app| Ok(handed(app.plugins.shared.images.borrow_mut().add(&plugin, &bytes, 0, 0))))
    })?;
    let plugin = vmx.half.id.clone();
    method(lua, t, "FromSvg", move |_, svg: String| with_app("Images:FromSvg", |app| Ok(handed(app.plugins.shared.images.borrow_mut().add_svg(&plugin, &svg)))))?;
    let plugin = vmx.half.id.clone();
    method(lua, t, "FromRgba", move |_, (pixels, width, height): (Value, u32, u32)| {
        let bytes = bytes_of(&pixels).ok_or_else(|| mlua::Error::runtime("Images:FromRgba takes a buffer"))?;
        if width == 0 || height == 0 {
            return Ok(None);
        }
        with_app("Images:FromRgba", |app| Ok(handed(app.plugins.shared.images.borrow_mut().add(&plugin, &bytes, width, height))))
    })?;
    let plugin = vmx.half.id.clone();
    method(lua, t, "Release", move |_, id: u32| {
        with_app("Images:Release", |app| {
            app.plugins.shared.images.borrow_mut().remove(&plugin, id);
            Ok(())
        })
    })
}

/// An id the store handed back, nil when it refused (0).
fn handed(id: u32) -> Option<u32> {
    (id != 0).then_some(id)
}

fn bytes_of(v: &Value) -> Option<Vec<u8>> {
    match v {
        Value::String(s) => Some(s.as_bytes().to_vec()),
        Value::Buffer(b) => Some(b.to_vec()),
        _ => None,
    }
}

/// Plugin `plugin`'s image from `src` (`Images:Load`): the web, the window's server, a data URL
/// or a file of the plugin's folder.
pub fn load_src(app: &App, plugin: &str, src: &str) -> u32 {
    let src = src.trim();
    let lower = src.get(..8).unwrap_or(src).to_ascii_lowercase();
    let (key, source): (String, Box<dyn FnOnce() -> Option<Source>>) = if lower.starts_with("https://") || lower.starts_with("http://") {
        let url = src.to_owned();
        (format!("web:{src}"), Box::new(move || Some(Source::Web(url))))
    } else if lower.starts_with("data:") {
        use sha2::{Digest, Sha256};
        let url = src.to_owned();
        let digest = Sha256::digest(src.as_bytes());
        (format!("data:{digest:x}"), Box::new(move || Some(Source::Data(url))))
    } else if src.starts_with('/') && !src.starts_with("//") {
        let server = match &*app.source {
            crate::Source::Live { server, .. } => Some(server.clone()),
            _ => None,
        };
        let path = src.to_owned();
        (format!("server:{}:{src}", app.server_key().unwrap_or_default()), Box::new(move || Some(Source::Server { server: server?, path })))
    } else {
        let origin = app.plugins.list.iter().find(|p| *p.half.id == *plugin).map(|p| p.origin.clone());
        let path = src.replace('\\', "/");
        (format!("file:{path}"), Box::new(move || plugin_file(origin?, &path)))
    };
    app.plugins.shared.images.borrow_mut().load(plugin, key, source)
}

/// File `path` of a plugin's folder: relative, and inside it.
fn plugin_file(origin: Origin, path: &str) -> Option<Source> {
    let inside = !path.is_empty() && !path.starts_with('/') && !path.contains(':') && path.split('/').all(|part| !part.is_empty() && part != "..");
    if !inside {
        return None;
    }
    match origin {
        Origin::Dir { dir, .. } => Some(Source::File(dir.join(path))),
        Origin::Embedded(files, _) => files.iter().find(|(name, _)| name.replace('\\', "/") == path).map(|(_, text)| Source::Embedded(text)),
    }
}

/// `ui.video(props)`: the video playing muted on a loop while it shows. As a page's `<video>`,
/// its box shows what's behind it until the first frame, and stays that way if it can't play.
pub fn video_node(a: &mut NodeArgs) -> Result<El, String> {
    cx::with_app(|app| {
        let plugin = cx::rendering().ok_or("ui.video draws only while a plugin renders")?;
        let src = a.string("src").filter(|s| s.starts_with("https://") || s.starts_with("http://")).ok_or("give it an http or https src")?.to_owned();
        let fit = if a.string("fit") == Some("cover") { SurfaceFit::Fill } else { SurfaceFit::Fit };
        let radius = a.number("radius").unwrap_or(0.0) as f32;
        cx::read(Dep::Video(plugin.clone(), src.clone()));
        let player = app.plugins.shared.images.borrow_mut().player(&plugin, &src);
        let failed = matches!(player.state(), rune::PlayerState::Failed(_));
        let mut out = if failed { el().role("img") } else { el().role("img").video(&player, fit) };
        if let Some(label) = a.string("label") {
            out = out.label(label.to_owned());
        }
        if radius > 0.0 {
            out = out.radius(radius).clip_rounded(radius);
        }
        Ok(out)
    })
    .ok_or_else(|| "ui.video draws only in a window".to_owned())?
}

/// `ui.image(props, fallback?)`: the image fitted in its box over its placeholder, the fallback
/// in its place once it failed, and the image viewer on a press with `preview`.
pub fn node(a: &mut NodeArgs) -> Result<El, String> {
    cx::with_app(|app| {
        let plugin = cx::rendering().ok_or("ui.image draws only while a plugin renders")?;
        let id = match (a.number("image"), a.string("src").map(str::to_owned)) {
            (Some(id), _) => id as u32,
            (None, Some(src)) => load_src(app, &plugin, &src),
            (None, None) => return Err("give it an image or a src".into()),
        };
        cx::read(Dep::Image(plugin.clone(), id));
        let images = app.plugins.shared.images.borrow();
        let state = images.get(&plugin, id);
        if matches!(state, None | Some(Load::Failed(_))) && !a.children.is_empty() {
            let mut fallback = std::mem::take(&mut a.children);
            return Ok(if fallback.len() == 1 { fallback.remove(0) } else { el().children(fallback) });
        }
        let fit = a.string("fit").map(str::to_owned);
        let draw = |pic: El, pixels: &rune::Pixels| match fit.as_deref() {
            Some("cover") => pic.pixels_fit(pixels, SurfaceFit::Fill),
            Some("fill") => pic.pixels(pixels),
            _ => pic.pixels_fit(pixels, SurfaceFit::Fit),
        };
        let radius = a.number("radius").unwrap_or(0.0) as f32;
        let placeholder = a.color("placeholder");
        let label = a.string("label").map(str::to_owned);
        let loaded = matches!(state, Some(Load::Loaded { .. }));
        let mut out = el().role("img");
        if let Some(label) = &label {
            out = out.label(label.clone());
        }
        if radius > 0.0 {
            out = out.radius(radius).clip_rounded(radius);
        }
        if let Some(c) = placeholder {
            out = out.fill(c);
        }
        let image = state.and_then(Load::image);
        out = if placeholder.is_some() {
            // Transparent pixels still show the placeholder underneath. Decode completion is not input.
            match image {
                Some(image) => out.child(draw(el().key("image").w_full().h_full().transition(Transition::none()), image.pixels())),
                None => out,
            }
        } else {
            match image {
                Some(image) => draw(out, image.pixels()),
                None => out,
            }
        };
        if !a.flag("preview") {
            return Ok(out);
        }
        let Some((image, bytes)) = state.and_then(crate::plugin_host::images::shown).filter(|_| loaded) else { return Ok(out) };
        let file = opencode_kit::model::Attachment { mime: load::mime(&bytes).into(), name: label.unwrap_or_else(|| "image".into()), bytes, media: None };
        Ok(session_view::images::opens::<App>(app, out, el(), session_view::images::Strip::new(format!("plugin-image-{plugin}-{id}"), std::sync::Arc::from([file]), 0), image, None))
    })
    .ok_or_else(|| "ui.image draws only in a window".to_owned())?
}
