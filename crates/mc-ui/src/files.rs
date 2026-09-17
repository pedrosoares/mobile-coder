//! The Files pane: browse the sandbox and read files.
//!
//! Read-only on purpose, for now. Editing happens through the agent, whose edits
//! arrive as reviewable tool calls in the chat; a second, silent write path from
//! a phone keyboard would undercut that.

use std::sync::Arc;

use freya::prelude::*;
use mc_core::{
    EntryKind, FileBrowser, FileEntry, FilePreview,
    files::{format_size, join, parent},
};

use crate::theme;

/// How much of a file to load for preview. A phone screen cannot usefully show
/// more, and the read happens on the UI thread.
const PREVIEW_LIMIT: usize = 128 * 1024;

#[derive(Clone, PartialEq)]
enum View {
    Listing(Result<Vec<FileEntry>, String>),
    Preview { path: String, content: Result<FilePreview, String> },
}

/// The Files pane. A [`Component`] for the same reason as
/// [`crate::chat::ChatView`]: its hooks need their own scope.
pub struct FilesView {
    pub browser: Option<Arc<dyn FileBrowser>>,
    /// Owned by the app root, so coming back lands where the user left off.
    /// Empty means "not visited yet"; the browser fills in its home.
    pub cwd: State<String>,
}

impl PartialEq for FilesView {
    fn eq(&self, other: &Self) -> bool {
        self.cwd == other.cwd
            && match (&self.browser, &other.browser) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            }
    }
}

impl Component for FilesView {
    fn render(&self) -> impl IntoElement {
        // Branch into separate components rather than returning early: an
        // early return before the browser's hooks would skip them on some
        // renders, which is the same hook-order violation.
        match &self.browser {
            Some(browser) => Browser { browser: browser.clone(), cwd: self.cwd }.into_element(),
            None => rect()
                .width(Size::fill())
                .height(Size::fill())
                .center()
                .color(theme::MUTED)
                .child("File browsing is not available here.")
                .into_element(),
        }
    }
}

struct Browser {
    browser: Arc<dyn FileBrowser>,
    cwd: State<String>,
}

impl PartialEq for Browser {
    fn eq(&self, other: &Self) -> bool {
        self.cwd == other.cwd && Arc::ptr_eq(&self.browser, &other.browser)
    }
}

impl Component for Browser {
    fn render(&self) -> impl IntoElement {
        browser_body(self.browser.clone(), self.cwd)
    }
}

