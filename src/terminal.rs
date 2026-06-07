//! Terminal grid model and VT/ANSI parser.
//!
//! `Term` implements [`vte::Perform`], so feeding it the bytes coming out of the
//! PTY incrementally builds a grid of [`Cell`]s that the renderer draws. We
//! implement a pragmatic subset of xterm: enough for PowerShell, colored
//! prompts, line editing, and `clear`.

use std::collections::VecDeque;

use vte::{Params, Perform};

/// Default number of scrolled-off lines kept for scrollback.
pub const DEFAULT_SCROLLBACK: usize = 5000;

/// A color as it appears in an SGR sequence. Resolved to concrete RGB at draw
/// time so theme changes don't require re-parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    /// The configured default fg/bg.
    Default,
    /// 0..=255 xterm palette index.
    Indexed(u8),
    /// 24-bit truecolor.
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, Copy)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub inverse: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            fg: Color::Default,
            bg: Color::Default,
            bold: false,
            inverse: false,
        }
    }
}

#[derive(Clone, Copy)]
struct Pen {
    fg: Color,
    bg: Color,
    bold: bool,
    inverse: bool,
}

impl Default for Pen {
    fn default() -> Self {
        Self {
            fg: Color::Default,
            bg: Color::Default,
            bold: false,
            inverse: false,
        }
    }
}

pub struct Term {
    pub cols: usize,
    pub rows: usize,
    pub cells: Vec<Cell>,
    pub cursor_x: usize,
    pub cursor_y: usize,
    pub cursor_visible: bool,
    /// Bumped on every mutation so the renderer can skip redraws when idle.
    pub dirty: u64,

    pen: Pen,
    saved_cursor: (usize, usize),
    // Scroll region (inclusive), defaults to whole screen.
    scroll_top: usize,
    scroll_bot: usize,
    // Pending wrap: cursor is "past" the last column.
    wrap_next: bool,

    /// Lines that have scrolled off the top, oldest first. Each row is `cols`
    /// wide at the time it was captured.
    scrollback: VecDeque<Vec<Cell>>,
    scrollback_max: usize,
    /// Lines the viewport is scrolled back from the live bottom; 0 = following.
    view_offset: usize,
}

impl Term {
    pub fn new(cols: usize, rows: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Self {
            cols,
            rows,
            cells: vec![Cell::default(); cols * rows],
            cursor_x: 0,
            cursor_y: 0,
            cursor_visible: true,
            dirty: 0,
            pen: Pen::default(),
            saved_cursor: (0, 0),
            scroll_top: 0,
            scroll_bot: rows - 1,
            wrap_next: false,
            scrollback: VecDeque::new(),
            scrollback_max: DEFAULT_SCROLLBACK,
            view_offset: 0,
        }
    }

    /// Set how many scrolled-off lines to retain. Trims existing history if the
    /// new cap is smaller.
    pub fn set_scrollback_max(&mut self, max: usize) {
        self.scrollback_max = max;
        while self.scrollback.len() > self.scrollback_max {
            self.scrollback.pop_front();
        }
        self.view_offset = self.view_offset.min(self.scrollback.len());
    }

    #[inline]
    pub fn scrolled(&self) -> bool {
        self.view_offset > 0
    }

    /// Positive scrolls toward older output, negative toward the live screen.
    pub fn scroll_view(&mut self, delta: isize) {
        let max = self.scrollback.len() as isize;
        let next = (self.view_offset as isize + delta).clamp(0, max) as usize;
        if next != self.view_offset {
            self.view_offset = next;
            self.dirty += 1;
        }
    }

    pub fn scroll_to_bottom(&mut self) {
        if self.view_offset != 0 {
            self.view_offset = 0;
            self.dirty += 1;
        }
    }

