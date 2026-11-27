//! Shared virtualized session timeline: message rows, assistant turn folding, streamed line
//! reveal, disclosure glides, scroll follower, and edge fades (`p255`).

use opencode_kit::model::{self, Assistant, InboxItem, Message, Part, Transcript};
use rune::{El, GrowthSpec, ScrollSpec, col, edge, el, row, text};

use crate::TimelineHost;
use crate::compaction::Run;
use crate::fold::Fold;
use crate::palette::Pal;
use crate::style::UI;
use crate::turns::{self, Block, Entry, Step};

pub mod attachments;
mod delivery;
mod dividers;
pub mod parts;
mod projection;
pub mod rhythm;
mod span;

pub use dividers::divider_runs;
pub use parts::*;
pub use projection::{Projection, poll as projection_poll, rows, wanted as projection_wanted};

/// One timeline row's content, before it becomes a growth-stack child.
#[derive(Clone, PartialEq)]
pub struct RowSpec {
    pub key: String,
    pub content: RowContent,
    pub streaming: bool,
    /// The room it holds under what it shows, which sets the row below apart (`rhythm`).
    pub under: f32,
    /// A divider stands under it, this beside it above: the row holds the room over the
    /// divider's words too (`dividers`, `compaction::Over`).
    pub divider: Option<crate::compaction::Beside>,
}

#[derive(Clone, PartialEq)]
pub enum RowContent {
    /// `top`: the room over its bubble (`prompt_images`), which the row above holds (`rhythm`).
    User { message: usize, above: bool, below: bool, top: f32, fold: Fold },
    /// An undelivered steer: the bubble it becomes once delivered, muted and marked Pending,
    /// under the same row key so delivery keeps it in place.
    Pending { id: String, above: bool, below: bool, top: f32, fold: Fold },
    /// A switched agent or model, a move, updated instructions (`turn_errors`), and what sits
    /// beside it should it draw as a divider, and the room over it as a line (`rhythm`).
    Notice(usize, Run, f32),
    /// A background shell or subagent that came back, its message still waiting in the inbox:
    /// the notice it shows as once delivered, under the same row key, and the room over it.
    PendingNotice(String, f32),
    /// The turn's last step's error, or its Retry row while the server waits to try it again.
    Error { message: usize, pt: f32 },
    Retry { message: usize, pt: f32 },
    Compaction { message: usize, piece: crate::compaction::Piece, run: Run },
    /// A compaction the server admitted but hasn't started.
    CompactionQueued(Run),
    /// Between a stopped step and the steps the run went on with: Interrupted.
    TurnDivider(Run),
    TurnText { message: usize, part: usize, live: bool, pt: f32 },
    Turn { start: usize, end: usize, block: usize, live: bool, pt: f32 },
    /// `asking`: the session waits on a question or permission. `last`: it ends the timeline and
    /// nothing waits to send, so a finished reply's footer takes room of its own (`parts::turn_actions`).
    Actions { message: usize, busy: bool, asking: bool, last: bool },
    /// A prompt's images, and a tool's (by id, in messages `start..end`): built, and so
    /// fetched and decoded, only when the row mounts.
    UserImages { id: String, pt: f32 },
    ToolImages { start: usize, end: usize, tool: String },
    /// Changed files: the run that ended at message `idle` (its idle marker), from prompt
    /// `from`, which changed `files`.
    /// `pt`: the room over Changed files (`pt-4`, less what the reply's footer leaves under it).
    TurnDiff { idle: usize, from: String, files: Vec<String>, pt: f32 },
    /// Room at the timeline's end for the pills' lane, so its last row stands clear over them
    /// (t1129); a row, so the pinned timeline glides up as it opens rather than jumping.
    Lane(f32),
}

impl RowSpec {
    /// The room under what the row shows: to the row below, and over a divider's words.
    fn room_under<S: TimelineHost>(&self, s: &S) -> f32 {
        self.under + self.divider.map_or(0.0, |beside| crate::compaction::side(s, beside))
    }

    /// A row that folds open and shut under its header, keyed in `expanded` by its row key:
    /// a tool group, or the quiet Compacted divider's summary.
    fn folds(&self) -> bool {
        self.key.starts_with("c-") || matches!(self.content, RowContent::Compaction { piece: crate::compaction::Piece::Quiet, .. })
    }

    fn wrap<S: TimelineHost>(&self, s: &S, el: El) -> El {
        self.wrap_paced(s, None, el, false)
    }

    /// A prompt's row: [`Self::wrap`] with its turn heading first, so a reader reaches the
    /// heading, then the prompt.
    fn wrap_turn<S: TimelineHost>(&self, s: &S, turn: (usize, usize), more: bool, text: &str, bubble: El) -> El {
        if !s.assistive() {
            return self.wrap(s, bubble);
        }
        // Out of layout and drawing nothing: the row's top line, for a reader's heading keys.
        // The count and place are in its words, true whether the other turns are mounted or not.
        let heading = el()
            .key("turn-heading")
            .absolute(0.0, 0.0)
            .w_full()
            .h(1.0)
            .role("heading")
            .level(2)
            .label(turn_words(turn, more, text));
        self.wrap_paced(s, Some(heading), bubble, false)
    }

