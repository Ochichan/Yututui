//! Sixel protocol implementations.
//! Uses [`icy_sixel`] to draw image pixels, if the terminal [supports] the [Sixel] protocol.
//!
//! Delivers the image on each render as [Sixel]s.
//!
//! [`icy_sixel`]: https://github.com/mkrueger/icy_sixel
//! [supports]: https://arewesixelyet.com
//! [Sixel]: https://en.wikipedia.org/wiki/Sixel
use icy_sixel::{EncodeOptions, sixel_encode};
use image::DynamicImage;
use ratatui::{
    buffer::{Buffer, CellDiffOption},
    layout::{Position, Rect, Size},
};

use super::{ProtocolTrait, StatefulProtocolTrait, clear_area};
use crate::{
    RenderScale, Result, errors::Errors, picker::cap_parser::Parser, protocol::UNIT_WIDTH,
};

#[derive(Clone, Default)]
pub struct Sixel {
    pub data: String,
    pub size: Size,
    pub is_tmux: bool,
    /// yututui patch: per-encode anchor-cell tag so a freshly built protocol re-emits once. See
    /// [`crate::protocol::next_redraw_tag`].
    pub redraw_tag: u32,
}

impl Sixel {
    pub fn new(image: DynamicImage, size: Size, is_tmux: bool) -> Result<Self> {
        let data = encode(&image, size, is_tmux, RenderScale::Normal)?;
        Ok(Self {
            data,
            size,
            is_tmux,
            redraw_tag: super::next_redraw_tag(),
        })
    }
}

// TODO: change E to sixel_rs::status::Error and map when calling
fn encode(
    img: &DynamicImage,
    size: Size,
    is_tmux: bool,
    render_scale: RenderScale,
) -> Result<String> {
    let (w, h) = (img.width(), img.height());
    let img_rgba8 = img.to_rgba8();
    let bytes = img_rgba8.as_raw();
    let (start, escape, end) = Parser::tmux_start_escape_end(is_tmux);

    // yututui patch: a DECDHL raster spans twice the physical rows, while CSI X still counts
    // logical columns on the double-width line.
    let clear_size = render_scale.clear_size(size);
    let width = clear_size.width;
    let height = clear_size.height;

    let sixel_data = sixel_encode(bytes, w as usize, h as usize, &EncodeOptions::default())
        .map_err(|err| Errors::Sixel(format!("sixel encoding error: {err}")))?;

    let mut data = String::new();
    if is_tmux {
        if !sixel_data.starts_with('\x1b') {
            return Err(Errors::Tmux("sixel string did not start with escape"));
        }
        // The clear sequence must be inside the tmux passthrough since it uses
        // doubled escapes.
        data.push_str(start);
        clear_area(&mut data, escape, width, height);
        data.push_str(escape);
        data.push_str(&sixel_data[1..]);
        data.push_str(end);
    } else {
        clear_area(&mut data, escape, width, height);
        data.push_str(&sixel_data);
    }

    Ok(data)
}

impl ProtocolTrait for Sixel {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if self.size.width > area.width || self.size.height > area.height {
            return;
        }
        let render_area = Rect::new(area.x, area.y, self.size.width, self.size.height);

        render(&self.data, render_area, buf);

        // yututui patch: stamp the anchor cell's (invisible) foreground with this protocol's
        // redraw tag so a freshly built protocol differs from the displayed frame and ratatui's
        // diff re-flushes the whole sixel exactly once — wiping any popup residue. See
        // `crate::protocol::next_redraw_tag`.
        if let Some(cell) = buf.cell_mut(Into::<Position>::into(render_area)) {
            cell.set_fg(super::redraw_tag_color(self.redraw_tag));
        }
    }

    fn size(&self) -> Size {
        self.size
    }
}

pub(crate) fn render(data: &str, area: Rect, buf: &mut Buffer) {
    buf.cell_mut(Into::<Position>::into(area))
        .map(|cell| cell.set_symbol(data).set_diff_option(UNIT_WIDTH));

    let mut skip_first = false;

    // Skip entire area
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if !skip_first {
                skip_first = true;
                continue;
            }
            buf.cell_mut((x, y))
                .map(|cell| cell.set_diff_option(CellDiffOption::Skip));
        }
    }
}

impl StatefulProtocolTrait for Sixel {
    fn resize_encode(&mut self, img: DynamicImage, size: Size) -> Result<()> {
        self.resize_encode_scaled(img, size, RenderScale::Normal)
    }

    fn resize_encode_scaled(
        &mut self,
        img: DynamicImage,
        size: Size,
        render_scale: RenderScale,
    ) -> Result<()> {
        // yututui patch: the outer state has already resized pixels with scaled font metrics;
        // carry the scale here for DECDHL-aware clear geometry.
        let data = encode(&img, size, self.is_tmux, render_scale)?;
        *self = Sixel {
            data,
            size,
            // yututui patch: a re-encode (resize, or a rebuilt protocol) gets a fresh tag so the
            // next render re-flushes the anchor cell exactly once.
            redraw_tag: super::next_redraw_tag(),
            ..*self
        };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// yututui patch regression: two protocols built from the *same* image must stamp DIFFERENT
    /// anchor-cell foregrounds. That's what lets ratatui's frame diff re-flush the whole sixel
    /// when `App::refresh_art` rebuilds the protocol after a popup closes — without it the anchor
    /// cell is byte-identical, the diff skips it, and the art ghosts (Sixel on Windows Terminal).
    #[test]
    fn rebuilt_protocol_perturbs_anchor_cell_so_the_diff_reemits() {
        let img = image::DynamicImage::new_rgb8(2, 2);
        let size = Size::new(1, 1);
        let area = Rect::new(0, 0, 1, 1);

        let a = Sixel::new(img.clone(), size, false).unwrap();
        let b = Sixel::new(img, size, false).unwrap();
        assert_ne!(
            a.redraw_tag, b.redraw_tag,
            "each encode must take a unique tag"
        );

        let mut buf_a = Buffer::empty(area);
        let mut buf_b = Buffer::empty(area);
        a.render(area, &mut buf_a);
        b.render(area, &mut buf_b);

        let cell_a = buf_a.cell((0, 0)).unwrap();
        let cell_b = buf_b.cell((0, 0)).unwrap();
        // Same image => identical escape in the symbol, but the (invisible) fg differs, so the
        // cells are unequal and the frame diff emits the second one over the first.
        assert_eq!(
            cell_a.symbol(),
            cell_b.symbol(),
            "same image keeps the same escape"
        );
        assert_ne!(
            cell_a.fg, cell_b.fg,
            "a rebuilt protocol must perturb the anchor fg"
        );
        assert_ne!(
            cell_a, cell_b,
            "anchor cells must differ so ratatui re-emits the sixel"
        );
    }

    #[test]
    fn double_width_scale_clears_logical_columns_across_physical_rows() {
        let img = image::DynamicImage::new_rgb8(2, 2);
        let data = encode(
            &img,
            Size::new(3, 2),
            false,
            RenderScale::Uniform {
                factor: 2,
                double_width_lines: true,
            },
        )
        .unwrap();

        assert!(
            data.starts_with("\x1b[3X\x1b[1B\x1b[3X\x1b[1B\x1b[3X\x1b[1B\x1b[3X\x1b[1B\x1b[4A")
        );
        assert!(!data.starts_with("\x1b[6X"));
    }
}
