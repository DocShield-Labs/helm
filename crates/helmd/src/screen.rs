//! The terminal model: one `alacritty_terminal` VT emulator per session
//! plus the line history that scrolled off its top.
//!
//! Everything a client can see comes from here as *rows* — styled runs
//! of text, never raw bytes:
//!
//!   - the live grid, read on demand (`snapshot`) or as what changed
//!     since the last flush (`take_update`);
//!   - history: rows the primary grid scrolled out, addressed by an
//!     absolute line number that is monotonic for the session's lifetime
//!     (`history_start ..< top_line`), capped by row count;
//!   - terminal queries (DA, DSR/CPR, color, text-area size) answered by
//!     the model itself, straight to the PTY, so no client ever sees a
//!     query or answers one twice.
//!
//! alacritty keeps its own scrollback ring; rows that scroll into it
//! are exported into `history` after every feed, but stay in the ring
//! (trimmed to a small tail) so a rows-grow can pull them back onto the
//! grid the way every normal terminal restores scrollback. Without
//! that, a grow leaves the top of the screen blank, SIGWINCH makes the
//! application repaint content the model already exported, and the
//! repaint lands at NEW line numbers — a permanent duplicate (measured:
//! examples/replay.rs). Restored rows keep their original absolute
//! lines: `top_line` moves back down and history is un-exported to
//! match, so a line is never claimed by history and the grid at once.
//! History rows are immutable except for exactly those reclaimed
//! lines, which are re-exported (possibly modified) when they scroll
//! out again — clients upsert by absolute line.

use std::collections::hash_map::DefaultHasher;
use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Line;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::{
    Color as VteColor, CursorShape as VteShape, NamedColor, Processor, Rgb,
};
use parking_lot::Mutex;

use helm_proto::{attrs, modes, Color, Cursor, CursorShape, Row, Screen, Span, Style};

use crate::session::READ_BUF_LEN;

/// Rows of history retained per session. ~100 B per row of typical output,
/// so this is ~10 MB for a session that has scrolled 100k rows — a day of
/// agent output. Phase 2 moves the tail to disk.
pub const MAX_HISTORY_ROWS: usize = 100_000;

/// alacritty's own scrollback ring cap. The ring is trimmed to
/// `RING_KEEP` after every export, so between drains it only has to
/// hold that tail plus what one feed can scroll out: at most one line
/// per byte of a PTY read. If it ever saturated, alacritty would evict
/// rows silently and our absolute line numbers would drift, so the
/// margin is generous and `drain_history` asserts it.
const MODEL_SCROLLBACK: usize = 4 * READ_BUF_LEN;

/// Rows left in alacritty's ring after an export — the tail a rows-grow
/// can restore. Larger than any real screen height; every row in it is
/// already exported, so trimming loses nothing.
const RING_KEEP: usize = 512;

/// The PTY writer, shared between the session (client input) and the
/// model (query answers).
pub type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// What changed since the last `take_update`.
#[derive(Debug)]
pub enum Update {
    None,
    /// Everything: paint the whole grid.
    Full(Screen),
    /// The grid scrolled up by `scroll` rows (those rows went to
    /// history), then only `rows` differ from what was last sent.
    Partial {
        top_line: u64,
        scroll: u16,
        rows: Vec<(u16, Row)>,
        cursor: Cursor,
        modes: u32,
    },
}

fn pack(cols: u16, rows: u16) -> u32 {
    ((cols as u32) << 16) | rows as u32
}

/// alacritty's event sink. Only the events that are answers to the
/// application matter here; titles, clipboard and bells are either
/// handled upstream (the marker parser strips bells and OSC 9) or not
/// yet surfaced (phase 2: OSC 52 → app clipboard, title → session name).
struct Listener {
    writer: SharedWriter,
    /// `pack(cols, rows)` — the model's size, for text-area queries.
    size: Arc<AtomicU32>,
}

impl Listener {
    fn reply(&self, text: &str) {
        let mut w = self.writer.lock();
        let _ = w.write_all(text.as_bytes());
        let _ = w.flush();
    }
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(text) => self.reply(&text),
            Event::ColorRequest(index, format) => self.reply(&format(default_color(index))),
            Event::TextAreaSizeRequest(format) => {
                let packed = self.size.load(Ordering::Relaxed);
                let ws = WindowSize {
                    num_cols: (packed >> 16) as u16,
                    num_lines: (packed & 0xffff) as u16,
                    cell_width: 8,
                    cell_height: 16,
                };
                self.reply(&format(ws));
            }
            _ => {}
        }
    }
}

/// Palette reported to OSC 4/10/11/12 queries. Mirrors the frontend's
/// default theme closely enough that theme-detecting programs pick
/// "dark"; per-host theme plumbing is a later concern.
fn default_color(index: usize) -> Rgb {
    const ANSI16: [(u8, u8, u8); 16] = [
        (0x3a, 0x3a, 0x3e),
        (0xe0, 0x56, 0x4a),
        (0x3d, 0xba, 0x7e),
        (0xe8, 0xb0, 0x4b),
        (0x4b, 0x8b, 0xf5),
        (0xc6, 0x78, 0xdd),
        (0x56, 0xb6, 0xc2),
        (0xd7, 0xd6, 0xd2),
        (0x6b, 0x6b, 0x70),
        (0xff, 0x7b, 0x70),
        (0x5e, 0xd3, 0x9a),
        (0xf5, 0xc5, 0x6b),
        (0x7a, 0xa8, 0xff),
        (0xd9, 0x8f, 0xe8),
        (0x7b, 0xd0, 0xda),
        (0xf5, 0xf4, 0xf0),
    ];
    let (r, g, b) = match index {
        0..=15 => ANSI16[index],
        16..=231 => {
            let i = index - 16;
            let v = |c: usize| if c == 0 { 0 } else { (55 + c * 40) as u8 };
            (v(i / 36), v((i % 36) / 6), v(i % 6))
        }
        232..=255 => {
            let g = (8 + (index - 232) * 10) as u8;
            (g, g, g)
        }
        i if i == NamedColor::Background as usize => (0x0b, 0x0c, 0x0e),
        // Foreground, cursor and friends.
        _ => (0xd7, 0xd6, 0xd2),
    };
    Rgb { r, g, b }
}