    /// [`Self::wrap`], with `paced` streaming rows climbing at the stream's pace rather than a
    /// line at a time: a reply with a fenced block in it, whose diagram, outline or code can
    /// grow by many lines in one build.
    fn wrap_paced<S: TimelineHost>(&self, s: &S, heading: Option<El>, el: El, paced: bool) -> El {
        // The room between two rows is the upper row's, under what it shows (`rhythm`).
        let row = col().w_full().max_w(crate::style::content_w()).pb(self.room_under(s)).children(heading).child(el);
        let row = if matches!(self.content, RowContent::User { .. } | RowContent::Pending { .. }) && s.smooth_timeline() {
            row.joined_row_face("bubble")
        } else { row };
        let k = s.knobs();
        if self.streaming && (paced || crate::line_reveal::reveal(k) == crate::line_reveal::Reveal::HeldRoom) {
            row.streaming(true)
        } else if self.streaming {
            // Streaming words open their row a line at a time, each line on its own quick spring
            // the moment it wraps, clipped at its moving edge: the line shows whole almost at once,
            // nothing paints over Working, and Working steps down by just that line, however
            // slowly the words come (p252, `line_reveal`).
            row.fold_glide(crate::line_reveal::line_grow(k)).fold_feather(crate::line_reveal::feather(k))
        } else if self.folds() {
            // A fold glides on its own quick spring, however long it is. Shutting, what it showed
            // hangs below the header until it unmounts, and Rune clips a row only while its height
            // moves: a short group lands first, and its steps flashed over the rows below (t1107).
            let row = if crate::disclosure::is_shutting(s, &self.key) { row.clip_soft(0.0) } else { row };
            row.fold_glide(crate::disclosure::fold(k)).fold_feather(k.get(&crate::disclosure::FOLD_FEATHER))
        } else {
            // Finished content opens in one quick glide, which a pinned timeline follows.
            row.arrives_whole()
        }
    }

    /// A reply's text part (part `part` of message `mi`), its words held back for Grow, then
    /// show (`line_reveal`). The text the timeline's last footer copies (`footer`) shares the
    /// footer's hover; the text that ends an earlier turn holds its own footer (`turn_footer`).
    #[allow(clippy::too_many_arguments)]
    fn text_row<S: TimelineHost>(&self, s: &S, p: &Pal, t: &Transcript, mi: usize, part: usize, body: &str, fade: f32, live: bool, pt: f32, footer: bool, keep_copy: bool) -> El {
        let message = t.messages[mi].id();
        let prompt = prompt_dir(t, mi);
        let has_turn_footer = !live && turn_footer_text(t, mi, part);
        let trim_bottom = if has_turn_footer || (!live && footer) { 0.0 } else { rhythm::prose_bottom_trim(body) };
        let md = |text: &str, fade: f32| {
            let el = markdown_view_find(s, p, &self.key, text, fade, live, Some(message), prompt);
            if trim_bottom > 0.0 { el.mb(-trim_bottom) } else { el }
        };
        // A burst paces in a slice at a time (`line_reveal::Paced`).
        let body = s.line_hold().paced(message, part).filter(|&n| body.is_char_boundary(n)).map_or(body, |n| &body[..n]);
        let words = crate::line_reveal::words(col().w_full().pt(pt), md, body, fade, s.line_hold().shown(message, part));
        if has_turn_footer {
            let words = words.child(turn_footer(s, p, t, mi, part, keep_copy));
            return self.wrap_paced(s, None, words, body.contains("```")).hover_group_focus().hover_motion(crate::style::INSTANT, crate::style::INSTANT);
        }
        let row = self.wrap_paced(s, None, words, body.contains("```"));
        if footer { row.hover_group_shared(REPLY_HOVER).hover_motion(crate::style::INSTANT, crate::style::INSTANT) } else { row }
    }

