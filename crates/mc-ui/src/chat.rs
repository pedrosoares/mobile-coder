//! The chat screen.

use std::sync::Arc;

use freya::prelude::*;
use mc_core::{AgentHandle, ChatItem, ChatLog, SessionEntry, SessionLibrary, ToolState};

use crate::theme;

/// How much of a tool's output to show inline. The full output went to the
/// model; a phone screen needs the gist.
const TOOL_PREVIEW_LINES: usize = 6;
/// Thinking is context, not content - keep it to a glance.
const THINKING_PREVIEW_CHARS: usize = 280;
/// How far from the bottom still counts as being at the bottom.
///
/// Slack rather than an exact match, because the two measurements that decide
/// it - the content and the viewport - arrive in separate events, so for a
/// frame or two they disagree by a pixel or two.
const FOLLOW_SLACK: f32 = 8.0;

/// What the transcript measures, and where its bottom is.
///
/// A chat follows the newest text, but not at the cost of the reader: scrolling
/// up to check an earlier command has to *stay* up, even while a reply streams
/// in below. So the rule is the one every chat uses - keep to the bottom while
/// the reader is at the bottom, and stop the moment they are not - and both
/// halves need to know how tall the content is against how much of it fits.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct Transcript {
    /// Every message, end to end.
    content: f32,
    /// How much of it is on screen.
    viewport: f32,
}

impl Transcript {
    /// The scroll position that puts the last line at the bottom edge.
    ///
    /// Negative, because scrolling down moves the content up. Zero when
    /// everything fits, which is also when there is nothing to follow.
    fn bottom(self) -> f32 {
        -(self.content - self.viewport).max(0.0)
    }
}

/// Where to scroll to when the transcript changes size, if anywhere.
///
/// `y` is the current scroll position, `previous` the sizes it was measured
/// against, `measured` the new ones. `None` means leave the view alone: either
/// the reader had scrolled up - their position is theirs to keep - or the view
/// is already where it should be.
///
/// That last case matters more than it looks: scrolling to a position the view
/// already holds still lays out again, which measures again, which would scroll
/// again, and the frame never settles.
fn follow_target(y: f32, previous: Transcript, measured: Transcript) -> Option<f32> {
    if y - previous.bottom() > FOLLOW_SLACK {
        return None;
    }
    let bottom = measured.bottom();
    (y > bottom).then_some(bottom)
}

/// How often the status line refreshes the things that change outside Freya:
/// the "Copied" message and the background job count.
const STATUS_POLL: std::time::Duration = std::time::Duration::from_millis(250);

/// The chat pane.
///
/// A [`Component`], not a plain function, so its hooks live in their own scope.
/// Called as a function from the root, its hooks belonged to the root's scope -
/// and switching panes swapped them for another pane's, which Freya rejects with
/// "hook functions cannot be called conditionally" (seen on the emulator).
///
/// `native_composer`: the platform draws the message box (see
/// [`crate::MobileCoder::native_composer`]), so only the transcript and status
/// line are rendered here.
pub struct ChatView {
    pub agent: Option<AgentHandle>,
    pub native_composer: bool,
    /// Where the chats are kept, for the picker to list and delete.
    pub library: Option<Arc<SessionLibrary>>,
    /// Whether the picker is open. Owned by the root, so it survives a look at
    /// another tab.
    pub picking: State<bool>,
    /// Owned by the app root, so switching panes neither clears the
    /// conversation nor stops it recording. See [`crate::MobileCoder`].
    pub log: State<ChatLog>,
    /// Likewise the half-typed message.
    pub draft: State<String>,
}

impl PartialEq for ChatView {
    fn eq(&self, other: &Self) -> bool {
        // The handle is fixed for the life of the app; these can differ.
        self.native_composer == other.native_composer
            && self.log == other.log
            && self.draft == other.draft
            && self.picking == other.picking
    }
}

impl Component for ChatView {
    fn render(&self) -> impl IntoElement {
        chat_body(
            self.agent.clone(),
            self.native_composer,
            self.log,
            self.draft,
            self.library.clone(),
            self.picking,
        )
    }
}

