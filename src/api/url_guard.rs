use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use reqwest::header::{LOCATION, RANGE};
use reqwest::{Method, StatusCode, Url};

use super::{PlayableUrlError, validate_playable_url};
use crate::search_source::SearchSource;

const DNS_TIMEOUT: Duration = Duration::from_secs(3);
const PROBE_TIMEOUT: Duration = Duration::from_secs(4);
const MAX_REDIRECTS: usize = 5;

/// Validate an arbitrary playable URL immediately before handing it to an external network
/// client. Generated YouTube/googlevideo URLs stay on the cheaper string policy; arbitrary
/// provider, radio, and pre-resolved CDN URLs must resolve only to public destinations and may
/// not redirect into private/local networks.
pub async fn validate_playable_url_destination(
    source: SearchSource,
    raw: &str,
) -> Result<String, PlayableUrlError> {
    let clean = validate_playable_url(source, raw)?;
    let url = Url::parse(&clean).map_err(|e| PlayableUrlError::Invalid(e.to_string()))?;
    if is_trusted_generated_url(source, &url) {
        return Ok(clean);
    }
    validate_url_destination(&url).await?;
    follow_redirects(url).await.map(|url| url.to_string())
}

/// Player boundary guard. Local filesystem paths are allowed through; remote HTTP(S) targets get
/// destination validation unless they are app-generated YouTube/googlevideo URLs.
pub async fn validate_playback_target_for_handoff(raw: &str) -> Result<String, PlayableUrlError> {
    let trimmed = raw.trim();
    let Ok(url) = Url::parse(trimmed) else {
        return Ok(raw.to_owned());
    };
    match url.scheme() {
        "http" | "https" => {
            if is_trusted_handoff_url(&url) {
                return validate_playable_url(SearchSource::Youtube, trimmed);
            }
            validate_playable_url_destination(SearchSource::All, trimmed).await
        }
        scheme => {
            #[cfg(windows)]
            if is_windows_drive_path(trimmed) {
                return Ok(raw.to_owned());
            }
            Err(PlayableUrlError::UnsupportedScheme(scheme.to_owned()))
        }
    }
}

async fn follow_redirects(mut url: Url) -> Result<Url, PlayableUrlError> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(PROBE_TIMEOUT)
        .build()
        .map_err(|e| PlayableUrlError::ProbeFailed(e.to_string()))?;

    for _ in 0..MAX_REDIRECTS {
        validate_url_destination(&url).await?;
        let Some(next) = probe_redirect(&client, &url).await? else {
            return Ok(url);
        };
        url = next;
    }
    Err(PlayableUrlError::RedirectLimit { max: MAX_REDIRECTS })
}

async fn probe_redirect(
    client: &reqwest::Client,
    url: &Url,
) -> Result<Option<Url>, PlayableUrlError> {
    let response = match client.request(Method::HEAD, url.clone()).send().await {
        Ok(response) if response.status() == StatusCode::METHOD_NOT_ALLOWED => {
            range_probe(client, url).await?
        }
        Ok(response) => response,
        Err(_) => range_probe(client, url).await?,
    };
    if !response.status().is_redirection() {
        return Ok(None);
    }
    let location = response
        .headers()
        .get(LOCATION)
        .ok_or(PlayableUrlError::RedirectMissingLocation)?
        .to_str()
        .map_err(|e| PlayableUrlError::RedirectInvalid(e.to_string()))?;
    let next = redirect_target(url, location)?;
    let clean = validate_playable_url(SearchSource::All, next.as_str())?;
    Url::parse(&clean)
        .map(Some)
        .map_err(|e| PlayableUrlError::RedirectInvalid(e.to_string()))
}

async fn range_probe(
    client: &reqwest::Client,
    url: &Url,
) -> Result<reqwest::Response, PlayableUrlError> {
    client
        .request(Method::GET, url.clone())
        .header(RANGE, "bytes=0-0")
        .send()
        .await
        .map_err(|e| PlayableUrlError::ProbeFailed(e.to_string()))
}

fn redirect_target(base: &Url, location: &str) -> Result<Url, PlayableUrlError> {
    base.join(location)
        .map_err(|e| PlayableUrlError::RedirectInvalid(e.to_string()))
}

async fn validate_url_destination(url: &Url) -> Result<(), PlayableUrlError> {
    let host = url.host_str().ok_or(PlayableUrlError::MissingHost)?;
    let normalized_host = host.trim_end_matches('.').to_ascii_lowercase();
    let ip_host = normalized_host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(&normalized_host);
    if let Ok(ip) = ip_host.parse::<IpAddr>() {
        return validate_resolved_ips(host, [ip]);
    }
    let port = url.port_or_known_default().ok_or_else(|| {
        PlayableUrlError::ProbeFailed("URL has no default port for destination check".to_owned())
    })?;
    let addrs = tokio::time::timeout(DNS_TIMEOUT, tokio::net::lookup_host((host, port)))
        .await
        .map_err(|_| PlayableUrlError::DnsResolution {
            host: host.to_owned(),
        })?
        .map_err(|_| PlayableUrlError::DnsResolution {
            host: host.to_owned(),
        })?;
    let ips = addrs.map(|addr| addr.ip()).collect::<Vec<_>>();
    validate_resolved_ips(host, ips)
}