    pub fn build<S: TimelineHost>(&self, s: &S, p: &Pal, t: &Transcript, fade: f32, latest_copy: Option<usize>) -> El {
        let eager = EAGER_ROWS.get();
        let find_ver = s.find_row_version(&self.key);
        let k = s.knobs();
        if !eager {
            match &self.content {
                RowContent::TurnText { message, part, live, pt } => {
                    if let Message::Assistant(a) = &t.messages[*message]
                        && let Some(Part::Text { text: body }) = a.parts.get(*part)
                    {
                        // Only a reply with a diagram in it follows the column's width.
                        let dw = if body.contains("mermaid") { crate::genui::width(s).to_bits() } else { 0 };
                        let reveal = crate::line_reveal::reveal(k);
                        let line = (reveal, k.get(&crate::line_reveal::LINE_GROW).to_bits(), crate::line_reveal::feather(k).to_bits(), s.line_hold().shown(&a.id, *part), s.line_hold().paced(&a.id, *part));
                        // A code block's Copy shows its tick, its wrapping follows Settings and any
                        // per-block toggle, and math (display and inline) follows async completion;
                        // rows without fences read none of these.
                        let has_fence = body.contains("```") || body.contains("~~~");
                        let copied = has_fence.then(|| crate::copied::current(s));
                        let wrap = has_fence.then(|| {
                            let default = s.wrap_code_blocks();
                            let overrides = s
                                .code_wrap_overrides()
                                .filter(|m| !m.is_empty())
                                .map(|m| {
                                    // Its blocks' toggles: `<row>:<n>`, and in a reply with `ui` blocks
                                    // or videos `<row>-md-<start>:<n>` / `<row>-v-<start>:<n>`.
                                    let mut v: Vec<(String, bool)> = m
                                        .iter()
                                        .filter(|(k, _)| k.strip_prefix(self.key.as_str()).is_some_and(|rest| rest.starts_with(':') || rest.starts_with("-md-") || rest.starts_with("-v-")))
                                        .map(|(k, &on)| (k.clone(), on))
                                        .collect();
                                    v.sort();
                                    v
                                })
                                .unwrap_or_default();
                            (default, overrides)
                        });
                        // Display or inline math: the row's own version, which moves only when an
                        // equation it drew while that was still rendering finishes
                        // (`rune_kit::math::collect`).
                        let has_math = rune_kit::math::may_have_math(body);
                        if has_math && let Some(m) = s.math_client() {
                            m.hold();
                        }
                        let math_gen = has_math.then(|| rune_kit::math::version(&self.key));
                        // A picture landing (an image, a link's favicon), the viewer opening one, or
                        // the session's directory its paths read from becoming known rebuilds the row.
                        let footer = footer_text(t, *message, *part);
                        // The footer's Sources chip shows its sites' favicons with site icons on.
                        let favicons = footer && k.get(&crate::knobs::SOURCES_ENABLED) > 0.5 && k.get(&crate::knobs::SOURCES_ICONS) > 0.5;
                        let pictures = (body.contains("![") || body.contains("://") || favicons).then(|| {
                            let m = s.media_store();
                            let directory = s.active_session_id().and_then(|id| s.session_directory(id));
                            (m.markdown.version(), m.viewing.as_ref().map(|v| v.id.clone()), directory, favicons)
                        });
                        let keep_copy = latest_copy == Some(*message);
                        // An earlier turn's Copy tick, its metadata (which omits the model in a
                        // narrow window), and whether its Copy stays visible as the latest.
                        let own = turn_footer_text(t, *message, *part).then(|| (crate::copied::current(s), crate::style::narrow(), keep_copy));
                        // A session id it names being looked up, renamed, starting or stopping.
                        let chips = body.contains("ses_").then(|| crate::session_chips::version(s, body));
                        // A path its code names being found (`TimelineHost::code_links`); a streaming
                        // reply asks nothing, so its rows don't look.
                        let code = (!*live && body.contains('`')).then(|| s.code_links_version());
                        let videos = (!*live && opencode_kit::path::mentions_video(body)).then(|| (s.media_store().videos.version(), s.active_session_id().and_then(|id| s.session_directory(id))));
                        let scale_bits = crate::scroller::device_scale().to_bits();
                        let look = (p.theme, p.generation, p.dark, opencode_i18n::locale().id(), crate::style::hover_none());
                        let ver = (&t.session, body.as_str(), *live, [pt.to_bits(), self.room_under(s).to_bits()], fade.to_bits(), look, self.streaming, find_ver, dw, s.diagrams().hidden_in(body), line, (copied, wrap, math_gen, pictures, footer, crate::style::content_w().to_bits(), scale_bits, chips, own, (code, videos)));
                        return rune::memo(self.key.clone(), ver, || {
                            ROW_BUILDS.set(ROW_BUILDS.get() + 1);
                            if has_math {
                                return rune_kit::math::collect(&self.key, || self.text_row(s, p, t, *message, *part, body, fade, *live, *pt, footer, keep_copy));
                            }
                            self.text_row(s, p, t, *message, *part, body, fade, *live, *pt, footer, keep_copy)
                        });
                    }
                }
                RowContent::Turn { start, end, block, live, pt }
                    if self.key.starts_with("c-") && !s.expanded().contains(&self.key) && !crate::disclosure::is_shutting(s, &self.key) =>
                {
                    let fold = crate::disclosure::fold(k).duration.to_bits();
                    let detail = s.timeline_detail();
                    let script_diffs = s.full_codemode(&t.session);
                    // A group of thoughts alone is a thought row (`turns::group`), which opens by
                    // its first thought's key and not by the group's: the row is shut as a
                    // group and may be open as a thought, so the thoughts' toggles are part of it.
                    let thoughts = s.expanded().stamp("r-");
                    let look = (p.theme, p.generation, p.dark, opencode_i18n::locale().id(), crate::style::hover_none());
                    let ver = (&t.session, group_hash(&t.messages[*start..*end], &detail, script_diffs), *block, *live, [pt.to_bits(), self.room_under(s).to_bits()], look, self.streaming, (fold, crate::style::content_w().to_bits(), crate::scroller::device_scale().to_bits(), detail, script_diffs, s.shell_output_version(), crate::style::sticky_top().to_bits(), s.reduced_motion(), thoughts));
                    return rune::memo(self.key.clone(), ver, || {
                        ROW_BUILDS.set(ROW_BUILDS.get() + 1);
                        let blocks = turns::fold_with_flags(&entries(&t.messages[*start..*end], &detail), *live, &detail, script_diffs);
                        self.wrap(s, turns::block_row(s, p, &t.session, &blocks, *block, *start..*end, *live, fade, *pt, script_diffs))
                    });
                }
                RowContent::User { message, above, below, top, fold } => {
                    if let Message::User { id, text, .. } = &t.messages[*message]
                        && s.user_glow_at(id).is_none()
                        && !s.user_menu_open_on(id)
                        && model::user_comments(id).is_empty()
                        && opencode_kit::mention::user_mentions(id).is_empty()
                        && !text.contains("ses_")
                    {
                        let open = s.expanded().contains(&crate::fold::key(id));
                        let meta = prompt_meta(s, t, Some(*message));
                        // A turn's place is only for its heading, which only a reader gets.
                        let (turn, more) = if s.assistive() { (turn_of(t, *message), s.more_above(&t.session)) } else { ((0, 0), false) };
                        let look = (p.theme, p.generation, p.dark, opencode_i18n::locale().id(), crate::style::hover_none());
                        let ver = (&t.session, id.as_str(), text.as_str(), [*above, *below, fold.past, s.fold_user_messages(), open, self.streaming, s.smooth_timeline()], (look, fold.viewport.to_bits(), [top.to_bits(), self.room_under(s).to_bits()], crate::scroller::device_scale().to_bits()), find_ver, s.user_status_version(id), &meta, crate::style::content_w().to_bits(), turn, more, s.assistive());
                        return rune::memo(self.key.clone(), ver, || {
                            ROW_BUILDS.set(ROW_BUILDS.get() + 1);
                            let run = Place { above: *above, below: *below, top: *top, fold: *fold };
                            self.wrap_turn(s, turn, more, text, user_bubble(s, p, &t.session, id, text, run, 0.0, false, None, &meta))
                        });
                    }
                }
                _ => {}
            }
        }
        ROW_BUILDS.set(ROW_BUILDS.get() + 1);
        let el = match &self.content {
            RowContent::User { message, above, below, top, fold } => match &t.messages[*message] {
                Message::User { id, text, .. } => {
                    let glow = s.user_glow_at(id).map_or(0.0, |at| crate::knobs::glow(k, s.clock() - at));
                    let run = Place { above: *above, below: *below, top: *top, fold: *fold };
                    let bubble = user_bubble(s, p, &t.session, id, text, run, glow, false, crate::user_parts::quotes_on_message(s, p, id, text, false), &prompt_meta(s, t, Some(*message)));
                    if !s.assistive() {
                        return self.wrap(s, bubble);
                    }
                    return self.wrap_turn(s, turn_of(t, *message), s.more_above(&t.session), text, bubble);
                }
                _ => unreachable!(),
            },
            RowContent::Pending { id, above, below, top, fold } => {
                let item = s.inbox_for(&t.session).and_then(|i| i.items.iter().find(|i| &i.id == id));
                match item {
                    Some(item) => {
                        let cards = crate::user_parts::quotes_on_message(s, p, id, &item.text, true);
                        let label = if cards.is_some() { item.text.clone() } else { s.pending_steer_label(item) };
                        let run = Place { above: *above, below: *below, top: *top, fold: *fold };
                        user_bubble(s, p, &t.session, id, &label, run, 0.0, true, cards, &prompt_meta(s, t, None))
                    }
                    None => col(),
                }
            }
            RowContent::Notice(i, run, pt) => match &t.messages[*i] {
                Message::Notice(n) => col().w_full().pt(*pt).child(crate::turn_errors::notice(s, p, n, *run)),
                _ => unreachable!(),
            },
            RowContent::PendingNotice(id, pt) => match s.inbox_for(&t.session).and_then(|i| i.waiting_notices().find(|n| &n.id == id)) {
                Some(n) => col().w_full().pt(*pt).child(crate::turn_errors::notice(s, p, n, Run::default())),
                None => col(),
            },
            RowContent::Error { message, pt } => match &t.messages[*message] {
                Message::Assistant(a) => crate::turn_errors::error(s, p, a).pt(*pt),
                _ => unreachable!(),
            },
            RowContent::Retry { message, pt } => match &t.messages[*message] {
                Message::Assistant(Assistant { retry: Some(r), .. }) => crate::turn_errors::retry(s, p, r).pt(*pt),
                _ => col(),
            },
            RowContent::Compaction { message, piece, run } => match &t.messages[*message] {
                Message::Compaction(c) => crate::compaction::view(s, p, c, *piece, *run),
                _ => unreachable!(),
            },
            RowContent::CompactionQueued(run) => crate::compaction::queued(s, p, *run),
            RowContent::TurnDivider(run) => crate::compaction::divider_of(s, p, text(opencode_i18n::t!("ui.message.interrupted")).key("label").font(UI).color(p.faint), *run),
            RowContent::Lane(h) => el().w_full().h(*h),
            RowContent::TurnDiff { idle, from, files, pt } => crate::turn_diff::view(s, p, &t.session, t.messages[*idle].id(), from, files, *pt),
            // Held room keeps a line of room over Working, which streaming text draws into (#304).
            RowContent::Actions { message, busy, asking, last } => col().w_full().pt(crate::line_reveal::room(s)).child(turn_actions(s, p, *busy, *asking, *last, &t.session, *message, &t.messages[*message])),
            RowContent::UserImages { id, pt } => attachments::view(s, p, id).map_or_else(col, |a| col().w_full().child(a)).pt(*pt),
            RowContent::ToolImages { start, end, tool } => {
                let files = t.messages[*start..*end].iter().find_map(|m| match m {
                    Message::Assistant(a) => a.parts.iter().find_map(|part| match part {
                        Part::Tool { id, files, .. } if id == tool => Some(files),
                        _ => None,
                    }),
                    _ => None,
                });
                files.and_then(|f| crate::images::gallery(s, p, &format!("tool-images-{tool}"), f, false, 0.0, None)).unwrap_or_else(col).pt(8.0)
            }
            RowContent::TurnText { message, part, live, pt } => match &t.messages[*message] {
                Message::Assistant(a) => match &a.parts[*part] {
                    Part::Text { text: body } => return self.text_row(s, p, t, *message, *part, body, fade, *live, *pt, footer_text(t, *message, *part), latest_copy == Some(*message)),
                    _ => unreachable!(),
                },
                _ => unreachable!(),
            },
            RowContent::Turn { start, end, block, live, pt } => {
                let detail = s.timeline_detail();
                let script_diffs = s.full_codemode(&t.session);
                let blocks = turns::fold_with_flags(&entries(&t.messages[*start..*end], &detail), *live, &detail, script_diffs);
                match blocks.get(*block) {
                    Some(Block::Text { key, body, live, msg, part }) => {
                        let message = key.strip_prefix("t-").and_then(|k| k.rsplit_once('-')).map(|(id, _)| id);
                        let has_turn_footer = !*live && turn_footer_text(t, start + msg, *part);
                        let trim_bottom = if has_turn_footer || (!*live && footer_text(t, start + msg, *part)) { 0.0 } else { rhythm::prose_bottom_trim(body) };
                        let md_el = markdown_view_find(s, p, &self.key, body, fade, *live, message, prompt_dir(t, start + msg));
                        let text = col().w_full().pt(*pt).child(if trim_bottom > 0.0 { md_el.mb(-trim_bottom) } else { md_el });
                        if has_turn_footer {
                            return self.wrap(s, text.child(turn_footer(s, p, t, start + msg, *part, latest_copy == Some(start + msg)))).hover_group_focus().hover_motion(crate::style::INSTANT, crate::style::INSTANT);
                        }
                        text
                    }
                    _ => turns::block_row(s, p, &t.session, &blocks, *block, *start..*end, *live, fade, *pt, script_diffs),
                }
            }
        };
        self.wrap(s, el)
    }
}