    /// The cell shown at viewport position `(x, y)`, resolving the scrollback
    /// view. When scrolled back, the upper rows come from history.
    #[inline]
    pub fn view_cell(&self, x: usize, y: usize) -> Cell {
        if self.view_offset == 0 {
            return self.cells[y * self.cols + x];
        }
        let sb = self.scrollback.len();
        // Absolute line index in the [history.., live screen] timeline.
        let abs = (sb - self.view_offset) + y;
        if abs < sb {
            // History rows may have a different width (captured pre-resize).
            self.scrollback[abs].get(x).copied().unwrap_or_default()
        } else {
            self.cells[(abs - sb) * self.cols + x]
        }
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }
        // Naive reflow: copy the top-left overlap. Full reflow is out of scope.
        let mut new = vec![Cell::default(); cols * rows];
        for y in 0..rows.min(self.rows) {
            for x in 0..cols.min(self.cols) {
                new[y * cols + x] = self.cells[y * self.cols + x];
            }
        }
        self.cells = new;
        self.cols = cols;
        self.rows = rows;
        self.cursor_x = self.cursor_x.min(cols - 1);
        self.cursor_y = self.cursor_y.min(rows - 1);
        self.scroll_top = 0;
        self.scroll_bot = rows - 1;
        self.wrap_next = false;
        // History keeps its old width (view_cell bounds-checks); just snap back
        // to the live screen so the new geometry isn't shown mid-scroll.
        self.view_offset = 0;
        self.dirty += 1;
    }

    #[inline]
    fn idx(&self, x: usize, y: usize) -> usize {
        y * self.cols + x
    }

    /// A blank cell carrying the current background pen (so erased regions keep
    /// the active background color, as xterm does).
    #[inline]
    fn blank_cell(&self) -> Cell {
        Cell {
            bg: self.pen.bg,
            ..Cell::default()
        }
    }

    fn clear_region(&mut self, start: usize, end: usize) {
        let blank = self.blank_cell();
        self.cells[start..end].fill(blank);
    }

    /// Blank whole rows `[top, bot]` (inclusive).
    fn clear_rows(&mut self, top: usize, bot: usize) {
        let (start, end) = (self.idx(0, top), self.idx(0, bot) + self.cols);
        self.clear_region(start, end);
    }

    fn scroll_up(&mut self, n: usize) {
        let n = n.min(self.scroll_bot - self.scroll_top + 1);
        let (top, bot) = (self.scroll_top, self.scroll_bot);
        // Only normal full-height scrolling (region anchored at the top) feeds
        // scrollback; a restricted scroll region is an app drawing in place.
        if top == 0 && self.scrollback_max > 0 {
            for y in 0..n {
                let start = self.idx(0, y);
                self.scrollback
                    .push_back(self.cells[start..start + self.cols].to_vec());
            }
            while self.scrollback.len() > self.scrollback_max {
                self.scrollback.pop_front();
            }
            // Keep the viewport pinned to the same content while scrolled back.
            if self.view_offset > 0 {
                self.view_offset = (self.view_offset + n).min(self.scrollback.len());
            }
        }
        for y in top..=(bot - n) {
            let (dst, src) = (self.idx(0, y), self.idx(0, y + n));
            self.cells.copy_within(src..src + self.cols, dst);
        }
        self.clear_rows(bot - n + 1, bot);
    }

    fn scroll_down(&mut self, n: usize) {
        let n = n.min(self.scroll_bot - self.scroll_top + 1);
        let (top, bot) = (self.scroll_top, self.scroll_bot);
        for y in ((top + n)..=bot).rev() {
            let (dst, src) = (self.idx(0, y), self.idx(0, y - n));
            self.cells.copy_within(src..src + self.cols, dst);
        }
        self.clear_rows(top, top + n - 1);
    }

    fn newline(&mut self) {
        if self.cursor_y == self.scroll_bot {
            self.scroll_up(1);
        } else if self.cursor_y < self.rows - 1 {
            self.cursor_y += 1;
        }
    }

    fn put_char(&mut self, c: char) {
        if self.wrap_next {
            self.cursor_x = 0;
            self.newline();
            self.wrap_next = false;
        }
        let i = self.idx(self.cursor_x, self.cursor_y);
        self.cells[i] = Cell {
            ch: c,
            fg: self.pen.fg,
            bg: self.pen.bg,
            bold: self.pen.bold,
            inverse: self.pen.inverse,
        };
        if self.cursor_x + 1 >= self.cols {
            self.wrap_next = true;
        } else {
            self.cursor_x += 1;
        }
    }

    /// SGR: colors and text attributes. No params means reset.
    fn sgr(&mut self, params: &Params) {
        let mut iter = params.iter();
        let mut any = false;
        while let Some(p) = iter.next() {
            any = true;
            let code = p[0];
            match code {
                0 => self.pen = Pen::default(),
                1 => self.pen.bold = true,
                22 => self.pen.bold = false,
                7 => self.pen.inverse = true,
                27 => self.pen.inverse = false,
                30..=37 => self.pen.fg = Color::Indexed((code - 30) as u8),
                40..=47 => self.pen.bg = Color::Indexed((code - 40) as u8),
                90..=97 => self.pen.fg = Color::Indexed((code - 90 + 8) as u8),
                100..=107 => self.pen.bg = Color::Indexed((code - 100 + 8) as u8),
                39 => self.pen.fg = Color::Default,
                49 => self.pen.bg = Color::Default,
                38 | 48 => {
                    // Extended color. Sub-params may be packed in this `p` slice
                    // (colon form) or spread across following params (semicolon).
                    let is_fg = code == 38;
                    let col = if p.len() >= 2 {
                        parse_extended(&p[1..])
                    } else {
                        parse_extended_spread(&mut iter)
                    };
                    if let Some(c) = col {
                        if is_fg {
                            self.pen.fg = c;
                        } else {
                            self.pen.bg = c;
                        }
                    }
                }
                _ => {}
            }
        }
        if !any {
            self.pen = Pen::default();
        }
    }
}

