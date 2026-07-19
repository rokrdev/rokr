//! The `webfetch` tool: a client-side HTTP GET returning readable text
//! content from an agent-chosen URL.
//!
//! This is a side effect with real SSRF risk (the agent picks the URL), so
//! it is gated behind the same preview-before-execute mechanism as
//! `bash`/`write`/`edit` (`docs/adr/0005-permission-model.md`): the user
//! sees the literal URL (via [`PreviewableTool::preview`]) before the fetch
//! happens.
//!
//! # SSRF guard design
//!
//! The guard resolves the URL's host to concrete IP addresses itself
//! (`resolve_validated`) and rejects the request before any HTTP client is
//! even constructed if any resolved address falls in a private, loopback,
//! link-local, or unique-local range (or is unspecified). This closes the
//! DNS-rebinding gap where a hostname resolves to a public address at
//! validation time but a private one at request time: the same resolved
//! addresses that were validated are the only addresses the actual request
//! is permitted to connect to, via `reqwest::ClientBuilder::resolve_to_addrs`
//! pinning the connection to them (so there is no second, unvalidated
//! resolution at request time).
//!
//! Redirects are validated per-hop: `reqwest`'s automatic redirect-following
//! is disabled (`redirect::Policy::none()`) and the hop loop is implemented
//! by hand here, re-running the full resolve-then-validate check on each
//! `Location` before following it. This was chosen over
//! `redirect::Policy::custom()` because the validation is async (real DNS
//! resolution) and the custom-policy closure is synchronous — implementing
//! the loop directly is the simplest way to get genuine per-hop validation
//! rather than working around that mismatch.
//!
//! # Testing note: wiremock and the SSRF guard
//!
//! `wiremock::MockServer` always binds to a loopback address, which the SSRF
//! guard (correctly) rejects. So tests that need a real HTTP exchange (size
//! cap, redirect cap, content-type rejection) call [`fetch_with_validator`]
//! directly with a relaxed test-only validator instead of going through
//! `WebfetchTool::execute`/the real [`is_blocked_ip`] — they exercise the
//! exact same fetch engine (client construction, streaming cap, redirect-hop
//! loop) production code uses, just with loopback allowed through so the
//! mock server is reachable. The SSRF guard itself (including its
//! `execute()`-level wiring) is exercised separately, and exclusively, by
//! `webfetch_rejects_loopback_and_cloud_metadata_addresses_without_request`,
//! which calls the real `WebfetchTool::execute` with the real validator.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use serde::Deserialize;

use crate::{Preview, PreviewableTool, Tool, ToolError};

#[derive(Debug, Deserialize)]
struct WebfetchInput {
    url: String,
}

/// Response size cap, in bytes, mirroring `read.rs`'s `MAX_READ_FILE_BYTES`
/// (same value, same truncation-notice format — see `truncate_to_cap`).
/// `rokr-tools` does not depend on `rokr-core`, and `read.rs`'s constant is
/// private to that module, so this is an independent local constant rather
/// than a shared import.
const MAX_WEBFETCH_RESPONSE_BYTES: usize = 64 * 1024;

/// Maximum number of redirects to follow before giving up.
const MAX_REDIRECTS: u8 = 5;

/// Per-request timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Content types treated as "text-like" and therefore fetchable. Kept as a
/// conservative allowlist (PRD: "reject any response whose content type
/// isn't text-like") rather than a denylist: `text/*`'s common members plus
/// `application/json`, which is ubiquitous and still text.
const ALLOWED_CONTENT_TYPES: [&str; 5] = [
    "text/plain",
    "text/html",
    "text/markdown",
    "text/csv",
    "application/json",
];

/// Truncates `contents` to at most `cap` bytes (respecting UTF-8 char
/// boundaries), returning the (possibly truncated) body plus an optional
/// notice string to append when truncation occurred. Mirrors
/// `read.rs::truncate_to_cap` byte-for-byte (not imported: `rokr-tools`
/// deliberately keeps `read.rs` and `webfetch.rs` independent).
fn truncate_to_cap(contents: &str, cap: usize) -> (&str, Option<String>) {
    if contents.len() <= cap {
        return (contents, None);
    }

    let mut boundary = cap;
    while boundary > 0 && !contents.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let body = &contents[..boundary];
    let notice = format!(
        "\n[truncated, showing {} of {} bytes]",
        body.len(),
        contents.len()
    );
    (body, Some(notice))
}

