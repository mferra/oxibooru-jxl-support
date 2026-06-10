use crate::api::error::{ApiError, ApiResult};
use crate::config::Config;
use crate::content::upload::{MAX_UPLOAD_SIZE, UploadToken};
use crate::filesystem;
use crate::model::enums::MimeType;
use futures::StreamExt;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue, LOCATION, REFERER};
use reqwest::redirect::Policy;
use reqwest::{Client, StatusCode};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
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
async fn resolve_validated_addr(host: &str, port: u16) -> ApiResult<SocketAddr> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port)).await?.collect();
    if addrs.iter().any(|addr| is_blocked_ip(addr.ip())) {
        return Err(forbidden_url("URL resolves to a disallowed address"));
    }
    addrs.into_iter().next().ok_or_else(|| forbidden_url("Could not resolve host"))
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

/// Attempts to download file at the specified `url`.
/// If successful, the file is saved in the temporary uploads directory
/// and a content token is returned.
pub async fn from_url(config: &Config, mut url: Url) -> ApiResult<UploadToken> {
    let mut add_referer = false;
    let mut response = None;
    for _ in 0..MAX_ATTEMPTS {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(forbidden_url("Only http and https URLs are allowed"));
        }

        let host = url.host_str().ok_or_else(|| forbidden_url("URL has no host"))?.to_owned();
        let port = url.port_or_known_default().ok_or_else(|| forbidden_url("URL has no port"))?;
        let addr = resolve_validated_addr(&host, port).await?;

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
    let response = response.ok_or_else(|| forbidden_url("Too many redirects"))?;

    if response.content_length().is_some_and(|len| len > MAX_DOWNLOAD_SIZE) {
        return Err(forbidden_url("Content exceeds maximum allowed download size"));
    }

    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .map(|header_value| header_value.to_str())
        .transpose()?;
    let mime_type = MimeType::from_str(content_type.unwrap_or("")).map_err(Box::from)?;

    let mut downloaded: u64 = 0;
    let limited_stream = response.bytes_stream().map(move |chunk_result| {
        let chunk = chunk_result?;
        downloaded += chunk.len() as u64;
        if downloaded > MAX_DOWNLOAD_SIZE {
            return Err(forbidden_url("Content exceeds maximum allowed download size"));
        }
        Ok(chunk)
    });

    filesystem::save_uploaded_file(config, limited_stream, mime_type).await
}
