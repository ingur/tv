//! Terminal view - owns the terminal rendering state and input handling.
//!
//! TerminalView encapsulates everything about the user's terminal experience:
//! - Alacritty terminal state (grid, cursor, mode)
//! - Mouse selection (click detection, drag scroll)
//! - Keyboard/mouse → PTY byte conversion
//! - Rendering (grid, cursor)

use std::io;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line as GridLine, Point};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags as CellFlags;
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, CursorShape, NamedColor, Processor};
use anyhow::Result;
use crossterm::cursor::{Hide, MoveTo, SetCursorStyle, Show};
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::terminal;
use crossterm::ExecutableCommand;
use portable_pty::PtySize;
use ratatui::prelude::*;

use super::prompt::{self, PromptInfo};

// === Terminal Events ===

/// Events from the alacritty terminal that need to be forwarded.
pub enum TerminalEvent {
    /// Data to write to the PTY (e.g., device attribute replies).
    PtyWrite(Vec<u8>),
    /// Child application set the window title (OSC 2).
    Title(String),
    /// Child application reset the window title.
    ResetTitle,
    /// Terminal bell (\x07).
    Bell,
}

// === PTY Event Proxy ===

struct PtyEventProxy {
    tx: Sender<TerminalEvent>,
}

impl PtyEventProxy {
    fn new(tx: Sender<TerminalEvent>) -> Self {
        Self { tx }
    }
}

impl EventListener for PtyEventProxy {
    fn send_event(&self, event: TermEvent) {
        let mapped = match event {
            TermEvent::PtyWrite(s) => TerminalEvent::PtyWrite(s.into_bytes()),
            TermEvent::Title(s) => TerminalEvent::Title(s),
            TermEvent::ResetTitle => TerminalEvent::ResetTitle,
            TermEvent::Bell => TerminalEvent::Bell,
            TermEvent::ClipboardStore(_ty, text) => {
                if let Ok(mut clipboard) = arboard::Clipboard::new() {
                    let _ = clipboard.set_text(text);
                }
                return;
            }
            TermEvent::ClipboardLoad(_ty, format_fn) => {
                let text = arboard::Clipboard::new()
                    .ok()
                    .and_then(|mut cb| cb.get_text().ok())
                    .unwrap_or_default();
                TerminalEvent::PtyWrite(format_fn(&text).into_bytes())
            }
            _ => return,
        };
        let _ = self.tx.send(mapped);
    }
}

// === Size ===

#[derive(Clone, Copy)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
}

impl Size {
    pub fn get() -> Self {
        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        Self { cols, rows }
    }

