//! Settings search (`settings/search.tsx`, `search-index.ts`, `search-results.ts`,
//! `search-machine.ts`): the rail's field finds pages, every row of every page, the servers and
//! their projects, and what plugins' manifests and pages declare. Its results take the page
//! list's place in the rail, grouped under sticky headers; opening one shows its page, scrolls
//! its row to the middle and lights it for 1.8 s, while the results stay beside it. ⌘F
//! (`settings.search.focus`) focuses the field; Escape clears it, then leaves Settings. The same
//! index answers the command palette's Settings results ([`palette`], [`reveal`]).

use opencode_i18n::{key, t};
use opencode_kit::servers::ServerKey;
use opencode_ui::keymap::{self, Bind};
use rune::{Icon, Key, KeyEvent, TextEdit, Tx};
use rune_theme_opencode::icons as ic;

use crate::host::SettingsHost;
use crate::pages::{self, ExtensionsTab, Page};

mod view;

pub use view::{field, results};

/// How many results show before "Narrow your search" (`shown`'s 60).
const SHOWN: usize = 60;
/// How long a revealed row stays lit before starting its fade.
const REVEAL_LIT_S: f64 = 0.05;
/// How long a revealed row stays lit (`settings-search-reveal`).
const REVEAL_S: f64 = 1.8;

/// One thing settings search finds.
#[derive(Clone, Debug)]
pub struct Entry {
    pub id: String,
    pub title: String,
    pub description: String,
    pub keywords: String,
    /// What it belongs to: a server's name, a plugin's, or nothing for the app's own.
    pub owner: String,
    /// Where it is, as the result names it: "Preferences / General", "Appearance".
    pub where_: String,
    pub page: Page,
    /// The row it reveals (`data-action`), none for a page.
    pub target: Option<String>,
    pub subtab: Option<ExtensionsTab>,
    /// A page of its own ("Pages").
    pub top: bool,
    /// A server or a project rather than a setting.
    pub entity: bool,
    pub project: Option<String>,
    pub icon: &'static Icon,
    /// The server whose page it's on, when Settings lists more than one: opening it shows that
    /// server's pages.
    pub server: Option<ServerKey>,
}

/// The rail's search: its field, the lit and opened results, and the row being revealed.
#[derive(Default)]
pub struct Find {
    pub query: TextEdit,
    /// The result the arrows light, by id.
    pub highlighted: Option<String>,
    /// The result opened last (`aria-current`), by id.
    pub selected: Option<String>,
    /// The target id opened from search or palette, kept for the visit so collapsible sections
    /// (such as Settings > Browser > Advanced) can auto-reveal the target row.
    pub revealed_target: Option<String>,
    /// The row a result or the palette opened, lit then faded on the app's clock.
    reveal: Reveal,
    /// In narrow layouts, selecting a result collapses the results overlay until the query is edited or refocused.
    pub collapsed_narrow: bool,
}

/// Three-phase highlight-then-fade timer for the row opened from search or the command palette.
#[derive(Clone, Debug, Default, PartialEq)]
enum Reveal {
    #[default]
    Idle,
    /// First frame draws it lit until `fade_at`; `end_at` carries when the subsequent fade ends.
    Lit { target: String, fade_at: f64, end_at: f64 },
    /// Fading out until `end_at`.
    Fading { target: String, end_at: f64 },
}

enum RevealEvent {
    Start { target: String, now: f64 },
    Tick { now: f64 },
    Clear,
}

enum RevealEffect {
    None,
    Changed { wake_after: Option<std::time::Duration> },
}

