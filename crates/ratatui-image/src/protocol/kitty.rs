//! Kitty protocol.
//!
//! Transmits the image once on first render, tracked via an AtomicBool, and then subsequentially
//! renders the image with the [unicode-placeholders] feature of the [kitty protocol].
//!
//! [unicode-placeholders]: https://sw.kovidgoyal.net/kitty/graphics-protocol/#unicode-placeholders
//! [kitty protocol]: https://sw.kovidgoyal.net/kitty/graphics-protocol
use std::fmt::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::protocol::UNIT_WIDTH;
use crate::{RenderScale, Result, picker::cap_parser::Parser};
use image::DynamicImage;
use ratatui::buffer::CellDiffOption;
use ratatui::layout::Size;
use ratatui::{buffer::Buffer, layout::Rect};

use super::{ProtocolTrait, StatefulProtocolTrait};

// Keep graphics-protocol album art behind all TUI text/popups. Kitty treats negative
// z-index placements as below text; values below INT32_MIN/2 also stay below cells with
// non-default backgrounds, which matters for highlighted popup rows.
const TEXT_BACKGROUND_Z_INDEX: i32 = i32::MIN / 2 - 1;

#[derive(Default, Clone)]
struct KittyProtoState {
    transmitted: Arc<AtomicBool>,
    transmit_str: Option<String>,
    id: (u32, String, u16), // Full ID, Formatted color ID, ID extra part for diacritic
    // yututui patch: zoomed Kitty uses one explicit transmit-and-place anchor instead of Unicode
    // placeholders; scale one keeps the upstream-compatible virtual path byte-identical.
    direct: bool,
}

impl KittyProtoState {
    fn new(img: &DynamicImage, id: u32, is_tmux: bool) -> Self {
        Self::new_with_z_index(img, id, is_tmux, TEXT_BACKGROUND_Z_INDEX)
    }

    fn new_with_z_index(img: &DynamicImage, id: u32, is_tmux: bool, z_index: i32) -> Self {
        let transmit_str = transmit_virtual(img, id, is_tmux, z_index);
        let [id_extra, id_r, id_g, id_b] = id.to_be_bytes();
        let id_color = format!("\x1b[38;2;{id_r};{id_g};{id_b}m");
        let id_extra = u16::from(id_extra);
        Self {
            transmitted: Arc::new(AtomicBool::new(false)),
            transmit_str: Some(transmit_str),
            id: (id, id_color, id_extra),
            direct: false,
        }
    }

    fn new_with_z_index_and_scale(
        img: &DynamicImage,
        id: u32,
        is_tmux: bool,
        z_index: i32,
        size: Size,
        render_scale: RenderScale,
    ) -> Self {
        let render_scale = render_scale.normalized();
        if render_scale == RenderScale::Normal {
            return Self::new_with_z_index(img, id, is_tmux, z_index);
        }

        let transmit_str = transmit_direct(img, id, is_tmux, z_index, size, render_scale);
        let [id_extra, id_r, id_g, id_b] = id.to_be_bytes();
        let id_color = format!("\x1b[38;2;{id_r};{id_g};{id_b}m");
        Self {
            transmitted: Arc::new(AtomicBool::new(false)),
            transmit_str: Some(transmit_str),
            id: (id, id_color, u16::from(id_extra)),
            direct: true,
        }
    }

    // yututui patch: ALWAYS return the transmit sequence (upstream returned it only on the first
    // render, gated by `transmitted`). The transmit blob is prepended to image-row 0's first cell;
    // dropping it on the 2nd render made that cell's string *change*, so ratatui re-flushed it and
    // re-planted the row's unicode placeholders across the full width — resurfacing the image
    // *through* the top edge of any popup drawn over row 0 (the `eq:`/`radio:` dropdowns). Keeping
    // the blob makes the cell byte-identical every frame, so ratatui's frame diff skips it after the
    // first flush: the image is still transmitted to the terminal exactly once, but the cell never
    // "changes back" and never re-plants over the popup. The `swap` is kept (result ignored) only so
    // the `transmitted` field stays referenced.
    fn make_transmit(&self) -> Option<&str> {
        let _already_sent = self.transmitted.swap(true, Ordering::SeqCst);
        self.transmit_str.as_deref()
    }
}

