//! Whole-UI text zoom, over whichever scaling mechanism the terminal actually has.
//!
//! Terminals cannot change their font size from an application, but two escape-level
//! mechanisms can render bigger glyphs, and [`ZoomBackend`] builds the same whole-app
//! zoom out of either: it reports a virtual grid of `real / s` to ratatui — so every
//! view lays out exactly as if the window were smaller — and re-emits each diffed cell
//! run scaled. Layout code never learns zoom exists.
//!
//! - [`ZoomMode::Osc66`] — the kitty [text sizing protocol]: `ESC ] 66 ; s=N ; text ST`
//!   paints N×-scaled glyphs (plus an `n/d` fraction for the in-between levels), giving
//!   the full 100–300% ladder. kitty ≥ 0.40 today; adopters inherit it via the probe.
//! - [`ZoomMode::Decdhl`] — the venerable VT100 double-size lines (`ESC # 3` / `ESC # 4`
//!   top/bottom halves): exactly 2×, but supported far more widely — Windows Terminal,
//!   xterm, konsole. Each virtual row is emitted twice, once per half, with identical
//!   text (konsole requires the halves to match).
//!
//! At 100% every method delegates verbatim to the wrapped [`CrosstermBackend`]; the
//! escape-byte stream is byte-identical to running without this wrapper (same contract as
//! the animation flags: off means indistinguishable from before the feature existed).
//!
//! Support is detected once at startup ([`detect_mode`]): OSC 66 via the protocol's
//! documented cursor-position handshake, DECDHL via an autowrap-geometry probe (on a
//! real double-width line, wrap fires at half the columns — cursor addressing clamps
//! turned out not to be implemented by every renderer, e.g. Apple's Terminal renders
//! nothing double and fails both probes). Terminals with neither keep scale pinned at
//! 1 and the reducer explains via a status toast instead of quietly doing nothing.
//!
//! [text sizing protocol]: https://sw.kovidgoyal.net/kitty/text-sizing-protocol/

use std::cell::Cell as StateCell;
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU16, Ordering};

use crossterm::cursor::MoveTo;
use crossterm::queue;
use crossterm::style::{
    Attribute as CAttribute, Color as CColor, Colors as CColors, SetAttribute, SetBackgroundColor,
    SetColors, SetForegroundColor, SetUnderlineColor,
};
use ratatui::backend::{Backend, ClearType, CrosstermBackend, IntoCrossterm, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};
use ratatui::style::{Color, Modifier};
use unicode_width::UnicodeWidthStr;

/// Which glyph-scaling mechanism the terminal offers (see the module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZoomMode {
    /// Neither mechanism: zoom stays at 100% and the reducer explains via a toast.
    None,
    /// kitty text sizing protocol — the full percent ladder.
    Osc66,
    /// VT100 double-size lines — 100% or 200%, nothing in between.
    Decdhl,
}

impl ZoomMode {
    /// The zoom steps this mode can render, in percent. OSC 66 gets fine 25% steps near
    /// normal size (where most eyes live) and coarser ones above 2×; DECDHL is 2× only.
    pub fn levels(self) -> &'static [u16] {
        match self {
            ZoomMode::None => &[100],
            ZoomMode::Osc66 => LEVELS,
            ZoomMode::Decdhl => &[100, 200],
        }
    }

    /// The level the Settings "large text" toggle jumps to: comfortable 150% where the
    /// protocol can render it, the only available 200% on double-size-line terminals.
    pub fn big_percent(self) -> u16 {
        match self {
            ZoomMode::Decdhl => 200,
            _ => 150,
        }
    }
}

/// The OSC 66 zoom steps, in percent. Each level maps onto the protocol as an integer
/// cell scale `s` plus an optional fractional glyph scale `n/d` (see [`zoom_params`]):
/// the grid always coarsens in whole cells, the glyphs inside can be sized in between.
pub const LEVELS: &[u16] = &[100, 125, 150, 175, 200, 250, 300];

/// The protocol parameters for a zoom percent: the integer cell scale `s` (each virtual
/// cell occupies `s`×`s` physical cells — this is what layout and mouse mapping use) and
/// the optional `n/d` fraction rendering glyphs at `s·n/d` cells within those blocks.
/// The fraction never changes cell occupancy or cursor advance (verified against kitty:
/// `s=2:n=3:d=4` advances exactly like `s=2`).
pub fn zoom_params(percent: u16) -> (u16, Option<(u16, u16)>) {
    match snap(percent) {
        125 => (2, Some((5, 8))), // 2 · 5/8 = 1.25
        150 => (2, Some((3, 4))), // 2 · 3/4 = 1.5
        175 => (2, Some((7, 8))), // 2 · 7/8 = 1.75
        200 => (2, None),
        250 => (3, Some((5, 6))), // 3 · 5/6 = 2.5
        300 => (3, None),
        _ => (1, None),
    }
}

/// Snap an arbitrary persisted value to the nearest supported level. Ties break toward
/// the larger level, so a 150% saved under kitty still reads as "big" (→200%) on a
/// double-size-line terminal instead of silently collapsing to normal.
pub fn snap(percent: u16) -> u16 {
    snap_in(LEVELS, percent)
}

