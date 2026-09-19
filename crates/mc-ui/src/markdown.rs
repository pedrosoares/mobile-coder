//! Markdown for assistant replies.
//!
//! Models answer in markdown, and shown raw it reads as noise - `**3.21.7**`,
//! fence lines around every command. Freya's own `markdown` feature cannot be
//! used: `freya 0.5.0-rc.6` requires `freya-markdown ^0.5.0-rc.6`, and the newest
//! published on crates.io is `rc.3`, so the dependency does not resolve.
//!
//! This covers what chat replies actually contain - emphasis, inline code, code
//! blocks, headings, lists, quotes - and nothing more. Parsing is split from
//! drawing so the rules are testable without a UI.
//!
//! Replies are re-rendered on every streamed delta, so the input is routinely
//! incomplete: an unclosed fence, a dangling `**`. CommonMark degrades gracefully
//! there (an open fence runs to the end; an unmatched `**` stays literal), which
//! is exactly the behaviour a stream needs.

use freya::prelude::*;
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::theme;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inline {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub strike: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Paragraph(Vec<Inline>),
    Heading { level: u8, inlines: Vec<Inline> },
    Code { language: Option<String>, text: String },
    /// `marker` is `•` or a number with its dot; `depth` starts at 0.
    ListItem { marker: String, depth: usize, inlines: Vec<Inline> },
    Quote(Vec<Inline>),
    /// First row is the header.
    Table(Vec<Vec<Vec<Inline>>>),
    Rule,
}

