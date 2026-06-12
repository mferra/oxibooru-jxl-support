use crate::api::error::{ApiError, ApiResult};
use crate::config::Config;
use crate::content::upload::{MAX_UPLOAD_SIZE, UploadToken};
use crate::filesystem;
use crate::model::enums::MimeType;
use axum::body::Bytes;
use futures::{Stream, StreamExt};
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue, LOCATION, REFERER};
use reqwest::redirect::Policy;
use reqwest::{Client, Response, StatusCode};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;
use url::Url;

// Some websites expect a user-agent
const FAKE_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:135.0) Gecko/20100101 Firefox/135.0";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Bounds the number of HTTP requests made for a single `from_url` call. Covers both
/// redirect hops and the one-time retry-with-Referer, so it's intentionally a bit
/// higher than a typical max-redirects value.
const MAX_ATTEMPTS: u8 = 8;

const MAX_DOWNLOAD_SIZE: u64 = MAX_UPLOAD_SIZE as u64;

fn forbidden_url(message: &'static str) -> ApiError {
    Box::<dyn std::error::Error + Send + Sync>::from(message).into()
}

/// Returns `true` if `ip` is loopback, private, link-local, or otherwise not globally
/// routable. Used to block server-side request forgery (SSRF) against internal services
/// (e.g. the cloud metadata endpoint, other containers, or the host itself).
///
/// This is a denylist, not an exhaustive one: it covers the ranges attackers commonly
/// target, not every reserved IP range in existence.
fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_documentation()
                || ip.is_unspecified()
                || ip.is_multicast()
                || is_shared_address_space(ip)
        }
        IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
            Some(ip) => is_blocked_ip(IpAddr::V4(ip)),
            None => {
                ip.is_loopback()
                    || ip.is_unspecified()
                    || ip.is_multicast()
                    || (ip.segments()[0] & 0xfe00) == 0xfc00 // unique local: fc00::/7
                    || (ip.segments()[0] & 0xffc0) == 0xfe80 // link-local: fe80::/10
            }
        },
    }
}

/// 100.64.0.0/10, reserved for carrier-grade NAT.
fn is_shared_address_space(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    a == 100 && (64..128).contains(&b)
}

/// Resolves `host` and returns a single address to connect to, after verifying that
/// none of the resolved addresses point at a disallowed (private/internal) range.
///
/// Pinning to one validated address (via [`Client::resolve`] in the caller) prevents
/// DNS-rebinding attacks, where the host would resolve to a safe address during
/// validation but to an internal address at connection time.
async fn resolve_validated_addr(host: &str, port: u16, allow_private: bool) -> ApiResult<SocketAddr> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port)).await?.collect();
    if !allow_private && addrs.iter().any(|addr| is_blocked_ip(addr.ip())) {
        return Err(forbidden_url("URL resolves to a disallowed address"));
    }
    addrs.into_iter().next().ok_or_else(|| forbidden_url("Could not resolve host"))
}

/// Determines the host (for [`Client::resolve`] pinning) and validated address to
/// connect to for `url`.
///
/// IP-literal hosts (e.g. `http://[::1]/`) are validated directly, since
/// [`tokio::net::lookup_host`] doesn't accept the bracketed form `url::Url::host_str`
/// returns for IPv6 literals and a literal address doesn't need DNS resolution anyway.
async fn resolve_target(url: &Url, allow_private: bool) -> ApiResult<(String, SocketAddr)> {
    let port = url.port_or_known_default().ok_or_else(|| forbidden_url("URL has no port"))?;
    match url.host() {
        Some(url::Host::Domain(domain)) => {
            let addr = resolve_validated_addr(domain, port, allow_private).await?;
            Ok((domain.to_owned(), addr))
        }
        Some(url::Host::Ipv4(ip)) => {
            if !allow_private && is_blocked_ip(IpAddr::V4(ip)) {
                return Err(forbidden_url("URL points to a disallowed address"));
            }
            Ok((ip.to_string(), SocketAddr::new(IpAddr::V4(ip), port)))
        }
        Some(url::Host::Ipv6(ip)) => {
            if !allow_private && is_blocked_ip(IpAddr::V6(ip)) {
                return Err(forbidden_url("URL points to a disallowed address"));
            }
            Ok((ip.to_string(), SocketAddr::new(IpAddr::V6(ip), port)))
        }
        None => Err(forbidden_url("URL has no host")),
    }
}