/// Load the transcript of whichever chat the worker just opened.
///
/// The worker owns which session is current; this turns that into what is on
/// screen. Spawned from the app root for the same reason as [`collect_events`]:
/// a chat opened while the user is on another tab still has to be there when
/// they come back.
pub fn follow_open_chats(
    agent: Option<AgentHandle>,
    library: Option<Arc<SessionLibrary>>,
    mut log: State<ChatLog>,
) {
    let (Some(handle), Some(library)) = (agent, library) else { return };
    let mut events = handle.subscribe();
    spawn_forever(async move {
        loop {
            match events.recv().await {
                Ok(mc_core::Event::SessionOpened { session }) => {
                    let restored = library
                        .load(session)
                        .map(|session| ChatLog::from_session(&session))
                        .unwrap_or_default();
                    log.set(restored);
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

/// Collect agent events into `log` for as long as the app runs.
///
/// Spawned from the app root, not from the chat pane: a task spawned in a pane
/// is cancelled when that pane goes away, so a reply arriving while the user was
/// on another tab used to be lost outright.
pub fn collect_events(agent: Option<AgentHandle>, mut log: State<ChatLog>) {
    let Some(handle) = agent else { return };
    let mut events = handle.subscribe();
    spawn_forever(async move {
        loop {
            match events.recv().await {
                Ok(event) => log.write().apply(event),
                // Fell behind a burst of deltas. Keep going: missing a few
                // fragments beats freezing the chat.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

fn chat_body(
    agent: Option<AgentHandle>,
    native_composer: bool,
    mut log: State<ChatLog>,
    mut draft: State<String>,
    library: Option<Arc<SessionLibrary>>,
    picking: State<bool>,
) -> impl IntoElement {
    let mut scroll = use_scroll_controller(|| ScrollConfig {
        default_vertical_position: ScrollPosition::End,
        ..Default::default()
    });

    // Follow the conversation as it grows - see `Transcript`.
    let mut transcript_size = use_state(Transcript::default);
    let mut follow = move |measured: Transcript| {
        let previous = *transcript_size.peek();
        if measured == previous {
            return;
        }
        // Where the view is now. Scroll positions run from 0 at the top down to
        // `bottom`, which is negative.
        let (_, y) = <(i32, i32)>::from(scroll);
        transcript_size.set(measured);
        if let Some(bottom) = follow_target(y as f32, previous, measured) {
            scroll.scroll_to_y(bottom as i32);
        }
    };

    let busy = log.read().busy;
    let can_send = agent.is_some() && !busy;
    // While a turn runs the button stops it instead: on a phone, force-quitting
    // is otherwise the only way out of a turn going the wrong way.
    let stop_handle = agent.clone();

    let submit_handle = agent.clone();
    let mut send = move |text: String| {
        let text = text.trim().to_string();
        if text.is_empty() || log.read().busy {
            return;
        }
        if let Some(handle) = &submit_handle {
            if handle.submit(text) {
                // Sending is a decision to watch the reply: go to the bottom
                // even if the reader had scrolled up to check something.
                let bottom = transcript_size.peek().bottom();
                scroll.scroll_to_y(bottom as i32);
                // Clear on a later tick, never inside the handler. The Input
                // calling `on_submit` may still hold a borrow of `draft`, and
                // writing to a borrowed State panics - on Android that panic
                // takes the whole window down (seen on the emulator).
                spawn(async move { draft.set(String::new()) });
            } else {
                log.write().items.push(ChatItem::Error(
                    "The agent stopped running. Restart the app.".into(),
                ));
            }
        }
    };

    let items: Vec<ChatItem> = log.read().items.clone();

    let transcript = ScrollView::new_controlled(scroll)
        .width(Size::fill())
        .height(Size::flex(1.))
        // The viewport: what the reader can see at once. It shrinks when the
        // keyboard opens, which is a moment the chat has to follow too.
        .on_sized(move |e: Event<SizedEventData>| {
            // Read into a local first. A temporary lives to the end of its
            // statement, so measuring inside the call would hold `peek`'s guard
            // open while `follow` writes to the same State - and writing to a
            // borrowed State panics on the render thread, which on Android
            // takes the window down with it (seen on the emulator).
            let measured = Transcript { viewport: e.area.height(), ..*transcript_size.peek() };
            follow(measured);
        })
        .child(
            rect()
                .width(Size::fill())
                .padding(12.)
                .spacing(10.)
                // The content: every message laid out end to end. This fires as
                // a reply streams in, one measurement per laid-out frame, which
                // is what makes the chat follow the text rather than only the
                // arrival of whole messages.
                .on_sized(move |e: Event<SizedEventData>| {
                    let measured =
                        Transcript { content: e.area.height(), ..*transcript_size.peek() };
                    follow(measured);
                })
                .children(items.into_iter().enumerate().map(|(i, item)| {
                    rect().key(i).width(Size::fill()).child(render_item(item))
                })),
        );

    let status = if busy { "Working…" } else if agent.is_some() { "Ready" } else { "Not configured" };
    // Neither of these is Freya state: a copy sets a static, and jobs are
    // started by the agent on another thread. Poll both - it is a line of text
    // twice a second - so the message appears, expires, and the job count keeps
    // up with the agent starting and killing things.
    let mut ticker = use_state(|| (crate::clipboard::toast(), mc_core::jobs::running()));
    use_hook(move || {
        spawn(async move {
            loop {
                async_io::Timer::after(STATUS_POLL).await;
                let now = (crate::clipboard::toast(), mc_core::jobs::running());
                if *ticker.peek() != now {
                    ticker.set(now);
                }
            }
        });
    });
    let (toast, jobs) = ticker.read().clone();
    let mut status = toast.unwrap_or_else(|| status.to_string());
    // How much of the model's context the conversation now costs to send. It is
    // the number that decides when older turns get summarized away, so it should
    // not be a surprise when that happens.
    if let Some(tokens) = log.read().context_tokens {
        status = format!("{status} · {} context", compact_count(tokens));
    }
    // Background jobs keep running with nothing else on screen to say so, and
    // on a phone that is how a process ends up forgotten.
    if jobs > 0 {
        status = format!(
            "{status} · {jobs} background job{}",
            if jobs == 1 { "" } else { "s" }
        );
    }

    let mut send_on_press = send.clone();
    let composer = rect()
        .content(Content::Flex)
        .horizontal()
        .width(Size::fill())
        .padding(8.)
        .spacing(8.)
        .background(theme::SURFACE)
        .cross_align(Alignment::Center)
        .child(
            Input::new(draft)
                .placeholder(if busy { "Waiting for the agent…" } else { "Ask the agent to build something" })
                .enabled(can_send)
                .width(Size::flex(1.))
                .on_submit(move |text: String| send(text)),
        )
        .child(if busy {
            Button::new()
                .enabled(agent.is_some())
                .on_press(move |_| {
                    if let Some(handle) = &stop_handle {
                        handle.stop();
                    }
                })
                .child("Stop")
        } else {
            Button::new()
                .enabled(can_send)
                .on_press(move |_| {
                    // Copy the text out first. `send_on_press(draft.read().clone())`
                    // keeps the read guard alive for the whole call, so the
                    // write inside `send` hit a borrowed State and panicked.
                    let text = draft.read().clone();
                    send_on_press(text);
                })
                .child("Send")
        });

    // Content::Flex lets the transcript take whatever the status line and the
    // composer leave - which shrinks correctly when the keyboard opens.
    // The chat bar: which conversation this is, and the way to another.
    //
    // Only when there is a library to list - the desktop dev shell and the
    // tests run without one, and a switcher that cannot switch is worse than
    // no switcher.
    // Bumped when the list changes under the picker - a delete - so the render
    // that draws it runs again. The list is read from disk, which nothing else
    // here would notice.
    let revision = use_state(|| 0u64);
    let bar = library.as_ref().map(|library| {
        chat_bar(library.clone(), agent.clone(), picking, busy, log, revision)
    });

    let mut view = rect()
        .content(Content::Flex)
        .width(Size::fill())
        .height(Size::fill())
        .background(theme::GROUND)
        .color(theme::INK)
        .maybe_child(bar)
        .child(transcript)
        .child(
            rect()
                .width(Size::fill())
                .padding((2., 12.))
                .child(label().text(status).font_size(12.).color(theme::MUTED)),
        );
    if !native_composer {
        view = view.child(composer);
    }
    view
}

fn render_item(item: ChatItem) -> Element {
    match item {
        ChatItem::User(text) => rect()
            .width(Size::fill())
            .main_align(Alignment::End)
            .horizontal()
            .child(
                rect()
                    .max_width(Size::percent(85.))
                    .padding((8., 12.))
                    .corner_radius(12.)
                    .background(theme::ACCENT_DIM)
                    .child(label().text(text).font_size(15.)),
            )
            .into(),

        ChatItem::Assistant(text) => rect()
            .width(Size::fill())
            .child(crate::markdown::render(&text))
            .child(
                rect()
                    .width(Size::fill())
                    .horizontal()
                    .main_align(Alignment::End)
                    .child(copy_button(text)),
            )
            .into(),

        ChatItem::Thinking(text) => {
            let shown = tail_chars(&text, THINKING_PREVIEW_CHARS);
            rect()
                .width(Size::fill())
                .child(
                    label()
                        .text(format!("thinking · {shown}"))
                        .font_size(12.)
                        .color(theme::MUTED)
                        .max_lines(3),
                )
                .into()
        }

        ChatItem::Tool { name, summary, state, .. } => {
            let (badge, badge_color, output) = match &state {
                ToolState::Running => ("running", theme::MUTED, None),
                ToolState::Succeeded(out) => ("ok", theme::ACCENT, Some(out.clone())),
                ToolState::Failed(out) => ("failed", theme::DANGER, Some(out.clone())),
            };

            let mut card = rect()
                .width(Size::fill())
                .padding(8.)
                .spacing(4.)
                .corner_radius(8.)
                .background(theme::SURFACE)
                .child(
                    rect()
                        .content(Content::Flex)
                        .horizontal()
                        .width(Size::fill())
                        .spacing(8.)
                        .cross_align(Alignment::Center)
                        .child(label().text(name).font_size(12.).color(theme::ACCENT))
                        .child(
                            rect()
                                .width(Size::flex(1.))
                                .child(label().text(badge).font_size(12.).color(badge_color)),
                        )
                        // The whole card, so a failing command and its error
                        // travel together - that pair is what gets pasted into
                        // a search or sent to someone.
                        .child(copy_button(match &state {
                            ToolState::Running => summary.clone(),
                            ToolState::Succeeded(out) | ToolState::Failed(out) => {
                                format!("$ {summary}\n{out}")
                            }
                        })),
                )
                // Commands and their output are code: monospace keeps columns
                // (ls -la, compiler errors) lined up.
                .child(
                    label()
                        .text(summary)
                        .font_family(crate::markdown::MONO)
                        .font_size(13.)
                        .max_lines(2),
                );

            if let Some(out) = output.filter(|o| !o.trim().is_empty()) {
                card = card.child(
                    label()
                        .text(head_lines(&out, TOOL_PREVIEW_LINES))
                        .font_family(crate::markdown::MONO)
                        .font_size(12.)
                        .color(theme::MUTED),
                );
            }
            card.into()
        }

        ChatItem::Error(text) => rect()
            .width(Size::fill())
            .padding(8.)
            .corner_radius(8.)
            .background(theme::DANGER_DIM)
            .child(label().text(text.clone()).font_size(14.).color(theme::DANGER))
            .child(
                rect()
                    .width(Size::fill())
                    .horizontal()
                    .main_align(Alignment::End)
                    .child(copy_button(text)),
            )
            .into(),

        ChatItem::Notice(text) => rect()
            .width(Size::fill())
            .child(label().text(text).font_size(12.).color(theme::MUTED))
            .into(),
    }
}

/// The bar above the transcript: the current chat, and the way to the others.
///
/// A bar rather than a fifth tab: the tab row is already four wide on a folded
/// phone, and a chat switcher belongs *inside* the chat, next to what it
/// switches.
fn chat_bar(
    library: Arc<SessionLibrary>,
    agent: Option<AgentHandle>,
    mut picking: State<bool>,
    busy: bool,
    log: State<ChatLog>,
    mut revision: State<u64>,
) -> Element {
    let open = *picking.read();
    // Read so that bumping it redraws this bar.
    let _ = *revision.read();
    // Listed only while the picker is open: it reads every chat off the disk,
    // and doing that per frame behind a closed panel would be a waste.
    let chats: Vec<SessionEntry> = if open { library.list() } else { Vec::new() };
    let title = current_title(&log, &chats);

    let new_chat = agent.clone();
    let mut bar = rect()
        .width(Size::fill())
        .background(theme::SURFACE)
        .child(
            rect()
                .content(Content::Flex)
                .horizontal()
                .width(Size::fill())
                .padding((6., 12.))
                .spacing(8.)
                .cross_align(Alignment::Center)
                .child(
                    rect().width(Size::flex(1.)).child(
                        Button::new()
                            .compact()
                            .flat()
                            // Full width: the button hugged its label, so a tap
                            // anywhere past the end of a short title did
                            // nothing (measured on the emulator with "hello").
                            .width(Size::fill())
                            .on_press(move |_| picking.toggle())
                            // The label inside its own full-width rect, or the
                            // button centres it and the title stops reading as
                            // a title.
                            .child(
                                rect().width(Size::fill()).child(
                                    label()
                                        .text(format!("{} {title}", if open { "▾" } else { "▸" }))
                                        .font_size(13.)
                                        .max_lines(1),
                                ),
                            ),
                    ),
                )
                .child(
                    Button::new()
                        .compact()
                        // Not mid-turn: the reply would arrive in a chat the
                        // user is no longer looking at.
                        .enabled(!busy)
                        .on_press(move |_| {
                            if let Some(handle) = &new_chat {
                                handle.new_chat();
                            }
                            picking.set(false);
                        })
                        .child("New"),
                ),
        );

    if open {
        let mut list = rect().width(Size::fill()).padding((0., 8., 8., 8.)).spacing(2.);
        if chats.is_empty() {
            list = list.child(
                label().text("No other chats yet.").font_size(12.).color(theme::MUTED),
            );
        }
        for chat in chats {
            let id = chat.id;
            let open_chat = agent.clone();
            let library = library.clone();
            list = list.child(
                rect()
                    .content(Content::Flex)
                    .horizontal()
                    .width(Size::fill())
                    .padding((6., 8.))
                    .corner_radius(6.)
                    .spacing(8.)
                    .cross_align(Alignment::Center)
                    .child(
                        rect()
                            .width(Size::flex(1.))
                            .on_press(move |_| {
                                if !busy && let Some(handle) = &open_chat {
                                    handle.open_chat(id);
                                }
                                picking.set(false);
                            })
                            .child(label().text(chat.title.clone()).font_size(14.).max_lines(1))
                            .child(
                                label()
                                    .text(format!("{} messages", chat.turns))
                                    .font_size(11.)
                                    .color(theme::MUTED),
                            ),
                    )
                    .child(
                        Button::new()
                            .compact()
                            .flat()
                            .on_press(move |_| {
                                // No confirmation, and no undo: a transcript is
                                // the only copy. Kept as a small, quiet button
                                // rather than a swipe, which on a list this
                                // short would be easier to do by accident.
                                let _ = library.delete(id);
                                let next = *revision.peek() + 1;
                                revision.set(next);
                            })
                            .child(label().text("Delete").font_size(11.).color(theme::DANGER)),
                    ),
            );
        }
        bar = bar.child(list);
    }
    bar.child(rect().width(Size::fill()).height(Size::px(1.)).background(theme::DIVIDER)).into()
}

/// What to call the chat on screen.
///
/// The worker owns the session, so the view has to work it out the same way the
/// library does: from the first thing the user said.
fn current_title(log: &State<ChatLog>, chats: &[SessionEntry]) -> String {
    let first = log.read().items.iter().find_map(|item| match item {
        ChatItem::User(text) => Some(text.clone()),
        _ => None,
    });
    match first {
        Some(text) => {
            let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
            if line.chars().count() <= 40 {
                line.to_string()
            } else {
                format!("{}…", line.chars().take(39).collect::<String>().trim_end())
            }
        }
        // Nothing said yet: name it after nothing, not after another chat.
        None => {
            let _ = chats;
            mc_core::session::DEFAULT_TITLE.to_string()
        }
    }
}

/// A quiet "Copy" that puts `text` on the clipboard.
///
/// Copying matters more here than on a desktop: the phone is the only machine,
/// so an answer, a command or an error has to be able to leave the app - into a
/// browser search, a note, a message to someone.
fn copy_button(text: String) -> Element {
    Button::new()
        .compact()
        .flat()
        .on_press(move |_| crate::clipboard::copy(&text))
        .child(label().text("Copy").font_size(11.).color(theme::MUTED))
        .into_element()
}

/// The first `n` lines, with a count of what was left out.
fn head_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= n {
        return text.trim_end().to_string();
    }
    format!("{}\n… {} more lines", lines[..n].join("\n"), lines.len() - n)
}

/// A token count at a glance: `840`, `38k`, `1.2M`.
fn compact_count(tokens: u32) -> String {
    match tokens {
        0..=999 => tokens.to_string(),
        1_000..=999_999 => format!("{}k", tokens / 1_000),
        _ => format!("{:.1}M", tokens as f32 / 1_000_000.),
    }
}

/// The last `n` characters - for thinking, the most recent reasoning matters.
fn tail_chars(text: &str, n: usize) -> String {
    let count = text.chars().count();
    if count <= n {
        return text.trim().to_string();
    }
    format!("…{}", text.chars().skip(count - n).collect::<String>().trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_count_reads_at_a_glance() {
        assert_eq!(compact_count(0), "0");
        assert_eq!(compact_count(840), "840");
        assert_eq!(compact_count(38_400), "38k");
        assert_eq!(compact_count(1_250_000), "1.2M");
    }

    #[test]
    fn the_bottom_is_what_does_not_fit_on_screen() {
        let screen = Transcript { content: 1000., viewport: 400. };
        assert_eq!(screen.bottom(), -600.);
        // Nothing to scroll: a short conversation sits at the top, not pinned
        // to a negative offset.
        assert_eq!(Transcript { content: 100., viewport: 400. }.bottom(), 0.);
    }

    #[test]
    fn a_reader_at_the_bottom_is_carried_along() {
        let before = Transcript { content: 1000., viewport: 400. };
        let after = Transcript { content: 1200., viewport: 400. };
        // At the bottom (-600) when 200px of reply arrived: follow it down.
        assert_eq!(follow_target(-600., before, after), Some(-800.));
        // A pixel or two off still counts - the two measurements arrive in
        // separate events and disagree for a frame.
        assert_eq!(follow_target(-596., before, after), Some(-800.));
    }

    #[test]
    fn a_reader_who_scrolled_up_is_left_where_they_are() {
        let before = Transcript { content: 1000., viewport: 400. };
        let after = Transcript { content: 1200., viewport: 400. };
        // Reading something 300px above the bottom while the reply streams in.
        assert_eq!(follow_target(-300., before, after), None);
        assert_eq!(follow_target(0., before, after), None, "at the very top");
    }

    #[test]
    fn a_view_already_at_the_bottom_is_not_scrolled_again() {
        // Same size twice, and the height shrinking: in both cases the view is
        // at or past the bottom already. Scrolling here would re-lay out, and
        // re-measure, and never settle.
        let screen = Transcript { content: 1000., viewport: 400. };
        assert_eq!(follow_target(-600., screen, screen), None);
        let shorter = Transcript { content: 800., viewport: 400. };
        assert_eq!(follow_target(-600., screen, shorter), None);
    }

    #[test]
    fn long_tool_output_is_cut_with_a_count() {
        let out = (1..=10).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let shown = head_lines(&out, 6);
        assert!(shown.starts_with("line 1\n"));
        assert!(shown.contains("line 6"));
        assert!(!shown.contains("line 7"));
        assert!(shown.ends_with("… 4 more lines"));
    }

    #[test]
    fn short_output_is_left_alone() {
        assert_eq!(head_lines("a\nb\n", 6), "a\nb");
    }

    #[test]
    fn thinking_keeps_the_most_recent_text() {
        assert_eq!(tail_chars("abcdefghij", 4), "…ghij");
        assert_eq!(tail_chars("short", 40), "short");
    }
}