pub struct SessionScreen {
    term: Term<Listener>,
    parser: Processor,
    size: Arc<AtomicU32>,
    history: VecDeque<Row>,
    /// Absolute line of `history[0]`.
    history_start: u64,
    /// Absolute line of grid row 0 (== rows ever scrolled out).
    top_line: u64,
    /// Rows in `[pending_first, top_line)` scrolled out since clients
    /// were last told.
    pending_first: u64,
    /// Ring rows already exported to `history` — the drain baseline.
    ring_rows: usize,
    /// A resize happened while the alt screen hid the primary grid; the
    /// ring change is reconciled at the next non-alt drain.
    alt_resized: bool,
    max_history: usize,
    /// What clients were last told the grid looks like: a hash per row
    /// (alacritty re-damages the cursor's row on every read, and marks
    /// every scroll as full damage, so rows are diffed before they go
    /// out), plus the shape and cursor/modes of that frame.
    sent_hashes: Vec<u64>,
    sent_cols: usize,
    sent_top_line: u64,
    sent_cursor: Option<Cursor>,
    sent_modes: u32,
    empty_hash: u64,
}

impl SessionScreen {
    pub fn new(cols: u16, rows: u16, writer: SharedWriter) -> Self {
        Self::with_limits(cols, rows, writer, MAX_HISTORY_ROWS)
    }

    pub fn with_limits(cols: u16, rows: u16, writer: SharedWriter, max_history: usize) -> Self {
        let cols = cols.max(2);
        let rows = rows.max(1);
        let size = Arc::new(AtomicU32::new(pack(cols, rows)));
        let config = Config {
            scrolling_history: MODEL_SCROLLBACK,
            ..Config::default()
        };
        let listener = Listener {
            writer,
            size: size.clone(),
        };
        let mut term = Term::new(
            config,
            &TermSize::new(cols as usize, rows as usize),
            listener,
        );
        // Unfocused alacritty renders a hollow cursor; we have one focus.
        term.is_focused = true;
        Self {
            term,
            parser: Processor::new(),
            size,
            history: VecDeque::new(),
            history_start: 0,
            top_line: 0,
            pending_first: 0,
            ring_rows: 0,
            alt_resized: false,
            max_history,
            sent_hashes: Vec::new(),
            sent_cols: 0,
            sent_top_line: 0,
            sent_cursor: None,
            sent_modes: 0,
            empty_hash: row_hash(&Row::default()),
        }
    }

    /// Everything retained, for an upgrade snapshot: the history rows
    /// followed by the grid's used rows (the visible tail joins the
    /// transcript), with the absolute line of the first row returned.
    pub fn snapshot_rows(&self) -> (u64, Vec<Row>) {
        let mut rows: Vec<Row> = self.history.iter().cloned().collect();
        let mut last_used = 0usize;
        for l in 0..self.term.screen_lines() {
            let row = self.grid_row(l as i32);
            rows.push(row);
            if !rows.last().expect("just pushed").spans.is_empty() {
                last_used = rows.len();
            }
        }
        rows.truncate(last_used.max(self.history.len()));
        (self.history_start, rows)
    }

    /// Seed a fresh screen with resurrected history: the rows a previous
    /// daemon snapshotted before an upgrade. Absolute numbering continues
    /// (`top_line` starts past the seeded rows), so the snapshotted block
    /// table stays valid. Call before the PTY produces meaningful output.
    pub fn seed_history(&mut self, history_start: u64, rows: Vec<helm_proto::Row>) {
        self.history_start = history_start;
        self.top_line = history_start + rows.len() as u64;
        self.pending_first = self.top_line;
        self.sent_top_line = self.top_line;
        self.history = rows.into();
    }

    pub fn top_line(&self) -> u64 {
        self.top_line
    }

    pub fn history_start(&self) -> u64 {
        self.history_start
    }

    pub fn alt_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    /// Feed PTY bytes (already stripped of OSC 133 / bells by the marker
    /// parser). Rows that scroll out land in history.
    pub fn advance(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.parser.advance(&mut self.term, bytes);
        self.drain_history();
    }

    /// Absolute line under the cursor — where a marker seen now lands.
    pub fn cursor_abs_line(&self) -> u64 {
        self.top_line + self.term.grid().cursor.point.line.0.max(0) as u64
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        let cols = cols.max(2);
        let rows = rows.max(1);
        self.term
            .resize(TermSize::new(cols as usize, rows as usize));
        self.size.store(pack(cols, rows), Ordering::Relaxed);
        if self.alt_screen() {
            // `grid()` is the alt grid here; the primary's ring moved
            // invisibly. Reconciled at the next non-alt drain.
            self.alt_resized = true;
            return;
        }
        // Shrinking pushes top rows into the ring (export); growing
        // pulls rows back out of it onto the grid (restore).
        self.reconcile_ring(true);
    }