/// Builds a client pinned to `addr` for `host`, with redirects disabled so the caller
/// can validate each redirect target before following it.
fn build_client(host: &str, addr: SocketAddr, headers: HeaderMap) -> ApiResult<Client> {
    Client::builder()
        .user_agent(FAKE_USER_AGENT)
        .default_headers(headers)
        .redirect(Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .resolve(host, addr)
        .build()
        .map_err(ApiError::from)
}

/// Fetches `url`, following redirects manually so each target can be validated.
/// `allow_private` permits targets in private address ranges (e.g. a LAN file server).
async fn fetch_response(mut url: Url, allow_private: bool) -> ApiResult<Response> {
    let mut add_referer = false;
    let mut response = None;
    for _ in 0..MAX_ATTEMPTS {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(forbidden_url("Only http and https URLs are allowed"));
        }

        let (host, addr) = resolve_target(&url, allow_private).await?;

        let mut headers = HeaderMap::new();
        if add_referer {
            // Some websites will 403 without a Referer header.
            headers.insert(REFERER, HeaderValue::from_str(url.as_str())?);
        }

        let client = build_client(&host, addr, headers)?;
        let candidate = client.get(url.clone()).send().await?;

        if candidate.status() == StatusCode::FORBIDDEN && !add_referer {
            add_referer = true;
            continue;
        }
        add_referer = false;

        if candidate.status().is_redirection() {
            let location = candidate
                .headers()
                .get(LOCATION)
                .ok_or_else(|| forbidden_url("Redirect response missing Location header"))?
                .to_str()?;
            url = url.join(location).map_err(Box::from)?;
            continue;
        }

        response = Some(candidate.error_for_status()?);
        break;
    }
    response.ok_or_else(|| forbidden_url("Too many redirects"))
}

/// Enforces [`MAX_DOWNLOAD_SIZE`] on `response` and returns its limited byte stream.
fn limited_stream(response: Response) -> ApiResult<impl Stream<Item = ApiResult<Bytes>> + Unpin> {
    if response.content_length().is_some_and(|len| len > MAX_DOWNLOAD_SIZE) {
        return Err(forbidden_url("Content exceeds maximum allowed download size"));
    }

    let mut downloaded: u64 = 0;
    Ok(response.bytes_stream().map(move |chunk_result| {
        let chunk = chunk_result?;
        downloaded += chunk.len() as u64;
        if downloaded > MAX_DOWNLOAD_SIZE {
            return Err(forbidden_url("Content exceeds maximum allowed download size"));
        }
        Ok(chunk)
    }))
}

/// Attempts to download file at the specified `url`.
/// If successful, the file is saved in the temporary uploads directory
/// and a content token is returned.
pub async fn from_url(config: &Config, url: Url) -> ApiResult<UploadToken> {
    let response = fetch_response(url, false).await?;

    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .map(|header_value| header_value.to_str())
        .transpose()?;
    let mime_type = MimeType::from_str(content_type.unwrap_or("")).map_err(Box::from)?;

    let stream = limited_stream(response)?;
    filesystem::save_uploaded_file(config, stream, mime_type).await
}

/// Downloads an archive (e.g. a CBZ) from `url` into the temporary uploads
/// directory and returns its path. Unlike [`from_url`], the Content-Type is
/// not validated, since archives never become post content. Private (LAN)
/// targets are permitted when `allow_lan_archive_downloads` is enabled.
pub async fn archive_from_url(config: &Config, url: Url) -> ApiResult<PathBuf> {
    let response = fetch_response(url, config.allow_lan_archive_downloads).await?;
    let stream = limited_stream(response)?;
    filesystem::save_uploaded_archive(config, stream).await
}