fn snap_in(levels: &[u16], percent: u16) -> u16 {
    levels
        .iter()
        .copied()
        .min_by_key(|l| (l.abs_diff(percent), u16::MAX - l))
        .unwrap_or(100)
}

/// The next level up or down from `percent`; saturates at the ends of [`LEVELS`].
pub fn step(percent: u16, up: bool) -> u16 {
    step_in(LEVELS, percent, up)
}

fn step_in(levels: &[u16], percent: u16, up: bool) -> u16 {
    let current = snap_in(levels, percent);
    let idx = levels.iter().position(|&l| l == current).unwrap_or(0);
    let idx = if up {
        (idx + 1).min(levels.len() - 1)
    } else {
        idx.saturating_sub(1)
    };
    levels[idx]
}

/// Shared zoom state — the detected [`ZoomMode`] plus the current level (percent) — read
/// by the backend on every draw and by the event translator to map physical mouse cells
/// onto the virtual grid. Plain atomics because writers (the reducer; `main` for the
/// mode) and readers (backend, translator) live on the same thread today — the Arc is
/// for ownership, not synchronisation subtlety.
#[derive(Clone, Debug)]
pub struct ZoomHandle {
    percent: Arc<AtomicU16>,
    mode: Arc<AtomicU8>,
}

impl Default for ZoomHandle {
    fn default() -> Self {
        Self {
            percent: Arc::new(AtomicU16::new(100)),
            mode: Arc::new(AtomicU8::new(0)),
        }
    }
}

impl ZoomHandle {
    /// The current level in percent (always one of the mode's levels).
    pub fn percent(&self) -> u16 {
        self.percent.load(Ordering::Relaxed)
    }

    pub fn mode(&self) -> ZoomMode {
        match self.mode.load(Ordering::Relaxed) {
            1 => ZoomMode::Osc66,
            2 => ZoomMode::Decdhl,
            _ => ZoomMode::None,
        }
    }

    /// Record the detected mode (done once at startup). Re-snaps the level so a percent
    /// set before detection can't outlive what the mode can render.
    pub fn set_mode(&self, mode: ZoomMode) {
        let raw = match mode {
            ZoomMode::None => 0,
            ZoomMode::Osc66 => 1,
            ZoomMode::Decdhl => 2,
        };
        self.mode.store(raw, Ordering::Relaxed);
        self.set(self.percent());
    }

    /// Whether the terminal can zoom at all.
    pub fn supported(&self) -> bool {
        self.mode() != ZoomMode::None
    }

    /// The integer cell scale of the current level — the factor between the physical
    /// grid and the virtual grid that layout and mouse events use. (In both modes a
    /// zoomed virtual cell occupies `s`×`s` physical cells.)
    pub fn scale(&self) -> u8 {
        match self.mode() {
            ZoomMode::Osc66 => zoom_params(self.percent()).0 as u8,
            ZoomMode::Decdhl if self.percent() > 100 => 2,
            _ => 1,
        }
    }

    /// The per-axis divisor that maps a physical mouse cell onto the virtual grid, as
    /// `(column_scale, row_scale)`. This mirrors the cursor geometry in
    /// [`ZoomBackend::get_cursor_position`]: OSC 66 scales both axes by `s`, but DECDHL
    /// doubles only the **row** (each virtual row is two physical `ESC#3`/`ESC#4` lines)
    /// while the **column** stays logical — the terminal paints each logical column at
    /// double width and reports mouse/cursor columns in that same logical space. Dividing
    /// the column by `s` too (as a single scalar would) lands clicks half a screen left of
    /// the glyph under the pointer, which is the large-text mouse bug on Windows Terminal.
    pub fn mouse_scale(&self) -> (u8, u8) {
        let s = self.scale();
        match self.mode() {
            ZoomMode::Decdhl if self.percent() > 100 => (1, s),
            _ => (s, s),
        }
    }

    /// Snap to the nearest level the mode can render and store. Returns the applied
    /// percent.
    pub fn set(&self, percent: u16) -> u16 {
        let percent = snap_in(self.mode().levels(), percent);
        self.percent.store(percent, Ordering::Relaxed);
        percent
    }

    /// The next level up or down from the current one; saturates at the mode's ends.
    pub fn step(&self, up: bool) -> u16 {
        step_in(self.mode().levels(), self.percent(), up)
    }

    /// The largest level this mode offers (for the bounds toast).
    pub fn max_percent(&self) -> u16 {
        self.mode().levels().last().copied().unwrap_or(100)
    }
}

/// Detect which zoom mechanism this terminal has. Must run with raw mode + the
/// alternate screen active and **before** the exclusive terminal event worker exists —
/// the probes read their CPR replies off stdin themselves (`crossterm::cursor::position()`).
///
/// Order matters: OSC 66 is strictly better (fine-grained levels), so it's checked
/// first on the terminal families that ship or answer it quickly; Windows Terminal is
/// trusted from `WT_SESSION` (its double-size-line support is documented, and the
/// Unix-style geometry probe can't be read back through the Windows console API);
/// everything else gets the DECDWL autowrap probe. `YTM_TUI_TEXT_SIZING` overrides:
/// `off` disables zoom entirely, `on` forces the OSC 66 probe, `dhl` forces double-size
/// lines without probing.
pub fn detect_mode() -> ZoomMode {
    match std::env::var("YTM_TUI_TEXT_SIZING").ok().as_deref() {
        Some("0" | "false" | "False" | "FALSE" | "off" | "Off" | "OFF") => return ZoomMode::None,
        Some("dhl" | "DHL" | "decdhl") => return ZoomMode::Decdhl,
        _ => {}
    }
    if should_probe_osc66() && probe_osc66() {
        return ZoomMode::Osc66;
    }
    if std::env::var_os("WT_SESSION").is_some() {
        return ZoomMode::Decdhl;
    }
    if should_probe_decdwl() && probe_decdwl() {
        return ZoomMode::Decdhl;
    }
    ZoomMode::None
}

