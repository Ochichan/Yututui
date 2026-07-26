//! Exact-origin network policy for a user-configured private music server.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use reqwest::Url;

const MAX_ORIGIN_BYTES: usize = 4_096;
const MAX_CUSTOM_CA_BYTES: usize = 192 * 1024;
const DNS_TIMEOUT: Duration = Duration::from_secs(3);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
const READ_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginError {
    InvalidOrigin,
    InsecureOrigin,
    ResolutionFailed,
    DestinationRejected,
    InvalidCertificate,
    ClientBuildFailed,
    RedirectRejected,
}

impl std::fmt::Display for OriginError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::InvalidOrigin => "invalid server origin",
            Self::InsecureOrigin => "insecure server origin is not allowed",
            Self::ResolutionFailed => "server name could not be resolved",
            Self::DestinationRejected => "server destination is not allowed",
            Self::InvalidCertificate => "custom certificate authority is invalid",
            Self::ClientBuildFailed => "secure HTTP client could not be created",
            Self::RedirectRejected => "server redirect was rejected",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for OriginError {}

/// A configured origin is intentionally not `Debug`: endpoints must not enter diagnostics.
#[derive(Clone, PartialEq, Eq)]
pub struct ConfiguredPrivateOrigin {
    base_url: Url,
    allow_lan_http: bool,
}

impl ConfiguredPrivateOrigin {
    pub fn new(raw: &str, allow_lan_http: bool) -> Result<Self, OriginError> {
        if raw.is_empty()
            || raw.len() > MAX_ORIGIN_BYTES
            || raw.chars().any(is_forbidden_text_character)
        {
            return Err(OriginError::InvalidOrigin);
        }
        let mut base_url = Url::parse(raw).map_err(|_| OriginError::InvalidOrigin)?;
        if !base_url.username().is_empty()
            || base_url.password().is_some()
            || base_url.query().is_some()
            || base_url.fragment().is_some()
            || base_url.host_str().is_none()
        {
            return Err(OriginError::InvalidOrigin);
        }
        match base_url.scheme() {
            "https" => {}
            "http" if allow_lan_http => {}
            "http" => return Err(OriginError::InsecureOrigin),
            _ => return Err(OriginError::InvalidOrigin),
        }
        if !base_url.path().ends_with('/') {
            let path = format!("{}/", base_url.path());
            base_url.set_path(&path);
        }
        Ok(Self {
            base_url,
            allow_lan_http,
        })
    }

    pub fn uses_lan_http(&self) -> bool {
        self.base_url.scheme() == "http"
    }

    pub(crate) fn canonical(&self) -> &str {
        self.base_url.as_str()
    }

    pub(crate) fn endpoint(&self, method: &str) -> Result<Url, OriginError> {
        if method.is_empty()
            || !method
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return Err(OriginError::InvalidOrigin);
        }
        self.base_url
            .join(&format!("rest/{method}.view"))
            .map_err(|_| OriginError::InvalidOrigin)
    }

    pub(crate) fn native_endpoint(&self, path: &str) -> Result<Url, OriginError> {
        if path.is_empty()
            || path.len() > MAX_ORIGIN_BYTES
            || path.starts_with('/')
            || !path.split('/').all(|segment| {
                !segment.is_empty()
                    && segment
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            })
        {
            return Err(OriginError::InvalidOrigin);
        }
        self.base_url
            .join(path)
            .map_err(|_| OriginError::InvalidOrigin)
    }

    pub async fn build_pinned_client(
        &self,
        custom_ca_pem: Option<&[u8]>,
    ) -> Result<PinnedOriginClient, OriginError> {
        let host = self.base_url.host_str().ok_or(OriginError::InvalidOrigin)?;
        let port = self
            .base_url
            .port_or_known_default()
            .ok_or(OriginError::InvalidOrigin)?;
        let addresses = resolve_addresses(host, port).await?;
        if addresses.is_empty() {
            return Err(OriginError::ResolutionFailed);
        }
        let lan_http = self.base_url.scheme() == "http";
        if addresses
            .iter()
            .any(|address| !is_acceptable_destination(address.ip(), lan_http))
        {
            return Err(OriginError::DestinationRejected);
        }

        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .user_agent("yututui-open-subsonic/1")
            .resolve_to_addrs(host, &addresses);
        if let Some(pem) = custom_ca_pem {
            if pem.is_empty() || pem.len() > MAX_CUSTOM_CA_BYTES {
                return Err(OriginError::InvalidCertificate);
            }
            let certificates = reqwest::Certificate::from_pem_bundle(pem)
                .map_err(|_| OriginError::InvalidCertificate)?;
            if certificates.is_empty() {
                return Err(OriginError::InvalidCertificate);
            }
            for certificate in certificates {
                builder = builder.add_root_certificate(certificate);
            }
        }
        let client = builder
            .build()
            .map_err(|_| OriginError::ClientBuildFailed)?;
        Ok(PinnedOriginClient {
            client,
            origin: self.clone(),
        })
    }

    pub(crate) fn same_origin(&self, candidate: &Url) -> bool {
        self.base_url.origin() == candidate.origin()
    }

    pub(crate) fn validate_redirect(
        &self,
        current: &Url,
        location: &str,
    ) -> Result<Url, OriginError> {
        if location.is_empty()
            || location.len() > MAX_ORIGIN_BYTES
            || location.chars().any(is_forbidden_text_character)
        {
            return Err(OriginError::RedirectRejected);
        }
        let target = current
            .join(location)
            .map_err(|_| OriginError::RedirectRejected)?;
        if !target.username().is_empty()
            || target.password().is_some()
            || target.query().is_some()
            || target.fragment().is_some()
            || !self.same_origin(&target)
        {
            return Err(OriginError::RedirectRejected);
        }
        Ok(target)
    }
}