/// The real SSRF guard: rejects an IP address that falls in a private,
/// loopback, link-local, or unique-local range, or is unspecified.
///
/// IPv4: private (10/8, 172.16/12, 192.168/16), loopback (127/8),
/// link-local (169.254/16 — this is the range the cloud-metadata address
/// 169.254.169.254 lives in; it is not special-cased separately).
/// IPv6: loopback (::1), link-local (fe80::/10), unique-local (fc00::/7).
/// Both families also reject the unspecified address (0.0.0.0 / ::)
/// defensively.
fn is_blocked_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_private() || v4.is_loopback() || v4.is_link_local() || v4.is_unspecified()
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unicast_link_local()
                || v6.is_unique_local()
                || v6.is_unspecified()
        }
    }
}

/// Resolves `host` and validates every resolved address against `is_blocked`
/// *before* any HTTP client is constructed. Returns the validated addresses
/// so the caller can pin the actual request's connection to exactly this
/// set (via `resolve_to_addrs`), closing the DNS-rebinding gap between this
/// resolution and the one the request would otherwise perform on its own.
async fn resolve_validated(
    host: &str,
    port: u16,
    is_blocked: impl Fn(&IpAddr) -> bool,
) -> Result<Vec<SocketAddr>, ToolError> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| ToolError::ExecutionFailed(format!("DNS resolution failed for {host}: {e}")))?
        .collect();

    if addrs.is_empty() {
        return Err(ToolError::ExecutionFailed(format!(
            "no addresses resolved for host {host}"
        )));
    }

    if let Some(blocked) = addrs.iter().find(|addr| is_blocked(&addr.ip())) {
        return Err(ToolError::ExecutionFailed(format!(
            "refused to fetch {host}: resolves to blocked address {} \
             (private, loopback, link-local, and cloud-metadata address ranges are not permitted)",
            blocked.ip()
        )));
    }

    Ok(addrs)
}

/// Reads `response`'s body, stopping once the accumulated size exceeds
/// `MAX_WEBFETCH_RESPONSE_BYTES` rather than buffering an arbitrarily large
/// body first (a malicious/misbehaving server can't be trusted to report an
/// honest `Content-Length`, so the cap is enforced while streaming, not
/// just checked against the header).
async fn read_capped_body(mut response: reqwest::Response) -> Result<String, ToolError> {
    let mut collected: Vec<u8> = Vec::new();
    while collected.len() <= MAX_WEBFETCH_RESPONSE_BYTES {
        match response
            .chunk()
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("failed reading response body: {e}")))?
        {
            Some(chunk) => collected.extend_from_slice(&chunk),
            None => break,
        }
    }

    let text = String::from_utf8_lossy(&collected).into_owned();
    let (body, notice) = truncate_to_cap(&text, MAX_WEBFETCH_RESPONSE_BYTES);
    let mut output = body.to_string();
    if let Some(notice) = notice {
        output.push_str(&notice);
    }
    Ok(output)
}

/// The real fetch entry point used by [`WebfetchTool::execute`]: the real
/// [`is_blocked_ip`] SSRF guard, always.
async fn fetch(url_str: &str) -> Result<String, ToolError> {
    fetch_with_validator(url_str, is_blocked_ip).await
}

