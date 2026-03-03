use crate::tui::theme;
use pulldown_cmark::{Alignment, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::borrow::Cow;

/// Render markdown text into styled ratatui lines.
/// Each line is indented with 2 spaces to match the conversation layout.
pub fn render_markdown(text: &str) -> Vec<Line<'static>> {
    let mut renderer = MarkdownRenderer::new();
    let opts = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_HEADING_ATTRIBUTES;
    let parser = Parser::new_ext(text, opts);

    for event in parser {
        renderer.process(event);
    }
    renderer.finish()
}

const INDENT: &str = "  ";
const MIN_COL_WIDTH: usize = 3;
const MAX_COL_WIDTH: usize = 40;

struct MarkdownRenderer {
    lines: Vec<Line<'static>>,
    /// Current inline spans being accumulated for a paragraph/heading/list item.
    spans: Vec<Span<'static>>,
    /// Style stack for nested inline formatting (bold, italic, code).
    style_stack: Vec<Style>,
    /// Current heading level (0 = not in heading).
    heading_level: u8,
    /// List nesting: each entry is Some(counter) for ordered, None for unordered.
    list_stack: Vec<Option<u64>>,
    /// Whether we're at the start of a list item (need to emit bullet/number).
    list_item_start: bool,
    /// Table state.
    table: Option<TableState>,
    /// Whether we're inside a code block.
    in_code_block: bool,
    /// Code block language label.
    code_lang: Option<String>,
    /// Accumulated code block lines.
    code_lines: Vec<String>,
    /// Whether we're inside a link — accumulate text, emit styled at end.
    link_url: Option<String>,
}

struct TableState {
    alignments: Vec<Alignment>,
    header_row: Vec<String>,
    body_rows: Vec<Vec<String>>,
    current_row: Vec<String>,
    current_cell: String,
    in_header: bool,
}

impl MarkdownRenderer {
    fn new() -> Self {
        Self {
            lines: Vec::new(),
            spans: Vec::new(),
            style_stack: vec![Style::default().fg(theme::FROST)],
            heading_level: 0,
            list_stack: Vec::new(),
            list_item_start: false,
            table: None,
            in_code_block: false,
            code_lang: None,
            code_lines: Vec::new(),
            link_url: None,
        }
    }

    fn current_style(&self) -> Style {
        self.style_stack.last().copied().unwrap_or(theme::text())
    }

    fn list_indent(&self) -> String {
        let depth = self.list_stack.len().saturating_sub(1);
        format!("{}{}", INDENT, "  ".repeat(depth))
    }

