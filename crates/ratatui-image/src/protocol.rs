//! Protocol backends for the widgets

use std::{
    fmt::Write,
    num::NonZeroU16,
    sync::Arc,
    sync::atomic::{AtomicU32, Ordering},
};

use image::{DynamicImage, ImageBuffer, Rgba, imageops};
use ratatui::{
    buffer::{Buffer, CellDiffOption},
    layout::{Rect, Size},
    style::Color,
};

use self::{
    halfblocks::Halfblocks,
    iterm2::Iterm2,
    kitty::{Kitty, StatefulKitty},
    sixel::Sixel,
};
use crate::{FontSize, ResizeEncodeRender, Result};

use super::Resize;

pub mod halfblocks;
pub mod iterm2;
pub mod kitty;
pub mod sixel;

const UNIT_WIDTH: CellDiffOption = CellDiffOption::ForcedWidth(NonZeroU16::new(1).unwrap());

/// yututui patch: a process-wide monotonic counter, bumped once per Sixel/iTerm2 (re-)encode.
///
/// The Sixel and iTerm2 protocols deliver the whole image as a single escape parked in the render
/// area's top-left ("anchor") cell, with every other covered cell marked [`CellDiffOption::Skip`].
/// A popup (`eq:`/`radio:` dropdown, queue window, About card) that overdraws part of the art and
/// then closes leaves those `Skip` cells blank — only re-transmitting the anchor escape (whose
/// `clear_area` prefix wipes the residue) repaints it. But ratatui's frame diff skips the anchor
/// when its cell is byte-identical to the displayed frame, and re-encoding the same image yields
/// the same bytes — so the Kitty-style "mint a new graphics id" trick (`App::refresh_art`) is a
/// no-op here and the art ghosts (notably under Sixel on Windows Terminal).
///
/// Stamping this tag into the anchor cell's foreground (invisible — the image pixels paint over
/// the cell) makes a freshly built protocol differ from the displayed frame, so the diff
/// re-flushes the anchor exactly once. Steady-state renders reuse the same protocol and tag, so
/// the cell stays identical and the diff skips it: no per-frame re-transmit, no flicker.
static REDRAW_TAG: AtomicU32 = AtomicU32::new(1);

/// Next anchor-cell redraw tag (see [`REDRAW_TAG`]). Bumped at each encode so consecutive encodes
/// of the same image still differ.
pub(crate) fn next_redraw_tag() -> u32 {
    REDRAW_TAG.fetch_add(1, Ordering::Relaxed)
}

/// Map a [`next_redraw_tag`] value to the anchor cell's foreground colour. The image pixels cover
/// the cell, so this colour never shows; it exists only to perturb the cell for the frame diff.
pub(crate) fn redraw_tag_color(tag: u32) -> Color {
    let [_, r, g, b] = tag.to_be_bytes();
    Color::Rgb(r, g, b)
}

pub(crate) trait ProtocolTrait: Send + Sync {
    /// Render the currently resized and encoded data to the buffer.
    fn render(&self, area: Rect, buf: &mut Buffer);

    // Get the size of the image.
    fn size(&self) -> Size;
}

trait StatefulProtocolTrait: ProtocolTrait {
    /// Resize the image and encode it for rendering. The result should be stored statefully so
    /// that next call for the given area does not need to redo the work.
    ///
    /// This can be done in a background thread, and the result is stored in this [StatefulProtocol].
    fn resize_encode(&mut self, img: DynamicImage, size: Size) -> Result<()>;
}

/// A fixed-size image protocol for the [crate::Image] widget.
#[derive(Clone)]
pub enum Protocol {
    Halfblocks(Halfblocks),
    Sixel(Sixel),
    Kitty(Kitty),
    ITerm2(Iterm2),
}