/// Parse markdown into blocks.
pub fn parse(source: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut inlines: Vec<Inline> = Vec::new();

    // Style currently in effect for text events.
    let (mut bold, mut italic, mut strike) = (0u32, 0u32, 0u32);
    // Open container context.
    let mut heading: Option<u8> = None;
    let mut quote_depth = 0usize;
    let mut code: Option<(Option<String>, String)> = None;
    // One entry per open list: Some(next number) for ordered, None for bullets.
    let mut lists: Vec<Option<u64>> = Vec::new();
    let mut item_marker: Option<String> = None;
    // Table being collected: rows of cells, each cell a run of inlines.
    let mut table: Option<Vec<Vec<Vec<Inline>>>> = None;

    let push_text = |inlines: &mut Vec<Inline>, text: &str, bold: u32, italic: u32, strike: u32, code: bool| {
        if text.is_empty() {
            return;
        }
        let style = Inline { text: String::new(), bold: bold > 0, italic: italic > 0, code, strike: strike > 0 };
        // Merge runs of identical style, so a paragraph is a few spans rather
        // than one per parser event.
        match inlines.last_mut() {
            Some(last)
                if last.bold == style.bold
                    && last.italic == style.italic
                    && last.code == style.code
                    && last.strike == style.strike =>
            {
                last.text.push_str(text)
            }
            _ => inlines.push(Inline { text: text.to_string(), ..style }),
        }
    };

    // Close whatever text block is being collected.
    let flush = |blocks: &mut Vec<Block>,
                 inlines: &mut Vec<Inline>,
                 heading: Option<u8>,
                 quote_depth: usize,
                 lists: &[Option<u64>],
                 item_marker: &mut Option<String>| {
        if inlines.iter().all(|i| i.text.trim().is_empty()) && item_marker.is_none() {
            inlines.clear();
            return;
        }
        let taken = std::mem::take(inlines);
        let block = if let Some(marker) = item_marker.take() {
            Block::ListItem { marker, depth: lists.len().saturating_sub(1), inlines: taken }
        } else if let Some(level) = heading {
            Block::Heading { level, inlines: taken }
        } else if quote_depth > 0 {
            Block::Quote(taken)
        } else {
            Block::Paragraph(taken)
        };
        blocks.push(block);
    };

    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
    for event in Parser::new_ext(source, options) {
        // Inside a table, text collects into the current cell instead of a
        // paragraph. Style events still apply.
        if let Some(rows) = table.as_mut() {
            match &event {
                Event::End(TagEnd::Table) => {
                    let rows = table.take().unwrap_or_default();
                    if !rows.is_empty() {
                        blocks.push(Block::Table(rows));
                    }
                    continue;
                }
                // pulldown-cmark puts header cells directly under TableHead,
                // and body cells under TableRow - both start a new row.
                Event::Start(Tag::TableHead) | Event::Start(Tag::TableRow) => {
                    rows.push(Vec::new());
                    continue;
                }
                Event::Start(Tag::TableCell) => {
                    if let Some(row) = rows.last_mut() {
                        row.push(Vec::new());
                    }
                    continue;
                }
                Event::Text(text) | Event::Code(text) => {
                    let is_code = matches!(event, Event::Code(_));
                    if let Some(cell) = rows.last_mut().and_then(|row| row.last_mut()) {
                        push_text(cell, text, bold, italic, strike, is_code);
                    }
                    continue;
                }
                Event::Start(Tag::Strong)
                | Event::End(TagEnd::Strong)
                | Event::Start(Tag::Emphasis)
                | Event::End(TagEnd::Emphasis)
                | Event::Start(Tag::Strikethrough)
                | Event::End(TagEnd::Strikethrough) => {}
                _ => continue,
            }
        }

        if let Some((_, buffer)) = code.as_mut() {
            match event {
                Event::Text(text) => {
                    buffer.push_str(&text);
                    continue;
                }
                Event::End(TagEnd::CodeBlock) => {
                    let (language, text) = code.take().unwrap_or_default();
                    blocks.push(Block::Code { language, text: text.trim_end_matches('\n').to_string() });
                    continue;
                }
                _ => continue,
            }
        }

        match event {
            Event::Start(Tag::Strong) => bold += 1,
            Event::End(TagEnd::Strong) => bold = bold.saturating_sub(1),
            Event::Start(Tag::Emphasis) => italic += 1,
            Event::End(TagEnd::Emphasis) => italic = italic.saturating_sub(1),
            Event::Start(Tag::Strikethrough) => strike += 1,
            Event::End(TagEnd::Strikethrough) => strike = strike.saturating_sub(1),

            Event::Start(Tag::Heading { level, .. }) => {
                flush(&mut blocks, &mut inlines, heading, quote_depth, &lists, &mut item_marker);
                heading = Some(match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    _ => 3,
                });
            }
            Event::End(TagEnd::Heading(_)) => {
                flush(&mut blocks, &mut inlines, heading, quote_depth, &lists, &mut item_marker);
                heading = None;
            }

            Event::Start(Tag::BlockQuote(_)) => quote_depth += 1,
            Event::End(TagEnd::BlockQuote(_)) => {
                flush(&mut blocks, &mut inlines, heading, quote_depth, &lists, &mut item_marker);
                quote_depth = quote_depth.saturating_sub(1);
            }

            Event::Start(Tag::List(start)) => {
                flush(&mut blocks, &mut inlines, heading, quote_depth, &lists, &mut item_marker);
                lists.push(start);
            }
            Event::End(TagEnd::List(_)) => {
                flush(&mut blocks, &mut inlines, heading, quote_depth, &lists, &mut item_marker);
                lists.pop();
            }
            Event::Start(Tag::Item) => {
                flush(&mut blocks, &mut inlines, heading, quote_depth, &lists, &mut item_marker);
                item_marker = Some(match lists.last_mut() {
                    Some(Some(n)) => {
                        let marker = format!("{n}.");
                        *n += 1;
                        marker
                    }
                    _ => "•".to_string(),
                });
            }
            Event::End(TagEnd::Item) => {
                flush(&mut blocks, &mut inlines, heading, quote_depth, &lists, &mut item_marker);
            }

            Event::Start(Tag::Table(_)) => {
                flush(&mut blocks, &mut inlines, heading, quote_depth, &lists, &mut item_marker);
                table = Some(Vec::new());
            }

            Event::Start(Tag::CodeBlock(kind)) => {
                flush(&mut blocks, &mut inlines, heading, quote_depth, &lists, &mut item_marker);
                let language = match kind {
                    CodeBlockKind::Fenced(lang) if !lang.trim().is_empty() => Some(lang.trim().to_string()),
                    _ => None,
                };
                code = Some((language, String::new()));
            }

            Event::End(TagEnd::Paragraph) => {
                // Inside a list item the paragraph belongs to the item; the
                // item closes it.
                if item_marker.is_none() {
                    flush(&mut blocks, &mut inlines, heading, quote_depth, &lists, &mut item_marker);
                }
            }

            Event::Text(text) => push_text(&mut inlines, &text, bold, italic, strike, false),
            Event::Code(text) => push_text(&mut inlines, &text, bold, italic, strike, true),
            Event::SoftBreak => push_text(&mut inlines, " ", bold, italic, strike, false),
            Event::HardBreak => push_text(&mut inlines, "\n", bold, italic, strike, false),
            Event::Rule => {
                flush(&mut blocks, &mut inlines, heading, quote_depth, &lists, &mut item_marker);
                blocks.push(Block::Rule);
            }
            // Raw HTML and the like: show the source rather than drop it.
            Event::Html(text) | Event::InlineHtml(text) => {
                push_text(&mut inlines, &text, bold, italic, strike, false)
            }
            _ => {}
        }
    }

    // A stream can stop mid-table, mid-fence or mid-paragraph; show what arrived.
    if let Some(rows) = table.take().filter(|rows| !rows.is_empty()) {
        blocks.push(Block::Table(rows));
    }
    if let Some((language, text)) = code.take() {
        blocks.push(Block::Code { language, text: text.trim_end_matches('\n').to_string() });
    }
    flush(&mut blocks, &mut inlines, heading, quote_depth, &lists, &mut item_marker);
    blocks
}