/// Probe whether the terminal renders OSC 66 scaled text, using the handshake from the
/// protocol spec: emit a 2×-scaled space and check the cursor advanced exactly two
/// columns. Terminals without the protocol swallow the OSC (it's a well-formed escape
/// sequence) and leave the cursor untouched; terminals that only honour the `w=` half of
/// the spec (foot, today) don't advance for `s=` either.
///
/// The probe glyph is a space at the top-left of the still-blank alternate screen, so
/// nothing user-visible is left behind; the first frame paints over it.
pub fn probe_osc66() -> bool {
    let probe = || -> io::Result<bool> {
        {
            let mut out = io::stdout().lock();
            // Home the cursor so the 2-rows-tall probe block can't hang off the bottom
            // of the screen (where a supporting terminal would scroll and break the
            // row-stability check below).
            out.write_all(b"\x1b[H")?;
            out.flush()?;
        }
        let before = crossterm::cursor::position()?;
        {
            let mut out = io::stdout().lock();
            out.write_all(b"\x1b]66;s=2; \x1b\\")?;
            out.flush()?;
        }
        let after = crossterm::cursor::position()?;
        Ok(after.1 == before.1 && after.0 == before.0 + 2)
    };
    probe().unwrap_or(false)
}

/// Whether [`probe_osc66`] should run at all. The probe costs two cursor-position
/// round-trips (fast on any terminal that answers CPR, but crossterm's fallback timeout
/// is 2s per query on one that never answers), so it is gated to terminal families known
/// to answer CPR promptly — kitty implements the protocol today; ghostty/wezterm answer
/// the probe correctly (unsupported, for now) and pick the feature up automatically once
/// they ship it. `YTM_TUI_TEXT_SIZING=on` forces the probe, and tmux (which re-encodes
/// escapes for the outer terminal) is deliberately not probed.
fn should_probe_osc66() -> bool {
    if matches!(
        std::env::var("YTM_TUI_TEXT_SIZING").ok().as_deref(),
        Some("1" | "true" | "True" | "TRUE" | "on" | "On" | "ON")
    ) {
        return true;
    }
    let term = std::env::var("TERM").unwrap_or_default().to_lowercase();
    let term_program = std::env::var("TERM_PROGRAM")
        .unwrap_or_default()
        .to_lowercase();
    if term.starts_with("screen") || term.starts_with("tmux") {
        return false;
    }
    term.contains("kitty")
        || term.contains("ghostty")
        || term.contains("wezterm")
        || term_program.contains("ghostty")
        || term_program.contains("wezterm")
}

/// Probe whether the terminal implements VT100 double-width line *geometry*: on a real
/// DECDWL line autowrap fires at half the columns, so write just past half and see
/// whether the cursor left the row. Renderers that ignore `ESC # 6` keep the cursor on
/// the same row (Apple's Terminal, tested; ditto anything that silently swallows the
/// sequence), and terminals that only *store* the rendition without layout don't wrap
/// either — exactly the ones that would double-print every line if we emitted DECDHL.
///
/// Runs on the still-blank alternate screen; the probe line is reset to single width
/// and cleared, and the first frame repaints everything anyway.
fn probe_decdwl() -> bool {
    let probe = || -> io::Result<bool> {
        let (cols, _) = crossterm::terminal::size()?;
        if cols < 8 {
            return Ok(false);
        }
        {
            let mut out = io::stdout().lock();
            // Ensure autowrap, home, make row 1 double-width, then write one char past
            // the half-width boundary.
            out.write_all(b"\x1b[?7h\x1b[H\x1b#6")?;
            let fill = usize::from(cols / 2) + 1;
            out.write_all(&vec![b' '; fill])?;
            out.flush()?;
        }
        let after = crossterm::cursor::position()?;
        {
            let mut out = io::stdout().lock();
            // Undo: single-width again and wipe the probe rows.
            out.write_all(b"\x1b[H\x1b#5\x1b[2J\x1b[H")?;
            out.flush()?;
        }
        Ok(after.1 > 0)
    };
    probe().unwrap_or(false)
}

/// Where the DECDWL probe is worth its two CPR round-trips: any plausibly-interactive
/// Unix terminal that isn't a multiplexer (tmux/screen re-encode escapes for the outer
/// terminal, and double-size lines through them are a lie). Windows never probes —
/// Windows Terminal is trusted via `WT_SESSION` in [`detect_mode`], and legacy conhost
/// can't answer the query through the console API.
fn should_probe_decdwl() -> bool {
    if cfg!(windows) {
        return false;
    }
    let term = std::env::var("TERM").unwrap_or_default().to_lowercase();
    !(term.is_empty() || term == "dumb" || term.starts_with("screen") || term.starts_with("tmux"))
}