    /// Reconcile the ring after a feed: export what scrolled out.
    fn drain_history(&mut self) {
        // The alt grid has no scrollback and the primary is parked
        // while a TUI holds the screen: nothing can have scrolled out.
        // (A resize during alt is flagged and reconciled on exit.)
        if self.alt_screen() {
            return;
        }
        let restore = std::mem::take(&mut self.alt_resized);
        self.reconcile_ring(restore);
    }

    /// Bring `history`/`top_line` in line with alacritty's ring.
    ///
    /// The ring grew: rows scrolled out (a feed, or a shrink) — export
    /// the new tail, leaving it in the ring for a later grow.
    ///
    /// The ring shrank and `may_restore` (a resize did it): a rows-grow
    /// pulled rows back onto the grid, LIFO, exactly the newest exported
    /// rows — un-export them so their lines are the grid's again.
    ///
    /// The ring shrank in a feed (`may_restore` false): the application
    /// cleared its scrollback (ED3/RIS), which empties the ring without
    /// moving grid row 0. Everything wiped was already exported; rows
    /// scrolled after the clear in the same feed are the whole ring now,
    /// so the baseline resets to zero and they export normally. (Rows
    /// that scrolled *before* the clear in the same feed are lost and
    /// `top_line` does not advance for them — the same blind spot the
    /// clear-every-drain model had.)
    fn reconcile_ring(&mut self, may_restore: bool) {
        let ring = self.term.grid().history_size();
        if ring < self.ring_rows {
            if may_restore {
                let restored = (self.ring_rows - ring).min(self.history.len());
                self.top_line -= restored as u64;
                let keep = self.history.len() - restored;
                self.history.truncate(keep);
                self.pending_first = self.pending_first.min(self.top_line);
                self.ring_rows = ring;
                return;
            }
            self.ring_rows = 0;
        }
        let scrolled = ring - self.ring_rows;
        if scrolled > 0 {
            debug_assert!(
                ring < MODEL_SCROLLBACK,
                "alacritty scrollback saturated: lines would drift"
            );
            for i in (1..=scrolled).rev() {
                self.history.push_back(self.grid_row(-(i as i32)));
            }
            self.top_line += scrolled as u64;
            while self.history.len() > self.max_history {
                self.history.pop_front();
                self.history_start += 1;
            }
        }
        self.ring_rows = ring;
        self.trim_ring();
    }

    /// Trim the ring to `RING_KEEP`: everything in it is exported, so
    /// only the tail a grow could want back needs to stay. The grid API
    /// has no plain truncate — `update_history` shrinks stored rows AND
    /// lowers the ring's cap, so the cap is restored right after; the
    /// cap is what keeps one huge feed from silently evicting rows.
    fn trim_ring(&mut self) {
        if self.ring_rows > RING_KEEP {
            self.term.grid_mut().update_history(RING_KEEP);
            self.term.grid_mut().update_history(MODEL_SCROLLBACK);
            self.ring_rows = RING_KEEP;
        }
    }

    fn grid_row(&self, line: i32) -> Row {
        encode_row(&self.term.grid()[Line(line)], self.term.columns())
    }

    /// Rows scrolled out since the last call, with the absolute line of
    /// the first. Empty when nothing scrolled.
    pub fn take_pending_history(&mut self) -> (u64, Vec<Row>) {
        let first = self.pending_first.max(self.history_start);
        self.pending_first = self.top_line;
        if first >= self.top_line {
            return (self.top_line, Vec::new());
        }
        let a = (first - self.history_start) as usize;
        let b = (self.top_line - self.history_start) as usize;
        (first, self.history.range(a..b).cloned().collect())
    }

    /// Nobody is listening: forget what would have been sent.
    pub fn discard_pending(&mut self) {
        self.pending_first = self.top_line;
    }

    /// Damage since the last call, as the smallest thing a client can
    /// apply: a scroll plus the rows that differ from the last frame, or
    /// a full screen when the shape changed (resize, alt-screen swap) or
    /// the grid scrolled clean through. Also ends an expired synchronized
    /// update (`CSI ? 2026 h` with no `l`) so a stalled application
    /// can't freeze the frame forever.
    pub fn take_update(&mut self) -> Update {
        if let Some(deadline) = self.parser.sync_timeout().sync_timeout() {
            if Instant::now() >= deadline {
                self.parser.stop_sync(&mut self.term);
                self.drain_history();
            }
        }
        let damaged: Option<Vec<usize>> = match self.term.damage() {
            TermDamage::Full => None,
            TermDamage::Partial(lines) => Some(lines.map(|d| d.line).collect()),
        };
        self.term.reset_damage();
        let cursor = self.cursor();
        let modes = self.modes();
        let cols = self.term.columns();
        let nrows = self.term.screen_lines();

        // A grow-restore moves `top_line` BACK (rows return to the grid);
        // that is never a scroll, always a full repaint — expressed here
        // as the absence of a forward scroll.
        let forward = self.top_line.checked_sub(self.sent_top_line);
        let same_shape = self.sent_hashes.len() == nrows && self.sent_cols == cols;
        let alt_flip = (self.sent_modes ^ modes) & modes::ALT_SCREEN != 0;
        if !same_shape || alt_flip || forward.is_none_or(|s| s >= nrows as u64) {
            let screen = self.snapshot();
            self.sent_hashes = screen.lines.iter().map(row_hash).collect();
            self.sent_cols = cols;
            self.sent_top_line = self.top_line;
            self.sent_cursor = Some(cursor);
            self.sent_modes = modes;
            return Update::Full(screen);
        }
        let scroll = forward.unwrap_or(0) as usize;
        if scroll > 0 {
            self.sent_hashes.drain(..scroll);
            self.sent_hashes.resize(nrows, self.empty_hash);
        }
        // A scroll (or anything alacritty calls full damage) means any
        // row may differ; otherwise only the damaged ones can.
        let candidates: Vec<usize> = match damaged {
            Some(lines) if scroll == 0 => lines.into_iter().filter(|&l| l < nrows).collect(),
            _ => (0..nrows).collect(),
        };
        let mut rows = Vec::new();
        for l in candidates {
            let row = self.grid_row(l as i32);
            let h = row_hash(&row);
            if self.sent_hashes[l] != h {
                self.sent_hashes[l] = h;
                rows.push((l as u16, row));
            }
        }
        self.sent_top_line = self.top_line;
        let unchanged = rows.is_empty()
            && scroll == 0
            && self.sent_cursor == Some(cursor)
            && self.sent_modes == modes;
        if unchanged {
            return Update::None;
        }
        self.sent_cursor = Some(cursor);
        self.sent_modes = modes;
        Update::Partial {
            top_line: self.top_line,
            scroll: scroll as u16,
            rows,
            cursor,
            modes,
        }
    }