/// A turn's messages as its entries, one for each: its steps, the notices that fold with its
/// work, and what it passes over.
pub fn entries<'a>(messages: &'a [Message], detail: &crate::detail::Detail) -> Vec<Entry<'a>> {
    messages.iter().map(|m| match m {
        Message::Assistant(a) => Entry::Step(a),
        Message::Note { step, .. } => Entry::Step(step),
        Message::Notice(n) if n.kind == model::NoticeKind::Idle => Entry::Idle,
        Message::Notice(n) if crate::detail::notice_folds(n, detail) => Entry::Notice(n),
        _ => Entry::Skip,
    }).collect()
}

fn group_hash(messages: &[Message], detail: &crate::detail::Detail, script_diffs: bool) -> u64 {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut h = DefaultHasher::new();
    let edits = script_diffs && detail.placement(crate::detail::Category::Edit) != crate::detail::Placement::Hidden;
    for m in messages {
        if let Message::Notice(n) = m {
            (3u8, &n.id).hash(&mut h);
        }
        let step = match m {
            Message::Assistant(a) => Some(a),
            Message::Note { step, .. } => Some(&**step),
            _ => None,
        };
        if let Some(a) = step {
            (&a.id, a.completed, a.parts.len(), &a.text_phases).hash(&mut h);
            for p in &a.parts {
                match p {
                    Part::Tool { id, name, status, metadata, .. } => {
                        (0u8, id, name, std::mem::discriminant(status)).hash(&mut h);
                        // A script's changes name the group as they arrive; progress that adds
                        // none keeps the row.
                        if edits {
                            crate::activity::rules::script_changes(name, status, metadata).for_each(|(i, f)| (i, f["file"].as_str()).hash(&mut h));
                        }
                        if script_diffs && name == "execute" {
                            metadata["toolCalls"].as_array().into_iter().flatten().for_each(|c| {
                                (c["tool"].as_str(), c["status"].as_str(), c["input"]["method"].as_str(), c["input"]["path"].as_str()).hash(&mut h);
                            });
                        }
                    }
                    Part::Reasoning { text, completed, .. } => (1u8, text.len(), completed).hash(&mut h),
                    Part::Text { text } => (2u8, text.len()).hash(&mut h),
                }
            }
        }
    }
    h.finish()
}

