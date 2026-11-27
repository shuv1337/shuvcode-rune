//! Where the preferences are kept: what Desktop has too in Desktop's `settings.v3` shape
//! (`packages/app/src/settings/model.tsx`, the `opencode.global.dat` namespace) and the language
//! in Desktop's `language` (`runtime/i18n/language.tsx`), so a profile seeded from Desktop, and
//! Desktop's plugins reading `settings.v3`, see the same values; the rest, Rune's own, under
//! `rune.settings`. `Settings::{to_json, from_json}` stay the one flat form in memory; this
//! translates it. Fields of `settings.v3` the app doesn't know (`general.releaseNotes`,
//! `sessionSummary`, …) are kept as they are.

use opencode_platform::storage;
use serde_json::{Map, Value, json};

use crate::Settings;
use crate::types::{CODE_FONTS, UI_FONTS};

pub(crate) const GLOBAL: &str = "opencode.global.dat";
const V3: &str = "settings.v3";
const RUNE: &str = "rune.settings";
const LANGUAGE: &str = "language";

/// The flat keys kept in `settings.v3`, and where: (flat key, v3 section, v3 key).
const SHARED: [(&str, &str, &str); 29] = [
    ("showAgentPicker", "general", "showCustomAgents"),
    ("tabGroups", "general", "tabGroups"),
    ("automations", "general", "automations"),
    ("foldUserMessages", "general", "foldUserMessages"),
    ("newQuestionUI", "general", "newQuestionUI"),
    ("sharedSidePanel", "general", "sharedSidePanel"),
    ("sidebarOrganize", "general", "sidebarOrganize"),
    ("mobileTitlebarPosition", "general", "mobileTitlebarPosition"),
    ("mobileDiffWrap", "general", "mobileDiffWrap"),
    ("followUpBehavior", "general", "followUpBehavior"),
    ("questionSubmitKey", "general", "questionSubmitKey"),
    ("timelineDetail", "general", "timelineDetail"),
    ("timelineWidth", "general", "timelineWidth"),
    ("backgroundActivity", "general", "backgroundActivity"),
    ("showTurnDiffs", "general", "showTurnDiffs"),
    ("wrapCodeBlocks", "general", "wrapCodeBlocks"),
    ("tabLayout", "appearance", "tabLayout"),
    ("navigation", "appearance", "navigation"),
    ("tabGrouping", "appearance", "tabGrouping"),
    ("reduceMotion", "appearance", "reduceMotion"),
    ("uiFont", "appearance", "sans"),
    ("codeFont", "appearance", "mono"),
    ("terminalFontName", "appearance", "terminal"),
    ("keybinds", "", "keybinds"),
    ("workspaces", "", "workspaces"),
    ("notifications", "", "notifications"),
    ("sounds", "", "sounds"),
    ("permissions.autoApprove", "permissions", "autoApprove"),
    ("permissions.projects", "permissions", "autoApproveProjects"),
];

/// The preferences, from memory (the boot packet before the first frame). A profile from before
/// the split keeps everything under `rune.settings`: it's split on this read, once.
pub fn load() -> Settings {
    let rune = storage::read_json(GLOBAL, RUNE).filter(Value::is_object);
    let v3 = storage::read_json(GLOBAL, V3).filter(Value::is_object);
    let language = storage::read_json(GLOBAL, LANGUAGE);
    let before_split = rune.as_ref().is_some_and(|r| r.get("showAgentPicker").is_some());
    let flat = match (before_split, rune, v3) {
        (true, Some(rune), _) => rune,
        (_, rune, v3) => flat(rune.unwrap_or_else(|| json!({})), v3.as_ref(), language.as_ref()),
    };
    let settings = Settings::from_json(&flat);
    if before_split {
        save(&settings);
    }
    settings
}

/// Written behind; an unchanged set writes nothing.
pub fn save(settings: &Settings) {
    let all = settings.to_json();
    let mut rune = all.as_object().cloned().unwrap_or_default();
    storage::update_json(GLOBAL, V3, |v3| {
        for (flat, section, key) in SHARED {
            let value = take(&mut rune, flat);
            let Some(value) = value.map(|v| shared_value(flat, v)) else { continue };
            let target = if section.is_empty() { &mut *v3 } else { object(v3, section) };
            target.insert(key.to_owned(), value);
        }
        // Desktop's required fields that the app doesn't set: its defaults.
        object(v3, "appearance").entry("fontSize").or_insert(json!(14));
    });
    if let Some(Value::Object(p)) = rune.get("permissions")
        && p.is_empty()
    {
        rune.remove("permissions");
    }
    match rune.remove("language") {
        Some(Value::String(locale)) => storage::write_json(GLOBAL, LANGUAGE, &json!({ "locale": locale })),
        _ => storage::delete(GLOBAL, LANGUAGE),
    }
    storage::write_json(GLOBAL, RUNE, &Value::Object(rune));
}

/// The flat form from the split rows.
fn flat(mut rune: Value, v3: Option<&Value>, language: Option<&Value>) -> Value {
    let Some(map) = rune.as_object_mut() else { return rune };
    if let Some(v3) = v3 {
        for (flat, section, key) in SHARED {
            let from = if section.is_empty() { v3.get(key) } else { v3.get(section).and_then(|s| s.get(key)) };
            let Some(value) = from.filter(|v| !v.is_null()) else { continue };
            match flat.split_once('.') {
                Some((outer, inner)) => {
                    object(map, outer).insert(inner.to_owned(), value.clone());
                }
                // Desktop's "" is its default font: the flat form's default is no name.
                None if matches!(flat, "uiFont" | "codeFont") && value.as_str() == Some("") => {}
                None => {
                    map.insert(flat.to_owned(), value.clone());
                }
            }
        }
    }
    if let Some(locale) = language.and_then(|l| l["locale"].as_str()) {
        map.insert("language".to_owned(), json!(locale));
    }
    rune
}

/// A flat key's value, taken out of `rune` (`a.b` reaches into `a`).
fn take(rune: &mut Map<String, Value>, flat: &str) -> Option<Value> {
    match flat.split_once('.') {
        Some((outer, inner)) => rune.get_mut(outer)?.as_object_mut()?.remove(inner),
        None => rune.remove(flat),
    }
}

/// The value as Desktop keeps it: its default fonts are "".
fn shared_value(flat: &str, value: Value) -> Value {
    let default = match flat {
        "uiFont" => UI_FONTS[0].name,
        "codeFont" => CODE_FONTS[0].name,
        _ => return value,
    };
    if value.as_str() == Some(default) { json!("") } else { value }
}

/// `map[key]` as an object, made one when it isn't.
fn object<'a>(map: &'a mut Map<String, Value>, key: &str) -> &'a mut Map<String, Value> {
    let slot = map.entry(key.to_owned()).or_insert_with(|| json!({}));
    if !slot.is_object() {
        *slot = Value::Object(Map::new());
    }
    let Value::Object(m) = slot else { unreachable!("made an object above") };
    m
}
