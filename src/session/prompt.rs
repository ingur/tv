//! Permission prompt UI.
//!
//! Self-contained popup dialog for requesting user permission when a session
//! receives read/write requests. Handles its own input, layout, and rendering.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use super::dispatch::PermissionType;
use crate::types::Request;

// === Types ===

/// Extracted prompt information for rendering, avoiding borrow conflicts.
pub struct PromptInfo {
    pub source_id: String,
    pub source_tag: Option<String>,
    pub permission_type: PermissionType,
    pub request: Request,
    pub selected: usize,
    pub expanded: bool,
    pub scroll_offset: usize,
}

pub enum PromptResult {
    Allow,
    AlwaysAllow,
    Deny,
    Navigate,
}

// === Constants ===

const PROMPT_BUTTONS: [(&str, Color); 3] = [
    ("Allow Once", Color::Green),
    ("Allow Session", Color::Blue),
    ("Deny", Color::Red),
];

const MIN_PROMPT_WIDTH: u16 = 28;
const MIN_PROMPT_HEIGHT: u16 = 7;
const MIN_RESIZE_WIDTH: u16 = 14;
const MIN_RESIZE_HEIGHT: u16 = 3;
const MAX_PROMPT_WIDTH: u16 = 52;
const HORIZONTAL_THRESHOLD: u16 = 44;

// === Input Handling ===

pub struct PromptKeyResult {
    pub result: PromptResult,
    pub selected: usize,
    pub toggle_expanded: bool,
    pub scroll_delta: i32,
}

pub fn handle_prompt_key(
    key: &KeyEvent,
    selected: usize,
    expanded: bool,
    term_width: u16,
    term_height: u16,
) -> PromptKeyResult {
    if !can_show_prompt(term_width, term_height) {
        return PromptKeyResult {
            result: PromptResult::Navigate,
            selected,
            toggle_expanded: false,
            scroll_delta: 0,
        };
    }

    if expanded {
        return handle_expanded_key(key, selected);
    }

    handle_collapsed_key(key, selected)
}

/// In expanded mode: scroll with j/k/Up/Down, Space/Esc to collapse.
fn handle_expanded_key(key: &KeyEvent, selected: usize) -> PromptKeyResult {
    match key.code {
        KeyCode::Esc | KeyCode::Char(' ') => PromptKeyResult {
            result: PromptResult::Navigate,
            selected,
            toggle_expanded: true,
            scroll_delta: 0,
        },
        KeyCode::Up | KeyCode::Char('k') => PromptKeyResult {
            result: PromptResult::Navigate,
            selected,
            toggle_expanded: false,
            scroll_delta: -1,
        },
        KeyCode::Down | KeyCode::Char('j') => PromptKeyResult {
            result: PromptResult::Navigate,
            selected,
            toggle_expanded: false,
            scroll_delta: 1,
        },
        _ => PromptKeyResult {
            result: PromptResult::Navigate,
            selected,
            toggle_expanded: false,
            scroll_delta: 0,
        },
    }
}

/// In collapsed mode: navigate buttons, Enter to select, Space to expand, Esc to deny.
fn handle_collapsed_key(key: &KeyEvent, selected: usize) -> PromptKeyResult {
    match key.code {
        KeyCode::Esc => PromptKeyResult {
            result: PromptResult::Deny,
            selected,
            toggle_expanded: false,
            scroll_delta: 0,
        },
        KeyCode::Enter => PromptKeyResult {
            result: select_result(selected),
            selected,
            toggle_expanded: false,
            scroll_delta: 0,
        },
        KeyCode::Char(' ') => PromptKeyResult {
            result: PromptResult::Navigate,
            selected,
            toggle_expanded: true,
            scroll_delta: 0,
        },
        KeyCode::Left
        | KeyCode::Char('h')
        | KeyCode::Up
        | KeyCode::BackTab
        | KeyCode::Char('k') => PromptKeyResult {
            result: PromptResult::Navigate,
            selected: prev_button(selected),
            toggle_expanded: false,
            scroll_delta: 0,
        },
        KeyCode::Right | KeyCode::Char('l') | KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => {
            PromptKeyResult {
                result: PromptResult::Navigate,
                selected: next_button(selected),
                toggle_expanded: false,
                scroll_delta: 0,
            }
        }
        _ => PromptKeyResult {
            result: PromptResult::Navigate,
            selected,
            toggle_expanded: false,
            scroll_delta: 0,
        },
    }
}