impl Reveal {
    fn step(&mut self, event: RevealEvent) -> RevealEffect {
        match (std::mem::take(self), event) {
            (_, RevealEvent::Start { target, now }) => {
                *self = Self::Lit { target, fade_at: now + REVEAL_LIT_S, end_at: now + REVEAL_S + 0.1 };
                RevealEffect::Changed { wake_after: Some(std::time::Duration::from_millis(50)) }
            }
            (Self::Idle, RevealEvent::Clear | RevealEvent::Tick { .. }) => RevealEffect::None,
            (Self::Lit { .. } | Self::Fading { .. }, RevealEvent::Clear) => {
                RevealEffect::Changed { wake_after: None }
            }
            (Self::Lit { target, fade_at, end_at }, RevealEvent::Tick { now }) => {
                if now > fade_at {
                    *self = Self::Fading { target, end_at };
                    RevealEffect::Changed { wake_after: Some(std::time::Duration::from_secs_f64(REVEAL_S + 0.1)) }
                } else {
                    *self = Self::Lit { target, fade_at, end_at };
                    RevealEffect::None
                }
            }
            (Self::Fading { target, end_at }, RevealEvent::Tick { now }) => {
                if now > end_at {
                    RevealEffect::Changed { wake_after: None }
                } else {
                    *self = Self::Fading { target, end_at };
                    RevealEffect::None
                }
            }
        }
    }
}

/// Whether the rail shows results instead of its pages.
pub fn active(s: &dyn SettingsHost) -> bool {
    !s.settings_pages().find.query.text.trim().is_empty()
}

/// Target entry id opened from settings search or palette for the current visit.
pub fn revealed_target(s: &dyn SettingsHost) -> Option<&str> {
    s.settings_pages().find.revealed_target.as_deref()
}

/// Whether row `target` is being revealed: `Some(true)` while lit, `Some(false)` while it fades.
pub fn reveal_of(s: &dyn SettingsHost, target: &str) -> Option<bool> {
    match &s.settings_pages().find.reveal {
        Reveal::Lit { target: t, .. } if t == target => Some(true),
        Reveal::Fading { target: t, .. } if t == target => Some(false),
        _ => None,
    }
}

/// Clears the search, as Escape or closing Settings does.
pub fn clear(s: &mut dyn SettingsHost) {
    let find = &mut s.settings_pages_mut().find;
    let focused = find.query.focused;
    *find = Find::default();
    find.query.focused = focused;
}

/// A page opened from the rail ends the reveal, as another view does.
pub fn leave(s: &mut dyn SettingsHost) {
    s.settings_pages_mut().find.revealed_target = None;
    s.settings_pages_mut().find.reveal.step(RevealEvent::Clear);
}

/// Starts the lit row's fade after its first frame, and drops it once faded.
pub fn poll(s: &mut dyn SettingsHost, tx: &mut Tx) -> bool {
    let now = s.clock();
    let open = s.settings_open();
    match &s.settings_pages().find.reveal {
        Reveal::Idle => return false,
        Reveal::Lit { fade_at, .. } if open && now <= *fade_at => {
            tx.wake_after(std::time::Duration::from_millis(50));
            return false;
        }
        Reveal::Fading { end_at, .. } if open && now <= *end_at => return false,
        _ => {}
    }
    let event = if !open { RevealEvent::Clear } else { RevealEvent::Tick { now } };
    match s.settings_pages_mut().find.reveal.step(event) {
        RevealEffect::Changed { wake_after } => {
            if let Some(d) = wake_after {
                tx.wake_after(d);
            }
            true
        }
        RevealEffect::None => false,
    }
}