/// Parse `5;n` (indexed) or `2;r;g;b` (rgb) from a single colon-packed param.
fn parse_extended(sub: &[u16]) -> Option<Color> {
    match sub.first()? {
        5 => sub.get(1).map(|n| Color::Indexed(*n as u8)),
        2 => {
            // sub = [2, r, g, b] possibly with a colorspace id; take last 3.
            let n = sub.len();
            if n >= 4 {
                Some(Color::Rgb(
                    sub[n - 3] as u8,
                    sub[n - 2] as u8,
                    sub[n - 1] as u8,
                ))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Parse the semicolon-spread form where 38;5;n / 38;2;r;g;b arrive as separate
/// top-level params.
fn parse_extended_spread(iter: &mut vte::ParamsIter) -> Option<Color> {
    let kind = iter.next()?[0];
    match kind {
        5 => Some(Color::Indexed(iter.next()?[0] as u8)),
        2 => {
            let r = iter.next()?[0] as u8;
            let g = iter.next()?[0] as u8;
            let b = iter.next()?[0] as u8;
            Some(Color::Rgb(r, g, b))
        }
        _ => None,
    }
}

impl Perform for Term {
    fn print(&mut self, c: char) {
        self.put_char(c);
        self.dirty += 1;
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => {
                self.newline();
                self.wrap_next = false;
            }
            b'\r' => {
                self.cursor_x = 0;
                self.wrap_next = false;
            }
            b'\t' => {
                let next = ((self.cursor_x / 8) + 1) * 8;
                self.cursor_x = next.min(self.cols - 1);
            }
            0x08 => {
                // backspace
                if self.cursor_x > 0 {
                    self.cursor_x -= 1;
                }
                self.wrap_next = false;
            }
            0x07 => {} // bell — ignore
            _ => {}
        }
        self.dirty += 1;
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        let private = intermediates.first() == Some(&b'?');
        // First numeric param with a default.
        let p1 = |def: usize| -> usize {
            params
                .iter()
                .next()
                .map(|p| p[0] as usize)
                .filter(|v| *v != 0)
                .unwrap_or(def)
        };
        let nth = |n: usize, def: usize| -> usize {
            params
                .iter()
                .nth(n)
                .map(|p| p[0] as usize)
                .filter(|v| *v != 0)
                .unwrap_or(def)
        };

        match action {
            'm' => self.sgr(params),
            'A' => self.cursor_y = self.cursor_y.saturating_sub(p1(1)),
            'B' => self.cursor_y = (self.cursor_y + p1(1)).min(self.rows - 1),
            'C' => self.cursor_x = (self.cursor_x + p1(1)).min(self.cols - 1),
            'D' => self.cursor_x = self.cursor_x.saturating_sub(p1(1)),
            'G' | '`' => self.cursor_x = (p1(1) - 1).min(self.cols - 1),
            'd' => self.cursor_y = (p1(1) - 1).min(self.rows - 1),
            'H' | 'f' => {
                let row = p1(1) - 1;
                let col = nth(1, 1) - 1;
                self.cursor_y = row.min(self.rows - 1);
                self.cursor_x = col.min(self.cols - 1);
                self.wrap_next = false;
            }
            'J' => {
                let mode = params.iter().next().map(|p| p[0]).unwrap_or(0);
                let cur = self.idx(self.cursor_x, self.cursor_y);
                let total = self.cols * self.rows;
                match mode {
                    0 => self.clear_region(cur, total), // cursor..end
                    1 => self.clear_region(0, cur + 1), // start..cursor
                    2 | 3 => {
                        self.clear_region(0, total);
                        // Some apps expect home after full clear; PowerShell's
                        // Clear-Host issues its own CUP, so leave cursor.
                    }
                    _ => {}
                }
            }
            'K' => {
                let mode = params.iter().next().map(|p| p[0]).unwrap_or(0);
                let line = self.idx(0, self.cursor_y);
                let cur = self.idx(self.cursor_x, self.cursor_y);
                match mode {
                    0 => self.clear_region(cur, line + self.cols),
                    1 => self.clear_region(line, cur + 1),
                    2 => self.clear_region(line, line + self.cols),
                    _ => {}
                }
            }
            'X' => {
                // Erase n chars from cursor.
                let n = p1(1);
                let start = self.idx(self.cursor_x, self.cursor_y);
                let end = (start + n).min(self.idx(0, self.cursor_y) + self.cols);
                self.clear_region(start, end);
            }
            'P' => {
                // Delete n chars: shift the rest of the line left.
                let n = p1(1).min(self.cols - self.cursor_x);
                let row = self.idx(0, self.cursor_y);
                let from = self.cursor_x + n;
                let blank = self.blank_cell();
                for x in self.cursor_x..self.cols {
                    self.cells[row + x] = if from + (x - self.cursor_x) < self.cols {
                        self.cells[row + from + (x - self.cursor_x)]
                    } else {
                        blank
                    };
                }
            }
            '@' => {
                // Insert n blank chars: shift right.
                let n = p1(1).min(self.cols - self.cursor_x);
                let row = self.idx(0, self.cursor_y);
                let blank = self.blank_cell();
                for x in (self.cursor_x..self.cols).rev() {
                    self.cells[row + x] = if x >= self.cursor_x + n {
                        self.cells[row + x - n]
                    } else {
                        blank
                    };
                }
            }
            'L' => {
                // Insert n lines at cursor row within scroll region.
                if self.cursor_y >= self.scroll_top && self.cursor_y <= self.scroll_bot {
                    let saved = self.scroll_top;
                    self.scroll_top = self.cursor_y;
                    self.scroll_down(p1(1));
                    self.scroll_top = saved;
                }
            }
            'M' => {
                if self.cursor_y >= self.scroll_top && self.cursor_y <= self.scroll_bot {
                    let saved = self.scroll_top;
                    self.scroll_top = self.cursor_y;
                    self.scroll_up(p1(1));
                    self.scroll_top = saved;
                }
            }
            'r' => {
                let top = p1(1) - 1;
                let bot = nth(1, self.rows) - 1;
                if top < bot && bot < self.rows {
                    self.scroll_top = top;
                    self.scroll_bot = bot;
                }
                self.cursor_x = 0;
                self.cursor_y = self.scroll_top;
            }
            's' => self.saved_cursor = (self.cursor_x, self.cursor_y),
            'u' => {
                self.cursor_x = self.saved_cursor.0.min(self.cols - 1);
                self.cursor_y = self.saved_cursor.1.min(self.rows - 1);
            }
            'h' | 'l' => {
                let set = action == 'h';
                if private {
                    for p in params.iter() {
                        if p[0] == 25 {
                            self.cursor_visible = set; // DECTCEM: show/hide cursor
                        }
                    }
                }
            }
            _ => {}
        }
        self.dirty += 1;
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
        match byte {
            b'M' => {
                // Reverse index: move up, scrolling if at top of region.
                if self.cursor_y == self.scroll_top {
                    self.scroll_down(1);
                } else if self.cursor_y > 0 {
                    self.cursor_y -= 1;
                }
            }
            b'7' => self.saved_cursor = (self.cursor_x, self.cursor_y),
            b'8' => {
                self.cursor_x = self.saved_cursor.0.min(self.cols - 1);
                self.cursor_y = self.saved_cursor.1.min(self.rows - 1);
            }
            b'c' => {
                // Full reset.
                self.pen = Pen::default();
                self.clear_region(0, self.cols * self.rows);
                self.cursor_x = 0;
                self.cursor_y = 0;
            }
            _ => {}
        }
        self.dirty += 1;
    }

    // OSC (titles etc.) and DCS are ignored for the POC.
    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {}
    fn hook(&mut self, _: &Params, _: &[u8], _: bool, _: char) {}
    fn put(&mut self, _: u8) {}
    fn unhook(&mut self) {}
}

/// The classic xterm 256-color palette, computed once.
pub fn xterm_palette() -> [[u8; 3]; 256] {
    let mut p = [[0u8; 3]; 256];
    // 16 base ANSI colors.
    const BASE: [[u8; 3]; 16] = [
        [0x00, 0x00, 0x00],
        [0xCD, 0x00, 0x00],
        [0x00, 0xCD, 0x00],
        [0xCD, 0xCD, 0x00],
        [0x00, 0x00, 0xEE],
        [0xCD, 0x00, 0xCD],
        [0x00, 0xCD, 0xCD],
        [0xE5, 0xE5, 0xE5],
        [0x7F, 0x7F, 0x7F],
        [0xFF, 0x00, 0x00],
        [0x00, 0xFF, 0x00],
        [0xFF, 0xFF, 0x00],
        [0x5C, 0x5C, 0xFF],
        [0xFF, 0x00, 0xFF],
        [0x00, 0xFF, 0xFF],
        [0xFF, 0xFF, 0xFF],
    ];
    p[..16].copy_from_slice(&BASE);
    // 6x6x6 color cube (16..=231).
    let levels = [0u8, 95, 135, 175, 215, 255];
    let mut i = 16;
    for r in 0..6 {
        for g in 0..6 {
            for b in 0..6 {
                p[i] = [levels[r], levels[g], levels[b]];
                i += 1;
            }
        }
    }
    // Grayscale ramp (232..=255).
    for j in 0..24 {
        let v = 8 + j as u8 * 10;
        p[232 + j] = [v, v, v];
    }
    p
}
