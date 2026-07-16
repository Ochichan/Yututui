//! Correlated request/reply adapter from a transfer job to the active playlist owner.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::oneshot;

use super::{EventSink, TransferEvent};
use crate::transfer::local_playlist::{
    LocalPlaylistOwnerReply, LocalPlaylistOwnerRequest, LocalPlaylistPatch, LocalPlaylistStore,
    LocalPlaylistStoreError, LocalPlaylistWriteOutcome,
};
use crate::util::delivery::DeliveryError;

type OwnerReply = Result<LocalPlaylistOwnerReply, LocalPlaylistStoreError>;

struct RequestInner {
    correlation_id: u64,
    request: LocalPlaylistOwnerRequest,
    reply: Mutex<Option<oneshot::Sender<OwnerReply>>>,
}

/// Cloneable event payload whose single reply sender remains owned across ingress retries.
#[derive(Clone)]
pub struct LocalPlaylistRequest {
    inner: Arc<RequestInner>,
}

impl fmt::Debug for LocalPlaylistRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalPlaylistRequest")
            .field("correlation_id", &self.correlation_id())
            .field("request", self.request())
            .finish_non_exhaustive()
    }
}

impl LocalPlaylistRequest {
    fn new(
        correlation_id: u64,
        request: LocalPlaylistOwnerRequest,
    ) -> (Self, oneshot::Receiver<OwnerReply>) {
        let (reply, receiver) = oneshot::channel();
        (
            Self {
                inner: Arc::new(RequestInner {
                    correlation_id,
                    request,
                    reply: Mutex::new(Some(reply)),
                }),
            },
            receiver,
        )
    }

    pub(crate) fn correlation_id(&self) -> u64 {
        self.inner.correlation_id
    }

    pub(crate) fn request(&self) -> &LocalPlaylistOwnerRequest {
        &self.inner.request
    }

    /// Settle this exact request once. `false` means cancellation/shutdown already dropped the
    /// waiting job or a duplicate owner path attempted to reply.
    pub(crate) fn respond(&self, reply: OwnerReply) -> bool {
        let sender = self
            .inner
            .reply
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        sender.is_some_and(|sender| sender.send(reply).is_ok())
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        correlation_id: u64,
        request: LocalPlaylistOwnerRequest,
    ) -> (Self, oneshot::Receiver<OwnerReply>) {
        Self::new(correlation_id, request)
    }
}

pub(super) struct OwnerLocalPlaylistStore {
    emit: EventSink,
    next_request_id: Arc<AtomicU64>,
}

impl OwnerLocalPlaylistStore {
    pub(super) fn new(emit: EventSink, next_request_id: Arc<AtomicU64>) -> Self {
        Self {
            emit,
            next_request_id,
        }
    }

    async fn round_trip(
        &mut self,
        operation: LocalPlaylistOwnerRequest,
    ) -> Result<LocalPlaylistOwnerReply, LocalPlaylistStoreError> {
        let resumable = matches!(operation, LocalPlaylistOwnerRequest::Apply(_));
        let correlation_id = self.next_request_id.fetch_add(1, Ordering::AcqRel) + 1;
        let (request, reply) = LocalPlaylistRequest::new(correlation_id, operation);
        let event = TransferEvent::LocalPlaylistRequest(request);
        emit_request_reliably(&self.emit, event)
            .await
            .map_err(|error| request_error(correlation_id, resumable, error.to_string()))?;
        reply.await.map_err(|_| {
            request_error(
                correlation_id,
                resumable,
                "playlist owner dropped the correlated reply",
            )
        })?
    }
}

impl LocalPlaylistStore for OwnerLocalPlaylistStore {
    async fn snapshot(&mut self) -> Result<crate::playlists::Playlists, LocalPlaylistStoreError> {
        match self.round_trip(LocalPlaylistOwnerRequest::Snapshot).await? {
            LocalPlaylistOwnerReply::Snapshot(snapshot) => Ok(snapshot),
            LocalPlaylistOwnerReply::Applied(_) => Err(LocalPlaylistStoreError::fatal(
                "playlist owner returned an apply reply for a snapshot request",
            )),
        }
    }

    async fn apply(
        &mut self,
        patch: LocalPlaylistPatch,
    ) -> Result<LocalPlaylistWriteOutcome, LocalPlaylistStoreError> {
        match self
            .round_trip(LocalPlaylistOwnerRequest::Apply(patch))
            .await?
        {
            LocalPlaylistOwnerReply::Applied(outcome) => Ok(outcome),
            LocalPlaylistOwnerReply::Snapshot(_) => Err(LocalPlaylistStoreError::resumable(
                "playlist owner returned a snapshot reply for an apply request",
            )),
        }
    }
}

fn request_error(
    correlation_id: u64,
    resumable: bool,
    message: impl fmt::Display,
) -> LocalPlaylistStoreError {
    let message = format!("playlist owner request {correlation_id} failed: {message}");
    if resumable {
        LocalPlaylistStoreError::resumable(message)
    } else {
        LocalPlaylistStoreError::fatal(message)
    }
}

async fn emit_request_reliably(
    emit: &EventSink,
    event: TransferEvent,
) -> Result<(), DeliveryError> {
    loop {
        match emit(event.clone()) {
            Ok(_) => return Ok(()),
            Err(DeliveryError::Closed) => return Err(DeliveryError::Closed),
            Err(
                DeliveryError::Busy
                | DeliveryError::StaleOrFull
                | DeliveryError::BestEffortDropped
                | DeliveryError::Saturated,
            ) => tokio::time::sleep(Duration::from_millis(5)).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn correlation_is_exact_and_a_second_reply_is_rejected() {
        let (request, reply) = LocalPlaylistRequest::new(41, LocalPlaylistOwnerRequest::Snapshot);
        assert_eq!(request.correlation_id(), 41);
        assert!(request.respond(Ok(LocalPlaylistOwnerReply::Snapshot(
            crate::playlists::Playlists::default()
        ))));
        assert!(matches!(
            reply.await.expect("correlated reply"),
            Ok(LocalPlaylistOwnerReply::Snapshot(_))
        ));
        assert!(!request.respond(Err(LocalPlaylistStoreError::fatal("duplicate"))));
    }

    #[tokio::test]
    async fn cancelled_waiter_makes_the_owner_reply_inert() {
        let (request, reply) = LocalPlaylistRequest::new(7, LocalPlaylistOwnerRequest::Snapshot);
        drop(reply);
        assert!(!request.respond(Ok(LocalPlaylistOwnerReply::Snapshot(
            crate::playlists::Playlists::default()
        ))));
    }
}