    pub fn pty(&self) -> PtySize {
        PtySize {
            rows: self.rows,
            cols: self.cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

impl Dimensions for Size {
    fn columns(&self) -> usize {
        self.cols as usize
    }

    fn screen_lines(&self) -> usize {
        self.rows as usize
    }

    fn total_lines(&self) -> usize {
        self.rows as usize
    }
}

// === Selection State ===

/// Scroll margin: rows from each viewport edge that trigger auto-scroll during
/// drag selection. Larger distance into the margin = faster scroll speed.
/// Computed dynamically via `scroll_margin()` to handle small terminals.
const MAX_SCROLL_MARGIN: u16 = 3;

/// Compute the scroll margin for the current terminal height.
/// Scales down for small terminals to avoid the scroll zones overlapping.
fn scroll_margin(rows: u16) -> u16 {
    (rows / 4).min(MAX_SCROLL_MARGIN)
}

/// Active auto-scroll state when dragging inside a scroll margin.
struct DragEdge {
    /// Current mouse row in the viewport.
    row: u16,
    /// Mouse column when drag entered the scroll zone. Stays fixed to prevent
    /// the selection endpoint from drifting during auto-scroll ticks.
    column: Column,
    /// Last time we scrolled. Used for throttling.
    last_scroll: Instant,
}

/// Mouse selection interaction state.
///
/// The actual selected range lives on `term.selection` (owned by alacritty).
/// This struct tracks the UI interaction around it: click detection and
/// auto-scroll during drag at viewport edges.
struct SelectionState {
    /// Timestamp of last click. For double/triple click detection.
    last_click_time: Instant,
    /// Grid point of last click. For double/triple click detection.
    last_click_point: Option<Point>,
    /// Click counter: 1=single, 2=double, 3=triple. Cycles on repeated clicks.
    click_count: u8,
    /// Auto-scroll state when dragging inside a scroll margin.
    drag_edge: Option<DragEdge>,
}

impl Default for SelectionState {
    fn default() -> Self {
        Self {
            last_click_time: Instant::now() - Duration::from_secs(10),
            last_click_point: None,
            click_count: 0,
            drag_edge: None,
        }
    }
}

// === Terminal View ===

/// Owns all terminal rendering state and input handling.
///
/// Encapsulates the alacritty terminal, VTE parser, selection state, and size.
/// Methods return PTY bytes for the session to write, keeping PTY I/O centralized.
pub struct TerminalView {
    // Alacritty terminal state
    term: Term<PtyEventProxy>,
    term_parser: Processor,
    event_rx: Receiver<TerminalEvent>,

    // Terminal dimensions
    size: Size,

    // Mouse selection interaction
    selection: SelectionState,

    // Render state
    dirty: bool,
}

impl TerminalView {
    /// Create a new TerminalView with the given size.
    pub fn new(size: Size) -> Self {
        let (event_tx, event_rx) = mpsc::channel();
        let event_proxy = PtyEventProxy::new(event_tx);
        let term = Term::new(TermConfig::default(), &size, event_proxy);
        let term_parser = Processor::new();

        Self {
            term,
            term_parser,
            event_rx,
            size,
            selection: SelectionState::default(),
            dirty: true,
        }
    }

    // === Accessors ===

    pub fn size(&self) -> Size {
        self.size
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Whether the child application wants focus events (DECSET 1004).
    pub fn wants_focus_events(&self) -> bool {
        self.term.mode().contains(TermMode::FOCUS_IN_OUT)
    }

    // === PTY Output Processing ===

    /// Feed PTY output data to the terminal renderer.
    pub fn feed_pty_output(&mut self, data: &[u8]) {
        self.term_parser.advance(&mut self.term, data);
        self.dirty = true;
    }

    /// Drain terminal events from alacritty (PTY writes, title changes, bell, etc.).
    pub fn drain_terminal_events(&mut self) -> Vec<TerminalEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.event_rx.try_recv() {
            events.push(event);
        }
        events
    }

    // === Resize ===

    /// Update terminal size. Returns the new size for the caller to resize PTY/text parser.
    pub fn resize(&mut self) -> Size {
        self.size = Size::get();
        self.term.resize(self.size);
        self.dirty = true;
        self.size
    }

    // === Input Handling ===

    /// Process a key event. Returns bytes to write to PTY, or None if handled internally.
    pub fn handle_key(&mut self, key: &KeyEvent) -> Option<Vec<u8>> {
        if key.kind != KeyEventKind::Press {
            return None;
        }

        // Clear selection on any keypress
        if self.term.selection.is_some() {
            self.term.selection = None;
            self.dirty = true;
        }

        // Snap to bottom if scrolled up in history
        if self.term.grid().display_offset() != 0 {
            self.term.scroll_display(Scroll::Bottom);
            self.dirty = true;
        }

        // Convert key to PTY bytes (mode-aware: respects APP_CURSOR)
        self.key_to_bytes(key)
    }

    /// Convert a key event to PTY bytes, respecting terminal mode flags.
    ///
    /// When the child application enables Application Cursor Keys (DECCKM),
    /// unmodified arrow/Home/End keys use SS3 encoding (`\x1bOA`) instead of
    /// CSI encoding (`\x1b[A`). Modified keys always use CSI with modifier params.
    fn key_to_bytes(&self, key: &KeyEvent) -> Option<Vec<u8>> {
        let app_cursor = self.term.mode().contains(TermMode::APP_CURSOR);
        encode_key_event(key, app_cursor)
    }

    /// Encode pasted text for the PTY. Wraps in bracketed paste sequences if
    /// the child process has requested bracketed paste mode.
    pub fn encode_paste(&mut self, text: &str) -> Vec<u8> {
        // Clear selection on paste (same as keypress)
        if self.term.selection.is_some() {
            self.term.selection = None;
            self.dirty = true;
        }

        // Snap to bottom if scrolled up
        if self.term.grid().display_offset() != 0 {
            self.term.scroll_display(Scroll::Bottom);
            self.dirty = true;
        }

        // Normalize newlines: \r\n → \r, then \n → \r (standard terminal paste behavior)
        let normalized = text.replace("\r\n", "\r").replace('\n', "\r");

        let bracketed = self.term.mode().contains(TermMode::BRACKETED_PASTE);
        let mut bytes = Vec::with_capacity(normalized.len() + 12);
        if bracketed {
            bytes.extend_from_slice(b"\x1b[200~");
        }
        bytes.extend_from_slice(normalized.as_bytes());
        if bracketed {
            bytes.extend_from_slice(b"\x1b[201~");
        }
        bytes
    }

    /// Process a mouse event. Returns bytes to write to PTY, or None if handled internally.
    pub fn handle_mouse(&mut self, mouse: &MouseEvent) -> Option<Vec<u8>> {
        // Forward to PTY if terminal wants mouse events
        if let Some(bytes) = mouse_to_bytes(mouse, self.term.mode()) {
            return Some(bytes);
        }

        // Terminal doesn't want mouse — handle selection and scrollback
        const SCROLL_LINES: i32 = 3;
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.handle_mouse_down(mouse);
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.handle_mouse_drag(mouse);
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.handle_mouse_up();
            }
            MouseEventKind::ScrollUp => {
                self.term.scroll_display(Scroll::Delta(SCROLL_LINES));
                self.dirty = true;
            }
            MouseEventKind::ScrollDown => {
                self.term.scroll_display(Scroll::Delta(-SCROLL_LINES));
                self.dirty = true;
            }
            _ => {}
        }

        None
    }

    /// Handle left mouse button down — start or extend selection.
    fn handle_mouse_down(&mut self, mouse: &MouseEvent) {
        let point = self.mouse_point(mouse);

        // Shift+Click: extend existing selection
        if mouse.modifiers.contains(KeyModifiers::SHIFT) && self.term.selection.is_some() {
            if let Some(ref mut sel) = self.term.selection {
                sel.update(point, alacritty_terminal::index::Side::Left);
            }
            self.dirty = true;
            return;
        }

        let now = Instant::now();
        let sel = &mut self.selection;

        // Detect double/triple click
        if now - sel.last_click_time < Duration::from_millis(500)
            && sel.last_click_point == Some(point)
        {
            sel.click_count = (sel.click_count % 3) + 1;
        } else {
            sel.click_count = 1;
        }
        sel.last_click_time = now;
        sel.last_click_point = Some(point);

        let sel_type = match sel.click_count {
            2 => SelectionType::Semantic,
            3 => SelectionType::Lines,
            _ => SelectionType::Simple,
        };

        let mut new_sel = Selection::new(sel_type, point, alacritty_terminal::index::Side::Left);

        // For word/line selection, expand to full boundaries
        if sel.click_count >= 2 {
            new_sel.include_all();
        }

        self.term.selection = Some(new_sel);
        self.dirty = true;
    }

    /// Handle left mouse button drag — update selection and auto-scroll.
    fn handle_mouse_drag(&mut self, mouse: &MouseEvent) {
        let point = self.mouse_point(mouse);
        if let Some(ref mut sel) = self.term.selection {
            sel.update(point, alacritty_terminal::index::Side::Left);
        }

        // Auto-scroll when dragging inside the scroll margin
        let margin = scroll_margin(self.size.rows);
        let in_scroll_zone =
            margin > 0 && (mouse.row < margin || mouse.row >= self.size.rows - margin);

        if in_scroll_zone {
            if let Some(ref mut edge) = self.selection.drag_edge {
                // Already scrolling — update position
                edge.row = mouse.row;
            } else {
                // Just entered scroll zone — start scrolling
                let now = Instant::now();
                self.selection.drag_edge = Some(DragEdge {
                    row: mouse.row,
                    column: Column(mouse.column as usize),
                    last_scroll: now,
                });
                // Immediate first scroll
                let delta = if mouse.row < margin { 1 } else { -1 };
                self.term.scroll_display(Scroll::Delta(delta));
            }
        } else {
            self.selection.drag_edge = None;
        }

        self.dirty = true;
    }

    /// Handle left mouse button up — finalize selection and copy.
    fn handle_mouse_up(&mut self) {
        self.selection.drag_edge = None;

        // Clear empty selections (single click without drag)
        if self.term.selection.as_ref().is_some_and(|s| s.is_empty()) {
            self.term.selection = None;
        } else {
            self.copy_selection();
            // Leave selection visible until next click or keypress
        }
    }

    /// Auto-scroll while dragging inside the scroll margin.
    /// Deeper into the margin = faster scroll. Always 1 line per tick.
    pub fn tick_scroll_drag(&mut self) {
        let Some(ref mut edge) = self.selection.drag_edge else {
            return;
        };

        let margin = scroll_margin(self.size.rows);
        let (distance, scrolling_up) = if edge.row < margin {
            (margin - edge.row, true)
        } else if edge.row >= self.size.rows - margin {
            (edge.row - (self.size.rows - margin - 1), false)
        } else {
            return;
        };

        // Deeper into the margin = shorter interval = faster scroll
        let interval_ms = match distance {
            1 => 120, // ~8 lines/sec
            2 => 30,  // ~33 lines/sec
            _ => 8,   // ~120 lines/sec
        };

        if edge.last_scroll.elapsed() < Duration::from_millis(interval_ms) {
            return;
        }
        edge.last_scroll = Instant::now();

        let delta = if scrolling_up { 1 } else { -1 };
        let old_offset = self.term.grid().display_offset();
        self.term.scroll_display(Scroll::Delta(delta));

        // If scroll was a no-op (no history to scroll into), don't update
        // selection — avoids jitter when drag handler and tick fight over
        // the selection endpoint.
        if self.term.grid().display_offset() == old_offset {
            return;
        }

        // Update selection end to track the scroll, using the stable mouse
        // column from when the drag entered the scroll zone (prevents drift).
        let display_offset = self.term.grid().display_offset() as i32;
        let line = GridLine(edge.row as i32 - display_offset);
        if let Some(ref mut sel) = self.term.selection {
            sel.update(
                Point::new(line, edge.column),
                alacritty_terminal::index::Side::Left,
            );
        }
        self.dirty = true;
    }

    // === Rendering ===

    /// Render terminal grid and optional prompt overlay.
    pub fn render(
        &mut self,
        rterm: &mut ratatui::Terminal<CrosstermBackend<&mut io::Stdout>>,
        prompt: Option<&PromptInfo>,
    ) -> Result<()> {
        rterm.backend_mut().execute(BeginSynchronizedUpdate)?;

        let render_result = (|| -> Result<()> {
            rterm.draw(|frame| {
                render_grid(&self.term, frame.buffer_mut());

                if let Some(info) = prompt {
                    prompt::render_prompt(frame, info);
                }
            })?;

            // Handle cursor — hide when prompt is showing
            if prompt.is_some() {
                rterm.backend_mut().execute(Hide)?;
            } else {
                render_cursor(&self.term, rterm)?;
            }

            Ok(())
        })();

        let _ = rterm.backend_mut().execute(EndSynchronizedUpdate);
        self.dirty = false;

        render_result
    }

    // === Private Helpers ===

    /// Convert mouse coordinates to an alacritty grid Point, accounting for display offset.
    fn mouse_point(&self, mouse: &MouseEvent) -> Point {
        let display_offset = self.term.grid().display_offset() as i32;
        let col = Column((mouse.column as usize).min(self.size.cols as usize - 1));
        let line = GridLine(mouse.row as i32 - display_offset);
        Point::new(line, col)
    }

    /// Copy the current selection to the system clipboard.
    fn copy_selection(&self) {
        if let Some(text) = self.term.selection_to_string()
            && !text.is_empty()
                && let Ok(mut clipboard) = arboard::Clipboard::new() {
                    let _ = clipboard.set_text(text);
                }
    }
}

// === Input Encoding ===

/// Check if the terminal is in any mouse reporting mode.
fn wants_mouse(mode: &TermMode) -> bool {
    mode.intersects(TermMode::MOUSE_REPORT_CLICK | TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION)
}

/// Encode a mouse event as bytes to send to the PTY.
///
/// Returns None if the terminal is not in mouse mode or the event can't be encoded.
/// Uses SGR encoding (mode 1006) when available, falls back to X10/normal encoding.
fn mouse_to_bytes(mouse: &MouseEvent, mode: &TermMode) -> Option<Vec<u8>> {
    if !wants_mouse(mode) {
        return None;
    }

    // Check if we should report this event type
    let should_report = match mouse.kind {
        MouseEventKind::Down(_) | MouseEventKind::Up(_) => {
            // Clicks are reported in all mouse modes
            true
        }
        MouseEventKind::Drag(_) => {
            // Drag requires MOUSE_DRAG or MOUSE_MOTION
            mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION)
        }
        MouseEventKind::Moved => {
            // Motion without button requires MOUSE_MOTION (mode 1003)
            mode.contains(TermMode::MOUSE_MOTION)
        }
        MouseEventKind::ScrollUp
        | MouseEventKind::ScrollDown
        | MouseEventKind::ScrollLeft
        | MouseEventKind::ScrollRight => {
            // Scroll is always reported if any mouse mode is active
            true
        }
    };