    /// The whole grid.
    pub fn snapshot(&self) -> Screen {
        let rows = self.term.screen_lines();
        Screen {
            cols: self.term.columns() as u16,
            rows: rows as u16,
            top_line: self.top_line,
            history_start: self.history_start,
            lines: (0..rows).map(|l| self.grid_row(l as i32)).collect(),
            cursor: self.cursor(),
            modes: self.modes(),
        }
    }

    pub fn cursor(&self) -> Cursor {
        let point = self.term.grid().cursor.point;
        let style = self.term.cursor_style();
        let (shape, hidden) = match style.shape {
            VteShape::Block | VteShape::HollowBlock => (CursorShape::Block, false),
            VteShape::Underline => (CursorShape::Underline, false),
            VteShape::Beam => (CursorShape::Beam, false),
            VteShape::Hidden => (CursorShape::Block, true),
        };
        Cursor {
            row: point.line.0.max(0) as u16,
            col: point.column.0 as u16,
            visible: !hidden && self.term.mode().contains(TermMode::SHOW_CURSOR),
            shape,
            blink: style.blinking,
        }
    }

    pub fn modes(&self) -> u32 {
        let m = self.term.mode();
        let mut out = 0;
        let map = [
            (TermMode::APP_CURSOR, modes::APP_CURSOR),
            (TermMode::APP_KEYPAD, modes::APP_KEYPAD),
            (TermMode::BRACKETED_PASTE, modes::BRACKETED_PASTE),
            (TermMode::FOCUS_IN_OUT, modes::FOCUS_IN_OUT),
            (TermMode::MOUSE_REPORT_CLICK, modes::MOUSE_CLICK),
            (TermMode::MOUSE_DRAG, modes::MOUSE_DRAG),
            (TermMode::MOUSE_MOTION, modes::MOUSE_MOTION),
            (TermMode::SGR_MOUSE, modes::SGR_MOUSE),
            (TermMode::UTF8_MOUSE, modes::UTF8_MOUSE),
            (TermMode::ALT_SCREEN, modes::ALT_SCREEN),
            (TermMode::ALTERNATE_SCROLL, modes::ALTERNATE_SCROLL),
        ];
        for (term_bit, ours) in map {
            if m.contains(term_bit) {
                out |= ours;
            }
        }
        out
    }

    /// History rows in `[from, to)`, clamped to what's retained and to
    /// `MAX_HISTORY_PAGE` rows counted back from `to` (a client paging
    /// upwards wants the newest rows first). Returns the absolute line
    /// of the first row returned.
    pub fn history_page(&self, from: u64, to: u64) -> (u64, Vec<Row>) {
        let hi = to.min(self.top_line);
        let lo = from
            .max(self.history_start)
            .max(hi.saturating_sub(helm_proto::MAX_HISTORY_PAGE));
        if hi <= lo {
            return (hi, Vec::new());
        }
        let a = (lo - self.history_start) as usize;
        let b = (hi - self.history_start) as usize;
        (lo, self.history.range(a..b).cloned().collect())
    }

    /// Visit every retained row with its absolute line, history first
    /// then the grid — the search corpus. Borrows history rows; only
    /// grid rows are encoded on the fly.
    pub fn for_each_row(&self, mut f: impl FnMut(u64, &Row)) {
        for (i, r) in self.history.iter().enumerate() {
            f(self.history_start + i as u64, r);
        }
        for l in 0..self.term.screen_lines() {
            f(self.top_line + l as u64, &self.grid_row(l as i32));
        }
    }

    /// Text of the last non-blank grid row — notification previews.
    pub fn last_nonempty_text(&self) -> String {
        for l in (0..self.term.screen_lines()).rev() {
            let text = self.grid_row(l as i32).text();
            if !text.trim().is_empty() {
                return text.trim().chars().take(120).collect();
            }
        }
        String::new()
    }
}

// -------------------------------------------------------------------
// Cell → Row encoding
// -------------------------------------------------------------------