impl Protocol {
    pub(crate) fn render(&self, area: Rect, buf: &mut Buffer) {
        let inner: &dyn ProtocolTrait = match self {
            Self::Halfblocks(halfblocks) => halfblocks,
            Self::Sixel(sixel) => sixel,
            Self::Kitty(kitty) => kitty,
            Self::ITerm2(iterm2) => iterm2,
        };
        inner.render(area, buf);
    }
    // Get the size of the image.
    pub fn size(&self) -> Size {
        let inner: &dyn ProtocolTrait = match self {
            Self::Halfblocks(halfblocks) => halfblocks,
            Self::Sixel(sixel) => sixel,
            Self::Kitty(kitty) => kitty,
            Self::ITerm2(iterm2) => iterm2,
        };
        inner.size()
    }

    /// Returns a placeholder area, if the image will not render into the given area, or `None`.
    ///
    /// The returned [`ratatui::layout::Rect`] is the area the image would cover, constrained by
    /// the size of `area` argument, if the image does not fit.
    ///
    /// Kitty and Halfblocks can always render partially, so they always return `None`.
    pub fn needs_placeholder(&self, area: Rect) -> Option<Rect> {
        let image_size = self.size();
        if area.width < image_size.width
            || area.height < image_size.height
                && (matches!(self, Self::Sixel(_)) || matches!(self, Self::Halfblocks(_)))
        {
            let mut placeholder_area = area;
            placeholder_area.width = placeholder_area.width.min(image_size.width);
            placeholder_area.height = placeholder_area.height.min(image_size.height);
            return Some(placeholder_area);
        }
        // Kitty and Halfblocks can render into a smaller area.
        None
    }
}

/// A stateful resizing image protocol for the [crate::StatefulImage] widget.
///
/// The [crate::thread::ThreadProtocol] widget also uses this, and is the reason why resizing is
/// split from rendering.
pub struct StatefulProtocol {
    source: ImageSource,
    font_size: FontSize,
    hash: u64,
    protocol_type: StatefulProtocolType,
    last_encoding_result: Option<Result<()>>,
}

#[derive(Clone)]
pub enum StatefulProtocolType {
    Halfblocks(Halfblocks),
    Sixel(Sixel),
    Kitty(StatefulKitty),
    ITerm2(Iterm2),
}

impl StatefulProtocolType {
    fn inner_trait(&self) -> &dyn StatefulProtocolTrait {
        match self {
            Self::Halfblocks(halfblocks) => halfblocks,
            Self::Sixel(sixel) => sixel,
            Self::Kitty(kitty) => kitty,
            Self::ITerm2(iterm2) => iterm2,
        }
    }
    fn inner_trait_mut(&mut self) -> &mut dyn StatefulProtocolTrait {
        match self {
            Self::Halfblocks(halfblocks) => halfblocks,
            Self::Sixel(sixel) => sixel,
            Self::Kitty(kitty) => kitty,
            Self::ITerm2(iterm2) => iterm2,
        }
    }
}

impl StatefulProtocol {
    pub fn new(
        image: DynamicImage,
        font_size: FontSize,
        background_color: Option<Rgba<u8>>,
        protocol_type: StatefulProtocolType,
    ) -> Self {
        Self::new_shared(Arc::new(image), font_size, background_color, protocol_type)
    }

    /// Construct a protocol that shares its immutable decoded source pixels with its owner.
    /// Resized protocol buffers remain independently owned; only the full-resolution source is
    /// shared, avoiding a multi-megabyte deep clone for every held album cover.
    pub fn new_shared(
        image: Arc<DynamicImage>,
        font_size: FontSize,
        background_color: Option<Rgba<u8>>,
        protocol_type: StatefulProtocolType,
    ) -> Self {
        let source = ImageSource::new_shared(image, font_size, background_color);
        Self {
            source,
            font_size,
            hash: u64::default(),
            protocol_type,
            last_encoding_result: None,
        }
    }

    // Calculate the area that this image will ultimately render to, inside the given area.
    pub fn size_for(&self, resize: Resize, size: Size) -> Size {
        resize.size_for(&self.source.image, self.font_size, size)
    }

    pub fn protocol_type(&self) -> &StatefulProtocolType {
        &self.protocol_type
    }

    pub fn protocol_type_owned(self) -> StatefulProtocolType {
        self.protocol_type
    }