    if !should_report {
        return None;
    }

    // Build button code
    let (button_code, is_release) = match mouse.kind {
        MouseEventKind::Down(btn) => (button_number(btn), false),
        MouseEventKind::Up(btn) => (button_number(btn), true),
        MouseEventKind::Drag(btn) => (button_number(btn) + 32, false), // +32 for motion
        MouseEventKind::Moved => (32 + 3, false), // 3 = no button, +32 for motion
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        MouseEventKind::ScrollLeft => (66, false),
        MouseEventKind::ScrollRight => (67, false),
    };

    // Add modifier bits
    let mut code = button_code;
    if mouse.modifiers.contains(KeyModifiers::SHIFT) {
        code += 4;
    }
    if mouse.modifiers.contains(KeyModifiers::ALT) {
        code += 8;
    }
    if mouse.modifiers.contains(KeyModifiers::CONTROL) {
        code += 16;
    }

    // Coordinates are 1-based in the protocol
    let col = mouse.column + 1;
    let row = mouse.row + 1;

    if mode.contains(TermMode::SGR_MOUSE) {
        // SGR encoding: \x1b[<Cb;Cx;CyM (press) or \x1b[<Cb;Cx;Cym (release)
        let suffix = if is_release { 'm' } else { 'M' };
        Some(format!("\x1b[<{};{};{}{}", code, col, row, suffix).into_bytes())
    } else {
        // X10/Normal encoding: \x1b[M Cb Cx Cy (all as single bytes + 32)
        // This encoding can't represent coordinates > 223 or releases properly
        if is_release {
            // X10 doesn't have release events, normal mode uses button 3
            return None;
        }
        if col > 223 || row > 223 {
            return None; // Can't encode in legacy format
        }
        Some(vec![
            0x1b,
            b'[',
            b'M',
            (code + 32),
            (col + 32) as u8,
            (row + 32) as u8,
        ])
    }
}