    fn process(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(text) => self.text(&text),
            Event::Code(code) => self.inline_code(&code),
            Event::SoftBreak => self.soft_break(),
            Event::HardBreak => self.hard_break(),
            Event::Rule => self.rule(),
            _ => {}
        }
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Heading { level, .. } => {
                self.heading_level = level as u8;
                let style = match level {
                    pulldown_cmark::HeadingLevel::H1 | pulldown_cmark::HeadingLevel::H2 => {
                        Style::default()
                            .fg(theme::HONEY)
                            .add_modifier(Modifier::BOLD)
                    }
                    pulldown_cmark::HeadingLevel::H3 => Style::default()
                        .fg(theme::FROST)
                        .add_modifier(Modifier::BOLD),
                    _ => Style::default()
                        .fg(theme::ICE)
                        .add_modifier(Modifier::BOLD),
                };
                self.style_stack.push(style);
            }
            Tag::Paragraph => {}
            Tag::Emphasis => {
                let base = self.current_style();
                self.style_stack.push(base.add_modifier(Modifier::ITALIC));
            }
            Tag::Strong => {
                let base = self.current_style();
                self.style_stack.push(base.add_modifier(Modifier::BOLD));
            }
            Tag::List(start) => {
                self.list_stack.push(start);
            }
            Tag::Item => {
                self.list_item_start = true;
            }
            Tag::CodeBlock(kind) => {
                self.in_code_block = true;
                self.code_lines.clear();
                self.code_lang = match kind {
                    pulldown_cmark::CodeBlockKind::Fenced(lang) => {
                        let l = lang.to_string();
                        if l.is_empty() { None } else { Some(l) }
                    }
                    _ => None,
                };
            }
            Tag::Table(alignments) => {
                self.table = Some(TableState {
                    alignments,
                    header_row: Vec::new(),
                    body_rows: Vec::new(),
                    current_row: Vec::new(),
                    current_cell: String::new(),
                    in_header: false,
                });
            }
            Tag::TableHead => {
                if let Some(ref mut t) = self.table {
                    t.in_header = true;
                    t.current_row.clear();
                }
            }
            Tag::TableRow => {
                if let Some(ref mut t) = self.table {
                    t.current_row.clear();
                }
            }
            Tag::TableCell => {
                if let Some(ref mut t) = self.table {
                    t.current_cell.clear();
                }
            }
            Tag::Link { dest_url, .. } => {
                self.link_url = Some(dest_url.to_string());
            }
            _ => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Heading(_) => {
                self.style_stack.pop();
                self.lines.push(Line::from(""));
                self.flush_spans();
                self.heading_level = 0;
            }
            TagEnd::Paragraph => {
                self.flush_spans();
                self.lines.push(Line::from(""));
            }
            TagEnd::Emphasis | TagEnd::Strong => {
                self.style_stack.pop();
            }
            TagEnd::List(_) => {
                self.list_stack.pop();
                if self.list_stack.is_empty() {
                    self.lines.push(Line::from(""));
                }
            }
            TagEnd::Item => {}
            TagEnd::CodeBlock => {
                self.in_code_block = false;
                self.emit_code_block();
            }
            TagEnd::Table => {
                self.emit_table();
            }
            TagEnd::TableHead => {
                if let Some(ref mut t) = self.table {
                    t.header_row = std::mem::take(&mut t.current_row);
                    t.in_header = false;
                }
            }
            TagEnd::TableRow => {
                if let Some(ref mut t) = self.table {
                    let row = std::mem::take(&mut t.current_row);
                    t.body_rows.push(row);
                }
            }
            TagEnd::TableCell => {
                if let Some(ref mut t) = self.table {
                    let cell = std::mem::take(&mut t.current_cell);
                    t.current_row.push(cell);
                }
            }
            TagEnd::Link => {
                if let Some(url) = self.link_url.take() {
                    // Text was already accumulated in spans with HONEY style;
                    // append the URL in muted parens
                    if !url.is_empty() {
                        self.spans.push(Span::styled(
                            format!(" ({})", url),
                            theme::muted(),
                        ));
                    }
                }
            }
            _ => {}
        }
    }

    fn text(&mut self, text: &str) {
        // Table cell text
        if let Some(ref mut t) = self.table {
            t.current_cell.push_str(text);
            return;
        }

        // Code block text
        if self.in_code_block {
            // pulldown-cmark gives us the entire code block content as one text event
            // but it may also come in multiple events, so accumulate
            self.code_lines
                .extend(text.lines().map(String::from));
            // If the text ends with a newline and the last "line" is empty, drop it
            // (pulldown-cmark includes trailing newline)
            if text.ends_with('\n') && self.code_lines.last().is_some_and(|l| l.is_empty()) {
                self.code_lines.pop();
            }
            return;
        }

        // Link text — style as HONEY
        if self.link_url.is_some() {
            self.spans.push(Span::styled(
                text.to_string(),
                Style::default().fg(theme::HONEY),
            ));
            return;
        }

        // List item start — prepend bullet/number
        if self.list_item_start {
            self.list_item_start = false;
            let indent = self.list_indent();
            match self.list_stack.last_mut() {
                Some(Some(n)) => {
                    let prefix = format!("{}{}. ", indent, n);
                    *n += 1;
                    self.spans
                        .push(Span::styled(prefix, Style::default().fg(theme::HONEY)));
                }
                _ => {
                    let prefix = format!("{}\u{2022} ", indent);
                    self.spans
                        .push(Span::styled(prefix, Style::default().fg(theme::HONEY)));
                }
            }
        }

        let style = self.current_style();
        self.spans.push(Span::styled(text.to_string(), style));
    }

    fn inline_code(&mut self, code: &str) {
        // Table cell inline code
        if let Some(ref mut t) = self.table {
            t.current_cell.push_str(code);
            return;
        }

        self.spans.push(Span::styled(
            format!("`{}`", code),
            Style::default().fg(theme::POLLEN),
        ));
    }

    fn soft_break(&mut self) {
        self.flush_spans();
    }

    fn hard_break(&mut self) {
        self.flush_spans();
    }

    fn rule(&mut self) {
        self.lines.push(Line::from(Span::styled(
            format!("{}────────────────────────────────────────", INDENT),
            Style::default().fg(theme::STEEL),
        )));
        self.lines.push(Line::from(""));
    }

    /// Flush accumulated spans into a Line.
    fn flush_spans(&mut self) {
        if self.spans.is_empty() {
            return;
        }
        let mut all_spans = vec![Span::raw(INDENT.to_string())];
        all_spans.append(&mut self.spans);
        self.lines.push(Line::from(all_spans));
    }

    fn emit_code_block(&mut self) {
        let lang_label = self.code_lang.take();
        let border_style = Style::default().fg(theme::STEEL);
        let code_style = Style::default().fg(theme::SLATE);

        // Top border with optional language label
        let top = if let Some(ref lang) = lang_label {
            format!("{}\u{250c}\u{2500}\u{2500} {} \u{2500}\u{2500}", INDENT, lang)
        } else {
            format!("{}\u{250c}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}", INDENT)
        };
        self.lines.push(Line::from(Span::styled(top, border_style)));

        for line in &self.code_lines {
            self.lines.push(Line::from(vec![
                Span::styled(format!("{}\u{2502} ", INDENT), border_style),
                Span::styled(line.to_string(), code_style),
            ]));
        }

        self.lines.push(Line::from(Span::styled(
            format!("{}\u{2514}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}", INDENT),
            border_style,
        )));
        self.lines.push(Line::from(""));
        self.code_lines.clear();
    }

    fn emit_table(&mut self) {
        let table = match self.table.take() {
            Some(t) => t,
            None => return,
        };

        let num_cols = table.alignments.len().max(
            table
                .header_row
                .len()
                .max(table.body_rows.first().map_or(0, |r| r.len())),
        );
        if num_cols == 0 {
            return;
        }

        // Compute column widths
        let mut widths: Vec<usize> = vec![MIN_COL_WIDTH; num_cols];
        for (i, cell) in table.header_row.iter().enumerate() {
            if i < num_cols {
                widths[i] = widths[i].max(cell.len()).min(MAX_COL_WIDTH);
            }
        }
        for row in &table.body_rows {
            for (i, cell) in row.iter().enumerate() {
                if i < num_cols {
                    widths[i] = widths[i].max(cell.len()).min(MAX_COL_WIDTH);
                }
            }
        }

        let border_style = Style::default().fg(theme::STEEL);
        let header_style = Style::default()
            .fg(theme::FROST)
            .add_modifier(Modifier::BOLD);
        let cell_style = Style::default().fg(theme::FROST);

        // Top border: ┌─────┬─────┐
        let top = format!(
            "{}\u{250c}{}\u{2510}",
            INDENT,
            widths
                .iter()
                .map(|w| "\u{2500}".repeat(w + 2))
                .collect::<Vec<_>>()
                .join("\u{252c}")
        );
        self.lines.push(Line::from(Span::styled(top, border_style)));

        // Header row
        if !table.header_row.is_empty() {
            let mut spans = vec![Span::styled(format!("{}\u{2502}", INDENT), border_style)];
            for (i, cell) in table.header_row.iter().enumerate() {
                let w = widths.get(i).copied().unwrap_or(MIN_COL_WIDTH);
                let content = fit_cell(cell, w, table.alignments.get(i));
                spans.push(Span::styled(format!(" {} ", content), header_style));
                spans.push(Span::styled("\u{2502}", border_style));
            }
            // Pad missing columns
            for i in table.header_row.len()..num_cols {
                let w = widths.get(i).copied().unwrap_or(MIN_COL_WIDTH);
                spans.push(Span::styled(" ".repeat(w + 2), header_style));
                spans.push(Span::styled("\u{2502}", border_style));
            }
            self.lines.push(Line::from(spans));

            // Separator: ├─────┼─────┤
            let sep = format!(
                "{}\u{251c}{}\u{2524}",
                INDENT,
                widths
                    .iter()
                    .map(|w| "\u{2500}".repeat(w + 2))
                    .collect::<Vec<_>>()
                    .join("\u{253c}")
            );
            self.lines.push(Line::from(Span::styled(sep, border_style)));
        }

        // Body rows
        for row in &table.body_rows {
            let mut spans = vec![Span::styled(format!("{}\u{2502}", INDENT), border_style)];
            for (i, cell) in row.iter().enumerate() {
                let w = widths.get(i).copied().unwrap_or(MIN_COL_WIDTH);
                let content = fit_cell(cell, w, table.alignments.get(i));
                spans.push(Span::styled(format!(" {} ", content), cell_style));
                spans.push(Span::styled("\u{2502}", border_style));
            }
            for i in row.len()..num_cols {
                let w = widths.get(i).copied().unwrap_or(MIN_COL_WIDTH);
                spans.push(Span::styled(" ".repeat(w + 2), cell_style));
                spans.push(Span::styled("\u{2502}", border_style));
            }
            self.lines.push(Line::from(spans));
        }

        // Bottom border: └─────┴─────┘
        let bot = format!(
            "{}\u{2514}{}\u{2518}",
            INDENT,
            widths
                .iter()
                .map(|w| "\u{2500}".repeat(w + 2))
                .collect::<Vec<_>>()
                .join("\u{2534}")
        );
        self.lines.push(Line::from(Span::styled(bot, border_style)));
        self.lines.push(Line::from(""));
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush_spans();
        self.lines
    }
}

