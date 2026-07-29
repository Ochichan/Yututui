//! Real-server tolerances the WebDAV vault client needs at connection time.
//!
//! Both behaviours here exist because of measured servers, not specification reading:
//!
//! - The configured vault root may not exist yet. `ensure_ancestor_collections` only creates
//!   collections *below* the root, so the first `MKCOL <root>/yututui` used to fail with 409 on
//!   Nextcloud and never explain why. Creating the root itself first is idempotent (405 means it
//!   was already there) and keeps a fresh endpoint path usable.
//! - Apache mod_dav marks an entity tag weak (`W/"…"`) for as long as the file's mtime is within
//!   one second of the response, and returns no `ETag` at all on PUT. A write followed by an
//!   immediate readback therefore observes a weak tag and fails closed with
//!   `MissingStrongEntityTag`, even though the very same object reports a strong tag a moment
//!   later. Only that transient case is retried; a server that never produces a strong tag still
//!   fails, because compare-and-swap correctness depends on it.

use super::*;

/// Waits before re-reading an object whose entity tag came back weak. Sized for the one-second
/// mtime window in Apache's `ETag` weakening rule, with one extra attempt for clock skew.
const WEAK_ETAG_READBACK_DELAYS: [Duration; 2] =
    [Duration::from_millis(400), Duration::from_millis(900)];

impl WebDavCapabilities {
    /// Whether the endpoint answered OPTIONS as a WebDAV class 1 collection.
    ///
    /// The individual method flags are diagnostics, not gates. RFC 4918 defines `Allow` per
    /// resource: MKCOL is never legal on a collection that already exists, and PUT is never legal
    /// on a collection at all, so no conforming server advertises both on the vault root. The
    /// servers measured for this fix disagree exactly that way — Nextcloud omits MKCOL on an
    /// existing collection, Apache mod_dav omits PUT and MKCOL, and on a missing path Apache omits
    /// GET and PROPFIND instead. [`super::BlockingWebDavTransport::probe_capabilities`] therefore
    /// proves MKCOL, PUT, GET and PROPFIND by performing them against a dedicated marker object —
    /// stronger evidence than a header list — and records the outcome in these flags.
    pub fn supports_encrypted_sync(self) -> bool {
        self.dav_class_1 && self.options
    }
}

impl WebDavClient {
    /// Create the configured vault root collection if it is missing.
    ///
    /// `AlreadyPresent` (405) is the normal answer for an endpoint the user already created, and a
    /// 409 still surfaces as an error because it means the root's own parent does not exist — that
    /// is a wrong endpoint, not something to paper over.
    pub(super) async fn mkcol_root(
        &self,
        credential: &VaultCredential,
    ) -> Result<CollectionWriteResult, WebDavError> {
        let method = Method::from_bytes(b"MKCOL").map_err(|_| WebDavError::InvalidResponse)?;
        let request = self.authenticated_request(method, self.base.clone(), credential)?;
        let response = self.execute(request).await?;
        match response.status() {
            StatusCode::CREATED => Ok(CollectionWriteResult::Created),
            StatusCode::METHOD_NOT_ALLOWED => Ok(CollectionWriteResult::AlreadyPresent),
            _ => Err(status_error(&response)),
        }
    }

    /// Read an object back after writing it, tolerating a briefly weak entity tag.
    pub(super) async fn get_readback(
        &self,
        key: &ObjectKey,
        credential: &VaultCredential,
        body_limit: usize,
        deadline: Option<VaultDeadline>,
    ) -> Result<Option<(EncryptedObject, ObjectMetadata)>, WebDavError> {
        for delay in WEAK_ETAG_READBACK_DELAYS {
            match self.get_inner(key, credential, body_limit, deadline).await {
                Err(WebDavError::MissingStrongEntityTag) => {
                    await_with_deadline(deadline, async move {
                        tokio::time::sleep(delay).await;
                        Ok(())
                    })
                    .await?;
                }
                result => return result,
            }
        }
        self.get_inner(key, credential, body_limit, deadline).await
    }
}
