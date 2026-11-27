//! Canonical palette display order for every built-in command (`App::palette_commands` /
//! `App::catalog`).
//!
//! Every [`opencode_kernel::CommandEntry`] constructor requires an explicit `order` value so no
//! built-in command can silently default to the end of the palette. Values are kept in one table,
//! spaced by 10, matching the pre-refactor `rune_register::collect` sequence at `31a59a9` (where
//! registration blocks sorted by module path: `add_selection`, `commands`, `connect`, `egg`,
//! `export_logs`, `mcp`, `text_rendering`, `usage`).
//!
//! # Placing a new built-in command
//!
//! 1. Pick the exact position where the command should appear in the command palette and add a
//!    `pub const` here with a value strictly between its neighbours (renumber by 10s if a gap is
//!    full so values stay strictly increasing and never rely on feature registration order).
//! 2. Pass `crate::commands::order::YOUR_COMMAND` to `CommandEntry::from_key` / `CommandEntry::new`
//!    in the owning feature's `Feature::register` (or in `register_shell_commands` for core shell
//!    and window commands).
//! 3. If the command is native-only (`#[cfg(not(target_arch = "wasm32"))]`), gate its constant here
//!    the same way so browser builds stay warning-free.
//!
//! Composer `/` popover order (`features/composer-input/src/slash.rs::ORDER`) and Settings ›
//! Shortcuts order (alphabetical per section in `features/settings/src/pages/shortcuts.rs`) sort
//! independently of these values.

pub const CONTEXT_ADD_SELECTION: u16 = 10;
pub const NEW: u16 = 20;
pub const HOME: u16 = 30;
pub const AUTOMATIONS: u16 = 35;
pub const BACK: u16 = 40;
pub const FORWARD: u16 = 50;
pub const AUTO_ACCEPT: u16 = 60;
pub const SIDEBAR: u16 = 70;
pub const SETTINGS: u16 = 80;
#[cfg(not(target_arch = "wasm32"))]
pub const OPEN_PROJECT: u16 = 90;
pub const SELECT_PROJECT: u16 = 100;
pub const SESSION_LOCATION_SELECT: u16 = 110;
pub const SESSION_BRANCH_CHOOSE: u16 = 120;
pub const SESSIONS: u16 = 130;
pub const QUICK_OPEN: u16 = 135;
pub const COMMANDS: u16 = 140;
pub const RELOAD: u16 = 150;
pub const COMPACT: u16 = 160;
pub const UNDO: u16 = 170;
pub const REDO: u16 = 180;
pub const FORK: u16 = 190;
pub const BACKGROUND: u16 = 200;
pub const RENAME: u16 = 210;
pub const TITLE_GENERATE: u16 = 215;
pub const EXPORT: u16 = 220;
pub const FILE_OPEN: u16 = 230;
pub const COPY_SESSION: u16 = 240;
pub const SESSION_ARCHIVE: u16 = 245;
pub const SESSION_UNARCHIVE: u16 = 247;
pub const SESSION_DELETE: u16 = 250;
pub const COPY_PROJECT: u16 = 260;
pub const SIDE: u16 = 270;
pub const SESSION_JUMP_TO_MESSAGE: u16 = 280;
pub const MESSAGE_PREVIOUS: u16 = 290;
pub const MESSAGE_NEXT: u16 = 300;
pub const MODEL: u16 = 310;
pub const VARIANT: u16 = 320;
pub const SPEED: u16 = 325;
pub const AGENT: u16 = 330;
pub const AGENT_BACK: u16 = 340;
pub const FIND: u16 = 350;
pub const FOCUS: u16 = 360;
pub const NEXT_TAB: u16 = 370;
pub const PREVIOUS_TAB: u16 = 380;
pub const PREVIOUS_UNREAD: u16 = 390;
pub const NEXT_UNREAD: u16 = 400;
pub const CLOSE_TAB: u16 = 410;
pub const CLOSE_SESSION_TAB: u16 = 420;
pub const REOPEN_TAB: u16 = 430;
#[cfg(not(target_arch = "wasm32"))]
pub const REOPEN_WINDOW: u16 = 440;
pub const ZOOM_IN: u16 = 450;
pub const ZOOM_OUT: u16 = 460;
pub const ZOOM_RESET: u16 = 470;
pub const FILE_ATTACH: u16 = 475;
pub const PROMPT_MODE_TOGGLE: u16 = 480;
pub const PROMPT_MODE_SHELL: u16 = 490;
pub const PROMPT_MODE_NORMAL: u16 = 500;
pub const PROMPT_STOP: u16 = 505;
pub const PROVIDER_CONNECT: u16 = 510;
pub const EGG_PLAY: u16 = 520;
#[cfg(not(target_arch = "wasm32"))]
pub const LOGS_EXPORT: u16 = 530;
pub const MCP_TOGGLE: u16 = 540;
pub const TEXT_RENDERING_CYCLE: u16 = 550;