#[derive(Clone, Default)]
pub struct Kitty {
    proto_state: KittyProtoState,
    size: Size,
}

impl Kitty {
    pub fn new(image: DynamicImage, size: Size, id: u32, is_tmux: bool) -> Result<Self> {
        let proto_state = KittyProtoState::new(&image, id, is_tmux);
        Ok(Self { proto_state, size })
    }

    pub fn new_with_z_index(
        image: DynamicImage,
        size: Size,
        id: u32,
        is_tmux: bool,
        z_index: i32,
    ) -> Result<Self> {
        let proto_state = KittyProtoState::new_with_z_index(&image, id, is_tmux, z_index);
        Ok(Self { proto_state, size })
    }

    /// Only for SlicedImage
    pub(crate) fn render_with_skip(&self, area: Rect, buf: &mut Buffer, skip_line_count: usize) {
        // Transmit only once. This is why self is mut.
        let seq = self.proto_state.make_transmit();

        render(
            area,
            self.size,
            buf,
            &self.proto_state.id,
            seq,
            skip_line_count,
        );
    }
}

impl ProtocolTrait for Kitty {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        // Transmit only once, track at this point via the AtomicBool in proto_state.
        let seq = self.proto_state.make_transmit();

        render(area, self.size, buf, &self.proto_state.id, seq, 0);
    }

    fn size(&self) -> Size {
        self.size
    }
}

#[derive(Clone)]
pub struct StatefulKitty {
    id: (u32, String, u16), // Full ID, Formatted color ID, ID extra part for diacritic
    size: Size,
    proto_state: KittyProtoState,
    is_tmux: bool,
    z_index: i32,
}

impl StatefulKitty {
    pub fn new(id: u32, is_tmux: bool) -> StatefulKitty {
        Self::new_with_z_index(id, is_tmux, TEXT_BACKGROUND_Z_INDEX)
    }

    pub fn new_with_z_index(id: u32, is_tmux: bool, z_index: i32) -> StatefulKitty {
        let [id_extra, id_r, id_g, id_b] = id.to_be_bytes();
        let id_color = format!("\x1b[38;2;{id_r};{id_g};{id_b}m");
        let id_extra = u16::from(id_extra);
        StatefulKitty {
            id: (id, id_color, id_extra),
            size: Size::default(),
            proto_state: KittyProtoState::default(),
            is_tmux,
            z_index,
        }
    }

    pub(crate) fn mark_rows_for_redraw(&self, area: Rect, damage: Rect, buf: &mut Buffer) {
        // yututui patch: direct placements are complete at their single transmit anchor.
        if self.proto_state.direct {
            return;
        }
        mark_rows_for_redraw(area, self.size, buf, &self.id, damage, 0);
    }
}

impl ProtocolTrait for StatefulKitty {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        // Transmit only once. This is why self is mut.
        let seq = self.proto_state.make_transmit();

        if self.proto_state.direct {
            render_direct(area, self.size, buf, seq);
        } else {
            render(area, self.size, buf, &self.id, seq, 0);
        }
    }

    fn size(&self) -> Size {
        self.size
    }
}

impl StatefulProtocolTrait for StatefulKitty {
    fn resize_encode(&mut self, img: DynamicImage, size: Size) -> Result<()> {
        self.resize_encode_scaled(img, size, RenderScale::Normal)
    }

    fn resize_encode_scaled(
        &mut self,
        img: DynamicImage,
        size: Size,
        render_scale: RenderScale,
    ) -> Result<()> {
        // yututui patch: scaled state chooses explicit direct placement; Normal delegates to the
        // exact virtual-placement constructor used before native zoom support.
        self.size = size;
        // If resized then we must transmit again.
        self.proto_state = KittyProtoState::new_with_z_index_and_scale(
            &img,
            self.id.0,
            self.is_tmux,
            self.z_index,
            size,
            render_scale,
        );
        Ok(())
    }
}