/// A turn heading's words take at most this many characters of the prompt.
const TURN_WORDS: usize = 80;

/// What a reader hears of a turn: "Prompt 3 of 12: " and the prompt's first line, cut to
/// [`TURN_WORDS`] characters. Its heading's name, and what Previous and Next message say. While
/// older messages aren't loaded (`more`), the count from the start isn't known, so the place is
/// counted from the latest prompt instead ("Prompt 3 from the end: "), which stays true.
pub fn turn_words((turn, turns): (usize, usize), more: bool, text: &str) -> String {
    let first: String = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").chars().take(TURN_WORDS).collect();
    if more {
        return opencode_i18n::t!("rune.a11y.turnFromEnd", back = turns + 1 - turn, text = first);
    }
    opencode_i18n::t!("rune.a11y.turn", turn = turn, turns = turns, text = first)
}

/// Prompt `message`'s place among the transcript's prompts, 1-based, and how many there are,
/// for its turn heading. Counted once per transcript as it stands (its session, length and last
/// message), not per row build.
pub fn turn_of(t: &Transcript, message: usize) -> (usize, usize) {
    type TurnsKey = (String, usize, Option<String>);
    type TurnsCache = (Option<TurnsKey>, Vec<usize>);
    thread_local! {
        static TURNS: std::cell::RefCell<TurnsCache> = const { std::cell::RefCell::new((None, Vec::new())) };
    }
    let last = t.messages.last().map(|m| m.id());
    TURNS.with(|c| {
        let mut c = c.borrow_mut();
        let same = c.0.as_ref().is_some_and(|(s, n, l)| *s == t.session && *n == t.messages.len() && l.as_deref() == last);
        if !same {
            c.1 = t.messages.iter().enumerate().filter(|(_, m)| matches!(m, Message::User { .. })).map(|(i, _)| i).collect();
            c.0 = Some((t.session.clone(), t.messages.len(), last.map(str::to_owned)));
        }
        (c.1.partition_point(|&i| i < message) + 1, c.1.len())
    })
}