const BODY_SIZE: f32 = 15.;
/// Monospace family for code.
///
/// On Android, Freya does not see system fonts by name: both `monospace` and
/// `Droid Sans Mono` silently fell back to the proportional UI font on the
/// emulator. So the Android shell reads the system's monospace file and registers
/// it under [`MONO_FONT_NAME`]; see [`android_mono_font`].
#[cfg(target_os = "android")]
pub const MONO: &str = MONO_FONT_NAME;
#[cfg(not(target_os = "android"))]
pub const MONO: &str = "monospace";

/// App-specific name the monospace font is registered under on Android.
pub const MONO_FONT_NAME: &str = "mc-mono";

/// The system monospace font's bytes, for `LaunchConfig::with_font`.
///
/// `DroidSansMono.ttf` is what AOSP's `fonts.xml` maps `monospace` to. `None`
/// if it is missing, in which case code falls back to the UI font - legible,
/// just not aligned.
pub fn android_mono_font() -> Option<Vec<u8>> {
    ["/system/fonts/DroidSansMono.ttf", "/system/fonts/CutiveMono.ttf"]
        .iter()
        .find_map(|path| std::fs::read(path).ok())
}

fn spans(inlines: Vec<Inline>, size: f32) -> Paragraph {
    paragraph().width(Size::fill()).font_size(size).spans_iter(inlines.into_iter().map(move |inline| {
        let mut span = Span::new(inline.text).font_size(size).color(theme::INK);
        if inline.bold {
            span = span.font_weight(FontWeight::BOLD);
        }
        if inline.italic {
            span = span.font_slant(FontSlant::Italic);
        }
        if inline.strike {
            span = span.text_decoration(TextDecoration::LineThrough);
        }
        if inline.code {
            // Colour only. A span's own `font_family` is ignored by Freya
            // 0.5.0-rc.6: `Span::to_text_style` builds its font list from the
            // parent paragraph's state instead of the merged span style, so a
            // monospace span inside a sentence renders in the UI font anyway.
            span = span.color(theme::ACCENT);
        }
        span
    }))
}

