//! Plain text VTE parser for history tracking.

use std::collections::VecDeque;

use alacritty_terminal::vte::{Params, Perform};

const CHAR_BACKSPACE: u8 = 0x08;
const CHAR_TAB: u8 = 0x09;
const CHAR_NEWLINE: u8 = 0x0A;
const CHAR_CARRIAGE_RETURN: u8 = 0x0D;

pub struct HistoryBuffer {
    lines: VecDeque<String>,
    max_lines: usize,
    base_index: usize,
}

impl HistoryBuffer {
    pub fn new(max_lines: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            max_lines,
            base_index: 0,
        }
    }

    pub fn push(&mut self, line: String) {
        self.lines.push_back(line);
        if self.lines.len() > self.max_lines {
            self.lines.pop_front();
            self.base_index += 1;
        }
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        // Keep base_index so future absolute indices remain valid
    }
}

struct SavedScreen {
    cells: Vec<Vec<char>>,
    cursor: (usize, usize),
}

struct Screen {
    cells: Vec<Vec<char>>,
    cursor_row: usize,
    cursor_col: usize,
    rows: usize,
    cols: usize,

    alt_cells: Vec<Vec<char>>,
    alt_cursor_row: usize,
    alt_cursor_col: usize,
    in_alt_screen: bool,
    saved_screen: Option<SavedScreen>,

    scroll_top: usize,
    scroll_bottom: usize,
    saved_cursor: Option<(usize, usize)>,
}

impl Screen {
    fn new(rows: usize, cols: usize) -> Self {
        Self {
            cells: vec![vec![' '; cols]; rows],
            cursor_row: 0,
            cursor_col: 0,
            rows,
            cols,
            alt_cells: vec![vec![' '; cols]; rows],
            alt_cursor_row: 0,
            alt_cursor_col: 0,
            in_alt_screen: false,
            saved_screen: None,
            scroll_top: 0,
            scroll_bottom: rows.saturating_sub(1),
            saved_cursor: None,
        }
    }

    fn cursor(&self) -> (usize, usize) {
        if self.in_alt_screen {
            (self.alt_cursor_row, self.alt_cursor_col)
        } else {
            (self.cursor_row, self.cursor_col)
        }
    }

    fn set_cursor(&mut self, row: usize, col: usize) {
        let row = row.min(self.rows.saturating_sub(1));
        let col = col.min(self.cols.saturating_sub(1));
        if self.in_alt_screen {
            self.alt_cursor_row = row;
            self.alt_cursor_col = col;
        } else {
            self.cursor_row = row;
            self.cursor_col = col;
        }
    }

    fn save_cursor(&mut self) {
        self.saved_cursor = Some(self.cursor());
    }

    fn restore_cursor(&mut self) {
        if let Some((row, col)) = self.saved_cursor {
            self.set_cursor(row, col);
        }
    }

    fn scroll_region(&self) -> (usize, usize) {
        (self.scroll_top, self.scroll_bottom)
    }

    fn set_scroll_region(&mut self, top: usize, bottom: usize) {
        let top = top.min(self.rows.saturating_sub(1));
        let bottom = bottom.min(self.rows.saturating_sub(1));
        if top < bottom {
            self.scroll_top = top;
            self.scroll_bottom = bottom;
        }
    }

    fn active_cells(&mut self) -> &mut Vec<Vec<char>> {
        if self.in_alt_screen {
            &mut self.alt_cells
        } else {
            &mut self.cells
        }
    }

    fn row_to_string(&self, row: usize) -> String {
        let cells = if self.in_alt_screen {
            &self.alt_cells
        } else {
            &self.cells
        };
        if row < cells.len() {
            cells[row].iter().collect::<String>().trim_end().to_string()
        } else {
            String::new()
        }
    }

    fn enter_alt_screen(&mut self) {
        if self.in_alt_screen {
            return;
        }
        self.saved_screen = Some(SavedScreen {
            cells: self.cells.clone(),
            cursor: (self.cursor_row, self.cursor_col),
        });
        self.alt_cells = vec![vec![' '; self.cols]; self.rows];
        self.alt_cursor_row = 0;
        self.alt_cursor_col = 0;
        self.in_alt_screen = true;
    }