/// A [`Backend`] that scales the whole UI by an integer factor using OSC 66.
///
/// Scale 1 is a transparent pass-through. At scale N, ratatui sees a `real/N` grid;
/// `draw` re-emits each changed cell at `(x·N, y·N)` wrapped in `ESC ] 66 ; s=N ; … ST`,
/// merging style-identical contiguous cells into one escape per run. Cells whose symbol
/// already carries raw escape bytes (ratatui-image's protocol anchor cells) are printed
/// verbatim at the scaled position instead — wrapping them would corrupt the image
/// protocol handshake. Matching native-art protocols use this raw-anchor path while zoomed.
pub struct ZoomBackend<W: Write> {
    inner: CrosstermBackend<W>,
    zoom: ZoomHandle,
    physical_rows: StateCell<u16>,
    dhl_active: bool,
    prepared_physical_rows: u16,
    touched_dhl_rows: u16,
}

impl<W: Write> ZoomBackend<W> {
    pub fn new(inner: CrosstermBackend<W>, zoom: ZoomHandle) -> Self {
        Self {
            inner,
            zoom,
            physical_rows: StateCell::new(0),
            dhl_active: false,
            prepared_physical_rows: 0,
            touched_dhl_rows: 0,
        }
    }

    fn scale(&self) -> u16 {
        u16::from(self.zoom.scale())
    }

    /// One diffed run: style-identical cells at physically consecutive scaled positions,
    /// flushed as a single OSC 66 sequence. `frac` is the glyph fraction of the current
    /// level (e.g. `3/4` at 150%); it changes only the rendered glyph size, never the
    /// `s`-based cell geometry this method positions with.
    ///
    /// `CrosstermBackend` implements `io::Write` by forwarding to its writer, which is
    /// how the scaled path smuggles raw bytes out without the unstable `writer_mut()`.
    fn flush_run(
        &mut self,
        run: &mut String,
        x: u16,
        y: u16,
        s: u16,
        frac: Option<(u16, u16)>,
    ) -> io::Result<()> {
        if run.is_empty() {
            return Ok(());
        }
        queue!(&mut self.inner, MoveTo(x * s, y * s))?;
        match frac {
            Some((n, d)) => write!(&mut self.inner, "\x1b]66;s={s}:n={n}:d={d};{run}\x1b\\")?,
            None => write!(&mut self.inner, "\x1b]66;s={s};{run}\x1b\\")?,
        }
        run.clear();
        Ok(())
    }

    fn queue_style(
        &mut self,
        fg: Color,
        bg: Color,
        underline: Color,
        modifier: Modifier,
    ) -> io::Result<()> {
        let w = &mut self.inner;
        // Reset, then rebuild: runs are long (whole styled spans), so per-run resets cost
        // little and sidestep tracking removed modifiers one attribute at a time.
        queue!(w, SetAttribute(CAttribute::Reset))?;
        for (m, attr) in [
            (Modifier::BOLD, CAttribute::Bold),
            (Modifier::DIM, CAttribute::Dim),
            (Modifier::ITALIC, CAttribute::Italic),
            (Modifier::UNDERLINED, CAttribute::Underlined),
            (Modifier::SLOW_BLINK, CAttribute::SlowBlink),
            (Modifier::RAPID_BLINK, CAttribute::RapidBlink),
            (Modifier::REVERSED, CAttribute::Reverse),
            (Modifier::CROSSED_OUT, CAttribute::CrossedOut),
            (Modifier::HIDDEN, CAttribute::Hidden),
        ] {
            if modifier.contains(m) {
                queue!(w, SetAttribute(attr))?;
            }
        }
        queue!(
            w,
            SetColors(CColors::new(fg.into_crossterm(), bg.into_crossterm()))
        )?;
        if underline != Color::Reset {
            queue!(w, SetUnderlineColor(underline.into_crossterm()))?;
        }
        Ok(())
    }
}

impl<W: Write> ZoomBackend<W> {
    /// Prime every complete physical row pair with its DECDHL rendition. Konsole remembers line
    /// renditions across clears, and preparing the full grid up front keeps cursor-only image
    /// anchors from inheriting a stale single-width row after a resize.
    fn prepare_dhl_rows(&mut self) -> io::Result<()> {
        let physical_rows = self.physical_rows.get();
        let needs_prepare = !self.dhl_active || physical_rows != self.prepared_physical_rows;
        self.dhl_active = true;
        if !needs_prepare {
            return Ok(());
        }

        let paired_rows = physical_rows / 2 * 2;
        for row in 0..paired_rows {
            queue!(&mut self.inner, MoveTo(0, row))?;
            write!(
                &mut self.inner,
                "{}",
                if row % 2 == 0 { "\x1b#3" } else { "\x1b#4" }
            )?;
        }
        self.prepared_physical_rows = physical_rows;
        self.touched_dhl_rows = self.touched_dhl_rows.max(paired_rows);
        Ok(())
    }

