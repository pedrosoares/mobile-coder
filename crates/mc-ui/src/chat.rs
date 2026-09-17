//! The chat screen.

use freya::prelude::*;
use mc_core::{AgentHandle, ChatItem, ChatLog, ToolState};

use crate::theme;

/// How much of a tool's output to show inline. The full output went to the
/// model; a phone screen needs the gist.
const TOOL_PREVIEW_LINES: usize = 6;
/// Thinking is context, not content - keep it to a glance.
const THINKING_PREVIEW_CHARS: usize = 280;

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
    }
}

impl Component for ChatView {
    fn render(&self) -> impl IntoElement {
        chat_body(self.agent.clone(), self.native_composer, self.log, self.draft)
    }
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
) -> impl IntoElement {
    let mut scroll = use_scroll_controller(|| ScrollConfig {
        default_vertical_position: ScrollPosition::End,
        ..Default::default()
    });

    // Follow the conversation as it grows, including messages that arrived
    // while this pane was not on screen.
    let length = log.read().items.len();
    use_side_effect(move || {
        let _ = length;
        scroll.scroll_to(ScrollPosition::End, Direction::Vertical);
    });

    let busy = log.read().busy;
    let can_send = agent.is_some() && !busy;

    let submit_handle = agent.clone();
    let mut send = move |text: String| {
        let text = text.trim().to_string();
        if text.is_empty() || log.read().busy {
            return;
        }
        if let Some(handle) = &submit_handle {
            if handle.submit(text) {
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
        .child(
            rect()
                .width(Size::fill())
                .padding(12.)
                .spacing(10.)
                .children(items.into_iter().enumerate().map(|(i, item)| {
                    rect().key(i).width(Size::fill()).child(render_item(item))
                })),
        );

    let status = if busy { "Working…" } else if agent.is_some() { "Ready" } else { "Not configured" };

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
        .child(
            Button::new()
                .enabled(can_send)
                .on_press(move |_| {
                    // Copy the text out first. `send_on_press(draft.read().clone())`
                    // keeps the read guard alive for the whole call, so the
                    // write inside `send` hit a borrowed State and panicked.
                    let text = draft.read().clone();
                    send_on_press(text);
                })
                .child("Send"),
        );

    // Content::Flex lets the transcript take whatever the status line and the
    // composer leave - which shrinks correctly when the keyboard opens.
    let mut view = rect()
        .content(Content::Flex)
        .width(Size::fill())
        .height(Size::fill())
        .background(theme::GROUND)
        .color(theme::INK)
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
                        .horizontal()
                        .width(Size::fill())
                        .spacing(8.)
                        .child(label().text(name).font_size(12.).color(theme::ACCENT))
                        .child(label().text(badge).font_size(12.).color(badge_color)),
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
            .child(label().text(text).font_size(14.).color(theme::DANGER))
            .into(),

        ChatItem::Notice(text) => rect()
            .width(Size::fill())
            .child(label().text(text).font_size(12.).color(theme::MUTED))
            .into(),
    }
}

/// The first `n` lines, with a count of what was left out.
fn head_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= n {
        return text.trim_end().to_string();
    }
    format!("{}\n… {} more lines", lines[..n].join("\n"), lines.len() - n)
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