/// Draw markdown as a column of blocks.
pub fn render(source: &str) -> impl IntoElement {
    rect().width(Size::fill()).spacing(8.).children(parse(source).into_iter().enumerate().map(
        |(i, block)| -> Element {
            match block {
                Block::Paragraph(inlines) => rect().key(i).width(Size::fill()).child(spans(inlines, BODY_SIZE)).into(),

                Block::Heading { level, inlines } => {
                    let size = match level {
                        1 => 20.,
                        2 => 18.,
                        _ => 16.,
                    };
                    let bolded = inlines.into_iter().map(|mut inline| {
                        inline.bold = true;
                        inline
                    });
                    rect().key(i).width(Size::fill()).child(spans(bolded.collect(), size)).into()
                }

                Block::Code { language, text } => {
                    let mut card = rect()
                        .key(i)
                        .width(Size::fill())
                        .padding(10.)
                        .spacing(4.)
                        .corner_radius(8.)
                        .background(theme::SURFACE);
                    // A code block is the thing most worth copying: a command
                    // to run, a file to paste. Its header carries the button.
                    let snippet = text.clone();
                    card = card.child(
                        rect()
                            .content(Content::Flex)
                            .horizontal()
                            .width(Size::fill())
                            .cross_align(Alignment::Center)
                            .child(
                                rect().width(Size::flex(1.)).child(
                                    label()
                                        .text(language.unwrap_or_default())
                                        .font_size(11.)
                                        .color(theme::MUTED),
                                ),
                            )
                            .child(
                                Button::new()
                                    .compact()
                                    .flat()
                                    .on_press(move |_| crate::clipboard::copy(&snippet))
                                    .child(
                                        label().text("Copy").font_size(11.).color(theme::MUTED),
                                    ),
                            ),
                    );
                    card.child(
                        // The font goes on the paragraph, not the span: Freya
                        // ignores span-level font families (see `spans`).
                        paragraph()
                            .width(Size::fill())
                            .font_family(MONO)
                            .font_size(13.)
                            .span(Span::new(text).font_size(13.).color(theme::INK)),
                    )
                    .into()
                }

                Block::ListItem { marker, depth, inlines } => rect()
                    .key(i)
                    .content(Content::Flex)
                    .horizontal()
                    .width(Size::fill())
                    .padding((0., 0., 0., 16. * depth as f32))
                    .spacing(8.)
                    .child(label().text(marker).font_size(BODY_SIZE).color(theme::MUTED))
                    .child(rect().width(Size::flex(1.)).child(spans(inlines, BODY_SIZE)))
                    .into(),

                Block::Quote(inlines) => rect()
                    .key(i)
                    .content(Content::Flex)
                    .horizontal()
                    .width(Size::fill())
                    .spacing(10.)
                    .child(rect().width(Size::px(3.)).height(Size::fill()).background(theme::DIVIDER))
                    .child(rect().width(Size::flex(1.)).child(spans(inlines, BODY_SIZE)))
                    .into(),

                Block::Table(rows) => {
                    let columns = rows.iter().map(Vec::len).max().unwrap_or(0).max(1);
                    let mut grid = rect()
                        .key(i)
                        .width(Size::fill())
                        .corner_radius(8.)
                        .background(theme::SURFACE)
                        .padding(4.);
                    for (r, row) in rows.into_iter().enumerate() {
                        let mut line = rect()
                            .content(Content::Flex)
                            .horizontal()
                            .width(Size::fill())
                            .padding((6., 8.))
                            .spacing(12.);
                        if r > 0 {
                            line = line.background(theme::GROUND);
                        }
                        // Equal columns: on a phone there is no room for widths
                        // negotiated from content, and wrapping keeps it legible.
                        for c in 0..columns {
                            let mut cell = row.get(c).cloned().unwrap_or_default();
                            if r == 0 {
                                for inline in &mut cell {
                                    inline.bold = true;
                                }
                            }
                            line = line.child(rect().width(Size::flex(1.)).child(spans(cell, 13.)));
                        }
                        grid = grid.child(line);
                    }
                    grid.into()
                }

                Block::Rule => rect().key(i).width(Size::fill()).height(Size::px(1.)).background(theme::DIVIDER).into(),
            }
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(text: &str) -> Inline {
        Inline { text: text.into(), ..Default::default() }
    }

    #[test]
    fn emphasis_becomes_styled_runs_not_literal_asterisks() {
        // The exact reply that showed raw markdown on the emulator.
        let blocks = parse("Alpine Linux **3.21.7** is the base of the sandbox.");
        assert_eq!(
            blocks,
            vec![Block::Paragraph(vec![
                plain("Alpine Linux "),
                Inline { text: "3.21.7".into(), bold: true, ..Default::default() },
                plain(" is the base of the sandbox."),
            ])]
        );
    }

    #[test]
    fn fenced_code_keeps_its_text_and_language_without_fence_lines() {
        let blocks = parse("Output:\n\n```sh\nLinux localhost 6.6.66\n```\n");
        assert_eq!(blocks[0], Block::Paragraph(vec![plain("Output:")]));
        assert_eq!(
            blocks[1],
            Block::Code { language: Some("sh".into()), text: "Linux localhost 6.6.66".into() }
        );
    }

    #[test]
    fn an_unclosed_fence_mid_stream_still_shows_as_code() {
        let blocks = parse("Running:\n\n```\ngcc -O2 hello.c");
        assert!(matches!(&blocks[1], Block::Code { text, .. } if text == "gcc -O2 hello.c"));
    }

    #[test]
    fn a_dangling_bold_marker_mid_stream_stays_literal() {
        let blocks = parse("The result is **4");
        let text: String = match &blocks[0] {
            Block::Paragraph(inlines) => inlines.iter().map(|i| i.text.as_str()).collect(),
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(text, "The result is **4");
    }

    #[test]
    fn inline_code_is_marked() {
        let blocks = parse("Run `uname -m` now");
        assert!(matches!(&blocks[0], Block::Paragraph(i) if i[1].code && i[1].text == "uname -m"));
    }

    #[test]
    fn lists_are_numbered_and_nested() {
        let blocks = parse("1. first\n2. second\n   - nested\n");
        let items: Vec<(String, usize)> = blocks
            .iter()
            .filter_map(|b| match b {
                Block::ListItem { marker, depth, .. } => Some((marker.clone(), *depth)),
                _ => None,
            })
            .collect();
        assert_eq!(items, vec![("1.".into(), 0), ("2.".into(), 0), ("•".into(), 1)]);
    }

    #[test]
    fn tables_become_rows_of_cells_with_the_header_first() {
        // Shape taken from a real reply that rendered as raw pipes.
        let blocks = parse("| Test | Result |\n|---|---|\n| `echo hello` | works |\n| pipe | **fails** |\n");
        let Block::Table(rows) = &blocks[0] else { panic!("expected a table, got {blocks:?}") };
        let text = |cell: &Vec<Inline>| cell.iter().map(|i| i.text.as_str()).collect::<String>();
        assert_eq!(rows.len(), 3);
        assert_eq!(text(&rows[0][0]), "Test");
        assert_eq!(text(&rows[1][0]), "echo hello");
        assert!(rows[1][0][0].code, "inline code inside a cell is kept");
        assert!(rows[2][1][0].bold, "emphasis inside a cell is kept");
    }

    #[test]
    fn a_table_cut_off_mid_stream_still_shows_its_rows() {
        let blocks = parse("| a | b |\n|---|---|\n| 1 | 2 |");
        assert!(matches!(&blocks[0], Block::Table(rows) if rows.len() == 2));
    }

    #[test]
    fn headings_carry_their_level() {
        assert!(matches!(&parse("## Build")[0], Block::Heading { level: 2, .. }));
    }
}
