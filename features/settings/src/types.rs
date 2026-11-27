//! Core preference value types owned by Settings: color scheme, interface layout, terminal
//! dock placement, notification kinds, reduce motion, text rendering, bundled font tables,
//! zoom levels, and Windows shell prompt-mark targets.

use rune::{FontSmoothing, SmoothingSupport};

pub use opencode_runtime::{DockPlacement, NotifyKind, NotifyKinds, ZOOM_LEVELS, ZOOM_MAX, ZOOM_MIN, step_zoom};

/// Desktop's colour scheme preference (`appearance`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Scheme {
    #[default]
    System,
    Light,
    Dark,
}

impl Scheme {
    pub const ALL: [Scheme; 3] = [Scheme::System, Scheme::Light, Scheme::Dark];

    pub fn name(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::System => opencode_i18n::t!("theme.scheme.system"),
            Self::Light => opencode_i18n::t!("theme.scheme.light"),
            Self::Dark => opencode_i18n::t!("theme.scheme.dark"),
        }
    }

    /// `"system"`, `"light"` or `"dark"`; the system's otherwise.
    pub fn parse(name: &str) -> Scheme {
        Self::ALL.into_iter().find(|s| s.name() == name).unwrap_or_default()
    }
}

/// Where the tabs live (`settings.json`'s `"tabLayout"`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum TabLayout {
    /// "Top tabs": a strip in the titlebar. Desktop's default.
    #[default]
    Horizontal,
    /// "Side tabs": a list in the sidebar.
    Vertical,
    /// "Inbox": the sidebar sorts sessions by what needs you.
    Inbox,
}

impl TabLayout {
    pub const ALL: [TabLayout; 3] = [TabLayout::Horizontal, TabLayout::Vertical, TabLayout::Inbox];

    pub fn name(self) -> &'static str {
        match self {
            Self::Horizontal => "horizontal",
            Self::Vertical => "vertical",
            Self::Inbox => "inbox",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Horizontal => opencode_i18n::t!("settings.general.row.interfaceMode.top"),
            Self::Vertical => opencode_i18n::t!("settings.general.row.interfaceMode.side"),
            Self::Inbox => opencode_i18n::t!("settings.general.row.interfaceMode.inbox"),
        }
    }

    pub fn parse(s: Option<&str>) -> TabLayout {
        Self::ALL.into_iter().find(|l| Some(l.name()) == s).unwrap_or_default()
    }

    /// Side tabs and Inbox have a sidebar; top tabs have none, so ⇧⌘B does nothing there.
    pub fn has_sidebar(self) -> bool {
        matches!(self, Self::Vertical | Self::Inbox)
    }
}

/// Settings › Appearance › Reduce motion.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ReduceMotion {
    #[default]
    System,
    On,
    Off,
}

impl ReduceMotion {
    pub const ALL: [ReduceMotion; 3] = [ReduceMotion::System, ReduceMotion::On, ReduceMotion::Off];

    pub fn name(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::On => "on",
            Self::Off => "off",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::System => opencode_i18n::t!("settings.appearance.row.reduceMotion.system"),
            Self::On => opencode_i18n::t!("settings.appearance.row.reduceMotion.on"),
            Self::Off => opencode_i18n::t!("settings.appearance.row.reduceMotion.off"),
        }
    }

    pub fn parse(s: Option<&str>) -> ReduceMotion {
        Self::ALL.into_iter().find(|m| Some(m.name()) == s).unwrap_or_default()
    }
}

/// A Text rendering choice, as saved (`textRenderer`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum TextRenderer {
    /// Rune's default here: Windfoil with Chromium's weight; on a Mac (and in a Mac's browser)
    /// CoreText's text without font smoothing, as official draws it ([`Self::default_mode`]).
    #[default]
    Default,
    WindfoilMac,
    WindfoilChromium,
    Slug,
    WindfoilRgb,
    /// DirectWrite grayscale with Skia's gamma on Windows, hinted grayscale elsewhere.
    ChromiumLike,
    ClearType,
    RetinaLike,
    HintedGpu,
    /// For measuring launch and frame cost only, by `--text-renderer first`, and never listed.
    First,
}