/// Every entry: the pages, then each page's rows, the plugins', the server and its projects.
pub fn index(s: &dyn SettingsHost) -> Vec<Entry> {
    let mut out = Vec::new();
    for page in crate::shell::groups(s).into_iter().flatten() {
        let label = crate::shell::label(s, &page);
        let keywords = match page {
            Page::Shortcuts => "keybind keyboard hotkey",
            Page::About => "version license credits",
            Page::Experimental => "experiments preview beta",
            Page::Worktrees => "workspaces disk usage cleanup delete",
            Page::Providers => "connect api key credentials",
            Page::Models => "model picker visibility",
            _ => "",
        };
        out.push(entry(&page, &label, &label, None, keywords, "", true));
    }
    let mut add = |page: Page, target: &str, title: opencode_i18n::Key, section: Option<opencode_i18n::Key>, description: Option<opencode_i18n::Key>, keywords: &str| {
        let text = |key: opencode_i18n::Key| opencode_i18n::I18N.text(key);
        let where_ = match (&page, section) {
            (Page::General, s) => format!("{} / {}", Page::General.label(), s.map_or(t!("settings.general.section.general"), text)),
            (_, Some(s)) => text(s).to_owned(),
            _ => page.label().to_owned(),
        };
        let mut e = entry(&page, text(title), &where_, Some(target), keywords, description.map_or("", text), false);
        e.id = format!("{}:{target}", page.id());
        out.push(e);
    };
    // A server's own rows are indexed only for the server its pages show, while they can load
    // (`canIndexServerSettings`).
    let servers = pages::servers::inventory::list(s);
    let selected = servers
        .iter()
        .find(|x| x.key == s.settings_pages().server.key)
        .or_else(|| servers.first())
        .cloned()
        .unwrap_or_else(pages::servers::inventory::fallback_own);
    let proto = s.settings().navigation_prototype();
    for (page, target, title, section, description, keywords) in ROWS {
        if *page == Page::Servers && !selected.availability.can_index() {
            continue;
        }
        if proto && *target == "settings-tab-layout" {
            continue;
        }
        add(page.clone(), target, *title, *section, *description, keywords);
    }
    if proto {
        add(
            Page::General,
            "settings-navigation",
            key!("settings.general.row.navigation.title"),
            None,
            Some(key!("settings.general.row.navigation.description")),
            "tabs list navigation sidebar mode layout vertical horizontal",
        );
        if s.settings().navigation() == crate::Navigation::Tabs {
            add(
                Page::General,
                "settings-tab-position",
                key!("settings.general.row.tabPosition.title"),
                None,
                None,
                "top side tabs position vertical horizontal",
            );
            add(
                Page::General,
                "settings-tab-grouping",
                key!("settings.general.row.tabGrouping.title"),
                None,
                None,
                "group tabs project server none",
            );
        }
    }
    if cfg!(target_os = "macos") {
        add(Page::General, "terminal-option-meta", key!("rune.settings.general.row.terminalOptionMeta.title"), None, None, "terminal option alt meta esc keyboard");
    }
    if cfg!(windows) {
        add(Page::General, "terminal-marks-pwsh", key!("rune.settings.general.row.terminalMarks.search"), None, None, "terminal shell integration prompt marks osc 133 powershell pwsh cmd git bash wsl");
    }
    add(Page::General, "terminal-placement", key!("rune.settings.general.row.terminalPlacement.title"), None, Some(key!("rune.settings.general.row.terminalPlacement.description")), "terminal dock side bottom");
    add(Page::Experimental, "record-glitches", key!("rune.settings.developer.rewind.title"), Some(key!("rune.settings.developer.title")), None, "glitch record frames developer");
    for (tab, id, title, keywords) in [
        (ExtensionsTab::Mcps, "mcps", t!("settings.extensions.tab.mcps"), "model context protocol tools"),
        (ExtensionsTab::Plugins, "plugins", t!("settings.extensions.tab.plugins"), "extension gui built-in"),
        (ExtensionsTab::Skills, "skills", t!("settings.extensions.tab.skills"), ""),
    ] {
        let mut e = entry(&Page::Extensions, title, Page::Extensions.label(), None, keywords, "", false);
        e.id = format!("extensions:{id}");
        e.subtab = Some(tab);
        out.push(e);
    }
    for b in keymap::all().iter().filter(|b| b.listed) {
        let mut e = entry(&Page::Shortcuts, &b.title(), Page::Shortcuts.label(), Some(&format!("shortcut-{}", b.id)), "shortcut keybind", "", false);
        e.id = format!("shortcut:{}", b.id);
        out.push(e);
    }
    plugin_entries(s, &mut out);
    let multiple = servers.len() > 1;
    for e in out.iter_mut().filter(|e| e.page.per_server() || e.target.as_deref() == Some("settings-shell")) {
        if multiple {
            e.owner = selected.name.clone();
            e.server = Some(selected.key.clone());
        }
    }
    for server in &servers {
        out.push(Entry {
            id: format!("server:{}", server.key),
            title: server.name.clone(),
            where_: t!("settings.server.section.connection").into(),
            owner: t!("settings.tab.servers").into(),
            entity: true,
            server: multiple.then(|| server.key.clone()),
            ..entry(&Page::Servers, "", "", None, "", "", false)
        });
    }
    // The projects of the server the pages show, once the window has read them.
    if selected.availability.can_index() && s.active_server_key() == selected.key {
        for project in s.projects() {
            let name = pages::projects::name(project);
            out.push(Entry {
                id: format!("project:{}:{}", selected.key, project.directory),
                title: name,
                where_: t!("settings.tab.projects").into(),
                owner: selected.name.clone(),
                entity: true,
                project: Some(project.directory.clone()),
                target: Some(format!("project-{}", project.directory)),
                server: multiple.then(|| selected.key.clone()),
                ..entry(&Page::Projects, "", "", None, "", "", false)
            });
        }
    }
    out
}