    fn assert_dhl_pair(&mut self, y: u16) -> io::Result<()> {
        let top = y.saturating_mul(2);
        for (row, sequence) in [(top, "\x1b#3"), (top.saturating_add(1), "\x1b#4")] {
            queue!(&mut self.inner, MoveTo(0, row))?;
            write!(&mut self.inner, "{sequence}")?;
        }
        self.touched_dhl_rows = self.touched_dhl_rows.max(top.saturating_add(2));
        Ok(())
    }

    /// Restore every row that may have acquired a persistent double-height rendition. This runs
    /// exactly once when leaving DECDHL, followed by a clear/home so no half-glyph survives.
    fn reset_dhl_rows(&mut self) -> io::Result<()> {
        if !self.dhl_active {
            return Ok(());
        }

        let rows = self
            .physical_rows
            .get()
            .max(self.prepared_physical_rows)
            .max(self.touched_dhl_rows);
        for row in 0..rows {
            queue!(&mut self.inner, MoveTo(0, row))?;
            write!(&mut self.inner, "\x1b#5")?;
        }
        write!(&mut self.inner, "\x1b[2J\x1b[H")?;
        self.dhl_active = false;
        self.prepared_physical_rows = 0;
        self.touched_dhl_rows = 0;
        Ok(())
    }

    /// One diffed run in double-size-line mode: the run is written twice, `ESC # 3` on
    /// the top physical row and `ESC # 4` on the bottom one, with identical text —
    /// konsole requires matching halves, and the others don't mind. The rendition is
    /// re-asserted per run (a few bytes) so no per-line bookkeeping has to survive
    /// clears/resizes; `x` is a *logical* column (double-width lines address in halves).
    fn flush_run_dhl(&mut self, run: &mut String, x: u16, y: u16) -> io::Result<()> {
        if run.is_empty() {
            return Ok(());
        }
        for (half, sequence) in [(0, "\x1b#3"), (1, "\x1b#4")] {
            let row = y.saturating_mul(2).saturating_add(half);
            // Assert the rendition from column 0 (valid on the line in either state),
            // then land on the logical column and write.
            queue!(&mut self.inner, MoveTo(0, row))?;
            write!(&mut self.inner, "{sequence}")?;
            queue!(&mut self.inner, MoveTo(x, row))?;
            write!(&mut self.inner, "{run}")?;
        }
        self.touched_dhl_rows = self
            .touched_dhl_rows
            .max(y.saturating_mul(2).saturating_add(2));
        run.clear();
        Ok(())
    }

    fn draw_dhl<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let mut run = String::new();
        let mut run_pos: (u16, u16) = (0, 0);
        let mut next: Option<(u16, u16)> = None;
        let mut style: Option<(Color, Color, Color, Modifier)> = None;

        for (x, y, cell) in content {
            let sym = cell.symbol();
            // Matching Sixel image anchors are emitted once at the logical art origin. Assert the
            // physical row pair first, then forward the protocol bytes without duplicating them.
            if sym.contains('\u{1b}') {
                let (rx, ry) = run_pos;
                self.flush_run_dhl(&mut run, rx, ry)?;
                self.assert_dhl_pair(y)?;
                queue!(&mut self.inner, MoveTo(x, y.saturating_mul(2)))?;
                write!(&mut self.inner, "{sym}")?;
                style = None;
                next = None;
                continue;
            }

            let cell_style = (cell.fg, cell.bg, cell.underline_color, cell.modifier);
            if style != Some(cell_style) || next != Some((x, y)) {
                let (rx, ry) = run_pos;
                self.flush_run_dhl(&mut run, rx, ry)?;
                if style != Some(cell_style) {
                    self.queue_style(cell.fg, cell.bg, cell.underline_color, cell.modifier)?;
                    style = Some(cell_style);
                }
                run_pos = (x, y);
            }
            run.push_str(sym);
            next = Some((x + sym.width().max(1) as u16, y));
        }
        let (rx, ry) = run_pos;
        self.flush_run_dhl(&mut run, rx, ry)?;

        queue!(
            &mut self.inner,
            SetForegroundColor(CColor::Reset),
            SetBackgroundColor(CColor::Reset),
            SetUnderlineColor(CColor::Reset),
            SetAttribute(CAttribute::Reset),
        )
    }
}

impl<W: Write> Backend for ZoomBackend<W> {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        if self.scale() <= 1 {
            self.reset_dhl_rows()?;
            return self.inner.draw(content);
        }
        if self.zoom.mode() == ZoomMode::Decdhl {
            self.prepare_dhl_rows()?;
            return self.draw_dhl(content);
        }
        self.reset_dhl_rows()?;
        let (s, frac) = zoom_params(self.zoom.percent());

        let mut run = String::new();
        // Virtual origin of the pending run and the virtual cell the next style-matching
        // symbol must start at for the run to stay contiguous.
        let mut run_pos: (u16, u16) = (0, 0);
        let mut next: Option<(u16, u16)> = None;
        let mut style: Option<(Color, Color, Color, Modifier)> = None;