fn can_show_prompt(width: u16, height: u16) -> bool {
    width >= MIN_PROMPT_WIDTH && height >= MIN_PROMPT_HEIGHT
}

fn can_show_resize_message(width: u16, height: u16) -> bool {
    width >= MIN_RESIZE_WIDTH && height >= MIN_RESIZE_HEIGHT
}

fn select_result(selected: usize) -> PromptResult {
    match selected {
        0 => PromptResult::Allow,
        1 => PromptResult::AlwaysAllow,
        _ => PromptResult::Deny,
    }
}

fn prev_button(selected: usize) -> usize {
    selected.saturating_sub(1)
}

fn next_button(selected: usize) -> usize {
    (selected + 1).min(PROMPT_BUTTONS.len() - 1)
}

// === Syntax Highlighting ===

/// Style for `{...}` tokens — braces and inner content, bold red to pop.
const TOKEN_STYLE: Style = Style::new().fg(Color::Red).bold();

/// Convert a plain string into a Line with syntax-highlighted `{...}` tokens.
/// Base text uses `base_style`. Entire `{...}` tokens use `TOKEN_STYLE`.
fn highlight_line(text: &str, base_style: Style) -> Line<'static> {
    let mut spans = Vec::new();
    let mut rest = text;

    while let Some(open) = rest.find('{') {
        if open > 0 {
            spans.push(Span::styled(rest[..open].to_string(), base_style));
        }

        if let Some(close) = rest[open..].find('}') {
            let end = open + close + 1;
            let token = &rest[open..end];
            spans.push(Span::styled(token.to_string(), TOKEN_STYLE));
            rest = &rest[end..];
        } else {
            spans.push(Span::styled(rest[open..].to_string(), base_style));
            rest = "";
            break;
        }
    }

    if !rest.is_empty() {
        spans.push(Span::styled(rest.to_string(), base_style));
    }

    Line::from(spans)
}

// === Scroll Clamping ===

/// Compute the maximum valid scroll offset for the expanded key view.
/// Replicates the exact popup dimension math from `render_prompt` so the
/// clamp in dispatch matches what the renderer actually displays.
/// Takes the full request to resolve {stdin} content for accurate sizing.
pub fn max_scroll_offset(request: &Request, term_width: u16, term_height: u16) -> usize {
    let keys = match extract_keys(request) {
        Some(k) => k,
        None => return 0,
    };

    let popup_width = MAX_PROMPT_WIDTH.min(term_width.saturating_sub(4));
    let use_horizontal = popup_width >= HORIZONTAL_THRESHOLD;
    let popup_height = base_popup_height(use_horizontal, term_height) + 3; // +3 for has_keys

    // Inner area: popup minus borders (1 on each side)
    let inner_width = popup_width.saturating_sub(2) as usize;
    let inner_height = popup_height.saturating_sub(2) as usize;

    // Text area: inner minus 1 column for scrollbar
    let text_width = inner_width.saturating_sub(1);
    if text_width == 0 || inner_height == 0 {
        return 0;
    }

    let joined = keys.join(" ");
    let total_lines = word_wrap(&joined, text_width).len();
    total_lines.saturating_sub(inner_height)
}

// === Word Wrapping ===