fn entry(page: &Page, title: &str, where_: &str, target: Option<&str>, keywords: &str, description: &str, top: bool) -> Entry {
    Entry {
        id: format!("{}:{}:{title}", page.id(), target.unwrap_or_default()),
        title: title.to_owned(),
        description: description.to_owned(),
        keywords: keywords.to_owned(),
        owner: String::new(),
        where_: where_.to_owned(),
        page: page.clone(),
        target: target.map(str::to_owned),
        subtab: None,
        top,
        entity: false,
        project: None,
        icon: page.icon(),
        server: None,
    }
}

/// What plugins declare: their manifests' `[[settings_search]]` rows (known before they load),
/// their settings rows (Extensions › Plugins) and their pages' entries.
fn plugin_entries(s: &dyn SettingsHost, out: &mut Vec<Entry>) {
    for e in s.plugin_search_entries() {
        let (page, where_) = match e.page.as_str() {
            "general" => (Page::General, format!("{} / {}", Page::General.label(), e.owner)),
            "plugins" => (Page::Extensions, Page::Extensions.label().to_owned()),
            "servers" => (Page::Servers, Page::Servers.label().to_owned()),
            id => {
                let (key, title) = s.resolve_plugin_page(id);
                (Page::Plugin(key), title.unwrap_or_else(|| e.owner.clone()))
            }
        };
        let top = matches!(page, Page::Plugin(_)) && e.id == e.page;
        let mut x = entry(&page, &e.title, &where_, (!top).then_some(e.id.as_str()), &e.keywords, &e.description, top);
        x.id = format!("plugin:{}:{}", e.owner, e.id);
        out.push(x);
    }
}

/// Desktop's rows, by page (`search-catalog.ts`).
type Row = (Page, &'static str, opencode_i18n::Key, Option<opencode_i18n::Key>, Option<opencode_i18n::Key>, &'static str);

const TIMELINE: Option<opencode_i18n::Key> = Some(key!("settings.timeline.title"));
const NOTIFICATIONS: Option<opencode_i18n::Key> = Some(key!("settings.general.section.notifications"));
const SOUNDS: Option<opencode_i18n::Key> = Some(key!("settings.general.section.sounds"));