    pub fn mark_kitty_rows_for_redraw(&self, area: Rect, damage: Rect, buf: &mut Buffer) {
        if let StatefulProtocolType::Kitty(kitty) = &self.protocol_type {
            kitty.mark_rows_for_redraw(area, damage, buf);
        }
    }

    /// This returns the latest Result returned when encoding, and none if there was no encoding since the last result read. It is encouraged but not required to handle it
    pub fn last_encoding_result(&mut self) -> Option<Result<()>> {
        self.last_encoding_result.take()
    }

    // Get the background color that fills in when resizing.
    pub fn background_color(&self) -> Option<Rgba<u8>> {
        self.source.background_color
    }

    fn last_encoding_area(&self) -> Size {
        self.protocol_type.inner_trait().size()
    }
}

impl ResizeEncodeRender for StatefulProtocol {
    fn resize_encode(&mut self, resize: &Resize, size: Size) {
        if size.width == 0 || size.height == 0 {
            return;
        }

        let img = resize.resize(
            &self.source.image,
            self.font_size,
            size,
            self.background_color(),
        );

        // TODO: save err in struct
        let result = self
            .protocol_type
            .inner_trait_mut()
            .resize_encode(img, size);

        if result.is_ok() {
            self.hash = self.source.hash
        }

        self.last_encoding_result = Some(result)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        self.protocol_type.inner_trait_mut().render(area, buf);
    }

    fn needs_resize(&self, resize: &Resize, size: Size) -> Option<Size> {
        resize.needs_resize(
            &self.source.image,
            Some(self.source.desired),
            self.font_size,
            Some(self.last_encoding_area()),
            size,
            self.source.hash != self.hash,
        )
    }
}

#[derive(Clone)]
/// Image source for [crate::protocol::StatefulProtocol]s
///
/// A `[StatefulProtocol]` needs to resize the ImageSource to its state when the available area
/// changes. A `[Protocol]` only needs it once.
///
/// # Examples
/// ```text
/// use image::{DynamicImage, ImageBuffer, Rgb};
/// use ratatui_image::ImageSource;
///
/// let image: ImageBuffer::from_pixel(300, 200, Rgb::<u8>([255, 0, 0])).into();
/// let source = ImageSource::new(image, "filename.png", (7, 14));
/// assert_eq!((43, 14), (source.rect.width, source.rect.height));
/// ```
///
struct ImageSource {
    /// The original image without resizing.
    pub image: Arc<DynamicImage>,
    /// The area that the [`ImageSource::image`] covers, but not necessarily fills.
    pub desired: Size,
    /// TODO: document this; when image changes but it doesn't need a resize, force a render.
    pub hash: u64,
    /// The background color that should be used for padding or background when resizing.
    pub background_color: Option<Rgba<u8>>,
}

impl ImageSource {
    /// Create an image source from shared decoded pixels. A non-transparent background requires
    /// a composited copy (the source pixels would otherwise be mutated); the usual transparent
    /// album-art path keeps the exact same allocation.
    pub fn new_shared(
        image: Arc<DynamicImage>,
        font_size: FontSize,
        background_color: Option<Rgba<u8>>,
    ) -> ImageSource {
        let desired = Resize::round_pixel_size_to_cells(image.width(), image.height(), font_size);

        // We only need to underlay the background color here if it's not completely transparent.
        let image = if let Some(background_color) = background_color
            && background_color.0[3] != 0
        {
            let mut bg: DynamicImage =
                ImageBuffer::from_pixel(image.width(), image.height(), background_color).into();
            imageops::overlay(&mut bg, image.as_ref(), 0, 0);
            Arc::new(bg)
        } else {
            image
        };

        ImageSource {
            image,
            desired,
            // The source is immutable for the lifetime of a StatefulProtocol. A nonzero marker
            // is sufficient to force the first encode, after which `self.hash` matches it.
            hash: 1,
            background_color,
        }
    }
}

#[cfg(test)]
mod shared_source_tests {
    use super::*;