        for (x, y, cell) in content {
            let sym = cell.symbol();
            // ratatui-image anchor cells smuggle whole escape sequences through the
            // buffer; forward them untouched (see struct docs).
            if sym.contains('\u{1b}') {
                let (rx, ry) = run_pos;
                self.flush_run(&mut run, rx, ry, s, frac)?;
                queue!(&mut self.inner, MoveTo(x * s, y * s))?;
                write!(&mut self.inner, "{sym}")?;
                style = None;
                next = None;
                continue;
            }

            let cell_style = (cell.fg, cell.bg, cell.underline_color, cell.modifier);
            if style != Some(cell_style) || next != Some((x, y)) {
                let (rx, ry) = run_pos;
                self.flush_run(&mut run, rx, ry, s, frac)?;
                if style != Some(cell_style) {
                    self.queue_style(cell.fg, cell.bg, cell.underline_color, cell.modifier)?;
                    style = Some(cell_style);
                }
                run_pos = (x, y);
            }
            run.push_str(sym);
            // Wide glyphs advance more than one virtual column; the buffer diff skips
            // their continuation cells, so track the true width for contiguity.
            next = Some((x + sym.width().max(1) as u16, y));
        }
        let (rx, ry) = run_pos;
        self.flush_run(&mut run, rx, ry, s, frac)?;