const ROWS: &[Row] = &[
    (Page::General, "settings-language", key!("settings.general.row.language.title"), None, None, ""),
    (Page::General, "settings-tab-layout", key!("settings.general.row.interfaceMode.title"), None, None, "top side tabs inbox sidebar mode layout vertical horizontal focus pro"),
    (Page::General, "settings-shared-side-panel", key!("settings.general.row.sharedSidePanel.title"), Some(key!("settings.general.section.general")), Some(key!("settings.general.row.sharedSidePanel.description")), "right side panel review sidebar persist keep open sessions switch"),
    (Page::General, "settings-new-session-start", key!("rune.settings.newSession.startIn.title"), None, Some(key!("rune.settings.newSession.startIn.description")), "new session start project default directory folder"),
    (Page::General, "settings-new-session-worktree", key!("rune.settings.newSession.worktree.title"), None, Some(key!("rune.settings.newSession.worktree.defaultDescription")), "worktree workspace checkout new last used"),
    (Page::General, "settings-auto-accept-permissions", key!("command.permissions.autoaccept.enable"), None, Some(key!("settings.permissions.autoAccept.default.description")), "permissions approve allow auto accept default"),
    (Page::General, "settings-timeline-detail", key!("settings.timeline.detail"), TIMELINE, None, "thinking reasoning tools timeline summary detailed"),
    (Page::General, "settings-background-activity", key!("settings.timeline.row.backgroundActivity.title"), TIMELINE, Some(key!("settings.timeline.row.backgroundActivity.description")), "capsule panel subagent shell background activity position alignment hidden left center right"),
    (Page::General, "settings-timeline-width", key!("settings.timeline.row.width.title"), TIMELINE, Some(key!("settings.timeline.row.width.description")), "column line length measure narrow focused comfortable wide readability"),
    (Page::General, "settings-fold-user-messages", key!("settings.timeline.row.foldUserMessages.title"), TIMELINE, Some(key!("settings.timeline.row.foldUserMessages.description")), "fold collapse long messages prompts read more"),
    (Page::General, "settings-show-turn-diffs", key!("settings.timeline.row.turnDiffs.title"), TIMELINE, Some(key!("settings.timeline.row.turnDiffs.description")), ""),
    (Page::General, "settings-wrap-code-blocks", key!("settings.timeline.row.wrapCodeBlocks.title"), TIMELINE, Some(key!("settings.timeline.row.wrapCodeBlocks.description")), "code wrap lines horizontal scroll"),
    (Page::General, "settings-show-custom-agents", key!("settings.general.row.showCustomAgents.title"), Some(key!("settings.general.section.general")), None, ""),
    (Page::General, "settings-follow-up-behavior", key!("settings.general.row.followUpBehavior.title"), None, None, "queue steer follow up"),
    (Page::General, "settings-question-submit-key", key!("settings.general.row.questionSubmitKey.title"), None, None, "question answer enter shift submit send keyboard newline multiline custom"),
    (Page::General, "settings-pinch-zoom", key!("settings.general.row.pinchZoom.title"), None, None, ""),
    (Page::General, "settings-spellcheck", key!("settings.general.row.spellcheck.title"), None, None, "spelling spell check dictionary"),
    (Page::General, "settings-mobile-diff-wrap", key!("plugin.review.settings.wrapLines.title"), None, None, "diff wrap lines"),
    (Page::Experimental, "settings-tab-groups", key!("settings.experimental.row.tabGroups.title"), None, Some(key!("settings.experimental.row.tabGroups.description")), "tab groups server project colors drag organize search sessions"),
    (Page::Experimental, "settings-sidebar-sections", key!("rune.settings.experimental.row.sidebarSections.title"), None, Some(key!("rune.settings.experimental.row.sidebarSections.description")), "sections select multiple batch group headers collapse side tabs"),
    (Page::Experimental, "settings-automations", key!("settings.experimental.row.automations.title"), None, Some(key!("settings.experimental.row.automations.description")), "automations schedules timers cron watches events"),
    (Page::Experimental, "settings-new-question-ui", key!("settings.experimental.row.newQuestionUI.title"), None, Some(key!("settings.experimental.row.newQuestionUI.description")), "question answers keyboard dock redesign"),
    (Page::Experimental, "settings-smooth-timeline", key!("rune.settings.experimental.row.smoothTimeline.title"), None, Some(key!("rune.settings.experimental.row.smoothTimeline.description")), "motion animation pending working queue steer tray delivery"),
    (Page::Experimental, "settings-sidebar-organize", key!("settings.experimental.row.sidebarOrganize.title"), None, Some(key!("settings.experimental.row.sidebarOrganize.description")), "vertical side tabs projects recents organize sidebar"),
    (Page::Appearance, "settings-color-scheme", key!("settings.general.row.colorScheme.title"), None, None, "dark mode light mode system"),
    (Page::Appearance, "settings-theme", key!("settings.general.row.theme.title"), None, None, ""),
    (Page::Appearance, "settings-ui-font", key!("settings.general.row.uiFont.title"), None, None, "interface typeface"),
    (Page::Appearance, "settings-code-font", key!("settings.general.row.font.title"), None, None, "code typeface"),
    (Page::Appearance, "settings-terminal-font", key!("settings.general.row.terminalFont.title"), None, None, "terminal typeface"),
    (Page::Appearance, "settings-zoom", key!("settings.appearance.row.zoom.title"), None, None, "scale size text font larger bigger accessibility"),
    (Page::Appearance, "settings-reduce-motion", key!("settings.appearance.row.reduceMotion.title"), None, None, "animation accessibility"),
    (Page::Appearance, "settings-text-renderer", key!("rune.textRendering.title"), None, Some(key!("rune.textRendering.description")), "font antialiasing smoothing windfoil directwrite cleartype retina"),
    (Page::Notifications, "settings-notifications-agent", key!("settings.general.notifications.agent.title"), NOTIFICATIONS, Some(key!("settings.general.notifications.agent.description")), "desktop notifications agent"),
    (Page::Notifications, "settings-notifications-permissions", key!("settings.general.notifications.permissions.title"), NOTIFICATIONS, Some(key!("settings.general.notifications.permissions.description")), "desktop notifications permissions"),
    (Page::Notifications, "settings-notifications-questions", key!("settings.general.notifications.questions.title"), NOTIFICATIONS, Some(key!("settings.general.notifications.questions.description")), "desktop notifications questions mcp elicitation input"),
    (Page::Notifications, "settings-notifications-errors", key!("settings.general.notifications.errors.title"), NOTIFICATIONS, Some(key!("settings.general.notifications.errors.description")), "desktop notifications errors"),
    (Page::Notifications, "settings-sounds-agent", key!("settings.general.sounds.agent.title"), SOUNDS, Some(key!("settings.general.sounds.agent.description")), "sound audio agent"),
    (Page::Notifications, "settings-sounds-permissions", key!("settings.general.sounds.permissions.title"), SOUNDS, Some(key!("settings.general.sounds.permissions.description")), "sound audio permissions"),
    (Page::Notifications, "settings-sounds-questions", key!("settings.general.sounds.questions.title"), SOUNDS, Some(key!("settings.general.sounds.questions.description")), "sound audio questions mcp elicitation input"),
    (Page::Notifications, "settings-sounds-errors", key!("settings.general.sounds.errors.title"), SOUNDS, Some(key!("settings.general.sounds.errors.description")), "sound audio errors"),
    (Page::Notifications, "settings-sounds-interface", key!("settings.general.sounds.interface.title"), SOUNDS, Some(key!("settings.general.sounds.interface.description")), "sound audio interface effects wordmark logo"),
    (Page::Servers, "settings-shell", key!("settings.general.row.shell.title"), None, Some(key!("settings.general.row.shell.description")), "bash zsh powershell"),
];