/// A client whose DNS answers and proxy/redirect policy were fixed at construction time.
#[derive(Clone)]
pub struct PinnedOriginClient {
    client: reqwest::Client,
    origin: ConfiguredPrivateOrigin,
}

impl PinnedOriginClient {
    pub(crate) fn client(&self) -> &reqwest::Client {
        &self.client
    }

    pub(crate) fn origin(&self) -> &ConfiguredPrivateOrigin {
        &self.origin
    }
}

pub fn custom_ca_fingerprint(pem: &[u8]) -> Result<String, OriginError> {
    if pem.is_empty() || pem.len() > MAX_CUSTOM_CA_BYTES {
        return Err(OriginError::InvalidCertificate);
    }
    let certificates =
        reqwest::Certificate::from_pem_bundle(pem).map_err(|_| OriginError::InvalidCertificate)?;
    if certificates.is_empty() {
        return Err(OriginError::InvalidCertificate);
    }
    use data_encoding::HEXLOWER;
    use sha2::{Digest as _, Sha256};
    Ok(HEXLOWER.encode(&Sha256::digest(pem)))
}

async fn resolve_addresses(host: &str, port: u16) -> Result<Vec<SocketAddr>, OriginError> {
    let mut addresses = if let Ok(address) = host.parse::<IpAddr>() {
        vec![SocketAddr::new(address, port)]
    } else {
        tokio::time::timeout(DNS_TIMEOUT, tokio::net::lookup_host((host, port)))
            .await
            .map_err(|_| OriginError::ResolutionFailed)?
            .map_err(|_| OriginError::ResolutionFailed)?
            .collect::<Vec<_>>()
    };
    addresses.sort_unstable();
    addresses.dedup();
    Ok(addresses)
}

fn is_acceptable_destination(address: IpAddr, lan_http: bool) -> bool {
    if lan_http {
        return match address {
            IpAddr::V4(address) => address.is_loopback() || address.is_private(),
            IpAddr::V6(address) => address.is_loopback() || is_ipv6_unique_local(address),
        };
    }
    match address {
        IpAddr::V4(address) => {
            !address.is_unspecified()
                && !address.is_broadcast()
                && !address.is_multicast()
                && address != Ipv4Addr::new(255, 255, 255, 255)
        }
        IpAddr::V6(address) => !address.is_unspecified() && !address.is_multicast(),
    }
}

fn is_ipv6_unique_local(address: Ipv6Addr) -> bool {
    address.octets()[0] & 0xfe == 0xfc
}

fn is_forbidden_text_character(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{200b}'
                | '\u{200c}'
                | '\u{200d}'
                | '\u{200e}'
                | '\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
                | '\u{feff}'
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_a_safe_origin_without_exposing_url_parts() {
        let origin =
            ConfiguredPrivateOrigin::new("https://music.example.test/navidrome", false).unwrap();
        assert_eq!(origin.canonical(), "https://music.example.test/navidrome/");
        assert_eq!(
            origin.endpoint("search3").unwrap().as_str(),
            "https://music.example.test/navidrome/rest/search3.view"
        );
        assert_eq!(
            origin.native_endpoint("auth/login").unwrap().as_str(),
            "https://music.example.test/navidrome/auth/login"
        );
        assert_eq!(
            origin.native_endpoint("api/scrobble").unwrap().as_str(),
            "https://music.example.test/navidrome/api/scrobble"
        );
        for unsafe_path in [
            "",
            "/auth/login",
            "../auth/login",
            "api//scrobble",
            "api/scrobble?token=secret",
        ] {
            assert!(
                origin.native_endpoint(unsafe_path).is_err(),
                "{unsafe_path}"
            );
        }
    }

    #[test]
    fn rejects_userinfo_query_fragment_and_unapproved_http() {
        for raw in [
            "https://user:secret@example.test/",
            "https://example.test/?token=secret",
            "https://example.test/#fragment",
            "ftp://example.test/",
            "https://example.test/\u{202e}",
        ] {
            assert!(ConfiguredPrivateOrigin::new(raw, false).is_err(), "{raw}");
        }
        assert!(ConfiguredPrivateOrigin::new("http://127.0.0.1:4040/", false).is_err());
    }

    #[tokio::test]
    async fn lan_http_only_accepts_private_or_loopback_destinations() {
        assert!(
            ConfiguredPrivateOrigin::new("http://127.0.0.1:4040/", true)
                .unwrap()
                .build_pinned_client(None)
                .await
                .is_ok()
        );
        assert!(
            ConfiguredPrivateOrigin::new("http://192.0.2.1:4040/", true)
                .unwrap()
                .build_pinned_client(None)
                .await
                .is_err()
        );
    }

    #[test]
    fn redirects_must_remain_on_the_exact_origin_without_parameters() {
        let origin =
            ConfiguredPrivateOrigin::new("https://music.example.test/base/", false).unwrap();
        let current = origin.endpoint("ping").unwrap();
        assert_eq!(
            origin
                .validate_redirect(&current, "/base/rest/ping2.view")
                .unwrap()
                .path(),
            "/base/rest/ping2.view"
        );
        assert!(
            origin
                .validate_redirect(&current, "https://other.example.test/ping")
                .is_err()
        );
        assert!(
            origin
                .validate_redirect(&current, "/ping?apiKey=secret")
                .is_err()
        );
    }
}