fn button_number(btn: MouseButton) -> u8 {
    match btn {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

/// Encode a key event to PTY bytes.
///
/// When `app_cursor` is true (child enabled DECCKM), unmodified arrow and
/// Home/End keys emit SS3 sequences (`\x1bOA`) instead of CSI (`\x1b[A`).
/// Modified keys always use CSI with modifier parameters regardless of mode.
fn encode_key_event(key: &KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let has_modifiers = ctrl || shift || alt;

    let bytes = match key.code {
        // Ctrl+letter: a-z → 0x01-0x1a
        KeyCode::Char(c) if ctrl && c.is_ascii_alphabetic() => {
            let ctrl_byte = (c.to_ascii_lowercase() as u8)
                .wrapping_sub(b'a')
                .wrapping_add(1);
            if alt {
                vec![0x1b, ctrl_byte]
            } else {
                vec![ctrl_byte]
            }
        }
        KeyCode::Char(c) => {
            let mut b = [0u8; 4];
            let utf8 = c.encode_utf8(&mut b).as_bytes().to_vec();
            if alt {
                let mut v = vec![0x1b];
                v.extend_from_slice(&utf8);
                v
            } else {
                utf8
            }
        }
        KeyCode::Enter => simple_key(b'\r', alt),
        KeyCode::Backspace => simple_key(0x7f, alt),
        KeyCode::Tab => simple_key(b'\t', alt),
        KeyCode::BackTab => {
            let mut v = Vec::with_capacity(4);
            if alt {
                v.push(0x1b);
            }
            v.extend_from_slice(&[0x1b, b'[', b'Z']);
            v
        }
        KeyCode::Esc => {
            if alt {
                vec![0x1b, 0x1b]
            } else {
                vec![0x1b]
            }
        }
        KeyCode::Insert => csi_key(b"2~", ctrl, shift, alt),

        // Arrow keys — SS3 in application cursor mode (unmodified only)
        KeyCode::Up => cursor_key(b'A', app_cursor, has_modifiers, ctrl, shift, alt),
        KeyCode::Down => cursor_key(b'B', app_cursor, has_modifiers, ctrl, shift, alt),
        KeyCode::Right => cursor_key(b'C', app_cursor, has_modifiers, ctrl, shift, alt),
        KeyCode::Left => cursor_key(b'D', app_cursor, has_modifiers, ctrl, shift, alt),

        // Home/End — SS3 in application cursor mode (unmodified only)
        KeyCode::Home => cursor_key(b'H', app_cursor, has_modifiers, ctrl, shift, alt),
        KeyCode::End => cursor_key(b'F', app_cursor, has_modifiers, ctrl, shift, alt),

        KeyCode::Delete => csi_key(b"3~", ctrl, shift, alt),
        KeyCode::PageUp => csi_key(b"5~", ctrl, shift, alt),
        KeyCode::PageDown => csi_key(b"6~", ctrl, shift, alt),

        // Function keys (F1-F4 use SS3 when unmodified, CSI when modified)
        KeyCode::F(n @ 1..=4) => {
            let suffix = match n {
                1 => b'P',
                2 => b'Q',
                3 => b'R',
                _ => b'S',
            };
            let modifier = xterm_modifier(ctrl, shift, alt);
            if modifier > 1 {
                let mut v = vec![0x1b, b'[', b'1', b';'];
                v.extend_from_slice(modifier.to_string().as_bytes());
                v.push(suffix);
                v
            } else {
                vec![0x1b, b'O', suffix]
            }
        }
        // F5-F12 use CSI N ~ format
        KeyCode::F(n @ 5..=12) => {
            let num: u8 = match n {
                5 => 15,
                6 => 17,
                7 => 18,
                8 => 19,
                9 => 20,
                10 => 21,
                11 => 23,
                _ => 24,
            };
            csi_tilde(num, ctrl, shift, alt)
        }

        _ => return None,
    };

    Some(bytes)
}

/// Encode an arrow/Home/End key, respecting application cursor mode (DECCKM).
///
/// Unmodified + app_cursor: SS3 encoding (`\x1bOA`)
/// All other cases:         CSI encoding (`\x1b[A` or `\x1b[1;5A` with modifiers)
fn cursor_key(
    suffix: u8,
    app_cursor: bool,
    has_modifiers: bool,
    ctrl: bool,
    shift: bool,
    alt: bool,
) -> Vec<u8> {
    if app_cursor && !has_modifiers {
        vec![0x1b, b'O', suffix]
    } else {
        csi_key(&[suffix], ctrl, shift, alt)
    }
}

/// Encode a single-byte key with optional Alt (ESC) prefix.
fn simple_key(byte: u8, alt: bool) -> Vec<u8> {
    if alt {
        vec![0x1b, byte]
    } else {
        vec![byte]
    }
}

/// Compute the xterm modifier parameter.
/// None/1 = unmodified, 2 = Shift, 3 = Alt, 5 = Ctrl, etc.
fn xterm_modifier(ctrl: bool, shift: bool, alt: bool) -> u8 {
    1 + (shift as u8) + (alt as u8) * 2 + (ctrl as u8) * 4
}

/// Encode a CSI key sequence with optional modifier parameter.
///
/// Unmodified Up:   `\x1b[A`
/// Ctrl+Up:         `\x1b[1;5A`
/// Ctrl+Delete:     `\x1b[3;5~`
fn csi_key(suffix: &[u8], ctrl: bool, shift: bool, alt: bool) -> Vec<u8> {
    let modifier = xterm_modifier(ctrl, shift, alt);
    let mut bytes = vec![0x1b, b'['];

    if modifier > 1 {
        if suffix.len() > 1 && suffix.last() == Some(&b'~') {
            // Tilde-terminated: e.g. `3~` → `3;5~`
            bytes.extend_from_slice(&suffix[..suffix.len() - 1]);
            bytes.push(b';');
            bytes.extend_from_slice(modifier.to_string().as_bytes());
            bytes.push(b'~');
        } else {
            // Letter-terminated: e.g. `A` → `1;5A`
            bytes.push(b'1');
            bytes.push(b';');
            bytes.extend_from_slice(modifier.to_string().as_bytes());
            bytes.extend_from_slice(suffix);
        }
    } else {
        bytes.extend_from_slice(suffix);
    }

    bytes
}

/// Encode F5-F12 style keys: `\x1b[N~` or `\x1b[N;modifier~`
fn csi_tilde(num: u8, ctrl: bool, shift: bool, alt: bool) -> Vec<u8> {
    let modifier = xterm_modifier(ctrl, shift, alt);
    let mut bytes = vec![0x1b, b'['];
    bytes.extend_from_slice(num.to_string().as_bytes());
    if modifier > 1 {
        bytes.push(b';');
        bytes.extend_from_slice(modifier.to_string().as_bytes());
    }
    bytes.push(b'~');
    bytes
}

// === Grid Rendering ===

fn ansi_to_ratatui_color(c: AnsiColor) -> Color {
    match c {
        AnsiColor::Spec(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
        AnsiColor::Indexed(i) => Color::Indexed(i),
        AnsiColor::Named(n) => match n {
            NamedColor::Black | NamedColor::DimBlack => Color::Black,
            NamedColor::Red | NamedColor::DimRed => Color::Red,
            NamedColor::Green | NamedColor::DimGreen => Color::Green,
            NamedColor::Yellow | NamedColor::DimYellow => Color::Yellow,
            NamedColor::Blue | NamedColor::DimBlue => Color::Blue,
            NamedColor::Magenta | NamedColor::DimMagenta => Color::Magenta,
            NamedColor::Cyan | NamedColor::DimCyan => Color::Cyan,
            NamedColor::White | NamedColor::DimWhite => Color::White,
            NamedColor::BrightBlack => Color::DarkGray,
            NamedColor::BrightRed => Color::LightRed,
            NamedColor::BrightGreen => Color::LightGreen,
            NamedColor::BrightYellow => Color::LightYellow,
            NamedColor::BrightBlue => Color::LightBlue,
            NamedColor::BrightMagenta => Color::LightMagenta,
            NamedColor::BrightCyan => Color::LightCyan,
            NamedColor::BrightWhite | NamedColor::BrightForeground => Color::White,
            _ => Color::Reset,
        },
    }
}

fn render_grid(term: &Term<PtyEventProxy>, buf: &mut Buffer) {
    let grid = term.grid();
    let display_offset = grid.display_offset() as i32;
    let rows = buf.area.height.min(grid.screen_lines() as u16);
    let cols = buf.area.width.min(grid.columns() as u16);

    // Resolve selection range once (cheap: just a bounds computation)
    let selection = term.selection.as_ref().and_then(|sel| sel.to_range(term));

    for row in 0..rows {
        // Account for display_offset: when scrolled up, show history lines
        let line = GridLine(row as i32 - display_offset);
        for col in 0..cols {
            let cell = &grid[line][Column(col as usize)];

            // Skip spacer cells — they are covered by the preceding wide character.
            if cell
                .flags
                .intersects(CellFlags::WIDE_CHAR_SPACER | CellFlags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }

            let c = if cell.c == '\0' { ' ' } else { cell.c };
            let mut fg = ansi_to_ratatui_color(cell.fg);
            let mut bg = ansi_to_ratatui_color(cell.bg);

            // Check if this cell is selected — swap fg/bg for highlight.
            // Resolve Reset to concrete colors first, otherwise swapping
            // Reset with Reset is invisible (default-colored text).
            if let Some(ref sel) = selection
                && sel.contains(Point::new(line, Column(col as usize)))
            {
                if fg == Color::Reset {
                    fg = Color::White;
                }
                if bg == Color::Reset {
                    bg = Color::Black;
                }
                std::mem::swap(&mut fg, &mut bg);
            }

            let buf_cell = &mut buf[(col, row)];

            // Build symbol with zero-width combining characters if present
            if let Some(zerowidth) = cell.zerowidth() {
                let mut symbol = String::with_capacity(4 + zerowidth.len() * 4);
                symbol.push(c);
                for &ch in zerowidth {
                    symbol.push(ch);
                }
                buf_cell.set_symbol(&symbol);
            } else {
                buf_cell.set_char(c);
            }

            buf_cell.set_fg(fg);
            buf_cell.set_bg(bg);

            if cell.flags.contains(CellFlags::INVERSE) {
                buf_cell.set_style(buf_cell.style().add_modifier(Modifier::REVERSED));
            }
            if cell.flags.contains(CellFlags::BOLD) {
                buf_cell.set_style(buf_cell.style().add_modifier(Modifier::BOLD));
            }
            if cell.flags.contains(CellFlags::DIM) {
                buf_cell.set_style(buf_cell.style().add_modifier(Modifier::DIM));
            }
            if cell.flags.contains(CellFlags::UNDERLINE) {
                buf_cell.set_style(buf_cell.style().add_modifier(Modifier::UNDERLINED));
            }
            if cell.flags.contains(CellFlags::ITALIC) {
                buf_cell.set_style(buf_cell.style().add_modifier(Modifier::ITALIC));
            }
            if cell.flags.contains(CellFlags::STRIKEOUT) {
                buf_cell.set_style(buf_cell.style().add_modifier(Modifier::CROSSED_OUT));
            }
            if cell.flags.contains(CellFlags::HIDDEN) {
                buf_cell.set_fg(bg);
            }
        }
    }
}

fn render_cursor(
    term: &Term<PtyEventProxy>,
    rterm: &mut ratatui::Terminal<CrosstermBackend<&mut io::Stdout>>,
) -> Result<()> {
    let grid = term.grid();
    let display_offset = grid.display_offset();
    let screen_lines = grid.screen_lines();

    let cursor_point = grid.cursor.point;
    let visual_row = cursor_point.line.0 as usize + display_offset;

    // Hide cursor if it's scrolled off the bottom of the viewport
    if visual_row >= screen_lines {
        rterm.backend_mut().execute(Hide)?;
        return Ok(());
    }

    let cursor_style = term.cursor_style();
    let show_cursor =
        term.mode().contains(TermMode::SHOW_CURSOR) && cursor_style.shape != CursorShape::Hidden;

    if show_cursor {
        rterm
            .backend_mut()
            .execute(MoveTo(cursor_point.column.0 as u16, visual_row as u16))?;

        let crossterm_style = match cursor_style.shape {
            CursorShape::Block => {
                if cursor_style.blinking {
                    SetCursorStyle::BlinkingBlock
                } else {
                    SetCursorStyle::SteadyBlock
                }
            }
            CursorShape::Underline => {
                if cursor_style.blinking {
                    SetCursorStyle::BlinkingUnderScore
                } else {
                    SetCursorStyle::SteadyUnderScore
                }
            }
            CursorShape::Beam => {
                if cursor_style.blinking {
                    SetCursorStyle::BlinkingBar
                } else {
                    SetCursorStyle::SteadyBar
                }
            }
            CursorShape::HollowBlock | CursorShape::Hidden => SetCursorStyle::SteadyBlock,
        };
        rterm.backend_mut().execute(crossterm_style)?;
        rterm.backend_mut().execute(Show)?;
    } else {
        rterm.backend_mut().execute(Hide)?;
    }

    Ok(())
}