fn browser_body(browser: Arc<dyn FileBrowser>, mut cwd: State<String>) -> impl IntoElement {
    if cwd.peek().is_empty() {
        cwd.set(browser.home());
    }
    // Read the directory once per visit, not per render: this pane is
    // remounted when its tab is opened, so the initialiser runs exactly then.
    // Re-reading on every render would hit the disk on the UI thread for each
    // streamed chat delta, since those re-render the root.
    let listed = cwd.peek().clone();
    let mut view = use_state({
        let browser = browser.clone();
        move || View::Listing(browser.list(&listed))
    });

    let open_dir = {
        let browser = browser.clone();
        move |path: String| {
            view.set(View::Listing(browser.list(&path)));
            cwd.set(path);
        }
    };

    let path = cwd.read().clone();
    let current = view.read().clone();

    // ---- header: where we are, and a way up ---------------------------------
    let (title, up_target) = match &current {
        View::Listing(_) => (path.clone(), parent(&path)),
        View::Preview { path: file, .. } => (file.clone(), Some(cwd.read().clone())),
    };
    let mut header = rect()
        .content(Content::Flex)
        .horizontal()
        .width(Size::fill())
        .padding((8., 12.))
        .spacing(8.)
        .cross_align(Alignment::Center)
        .background(theme::SURFACE);
    {
        let mut open_dir = open_dir.clone();
        let is_preview = matches!(current, View::Preview { .. });
        header = header.child(
            Button::new()
                .compact()
                .flat()
                .enabled(up_target.is_some())
                .on_press(move |_| {
                    if let Some(target) = up_target.clone() {
                        // From a preview, "back" returns to the listing it came
                        // from; from a listing, it goes to the parent.
                        open_dir(target);
                    }
                })
                .child(if is_preview { "Back" } else { "Up" }),
        );
    }
    header = header.child(
        rect().width(Size::flex(1.)).child(
            label()
                .text(title)
                .font_family(crate::markdown::MONO)
                .font_size(13.)
                .color(theme::MUTED)
                .max_lines(1),
        ),
    );

    // ---- body -----------------------------------------------------------------
    let body: Element = match current {
        View::Listing(Err(message)) | View::Preview { content: Err(message), .. } => {
            notice(&message, theme::DANGER).into_element()
        }

        View::Listing(Ok(entries)) if entries.is_empty() => notice("Empty directory", theme::MUTED).into_element(),

        View::Listing(Ok(entries)) => ScrollView::new()
            .width(Size::fill())
            .height(Size::fill())
            .child(rect().width(Size::fill()).children(entries.into_iter().enumerate().map(|(i, entry)| {
                let target = join(&path, &entry.name);
                let browser = browser.clone();
                let mut open_dir = open_dir.clone();
                let kind = entry.kind;
                let row = entry_row(entry);
                rect()
                    .key(i)
                    .width(Size::fill())
                    .on_press(move |_| match kind {
                        EntryKind::Directory => open_dir(target.clone()),
                        _ => view.set(View::Preview {
                            path: target.clone(),
                            content: browser.preview(&target, PREVIEW_LIMIT),
                        }),
                    })
                    .child(row)
            })))
            .into(),

        View::Preview { content: Ok(FilePreview::Binary { size }), .. } => {
            notice(&format!("Binary file, {} - not shown.", format_size(size)), theme::MUTED).into_element()
        }

        View::Preview { content: Ok(FilePreview::Text { text, truncated }), .. } => {
            let mut column = rect().width(Size::fill()).padding(12.).spacing(8.);
            if truncated {
                column = column.child(
                    label()
                        .text(format!("Showing the first {}.", format_size(PREVIEW_LIMIT as u64)))
                        .font_size(12.)
                        .color(theme::MUTED),
                );
            }
            ScrollView::new()
                .width(Size::fill())
                .height(Size::fill())
                .child(
                    column.child(
                        paragraph()
                            .width(Size::fill())
                            .font_family(crate::markdown::MONO)
                            .font_size(12.)
                            .span(Span::new(text).font_size(12.).color(theme::INK)),
                    ),
                )
                .into()
        }
    };

    rect()
        .content(Content::Flex)
        .width(Size::fill())
        .height(Size::fill())
        .background(theme::GROUND)
        .color(theme::INK)
        .child(header)
        .child(rect().width(Size::fill()).height(Size::px(1.)).background(theme::DIVIDER))
        .child(rect().width(Size::fill()).height(Size::flex(1.)).child(body))
        .into_element()
}

fn entry_row(entry: FileEntry) -> impl IntoElement {
    let (glyph, glyph_color) = match entry.kind {
        EntryKind::Directory => ("▸", theme::ACCENT),
        EntryKind::Symlink => ("→", theme::MUTED),
        EntryKind::File | EntryKind::Other => ("·", theme::MUTED),
    };
    let detail = entry.size.map(format_size).unwrap_or_default();

    rect()
        .content(Content::Flex)
        .horizontal()
        .width(Size::fill())
        // A full tap target per row: file lists are tapped with a thumb.
        .height(Size::px(theme::TAP_TARGET))
        .padding((0., 16.))
        .spacing(12.)
        .cross_align(Alignment::Center)
        .child(label().text(glyph).font_size(16.).color(glyph_color))
        .child(
            rect().width(Size::flex(1.)).child(
                label()
                    .text(entry.name)
                    .font_size(15.)
                    .color(theme::INK)
                    .max_lines(1),
            ),
        )
        .child(label().text(detail).font_size(12.).color(theme::MUTED))
}

fn notice(text: &str, color: (u8, u8, u8)) -> impl IntoElement {
    rect()
        .width(Size::fill())
        .padding(16.)
        .child(label().text(text.to_string()).font_size(14.).color(color))
}