fn render_direct(area: Rect, size: Size, buf: &mut Buffer, seq: Option<&str>) {
    let Some(seq) = seq else {
        return;
    };
    let width = area.width.min(size.width);
    let height = area.height.min(size.height);
    if width == 0 || height == 0 {
        return;
    }

    if let Some(cell) = buf.cell_mut((area.left(), area.top())) {
        cell.set_symbol(seq).set_diff_option(UNIT_WIDTH);
    }
    for y in 0..height {
        for x in 0..width {
            if x == 0 && y == 0 {
                continue;
            }
            if let Some(cell) = buf.cell_mut((area.left() + x, area.top() + y)) {
                cell.set_diff_option(CellDiffOption::Skip);
            }
        }
    }
}

fn render(
    area: Rect,
    size: Size,
    buf: &mut Buffer,
    (_, id_color, id_extra): &(u32, String, u16),
    mut seq: Option<&str>,
    skip_line_count: usize,
) {
    // yututui patch: clamp the width to the diacritic table too (upstream only clamps height).
    // Every cell below now carries an *explicit* column diacritic, and the table caps at
    // `DIACRITICS.len()`; beyond that a column can't be addressed, so don't emit it.
    let full_width = area.width.min(size.width).min(DIACRITICS.len() as u16);
    let width_usize = usize::from(full_width);

    let estimated_placeholder_row_size = id_color.len() +
        30 +  // diacritics
        (width_usize * 16) +  // yututui patch: explicit per-cell diacritics (~4 chars/cell)
        30; // restore cursor dance
    let estimated_transmit_row_size =
        estimated_placeholder_row_size + if let Some(seq) = seq { seq.len() } else { 0 };
    let mut symbol = String::with_capacity(estimated_transmit_row_size);

    // Restore saved cursor position including color, and now we have to move back to
    // the end of the area.
    let right = area.width - 1;
    let down = area.height - 1;
    let restore_cursor = format!("\x1b[u\x1b[{right}C\x1b[{down}B");

    // Clamp to effectively 297, the number of placeholders in the Kitty protocol.
    // Anything beyond would just render the something that's wrong, so skip.
    let height = area.height.min(size.height).min(DIACRITICS.len() as u16);
    for y in 0..height {
        // Draw each line of unicode placeholders but all into the first cell.
        // I couldn't work out actually drawing into each cell of the buffer so
        // that `.set_skip(true)` would be made unnecessary. Maybe some other escape
        // sequence gets sneaked in somehow.
        // It could also be made so that each cell starts and ends its own escape sequence
        // with the image id, but maybe that's worse.
        symbol.clear();
        if y == 1 {
            symbol.shrink_to(estimated_placeholder_row_size);
        }

        // If not transmitted in previous renders, only transmit once at the
        // first line.
        if let Some(seq) = seq.take() {
            symbol.push_str(seq);
        }

        let row_y = y + skip_line_count as u16;

        // Save cursor position, then set the image-id fg color once for the whole row (it
        // persists across the placeholders that follow).
        write!(symbol, "\x1b[s{id_color}").unwrap();

        // yututui patch: give EVERY placeholder cell its own explicit (row, col) diacritics
        // instead of only the first, with the rest inheriting their column from the left
        // neighbour. Inheriting placeholders break whenever another widget draws over the middle
        // of an image row (e.g. the `eq:`/`radio:` status-line dropdowns): the placeholders to the
        // right of the overdraw lose the neighbour they inherit from and stop rendering, blanking
        // that slice of the image to the terminal background. Explicit per-cell coordinates make
        // each cell self-sufficient, so an overdrawn row keeps painting the image on both sides of
        // the popup. Costs a few extra bytes per cell in the (rarely re-emitted) row escape.
        for x in 0..full_width {
            write!(
                symbol,
                "\u{10EEEE}{}{}{}",
                diacritic(row_y),
                diacritic(x),
                diacritic(*id_extra)
            )
            .unwrap();
        }

        for x in 1..full_width {
            // Skip or something may overwrite it
            if let Some(cell) = buf.cell_mut((area.left() + x, area.top() + y)) {
                cell.set_diff_option(CellDiffOption::Skip);
            }
        }

        symbol.push_str(&restore_cursor);

        if let Some(cell) = buf.cell_mut((area.left(), area.top() + y)) {
            cell.set_symbol(&symbol).set_diff_option(UNIT_WIDTH);
        }
    }
}