/// Word-wrap text to fit within `max_width` characters.
/// Never breaks mid-word. If a single token exceeds the line width, it gets
/// its own line and is truncated with `…`.
fn word_wrap(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![];
    }

    let mut lines = Vec::new();
    let mut current_line = String::new();

    for word in text.split(' ') {
        if word.is_empty() {
            continue;
        }

        let word_len = word.chars().count();
        let current_len = current_line.chars().count();

        if current_line.is_empty() {
            // First word on the line
            if word_len <= max_width {
                current_line.push_str(word);
            } else {
                // Single token wider than line — truncate
                let truncated: String = word.chars().take(max_width.saturating_sub(1)).collect();
                lines.push(format!("{truncated}…"));
            }
        } else if current_len + 1 + word_len <= max_width {
            // Fits on current line with a space
            current_line.push(' ');
            current_line.push_str(word);
        } else {
            // Doesn't fit — start a new line
            lines.push(std::mem::take(&mut current_line));
            if word_len <= max_width {
                current_line.push_str(word);
            } else {
                let truncated: String = word.chars().take(max_width.saturating_sub(1)).collect();
                lines.push(format!("{truncated}…"));
            }
        }
    }

    if !current_line.is_empty() {
        lines.push(current_line);
    }

    lines
}

// === Rendering ===

pub fn render_prompt(frame: &mut Frame, info: &PromptInfo) {
    let area = frame.area();

    if !can_show_prompt(area.width, area.height) {
        render_blocked_overlay(frame);
        return;
    }

    dim_background(frame.buffer_mut(), area);

    let keys = extract_keys(&info.request);
    let has_keys = keys.is_some();

    let popup_width = MAX_PROMPT_WIDTH.min(area.width.saturating_sub(4));
    let use_horizontal = popup_width >= HORIZONTAL_THRESHOLD;

    // Fixed height — same whether collapsed or expanded
    let popup_height = {
        let base = base_popup_height(use_horizontal, area.height);
        if has_keys {
            base + 3
        } else {
            base
        }
    };

    let popup_area = centered_rect(popup_width, popup_height, area);

    frame.render_widget(Clear, popup_area);

    // Title
    let title = match info.permission_type {
        PermissionType::Read => " Read Request ",
        PermissionType::Write => " Write Request ",
    };

    // Bottom label (only for Exec requests with keys)
    let bottom_label = if has_keys {
        if info.expanded {
            Some(" space: hide ")
        } else {
            Some(" space: show ")
        }
    } else {
        None
    };

    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().dark_gray())
        .title(title)
        .title_style(Style::new().bold().dark_gray())
        .title_alignment(Alignment::Center);

    if let Some(label) = bottom_label {
        block = block
            .title_bottom(label)
            .title_style(Style::new().bold().dark_gray())
            .title_alignment(Alignment::Center);
    }

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    if let (true, Some(keys)) = (info.expanded, keys.as_deref()) {
        render_expanded(frame, inner, keys, info.scroll_offset);
    } else {
        let source = format_source(&info.source_id, info.source_tag.as_deref());
        let action = action_verb(&info.request);

        if use_horizontal {
            render_collapsed_horizontal(
                frame,
                inner,
                source,
                action,
                keys.as_deref(),
                info.selected,
            );
        } else {
            render_collapsed_vertical(frame, inner, source, action, keys.as_deref(), info.selected);
        }
    }
}

/// Expanded view: word-wrapped keys fill the entire inner area with a scrollbar.
fn render_expanded(frame: &mut Frame, area: Rect, keys: &[String], scroll_offset: usize) {
    if area.width < 2 || area.height < 1 {
        return;
    }

    // Reserve 1 column on the right for the scrollbar
    let text_width = (area.width.saturating_sub(1)) as usize;
    let joined = keys.join(" ");
    let wrapped = word_wrap(&joined, text_width);
    let total_lines = wrapped.len();
    let visible_height = area.height as usize;

    // Clamp scroll offset
    let max_offset = total_lines.saturating_sub(visible_height);
    let offset = scroll_offset.min(max_offset);

    // Build visible lines
    let lines: Vec<Line> = wrapped
        .iter()
        .skip(offset)
        .take(visible_height)
        .map(|s| highlight_line(s, Style::new().dark_gray()))
        .collect();

    let text_area = Rect {
        width: text_width as u16,
        ..area
    };
    frame.render_widget(Paragraph::new(lines), text_area);

    // Scrollbar (only if content overflows)
    if total_lines > visible_height {
        let track_height = area.height as usize;
        let thumb_len = (track_height * visible_height / total_lines).max(1);
        let thumb_start = if max_offset == 0 {
            0
        } else {
            offset * (track_height - thumb_len) / max_offset
        };

        let buf = frame.buffer_mut();
        let sb_x = area.right() - 1;
        for i in 0..track_height {
            let (symbol, style) = if i >= thumb_start && i < thumb_start + thumb_len {
                ("█", Style::new().gray())
            } else {
                ("│", Style::new().dark_gray())
            };
            buf.set_string(sb_x, area.top() + i as u16, symbol, style);
        }
    }
}