impl TextRenderer {
    pub const ALL: [TextRenderer; 9] = [
        TextRenderer::Default,
        TextRenderer::WindfoilMac,
        TextRenderer::WindfoilChromium,
        TextRenderer::Slug,
        TextRenderer::WindfoilRgb,
        TextRenderer::ChromiumLike,
        TextRenderer::ClearType,
        TextRenderer::RetinaLike,
        TextRenderer::HintedGpu,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::WindfoilMac => "windfoil",
            Self::WindfoilChromium => "windfoil-chromium",
            Self::Slug => "slug",
            Self::WindfoilRgb => "windfoil-rgb",
            Self::ChromiumLike => "chromium",
            Self::ClearType => "cleartype",
            Self::RetinaLike => "retina",
            Self::HintedGpu => "hinted-gpu",
            Self::First => "first",
        }
    }

    /// A saved name; `classic`, the first build's Off position, is the system's own text.
    pub fn parse(s: Option<&str>) -> TextRenderer {
        match s {
            Some("classic") if cfg!(target_os = "macos") => Self::RetinaLike,
            Some("classic") => Self::ChromiumLike,
            Some("first") => Self::First,
            _ => Self::ALL.into_iter().find(|r| Some(r.name()) == s).unwrap_or_default(),
        }
    }

    pub fn label(self) -> &'static str {
        use opencode_i18n::t;
        match self {
            Self::Default if Self::mac_default() => t!("rune.textRendering.default.mac"),
            Self::Default => t!("rune.textRendering.default"),
            Self::WindfoilMac => t!("rune.textRendering.windfoil"),
            Self::WindfoilChromium => t!("rune.textRendering.windfoilChromium"),
            Self::Slug => t!("rune.textRendering.slug"),
            Self::WindfoilRgb => t!("rune.textRendering.windfoilRgb"),
            Self::ChromiumLike if cfg!(target_os = "windows") => t!("rune.textRendering.chromium.windows"),
            Self::ChromiumLike => t!("rune.textRendering.chromium"),
            Self::ClearType => t!("rune.textRendering.cleartype"),
            Self::RetinaLike => t!("rune.textRendering.retina"),
            Self::HintedGpu => t!("rune.textRendering.hintedGpu"),
            Self::First => t!("rune.textRendering.first"),
        }
    }

    /// What Default draws: CoreText without font smoothing on macOS, Rune's default elsewhere.
    pub fn default_mode() -> FontSmoothing {
        if Self::mac_default() { FontSmoothing::Off } else { FontSmoothing::default() }
    }

    fn mac_default() -> bool {
        FontSmoothing::default() == FontSmoothing::On
    }

    /// The mode it draws with, when Rune has one for it.
    pub fn mode(self) -> Option<FontSmoothing> {
        Some(match self {
            Self::Default => Self::default_mode(),
            Self::WindfoilMac => FontSmoothing::Windfoil,
            Self::WindfoilChromium => FontSmoothing::WindfoilChromium,
            Self::Slug => FontSmoothing::Slug,
            Self::ChromiumLike if cfg!(target_os = "windows") => FontSmoothing::DirectWrite,
            Self::ChromiumLike => FontSmoothing::Hinted,
            Self::RetinaLike => FontSmoothing::On,
            Self::HintedGpu => FontSmoothing::HintedGpu,
            Self::WindfoilRgb => FontSmoothing::WindfoilRgb,
            Self::ClearType => FontSmoothing::ClearType,
            Self::First => FontSmoothing::Off,
        })
    }

    /// Whether this machine draws it as named, and why not in a line.
    pub fn support(self) -> SmoothingSupport {
        match self.mode() {
            Some(FontSmoothing::ClearType) if !cfg!(target_os = "windows") => {
                SmoothingSupport::No(opencode_i18n::t!("rune.textRendering.reason.windowsOnly"))
            }
            Some(mode) => mode.support(),
            None => SmoothingSupport::No(""),
        }
    }

    /// The next choice after this one that draws here, for the palette.
    pub fn cycle(self) -> TextRenderer {
        let at = Self::ALL.iter().position(|r| *r == self).unwrap_or(0);
        (1..Self::ALL.len())
            .map(|i| Self::ALL[(at + i) % Self::ALL.len()])
            .find(|r| !matches!(r.support(), SmoothingSupport::No(_)))
            .unwrap_or(self)
    }
}