    fn leave_alt_screen(&mut self) {
        if !self.in_alt_screen {
            return;
        }
        if let Some(saved) = self.saved_screen.take() {
            self.cells = saved.cells;
            self.cursor_row = saved.cursor.0;
            self.cursor_col = saved.cursor.1;
        }
        self.in_alt_screen = false;
    }

    fn resize(&mut self, rows: usize, cols: usize) {
        self.rows = rows;
        self.cols = cols;

        self.cells.resize(rows, vec![' '; cols]);
        for row in &mut self.cells {
            row.resize(cols, ' ');
        }

        self.alt_cells.resize(rows, vec![' '; cols]);
        for row in &mut self.alt_cells {
            row.resize(cols, ' ');
        }

        if let Some(ref mut saved) = self.saved_screen {
            saved.cells.resize(rows, vec![' '; cols]);
            for row in &mut saved.cells {
                row.resize(cols, ' ');
            }
            saved.cursor.0 = saved.cursor.0.min(rows.saturating_sub(1));
            saved.cursor.1 = saved.cursor.1.min(cols.saturating_sub(1));
        }

        self.cursor_row = self.cursor_row.min(rows.saturating_sub(1));
        self.cursor_col = self.cursor_col.min(cols.saturating_sub(1));
        self.alt_cursor_row = self.alt_cursor_row.min(rows.saturating_sub(1));
        self.alt_cursor_col = self.alt_cursor_col.min(cols.saturating_sub(1));

        if let Some((ref mut r, ref mut c)) = self.saved_cursor {
            *r = (*r).min(rows.saturating_sub(1));
            *c = (*c).min(cols.saturating_sub(1));
        }

        self.scroll_top = 0;
        self.scroll_bottom = rows.saturating_sub(1);
    }
}

pub struct TextParser {
    screen: Screen,
    history: HistoryBuffer,
    vte_parser: alacritty_terminal::vte::Parser,
}

impl TextParser {
    pub fn new(rows: usize, cols: usize, history_size: usize) -> Self {
        Self {
            screen: Screen::new(rows, cols),
            history: HistoryBuffer::new(history_size),
            vte_parser: alacritty_terminal::vte::Parser::new(),
        }
    }

    pub fn advance(&mut self, data: &[u8]) {
        let mut parser = std::mem::take(&mut self.vte_parser);
        parser.advance(self, data);
        self.vte_parser = parser;
    }

    pub fn resize(&mut self, rows: usize, cols: usize) {
        self.screen.resize(rows, cols);
    }

    pub fn render_screen(&self) -> String {
        let mut lines: Vec<String> = (0..self.screen.rows)
            .map(|row| self.screen.row_to_string(row))
            .collect();

        // Trim trailing empty lines
        while lines.last().map(|s| s.is_empty()).unwrap_or(false) {
            lines.pop();
        }

        lines.join("\n")
    }

    pub fn get_history(&self, count: Option<usize>, offset: Option<usize>) -> String {
        // Calculate screen line count based on actual content, not total screen height
        let screen_line_count = if self.screen.in_alt_screen {
            self.screen
                .saved_screen
                .as_ref()
                .map(|s| s.cells.len())
                .unwrap_or(0)
        } else {
            // Use cursor row + 1 as content height (content is rows 0..=cursor_row)
            let (cursor_row, _) = self.screen.cursor();
            cursor_row + 1
        };

        let total_scrollback = self.history.lines.len();
        let total = total_scrollback + screen_line_count;

        let offset = offset.unwrap_or(0);
        let count = count.unwrap_or(total);

        if offset >= total {
            return String::new();
        }

        // Calculate range from the end (most recent)
        let end = total.saturating_sub(offset);
        let start = end.saturating_sub(count);

        // Check if we need screen lines at all
        let needs_screen_lines = end > total_scrollback;

        // Only allocate screen lines if we actually need them
        let screen_lines: Vec<String> = if needs_screen_lines {
            if self.screen.in_alt_screen {
                if let Some(ref saved) = self.screen.saved_screen {
                    saved
                        .cells
                        .iter()
                        .map(|row| row.iter().collect::<String>().trim_end().to_string())
                        .collect()
                } else {
                    Vec::new()
                }
            } else {
                (0..self.screen.rows)
                    .map(|row| self.screen.row_to_string(row))
                    .collect()
            }
        } else {
            Vec::new()
        };

        // Pre-allocate result with exact capacity
        let mut result: Vec<&str> = Vec::with_capacity(end - start);

        for i in start..end {
            if i < total_scrollback {
                if let Some(line) = self.history.lines.get(i) {
                    result.push(line.as_str());
                }
            } else {
                let screen_idx = i - total_scrollback;
                if let Some(line) = screen_lines.get(screen_idx) {
                    result.push(line.as_str());
                }
            }
        }

        // Trim trailing empty lines
        while result.last().map(|s| s.is_empty()).unwrap_or(false) {
            result.pop();
        }

        result.join("\n")
    }