/// `rankSettings`: entries any of whose words the query names, the pages first, then the
/// best match (the whole title, every word in it, in its keywords, anywhere), then by title.
pub fn rank(query: &str, items: Vec<Entry>) -> Vec<Entry> {
    let value = normalize(query);
    if value.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(Entry, u8)> = items
        .into_iter()
        .filter_map(|item| {
            let tokens: Vec<&str> = value.split(' ').collect();
            let title = normalize(&item.title);
            let primary = normalize(&format!("{} {}", item.title, item.keywords));
            let description = normalize(&item.description);
            let context = normalize(&format!("{} {}", item.owner, if item.entity { "" } else { &item.where_ }));
            let fuzzy = fuzzy(&value, &title);
            let named = tokens.iter().any(|t| format!("{primary} {description}").contains(t));
            if !named && !fuzzy {
                return None;
            }
            let all = |hay: &str| tokens.iter().all(|t| hay.contains(t));
            let score = if title == value {
                5
            } else if all(&title) {
                4
            } else if all(&primary) {
                3
            } else if all(&format!("{primary} {description} {context}")) {
                2
            } else if fuzzy {
                1
            } else {
                0
            };
            (score > 0).then_some((item, score))
        })
        .collect();
    scored.sort_by(|(a, sa), (b, sb)| b.top.cmp(&a.top).then(sb.cmp(sa)).then_with(|| rune::text::compare_names(&a.title, &b.title)));
    scored.into_iter().map(|(e, _)| e).collect()
}