#[cfg(test)]
mod test {
    use super::*;
    use std::net::Ipv6Addr;

    #[test]
    fn blocks_ipv4_internal_ranges() {
        assert!(is_blocked_ip(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))), "loopback");
        assert!(is_blocked_ip(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))), "private (10/8)");
        assert!(is_blocked_ip(IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1))), "private (172.16/12)");
        assert!(is_blocked_ip(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))), "private (192.168/16)");
        assert!(is_blocked_ip(IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254))), "link-local / cloud metadata");
        assert!(is_blocked_ip(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0))), "unspecified");
        assert!(is_blocked_ip(IpAddr::V4(Ipv4Addr::new(255, 255, 255, 255))), "broadcast");
        assert!(is_blocked_ip(IpAddr::V4(Ipv4Addr::new(224, 0, 0, 1))), "multicast");
        assert!(is_blocked_ip(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))), "documentation (TEST-NET-1)");
        assert!(is_blocked_ip(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))), "shared address space (CGNAT)");
        assert!(is_blocked_ip(IpAddr::V4(Ipv4Addr::new(100, 127, 255, 255))), "shared address space (CGNAT)");
    }

    #[test]
    fn allows_ipv4_global_addresses() {
        assert!(!is_blocked_ip(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(!is_blocked_ip(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
        assert!(!is_blocked_ip(IpAddr::V4(Ipv4Addr::new(100, 63, 255, 255))), "just below CGNAT range");
        assert!(!is_blocked_ip(IpAddr::V4(Ipv4Addr::new(100, 128, 0, 0))), "just above CGNAT range");
    }

    #[test]
    fn blocks_ipv6_internal_ranges() {
        assert!(is_blocked_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)), "loopback");
        assert!(is_blocked_ip(IpAddr::V6(Ipv6Addr::UNSPECIFIED)), "unspecified");
        assert!(is_blocked_ip(IpAddr::V6(Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 1))), "unique local (fc00::/7)");
        assert!(is_blocked_ip(IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1))), "link-local (fe80::/10)");
        assert!(is_blocked_ip(IpAddr::V6(Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 1))), "multicast");
        // IPv4-mapped addresses must be checked against the embedded IPv4 ranges too.
        assert!(is_blocked_ip(IpAddr::V6(Ipv4Addr::new(127, 0, 0, 1).to_ipv6_mapped())), "mapped loopback");
        assert!(is_blocked_ip(IpAddr::V6(Ipv4Addr::new(169, 254, 169, 254).to_ipv6_mapped())), "mapped metadata");
    }

    #[test]
    fn allows_ipv6_global_addresses() {
        assert!(!is_blocked_ip(IpAddr::V6(Ipv6Addr::new(0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8888))));
        assert!(!is_blocked_ip(IpAddr::V6(Ipv4Addr::new(8, 8, 8, 8).to_ipv6_mapped())), "mapped global address");
    }

    #[tokio::test]
    async fn rejects_disallowed_schemes() {
        let config = crate::config::test_config(None);
        for url in ["file:///etc/passwd", "ftp://example.com/file", "gopher://example.com/", "data:text/plain,hi"] {
            let result = from_url(&config, Url::parse(url).unwrap()).await;
            assert!(result.is_err(), "{url} should be rejected");
        }
    }

    #[tokio::test]
    async fn rejects_requests_to_internal_addresses() {
        let config = crate::config::test_config(None);
        for url in [
            "http://127.0.0.1/",
            "http://localhost/",
            "http://169.254.169.254/latest/meta-data/", // cloud metadata endpoint
            "http://10.0.0.1/",
            "http://[::1]/",
            "http://[fc00::1]/",
        ] {
            let result = from_url(&config, Url::parse(url).unwrap()).await;
            assert!(result.is_err(), "{url} should be rejected");
        }
    }
}