    pub fn clear(&mut self) {
        self.history.clear();
    }

    pub fn in_alt_screen(&self) -> bool {
        self.screen.in_alt_screen
    }

    /// Current cursor position (row, col).
    pub fn cursor(&self) -> (usize, usize) {
        self.screen.cursor()
    }

    /// Screen dimensions (rows, cols).
    pub fn dimensions(&self) -> (usize, usize) {
        (self.screen.rows, self.screen.cols)
    }

    fn scroll_up(&mut self) {
        let (scroll_top, scroll_bottom) = self.screen.scroll_region();
        let cols = self.screen.cols;

        if scroll_top == 0 && !self.screen.in_alt_screen {
            let line = self.screen.row_to_string(0);
            self.history.push(line);
        }

        let cells = self.screen.active_cells();
        if scroll_top < cells.len() {
            cells.remove(scroll_top);
        }
        let insert_pos = scroll_bottom.min(cells.len());
        cells.insert(insert_pos, vec![' '; cols]);
    }

    fn scroll_down(&mut self) {
        let (scroll_top, scroll_bottom) = self.screen.scroll_region();
        let cols = self.screen.cols;

        let cells = self.screen.active_cells();
        if scroll_bottom < cells.len() {
            cells.remove(scroll_bottom);
        }
        cells.insert(scroll_top, vec![' '; cols]);
    }

    fn insert_lines(&mut self, n: usize) {
        let (cursor_row, _) = self.screen.cursor();
        let (_, scroll_bottom) = self.screen.scroll_region();
        let cols = self.screen.cols;
        let cells = self.screen.active_cells();

        for _ in 0..n {
            if scroll_bottom < cells.len() {
                cells.remove(scroll_bottom);
            }
            if cursor_row <= cells.len() {
                cells.insert(cursor_row, vec![' '; cols]);
            }
        }
    }

    fn delete_lines(&mut self, n: usize) {
        let (cursor_row, _) = self.screen.cursor();
        let (_, scroll_bottom) = self.screen.scroll_region();
        let cols = self.screen.cols;
        let cells = self.screen.active_cells();

        for _ in 0..n {
            if cursor_row < cells.len() {
                cells.remove(cursor_row);
            }
            let insert_pos = scroll_bottom.min(cells.len());
            cells.insert(insert_pos, vec![' '; cols]);
        }
    }

    fn erase_chars(&mut self, n: usize) {
        let (cursor_row, cursor_col) = self.screen.cursor();
        let cells = self.screen.active_cells();
        if cursor_row >= cells.len() {
            return;
        }
        let cols = cells[cursor_row].len();

        for i in 0..n {
            let col = cursor_col + i;
            if col < cols {
                cells[cursor_row][col] = ' ';
            }
        }
    }

    fn insert_chars(&mut self, n: usize) {
        let (cursor_row, cursor_col) = self.screen.cursor();
        let cells = self.screen.active_cells();
        if cursor_row >= cells.len() {
            return;
        }
        let cols = cells[cursor_row].len();

        for _ in 0..n {
            if cursor_col < cols {
                cells[cursor_row].pop();
                cells[cursor_row].insert(cursor_col, ' ');
            }
        }
    }