fn mark_rows_for_redraw(
    area: Rect,
    size: Size,
    buf: &mut Buffer,
    (_, id_color, id_extra): &(u32, String, u16),
    damage: Rect,
    skip_line_count: usize,
) {
    let width = area.width.min(size.width).min(DIACRITICS.len() as u16);
    let height = area.height.min(size.height).min(DIACRITICS.len() as u16);
    let image = Rect {
        width,
        height,
        ..area
    };
    let overlap = image.intersection(damage);
    if overlap.is_empty() {
        return;
    }

    let anchor_x = image.left();
    for y in overlap.top()..overlap.bottom() {
        if damage.left() <= anchor_x && anchor_x < damage.right() {
            continue;
        }
        let row_y = y - image.top() + skip_line_count as u16;
        let mut symbol = String::with_capacity(id_color.len() + 24);
        write!(
            symbol,
            "\x1b[s{id_color}\u{10EEEE}{}{}{}\x1b[u",
            diacritic(row_y),
            diacritic(0),
            diacritic(*id_extra)
        )
        .unwrap();
        if let Some(cell) = buf.cell_mut((anchor_x, y)) {
            cell.set_symbol(&symbol).set_diff_option(UNIT_WIDTH);
        }
    }
}

/// Create a kitty escape sequence for transmitting and virtual-placement.
///
/// The image will be transmitted as RGBA in chunks of 4096 bytes.
/// A "virtual placement" (U=1) is created so that we can place it using unicode placeholders.
/// Removing the placements when the unicode placeholder is no longer there is being handled
/// automatically by kitty.
fn transmit_virtual(img: &DynamicImage, id: u32, is_tmux: bool, z_index: i32) -> String {
    let (w, h) = (img.width(), img.height());
    let img_rgba8 = img.to_rgba8();
    let bytes = img_rgba8.as_raw();

    let (start, escape, end) = Parser::tmux_start_escape_end(is_tmux);

    // Max chunk size is 4096 bytes of base64 encoded data
    const CHARS_PER_CHUNK: usize = 4096;
    const CHUNK_SIZE: usize = (CHARS_PER_CHUNK / 4) * 3;
    let chunks = bytes.chunks(CHUNK_SIZE);
    let chunk_count = chunks.len();

    // rough estimation for the worst-case size of what'll be written into `data` in the following
    // loop
    const WORST_CASE_ADDITIONAL_CHUNK_0_LEN: usize = 46;
    let bytes_written_per_chunk = 11 + CHARS_PER_CHUNK + (escape.len() * 2);
    let reserve_size =
        (chunk_count * bytes_written_per_chunk) + WORST_CASE_ADDITIONAL_CHUNK_0_LEN + end.len();

    let mut data = String::with_capacity(reserve_size);

    for (i, chunk) in chunks.enumerate() {
        data.push_str(start);
        // tmux seems to only allow a limited amount of data in each passthrough sequence, since
        // we're already chunking the data for the kitty protocol that's a good enough chunk size to
        // use for the passthrough chunks too.
        write!(data, "{escape}_Gq=2,").unwrap();

        if i == 0 {
            write!(data, "i={id},a=T,U=1,f=32,t=d,s={w},v={h},z={z_index},").unwrap();
        }

        // m=0 means over
        let more = u8::from(chunk_count > (i + 1));
        write!(data, "m={more};").unwrap();

        base64_simd::STANDARD.encode_append(chunk, &mut data);

        write!(data, "{escape}\\").unwrap();
        data.push_str(end);
    }

    data
}

