//! Widget that separates resize+encode from rendering.
//! This allows for rendering to be non-blocking, offloading resize+encode into another thread.
//! See examples/thread.rs and examples/tokio.rs for how to setup the threads and channels.
//! At least one worker thread for resize+encode is required, the example shows how to combine
//! the needs-resize-polling with other terminal events into one event loop.

use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(not(feature = "tokio"))]
use std::sync::mpsc::Sender;
#[cfg(feature = "tokio")]
use tokio::sync::mpsc::{Sender, error::TrySendError};

use image::Rgba;
use ratatui::{
    layout::Size,
    prelude::{Buffer, Rect},
};

use crate::{
    RenderScale, Resize, ResizeEncodeRender,
    errors::Errors,
    protocol::{StatefulProtocol, StatefulProtocolType},
};

/// yututui patch: process-global request identity. A response can outlive the [`ThreadProtocol`] that sent it
/// (for example when a track changes while the serial image worker is encoding). Per-instance
/// counters can collide in that case, so every handoff receives a process-unique generation.
static NEXT_RESIZE_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

fn next_resize_request_id() -> u64 {
    NEXT_RESIZE_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
}

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
    pending_id: Option<u64>,
    render_scale: RenderScale,
}

impl ThreadProtocol {
    pub fn new(tx: Sender<ResizeRequest>, inner: Option<StatefulProtocol>) -> ThreadProtocol {
        Self {
            inner,
            tx,
            pending_id: None,
            render_scale: RenderScale::Normal,
        }
    }

    pub fn replace_protocol(&mut self, mut proto: StatefulProtocol) {
        proto.set_render_scale(self.render_scale);
        self.inner = Some(proto);
        self.pending_id = None;
    }

    pub fn protocol_type(&self) -> Option<&StatefulProtocolType> {
        self.inner.as_ref().map(|inner| inner.protocol_type())
    }

    pub fn protocol_type_owned(self) -> Option<StatefulProtocolType> {
        self.inner.map(|inner| inner.protocol_type_owned())
    }

    pub fn mark_kitty_rows_for_redraw(&self, area: Rect, damage: Rect, buf: &mut Buffer) {
        if let Some(protocol) = &self.inner {
            protocol.mark_kitty_rows_for_redraw(area, damage, buf);
        }
    }

    /// yututui patch: set the latest desired native render scale.
    ///
    /// If a resize is already running, that response still returns ownership of the protocol.
    /// The desired scale is applied as soon as it returns and a follow-up resize supersedes its
    /// stale encoding without ever drawing it.
    pub fn set_render_scale(&mut self, render_scale: RenderScale) {
        self.render_scale = render_scale.normalized();
        if let Some(protocol) = &mut self.inner {
            protocol.set_render_scale(self.render_scale);
        }
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
    }

    pub fn update_resized_protocol(&mut self, mut completed: ResizeResponse) -> bool {
        let equal = self.pending_id == Some(completed.id);
        if equal {
            completed.protocol.set_render_scale(self.render_scale);
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
}

impl ResizeEncodeRender for ThreadProtocol {
    fn resize_encode_render(&mut self, resize: &Resize, area: Rect, buf: &mut Buffer) {
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
        let Some(protocol) = self.inner.take() else {
            return;
        };
        let id = next_resize_request_id();
        self.pending_id = Some(id);
        let request = ResizeRequest {
            protocol,
            resize: resize.clone(),
            size,
            id,
        };
        if let Err(request) = send_resize_request(&self.tx, request) {
            self.inner = Some(request.protocol);
            self.pending_id = None;
        }
    }

    /// Render the currently resized and encoded data to the buffer, if there isn't a pending `ResizeRequest`
    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let _ = self
            .inner
            .as_mut()
            .map(|protocol| protocol.render(area, buf));
    }
}

#[cfg(feature = "tokio")]
fn send_resize_request(
    tx: &Sender<ResizeRequest>,
    request: ResizeRequest,
) -> Result<(), ResizeRequest> {
    tx.try_send(request).map_err(|err| match err {
        TrySendError::Full(request) | TrySendError::Closed(request) => request,
    })
}

#[cfg(not(feature = "tokio"))]
fn send_resize_request(
    tx: &Sender<ResizeRequest>,
    request: ResizeRequest,
) -> Result<(), ResizeRequest> {
    tx.send(request).map_err(|err| err.0)
}

#[cfg(all(test, feature = "tokio"))]
mod tests {
    use image::DynamicImage;
    use ratatui::{buffer::Buffer, layout::Rect};