    fn delete_chars(&mut self, n: usize) {
        let (cursor_row, cursor_col) = self.screen.cursor();
        let cells = self.screen.active_cells();
        if cursor_row >= cells.len() {
            return;
        }
        let cols = cells[cursor_row].len();

        for _ in 0..n {
            if cursor_col < cells[cursor_row].len() {
                cells[cursor_row].remove(cursor_col);
                cells[cursor_row].push(' ');
            }
        }
        while cells[cursor_row].len() < cols {
            cells[cursor_row].push(' ');
        }
    }

    fn erase_line(&mut self, mode: usize) {
        let (cursor_row, cursor_col) = self.screen.cursor();
        let cells = self.screen.active_cells();
        if cursor_row >= cells.len() {
            return;
        }

        match mode {
            0 => {
                for col in cursor_col..cells[cursor_row].len() {
                    cells[cursor_row][col] = ' ';
                }
            }
            1 => {
                for col in 0..=cursor_col.min(cells[cursor_row].len().saturating_sub(1)) {
                    cells[cursor_row][col] = ' ';
                }
            }
            2 => {
                for col in 0..cells[cursor_row].len() {
                    cells[cursor_row][col] = ' ';
                }
            }
            _ => {}
        }
    }

    fn erase_display(&mut self, mode: usize) {
        let (cursor_row, _) = self.screen.cursor();

        match mode {
            0 => {
                self.erase_line(0);
                let cells = self.screen.active_cells();
                for row in cells.iter_mut().skip(cursor_row + 1) {
                    for cell in row.iter_mut() {
                        *cell = ' ';
                    }
                }
            }
            1 => {
                let cells = self.screen.active_cells();
                for row in cells.iter_mut().take(cursor_row) {
                    for cell in row.iter_mut() {
                        *cell = ' ';
                    }
                }
                self.erase_line(1);
            }
            2 => {
                if !self.screen.in_alt_screen {
                    let (cursor_row, _) = self.screen.cursor();
                    for row in 0..=cursor_row.min(self.screen.rows.saturating_sub(1)) {
                        let line = self.screen.row_to_string(row);
                        self.history.push(line);
                    }
                }
                let cells = self.screen.active_cells();
                for row in cells.iter_mut() {
                    for cell in row.iter_mut() {
                        *cell = ' ';
                    }
                }
            }
            3 => {
                // Erase scrollback - intentionally preserve our history
            }
            _ => {}
        }
    }
}