fn validate_resolved_ips<I>(host: &str, ips: I) -> Result<(), PlayableUrlError>
where
    I: IntoIterator<Item = IpAddr>,
{
    let mut saw_any = false;
    for ip in ips {
        saw_any = true;
        if is_blocked_playable_ip(ip) {
            return Err(PlayableUrlError::DestinationBlockedIp {
                host: host.to_owned(),
                ip: ip.to_string(),
            });
        }
    }
    if saw_any {
        Ok(())
    } else {
        Err(PlayableUrlError::DnsResolution {
            host: host.to_owned(),
        })
    }
}

pub(super) fn is_blocked_playable_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_blocked_ipv4(ip),
        IpAddr::V6(ip) => is_blocked_ipv6(ip),
    }
}

fn is_blocked_ipv4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || octets[0] == 0
        || (octets[0] == 100 && (octets[1] & 0b1100_0000) == 64)
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 2)
        || (octets[0] == 192 && octets[1] == 88 && octets[2] == 99)
        || (octets[0] == 198 && (octets[1] == 18 || octets[1] == 19))
        || (octets[0] == 198 && octets[1] == 51 && octets[2] == 100)
        || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113)
        || octets[0] >= 240
}

fn is_blocked_ipv6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_unique_local()
        || ip.is_unicast_link_local()
        || ip.is_multicast()
        || ip.to_ipv4_mapped().is_some_and(is_blocked_ipv4)
        || (segments[0] == 0x0100 && segments[1] == 0 && segments[2] == 0 && segments[3] == 0)
        || (segments[0] == 0x0064 && segments[1] == 0xff9b && segments[2] == 0x0001)
        || (segments[0] == 0x2001 && segments[1] < 0x0200)
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
        || segments[0] == 0x2002
        || (segments[0] == 0x3fff && (segments[1] & 0xf000) == 0)
}

fn is_trusted_generated_url(source: SearchSource, url: &Url) -> bool {
    matches!(source, SearchSource::Youtube) && is_trusted_handoff_url(url)
}

fn is_trusted_handoff_url(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == "youtu.be"
        || host == "youtube.com"
        || host.ends_with(".youtube.com")
        || host == "googlevideo.com"
        || host.ends_with(".googlevideo.com")
}

#[cfg(windows)]
fn is_windows_drive_path(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    bytes.len() >= 3
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
        && bytes[0].is_ascii_alphabetic()
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use super::*;

    #[test]
    fn blocks_special_ipv4_ranges_beyond_std_private_helpers() {
        for ip in [
            "0.1.2.3",
            "100.64.0.1",
            "192.0.2.1",
            "198.18.0.1",
            "198.51.100.2",
            "203.0.113.3",
            "240.0.0.1",
        ] {
            assert!(is_blocked_playable_ip(ip.parse().unwrap()), "{ip}");
        }
        assert!(!is_blocked_playable_ip("93.184.216.34".parse().unwrap()));
    }

    #[test]
    fn blocks_special_ipv6_and_ipv4_mapped_ranges() {
        for ip in [
            "2001:db8::1",
            "2002::1",
            "64:ff9b:1::1",
            "::ffff:192.168.0.1",
            "3fff::1",
        ] {
            assert!(is_blocked_playable_ip(ip.parse().unwrap()), "{ip}");
        }
        assert!(!is_blocked_playable_ip(
            "2606:2800:220:1:248:1893:25c8:1946".parse().unwrap()
        ));
    }

    #[test]
    fn resolved_ip_policy_rejects_any_private_answer() {
        let ips = ["93.184.216.34", "10.0.0.1"]
            .into_iter()
            .map(|ip| ip.parse::<IpAddr>().unwrap());
        let err = validate_resolved_ips("stream.example", ips).unwrap_err();
        assert!(matches!(
            err,
            PlayableUrlError::DestinationBlockedIp { ref host, ref ip }
                if host == "stream.example" && ip == "10.0.0.1"
        ));
    }

    #[test]
    fn redirect_target_string_policy_rejects_private_location() {
        let base = Url::parse("https://stream.example/audio").unwrap();
        let target = redirect_target(&base, "http://127.0.0.1/private").unwrap();
        assert!(matches!(
            validate_playable_url(SearchSource::All, target.as_str()).unwrap_err(),
            PlayableUrlError::BlockedIp(_)
        ));
    }

    #[tokio::test]
    async fn trusted_youtube_handoff_does_not_probe_network() {
        let url = "https://music.youtube.com/watch?v=dQw4w9WgXcQ";
        assert_eq!(
            validate_playback_target_for_handoff(url).await.unwrap(),
            url
        );
    }
}