    use super::*;
    use crate::{FontSize, protocol::kitty::StatefulKitty};

    #[test]
    fn scale_change_while_encoding_keeps_ownership_and_queues_latest_scale() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let protocol = StatefulProtocol::new(
            DynamicImage::new_rgba8(4, 2),
            FontSize::new(1, 1),
            None,
            StatefulProtocolType::Kitty(StatefulKitty::new(42, false)),
        );
        let mut threaded = ThreadProtocol::new(tx, Some(protocol));
        let area = Rect::new(0, 0, 2, 1);

        let mut first = Buffer::empty(area);
        threaded.resize_encode_render(&Resize::Scale(None), area, &mut first);
        let request_at_normal = rx.try_recv().expect("initial resize request");
        assert!(!first.cell((0, 0)).unwrap().symbol().contains("_G"));

        let latest = RenderScale::Uniform {
            factor: 2,
            double_width_lines: false,
        };
        threaded.set_render_scale(latest);
        let response_at_normal = request_at_normal.resize_encode().unwrap();
        assert!(
            threaded.update_resized_protocol(response_at_normal),
            "the in-flight response carries protocol ownership back"
        );

        let mut superseding = Buffer::empty(area);
        threaded.resize_encode_render(&Resize::Scale(None), area, &mut superseding);
        let request_at_latest = rx.try_recv().expect("latest scale immediately re-queued");
        assert!(
            !superseding.cell((0, 0)).unwrap().symbol().contains("_G"),
            "the stale normal-scale encoding must never render"
        );

        let response_at_latest = request_at_latest.resize_encode().unwrap();
        assert!(threaded.update_resized_protocol(response_at_latest));
        let mut rendered = Buffer::empty(area);
        threaded.resize_encode_render(&Resize::Scale(None), area, &mut rendered);
        let anchor = rendered.cell((0, 0)).unwrap().symbol();
        assert!(
            anchor.contains("s=4,v=2"),
            "latest scale also enlarges the encoded raster: {anchor:?}"
        );
        assert!(
            anchor.contains("c=4,r=2,C=1"),
            "latest direct placement: {anchor:?}"
        );
        assert!(
            !anchor.contains("U=1"),
            "scaled Kitty must not use placeholders"
        );
        assert!(rx.try_recv().is_err(), "latest encoding is now current");
    }

    #[test]
    fn response_from_previous_protocol_owner_cannot_install_into_replacement() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let area = Rect::new(0, 0, 2, 1);
        let resize = Resize::Scale(None);

        let protocol_a = StatefulProtocol::new(
            DynamicImage::new_rgba8(4, 2),
            FontSize::new(1, 1),
            None,
            StatefulProtocolType::Kitty(StatefulKitty::new(101, false)),
        );
        let mut owner_a = ThreadProtocol::new(tx.clone(), Some(protocol_a));
        owner_a.resize_encode(&resize, area.as_size());
        let request_a = rx.try_recv().expect("A is in flight");
        let request_a_id = request_a.id;

        let protocol_b = StatefulProtocol::new(
            DynamicImage::new_rgba8(4, 2),
            FontSize::new(1, 1),
            None,
            StatefulProtocolType::Kitty(StatefulKitty::new(202, false)),
        );
        let mut owner_b = ThreadProtocol::new(tx, Some(protocol_b));
        owner_b.resize_encode(&resize, area.as_size());

        let response_a = request_a.resize_encode().unwrap();
        assert!(
            !owner_b.update_resized_protocol(response_a),
            "A's completion must not replace B while B is queued"
        );

        let request_b = rx
            .try_recv()
            .expect("B follows A in the serial worker queue");
        assert_ne!(
            request_a_id, request_b.id,
            "request generations are process-global"
        );
        let response_b = request_b.resize_encode().unwrap();
        assert!(owner_b.update_resized_protocol(response_b));

        let mut rendered = Buffer::empty(area);
        owner_b.render(area, &mut rendered);
        let anchor = rendered.cell((0, 0)).unwrap().symbol();
        assert!(
            anchor.contains("i=202,"),
            "B remains the rendered owner: {anchor:?}"
        );
        assert!(
            !anchor.contains("i=101,"),
            "A pixels/id must stay discarded"
        );
    }
}