        queue!(
            &mut self.inner,
            SetForegroundColor(CColor::Reset),
            SetBackgroundColor(CColor::Reset),
            SetUnderlineColor(CColor::Reset),
            SetAttribute(CAttribute::Reset),
        )
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        let s = self.scale();
        let p = self.inner.get_cursor_position()?;
        // Double-width lines address columns logically, so only the row is scaled there.
        if self.zoom.mode() == ZoomMode::Decdhl {
            return Ok(Position { x: p.x, y: p.y / s });
        }
        Ok(Position {
            x: p.x / s,
            y: p.y / s,
        })
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let s = self.scale();
        let p: Position = position.into();
        if self.zoom.mode() == ZoomMode::Decdhl {
            return self.inner.set_cursor_position(Position {
                x: p.x,
                y: p.y.saturating_mul(s),
            });
        }
        self.inner.set_cursor_position(Position {
            x: p.x.saturating_mul(s),
            y: p.y.saturating_mul(s),
        })
    }

    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.inner.append_lines(n)
    }

    fn size(&self) -> io::Result<Size> {
        let s = self.scale();
        let size = self.inner.size()?;
        self.physical_rows.set(size.height);
        Ok(Size {
            width: size.width / s,
            height: size.height / s,
        })
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        let s = self.scale();
        let mut ws = self.inner.window_size()?;
        self.physical_rows.set(ws.columns_rows.height);
        ws.columns_rows.width /= s;
        ws.columns_rows.height /= s;
        Ok(ws)
    }

    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.inner)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Shared byte sink: `CrosstermBackend::writer()` is unstable-gated, so tests keep
    /// their own handle to the buffer the backend writes into.
    #[derive(Clone, Default)]
    struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn backend_with_percent(percent: u16) -> (ZoomBackend<CaptureWriter>, CaptureWriter) {
        let zoom = ZoomHandle::default();
        zoom.set_mode(ZoomMode::Osc66);
        zoom.set(percent);
        let sink = CaptureWriter::default();
        let backend = ZoomBackend::new(CrosstermBackend::new(sink.clone()), zoom);
        (backend, sink)
    }

    fn drawn_bytes(sink: &CaptureWriter) -> String {
        String::from_utf8(sink.0.lock().unwrap().clone()).unwrap()
    }

    fn clear_drawn_bytes(sink: &CaptureWriter) {
        sink.0.lock().unwrap().clear();
    }

    fn cell(sym: &str) -> Cell {
        let mut c = Cell::default();
        c.set_symbol(sym);
        c
    }

    #[test]
    fn handle_snaps_to_supported_levels() {
        let zoom = ZoomHandle::default();
        assert_eq!(zoom.percent(), 100);
        assert_eq!(zoom.scale(), 1);
        assert!(!zoom.supported(), "mode defaults to None");
        assert_eq!(zoom.set(300), 100, "without a mode nothing zooms");
        zoom.set_mode(ZoomMode::Osc66);
        assert_eq!(zoom.set(500), 300);
        assert_eq!(zoom.scale(), 3);
        assert_eq!(zoom.set(160), 150, "snaps to the nearest level");
        assert_eq!(
            zoom.scale(),
            2,
            "fractional levels still coarsen the grid by 2"
        );
        assert_eq!(zoom.set(0), 100);
    }

    #[test]
    fn steps_walk_the_level_ladder_and_saturate() {
        let up: Vec<u16> = LEVELS.iter().map(|&l| step(l, true)).collect();
        assert_eq!(up, vec![125, 150, 175, 200, 250, 300, 300]);
        let down: Vec<u16> = LEVELS.iter().map(|&l| step(l, false)).collect();
        assert_eq!(down, vec![100, 100, 125, 150, 175, 200, 250]);
    }

    #[test]
    fn fractional_levels_emit_the_glyph_fraction() {
        let (mut backend, sink) = backend_with_percent(150);
        let a = cell("a");
        backend.draw([(0u16, 0u16, &a)].into_iter()).unwrap();
        backend.flush().unwrap();
        let out = drawn_bytes(&sink);
        assert!(
            out.contains("\x1b]66;s=2:n=3:d=4;a\x1b\\"),
            "150% = 3/4 glyphs on a 2x grid: {out:?}"
        );

        let (mut backend, sink) = backend_with_percent(250);
        let a = cell("a");
        backend.draw([(0u16, 0u16, &a)].into_iter()).unwrap();
        backend.flush().unwrap();
        let out = drawn_bytes(&sink);
        assert!(
            out.contains("\x1b]66;s=3:n=5:d=6;a\x1b\\"),
            "250% = 5/6 glyphs on a 3x grid: {out:?}"
        );
    }

    #[test]
    fn scale_one_emits_no_osc_66() {
        let (mut backend, sink) = backend_with_percent(100);
        let a = cell("a");
        backend.draw([(0u16, 0u16, &a)].into_iter()).unwrap();
        backend.flush().unwrap();
        let out = drawn_bytes(&sink);
        assert!(
            !out.contains("\x1b]66"),
            "scale 1 must stay pass-through: {out:?}"
        );
        assert!(out.contains('a'));
    }

    #[test]
    fn scaled_draw_wraps_runs_and_scales_positions() {
        let (mut backend, sink) = backend_with_percent(200);
        let (a, b) = (cell("a"), cell("b"));
        // Contiguous same-style cells at virtual (3,2)-(4,2) → one OSC run at physical (6,4).
        backend
            .draw([(3u16, 2u16, &a), (4u16, 2u16, &b)].into_iter())
            .unwrap();
        backend.flush().unwrap();
        let out = drawn_bytes(&sink);
        assert!(out.contains("\x1b[5;7H"), "MoveTo must be scaled: {out:?}");
        assert!(
            out.contains("\x1b]66;s=2;ab\x1b\\"),
            "cells must merge into one run: {out:?}"
        );
    }

    #[test]
    fn style_change_splits_runs() {
        let (mut backend, sink) = backend_with_percent(200);
        let a = cell("a");
        let mut b = cell("b");
        b.fg = Color::Red;
        backend
            .draw([(0u16, 0u16, &a), (1u16, 0u16, &b)].into_iter())
            .unwrap();
        backend.flush().unwrap();
        let out = drawn_bytes(&sink);
        assert!(out.contains("\x1b]66;s=2;a\x1b\\"));
        assert!(out.contains("\x1b]66;s=2;b\x1b\\"));
    }

    #[test]
    fn wide_glyphs_keep_runs_contiguous() {
        let (mut backend, sink) = backend_with_percent(200);
        let (han, a) = (cell("한"), cell("a"));
        // "한" occupies virtual 0-1; "a" at virtual 2 continues the run physically.
        backend
            .draw([(0u16, 0u16, &han), (2u16, 0u16, &a)].into_iter())
            .unwrap();
        backend.flush().unwrap();
        let out = drawn_bytes(&sink);
        assert!(
            out.contains("\x1b]66;s=2;한a\x1b\\"),
            "wide glyph must not split the run: {out:?}"
        );
    }

    #[test]
    fn escape_bearing_cells_pass_through_unwrapped() {
        let (mut backend, sink) = backend_with_percent(200);
        let anchor = cell("\x1b_Gi=1\x1b\\");
        backend.draw([(1u16, 1u16, &anchor)].into_iter()).unwrap();
        backend.flush().unwrap();
        let out = drawn_bytes(&sink);
        assert!(out.contains("\x1b_Gi=1\x1b\\"));
        assert!(
            !out.contains("\x1b]66;s=2;\x1b_G"),
            "image anchors must not be OSC-wrapped: {out:?}"
        );
    }

    fn dhl_backend() -> (ZoomBackend<CaptureWriter>, CaptureWriter, ZoomHandle) {
        let zoom = ZoomHandle::default();
        zoom.set_mode(ZoomMode::Decdhl);
        zoom.set(200);
        let sink = CaptureWriter::default();
        let backend = ZoomBackend::new(CrosstermBackend::new(sink.clone()), zoom.clone());
        (backend, sink, zoom)
    }

    #[test]
    fn decdhl_mode_offers_only_normal_and_double() {
        let zoom = ZoomHandle::default();
        zoom.set_mode(ZoomMode::Decdhl);
        assert_eq!(
            zoom.set(150),
            200,
            "a kitty-saved 150% must stay big, not collapse"
        );
        assert_eq!(zoom.set(125), 100, "barely-zoomed snaps down");
        zoom.set(100);
        assert_eq!(zoom.step(true), 200);
        zoom.set(200);
        assert_eq!(zoom.step(true), 200, "saturates at the top");
        assert_eq!(zoom.step(false), 100);
        assert_eq!(zoom.max_percent(), 200);
        assert_eq!(zoom.scale(), 2);
    }

    #[test]
    fn decdhl_draw_emits_identical_top_and_bottom_halves() {
        let (mut backend, sink, _) = dhl_backend();
        let (a, b) = (cell("a"), cell("b"));
        // Contiguous cells at virtual (3,2)-(4,2): logical column 3, physical rows 4+5.
        backend
            .draw([(3u16, 2u16, &a), (4u16, 2u16, &b)].into_iter())
            .unwrap();
        backend.flush().unwrap();
        let out = drawn_bytes(&sink);
        assert!(
            out.contains("\x1b[5;1H\x1b#3\x1b[5;4Hab"),
            "top half: rendition then logical column: {out:?}"
        );
        assert!(
            out.contains("\x1b[6;1H\x1b#4\x1b[6;4Hab"),
            "bottom half repeats the same text: {out:?}"
        );
        assert!(
            !out.contains("\x1b]66"),
            "no OSC 66 in DECDHL mode: {out:?}"
        );
    }

    #[test]
    fn decdhl_cursor_maps_logical_columns_and_doubled_rows() {
        let (mut backend, sink, _) = dhl_backend();
        backend
            .set_cursor_position(Position { x: 5, y: 3 })
            .unwrap();
        backend.flush().unwrap();
        let out = drawn_bytes(&sink);
        assert!(
            out.contains("\x1b[7;6H"),
            "row doubles, column stays logical: {out:?}"
        );
    }

    #[test]
    fn mouse_scale_is_per_axis_and_matches_the_cursor_geometry() {
        let zoom = ZoomHandle::default();
        // No zoom: identity on both axes.
        assert_eq!(zoom.mouse_scale(), (1, 1));
        // OSC 66 is symmetric — both axes divide by the cell scale.
        zoom.set_mode(ZoomMode::Osc66);
        zoom.set(200);
        assert_eq!(zoom.mouse_scale(), (2, 2));
        // DECDHL doubles the row only; the column stays logical (mirrors
        // `get_cursor_position`, which scales the row and leaves the column alone).
        zoom.set_mode(ZoomMode::Decdhl);
        zoom.set(200);
        assert_eq!(zoom.mouse_scale(), (1, 2));
        // At 100% DECDHL is inert.
        zoom.set(100);
        assert_eq!(zoom.mouse_scale(), (1, 1));
    }

    #[test]
    fn decdhl_raw_image_anchor_is_emitted_once_on_a_primed_pair() {
        let (mut backend, sink, _) = dhl_backend();
        let raw = "\x1bPqSIXEL\x1b\\";
        let anchor = cell(raw);

        backend.draw([(3u16, 2u16, &anchor)].into_iter()).unwrap();
        backend.flush().unwrap();
        let out = drawn_bytes(&sink);

        assert_eq!(out.matches(raw).count(), 1, "raw protocol bytes: {out:?}");
        assert!(out.contains("\x1b[5;1H\x1b#3"), "top row primed: {out:?}");
        assert!(
            out.contains("\x1b[6;1H\x1b#4"),
            "bottom row primed: {out:?}"
        );
        assert!(
            out.contains(&format!("\x1b[5;4H{raw}")),
            "anchor lands at logical x / physical y: {out:?}"
        );
    }

    #[test]
    fn decdhl_primes_all_rows_again_after_physical_height_changes() {
        let (mut backend, sink, _) = dhl_backend();
        backend.physical_rows.set(4);
        backend.draw(std::iter::empty()).unwrap();
        backend.flush().unwrap();
        let first = drawn_bytes(&sink);
        assert_eq!(first.matches("\x1b#3").count(), 2);
        assert_eq!(first.matches("\x1b#4").count(), 2);

        clear_drawn_bytes(&sink);
        backend.physical_rows.set(6);
        backend.draw(std::iter::empty()).unwrap();
        backend.flush().unwrap();
        let resized = drawn_bytes(&sink);
        assert_eq!(resized.matches("\x1b#3").count(), 3);
        assert_eq!(resized.matches("\x1b#4").count(), 3);
    }

    #[test]
    fn leaving_decdhl_resets_each_row_then_clears_exactly_once() {
        let (mut backend, sink, zoom) = dhl_backend();
        backend.physical_rows.set(6);
        let a = cell("a");
        backend.draw([(0u16, 0u16, &a)].into_iter()).unwrap();
        backend.flush().unwrap();

        clear_drawn_bytes(&sink);
        zoom.set(100);
        let n = cell("n");
        backend.draw([(0u16, 0u16, &n)].into_iter()).unwrap();
        backend.flush().unwrap();
        let reset = drawn_bytes(&sink);
        assert_eq!(
            reset.matches("\x1b#5").count(),
            6,
            "reset every row: {reset:?}"
        );
        assert!(reset.contains("\x1b[2J\x1b[H"), "clear and home: {reset:?}");

        clear_drawn_bytes(&sink);
        backend.draw([(1u16, 0u16, &n)].into_iter()).unwrap();
        backend.flush().unwrap();
        let steady = drawn_bytes(&sink);
        assert!(!steady.contains("\x1b#5"), "reset is one-shot: {steady:?}");
        assert!(
            !steady.contains("\x1b[2J\x1b[H"),
            "clear is one-shot: {steady:?}"
        );
    }

    #[test]
    fn virtual_size_divides_by_scale() {
        // size() needs a real tty on the inner backend, so exercise the arithmetic via
        // the cursor mapping instead: virtual (5,3) at scale 3 → physical (15,9).
        let (mut backend, sink) = backend_with_percent(300);
        backend
            .set_cursor_position(Position { x: 5, y: 3 })
            .unwrap();
        backend.flush().unwrap();
        let out = drawn_bytes(&sink);
        assert!(
            out.contains("\x1b[10;16H"),
            "cursor must map to the scaled grid: {out:?}"
        );
    }
}