/// yututui patch: create a Kitty transmit-and-place command with an explicit physical-cell footprint.
///
/// This path is used only while the terminal grid itself is zoomed. `C=1` keeps the cursor at the
/// ratatui anchor, so the backend's logical-to-physical cursor mapping remains authoritative.
fn transmit_direct(
    img: &DynamicImage,
    id: u32,
    is_tmux: bool,
    z_index: i32,
    size: Size,
    render_scale: RenderScale,
) -> String {
    let (w, h) = (img.width(), img.height());
    let placement = render_scale.placement_size(size);
    let img_rgba8 = img.to_rgba8();
    let bytes = img_rgba8.as_raw();
    let (start, escape, end) = Parser::tmux_start_escape_end(is_tmux);

    const CHARS_PER_CHUNK: usize = 4096;
    const CHUNK_SIZE: usize = (CHARS_PER_CHUNK / 4) * 3;
    let chunks = bytes.chunks(CHUNK_SIZE);
    let chunk_count = chunks.len();
    const WORST_CASE_ADDITIONAL_CHUNK_0_LEN: usize = 72;
    let bytes_written_per_chunk = 11 + CHARS_PER_CHUNK + (escape.len() * 2);
    let reserve_size =
        (chunk_count * bytes_written_per_chunk) + WORST_CASE_ADDITIONAL_CHUNK_0_LEN + end.len();
    let mut data = String::with_capacity(reserve_size);

    for (i, chunk) in chunks.enumerate() {
        data.push_str(start);
        write!(data, "{escape}_Gq=2,").unwrap();
        if i == 0 {
            write!(
                data,
                "i={id},a=T,f=32,t=d,s={w},v={h},c={},r={},C=1,z={z_index},",
                placement.width, placement.height
            )
            .unwrap();
        }

        let more = u8::from(chunk_count > (i + 1));
        write!(data, "m={more};").unwrap();
        base64_simd::STANDARD.encode_append(chunk, &mut data);
        write!(data, "{escape}\\").unwrap();
        data.push_str(end);
    }

    data
}

#[cfg(test)]
mod tests {
    use image::{DynamicImage, ImageBuffer, Rgba};
    use ratatui::{buffer::Buffer, layout::Rect};

    use super::*;

    #[test]
    fn virtual_transmission_places_kitty_image_behind_text_and_backgrounds() {
        let image = DynamicImage::ImageRgba8(ImageBuffer::from_pixel(1, 1, Rgba([0, 0, 0, 0])));
        let seq = transmit_virtual(&image, 42, false, TEXT_BACKGROUND_Z_INDEX);

        assert!(seq.contains(&format!("z={TEXT_BACKGROUND_Z_INDEX},")));
    }

    #[test]
    fn virtual_transmission_can_place_kitty_image_in_foreground() {
        let image = DynamicImage::ImageRgba8(ImageBuffer::from_pixel(1, 1, Rgba([0, 0, 0, 0])));
        let seq = transmit_virtual(&image, 42, false, 0);

        assert!(seq.contains("z=0,"));
    }

    #[test]
    fn scale_one_keeps_the_virtual_transmission_byte_identical() {
        let image = DynamicImage::ImageRgba8(ImageBuffer::from_pixel(2, 1, Rgba([1, 2, 3, 4])));
        let normal = KittyProtoState::new_with_z_index(&image, 42, false, 0);
        let scaled = KittyProtoState::new_with_z_index_and_scale(
            &image,
            42,
            false,
            0,
            Size::new(2, 1),
            RenderScale::Normal,
        );

        assert_eq!(normal.transmit_str, scaled.transmit_str);
        assert!(!scaled.direct);
    }