    fn patterned_image() -> DynamicImage {
        let mut image = ImageBuffer::new(7, 5);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            *pixel = Rgba([
                (x * 31 + y * 7) as u8,
                (x * 11 + y * 37) as u8,
                (x * 19 + y * 13) as u8,
                if (x + y).is_multiple_of(3) { 96 } else { 255 },
            ]);
        }
        DynamicImage::ImageRgba8(image)
    }

    fn halfblocks() -> StatefulProtocolType {
        StatefulProtocolType::Halfblocks(Halfblocks::default())
    }

    #[test]
    fn transparent_shared_source_reuses_the_exact_pixel_allocation() {
        let image = Arc::new(DynamicImage::new_rgba8(64, 64));
        let protocol = StatefulProtocol::new_shared(
            Arc::clone(&image),
            FontSize::new(10, 20),
            None,
            StatefulProtocolType::Halfblocks(Halfblocks::default()),
        );

        assert!(Arc::ptr_eq(&image, &protocol.source.image));
        assert_eq!(Arc::strong_count(&image), 2);
    }

    #[test]
    fn opaque_background_keeps_compositing_semantics_without_mutating_shared_input() {
        let image = Arc::new(DynamicImage::new_rgba8(2, 2));
        let protocol = StatefulProtocol::new_shared(
            Arc::clone(&image),
            FontSize::new(10, 20),
            Some(Rgba([20, 30, 40, 255])),
            StatefulProtocolType::Halfblocks(Halfblocks::default()),
        );

        assert!(!Arc::ptr_eq(&image, &protocol.source.image));
        assert_eq!(image.to_rgba8().get_pixel(0, 0).0, [0, 0, 0, 0]);
        assert_eq!(
            protocol.source.image.to_rgba8().get_pixel(0, 0).0,
            [20, 30, 40, 255]
        );
    }

    #[test]
    fn owned_and_shared_constructors_encode_identical_pixels_at_multiple_sizes() {
        let image = patterned_image();
        for background in [None, Some(Rgba([20, 30, 40, 255]))] {
            for area in [Rect::new(0, 0, 5, 3), Rect::new(0, 0, 9, 4)] {
                let mut owned = StatefulProtocol::new(
                    image.clone(),
                    FontSize::new(10, 20),
                    background,
                    halfblocks(),
                );
                let mut shared = StatefulProtocol::new_shared(
                    Arc::new(image.clone()),
                    FontSize::new(10, 20),
                    background,
                    halfblocks(),
                );
                let mut owned_buffer = Buffer::empty(area);
                let mut shared_buffer = Buffer::empty(area);

                owned.resize_encode_render(&Resize::Fit(None), area, &mut owned_buffer);
                shared.resize_encode_render(&Resize::Fit(None), area, &mut shared_buffer);

                assert_eq!(owned_buffer, shared_buffer);
                assert_eq!(owned.last_encoding_area(), shared.last_encoding_area());
                assert!(owned.last_encoding_result().unwrap().is_ok());
                assert!(shared.last_encoding_result().unwrap().is_ok());
            }
        }
    }

    #[test]
    fn shared_source_and_protocol_remain_send_sync_for_resize_worker_handoff() {
        fn assert_send_sync<T: Send + Sync>() {}

        assert_send_sync::<Arc<DynamicImage>>();
        assert_send_sync::<StatefulProtocol>();
    }
}

// Transparency needs explicit erasing of stale characters, or they stay behind the rendered
// image due to skipping of the following characters _in the terminal buffer_.
// DECERA does not work in WezTerm, however ECH and and cursor CUD and CUU do.
// For each line, erase `width` characters, then move back and place image.
pub(crate) fn clear_area(data: &mut String, escape: &str, width: u16, height: u16) {
    if height == 1 {
        // If the image is a single row then we don't need to move the cursor around at all.
        write!(data, "{escape}[{width}X").unwrap();
    } else {
        for _ in 0..height {
            write!(data, "{escape}[{width}X{escape}[1B").unwrap();
        }
        write!(data, "{escape}[{height}A").unwrap();
    }
}