/// Fit cell content to a fixed width, with alignment support.
fn fit_cell(text: &str, width: usize, alignment: Option<&Alignment>) -> Cow<'static, str> {
    let text = text.trim();
    let len = text.len();

    if len > width {
        // Truncate with ellipsis
        let truncated = if width > 3 {
            format!("{}...", &text[..width - 3])
        } else {
            text[..width].to_string()
        };
        return Cow::Owned(truncated);
    }

    let padding = width - len;
    match alignment.unwrap_or(&Alignment::None) {
        Alignment::Right => Cow::Owned(format!("{}{}", " ".repeat(padding), text)),
        Alignment::Center => {
            let left = padding / 2;
            let right = padding - left;
            Cow::Owned(format!("{}{}{}", " ".repeat(left), text, " ".repeat(right)))
        }
        _ => Cow::Owned(format!("{}{}", text, " ".repeat(padding))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_passthrough() {
        let lines = render_markdown("Hello world");
        assert!(!lines.is_empty());
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains("Hello world"));
    }

    #[test]
    fn bold_text() {
        let lines = render_markdown("**bold**");
        let has_bold = lines.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::BOLD) && s.content.contains("bold"))
        });
        assert!(has_bold, "Expected bold styled span");
    }

    #[test]
    fn italic_text() {
        let lines = render_markdown("*italic*");
        let has_italic = lines.iter().any(|l| {
            l.spans.iter().any(|s| {
                s.style.add_modifier.contains(Modifier::ITALIC) && s.content.contains("italic")
            })
        });
        assert!(has_italic, "Expected italic styled span");
    }

    #[test]
    fn inline_code() {
        let lines = render_markdown("use `foo` here");
        let has_code = lines.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| s.style.fg == Some(theme::POLLEN) && s.content.contains("`foo`"))
        });
        assert!(has_code, "Expected inline code span with POLLEN color");
    }

    #[test]
    fn code_block() {
        let lines = render_markdown("```rust\nfn main() {}\n```");
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains("rust"), "Expected language label");
        assert!(text.contains("fn main()"), "Expected code content");
    }

    #[test]
    fn heading_h1() {
        let lines = render_markdown("# Title");
        let has_honey = lines.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| s.style.fg == Some(theme::HONEY) && s.content.contains("Title"))
        });
        assert!(has_honey, "Expected H1 with HONEY color");
    }

    #[test]
    fn unordered_list() {
        let lines = render_markdown("- item one\n- item two");
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains("\u{2022}"), "Expected bullet character");
        assert!(text.contains("item one"));
        assert!(text.contains("item two"));
    }

    #[test]
    fn ordered_list() {
        let lines = render_markdown("1. first\n2. second");
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains("1."), "Expected numbered list");
        assert!(text.contains("first"));
    }

    #[test]
    fn horizontal_rule() {
        let lines = render_markdown("---");
        let has_rule = lines.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| s.content.contains("\u{2500}"))
        });
        assert!(has_rule, "Expected horizontal rule");
    }

    #[test]
    fn table_rendering() {
        let md = "| A | B |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |";
        let lines = render_markdown(md);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        // Should have box-drawing chars
        assert!(text.contains("\u{250c}"), "Expected top-left corner");
        assert!(text.contains("\u{2502}"), "Expected vertical bar");
        assert!(text.contains("\u{2518}"), "Expected bottom-right corner");
        // Content
        assert!(text.contains("A"));
        assert!(text.contains("1"));
    }

    #[test]
    fn link_rendering() {
        let lines = render_markdown("[click here](https://example.com)");
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains("click here"));
        assert!(text.contains("https://example.com"));
    }

    #[test]
    fn fit_cell_truncates() {
        let result = fit_cell("very long text", 8, None);
        assert_eq!(result.as_ref(), "very ...");
    }

    #[test]
    fn fit_cell_pads_left() {
        let result = fit_cell("hi", 5, None);
        assert_eq!(result.as_ref(), "hi   ");
    }

    #[test]
    fn fit_cell_right_align() {
        let result = fit_cell("hi", 5, Some(&Alignment::Right));
        assert_eq!(result.as_ref(), "   hi");
    }

    #[test]
    fn fit_cell_center_align() {
        let result = fit_cell("hi", 6, Some(&Alignment::Center));
        assert_eq!(result.as_ref(), "  hi  ");
    }

    #[test]
    fn empty_input() {
        let lines = render_markdown("");
        // Should produce no lines (or only blank)
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.trim().is_empty());
    }
}