fn row_hash(r: &Row) -> u64 {
    let mut h = DefaultHasher::new();
    r.hash(&mut h);
    h.finish()
}

/// Cell flags that affect a run's style (as opposed to layout flags
/// like WRAPLINE / WIDE_CHAR).
const STYLE_FLAGS: Flags = Flags::BOLD
    .union(Flags::DIM)
    .union(Flags::ITALIC)
    .union(Flags::ALL_UNDERLINES)
    .union(Flags::INVERSE)
    .union(Flags::STRIKEOUT)
    .union(Flags::HIDDEN);

fn is_blank(cell: &Cell) -> bool {
    cell.c == ' '
        && cell.bg == VteColor::Named(NamedColor::Background)
        && !cell
            .flags
            .intersects(Flags::INVERSE | Flags::ALL_UNDERLINES | Flags::STRIKEOUT)
        && cell.zerowidth().map(|z| z.is_empty()).unwrap_or(true)
        && cell.hyperlink().is_none()
}

/// Do two cells belong to the same styled run? Compared on the raw cell
/// so a `Style` is only built once per run.
fn same_run(a: &Cell, b: &Cell) -> bool {
    a.fg == b.fg
        && a.bg == b.bg
        && (a.flags & STYLE_FLAGS) == (b.flags & STYLE_FLAGS)
        && a.hyperlink() == b.hyperlink()
}

/// (colour, "render dim") for a cell colour. alacritty's `Dim*` named
/// colours are a base colour plus dimming.
fn color(c: VteColor) -> (Color, bool) {
    match c {
        VteColor::Named(n) => match n {
            NamedColor::Foreground
            | NamedColor::Background
            | NamedColor::Cursor
            | NamedColor::BrightForeground => (Color::Default, false),
            NamedColor::DimForeground => (Color::Default, true),
            NamedColor::DimBlack
            | NamedColor::DimRed
            | NamedColor::DimGreen
            | NamedColor::DimYellow
            | NamedColor::DimBlue
            | NamedColor::DimMagenta
            | NamedColor::DimCyan
            | NamedColor::DimWhite => (Color::Indexed(n.to_bright() as u8), true),
            // Black..=White, BrightBlack..=BrightWhite: the 16 ANSI slots.
            _ => (Color::Indexed(n as u8), false),
        },
        VteColor::Spec(rgb) => (Color::Rgb(rgb.r, rgb.g, rgb.b), false),
        VteColor::Indexed(i) => (Color::Indexed(i), false),
    }
}

fn style_of(cell: &Cell) -> Style {
    let (fg, dim_fg) = color(cell.fg);
    let (bg, _) = color(cell.bg);
    let f = cell.flags;
    let mut a = 0u16;
    if f.contains(Flags::BOLD) {
        a |= attrs::BOLD;
    }
    if f.contains(Flags::DIM) || dim_fg {
        a |= attrs::DIM;
    }
    if f.contains(Flags::ITALIC) {
        a |= attrs::ITALIC;
    }
    if f.intersects(Flags::ALL_UNDERLINES) {
        a |= attrs::UNDERLINE;
    }
    if f.contains(Flags::DOUBLE_UNDERLINE) {
        a |= attrs::DOUBLE_UNDERLINE;
    }
    if f.contains(Flags::UNDERCURL) {
        a |= attrs::UNDERCURL;
    }
    if f.contains(Flags::INVERSE) {
        a |= attrs::INVERSE;
    }
    if f.contains(Flags::STRIKEOUT) {
        a |= attrs::STRIKE;
    }
    if f.contains(Flags::HIDDEN) {
        a |= attrs::HIDDEN;
    }
    Style {
        fg,
        bg,
        attrs: a,
        link: cell.hyperlink().map(|h| h.uri().to_string()),
    }
}