/// The Terminal font knob's choices, which the setting picks (Desktop's default OpenMono at index 3, `opencode#64797`).
pub const TERMINAL_FONTS: [&str; 4] = ["Commit Mono", "IBM Plex Mono", "SF Mono", "OpenMono"];

/// A font a row offers: its name, and the family list behind Rune's sans or mono slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Font {
    pub name: &'static str,
    pub family: &'static str,
}

const fn font(name: &'static str, family: &'static str) -> Font {
    Font { name, family }
}

/// The UI font row's choices, Desktop's default (Inter) first.
pub const UI_FONTS: [Font; 4] = [
    font("Inter", "Inter"),
    font("SF Pro", "System Font, Inter"),
    font("OpenMono", "OpenMono, Inter"),
    font("Commit Mono", "CommitMono, Inter"),
];

/// The code font row's choices, Desktop's default (OpenMono, `opencode#64797`) first.
pub const CODE_FONTS: [Font; 3] = [
    font("OpenMono", "OpenMono, OpenMono Icons"),
    font("Commit Mono", "CommitMono, OpenMono, OpenMono Icons"),
    font("SF Mono", "ui-monospace, OpenMono, OpenMono Icons"),
];

/// The index of the font named `name` in `fonts`, the default when there's none.
pub fn font_index(fonts: &[Font], name: Option<&str>) -> usize {
    fonts.iter().position(|f| Some(f.name) == name).unwrap_or(0)
}

/// The name typed for a font row when it isn't one of `fonts` (an installed family); empty
/// for one of them or none.
pub fn custom_name(fonts: &[Font], name: Option<&str>) -> String {
    let name = name.map(str::trim).unwrap_or("");
    if fonts.iter().any(|f| f.name == name) { String::new() } else { name.to_owned() }
}

/// `name` in a CSS family list (Desktop's settings `family`): bare when it is one word,
/// quoted otherwise.
pub fn css_family(name: &str) -> String {
    if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
        return name.to_owned();
    }
    format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A Windows shell the user can turn OSC 133 prompt marks on for (Settings › Preferences).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TerminalMarkShell {
    Pwsh,
    PowerShell,
    Cmd,
    GitBash,
    Wsl,
}

impl TerminalMarkShell {
    pub const ALL: [Self; 5] = [Self::Pwsh, Self::PowerShell, Self::Cmd, Self::GitBash, Self::Wsl];

    /// Its id in the settings (`terminal.marks`).
    pub fn id(self) -> &'static str {
        match self {
            Self::Pwsh => "pwsh",
            Self::PowerShell => "powershell",
            Self::Cmd => "cmd",
            Self::GitBash => "gitbash",
            Self::Wsl => "wsl",
        }
    }

    /// Its row's key in Settings.
    pub fn setting_key(self) -> &'static str {
        match self {
            Self::Pwsh => "terminal-marks-pwsh",
            Self::PowerShell => "terminal-marks-powershell",
            Self::Cmd => "terminal-marks-cmd",
            Self::GitBash => "terminal-marks-gitbash",
            Self::Wsl => "terminal-marks-wsl",
        }
    }

    /// Its product name, the same in every language.
    pub fn label(self) -> &'static str {
        match self {
            Self::Pwsh => "PowerShell 7",
            Self::PowerShell => "Windows PowerShell",
            Self::Cmd => "cmd",
            Self::GitBash => "Git Bash",
            Self::Wsl => "WSL",
        }
    }
}
