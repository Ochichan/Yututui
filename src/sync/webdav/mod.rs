//! Bounded WebDAV transport for encrypted personal-state objects.
//!
//! This layer owns HTTP protocol details only. It accepts and returns [`EncryptedObject`] values,
//! follows at most three exact-origin redirects, and never includes endpoint or credential text in
//! its errors. Merge policy, retries, scheduling, and primary-writer ownership live above it.

mod xml;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::error::Error as _;
use std::fmt;
use std::future::Future;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::Duration;

use age::secrecy::{ExposeSecret as _, SecretString};
use reqwest::header::{
    ACCEPT, ALLOW, AUTHORIZATION, CONTENT_TYPE, ETAG, HeaderMap, HeaderName, HeaderValue, IF_MATCH,
    IF_NONE_MATCH, LOCATION,
};
use reqwest::{Method, Request, Response, StatusCode, Url};

use super::crypto::{
    DeviceSecretMaterial, MAX_ENCRYPTED_OBJECT_BYTES, MAX_PROTECTED_PAYLOAD_BYTES,
    encrypt_json_to_recipients, random_id_hex, sha256_domain_hex,
};
use super::{
    EncryptedObject, ListCost, ListLimits, ListOutcome, ObjectCondition, ObjectKey, ObjectMetadata,
    ObjectWriteResult, VaultCredential, VaultCredentialKind, VaultDeadline, VaultError,
    VaultTransport,
};

pub const MAX_PROPFIND_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_PROPFIND_RESOURCES: usize = 10_000;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DIRECT_LIST_DEADLINE: Duration = Duration::from_secs(5 * 60);
const MAX_REDIRECTS: usize = 3;
const MAX_LIST_PROPFIND_REQUESTS: usize = 1_024;
const MAX_LIST_DEPTH: usize = 8;
const CONDITIONAL_PROBE_PLAINTEXT_LIMIT: usize = 16 * 1024;
const CAPABILITY_PROPFIND_BYTES: usize = 64 * 1024;
const CAPABILITY_PROPFIND_RESOURCES: usize = 64;
const MAX_ENDPOINT_BYTES: usize = 8 * 1024;
const MAX_LOCATION_BYTES: usize = 8 * 1024;
const MAX_PROTOCOL_HEADER_BYTES: usize = 8 * 1024;
const MAX_ETAG_BYTES: usize = 1024;
const DAV_HEADER: HeaderName = HeaderName::from_static("dav");
const DEPTH_HEADER: HeaderName = HeaderName::from_static("depth");
const READBACK_HASH_DOMAIN: &[u8] = b"yututui-webdav-put-readback-v1";
const CONDITIONAL_PROBE_KEY: &str = "yututui/v2/capability/conditional-put-v1.age";
const PROPFIND_BODY: &[u8] = br#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:"><d:prop><d:getetag/><d:getcontentlength/><d:resourcetype/></d:prop></d:propfind>"#;

/// A redacted WebDAV failure safe for retained status and audit summaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebDavError {
    InvalidEndpoint,
    UnsupportedScheme,
    EndpointCredentials,
    CertificateFailed,
    RequestFailed,
    ResponseTooLarge,
    InvalidResponse,
    InvalidXml,
    InvalidEntityTag,
    MissingStrongEntityTag,
    CrossOriginRedirect,
    RedirectLimitExceeded,
    InvalidRedirect,
    AuthenticationRequired,
    PermissionDenied,
    NotFound,
    MethodUnsupported,
    Conflict,
    PreconditionFailed,
    Locked,
    RateLimited,
    ServerUnavailable,
    UnexpectedStatus(u16),
    ResourceLimitExceeded,
    InvalidEncryptedObject,
    AmbiguousWrite,
}

impl fmt::Display for WebDavError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEndpoint => f.write_str("the WebDAV endpoint is invalid"),
            Self::UnsupportedScheme => f.write_str("the WebDAV endpoint scheme is not supported"),
            Self::EndpointCredentials => {
                f.write_str("the WebDAV endpoint must not contain credentials")
            }
            Self::CertificateFailed => {
                f.write_str("the WebDAV server certificate could not be verified")
            }
            Self::RequestFailed => f.write_str("the WebDAV request failed"),
            Self::ResponseTooLarge => f.write_str("the WebDAV response exceeded its size limit"),
            Self::InvalidResponse => f.write_str("the WebDAV response is invalid"),
            Self::InvalidXml => f.write_str("the WebDAV listing is invalid"),
            Self::InvalidEntityTag => f.write_str("the WebDAV entity tag is invalid"),
            Self::MissingStrongEntityTag => {
                f.write_str("the WebDAV server did not provide a strong entity tag")
            }
            Self::CrossOriginRedirect => {
                f.write_str("the WebDAV server redirected to a different origin")
            }
            Self::RedirectLimitExceeded => f.write_str("the WebDAV redirect limit was exceeded"),
            Self::InvalidRedirect => f.write_str("the WebDAV redirect is invalid"),
            Self::AuthenticationRequired => f.write_str("the WebDAV credentials were rejected"),
            Self::PermissionDenied => f.write_str("the WebDAV request was not allowed"),
            Self::NotFound => f.write_str("the WebDAV resource was not found"),
            Self::MethodUnsupported => {
                f.write_str("the WebDAV server does not support the required method")
            }
            Self::Conflict => f.write_str("the WebDAV collection is not ready"),
            Self::PreconditionFailed => {
                f.write_str("the WebDAV resource changed before it was written")
            }
            Self::Locked => f.write_str("the WebDAV resource is locked"),
            Self::RateLimited => f.write_str("the WebDAV server asked the client to retry later"),
            Self::ServerUnavailable => f.write_str("the WebDAV server is temporarily unavailable"),
            Self::UnexpectedStatus(status) => {
                write!(f, "the WebDAV server returned HTTP {status}")
            }
            Self::ResourceLimitExceeded => f.write_str("the WebDAV resource limit was exceeded"),
            Self::InvalidEncryptedObject => {
                f.write_str("the WebDAV object is not valid encrypted state")
            }
            Self::AmbiguousWrite => f.write_str("the WebDAV write result could not be verified"),
        }
    }
}