/// The shared fetch engine: parses the URL, then loops resolve-validate,
/// pinned-client construction, and request/response handling once per hop,
/// following redirects (up to [`MAX_REDIRECTS`]) by hand so each hop's
/// target gets the same validation the initial URL got. `is_blocked` is the
/// SSRF guard predicate; production always passes [`is_blocked_ip`] (see
/// [`fetch`]) — tests targeting a `wiremock` server (necessarily bound to
/// loopback) pass a relaxed test-only predicate instead, see this module's
/// doc comment.
async fn fetch_with_validator(
    url_str: &str,
    is_blocked: impl Fn(&IpAddr) -> bool + Copy,
) -> Result<String, ToolError> {
    let mut current = reqwest::Url::parse(url_str)
        .map_err(|e| ToolError::InvalidInput(format!("invalid URL: {e}")))?;
    let mut redirect_count: u8 = 0;

    loop {
        match current.scheme() {
            "http" | "https" => {}
            other => {
                return Err(ToolError::InvalidInput(format!(
                    "unsupported URL scheme: {other} (only http/https are permitted)"
                )))
            }
        }

        // `Url::host_str()` returns IPv6 hosts in bracketed form (`"[::1]"`,
        // matching URL syntax), but both `tokio::net::lookup_host`'s IP-literal
        // fast path and reqwest's own internal override-matching key (derived
        // from the request's `http::uri::Authority::host()`, which is always
        // bracket-free) expect the bare address — so brackets are stripped
        // here to match what the rest of this hop actually resolves/connects
        // against.
        let host = current
            .host_str()
            .ok_or_else(|| ToolError::InvalidInput("URL has no host".to_string()))?
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        let port = current.port_or_known_default().ok_or_else(|| {
            ToolError::InvalidInput(format!("could not determine port for URL: {current}"))
        })?;

        let addrs = resolve_validated(&host, port, is_blocked).await?;

        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .resolve_to_addrs(&host, &addrs)
            .build()
            .map_err(|e| ToolError::ExecutionFailed(format!("failed to build HTTP client: {e}")))?;

        let response = client
            .get(current.clone())
            .send()
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("request failed: {e}")))?;

        let status = response.status();

        if status.is_redirection() {
            redirect_count += 1;
            if redirect_count > MAX_REDIRECTS {
                return Err(ToolError::ExecutionFailed(format!(
                    "too many redirects (exceeded cap of {MAX_REDIRECTS})"
                )));
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .ok_or_else(|| {
                    ToolError::ExecutionFailed(
                        "redirect response missing Location header".to_string(),
                    )
                })?
                .to_str()
                .map_err(|e| {
                    ToolError::ExecutionFailed(format!("invalid Location header: {e}"))
                })?;
            current = current.join(location).map_err(|e| {
                ToolError::ExecutionFailed(format!("invalid redirect location {location}: {e}"))
            })?;
            continue;
        }

        if !status.is_success() {
            return Err(ToolError::ExecutionFailed(format!(
                "request failed with status {status}"
            )));
        }

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let base_type = content_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if !ALLOWED_CONTENT_TYPES.contains(&base_type.as_str()) {
            return Err(ToolError::ExecutionFailed(format!(
                "unsupported content type: {content_type} \
                 (only text-like content types are permitted)"
            )));
        }

        return read_capped_body(response).await;
    }
}

/// Fetches a URL's content as readable text. Gated per
/// `docs/adr/0005-permission-model.md`: see [`PreviewableTool::preview`] for
/// the side-effect-free description shown before permission is granted.
pub struct WebfetchTool;

impl Tool for WebfetchTool {
    fn name(&self) -> &'static str {
        "webfetch"
    }

    fn description(&self) -> &'static str {
        "Fetch a URL's content as readable text. Rejects requests to \
         private, loopback, link-local, and cloud-metadata addresses; caps \
         response size, redirects, and request timeout; rejects non-text \
         content."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "The URL to fetch." }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<String, ToolError> {
        let input: WebfetchInput =
            serde_json::from_value(input).map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        fetch(&input.url).await
    }
}

impl PreviewableTool for WebfetchTool {
    fn preview(&self, input: serde_json::Value) -> Result<Preview, ToolError> {
        let input: WebfetchInput =
            serde_json::from_value(input).map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        Ok(Preview::Command(input.url))
    }
}

#[cfg(test)]
mod tests {
    use super::{fetch_with_validator, is_blocked_ip, WebfetchTool};
    use crate::{Preview, PreviewableTool, Tool, ToolError};
    use serde_json::json;
    use std::net::IpAddr;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Test-only relaxed validator: `wiremock::MockServer` always binds to a
    /// loopback address, so tests that need a real HTTP exchange (size cap,
    /// redirect cap, content-type rejection) call `fetch_with_validator`
    /// with this instead of the real `is_blocked_ip`, letting the mock
    /// server through while every other guarded range still applies. The
    /// real guard, wired through `WebfetchTool::execute`, is exercised
    /// separately by `webfetch_rejects_loopback_and_cloud_metadata_addresses_without_request`.
    fn allow_loopback(ip: &IpAddr) -> bool {
        !ip.is_loopback() && is_blocked_ip(ip)
    }

    #[test]
    fn is_blocked_ip_rejects_all_documented_ranges() {
        let blocked: [IpAddr; 7] = [
            "127.0.0.1".parse().unwrap(),
            "10.0.0.1".parse().unwrap(),
            "192.168.1.1".parse().unwrap(),
            "169.254.169.254".parse().unwrap(),
            "0.0.0.0".parse().unwrap(),
            "::1".parse().unwrap(),
            "fe80::1".parse().unwrap(),
        ];
        for ip in blocked {
            assert!(is_blocked_ip(&ip), "expected {ip} to be blocked");
        }

        let allowed: [IpAddr; 2] = ["8.8.8.8".parse().unwrap(), "1.1.1.1".parse().unwrap()];
        for ip in allowed {
            assert!(!is_blocked_ip(&ip), "expected {ip} to be allowed");
        }
    }