thread_local! {
    pub static ROW_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub static EAGER_ROWS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub fn timeline_reset_row_builds() {
    ROW_BUILDS.set(0);
}

pub fn timeline_row_builds() -> usize {
    ROW_BUILDS.get()
}

/// Desktop's `tailSize`, the room under the timeline's last row (`virtualizer.tsx:152`, its
/// `paddingEnd`): the Working lane at its top, then the gap over the composer. The turn's
/// actions row (`parts::turn_actions`) holds that lane, so the padding is the rest, and stays
/// put: steers waiting under the actions row end the timeline with the gap alone.
const TAIL: f32 = 92.0;

/// The timeline's rows: the projection's, then the pills' lane under them while it shows.
#[derive(Clone)]
struct Shown {
    rows: std::rc::Rc<projection::Projection>,
    lane: Option<std::rc::Rc<RowSpec>>,
}

impl Shown {
    fn len(&self) -> usize {
        self.rows.len() + self.lane.is_some() as usize
    }

    fn get(&self, i: usize) -> Option<&RowSpec> {
        self.rows.get(i).or_else(|| (i == self.rows.len()).then_some(self.lane.as_deref()).flatten())
    }
}

/// The timeline, running down to the composer under the pills' `lane` (`jump::over`): at its end
/// its last row stands over the lane, clear of the fade into the composer. A session `header`
/// scrolls with it and sticks over its top, as Desktop's (`virtualizer.tsx:1157-1177`, the
/// scroll view, then the header, then the rows): rows scroll under it and fade out over the
/// 16 points below it (`session-identity-header.tsx:50-66`), and the rows' own headers stick
/// under it.
pub fn timeline<S: TimelineHost>(s: &S, p: &Pal, id: &str, t: Option<&Transcript>, lane: f32, header: Option<El>) -> El {
    // A place coming back (`crate::returning`) lands in the commit whose transcript has its row,
    // the one that first draws the rows: until then the timeline keeps its loading look rather
    // than draw its end and then move to the place.
    let t = t.filter(|_| !s.ends().due(id));
    let sticky = if header.is_some() { crate::style::HEADER_H } else { 0.0 };
    crate::style::set_sticky_top(sticky);
    // Held room's streaming text draws past its row's edge into the room over Working.
    let held_room = crate::line_reveal::reveal(s.knobs()) == crate::line_reveal::Reveal::HeldRoom;
    let e = rune::growth::StreamEdge { overflow: held_room, ..edge::FEATHER };
    // The pills' lane ends the rows, apart from the projection it doesn't change.
    let specs = Shown {
        rows: t.map(|t| rows(s, t)).unwrap_or_default(),
        lane: (t.is_some() && lane > 0.0).then(|| std::rc::Rc::new(RowSpec { key: "pill-lane".into(), content: RowContent::Lane(lane), streaming: false, under: 0.0, divider: None })),
    };
    let tail_extra = t.map_or(0.0, |t| rhythm::tail_extra(t, &s.timeline_detail(), s.full_codemode(&t.session), &specs.rows));
    let mut stack = col()
        .key("timeline")
        .w_full()
        .center()
        .px(crate::style::sides() / 2.0)
        .pt(crate::style::timeline_top())
        .pb(((if crate::style::hover_none() { 104.0 } else { TAIL }) + tail_extra - crate::working::LANE).max(crate::jump::fade(s, 0.0)))
        // One selection across the whole timeline, as a page's: a drag runs from a prompt
        // through tools and replies, Cmd+A takes every message, and caret browsing lets
        // Shift+Arrow / Arrow / Home / End extend or move it (out of Tab order, no ring).
        .selection_group(true)
        .caret_browsing(true)
        .tab_index(-1).no_focus_ring()
        .on_selection_change(S::on_timeline_selection_change)
        // While a panel's spring moves the timeline's edge, rows take the width they land at,
        // so their text wraps once, and glide from where the first in view was
        // (`virtualizer.tsx:254-268`, `panel-motion.ts` `holdLandingWidth`).
        .hold_landing_width()
        // Rows stack on whole points, as Desktop's virtualizer rounds each row's measured
        // height and places it at the running total (`virtualizer.tsx:362,370,1081`).
        .whole_rows()
        .growth(GrowthSpec { spring: crate::knobs::growth(s.knobs()), edge: e });
    let eager = EAGER_ROWS.get();
    // Where it's drawn, for its place when it shows again (`recall`).
    let marks = s.ends().recall.marks(id);
    if t.is_some() {
        let (marks, keys) = (marks.clone(), specs.clone());
        stack = stack.on_visible_rows(move |_: &mut S, shown| {
            marks.shown((shown.start.min(keys.len())..shown.end.min(keys.len())).filter_map(|i| keys.get(i).map(|r| r.key.clone())));
            false
        });
    }
    let latest_copy = t.and_then(|t| parts::latest_copy_message(s, t));
    // Eager construction is the benchmark and regression-test reference.
    if eager && let Some(t) = t {
        stack = stack.virtualize(80.0)
            .children((0..specs.len()).filter_map(|i| specs.get(i)).map(|r| marks.row(&r.key, r.build(s, p, t, e.word_fade, latest_copy).key(r.key.clone()))));
    } else if t.is_some() {
        let keys = specs.clone();
        let id = id.to_owned();
        let p = *p;
        let n = specs.len();
        let marks = marks.clone();
        // A projection kept from the last build has the keys it had: the list needn't ask each
        // row's again.
        let version = (specs.rows.generation, specs.lane.is_some());
        stack = stack.virtual_rows(
            n,
            80.0,
            move |_: &S, i| keys.get(i).map_or_else(|| "".into(), |r| r.key.clone().into()),
            move |s: &S, i| {
                // Rows build after this returns, maybe beside another timeline's.
                crate::style::set_sticky_top(sticky);
                let Some(r) = specs.get(i) else { return col() };
                // A row that leaves the mounted range takes its focused control with it: the
                // viewport takes focus then, without scrolling, so the scroll keys keep working
                // (official `session/timeline/virtualizer.tsx`, opencode#65921), not a control
                // somewhere else.
                marks.row(&r.key, s.with_transcript(&id, |t| r.build(s, &p, t, e.word_fade, latest_copy)).unwrap_or_else(col)).focus_return(format!("scroll-{id}"))
            },
        ).virtual_rows_version(version);
    }
    if t.is_none() {
        if let Some(head)=s.partial_output(id) {
            let copy_label = format!("{} · {}", opencode_i18n::t!("ui.message.copy"), opencode_i18n::t!("plugin.file.view.preview"));
            let session=id.to_owned();
            stack=stack.child(col().key("output-preview").w_full().max_w(crate::style::content_w()).padding(12.0).gap(4.0).fill(p.bg_base)
                .child(text(opencode_i18n::t!("plugin.file.view.preview")).font(crate::style::UI_MED).color(p.text))
                .child(crate::scroller::scroller("output-head").hidden(col().w_full().max_h(240.0),crate::tools::preview_words::<S>(p,head)))
                .child(row().key("copy-preview").self_start().role("button").focusable(true).label(copy_label.clone()).px(8.0).min_h(28.0).center().radius(6.0).hover(p.hover).fill(p.layer2)
                    .on_press_tx(move |s:&mut S,tx|{if let Some(part)=s.partial_output(&session){tx.copy(part.to_owned());}})
                    .child(text(copy_label).font(UI).color(p.text))));
        }
        // Official's session page shows only its header while the messages load: no spinner, no
        // words, at any delay (`session/screen.tsx:451-454`). A read that failed says why, with Retry.
        if let Some(why) = s.loading_error(id) {
            let session = id.to_owned();
            stack = stack.child(
                row()
                    .key("loading")
                    .w_full()
                    .max_w(crate::style::content_w())
                    .h(40.0)
                    .gap(8.0)
                    .center()
                    .child(text(why).font(UI).color(p.faint))
                    .child(
                        row().key("loading-retry").role("button").focusable(true).label_static(opencode_i18n::t!("common.retry")).px(8.0).min_h(28.0).center().radius(6.0).hover(p.hover).fill(p.layer2)
                            .on_press_tx(move |s: &mut S, tx: &mut rune::Tx| s.retry_loading(&session, tx))
                            .child(text(opencode_i18n::t!("common.retry")).font(UI).color(p.text)),
                    ),
            );
        }
    }
    // Its edges fade only while rows are under them, and never over a stuck group header. Under
    // a session header the top one is Desktop's `session-header-fade`: a 16 pt linear ramp of
    // the page's colour under the header's bottom edge, whole as soon as rows go under it; the
    // bottom one is Desktop's 20 px 4-stop piecewise-linear viewport mask (`packages/app/src/index.css:96-107`:
    // 0 -> 0.16 @ 5px -> 0.50 @ 10px -> 0.84 @ 15px -> 1.0 @ 20px).
    // Without a session header (narrow windows, `virtualizer.tsx:1182-1208`), official has only
    // the bottom 20 px fade (`end_only`) and never fades rows under the narrow location bar.
    let bottom_fade = rune::ScrollFade::strength(20.0).stops(&[(0.25, 0.16), (0.5, 0.5), (0.75, 0.84)]);
    let fade = match header {
        Some(_) => rune::ScrollFade::strength(16.0).linear().full_at(0.01).below_sticky().with_end(bottom_fade),
        None => bottom_fade.end_only(),
    };
    let mut scroll = marks.view(col())
        .key(format!("scroll-{id}"))
        .grow(1.0)
        .shrink()
        .w_full()
        .scroll(ScrollSpec { stick_bottom: true })
        .scroll_fade(fade)
        // It scrolls with the keys once a click focuses it but is no Tab stop, as Desktop's
        // `ScrollView` (`tabIndex={-1}` and its own `onKeyDown`, `scroll-view.tsx`,
        // opencode#65758), with no ring: the rows inside show focus (opencode#65690).
        .focusable(true)
        .tab_index(-1)
        .role("region")
        .label_static(opencode_i18n::t!("ui.scrollView.ariaLabel"))
        .no_focus_ring()
        .on_key(S::on_timeline_key)
        // `sticky top-0 z-30` (`session-identity-header.tsx:54`). It needs no fill of its own:
        // rows under it clip at its bottom edge, where their fade starts (`below_sticky`), so
        // the panel's corners and ring show as they do without it.
        .children(header.map(|h| h.sticky_top(0.0)))
        // With older messages still to land above, the rows stand at the viewport's end even while
        // they are shorter than it (a restored session's saved last turns in a tall window, a
        // newest page of short messages): where they stay once the rest is above them. Without
        // this they started at the top and moved down by what the viewport had left (70 pt in a
        // 1200 px window) when the server's page landed. A whole short session starts at the top.
        .children(s.more_above(id).then(|| el().key("above").grow(1.0)))
        .child(stack);
    // Near its top, older messages load above what's on screen.
    if let Some(top) = s.watch_scroll_top(id) {
        scroll = scroll.on_scroll_offset(top);
    }
    scroll
}

/// Transcript `t`'s rows, top to bottom, projected afresh ([`rows`] keeps them while what they
/// read holds).
pub(crate) fn project<S: TimelineHost>(s: &S, t: &Transcript) -> Vec<RowSpec> {
    let inputs = span::Inputs::read(s, t);
    span::project(s, t, &inputs, 0..inputs.shown, true)
}

/// The first message of transcript `t`'s row keyed `key`, if it has one.
pub fn row_message<S: TimelineHost>(s: &S, t: &Transcript, key: &str) -> Option<usize> {
    let rows = rows(s, t);
    let r = rows.iter().find(|r| r.key == key)?;
    match &r.content {
        RowContent::User { message, .. } | RowContent::Error { message, .. } | RowContent::Retry { message, .. } | RowContent::Compaction { message, .. } | RowContent::TurnText { message, .. } | RowContent::Actions { message, .. } => Some(*message),
        RowContent::Notice(i, ..) => Some(*i),
        RowContent::Turn { start, .. } | RowContent::ToolImages { start, .. } => Some(*start),
        RowContent::TurnDiff { idle, .. } => Some(*idle),
        RowContent::UserImages { id, .. } => t.messages.iter().position(|m| m.id() == id),
        RowContent::Pending { .. } | RowContent::PendingNotice(..) | RowContent::CompactionQueued(_) | RowContent::TurnDivider(_) | RowContent::Lane(_) => None,
    }
}

/// Whether transcript `t` has a row keyed `key`.
pub fn has_row<S: TimelineHost>(s: &S, t: &Transcript, key: &str) -> bool {
    rows(s, t).iter().any(|r| r.key == key)
}

/// The keys of a transcript's rows, top to bottom.
pub fn row_keys<S: TimelineHost>(s: &S, t: &Transcript) -> Vec<String> {
    rows(s, t).iter().map(|r| r.key.clone()).collect()
}

/// A prompt's attachments (`attachments`), in a row 8 above its bubble. Returns the room over
/// its bubble: 4 in a run (`above`), 8 under its pictures or where it starts the session, else
/// 16 (`rhythm` knows what it stands under). With `older` messages still to land above it
/// doesn't start the session.
pub fn prompt_images(out: &mut Vec<RowSpec>, id: &str, older: bool, above: bool) -> f32 {
    let start = out.is_empty() && !older;
    let gallery = attachments::any(id);
    if gallery {
        let pt = if start { 8.0 } else { 16.0 };
        out.push(RowSpec { key: format!("ui-{id}"), content: RowContent::UserImages { id: id.to_owned(), pt }, streaming: false, under: 0.0, divider: None });
    }
    if above { 4.0 } else if start || gallery { 8.0 } else { 16.0 }
}

/// Space above a turn's row (`rhythm` respaces it under a row it knows), as Desktop spaces assistant parts (`session-timeline-row.tsx:696-700`):
/// none under the prompt but a text part's own 24 (`message-part.css:569-571`), 8 between two
/// tools, 16 before anything else.
pub fn part_gap(prev: Option<bool>, tool: bool) -> f32 {
    match prev {
        None if !tool => 24.0,
        None => 0.0,
        Some(true) if tool => 8.0,
        Some(_) => 16.0,
    }
}

/// A turn's rows, `steps` being its messages `start..end` one for one. `live`: these steps end
/// the transcript and the last one is still running.
pub fn turn_rows(out: &mut Vec<RowSpec>, steps: &[Entry], start: usize, end: usize, live: bool, detail: &crate::detail::Detail, script_diffs: bool) {
    let blocks = turns::fold_with_flags(steps, live, detail, script_diffs);
    // Whether the row above was a tool; `None` above the turn's first row.
    let mut prev: Option<bool> = None;
    for (i, b) in blocks.into_iter().enumerate() {
        let tool = !matches!(b, Block::Text { .. });
        let pt = part_gap(prev, tool);
        let content = RowContent::Turn { start, end, block: i, live, pt };
        match b {
            Block::Group { key, steps, .. } => {
                // An open group's steps arrive whole: as a streaming row, opening one paced its
                // height at text-reveal speeds (seconds for a long group) and shutting snapped.
                out.push(RowSpec { streaming: false, content, key, under: 0.0, divider: None });
                // An image a tool returned shows even while its group is closed.
                for st in &steps {
                    if let Step::Tool { id, files, .. } = st
                        && crate::images::shows(files)
                    {
                        let content = RowContent::ToolImages { start, end, tool: id.to_string() };
                        out.push(RowSpec { key: format!("ti-{id}"), content, streaming: false, under: 0.0, divider: None });
                    }
                }
            }
            Block::Text { key, msg, part, live, .. } => {
                let content = if end - start == steps.len() { RowContent::TurnText { message: start + msg, part, live, pt } } else { content };
                out.push(RowSpec { content, key, streaming: live, under: 0.0, divider: None });
            }
            Block::Events { key, .. } | Block::Subagent { key, .. } => out.push(RowSpec { key, content, streaming: false, under: 0.0, divider: None }),
            Block::Question { id, .. } => out.push(RowSpec { key: format!("q-{id}"), content, streaming: false, under: 0.0, divider: None }),
            Block::Thought { key, .. } | Block::Calls { key, .. } => out.push(RowSpec { key, content, streaming: false, under: 0.0, divider: None }),
        }
        prev = Some(tool);
    }
    // Like Desktop, only the turn's last step reports its error, 24 below the work above it,
    // and only the running one its retry.
    let Some((at, a)) = steps.iter().enumerate().rev().find_map(|(i, e)| match e {
        Entry::Step(a) => Some((i, *a)),
        _ => None,
    }) else { return };
    let message = start + at;
    let pt = if prev.is_some() { 24.0 } else { 0.0 };
    if live && a.retry.is_some() {
        out.push(RowSpec { key: format!("r-{}", a.id), content: RowContent::Retry { message, pt }, streaming: false, under: 0.0, divider: None });
    } else if a.error.is_some() && !a.failure.as_ref().is_some_and(|f| interrupted(&f.kind)) {
        // 24 below what's above it (`pt-6`), until `rhythm` respaces it.
        out.push(RowSpec { key: format!("e-{}", a.id), content: RowContent::Error { message, pt: 24.0 }, streaming: false, under: 0.0, divider: None });
    }
}

/// An abort or interruption, which Desktop doesn't report as an error: the turn's tail says
/// Interrupted instead (`parts::turn_actions`).
pub fn interrupted(kind: &str) -> bool {
    let k = kind.to_lowercase();
    k.contains("abort") || k.contains("interrupt")
}

/// The direction of the prompt message `mi` answers (the last prompt at or before it), which its
/// reply reads in until its own words arrive.
fn prompt_dir(t: &Transcript, mi: usize) -> Option<rune::Direction> {
    let upto = (mi + 1).min(t.messages.len());
    t.messages[..upto].iter().rev().find_map(|m| match m {
        Message::User { text, .. } => Some(rune::Direction::of(text)),
        _ => None,
    })?
}