impl std::error::Error for WebDavError {}

/// An RFC 7232 entity tag retained exactly as it appears on the wire.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct EntityTag {
    wire: String,
    weak: bool,
}

impl EntityTag {
    pub fn parse(raw: &str) -> Result<Self, WebDavError> {
        let raw = raw.trim();
        if raw.is_empty() || raw.len() > MAX_ETAG_BYTES {
            return Err(WebDavError::InvalidEntityTag);
        }
        let (weak, quoted) = if let Some(quoted) = raw.strip_prefix("W/") {
            (true, quoted)
        } else {
            (false, raw)
        };
        let Some(opaque) = quoted
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
        else {
            return Err(WebDavError::InvalidEntityTag);
        };
        if opaque
            .bytes()
            .any(|byte| !byte.is_ascii() || byte == b'"' || byte == 0x7f || byte.is_ascii_control())
        {
            return Err(WebDavError::InvalidEntityTag);
        }
        Ok(Self {
            wire: raw.to_owned(),
            weak,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.wire
    }

    pub fn is_weak(&self) -> bool {
        self.weak
    }

    fn require_strong(&self) -> Result<(), WebDavError> {
        if self.weak {
            Err(WebDavError::MissingStrongEntityTag)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropfindDepth {
    Zero,
    One,
    Infinity,
}

impl PropfindDepth {
    fn header(self) -> HeaderValue {
        HeaderValue::from_static(match self {
            Self::Zero => "0",
            Self::One => "1",
            Self::Infinity => "infinity",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebDavResource {
    /// `None` denotes the collection on which PROPFIND was issued.
    pub key: Option<ObjectKey>,
    pub etag: Option<EntityTag>,
    pub content_length: Option<u64>,
    pub is_collection: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionWriteResult {
    Created,
    AlreadyPresent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WebDavCapabilities {
    pub dav_class_1: bool,
    pub dav_class_2: bool,
    pub options: bool,
    pub propfind: bool,
    pub mkcol: bool,
    pub get: bool,
    pub put: bool,
}

impl WebDavCapabilities {
    pub fn supports_encrypted_sync(self) -> bool {
        self.dav_class_1 && self.options && self.propfind && self.mkcol && self.get && self.put
    }
}

/// HTTP client rooted at one WebDAV collection.
///
/// Credentials are borrowed per operation instead of retained or made clonable. The internal
/// reqwest client has redirect following disabled; each redirect is evaluated here before an
/// authenticated request is replayed.
pub struct WebDavClient {
    http: reqwest::Client,
    base: Url,
}

impl WebDavClient {
    pub fn new(endpoint: &str) -> Result<Self, WebDavError> {
        Self::with_custom_ca(endpoint, None)
    }

    pub fn with_custom_ca(
        endpoint: &str,
        custom_ca_pem: Option<&[u8]>,
    ) -> Result<Self, WebDavError> {
        let base = normalize_endpoint(endpoint)?;
        let mut builder = reqwest::Client::builder()
            .user_agent("yututui-webdav/1")
            .timeout(REQUEST_TIMEOUT)
            // This client owns a reusable WebDAV credential. Implicit environment or system
            // proxies would let `http://127.0.0.1` escape the exact-loopback exception and expose
            // Basic/Bearer authentication in cleartext, so every supported origin is contacted
            // directly.
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none());
        if let Some(pem) = custom_ca_pem {
            if pem.is_empty() || pem.len() > super::profile::MAX_CUSTOM_CA_PEM_BYTES {
                return Err(WebDavError::CertificateFailed);
            }
            let certificates = reqwest::Certificate::from_pem_bundle(pem)
                .map_err(|_| WebDavError::CertificateFailed)?;
            if certificates.is_empty() {
                return Err(WebDavError::CertificateFailed);
            }
            for certificate in certificates {
                builder = builder.add_root_certificate(certificate);
            }
        }
        let http = builder
            .build()
            .map_err(|error| classify_request_error(&error))?;
        Ok(Self { http, base })
    }

    pub async fn options(
        &self,
        credential: &VaultCredential,
    ) -> Result<WebDavCapabilities, WebDavError> {
        let request = self.authenticated_request(Method::OPTIONS, self.base.clone(), credential)?;
        let response = self.execute(request).await?;
        if response.status() != StatusCode::OK && response.status() != StatusCode::NO_CONTENT {
            return Err(status_error(response.status()));
        }
        parse_capabilities(response.headers())
    }

    pub async fn mkcol(
        &self,
        key: &ObjectKey,
        credential: &VaultCredential,
    ) -> Result<CollectionWriteResult, WebDavError> {
        self.mkcol_inner(key, credential, None).await
    }

    async fn mkcol_with_deadline(
        &self,
        key: &ObjectKey,
        credential: &VaultCredential,
        deadline: VaultDeadline,
    ) -> Result<CollectionWriteResult, WebDavError> {
        self.mkcol_inner(key, credential, Some(deadline)).await
    }

    async fn mkcol_inner(
        &self,
        key: &ObjectKey,
        credential: &VaultCredential,
        deadline: Option<VaultDeadline>,
    ) -> Result<CollectionWriteResult, WebDavError> {
        let method = Method::from_bytes(b"MKCOL").map_err(|_| WebDavError::InvalidResponse)?;
        let request = self.authenticated_request(method, self.object_url(key)?, credential)?;
        let response = self.execute_inner(request, deadline).await?;
        match response.status() {
            StatusCode::CREATED => Ok(CollectionWriteResult::Created),
            StatusCode::METHOD_NOT_ALLOWED => Ok(CollectionWriteResult::AlreadyPresent),
            status => Err(status_error(status)),
        }
    }

    pub async fn propfind(
        &self,
        key: &ObjectKey,
        depth: PropfindDepth,
        credential: &VaultCredential,
        max_resources: usize,
    ) -> Result<Vec<WebDavResource>, WebDavError> {
        self.propfind_bounded(key, depth, credential, max_resources, MAX_PROPFIND_BYTES)
            .await
            .map(|(resources, _)| resources)
    }

    async fn propfind_bounded(
        &self,
        key: &ObjectKey,
        depth: PropfindDepth,
        credential: &VaultCredential,
        max_resources: usize,
        max_body_bytes: usize,
    ) -> Result<(Vec<WebDavResource>, usize), WebDavError> {
        if max_resources == 0 || max_resources > MAX_PROPFIND_RESOURCES {
            return Err(WebDavError::ResourceLimitExceeded);
        }
        if max_body_bytes == 0 || max_body_bytes > MAX_PROPFIND_BYTES {
            return Err(WebDavError::ResourceLimitExceeded);
        }
        let method = Method::from_bytes(b"PROPFIND").map_err(|_| WebDavError::InvalidResponse)?;
        let mut request = self.authenticated_request(method, self.object_url(key)?, credential)?;
        request.headers_mut().insert(DEPTH_HEADER, depth.header());
        request.headers_mut().insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/xml; charset=utf-8"),
        );
        *request.body_mut() = Some(PROPFIND_BODY.to_vec().into());

        let response = self.execute(request).await?;
        if response.status() != StatusCode::MULTI_STATUS {
            return Err(status_error(response.status()));
        }
        let response_url = response.url().clone();
        let bytes = read_limited(response, max_body_bytes).await?;
        let body_bytes = bytes.len();
        let raw = xml::parse_multistatus(&bytes, max_resources)?;
        self.resolve_resources(raw, &response_url)
            .map(|resources| (resources, body_bytes))
    }

    pub async fn get(
        &self,
        key: &ObjectKey,
        credential: &VaultCredential,
        max_plaintext_bytes: usize,
    ) -> Result<Option<(EncryptedObject, ObjectMetadata)>, WebDavError> {
        self.get_inner(key, credential, max_plaintext_bytes, None)
            .await
    }

    async fn get_with_deadline(
        &self,
        key: &ObjectKey,
        credential: &VaultCredential,
        max_plaintext_bytes: usize,
        deadline: VaultDeadline,
    ) -> Result<Option<(EncryptedObject, ObjectMetadata)>, WebDavError> {
        self.get_inner(key, credential, max_plaintext_bytes, Some(deadline))
            .await
    }

    async fn get_inner(
        &self,
        key: &ObjectKey,
        credential: &VaultCredential,
        max_plaintext_bytes: usize,
        deadline: Option<VaultDeadline>,
    ) -> Result<Option<(EncryptedObject, ObjectMetadata)>, WebDavError> {
        let body_limit = encrypted_body_limit(max_plaintext_bytes)?;
        let mut request =
            self.authenticated_request(Method::GET, self.object_url(key)?, credential)?;
        request
            .headers_mut()
            .insert(ACCEPT, HeaderValue::from_static("application/octet-stream"));
        let response = self.execute_inner(request, deadline).await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if response.status() != StatusCode::OK {
            return Err(status_error(response.status()));
        }
        let etag = strong_etag(response.headers())?;
        let bytes = await_with_deadline(deadline, read_limited(response, body_limit)).await?;
        let object =
            EncryptedObject::from_bytes(bytes).map_err(|_| WebDavError::InvalidEncryptedObject)?;
        let content_length = object
            .as_bytes()
            .len()
            .try_into()
            .map_err(|_| WebDavError::ResponseTooLarge)?;
        Ok(Some((
            object,
            ObjectMetadata {
                key: key.clone(),
                etag: etag.wire,
                content_length,
            },
        )))
    }

    pub async fn put(
        &self,
        key: &ObjectKey,
        object: &EncryptedObject,
        condition: ObjectCondition,
        credential: &VaultCredential,
    ) -> Result<ObjectWriteResult, WebDavError> {
        self.put_inner(key, object, condition, credential, None)
            .await
    }

    async fn put_with_deadline(
        &self,
        key: &ObjectKey,
        object: &EncryptedObject,
        condition: ObjectCondition,
        credential: &VaultCredential,
        deadline: VaultDeadline,
    ) -> Result<ObjectWriteResult, WebDavError> {
        self.put_inner(key, object, condition, credential, Some(deadline))
            .await
    }

    async fn put_inner(
        &self,
        key: &ObjectKey,
        object: &EncryptedObject,
        condition: ObjectCondition,
        credential: &VaultCredential,
        deadline: Option<VaultDeadline>,
    ) -> Result<ObjectWriteResult, WebDavError> {
        let response = match self
            .send_conditional_put(key, object, &condition, credential, deadline)
            .await
        {
            Ok(response) => response,
            Err(WebDavError::RequestFailed) => {
                return self
                    .verify_ambiguous_put(key, object, credential, deadline)
                    .await;
            }
            Err(error) => return Err(error),
        };
        let status = response.status();
        if status == StatusCode::PRECONDITION_FAILED {
            return Err(WebDavError::PreconditionFailed);
        }
        if status.is_server_error() {
            return self
                .verify_ambiguous_put(key, object, credential, deadline)
                .await;
        }
        let created = match (&condition, status) {
            (ObjectCondition::CreateOnly, StatusCode::CREATED) => true,
            (ObjectCondition::Match(_), StatusCode::OK | StatusCode::NO_CONTENT) => false,
            (ObjectCondition::CreateOnly, StatusCode::OK | StatusCode::NO_CONTENT)
            | (ObjectCondition::Match(_), StatusCode::CREATED) => {
                return Err(WebDavError::MethodUnsupported);
            }
            _ => return Err(status_error(status)),
        };

        if let Some(etag) = optional_etag(response.headers())?
            && !etag.is_weak()
        {
            let metadata = ObjectMetadata {
                key: key.clone(),
                etag: etag.wire,
                content_length: object
                    .as_bytes()
                    .len()
                    .try_into()
                    .map_err(|_| WebDavError::InvalidEncryptedObject)?,
            };
            return if created {
                Ok(ObjectWriteResult::Created(metadata))
            } else {
                Ok(ObjectWriteResult::Updated(metadata))
            };
        }

        let metadata = self
            .verify_present(key, object, credential, deadline)
            .await?;
        if created {
            Ok(ObjectWriteResult::Created(metadata))
        } else {
            Ok(ObjectWriteResult::Updated(metadata))
        }
    }

    async fn send_conditional_put(
        &self,
        key: &ObjectKey,
        object: &EncryptedObject,
        condition: &ObjectCondition,
        credential: &VaultCredential,
        deadline: Option<VaultDeadline>,
    ) -> Result<Response, WebDavError> {
        if !object.is_locally_produced() || object.as_bytes().len() > MAX_ENCRYPTED_OBJECT_BYTES {
            return Err(WebDavError::InvalidEncryptedObject);
        }
        let mut request =
            self.authenticated_request(Method::PUT, self.object_url(key)?, credential)?;
        request.headers_mut().insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        );
        apply_condition(request.headers_mut(), condition)?;
        *request.body_mut() = Some(object.as_bytes().to_vec().into());
        self.execute_inner(request, deadline).await
    }

    async fn probe_conditional_writes(
        &self,
        key: &ObjectKey,
        object: &EncryptedObject,
        credential: &VaultCredential,
    ) -> Result<(), WebDavError> {
        let existing = self
            .get(key, credential, CONDITIONAL_PROBE_PLAINTEXT_LIMIT)
            .await?;
        if existing.is_none() {
            let response = self
                .send_conditional_put(key, object, &ObjectCondition::CreateOnly, credential, None)
                .await?;
            match response.status() {
                StatusCode::CREATED => {}
                // Another client may have installed the same harmless marker after our GET.
                StatusCode::PRECONDITION_FAILED => {}
                _ => return Err(WebDavError::MethodUnsupported),
            }
        }

        let repeated_create = self
            .send_conditional_put(key, object, &ObjectCondition::CreateOnly, credential, None)
            .await?;
        if repeated_create.status() != StatusCode::PRECONDITION_FAILED {
            return Err(WebDavError::MethodUnsupported);
        }

        let impossible_etag = format!(
            "\"yututui-probe-{}\"",
            random_id_hex::<16>().map_err(|_| WebDavError::RequestFailed,)?
        );
        let mismatched_update = self
            .send_conditional_put(
                key,
                object,
                &ObjectCondition::Match(impossible_etag),
                credential,
                None,
            )
            .await?;
        if mismatched_update.status() != StatusCode::PRECONDITION_FAILED {
            return Err(WebDavError::MethodUnsupported);
        }

        // Always exercise a valid strong If-Match update. Merely observing an existing marker
        // does not prove that the server implements the compare-and-swap primitive used by heads
        // and manifests.
        let (_, metadata) = self
            .get(key, credential, CONDITIONAL_PROBE_PLAINTEXT_LIMIT)
            .await?
            .ok_or(WebDavError::MethodUnsupported)?;
        let matched_update = self
            .send_conditional_put(
                key,
                object,
                &ObjectCondition::Match(metadata.etag),
                credential,
                None,
            )
            .await?;
        if !matches!(
            matched_update.status(),
            StatusCode::OK | StatusCode::NO_CONTENT
        ) {
            return Err(WebDavError::MethodUnsupported);
        }
        self.verify_present(key, object, credential, None).await?;
        Ok(())
    }

    fn authenticated_request(
        &self,
        method: Method,
        url: Url,
        credential: &VaultCredential,
    ) -> Result<Request, WebDavError> {
        let builder = self.http.request(method, url);
        let builder = match credential.kind() {
            VaultCredentialKind::Password => {
                let username = credential
                    .username()
                    .ok_or(WebDavError::AuthenticationRequired)?;
                builder.basic_auth(
                    username.expose_secret(),
                    Some(credential.secret().expose_secret()),
                )
            }
            VaultCredentialKind::BearerToken => {
                builder.bearer_auth(credential.secret().expose_secret())
            }
        };
        let mut request = builder.build().map_err(|_| WebDavError::RequestFailed)?;
        if let Some(value) = request.headers_mut().get_mut(AUTHORIZATION) {
            value.set_sensitive(true);
        }
        Ok(request)
    }

    async fn execute(&self, template: Request) -> Result<Response, WebDavError> {
        self.execute_inner(template, None).await
    }

    async fn execute_inner(
        &self,
        template: Request,
        deadline: Option<VaultDeadline>,
    ) -> Result<Response, WebDavError> {
        let mut current = template.url().clone();
        for followed in 0..=MAX_REDIRECTS {
            let mut request = template.try_clone().ok_or(WebDavError::RequestFailed)?;
            *request.url_mut() = current.clone();
            let request = async {
                self.http
                    .execute(request)
                    .await
                    .map_err(|error| classify_request_error(&error))
            };
            let response = await_with_deadline(deadline, request).await?;
            if !response.status().is_redirection() {
                return Ok(response);
            }
            if followed == MAX_REDIRECTS {
                return Err(WebDavError::RedirectLimitExceeded);
            }
            if !matches!(
                response.status(),
                StatusCode::MOVED_PERMANENTLY
                    | StatusCode::FOUND
                    | StatusCode::TEMPORARY_REDIRECT
                    | StatusCode::PERMANENT_REDIRECT
            ) {
                return Err(WebDavError::InvalidRedirect);
            }
            let location = response
                .headers()
                .get(LOCATION)
                .ok_or(WebDavError::InvalidRedirect)?
                .to_str()
                .map_err(|_| WebDavError::InvalidRedirect)?;
            if location.is_empty() || location.len() > MAX_LOCATION_BYTES {
                return Err(WebDavError::InvalidRedirect);
            }
            let next = current
                .join(location)
                .map_err(|_| WebDavError::InvalidRedirect)?;
            validate_redirect(&self.base, &next)?;
            current = next;
        }
        Err(WebDavError::RedirectLimitExceeded)
    }

    fn object_url(&self, key: &ObjectKey) -> Result<Url, WebDavError> {
        let url = self
            .base
            .join(key.as_str())
            .map_err(|_| WebDavError::InvalidEndpoint)?;
        if url.origin() != self.base.origin() || !url.path().starts_with(self.base.path()) {
            return Err(WebDavError::InvalidEndpoint);
        }
        Ok(url)
    }

    fn resolve_resources(
        &self,
        raw: Vec<xml::RawResource>,
        response_url: &Url,
    ) -> Result<Vec<WebDavResource>, WebDavError> {
        let mut unique = BTreeMap::<Option<ObjectKey>, WebDavResource>::new();
        for resource in raw {
            let key = self.resolve_href(&resource.href, response_url)?;
            let etag = resource.etag.as_deref().map(EntityTag::parse).transpose()?;
            let resolved = WebDavResource {
                key: key.clone(),
                etag,
                content_length: resource.content_length,
                is_collection: resource.is_collection,
            };
            match unique.get(&key) {
                Some(_) => return Err(WebDavError::InvalidResponse),
                None => {
                    unique.insert(key, resolved);
                }
            }
        }
        Ok(unique.into_values().collect())
    }

    fn resolve_href(
        &self,
        raw: &str,
        response_url: &Url,
    ) -> Result<Option<ObjectKey>, WebDavError> {
        if raw.is_empty()
            || raw.len() > MAX_LOCATION_BYTES
            || raw.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err(WebDavError::InvalidResponse);
        }
        let url = response_url
            .join(raw)
            .map_err(|_| WebDavError::InvalidResponse)?;
        validate_redirect(&self.base, &url).map_err(|_| WebDavError::InvalidResponse)?;
        let base_path = self.base.path();
        let path = url.path();
        if path == base_path || path == base_path.trim_end_matches('/') {
            return Ok(None);
        }
        let suffix = path
            .strip_prefix(base_path)
            .ok_or(WebDavError::InvalidResponse)?;
        let suffix = if let Some(without_slash) = suffix.strip_suffix('/') {
            if without_slash.ends_with('/') {
                return Err(WebDavError::InvalidResponse);
            }
            without_slash
        } else {
            suffix
        };
        // ObjectKey's alphabet never needs percent encoding. Reject alternate encodings instead
        // of accepting two wire paths for one signed protocol key.
        if suffix.is_empty() || suffix.contains('%') {
            return Err(WebDavError::InvalidResponse);
        }
        ObjectKey::new(suffix.to_owned())
            .map(Some)
            .map_err(|_| WebDavError::InvalidResponse)
    }

    async fn verify_present(
        &self,
        key: &ObjectKey,
        expected: &EncryptedObject,
        credential: &VaultCredential,
        deadline: Option<VaultDeadline>,
    ) -> Result<ObjectMetadata, WebDavError> {
        let Some((actual, metadata)) = self
            .get_inner(key, credential, MAX_PROTECTED_PAYLOAD_BYTES, deadline)
            .await?
        else {
            return Err(WebDavError::AmbiguousWrite);
        };
        let expected_hash = sha256_domain_hex(READBACK_HASH_DOMAIN, &[expected.as_bytes()]);
        let actual_hash = sha256_domain_hex(READBACK_HASH_DOMAIN, &[actual.as_bytes()]);
        if expected_hash != actual_hash {
            return Err(WebDavError::AmbiguousWrite);
        }
        Ok(metadata)
    }

    async fn verify_ambiguous_put(
        &self,
        key: &ObjectKey,
        expected: &EncryptedObject,
        credential: &VaultCredential,
        deadline: Option<VaultDeadline>,
    ) -> Result<ObjectWriteResult, WebDavError> {
        match self
            .verify_present(key, expected, credential, deadline)
            .await
        {
            Ok(metadata) => Ok(ObjectWriteResult::AlreadyPresent(metadata)),
            Err(
                error @ (WebDavError::AuthenticationRequired
                | WebDavError::PermissionDenied
                | WebDavError::RequestFailed
                | WebDavError::MethodUnsupported
                | WebDavError::Locked
                | WebDavError::RateLimited
                | WebDavError::ServerUnavailable),
            ) => Err(error),
            Err(_) => Err(WebDavError::AmbiguousWrite),
        }
    }
}

/// Blocking adapter used by the manual-sync worker and transport conformance tests.
///
/// It makes one explicit, zeroizing copy of the non-clonable credential without implementing
/// `Debug` or `Clone`, and drives reqwest on a private current-thread runtime protected from
/// concurrent `block_on` calls. Callers use it from a blocking worker, never from an async executor
/// thread.
pub struct BlockingWebDavTransport {
    client: WebDavClient,
    credential: VaultCredential,
    runtime: Mutex<tokio::runtime::Runtime>,
}

impl BlockingWebDavTransport {
    pub fn new(endpoint: &str, credential: &VaultCredential) -> Result<Self, WebDavError> {
        Self::with_custom_ca(endpoint, None, credential)
    }

    pub fn with_custom_ca(
        endpoint: &str,
        custom_ca_pem: Option<&[u8]>,
        credential: &VaultCredential,
    ) -> Result<Self, WebDavError> {
        let client = WebDavClient::with_custom_ca(endpoint, custom_ca_pem)?;
        let credential = copy_credential(credential)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .map_err(|_| WebDavError::RequestFailed)?;
        Ok(Self {
            client,
            credential,
            runtime: Mutex::new(runtime),
        })
    }

    /// Perform the state-changing connection test used only before a profile is trusted.
    ///
    /// Besides OPTIONS and bounded PROPFIND, this proves both conditional-create and
    /// conditional-update behavior against a dedicated encrypted marker. Normal synchronization
    /// must not call this method because the valid If-Match proof intentionally rewrites that
    /// marker.
    pub fn probe_capabilities(&self) -> Result<WebDavCapabilities, WebDavError> {
        let capabilities = self.block_on(self.client.options(&self.credential))?;
        if capabilities.supports_encrypted_sync() {
            let key =
                ObjectKey::new(CONDITIONAL_PROBE_KEY).map_err(|_| WebDavError::InvalidResponse)?;
            let object = conditional_probe_object()?;
            self.block_on(async {
                self.ensure_ancestor_collections(&key, None).await?;
                self.client
                    .probe_conditional_writes(&key, &object, &self.credential)
                    .await?;
                let collection = ObjectKey::new("yututui/v2/capability")
                    .map_err(|_| WebDavError::InvalidResponse)?;
                self.client
                    .propfind_bounded(
                        &collection,
                        PropfindDepth::One,
                        &self.credential,
                        CAPABILITY_PROPFIND_RESOURCES,
                        CAPABILITY_PROPFIND_BYTES,
                    )
                    .await?;
                Ok(())
            })?;
        }
        Ok(capabilities)
    }

    /// Ensure one collection below the configured vault root exists.
    pub fn ensure_collection(&self, key: &ObjectKey) -> Result<CollectionWriteResult, WebDavError> {
        self.block_on(self.client.mkcol(key, &self.credential))
    }

    fn block_on<T>(
        &self,
        future: impl Future<Output = Result<T, WebDavError>>,
    ) -> Result<T, WebDavError> {
        let runtime = self
            .runtime
            .lock()
            .map_err(|_| WebDavError::RequestFailed)?;
        runtime.block_on(future)
    }

    async fn ensure_ancestor_collections(
        &self,
        key: &ObjectKey,
        deadline: Option<VaultDeadline>,
    ) -> Result<(), WebDavError> {
        let segments = key.as_str().split('/').collect::<Vec<_>>();
        let mut path = String::new();
        for segment in segments.iter().take(segments.len().saturating_sub(1)) {
            if !path.is_empty() {
                path.push('/');
            }
            path.push_str(segment);
            let collection =
                ObjectKey::new(path.clone()).map_err(|_| WebDavError::InvalidResponse)?;
            if let Some(deadline) = deadline {
                self.client
                    .mkcol_with_deadline(&collection, &self.credential, deadline)
                    .await?;
            } else {
                self.client.mkcol(&collection, &self.credential).await?;
            }
        }
        Ok(())
    }

    async fn list_bounded(
        &self,
        prefix: &ObjectKey,
        limits: ListLimits,
        deadline: VaultDeadline,
    ) -> Result<ListOutcome, WebDavError> {
        limits
            .validate()
            .map_err(|_| WebDavError::ResourceLimitExceeded)?;
        check_list_deadline(deadline)?;
        if limits.returned_objects > MAX_PROPFIND_RESOURCES
            || limits.scanned_resources > MAX_PROPFIND_RESOURCES
        {
            return Err(WebDavError::ResourceLimitExceeded);
        }
        let request_limit = limits.requests.min(MAX_LIST_PROPFIND_REQUESTS);
        let collection_limit = limits.scanned_collections.min(MAX_LIST_PROPFIND_REQUESTS);
        let response_byte_limit = limits.response_bytes.min(MAX_PROPFIND_BYTES);
        let mut pending = VecDeque::from([(prefix.clone(), 0_usize)]);
        let mut visited_collections = BTreeSet::new();
        let mut files = BTreeMap::<ObjectKey, ObjectMetadata>::new();
        let mut scanned_resources = 0_usize;
        let mut scanned_bytes = 0_usize;
        let mut requests = 0_usize;

        while let Some((collection, depth)) = pending.pop_front() {
            check_list_deadline(deadline)?;
            if !visited_collections.insert(collection.clone()) {
                continue;
            }
            if visited_collections.len() > collection_limit {
                return Err(WebDavError::ResourceLimitExceeded);
            }
            requests = next_list_request(requests, request_limit)?;
            let remaining = limits
                .scanned_resources
                .checked_sub(scanned_resources)
                .filter(|remaining| *remaining > 0)
                .ok_or(WebDavError::ResourceLimitExceeded)?;
            let remaining_bytes = response_byte_limit
                .checked_sub(scanned_bytes)
                .filter(|remaining| *remaining > 0)
                .ok_or(WebDavError::ResourceLimitExceeded)?;
            let remaining_time = deadline
                .remaining()
                .map_err(|_| WebDavError::RequestFailed)?;
            let (resources, response_bytes) = tokio::time::timeout(
                remaining_time,
                self.client.propfind_bounded(
                    &collection,
                    PropfindDepth::One,
                    &self.credential,
                    remaining,
                    remaining_bytes,
                ),
            )
            .await
            .map_err(|_| WebDavError::RequestFailed)??;
            check_list_deadline(deadline)?;
            scanned_bytes = scanned_bytes
                .checked_add(response_bytes)
                .filter(|bytes| *bytes <= response_byte_limit)
                .ok_or(WebDavError::ResourceLimitExceeded)?;
            scanned_resources = scanned_resources
                .checked_add(resources.len())
                .filter(|count| *count <= limits.scanned_resources)
                .ok_or(WebDavError::ResourceLimitExceeded)?;

            for resource in resources {
                let Some(key) = resource.key else {
                    continue;
                };
                if !is_key_within(prefix, &key) {
                    return Err(WebDavError::InvalidResponse);
                }
                if resource.is_collection {
                    if key != collection && !visited_collections.contains(&key) {
                        let child_depth = next_list_depth(depth)?;
                        pending.push_back((key, child_depth));
                    }
                    continue;
                }
                let etag = resource.etag.ok_or(WebDavError::MissingStrongEntityTag)?;
                etag.require_strong()?;
                let content_length = resource
                    .content_length
                    .ok_or(WebDavError::InvalidResponse)?;
                let metadata = ObjectMetadata {
                    key: key.clone(),
                    etag: etag.as_str().to_owned(),
                    content_length,
                };
                match files.get(&key) {
                    Some(existing) if existing != &metadata => {
                        return Err(WebDavError::InvalidResponse);
                    }
                    Some(_) => {}
                    None if files.len() == limits.returned_objects => {
                        return Err(WebDavError::ResourceLimitExceeded);
                    }
                    None => {
                        files.insert(key, metadata);
                    }
                }
            }
        }
        let returned_objects = files.len();
        let outcome = ListOutcome {
            objects: files.into_values().collect(),
            cost: ListCost {
                requests,
                response_bytes: scanned_bytes,
                scanned_collections: visited_collections.len(),
                scanned_resources,
                returned_objects,
            },
        };
        outcome
            .validate(limits)
            .map_err(|_| WebDavError::ResourceLimitExceeded)?;
        Ok(outcome)
    }
}

impl VaultTransport for BlockingWebDavTransport {
    fn get(
        &self,
        key: &ObjectKey,
        max_bytes: usize,
    ) -> Result<Option<(EncryptedObject, ObjectMetadata)>, VaultError> {
        self.block_on(self.client.get(key, &self.credential, max_bytes))
            .map_err(|error| map_vault_error(error, TransportOperation::Get))
    }

    fn get_with_deadline(
        &self,
        key: &ObjectKey,
        max_bytes: usize,
        deadline: VaultDeadline,
    ) -> Result<Option<(EncryptedObject, ObjectMetadata)>, VaultError> {
        self.block_on(
            self.client
                .get_with_deadline(key, &self.credential, max_bytes, deadline),
        )
        .map_err(|error| map_vault_error(error, TransportOperation::Get))
    }

    fn put(
        &self,
        key: &ObjectKey,
        object: &EncryptedObject,
        condition: ObjectCondition,
    ) -> Result<ObjectWriteResult, VaultError> {
        self.block_on(async {
            self.ensure_ancestor_collections(key, None).await?;
            self.client
                .put(key, object, condition, &self.credential)
                .await
        })
        .map_err(|error| map_vault_error(error, TransportOperation::Put))
    }

    fn put_with_deadline(
        &self,
        key: &ObjectKey,
        object: &EncryptedObject,
        condition: ObjectCondition,
        deadline: VaultDeadline,
    ) -> Result<ObjectWriteResult, VaultError> {
        self.block_on(async {
            self.ensure_ancestor_collections(key, Some(deadline))
                .await?;
            self.client
                .put_with_deadline(key, object, condition, &self.credential, deadline)
                .await
        })
        .map_err(|error| map_vault_error(error, TransportOperation::Put))
    }

    fn list(
        &self,
        prefix: &ObjectKey,
        max_resources: usize,
    ) -> Result<Vec<ObjectMetadata>, VaultError> {
        self.list_with_limits(
            prefix,
            ListLimits::for_returned_objects(max_resources),
            VaultDeadline::from_now(DIRECT_LIST_DEADLINE),
        )
        .map(|outcome| outcome.objects)
    }

    fn list_with_limits(
        &self,
        prefix: &ObjectKey,
        limits: ListLimits,
        deadline: VaultDeadline,
    ) -> Result<ListOutcome, VaultError> {
        self.block_on(self.list_bounded(prefix, limits, deadline))
            .map_err(|error| map_vault_error(error, TransportOperation::List))
    }
}

#[derive(Clone, Copy)]
enum TransportOperation {
    Get,
    Put,
    List,
}

fn map_vault_error(error: WebDavError, operation: TransportOperation) -> VaultError {
    match error {
        WebDavError::PreconditionFailed => VaultError::PreconditionFailed,
        WebDavError::AuthenticationRequired | WebDavError::PermissionDenied => {
            VaultError::RemoteAuthentication
        }
        WebDavError::CertificateFailed => VaultError::RemoteCertificate,
        WebDavError::MethodUnsupported => VaultError::RemoteUnsupported,
        WebDavError::RequestFailed
        | WebDavError::Locked
        | WebDavError::RateLimited
        | WebDavError::ServerUnavailable
        | WebDavError::AmbiguousWrite => VaultError::RemoteUnavailable,
        WebDavError::ResourceLimitExceeded => VaultError::ResourceLimitExceeded,
        WebDavError::ResponseTooLarge if matches!(operation, TransportOperation::List) => {
            VaultError::ResourceLimitExceeded
        }
        WebDavError::ResponseTooLarge => VaultError::PayloadTooLarge,
        WebDavError::InvalidEncryptedObject => VaultError::InvalidEncryptedObject,
        _ => VaultError::StorageFailed,
    }
}

fn is_key_within(prefix: &ObjectKey, candidate: &ObjectKey) -> bool {
    candidate == prefix
        || candidate
            .as_str()
            .strip_prefix(prefix.as_str())
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn next_list_request(current: usize, limit: usize) -> Result<usize, WebDavError> {
    current
        .checked_add(1)
        .filter(|requests| *requests <= limit)
        .ok_or(WebDavError::ResourceLimitExceeded)
}

async fn await_with_deadline<T>(
    deadline: Option<VaultDeadline>,
    future: impl Future<Output = Result<T, WebDavError>>,
) -> Result<T, WebDavError> {
    let Some(deadline) = deadline else {
        return future.await;
    };
    let remaining = deadline
        .remaining()
        .map_err(|_| WebDavError::RequestFailed)?
        .min(REQUEST_TIMEOUT);
    tokio::time::timeout(remaining, future)
        .await
        .map_err(|_| WebDavError::RequestFailed)?
}

fn check_list_deadline(deadline: VaultDeadline) -> Result<(), WebDavError> {
    deadline.check().map_err(|_| WebDavError::RequestFailed)
}

fn next_list_depth(current: usize) -> Result<usize, WebDavError> {
    current
        .checked_add(1)
        .filter(|depth| *depth <= MAX_LIST_DEPTH)
        .ok_or(WebDavError::ResourceLimitExceeded)
}

fn copy_credential(credential: &VaultCredential) -> Result<VaultCredential, WebDavError> {
    let secret = SecretString::from(credential.secret().expose_secret().to_owned());
    match credential.kind() {
        VaultCredentialKind::Password => {
            let username = credential
                .username()
                .ok_or(WebDavError::AuthenticationRequired)?
                .expose_secret()
                .to_owned();
            VaultCredential::password(username, secret).map_err(|_| WebDavError::InvalidResponse)
        }
        VaultCredentialKind::BearerToken => {
            VaultCredential::bearer_token(secret).map_err(|_| WebDavError::InvalidResponse)
        }
    }
}

fn conditional_probe_object() -> Result<EncryptedObject, WebDavError> {
    let device = DeviceSecretMaterial::generate_for("webdav-capability-probe")
        .map_err(|_| WebDavError::RequestFailed)?;
    let recipient = device.public_identity().age_recipient;
    encrypt_json_to_recipients(
        &serde_json::json!({
            "kind": "yututui_webdav_conditional_probe",
            "schema_version": 1,
        }),
        &[recipient],
    )
    .map_err(|_| WebDavError::RequestFailed)
}

fn normalize_endpoint(endpoint: &str) -> Result<Url, WebDavError> {
    let endpoint = endpoint.trim();
    if endpoint.is_empty()
        || endpoint.len() > MAX_ENDPOINT_BYTES
        || endpoint.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(WebDavError::InvalidEndpoint);
    }
    let mut url = Url::parse(endpoint).map_err(|_| WebDavError::InvalidEndpoint)?;
    if url.host_str().is_none() {
        return Err(WebDavError::InvalidEndpoint);
    }
    match url.scheme() {
        "https" => {}
        "http" if is_loopback_url(&url) => {}
        _ => return Err(WebDavError::UnsupportedScheme),
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(WebDavError::EndpointCredentials);
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(WebDavError::InvalidEndpoint);
    }
    if !url.path().ends_with('/') {
        let mut path = url.path().to_owned();
        path.push('/');
        url.set_path(&path);
    }
    Ok(url)
}

fn is_loopback_url(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

fn validate_redirect(base: &Url, target: &Url) -> Result<(), WebDavError> {
    if target.origin() != base.origin() {
        return Err(WebDavError::CrossOriginRedirect);
    }
    if !target.username().is_empty()
        || target.password().is_some()
        || target.query().is_some()
        || target.fragment().is_some()
    {
        return Err(WebDavError::InvalidRedirect);
    }
    Ok(())
}

fn apply_condition(
    headers: &mut HeaderMap,
    condition: &ObjectCondition,
) -> Result<(), WebDavError> {
    match condition {
        ObjectCondition::CreateOnly => {
            headers.insert(IF_NONE_MATCH, HeaderValue::from_static("*"));
        }
        ObjectCondition::Match(raw) => {
            let etag = EntityTag::parse(raw)?;
            etag.require_strong()?;
            let value =
                HeaderValue::from_str(etag.as_str()).map_err(|_| WebDavError::InvalidEntityTag)?;
            headers.insert(IF_MATCH, value);
        }
    }
    Ok(())
}

fn parse_capabilities(headers: &HeaderMap) -> Result<WebDavCapabilities, WebDavError> {
    let mut result = WebDavCapabilities {
        options: true,
        ..WebDavCapabilities::default()
    };
    for value in bounded_header_values(headers, &DAV_HEADER)? {
        for token in value.split(',').map(str::trim) {
            result.dav_class_1 |= token == "1";
            result.dav_class_2 |= token == "2";
        }
    }
    for value in bounded_header_values(headers, &ALLOW)? {
        for method in value.split(',').map(str::trim) {
            result.propfind |= method.eq_ignore_ascii_case("PROPFIND");
            result.mkcol |= method.eq_ignore_ascii_case("MKCOL");
            result.get |= method.eq_ignore_ascii_case("GET");
            result.put |= method.eq_ignore_ascii_case("PUT");
            result.options |= method.eq_ignore_ascii_case("OPTIONS");
        }
    }
    Ok(result)
}

fn bounded_header_values<'a>(
    headers: &'a HeaderMap,
    name: &HeaderName,
) -> Result<Vec<&'a str>, WebDavError> {
    let mut total = 0_usize;
    let mut values = Vec::new();
    for value in headers.get_all(name) {
        let value = value.to_str().map_err(|_| WebDavError::InvalidResponse)?;
        total = total
            .checked_add(value.len())
            .ok_or(WebDavError::InvalidResponse)?;
        if total > MAX_PROTOCOL_HEADER_BYTES {
            return Err(WebDavError::ResponseTooLarge);
        }
        values.push(value);
    }
    Ok(values)
}

fn optional_etag(headers: &HeaderMap) -> Result<Option<EntityTag>, WebDavError> {
    let mut values = headers.get_all(ETAG).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(WebDavError::InvalidEntityTag);
    }
    value
        .to_str()
        .map_err(|_| WebDavError::InvalidEntityTag)
        .and_then(EntityTag::parse)
        .map(Some)
}

fn strong_etag(headers: &HeaderMap) -> Result<EntityTag, WebDavError> {
    let etag = optional_etag(headers)?.ok_or(WebDavError::MissingStrongEntityTag)?;
    etag.require_strong()?;
    Ok(etag)
}

fn encrypted_body_limit(max_plaintext_bytes: usize) -> Result<usize, WebDavError> {
    if max_plaintext_bytes == 0 || max_plaintext_bytes > MAX_PROTECTED_PAYLOAD_BYTES {
        return Err(WebDavError::ResourceLimitExceeded);
    }
    Ok(max_plaintext_bytes
        .saturating_add(MAX_ENCRYPTED_OBJECT_BYTES - MAX_PROTECTED_PAYLOAD_BYTES)
        .min(MAX_ENCRYPTED_OBJECT_BYTES))
}

async fn read_limited(mut response: Response, max_bytes: usize) -> Result<Vec<u8>, WebDavError> {
    if let Some(length) = response.content_length()
        && length > max_bytes as u64
    {
        return Err(WebDavError::ResponseTooLarge);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| classify_request_error(&error))?
    {
        if bytes.len().saturating_add(chunk.len()) > max_bytes {
            return Err(WebDavError::ResponseTooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn classify_request_error(error: &reqwest::Error) -> WebDavError {
    let mut source = error.source();
    while let Some(current) = source {
        if current.is::<native_tls::Error>() {
            return WebDavError::CertificateFailed;
        }
        source = current.source();
    }
    WebDavError::RequestFailed
}

fn status_error(status: StatusCode) -> WebDavError {
    match status {
        StatusCode::UNAUTHORIZED => WebDavError::AuthenticationRequired,
        StatusCode::FORBIDDEN => WebDavError::PermissionDenied,
        StatusCode::NOT_FOUND => WebDavError::NotFound,
        StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED => {
            WebDavError::MethodUnsupported
        }
        StatusCode::CONFLICT => WebDavError::Conflict,
        StatusCode::PRECONDITION_FAILED => WebDavError::PreconditionFailed,
        StatusCode::LOCKED => WebDavError::Locked,
        StatusCode::TOO_MANY_REQUESTS => WebDavError::RateLimited,
        status if status.is_server_error() => WebDavError::ServerUnavailable,
        status => WebDavError::UnexpectedStatus(status.as_u16()),
    }
}

#[cfg(test)]
mod proxy_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
pub(crate) mod tls_tests;