/// Collapsed horizontal layout (wide terminals).
fn render_collapsed_horizontal(
    frame: &mut Frame,
    area: Rect,
    source: String,
    action: &'static str,
    keys: Option<&[String]>,
    selected: usize,
) {
    let mut lines = vec![Line::from("")];

    lines.push(Line::from(vec![
        Span::styled(source, Style::new().bold().cyan()),
        Span::raw(" wants to "),
        Span::styled(action, Style::new().bold()),
    ]));

    if let Some(keys) = keys {
        lines.push(Line::from(""));

        let max_w = area.width.saturating_sub(2) as usize;
        // Lines available for key preview: total height minus header(3) and footer(2)
        let available_lines = area.height.saturating_sub(5) as usize;
        let preview_lines = wrap_key_preview(keys, max_w, available_lines);

        for line in preview_lines {
            lines.push(highlight_line(&line, Style::new().dark_gray()));
        }
    }

    lines.push(Line::from(""));
    lines.push(horizontal_buttons(selected));

    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), area);
}

/// Collapsed vertical layout (narrow terminals).
fn render_collapsed_vertical(
    frame: &mut Frame,
    area: Rect,
    source: String,
    action: &'static str,
    keys: Option<&[String]>,
    selected: usize,
) {
    let mut lines = vec![Line::from("")];

    lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled(source, Style::new().bold().cyan()),
        Span::raw(" wants to "),
        Span::styled(action, Style::new().bold()),
    ]));

    if let Some(keys) = keys {
        lines.push(Line::from(""));

        let max_w = area.width.saturating_sub(4) as usize;
        // Lines available for key preview: total height minus header(3) and footer(buttons+1)
        let button_count = PROMPT_BUTTONS.len();
        let available_lines = area.height.saturating_sub(4 + button_count as u16) as usize;
        let preview_lines = wrap_key_preview(keys, max_w, available_lines);

        for line in preview_lines {
            let mut highlighted = highlight_line(&line, Style::new().dark_gray());
            highlighted.spans.insert(0, Span::raw(" "));
            lines.push(highlighted);
        }
    }

    lines.push(Line::from(""));
    let button_width = area.width.saturating_sub(2) as usize;
    lines.extend(vertical_buttons(selected, button_width));

    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Left), area);
}

/// Word-wrap key args for the collapsed preview area.
/// Returns at most `max_lines` lines. If the text overflows, the last line ends with `…`.
fn wrap_key_preview(keys: &[String], max_width: usize, max_lines: usize) -> Vec<String> {
    if max_width == 0 || max_lines == 0 {
        return vec![];
    }

    let joined = keys.join(" ");
    let all_lines = word_wrap(&joined, max_width);

    if all_lines.len() <= max_lines {
        return all_lines;
    }

    // Truncate: take max_lines-1, then add a truncated last line with …
    let mut result: Vec<String> = all_lines.into_iter().take(max_lines).collect();
    if let Some(last) = result.last_mut() {
        let char_count = last.chars().count();
        if char_count >= max_width {
            // Already full — replace last char with …
            let truncated: String = last.chars().take(max_width.saturating_sub(1)).collect();
            *last = format!("{truncated}…");
        } else {
            last.push('…');
        }
    }
    result
}

// === Helpers ===