/// A close fuzzy match on the title: its letters in order, mostly in runs (fuzzysort's 0.6).
fn fuzzy(query: &str, title: &str) -> bool {
    let len = query.chars().filter(|c| !c.is_whitespace()).count() as i64;
    opencode_search::model_search::score(query, &[title]).is_some_and(|s| s >= 18 * len)
}

fn normalize(value: &str) -> String {
    value.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A result's group (`category`): Pages, Settings, Projects or Servers.
fn category(e: &Entry) -> &'static str {
    if e.top {
        "Pages"
    } else if !e.entity {
        "Settings"
    } else if e.project.is_some() {
        "Projects"
    } else {
        "Servers"
    }
}

/// A group's header (`settings.search.group.*`).
fn category_label(group: &str) -> &'static str {
    match group {
        "Pages" => t!("settings.search.group.pages"),
        "Settings" => t!("settings.search.group.settings"),
        "Projects" => t!("settings.search.group.projects"),
        _ => t!("settings.search.group.servers"),
    }
}

/// The results as shown: the first 60, each group's together in the order they first rank.
fn shown(s: &dyn SettingsHost) -> (Vec<Entry>, bool) {
    let all = rank(&s.settings_pages().find.query.text, index(s));
    let more = all.len() > SHOWN;
    let top: Vec<Entry> = all.into_iter().take(SHOWN).collect();
    let mut order: Vec<&'static str> = Vec::new();
    for e in &top {
        if !order.contains(&category(e)) {
            order.push(category(e));
        }
    }
    (order.into_iter().flat_map(|c| top.iter().filter(move |e| category(e) == c).cloned()).collect(), more)
}

/// Opens result `e`: its page, its row scrolled to the middle and lit.
pub fn open(s: &mut dyn SettingsHost, e: &Entry, tx: &mut Tx) {
    let find = &mut s.settings_pages_mut().find;
    find.selected = Some(e.id.clone());
    find.highlighted = Some(e.id.clone());
    if crate::compact() {
        find.collapsed_narrow = true;
        find.query.focused = false;
    }
    reveal_entry(s, e, tx);
}

/// Opens `e` from the command palette: Settings on its page with its row lit, without a search.
pub fn reveal(s: &mut dyn SettingsHost, e: &Entry, tx: &mut Tx) {
    if !s.settings_open() {
        crate::open_settings_tx(s, e.page.clone(), tx);
    }
    clear(s);
    reveal_entry(s, e, tx);
    let key = e.target.as_ref().map_or_else(|| "settings".to_owned(), |t| format!("setting-{t}"));
    tx.focus_when_shown(key, tx.focus_visible());
}