/// Encode one grid row: trailing blanks trimmed, wide-char spacers
/// dropped, zero-width marks kept with their base cell, runs of equal
/// style merged into spans.
pub fn encode_row(row: &alacritty_terminal::grid::Row<Cell>, cols: usize) -> Row {
    let cells: &[Cell] = &row[..];
    let cells = &cells[..cells.len().min(cols)];
    let wrapped = cells
        .last()
        .map(|c| c.flags.contains(Flags::WRAPLINE))
        .unwrap_or(false);
    let end = cells
        .iter()
        .rposition(|c| !is_blank(c))
        .map(|i| i + 1)
        .unwrap_or(0);
    let mut spans: Vec<Span> = Vec::new();
    let mut run_start: Option<&Cell> = None;
    for cell in &cells[..end] {
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }
        if !run_start.is_some_and(|prev| same_run(prev, cell)) {
            spans.push(Span {
                text: String::new(),
                style: style_of(cell),
            });
            run_start = Some(cell);
        }
        let text = &mut spans.last_mut().expect("a run was just opened").text;
        text.push(cell.c);
        if let Some(zw) = cell.zerowidth() {
            text.extend(zw.iter());
        }
    }
    Row { spans, wrapped }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A writer that records query answers.
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn screen(cols: u16, rows: u16) -> (SessionScreen, Sink) {
        let sink = Sink::default();
        let writer: SharedWriter = Arc::new(Mutex::new(Box::new(sink.clone())));
        (SessionScreen::new(cols, rows, writer), sink)
    }

    fn texts(rows: &[Row]) -> Vec<String> {
        rows.iter().map(Row::text).collect()
    }

    #[test]
    fn scrolls_into_history_with_absolute_lines() {
        let (mut s, _) = screen(20, 4);
        for i in 0..10 {
            s.advance(format!("l{i}\r\n").as_bytes());
        }
        // 10 lines + the cursor's row = 11 rows; 4 fit in the grid.
        assert_eq!(s.top_line(), 7);
        assert_eq!(s.history_start(), 0);
        let (from, page) = s.history_page(0, u64::MAX);
        assert_eq!(from, 0);
        assert_eq!(
            texts(&page),
            (0..7).map(|i| format!("l{i}")).collect::<Vec<_>>()
        );
        let snap = s.snapshot();
        assert_eq!(texts(&snap.lines), vec!["l7", "l8", "l9", ""]);
        assert_eq!((snap.top_line, snap.history_start), (7, 0));
        assert_eq!(s.cursor_abs_line(), 10);
        assert_eq!((snap.cursor.row, snap.cursor.col), (3, 0));
        // Pending history is handed out once.
        let (first, pending) = s.take_pending_history();
        assert_eq!(first, 0);
        assert_eq!(pending.len(), 7);
        assert!(s.take_pending_history().1.is_empty());
        s.advance(b"l10\r\n");
        let (first, pending) = s.take_pending_history();
        assert_eq!((first, texts(&pending)), (7, vec!["l7".to_string()]));
    }

    #[test]
    fn wrap_flag_and_wide_chars() {
        let (mut s, _) = screen(4, 3);
        s.advance(b"abcdef");
        let snap = s.snapshot();
        assert_eq!(snap.lines[0].text(), "abcd");
        assert!(snap.lines[0].wrapped);
        assert_eq!(snap.lines[1].text(), "ef");
        assert!(!snap.lines[1].wrapped);

        let (mut s, _) = screen(4, 2);
        s.advance("漢x".as_bytes());
        let snap = s.snapshot();
        // The wide char occupies two cells but appears once.
        assert_eq!(snap.lines[0].text(), "漢x");
        assert_eq!(snap.cursor.col, 3);
    }

    #[test]
    fn styles_merge_into_spans() {
        let (mut s, _) = screen(20, 2);
        s.advance(b"\x1b[1;31mab\x1b[0mc \x1b[38;2;1;2;3;4md\x1b[0;2;34me");
        let row = &s.snapshot().lines[0];
        assert_eq!(row.spans.len(), 4);
        assert_eq!(row.spans[0].text, "ab");
        assert_eq!(row.spans[0].style.fg, Color::Indexed(1));
        assert_eq!(row.spans[0].style.attrs, attrs::BOLD);
        assert_eq!(row.spans[1].text, "c ");
        assert_eq!(row.spans[1].style, Style::default());
        assert_eq!(row.spans[2].text, "d");
        assert_eq!(row.spans[2].style.fg, Color::Rgb(1, 2, 3));
        assert_eq!(row.spans[2].style.attrs, attrs::UNDERLINE);
        // Dim blue: the base colour plus the DIM attribute.
        assert_eq!(row.spans[3].style.fg, Color::Indexed(4));
        assert_eq!(row.spans[3].style.attrs, attrs::DIM);
    }

    #[test]
    fn trailing_blanks_trimmed_but_colored_kept() {
        let (mut s, _) = screen(10, 2);
        s.advance(b"a   ");
        assert_eq!(s.snapshot().lines[0].spans.len(), 1);
        assert_eq!(s.snapshot().lines[0].text(), "a");
        let (mut s, _) = screen(10, 2);
        s.advance(b"\x1b[44m  \x1b[0m");
        let row = &s.snapshot().lines[0];
        assert_eq!(row.text(), "  ");
        assert_eq!(row.spans[0].style.bg, Color::Indexed(4));
    }

    #[test]
    fn hyperlinks_carry_the_uri() {
        let (mut s, _) = screen(20, 2);
        s.advance(b"\x1b]8;;https://x.dev\x1b\\link\x1b]8;;\x1b\\ plain");
        let row = &s.snapshot().lines[0];
        assert_eq!(row.spans[0].text, "link");
        assert_eq!(row.spans[0].style.link.as_deref(), Some("https://x.dev"));
        assert_eq!(row.spans[1].text, " plain");
        assert_eq!(row.spans[1].style.link, None);
    }

    #[test]
    fn damage_partial_then_scroll_then_full() {
        let (mut s, _) = screen(10, 3);
        assert!(matches!(s.take_update(), Update::Full(_)));
        assert!(matches!(s.take_update(), Update::None));
        s.advance(b"x");
        match s.take_update() {
            Update::Partial {
                scroll: 0,
                rows,
                cursor,
                ..
            } => {
                assert_eq!(rows.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![0]);
                assert_eq!(rows[0].1.text(), "x");
                assert_eq!((cursor.row, cursor.col), (0, 1));
            }
            other => panic!("expected partial, got {other:?}"),
        }
        // Two rows scroll out: a shift plus only the rows that differ
        // from the shifted frame — not a full repaint.
        s.advance(b"\r\n\r\n\r\n\r\ny");
        match s.take_update() {
            Update::Partial {
                scroll: 2,
                top_line: 2,
                rows,
                ..
            } => {
                assert_eq!(rows.len(), 1);
                assert_eq!((rows[0].0, rows[0].1.text()), (2, "y".into()));
            }
            other => panic!("expected a scrolled partial, got {other:?}"),
        }
        // Scrolling clean through the grid is a full frame again.
        s.advance(b"\r\n\r\n\r\n\r\n");
        assert!(matches!(s.take_update(), Update::Full(_)));
    }

    #[test]
    fn resize_shrink_pushes_rows_to_history() {
        let (mut s, _) = screen(10, 4);
        s.advance(b"a\r\nb\r\nc\r\nd");
        assert_eq!(s.top_line(), 0);
        s.resize(10, 2);
        assert_eq!(s.top_line(), 2);
        assert_eq!(texts(&s.history_page(0, u64::MAX).1), vec!["a", "b"]);
        assert_eq!(texts(&s.snapshot().lines), vec!["c", "d"]);
        assert!(matches!(s.take_update(), Update::Full(_)));
    }

    /// The invariant nothing may break: a line is owned by history XOR
    /// the grid, and the bookkeeping all agrees.
    fn assert_consistent(s: &SessionScreen) {
        assert_eq!(
            s.top_line(),
            s.history_start() + s.history.len() as u64,
            "history span must end exactly at top_line"
        );
    }

    #[test]
    fn grow_restores_rows_at_their_original_lines() {
        let (mut s, _) = screen(20, 4);
        for i in 0..10 {
            s.advance(format!("l{i}\r\n").as_bytes());
        }
        assert_eq!(s.top_line(), 7);
        let _ = s.take_update();
        // Grow by 4 rows: the 4 newest history rows come back to the
        // grid at their original absolute lines — top_line moves BACK.
        s.resize(20, 8);
        assert_eq!(s.top_line(), 3);
        assert_consistent(&s);
        assert_eq!(texts(&s.history_page(0, u64::MAX).1), vec!["l0", "l1", "l2"]);
        assert_eq!(
            texts(&s.snapshot().lines),
            vec!["l3", "l4", "l5", "l6", "l7", "l8", "l9", ""]
        );
        // No line is claimed twice, and the client gets a full repaint.
        assert!(matches!(s.take_update(), Update::Full(_)));
    }

    #[test]
    fn rescroll_after_restore_reexports_by_line() {
        let (mut s, _) = screen(20, 4);
        for i in 0..10 {
            s.advance(format!("l{i}\r\n").as_bytes());
        }
        let (first, rows) = s.take_pending_history();
        assert_eq!((first, rows.len()), (0, 7)); // clients told l0..l6
        s.resize(20, 8); // restore l3..l6
        assert_eq!(s.top_line(), 3);
        // Scroll again: the restored lines leave the grid a second time
        // and are re-exported from line 3 — an upsert for the client.
        for i in 10..16 {
            s.advance(format!("l{i}\r\n").as_bytes());
        }
        assert_consistent(&s);
        let (first, rows) = s.take_pending_history();
        assert_eq!(first, 3);
        assert_eq!(texts(&rows)[..4], ["l3", "l4", "l5", "l6"]);
        // And history itself holds each line exactly once.
        let (from, page) = s.history_page(0, u64::MAX);
        assert_eq!(from, 0);
        let expect: Vec<String> = (0..(s.top_line() as usize)).map(|i| format!("l{i}")).collect();
        assert_eq!(texts(&page), expect);
    }

    #[test]
    fn shrink_then_grow_round_trips_without_duplication() {
        let (mut s, _) = screen(20, 6);
        for i in 0..8 {
            s.advance(format!("l{i}\r\n").as_bytes());
        }
        let before: Vec<String> = {
            let (_, page) = s.history_page(0, u64::MAX);
            let mut v = texts(&page);
            v.extend(texts(&s.snapshot().lines));
            v
        };
        s.resize(20, 3); // push
        assert_consistent(&s);
        s.resize(20, 6); // pull the same rows back
        assert_consistent(&s);
        let after: Vec<String> = {
            let (_, page) = s.history_page(0, u64::MAX);
            let mut v = texts(&page);
            v.extend(texts(&s.snapshot().lines));
            v
        };
        assert_eq!(before, after, "a resize round trip must not duplicate or lose rows");
    }

    #[test]
    fn ed3_clears_ring_without_moving_lines() {
        let (mut s, _) = screen(20, 4);
        for i in 0..10 {
            s.advance(format!("l{i}\r\n").as_bytes());
        }
        assert_eq!(s.top_line(), 7);
        // The application wipes its scrollback: grid row 0 doesn't move,
        // exported history is ours and survives.
        s.advance(b"\x1b[3J");
        assert_eq!(s.top_line(), 7);
        assert_consistent(&s);
        assert_eq!(s.history_page(0, u64::MAX).1.len(), 7);
        // A grow now has nothing to restore (the ring is empty) — the
        // grid grows blank, and lines still don't drift.
        s.resize(20, 6);
        assert_eq!(s.top_line(), 7);
        assert_consistent(&s);
        // Scrolling afterwards exports from the right line.
        for i in 10..18 {
            s.advance(format!("l{i}\r\n").as_bytes());
        }
        assert_consistent(&s);
        let (_, page) = s.history_page(7, u64::MAX);
        assert_eq!(texts(&page)[0], "l7");
    }

    #[test]
    fn resize_during_alt_reconciles_on_exit() {
        let (mut s, _) = screen(20, 4);
        for i in 0..10 {
            s.advance(format!("l{i}\r\n").as_bytes());
        }
        assert_eq!(s.top_line(), 7);
        s.advance(b"\x1b[?1049h"); // enter alt
        s.resize(20, 8); // grow while the TUI owns the screen
        s.advance(b"\x1b[?1049l"); // exit alt
        s.advance(b"");
        // drain on the exit feed reconciles: rows restored to the grid.
        s.advance(b"x");
        assert_eq!(s.top_line(), 3);
        assert_consistent(&s);
        let (_, page) = s.history_page(0, u64::MAX);
        assert_eq!(texts(&page), vec!["l0", "l1", "l2"]);
    }

    #[test]
    fn ring_trim_never_moves_lines() {
        let (mut s, _) = screen(20, 4);
        // Scroll far past RING_KEEP so the ring gets trimmed repeatedly.
        for i in 0..(RING_KEEP + 300) {
            s.advance(format!("l{i}\r\n").as_bytes());
        }
        let expected_top = (RING_KEEP + 300 + 1 - 4) as u64;
        assert_eq!(s.top_line(), expected_top);
        assert_consistent(&s);
        // A grow can still restore up to the kept tail, at true lines.
        s.resize(20, 8);
        assert_eq!(s.top_line(), expected_top - 4);
        assert_consistent(&s);
        // Line N holds "lN" by construction; the newest history row is
        // the line just below the restored grid top.
        let (_, page) = s.history_page(s.top_line() - 1, u64::MAX);
        assert_eq!(texts(&page), vec![format!("l{}", s.top_line() - 1)]);
    }

    #[test]
    fn answers_queries_on_the_pty() {
        let (mut s, sink) = screen(10, 4);
        s.advance(b"\x1b[c");
        assert_eq!(sink.0.lock().as_slice(), b"\x1b[?6c");
        sink.0.lock().clear();
        s.advance(b"ab\x1b[6n");
        assert_eq!(sink.0.lock().as_slice(), b"\x1b[1;3R");
        sink.0.lock().clear();
        s.advance(b"\x1b]11;?\x1b\\");
        let reply = String::from_utf8(sink.0.lock().clone()).unwrap();
        assert!(reply.starts_with("\x1b]11;rgb:"), "{reply:?}");
    }

    #[test]
    fn history_cap_evicts_oldest() {
        let sink = Sink::default();
        let writer: SharedWriter = Arc::new(Mutex::new(Box::new(sink)));
        let mut s = SessionScreen::with_limits(10, 2, writer, 3);
        for i in 0..8 {
            s.advance(format!("l{i}\r\n").as_bytes());
        }
        // 8 lines + cursor row, 2 in the grid → 7 scrolled out, 3 kept.
        assert_eq!(s.top_line(), 7);
        assert_eq!(s.history_start(), 4);
        let (from, page) = s.history_page(0, u64::MAX);
        assert_eq!(from, 4);
        assert_eq!(texts(&page), vec!["l4", "l5", "l6"]);
        // Pending is whatever scrolled and is still retained.
        let (first, pending) = s.take_pending_history();
        assert_eq!((first, pending.len()), (4, 3));
    }

    #[test]
    fn history_page_clamps_and_pages_from_the_end() {
        let (mut s, _) = screen(10, 1);
        for i in 0..10 {
            s.advance(format!("l{i}\r\n").as_bytes());
        }
        let (from, page) = s.history_page(3, 6);
        assert_eq!(from, 3);
        assert_eq!(texts(&page), vec!["l3", "l4", "l5"]);
        // Past the end → empty at the clamp.
        let (from, page) = s.history_page(50, 60);
        assert_eq!((from, page.len()), (10, 0));
    }

    #[test]
    fn alt_screen_does_not_touch_history() {
        let (mut s, _) = screen(10, 2);
        s.advance(b"one\r\ntwo\r\n");
        assert_eq!(s.top_line(), 1);
        assert!(matches!(s.take_update(), Update::Full(_)));
        s.advance(b"\x1b[?1049h");
        assert!(s.alt_screen());
        assert!(s.modes() & modes::ALT_SCREEN != 0);
        // Entering the alt screen is a full frame.
        assert!(matches!(s.take_update(), Update::Full(_)));
        for i in 0..5 {
            s.advance(format!("tui{i}\r\n").as_bytes());
        }
        assert_eq!(s.top_line(), 1);
        s.advance(b"\x1b[?1049l");
        assert!(!s.alt_screen());
        assert_eq!(texts(&s.snapshot().lines), vec!["two", ""]);
    }

    #[test]
    fn modes_and_cursor_shape() {
        let (mut s, _) = screen(10, 2);
        s.advance(b"\x1b[?2004h\x1b[?1004h\x1b[?1000h\x1b[?1006h\x1b[?1h\x1b[5 q\x1b[?25l");
        let m = s.modes();
        for bit in [
            modes::BRACKETED_PASTE,
            modes::FOCUS_IN_OUT,
            modes::MOUSE_CLICK,
            modes::SGR_MOUSE,
            modes::APP_CURSOR,
        ] {
            assert!(m & bit != 0, "missing mode bit {bit}");
        }
        let c = s.cursor();
        assert_eq!(c.shape, CursorShape::Beam);
        assert!(c.blink);
        assert!(!c.visible);
    }

    #[test]
    fn search_corpus_spans_history_and_grid() {
        let (mut s, _) = screen(10, 2);
        // "h0" scrolls out when "g0"'s row is needed; "h1" and "g0" fill
        // the 2-row grid.
        s.advance(b"h0\r\nh1\r\ng0");
        let mut all = Vec::new();
        s.for_each_row(|l, r| all.push((l, r.text())));
        assert_eq!(
            all,
            vec![(0, "h0".into()), (1, "h1".into()), (2, "g0".into())]
        );
        assert_eq!(s.top_line(), 1);
        assert_eq!(s.last_nonempty_text(), "g0");
    }
}