impl Perform for TextParser {
    fn print(&mut self, c: char) {
        let (mut row, mut col) = self.screen.cursor();
        let cols = self.screen.cols;
        let (_, scroll_bottom) = self.screen.scroll_region();

        if col >= cols {
            col = 0;
            row += 1;
        }

        if row > scroll_bottom {
            self.scroll_up();
            row = scroll_bottom;
        }

        self.screen.active_cells()[row][col] = c;
        self.screen.set_cursor(row, col + 1);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            CHAR_BACKSPACE => {
                let (row, col) = self.screen.cursor();
                if col > 0 {
                    self.screen.set_cursor(row, col - 1);
                }
            }
            CHAR_TAB => {
                let (row, col) = self.screen.cursor();
                let next_tab = ((col + 8) / 8) * 8;
                self.screen
                    .set_cursor(row, next_tab.min(self.screen.cols.saturating_sub(1)));
            }
            CHAR_NEWLINE => {
                let (row, col) = self.screen.cursor();
                let (_, scroll_bottom) = self.screen.scroll_region();
                if row >= scroll_bottom {
                    self.scroll_up();
                    self.screen.set_cursor(scroll_bottom, col);
                } else {
                    self.screen.set_cursor(row + 1, col);
                }
            }
            CHAR_CARRIAGE_RETURN => {
                let (row, _) = self.screen.cursor();
                self.screen.set_cursor(row, 0);
            }
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        let param = |idx: usize, default: usize| -> usize {
            params
                .iter()
                .nth(idx)
                .and_then(|p| p.first().copied())
                .map(|v| if v == 0 { default } else { v as usize })
                .unwrap_or(default)
        };

        let is_private = intermediates.first() == Some(&b'?');

        match action {
            'A' => {
                let (row, col) = self.screen.cursor();
                self.screen.set_cursor(row.saturating_sub(param(0, 1)), col);
            }
            'B' | 'e' => {
                let (row, col) = self.screen.cursor();
                self.screen.set_cursor(row + param(0, 1), col);
            }
            'C' | 'a' => {
                let (row, col) = self.screen.cursor();
                self.screen.set_cursor(row, col + param(0, 1));
            }
            'D' => {
                let (row, col) = self.screen.cursor();
                self.screen.set_cursor(row, col.saturating_sub(param(0, 1)));
            }
            'E' => {
                let (row, _) = self.screen.cursor();
                self.screen.set_cursor(row + param(0, 1), 0);
            }
            'F' => {
                let (row, _) = self.screen.cursor();
                self.screen.set_cursor(row.saturating_sub(param(0, 1)), 0);
            }
            'G' | '`' => {
                let (row, _) = self.screen.cursor();
                self.screen.set_cursor(row, param(0, 1).saturating_sub(1));
            }
            'H' | 'f' => {
                let row = param(0, 1).saturating_sub(1);
                let col = param(1, 1).saturating_sub(1);
                self.screen.set_cursor(row, col);
            }
            'd' => {
                let (_, col) = self.screen.cursor();
                self.screen.set_cursor(param(0, 1).saturating_sub(1), col);
            }
            's' if intermediates.is_empty() => self.screen.save_cursor(),
            'u' if intermediates.is_empty() => self.screen.restore_cursor(),
            'J' => self.erase_display(param(0, 0)),
            'K' => self.erase_line(param(0, 0)),
            'S' => {
                for _ in 0..param(0, 1) {
                    self.scroll_up();
                }
            }
            'T' if intermediates.is_empty() => {
                for _ in 0..param(0, 1) {
                    self.scroll_down();
                }
            }
            'L' => self.insert_lines(param(0, 1)),
            'M' => self.delete_lines(param(0, 1)),
            'X' => self.erase_chars(param(0, 1)),
            '@' => self.insert_chars(param(0, 1)),
            'P' => self.delete_chars(param(0, 1)),
            'r' if intermediates.is_empty() => {
                let top = param(0, 1).saturating_sub(1);
                let bottom = param(1, self.screen.rows).saturating_sub(1);
                self.screen.set_scroll_region(top, bottom);
                self.screen.set_cursor(0, 0);
            }
            'h' if is_private => {
                let mode = param(0, 0);
                if mode == 1049 || mode == 47 || mode == 1047 {
                    self.screen.enter_alt_screen();
                }
            }
            'l' if is_private => {
                let mode = param(0, 0);
                if mode == 1049 || mode == 47 || mode == 1047 {
                    self.screen.leave_alt_screen();
                }
            }
            'm' => {} // SGR - ignore styling
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {
        // OSC sequences are ignored — we only track plain text content.
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        if intermediates.is_empty() {
            match byte {
                b'7' => self.screen.save_cursor(),
                b'8' => self.screen.restore_cursor(),
                b'D' => {
                    let (row, col) = self.screen.cursor();
                    let (_, scroll_bottom) = self.screen.scroll_region();
                    if row >= scroll_bottom {
                        self.scroll_up();
                    } else {
                        self.screen.set_cursor(row + 1, col);
                    }
                }
                b'M' => {
                    let (row, col) = self.screen.cursor();
                    let (scroll_top, _) = self.screen.scroll_region();
                    if row <= scroll_top {
                        self.scroll_down();
                    } else {
                        self.screen.set_cursor(row - 1, col);
                    }
                }
                b'E' => {
                    let (row, _) = self.screen.cursor();
                    let (_, scroll_bottom) = self.screen.scroll_region();
                    if row >= scroll_bottom {
                        self.scroll_up();
                        self.screen.set_cursor(scroll_bottom, 0);
                    } else {
                        self.screen.set_cursor(row + 1, 0);
                    }
                }
                _ => {}
            }
        }
    }

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: char) {}
    fn put(&mut self, _byte: u8) {}
    fn unhook(&mut self) {}
}