/// Extract display keys from request, resolving {stdin} to actual content.
/// This ensures the permission prompt shows the real text, not the placeholder.
fn extract_keys(request: &Request) -> Option<Vec<String>> {
    match request {
        Request::Exec { keys, stdin, .. } => {
            let mut display = keys.clone();
            if let Some(text) = stdin
                && let Some(pos) = display.iter().position(|k| k == "{stdin}")
            {
                display[pos] = text.clone();
            }
            Some(display)
        }
        _ => None,
    }
}

fn format_source(id: &str, tag: Option<&str>) -> String {
    tag.map(|t| format!("@{t}"))
        .unwrap_or_else(|| format!("[{id}]"))
}

fn action_verb(request: &Request) -> &'static str {
    match request {
        Request::Peek { .. } => "peek:",
        Request::History { .. } => "read history:",
        Request::Exec { .. } => "execute:",
        _ => "access",
    }
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    Rect::new(x, y, width, height)
}

fn base_popup_height(horizontal: bool, max_height: u16) -> u16 {
    let desired = if horizontal { 6 } else { 8 };
    desired.min(max_height.saturating_sub(2))
}

fn horizontal_buttons(selected: usize) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, (label, color)) in PROMPT_BUTTONS.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            format!(" {} ", label),
            button_style(i == selected, *color),
        ));
    }
    Line::from(spans)
}

fn vertical_buttons(selected: usize, width: usize) -> Vec<Line<'static>> {
    PROMPT_BUTTONS
        .iter()
        .enumerate()
        .map(|(i, (label, color))| {
            let right_pad = width.saturating_sub(label.len() + 1);
            let text = format!(" {}{}", label, " ".repeat(right_pad));
            Line::from(vec![
                Span::raw(" "),
                Span::styled(text, button_style(i == selected, *color)),
            ])
        })
        .collect()
}

fn button_style(selected: bool, color: Color) -> Style {
    if selected {
        Style::new().fg(Color::Black).bg(color).bold()
    } else {
        Style::new().fg(color)
    }
}

// === Blocked Overlay (terminal too small) ===

fn render_blocked_overlay(frame: &mut Frame) {
    let area = frame.area();
    if area.width < 2 || area.height < 2 {
        return;
    }

    dim_background(frame.buffer_mut(), area);

    if can_show_resize_message(area.width, area.height) {
        render_resize_popup(frame, area);
    } else {
        render_red_border(frame.buffer_mut(), area);
    }
}

fn render_resize_popup(frame: &mut Frame, area: Rect) {
    let width = MAX_PROMPT_WIDTH.min(area.width.saturating_sub(4));
    let height: u16 = 3;
    let popup = centered_rect(width, height.min(area.height.saturating_sub(2)), area);

    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().red());

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    if inner.height >= 1 && inner.width >= 6 {
        let base = "Resize";
        let x = (inner.width.saturating_sub(base.len() as u16)) / 2;
        frame.buffer_mut().set_string(
            inner.left() + x.saturating_sub(2),
            inner.top() + inner.height / 2,
            "↔ Resize",
            Style::new().red().bold(),
        );
    }
}

fn render_red_border(buf: &mut Buffer, area: Rect) {
    let style = Style::new().fg(Color::Red);
    let top = area.top();
    let bottom = area.bottom() - 1;
    let left = area.left();
    let right = area.right() - 1;

    buf[(left, top)].set_char('╭').set_style(style);
    buf[(right, top)].set_char('╮').set_style(style);
    buf[(left, bottom)].set_char('╰').set_style(style);
    buf[(right, bottom)].set_char('╯').set_style(style);

    for x in (left + 1)..right {
        buf[(x, top)].set_char('─').set_style(style);
        buf[(x, bottom)].set_char('─').set_style(style);
    }

    for y in (top + 1)..bottom {
        buf[(left, y)].set_char('│').set_style(style);
        buf[(right, y)].set_char('│').set_style(style);
    }
}

fn dim_background(buf: &mut Buffer, area: Rect) {
    let buf_area = buf.area;
    for y in area.top()..area.bottom().min(buf_area.bottom()) {
        for x in area.left()..area.right().min(buf_area.right()) {
            let cell = &mut buf[(x, y)];
            cell.set_style(cell.style().add_modifier(Modifier::DIM));
        }
    }
}