    #[test]
    fn direct_transmission_places_at_the_scaled_cell_size() {
        let image = DynamicImage::ImageRgba8(ImageBuffer::from_pixel(1, 1, Rgba([0, 0, 0, 0])));
        let seq = transmit_direct(
            &image,
            42,
            false,
            TEXT_BACKGROUND_Z_INDEX,
            Size::new(3, 2),
            RenderScale::Uniform {
                factor: 2,
                double_width_lines: false,
            },
        );

        assert!(seq.contains("a=T,f=32,t=d,s=1,v=1,c=6,r=4,C=1,"));
        assert!(seq.contains(&format!("z={TEXT_BACKGROUND_Z_INDEX},")));
        assert!(!seq.contains("U=1"));
    }

    #[test]
    fn direct_render_uses_one_anchor_and_skips_the_remaining_logical_cells() {
        let area = Rect::new(2, 1, 3, 2);
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 6));
        render_direct(area, area.as_size(), &mut buf, Some("\x1b_Gdirect\x1b\\"));

        let anchor = buf.cell((2, 1)).unwrap();
        assert_eq!(anchor.symbol(), "\x1b_Gdirect\x1b\\");
        assert_eq!(anchor.diff_option, UNIT_WIDTH);
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                if (x, y) != (area.left(), area.top()) {
                    assert_eq!(buf.cell((x, y)).unwrap().diff_option, CellDiffOption::Skip);
                    assert!(!buf.cell((x, y)).unwrap().symbol().contains('\u{10EEEE}'));
                }
            }
        }
    }

    #[test]
    fn interior_popup_damage_marks_anchor_without_retransmitting_image() {
        let area = Rect::new(2, 1, 5, 3);
        let mut buf = Buffer::empty(Rect::new(0, 0, 12, 8));
        let id = id_tuple(42);

        render(area, Size::new(5, 3), &mut buf, &id, None, 0);
        let before = buf.cell((area.left(), 2)).unwrap().symbol().to_owned();
        let untouched_before = buf.cell((area.left(), 1)).unwrap().symbol().to_owned();

        mark_rows_for_redraw(
            area,
            Size::new(5, 3),
            &mut buf,
            &id,
            Rect::new(area.left() + 2, 2, 2, 1),
            0,
        );
        let after = buf.cell((area.left(), 2)).unwrap().symbol().to_owned();

        assert_ne!(before, after);
        assert!(after.contains('\u{10EEEE}'));
        assert!(
            !after.contains("_G"),
            "row marker must not retransmit image pixels"
        );
        assert_eq!(
            buf.cell((area.left(), 1)).unwrap().symbol(),
            untouched_before,
            "uncovered rows are left untouched"
        );
    }

    #[test]
    fn damage_that_already_covers_anchor_needs_no_marker() {
        let area = Rect::new(2, 1, 5, 3);
        let mut buf = Buffer::empty(Rect::new(0, 0, 12, 8));
        let id = id_tuple(42);

        render(area, Size::new(5, 3), &mut buf, &id, None, 0);
        let before = buf.cell((area.left(), 2)).unwrap().symbol().to_owned();

        mark_rows_for_redraw(
            area,
            Size::new(5, 3),
            &mut buf,
            &id,
            Rect::new(area.left(), 2, 2, 1),
            0,
        );

        assert_eq!(buf.cell((area.left(), 2)).unwrap().symbol(), before);
    }

    fn id_tuple(id: u32) -> (u32, String, u16) {
        let [id_extra, id_r, id_g, id_b] = id.to_be_bytes();
        (
            id,
            format!("\x1b[38;2;{id_r};{id_g};{id_b}m"),
            u16::from(id_extra),
        )
    }
}

