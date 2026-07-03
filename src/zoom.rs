//! Whole-UI text zoom via the kitty text sizing protocol (OSC 66).
//!
//! Terminals cannot change their font size from an application, but kitty ≥ 0.40 (and,
//! progressively, other terminals adopting the [text sizing protocol]) can render a run of
//! text at an integer multiple of the cell size: `ESC ] 66 ; s=N ; text ST` paints `text`
//! as N×-scaled glyphs occupying an N-rows-tall block, advancing the cursor N cells per
//! column of text. [`ZoomBackend`] builds a whole-app zoom out of that single primitive:
//! at scale N it reports a virtual grid of `real / N` to ratatui — so every view lays out
//! exactly as if the window were smaller — and re-emits each diffed cell as an OSC 66 run
//! at the N×-scaled position. Layout code never learns zoom exists.
//!
//! At scale 1 every method delegates verbatim to the wrapped [`CrosstermBackend`]; the
//! escape-byte stream is byte-identical to running without this wrapper (same contract as
//! the animation flags: off means indistinguishable from before the feature existed).
//!
//! Support is probed once at startup ([`probe_support`]) with the protocol's documented
//! cursor-position handshake; unsupported terminals keep scale pinned at 1 and the
//! reducer explains via a status toast instead of quietly doing nothing.
//!
//! [text sizing protocol]: https://sw.kovidgoyal.net/kitty/text-sizing-protocol/

use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};

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

/// The supported zoom steps, in percent. Fine 25% steps near normal size (where most
/// eyes live), coarser above 2×. Each level maps onto the protocol as an integer cell
/// scale `s` plus an optional fractional glyph scale `n/d` (see [`zoom_params`]): the
/// grid always coarsens in whole cells, the glyphs inside can be sized in between.
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

/// Snap an arbitrary persisted value to the nearest supported level.
pub fn snap(percent: u16) -> u16 {
    LEVELS
        .iter()
        .copied()
        .min_by_key(|l| l.abs_diff(percent))
        .unwrap_or(100)
}

/// The next level up or down from `percent`; saturates at the ends of [`LEVELS`].
pub fn step(percent: u16, up: bool) -> u16 {
    let current = snap(percent);
    let idx = LEVELS.iter().position(|&l| l == current).unwrap_or(0);
    let idx = if up {
        (idx + 1).min(LEVELS.len() - 1)
    } else {
        idx.saturating_sub(1)
    };
    LEVELS[idx]
}

/// Shared zoom level (percent), read by the backend on every draw and by the event
/// translator to map physical mouse cells onto the virtual grid. A plain atomic because
/// writers (the reducer) and readers (backend, translator) live on the same thread
/// today — the Arc is for ownership, not synchronisation subtlety.
#[derive(Clone, Debug)]
pub struct ZoomHandle(Arc<AtomicU16>);

impl Default for ZoomHandle {
    fn default() -> Self {
        Self(Arc::new(AtomicU16::new(100)))
    }
}

impl ZoomHandle {
    /// The current level in percent (always one of [`LEVELS`]).
    pub fn percent(&self) -> u16 {
        self.0.load(Ordering::Relaxed)
    }

    /// The integer cell scale of the current level — the factor between the physical
    /// grid and the virtual grid that layout and mouse events use.
    pub fn scale(&self) -> u8 {
        zoom_params(self.percent()).0 as u8
    }

    /// Snap to the nearest supported level and store. Returns the applied percent.
    pub fn set(&self, percent: u16) -> u16 {
        let percent = snap(percent);
        self.0.store(percent, Ordering::Relaxed);
        percent
    }
}

/// Probe whether the terminal renders OSC 66 scaled text, using the handshake from the
/// protocol spec: emit a 2×-scaled space and check the cursor advanced exactly two
/// columns. Terminals without the protocol swallow the OSC (it's a well-formed escape
/// sequence) and leave the cursor untouched; terminals that only honour the `w=` half of
/// the spec (foot, today) don't advance for `s=` either. Must run with raw mode + the
/// alternate screen active and **before** the crossterm `EventStream` exists —
/// `crossterm::cursor::position()` reads the CPR reply off stdin itself.
///
/// The probe glyph is a space at the top-left of the still-blank alternate screen, so
/// nothing user-visible is left behind; the first frame paints over it.
pub fn probe_support() -> bool {
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

/// Whether [`probe_support`] should run at all. The probe costs two cursor-position
/// round-trips (fast on any terminal that answers CPR, but crossterm's fallback timeout
/// is 2s per query on one that never answers), so it is gated to terminal families known
/// to answer CPR promptly — kitty implements the protocol today; ghostty/wezterm answer
/// the probe correctly (unsupported, for now) and pick the feature up automatically once
/// they ship it. `YTM_TUI_TEXT_SIZING=on|off` overrides in either direction, and tmux
/// (which re-encodes escapes for the outer terminal) is deliberately not probed.
pub fn should_probe() -> bool {
    match std::env::var("YTM_TUI_TEXT_SIZING").ok().as_deref() {
        Some("0" | "false" | "False" | "FALSE" | "off" | "Off" | "OFF") => return false,
        Some("1" | "true" | "True" | "TRUE" | "on" | "On" | "ON") => return true,
        _ => {}
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

/// A [`Backend`] that scales the whole UI by an integer factor using OSC 66.
///
/// Scale 1 is a transparent pass-through. At scale N, ratatui sees a `real/N` grid;
/// `draw` re-emits each changed cell at `(x·N, y·N)` wrapped in `ESC ] 66 ; s=N ; … ST`,
/// merging style-identical contiguous cells into one escape per run. Cells whose symbol
/// already carries raw escape bytes (ratatui-image's protocol anchor cells) are printed
/// verbatim at the scaled position instead — wrapping them would corrupt the image
/// protocol handshake. (Pixel album art is hidden by the views while zoomed; this is a
/// safety net, not a rendering path.)
pub struct ZoomBackend<W: Write> {
    inner: CrosstermBackend<W>,
    zoom: ZoomHandle,
}

impl<W: Write> ZoomBackend<W> {
    pub fn new(inner: CrosstermBackend<W>, zoom: ZoomHandle) -> Self {
        Self { inner, zoom }
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

impl<W: Write> Backend for ZoomBackend<W> {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let (s, frac) = zoom_params(self.zoom.percent());
        if s <= 1 {
            return self.inner.draw(content);
        }

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
        Ok(Position {
            x: p.x / s,
            y: p.y / s,
        })
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let s = self.scale();
        let p: Position = position.into();
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
        Ok(Size {
            width: size.width / s,
            height: size.height / s,
        })
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        let s = self.scale();
        let mut ws = self.inner.window_size()?;
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
        zoom.set(percent);
        let sink = CaptureWriter::default();
        let backend = ZoomBackend::new(CrosstermBackend::new(sink.clone()), zoom);
        (backend, sink)
    }

    fn drawn_bytes(sink: &CaptureWriter) -> String {
        String::from_utf8(sink.0.lock().unwrap().clone()).unwrap()
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