fn reveal_entry(s: &mut dyn SettingsHost, e: &Entry, tx: &mut Tx) {
    tx.instant();
    let moved = e.server.as_ref().is_some_and(|server| *server != s.settings_pages().server.key);
    if let Some(server) = &e.server {
        s.settings_pages_mut().server.key = server.clone();
    }
    if s.settings_pages().page != e.page || moved {
        pages::show_page(s, e.page.clone());
    }
    if let Some(tab) = e.subtab {
        s.settings_pages_mut().tab = tab;
    }
    s.settings_pages_mut().find.revealed_target = e.target.clone();
    match &e.target {
        Some(target) => {
            let now = s.clock();
            let effect = s.settings_pages_mut().find.reveal.step(RevealEvent::Start { target: target.clone(), now });
            tx.scroll_center(format!("setting-{target}"));
            if let RevealEffect::Changed { wake_after: Some(d) } = effect {
                tx.wake_after(d);
            }
        }
        None => tx.scroll_to_instant("settings", 0.0, 0.0),
    }
}

/// The command palette's Settings results for `query` (VP-09): the same index and ranking as
/// the rail, at most `max`.
pub fn palette(s: &dyn SettingsHost, query: &str, max: usize) -> Vec<Entry> {
    rank(query, index(s)).into_iter().take(max).collect()
}

/// The field's and results' keys: ↑↓ light a result, Enter opens it, Escape clears the query;
/// ⌘F focuses the field from anywhere in Settings.
pub fn key(s: &mut dyn SettingsHost, ev: &KeyEvent, tx: &mut Tx) -> bool {
    if Bind::SettingsSearch.matches_tx(ev, tx) {
        tx.instant();
        let pages = s.settings_pages_mut();
        pages.search.focused = false;
        pages.shortcuts.filter.focused = false;
        pages.projects.filter.focused = false;
        pages.find.collapsed_narrow = false;
        let q = &mut pages.find.query;
        q.focused = true;
        let end = q.text.len();
        q.set_selection(0, end);
        return true;
    }
    let focused = s.settings_pages().find.query.focused;
    if focused && s.settings_pages().find.query.is_composing() {
        return false;
    }
    if ev.key == Key::Escape && active(s) {
        tx.instant();
        clear(s);
        s.settings_pages_mut().find.query.focused = focused;
        return true;
    }
    if !focused || !active(s) {
        return false;
    }
    if s.settings_pages().find.collapsed_narrow
        && matches!(ev.key, Key::Down | Key::Up)
        && !(ev.mods.shift || ev.mods.ctrl || ev.mods.cmd || ev.mods.alt)
    {
        tx.instant();
        s.settings_pages_mut().find.collapsed_narrow = false;
        return true;
    }
    let (list, _) = shown(s);
    let at = active_index(&list, s.settings_pages().find.highlighted.as_deref());
    let next = match ev.key {
        _ if list.is_empty() || ev.mods.shift || ev.mods.ctrl || ev.mods.cmd || ev.mods.alt => None,
        Key::Down => Some((at + 1) % list.len()),
        Key::Up => Some(if at == 0 { list.len() - 1 } else { at - 1 }),
        Key::Home => Some(0),
        Key::End => Some(list.len() - 1),
        _ => None,
    };
    if let Some(next) = next {
        tx.instant();
        s.settings_pages_mut().find.highlighted = Some(list[next].id.clone());
        if next == 0 {
            tx.scroll_to_instant("settings-results-scroll", 0.0, 0.0);
        } else {
            tx.scroll_into_view(format!("settings-result-{next}"));
        }
        return true;
    }
    if ev.key == Key::Enter
        && let Some(e) = list.get(at).cloned()
    {
        open(s, &e, tx);
        return true;
    }
    false
}

fn active_index(list: &[Entry], highlighted: Option<&str>) -> usize {
    highlighted.and_then(|id| list.iter().position(|e| e.id == id)).unwrap_or(0)
}