/// From https://sw.kovidgoyal.net/kitty/_downloads/1792bad15b12979994cd6ecc54c967a6/rowcolumn-diacritics.txt
/// See https://sw.kovidgoyal.net/kitty/graphics-protocol/#unicode-placeholders for further explanation.
static DIACRITICS: [char; 297] = [
    '\u{305}',
    '\u{30D}',
    '\u{30E}',
    '\u{310}',
    '\u{312}',
    '\u{33D}',
    '\u{33E}',
    '\u{33F}',
    '\u{346}',
    '\u{34A}',
    '\u{34B}',
    '\u{34C}',
    '\u{350}',
    '\u{351}',
    '\u{352}',
    '\u{357}',
    '\u{35B}',
    '\u{363}',
    '\u{364}',
    '\u{365}',
    '\u{366}',
    '\u{367}',
    '\u{368}',
    '\u{369}',
    '\u{36A}',
    '\u{36B}',
    '\u{36C}',
    '\u{36D}',
    '\u{36E}',
    '\u{36F}',
    '\u{483}',
    '\u{484}',
    '\u{485}',
    '\u{486}',
    '\u{487}',
    '\u{592}',
    '\u{593}',
    '\u{594}',
    '\u{595}',
    '\u{597}',
    '\u{598}',
    '\u{599}',
    '\u{59C}',
    '\u{59D}',
    '\u{59E}',
    '\u{59F}',
    '\u{5A0}',
    '\u{5A1}',
    '\u{5A8}',
    '\u{5A9}',
    '\u{5AB}',
    '\u{5AC}',
    '\u{5AF}',
    '\u{5C4}',
    '\u{610}',
    '\u{611}',
    '\u{612}',
    '\u{613}',
    '\u{614}',
    '\u{615}',
    '\u{616}',
    '\u{617}',
    '\u{657}',
    '\u{658}',
    '\u{659}',
    '\u{65A}',
    '\u{65B}',
    '\u{65D}',
    '\u{65E}',
    '\u{6D6}',
    '\u{6D7}',
    '\u{6D8}',
    '\u{6D9}',
    '\u{6DA}',
    '\u{6DB}',
    '\u{6DC}',
    '\u{6DF}',
    '\u{6E0}',
    '\u{6E1}',
    '\u{6E2}',
    '\u{6E4}',
    '\u{6E7}',
    '\u{6E8}',
    '\u{6EB}',
    '\u{6EC}',
    '\u{730}',
    '\u{732}',
    '\u{733}',
    '\u{735}',
    '\u{736}',
    '\u{73A}',
    '\u{73D}',
    '\u{73F}',
    '\u{740}',
    '\u{741}',
    '\u{743}',
    '\u{745}',
    '\u{747}',
    '\u{749}',
    '\u{74A}',
    '\u{7EB}',
    '\u{7EC}',
    '\u{7ED}',
    '\u{7EE}',
    '\u{7EF}',
    '\u{7F0}',
    '\u{7F1}',
    '\u{7F3}',
    '\u{816}',
    '\u{817}',
    '\u{818}',
    '\u{819}',
    '\u{81B}',
    '\u{81C}',
    '\u{81D}',
    '\u{81E}',
    '\u{81F}',
    '\u{820}',
    '\u{821}',
    '\u{822}',
    '\u{823}',
    '\u{825}',
    '\u{826}',
    '\u{827}',
    '\u{829}',
    '\u{82A}',
    '\u{82B}',
    '\u{82C}',
    '\u{82D}',
    '\u{951}',
    '\u{953}',
    '\u{954}',
    '\u{F82}',
    '\u{F83}',
    '\u{F86}',
    '\u{F87}',
    '\u{135D}',
    '\u{135E}',
    '\u{135F}',
    '\u{17DD}',
    '\u{193A}',
    '\u{1A17}',
    '\u{1A75}',
    '\u{1A76}',
    '\u{1A77}',
    '\u{1A78}',
    '\u{1A79}',
    '\u{1A7A}',
    '\u{1A7B}',
    '\u{1A7C}',
    '\u{1B6B}',
    '\u{1B6D}',
    '\u{1B6E}',
    '\u{1B6F}',
    '\u{1B70}',
    '\u{1B71}',
    '\u{1B72}',
    '\u{1B73}',
    '\u{1CD0}',
    '\u{1CD1}',
    '\u{1CD2}',
    '\u{1CDA}',
    '\u{1CDB}',
    '\u{1CE0}',
    '\u{1DC0}',
    '\u{1DC1}',
    '\u{1DC3}',
    '\u{1DC4}',
    '\u{1DC5}',
    '\u{1DC6}',
    '\u{1DC7}',
    '\u{1DC8}',
    '\u{1DC9}',
    '\u{1DCB}',
    '\u{1DCC}',
    '\u{1DD1}',
    '\u{1DD2}',
    '\u{1DD3}',
    '\u{1DD4}',
    '\u{1DD5}',
    '\u{1DD6}',
    '\u{1DD7}',
    '\u{1DD8}',
    '\u{1DD9}',
    '\u{1DDA}',
    '\u{1DDB}',
    '\u{1DDC}',
    '\u{1DDD}',
    '\u{1DDE}',
    '\u{1DDF}',
    '\u{1DE0}',
    '\u{1DE1}',
    '\u{1DE2}',
    '\u{1DE3}',
    '\u{1DE4}',
    '\u{1DE5}',
    '\u{1DE6}',
    '\u{1DFE}',
    '\u{20D0}',
    '\u{20D1}',
    '\u{20D4}',
    '\u{20D5}',
    '\u{20D6}',
    '\u{20D7}',
    '\u{20DB}',
    '\u{20DC}',
    '\u{20E1}',
    '\u{20E7}',
    '\u{20E9}',
    '\u{20F0}',
    '\u{2CEF}',
    '\u{2CF0}',
    '\u{2CF1}',
    '\u{2DE0}',
    '\u{2DE1}',
    '\u{2DE2}',
    '\u{2DE3}',
    '\u{2DE4}',
    '\u{2DE5}',
    '\u{2DE6}',
    '\u{2DE7}',
    '\u{2DE8}',
    '\u{2DE9}',
    '\u{2DEA}',
    '\u{2DEB}',
    '\u{2DEC}',
    '\u{2DED}',
    '\u{2DEE}',
    '\u{2DEF}',
    '\u{2DF0}',
    '\u{2DF1}',
    '\u{2DF2}',
    '\u{2DF3}',
    '\u{2DF4}',
    '\u{2DF5}',
    '\u{2DF6}',
    '\u{2DF7}',
    '\u{2DF8}',
    '\u{2DF9}',
    '\u{2DFA}',
    '\u{2DFB}',
    '\u{2DFC}',
    '\u{2DFD}',
    '\u{2DFE}',
    '\u{2DFF}',
    '\u{A66F}',
    '\u{A67C}',
    '\u{A67D}',
    '\u{A6F0}',
    '\u{A6F1}',
    '\u{A8E0}',
    '\u{A8E1}',
    '\u{A8E2}',
    '\u{A8E3}',
    '\u{A8E4}',
    '\u{A8E5}',
    '\u{A8E6}',
    '\u{A8E7}',
    '\u{A8E8}',
    '\u{A8E9}',
    '\u{A8EA}',
    '\u{A8EB}',
    '\u{A8EC}',
    '\u{A8ED}',
    '\u{A8EE}',
    '\u{A8EF}',
    '\u{A8F0}',
    '\u{A8F1}',
    '\u{AAB0}',
    '\u{AAB2}',
    '\u{AAB3}',
    '\u{AAB7}',
    '\u{AAB8}',
    '\u{AABE}',
    '\u{AABF}',
    '\u{AAC1}',
    '\u{FE20}',
    '\u{FE21}',
    '\u{FE22}',
    '\u{FE23}',
    '\u{FE24}',
    '\u{FE25}',
    '\u{FE26}',
    '\u{10A0F}',
    '\u{10A38}',
    '\u{1D185}',
    '\u{1D186}',
    '\u{1D187}',
    '\u{1D188}',
    '\u{1D189}',
    '\u{1D1AA}',
    '\u{1D1AB}',
    '\u{1D1AC}',
    '\u{1D1AD}',
    '\u{1D242}',
    '\u{1D243}',
    '\u{1D244}',
];

#[inline]
fn diacritic(y: u16) -> char {
    *DIACRITICS
        .get(usize::from(y))
        .unwrap_or_else(|| &DIACRITICS[0])
}