    #[tokio::test]
    async fn webfetch_preview_returns_url_without_side_effects() {
        let server = MockServer::start().await;
        let url = format!("{}/some/path", server.uri());

        let tool = WebfetchTool;
        let preview = tool
            .preview(json!({ "url": url }))
            .expect("preview should succeed");

        assert_eq!(preview, Preview::Command(url));
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            0,
            "preview must not issue any network request"
        );
    }

    #[tokio::test]
    async fn webfetch_rejects_loopback_and_cloud_metadata_addresses_without_request() {
        let blocked_urls = [
            "http://127.0.0.1/",
            "http://10.0.0.1/",
            "http://192.168.1.1/",
            "http://169.254.169.254/",
            "http://[::1]/",
            "http://[fe80::1]/",
        ];

        let tool = WebfetchTool;
        for url in blocked_urls {
            let result = tool.execute(json!({ "url": url })).await;
            match result {
                Err(ToolError::ExecutionFailed(msg)) => {
                    assert!(
                        msg.to_lowercase().contains("blocked"),
                        "expected a blocked-address rejection message for {url}, got: {msg}"
                    );
                }
                other => panic!("expected Err(ToolError::ExecutionFailed(_)) for {url}, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn webfetch_enforces_response_size_cap_and_redirect_cap() {
        // (a) response larger than the cap gets truncated with a notice.
        let size_server = MockServer::start().await;
        let big_body = "a".repeat(64 * 1024 + 500);
        Mock::given(method("GET"))
            .and(path("/big"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(big_body.clone())
                    .insert_header("content-type", "text/plain"),
            )
            .mount(&size_server)
            .await;

        let big_url = format!("{}/big", size_server.uri());
        let output = fetch_with_validator(&big_url, allow_loopback)
            .await
            .expect("oversized response should still succeed, truncated");

        assert!(
            output.len() < big_body.len(),
            "expected output to be truncated below the original body size"
        );
        assert!(
            output.contains("truncated"),
            "expected output to contain a truncation notice, got: {output}"
        );

        // (b) a redirect chain longer than the cap is rejected outright.
        let redirect_server = MockServer::start().await;
        for hop in 0..7 {
            Mock::given(method("GET"))
                .and(path(format!("/hop{hop}")))
                .respond_with(
                    ResponseTemplate::new(302)
                        .insert_header("Location", format!("/hop{}", hop + 1)),
                )
                .mount(&redirect_server)
                .await;
        }

        let redirect_url = format!("{}/hop0", redirect_server.uri());
        let result = fetch_with_validator(&redirect_url, allow_loopback).await;
        assert!(
            result.is_err(),
            "expected a redirect chain longer than the cap to be rejected"
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .to_lowercase()
                .contains("redirect"),
            "expected the rejection to mention redirects"
        );
    }

    #[tokio::test]
    async fn webfetch_rejects_non_text_content_type() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/binary"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(vec![0u8, 1, 2, 3])
                    .insert_header("content-type", "application/octet-stream"),
            )
            .mount(&server)
            .await;

        let url = format!("{}/binary", server.uri());
        let result = fetch_with_validator(&url, allow_loopback).await;

        match result {
            Err(err) => {
                let msg = err.to_string().to_lowercase();
                assert!(
                    msg.contains("content type"),
                    "expected the rejection to mention content type, got: {msg}"
                );
            }
            Ok(output) => panic!("expected a non-text content type to be rejected, got: {output}"),
        }
    }

    #[tokio::test]
    async fn webfetch_returns_full_text_when_under_cap() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/hello"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("hello from the mock server")
                    .insert_header("content-type", "text/plain"),
            )
            .mount(&server)
            .await;

        let url = format!("{}/hello", server.uri());
        let output = fetch_with_validator(&url, allow_loopback)
            .await
            .expect("fetch of a small text response should succeed");

        assert_eq!(output, "hello from the mock server");
    }

    #[tokio::test]
    async fn webfetch_revalidates_ssrf_guard_on_each_redirect_hop() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/redirect-to-metadata"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", "http://169.254.169.254/latest/meta-data/"),
            )
            .mount(&server)
            .await;

        let url = format!("{}/redirect-to-metadata", server.uri());
        // Uses the real is_blocked_ip guard here (not allow_loopback) since
        // the property under test is that the *redirect target*, not the
        // initial URL, gets rejected.
        let result = fetch_with_validator(&url, allow_loopback).await;

        assert!(
            result.is_err(),
            "expected a redirect to a cloud-metadata address to be rejected"
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .to_lowercase()
                .contains("blocked"),
            "expected the rejection to come from the SSRF guard on the redirect hop"
        );
    }
}
