//! Widget that separates resize+encode from rendering.
//! This allows for rendering to be non-blocking, offloading resize+encode into another thread.
//! See examples/thread.rs and examples/tokio.rs for how to setup the threads and channels.
//! At least one worker thread for resize+encode is required, the example shows how to combine
//! the needs-resize-polling with other terminal events into one event loop.

#[cfg(not(feature = "tokio"))]
use std::sync::mpsc::Sender;
#[cfg(feature = "tokio")]
use tokio::sync::mpsc::UnboundedSender as Sender;

use image::Rgba;
use ratatui::{
    layout::Size,
    prelude::{Buffer, Rect},
};

use crate::{
    Resize, ResizeEncodeRender,
    errors::Errors,
    protocol::{StatefulProtocol, StatefulProtocolType},
};

/// The only usage of this struct is to call `perform()` on it and pass the completed resize to `ThreadProtocols` `update_protocol()`
pub struct ResizeRequest {
    protocol: StatefulProtocol,
    resize: Resize,
    size: Size,
    id: u64,
}

impl ResizeRequest {
    pub fn resize_encode(mut self) -> Result<ResizeResponse, Errors> {
        self.protocol.resize_encode(&self.resize, self.size);
        self.protocol
            .last_encoding_result()
            .expect("The resize has just been performed")?;
        Ok(ResizeResponse {
            protocol: self.protocol,
            id: self.id,
        })
    }
}

/// The only usage of this struct is to pass it to `ThreadProtocols` `update_resize_protocol()`
pub struct ResizeResponse {
    protocol: StatefulProtocol,
    id: u64,
}

/// The state for a threaded [`crate::StatefulImage`].
///
/// Has `inner` [StatefulProtocol] and sents requests through the mspc channel to do the
/// `resize_encode()` work.
pub struct ThreadProtocol {
    inner: Option<StatefulProtocol>,
    tx: Sender<ResizeRequest>,
    id: u64,
    pending_id: Option<u64>,
    last_resize: Option<Resize>,
    last_area: Option<Size>,
}

impl ThreadProtocol {
    pub fn new(tx: Sender<ResizeRequest>, inner: Option<StatefulProtocol>) -> ThreadProtocol {
        Self {
            inner,
            tx,
            id: 0,
            pending_id: None,
            last_resize: None,
            last_area: None,
        }
    }

    pub fn replace_protocol(&mut self, proto: StatefulProtocol) {
        self.inner = Some(proto);
        self.pending_id = None;
        self.increment_id();
    }

    /// Replace the protocol without blanking the currently rendered image. If this widget has
    /// already been rendered, the new protocol is encoded for that last area in the background and
    /// swapped in when complete. Until then, the old protocol continues to render, avoiding the
    /// one-frame hole that a popup/art refresh would otherwise create.
    pub fn refresh_protocol(&mut self, proto: StatefulProtocol) {
        let Some(resize) = self.last_resize.clone() else {
            self.replace_protocol(proto);
            return;
        };
        let Some(area) = self.last_area else {
            self.replace_protocol(proto);
            return;
        };
        let size = proto.size_for(resize.clone(), area);
        self.increment_id();
        self.pending_id = Some(self.id);
        let request = ResizeRequest {
            protocol: proto,
            resize,
            size,
            id: self.id,
        };
        if let Err(err) = self.tx.send(request) {
            self.inner = Some(err.0.protocol);
            self.pending_id = None;
        }
    }

    pub fn protocol_type(&self) -> Option<&StatefulProtocolType> {
        self.inner.as_ref().map(|inner| inner.protocol_type())
    }

    pub fn protocol_type_owned(self) -> Option<StatefulProtocolType> {
        self.inner.map(|inner| inner.protocol_type_owned())
    }

    // Get the background color that fills in when resizing.
    pub fn background_color(&self) -> Option<Rgba<u8>> {
        self.inner
            .as_ref()
            .and_then(|inner| inner.background_color())
    }

    /// This function should be used when an image should be updated but the updated image is not yet available
    pub fn empty_protocol(&mut self) {
        self.inner = None;
        self.pending_id = None;
        self.increment_id();
    }

    pub fn update_resized_protocol(&mut self, completed: ResizeResponse) -> bool {
        let equal = self.id == completed.id && self.pending_id == Some(completed.id);
        if equal {
            self.inner = Some(completed.protocol);
            self.pending_id = None;
        }
        equal
    }

    pub fn size_for(&self, resize: Resize, size: Size) -> Option<Size> {
        self.inner
            .as_ref()
            .map(|protocol| protocol.size_for(resize, size))
    }

    fn increment_id(&mut self) {
        self.id = self.id.wrapping_add(1);
    }
}

impl ResizeEncodeRender for ThreadProtocol {
    fn resize_encode_render(&mut self, resize: &Resize, area: Rect, buf: &mut Buffer) {
        self.last_resize = Some(resize.clone());
        self.last_area = Some(area.into());
        if let Some(rect) = self.needs_resize(resize, area.into()) {
            self.resize_encode(resize, rect);
        }
        self.render(area, buf);
    }

    fn needs_resize(&self, resize: &Resize, size: Size) -> Option<Size> {
        if self.pending_id.is_some() {
            return None;
        }
        self.inner
            .as_ref()
            .and_then(|protocol| protocol.needs_resize(resize, size))
    }

    /// Senda a `ResizeRequest` through the channel if there already isn't a pending `ResizeRequest`
    fn resize_encode(&mut self, resize: &Resize, size: Size) {
        let _ = self.inner.take().map(|protocol| {
            self.increment_id();
            self.pending_id = Some(self.id);
            let request = ResizeRequest {
                protocol,
                resize: resize.clone(),
                size,
                id: self.id,
            };
            if let Err(err) = self.tx.send(request) {
                self.inner = Some(err.0.protocol);
                self.pending_id = None;
            }
        });
    }

    /// Render the currently resized and encoded data to the buffer, if there isn't a pending `ResizeRequest`
    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let _ = self
            .inner
            .as_mut()
            .map(|protocol| protocol.render(area, buf));
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{buffer::Buffer, layout::Rect};

    use super::*;
    use crate::picker::Picker;

    #[test]
    fn refresh_protocol_keeps_current_image_until_replacement_is_encoded() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let picker = Picker::halfblocks();
        let img = image::DynamicImage::new_rgb8(8, 8);
        let area = Rect::new(0, 0, 4, 4);
        let resize = Resize::Scale(None);
        let mut threaded =
            ThreadProtocol::new(tx, Some(picker.new_resize_protocol(img.clone())));

        let mut buf = Buffer::empty(area);
        threaded.resize_encode_render(&resize, area, &mut buf);
        let response = rx
            .try_recv()
            .expect("first render queues initial resize")
            .resize_encode()
            .unwrap();
        assert!(threaded.update_resized_protocol(response));

        let mut buf = Buffer::empty(area);
        threaded.resize_encode_render(&resize, area, &mut buf);
        assert!(
            rx.try_recv().is_err(),
            "matching area should not queue another resize"
        );
        assert!(threaded.protocol_type().is_some());

        threaded.refresh_protocol(picker.new_resize_protocol(img));
        assert!(
            threaded.protocol_type().is_some(),
            "old encoded image stays renderable while fresh protocol is pending"
        );
        rx.try_recv()
            .expect("fresh protocol is encoded in the background");
    }
}
