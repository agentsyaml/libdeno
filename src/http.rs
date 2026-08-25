// Minimal HTTP client for the deno_cache_dir file fetcher, used to fetch
// remote (https/http) modules into the global cache. Deliberately does not
// follow redirects; redirect handling belongs to the fetcher.

use deno_cache_dir::file_fetcher::HttpClient;
use deno_cache_dir::file_fetcher::SendError;
use deno_cache_dir::file_fetcher::SendResponse;
use deno_npm_cache::DownloadError;
use deno_npm_cache::NpmCacheHttpClient;
use deno_npm_cache::NpmCacheHttpClientBytesResponse;
use deno_npm_cache::NpmCacheHttpClientResponse;
use deno_npmrc::RegistryConfig;
use http::header::AUTHORIZATION;
use http::header::ETAG;
use http::header::IF_NONE_MATCH;
use http::header::LOCATION;
use http::HeaderMap;
use std::time::Duration;
use std::time::Instant;
use url::Url;

use crate::LibdenoError;

/// Cap on the decompressed response body we buffer for module fetches,
/// guarding against decompression bombs. reqwest auto-decompresses
/// gzip/brotli (enabled in Cargo.toml) at the transport layer before the body
/// reaches us, so this counts true memory usage, not the wire size.
const MAX_RESPONSE_BODY_BYTES: usize = 256 << 20;
/// Cap for explicit npm `.tgz` downloads: some legal packages exceed 256MiB.
/// Still bounded, so a broken/oversized response fails fast instead of OOMing
/// the host. This is a response-body cap after any HTTP Content-Encoding
/// decoding by reqwest, not wire-byte accounting; an npm `.tgz` response
/// remains compressed tarball data here. The ISIZE guard below is a coarse
/// publisher-controlled pre-check; neither boundary accounts for allocations
/// inside upstream extraction.
const MAX_TARBALL_BODY_BYTES: usize = 1 << 30;

/// Total wall-clock budget for one HTTP operation, including retries,
/// redirects, backoff, and response-body reads. This is a transport budget,
/// not `LibdenoOptions::execution_deadline` (that option is not passed into
/// this client).
const HTTP_RETRY_WALL_CLOCK_BUDGET: Duration = Duration::from_secs(300);

#[derive(Debug, Clone)]
pub struct ReqwestHttpClient {
    client: reqwest::Client,
}

impl ReqwestHttpClient {
    /// Sole constructor: every request path (file fetcher + npm installer)
    /// shares this builder config. reqwest::Client is Arc-backed, so `Clone`
    /// shares the same connection pool and configuration.
    ///
    /// reqwest 0.12 has no client-level response body limit (`response_body_limit`
    /// does not exist here), so the caps are enforced per read in
    /// [`read_body_limited`]. HTTP Content-Encoding is decoded before that
    /// helper, but an npm `.tgz` response body remains compressed tarball data.
    pub fn new() -> Result<Self, LibdenoError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(300))
            .user_agent(concat!("libdeno/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| LibdenoError::Runtime(deno_core::anyhow::anyhow!(e)))?;
        Ok(Self { client })
    }
}

/// Reads the full response body as delivered by reqwest into a Vec, failing
/// once the accumulated size exceeds the caller-supplied `limit` instead of
/// buffering an unbounded response. HTTP Content-Encoding has already been
/// decoded, while an npm `.tgz` response body remains compressed tarball data.
/// The caller picks the bound: module fetches pin the module cap, while npm
/// registry downloads use the explicit tarball-path check in [`npm_body_limit`].
/// Over-limit is an error, never a retry.
async fn read_body_limited(
    response: &mut reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("failed to read response body: {e}"))?
    {
        if chunk.len() > limit.saturating_sub(body.len()) {
            return Err(format!("response body exceeded the {limit}-byte limit"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Body cap for npm registry downloads. The URL path is the only evidence that
/// permits the larger budget: a metadata/JSON response cannot obtain it by
/// declaring a large `Content-Length`, and chunked metadata remains on the
/// module cap. An explicit `.tgz` path is bounded by the tarball cap even when
/// the registry omits `Content-Length`.
fn npm_body_limit(is_tarball: bool, _content_length: Option<u64>) -> usize {
    if is_tarball {
        MAX_TARBALL_BODY_BYTES
    } else {
        MAX_RESPONSE_BODY_BYTES
    }
}

fn remaining_http_budget(deadline: Instant) -> Result<Duration, String> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(format!(
            "HTTP request exceeded the {}-second wall-clock budget",
            HTTP_RETRY_WALL_CLOCK_BUDGET.as_secs()
        ))
    } else {
        Ok(remaining)
    }
}

/// Default post-decompression budget for npm tarballs, and the env override
/// (`LIBDENO_MAX_TARBALL_DECOMPRESSED_BYTES`). The budget is used for the
/// publisher-controlled ISIZE guard; it is deliberately configurable so hosts with
/// unusual packages can raise it.
fn tarball_decompress_budget() -> usize {
    const DEFAULT: usize = 1 << 30;
    static BUDGET: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *BUDGET.get_or_init(|| {
        std::env::var("LIBDENO_MAX_TARBALL_DECOMPRESSED_BYTES")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(DEFAULT)
    })
}

/// Rejects gzip tarballs whose ISIZE trailer (last 4 bytes, little-endian,
/// mod 2^32) claims a decompressed size above the configured budget.
///
/// This is a coarse guard, not a precise bound: ISIZE only reflects the last
/// gzip member (multi-member tarballs can decompress larger in total) and
/// wraps at 4 GiB. It exists to block the cheap malicious case — a tiny
/// tarball declaring ~4 GiB that would make upstream's `try_reserve` commit
/// that much virtual memory before the real decompressed size is known;
/// upstream falls back to streaming (no reservation) when the reserve fails,
/// and that path plus this check bound the practical exposure. The
/// `read_body_limited` cap above bounds the *compressed* bytes already.
fn guard_tarball_isize(is_tarball: bool, bytes: Vec<u8>) -> Result<Vec<u8>, String> {
    // Only .tgz URLs carry gzip-compressed tar data; registry metadata
    // (JSON, possibly transport-gzip-encoded) must not be size-checked here —
    // its trailer bytes are not an ISIZE and would spuriously fail.
    if !is_tarball || bytes.len() < 8 || bytes[..2] != [0x1f, 0x8b] {
        return Ok(bytes);
    }
    let isize = u32::from_le_bytes([
        bytes[bytes.len() - 4],
        bytes[bytes.len() - 3],
        bytes[bytes.len() - 2],
        bytes[bytes.len() - 1],
    ]) as usize;
    let budget = tarball_decompress_budget();
    if isize > budget {
        return Err(format!(
            "tarball declares {isize} decompressed bytes, over the \
             {budget}-byte budget (raise LIBDENO_MAX_TARBALL_DECOMPRESSED_BYTES to allow it)"
        ));
    }
    Ok(bytes)
}

#[async_trait::async_trait(?Send)]
impl HttpClient for ReqwestHttpClient {
    async fn send_no_follow(
        &self,
        url: &Url,
        headers: HeaderMap,
    ) -> Result<SendResponse, SendError> {
        // Transient failures (network errors, 429/5xx) are retried with a
        // bounded backoff, mirroring the npm path below: a single dropped
        // packet must not fail a module fetch for the whole run.
        let max_attempts = 3;
        let mut attempt = 0;
        let deadline = Instant::now() + HTTP_RETRY_WALL_CLOCK_BUDGET;
        loop {
            attempt += 1;
            let remaining =
                remaining_http_budget(deadline).map_err(|e| SendError::Failed(e.into()))?;
            match tokio::time::timeout(
                remaining,
                self.client.get(url.clone()).headers(headers.clone()).send(),
            )
            .await
            {
                Ok(Ok(response)) => {
                    let status = response.status();

                    // is_redirection() covers 300..=399 including 304, so the
                    // 304 check must come first: a 304 (re-validation of a
                    // stale cached module) is not a redirect and carries no
                    // Location header.
                    if status == http::StatusCode::NOT_MODIFIED {
                        return Ok(SendResponse::NotModified);
                    }

                    if status.is_redirection() {
                        let mut redirect_headers = HeaderMap::new();
                        if let Some(location) = response.headers().get(LOCATION) {
                            redirect_headers.insert(LOCATION, location.clone());
                        }
                        return Ok(SendResponse::Redirect(redirect_headers));
                    }

                    if status.is_success() {
                        let headers = response.headers().clone();
                        let mut response = response;
                        // Module fetches are pinned to the module cap: a
                        // malicious module server declaring a huge
                        // Content-Length must not buy a larger budget.
                        let remaining = remaining_http_budget(deadline)
                            .map_err(|e| SendError::Failed(e.into()))?;
                        let body = tokio::time::timeout(
                            remaining,
                            read_body_limited(&mut response, MAX_RESPONSE_BODY_BYTES),
                        )
                        .await
                        .map_err(|_| {
                            SendError::Failed(
                                format!(
                                    "HTTP request exceeded the {}-second wall-clock budget",
                                    HTTP_RETRY_WALL_CLOCK_BUDGET.as_secs()
                                )
                                .into(),
                            )
                        })?
                        .map_err(|e| SendError::Failed(e.into()))?;
                        return Ok(SendResponse::Success(headers, body));
                    }

                    if status == http::StatusCode::NOT_FOUND {
                        return Err(SendError::NotFound);
                    }
                    if (status.as_u16() == 429 || status.is_server_error())
                        && attempt < max_attempts
                    {
                        let delay = Duration::from_millis(200 * attempt);
                        let remaining = remaining_http_budget(deadline)
                            .map_err(|e| SendError::Failed(e.into()))?;
                        if delay >= remaining {
                            return Err(SendError::Failed(
                                format!(
                                    "HTTP request exceeded the {}-second wall-clock budget",
                                    HTTP_RETRY_WALL_CLOCK_BUDGET.as_secs()
                                )
                                .into(),
                            ));
                        }
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Err(SendError::StatusCode(status));
                }
                Ok(Err(_)) if attempt < max_attempts => {
                    let delay = Duration::from_millis(200 * attempt);
                    let remaining =
                        remaining_http_budget(deadline).map_err(|e| SendError::Failed(e.into()))?;
                    if delay >= remaining {
                        return Err(SendError::Failed(
                            format!(
                                "HTTP request exceeded the {}-second wall-clock budget",
                                HTTP_RETRY_WALL_CLOCK_BUDGET.as_secs()
                            )
                            .into(),
                        ));
                    }
                    tokio::time::sleep(delay).await;
                    continue;
                }
                Ok(Err(e)) => return Err(SendError::Failed(e.into())),
                Err(_) => {
                    return Err(SendError::Failed(
                        format!(
                            "HTTP request exceeded the {}-second wall-clock budget",
                            HTTP_RETRY_WALL_CLOCK_BUDGET.as_secs()
                        )
                        .into(),
                    ))
                }
            }
        }
    }
}

#[async_trait::async_trait(?Send)]
impl NpmCacheHttpClient for ReqwestHttpClient {
    async fn download_with_retries_on_any_tokio_runtime(
        &self,
        url: Url,
        maybe_auth: Option<String>,
        maybe_etag: Option<String>,
        maybe_registry_config: Option<&RegistryConfig>,
    ) -> Result<NpmCacheHttpClientResponse, DownloadError> {
        // Classify only the original request path. Redirects to opaque CDN
        // paths must preserve tarball handling, while metadata must not gain it.
        let is_tarball = url.path().ends_with(".tgz");
        let mut url = url;
        let err = |status_code, message: String| DownloadError {
            status_code,
            error: deno_error::JsErrorBox::generic(message),
        };

        // Use the npmrc credentials when the caller did not supply an explicit
        // auth value (private registries).
        let mut auth = maybe_auth.or_else(|| {
            maybe_registry_config.and_then(|c| {
                if let Some(token) = &c.auth_token {
                    Some(format!("Bearer {token}"))
                } else if let Some(token) = &c.auth {
                    Some(format!("Basic {token}"))
                } else if let (Some(user), Some(pass)) = (&c.username, &c.password) {
                    use base64::Engine;
                    Some(format!(
                        "Basic {}",
                        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"))
                    ))
                } else {
                    None
                }
            })
        });

        // The method name promises retries; the registry's 5xx/429 responses and
        // transient network errors are retried with a short backoff.
        let max_attempts = 3;
        let mut attempt = 0;
        let mut redirects_remaining: u32 = 10;
        let deadline = Instant::now() + HTTP_RETRY_WALL_CLOCK_BUDGET;
        loop {
            attempt += 1;
            let mut request = self.client.get(url.clone());
            if let Some(auth) = &auth {
                request = request.header(AUTHORIZATION, auth);
            }
            if let Some(etag) = &maybe_etag {
                request = request.header(IF_NONE_MATCH, etag);
            }

            let remaining = remaining_http_budget(deadline).map_err(|e| err(None, e))?;
            match tokio::time::timeout(remaining, request.send()).await {
                Ok(Ok(response)) => {
                    let status = response.status();

                    if status == http::StatusCode::NOT_FOUND {
                        return Ok(NpmCacheHttpClientResponse::NotFound);
                    }
                    if status == http::StatusCode::NOT_MODIFIED {
                        return Ok(NpmCacheHttpClientResponse::NotModified);
                    }
                    if status.is_success() {
                        let etag = response
                            .headers()
                            .get(ETAG)
                            .and_then(|v| v.to_str().ok())
                            .map(String::from);
                        let mut response = response;
                        let limit = npm_body_limit(is_tarball, response.content_length());
                        let remaining =
                            remaining_http_budget(deadline).map_err(|e| err(None, e))?;
                        let bytes = tokio::time::timeout(
                            remaining,
                            read_body_limited(&mut response, limit),
                        )
                        .await
                        .map_err(|_| {
                            err(
                                None,
                                format!(
                                    "npm registry request exceeded the {}-second wall-clock budget",
                                    HTTP_RETRY_WALL_CLOCK_BUDGET.as_secs()
                                ),
                            )
                        })?
                        .map_err(|e| {
                            err(None, format!("failed to read npm registry response: {e}"))
                        })?;
                        let bytes = guard_tarball_isize(is_tarball, bytes).map_err(|e| {
                            err(None, format!("failed to download npm tarball: {e}"))
                        })?;
                        return Ok(NpmCacheHttpClientResponse::Bytes(
                            NpmCacheHttpClientBytesResponse { bytes, etag },
                        ));
                    }
                    // Follow redirects manually (the client uses Policy::none):
                    // registries (GitHub Packages, corporate mirrors) redirect
                    // tarball/metadata URLs. 304 is already handled above.
                    if status.is_redirection() {
                        if redirects_remaining == 0 {
                            return Err(err(
                                Some(status.as_u16()),
                                format!("npm registry request failed with status {status}"),
                            ));
                        }
                        redirects_remaining -= 1;
                        let location = response
                            .headers()
                            .get(LOCATION)
                            .and_then(|v| v.to_str().ok())
                            .ok_or_else(|| {
                                err(
                                    Some(status.as_u16()),
                                    format!("npm registry request failed with status {status}"),
                                )
                            })?;
                        let next = url.join(location).map_err(|e| {
                            err(
                                Some(status.as_u16()),
                                format!("invalid npm registry redirect location: {e}"),
                            )
                        })?;
                        // Never leak registry bearer tokens off-origin: drop
                        // auth on scheme/host/port change (origin, like the
                        // file fetcher's analog), not just host.
                        if next.origin() != url.origin() {
                            auth = None;
                        }
                        url = next;
                        attempt = 0; // a redirect is a new request, not a retry
                        continue;
                    }
                    // 429/5xx are transient; everything else is a hard failure.
                    if (status.as_u16() == 429 || status.is_server_error())
                        && attempt < max_attempts
                    {
                        let delay = Duration::from_millis(200 * attempt);
                        let remaining =
                            remaining_http_budget(deadline).map_err(|e| err(None, e))?;
                        if delay >= remaining {
                            return Err(err(
                                None,
                                format!(
                                    "npm registry request exceeded the {}-second wall-clock budget",
                                    HTTP_RETRY_WALL_CLOCK_BUDGET.as_secs()
                                ),
                            ));
                        }
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Err(err(
                        Some(status.as_u16()),
                        format!("npm registry request failed with status {status}"),
                    ));
                }
                Ok(Err(_e)) if attempt < max_attempts => {
                    let delay = Duration::from_millis(200 * attempt);
                    let remaining = remaining_http_budget(deadline).map_err(|e| err(None, e))?;
                    if delay >= remaining {
                        return Err(err(
                            None,
                            format!(
                                "npm registry request exceeded the {}-second wall-clock budget",
                                HTTP_RETRY_WALL_CLOCK_BUDGET.as_secs()
                            ),
                        ));
                    }
                    tokio::time::sleep(delay).await;
                    continue;
                }
                Ok(Err(e)) => {
                    return Err(err(None, format!("npm registry request failed: {e}")));
                }
                Err(_) => {
                    return Err(err(
                        None,
                        format!(
                            "npm registry request exceeded the {}-second wall-clock budget",
                            HTTP_RETRY_WALL_CLOCK_BUDGET.as_secs()
                        ),
                    ));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Serves a single canned HTTP response on an ephemeral port and returns
    /// the URL. The client request is drained so the server can respond.
    fn serve(response: &'static str) -> String {
        serve_many(vec![response])
    }

    /// Serves one canned HTTP response per accepted connection, in sequence.
    fn serve_many(responses: Vec<&'static str>) -> String {
        serve_many_bytes(
            responses
                .into_iter()
                .map(|response| response.as_bytes().to_vec())
                .collect(),
        )
    }

    fn serve_many_bytes(responses: Vec<Vec<u8>>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for response in responses {
                if let Ok((mut stream, _)) = listener.accept() {
                    let mut buf = [0u8; 2048];
                    let _ = stream.read(&mut buf); // drain the request line + headers
                    let _ = stream.write_all(&response);
                    let _ = stream.flush();
                }
            }
        });
        format!("http://{addr}/")
    }

    /// Like `serve_many`, but accepts two connections in sequence: the first
    /// gets `first`, the second gets `second`. Used to simulate a redirect
    /// followed by the real response.
    fn serve_then(first: &'static str, second: &'static str) -> String {
        serve_many(vec![first, second])
    }

    fn serve_then_bytes(first: &'static str, second: Vec<u8>) -> String {
        serve_many_bytes(vec![first.as_bytes().to_vec(), second])
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn not_modified_is_reported_not_redirected() {
        // Regression: is_redirection() includes 304, so the 304 branch must be
        // checked first. A stale-cache re-validation must yield NotModified.
        let url = serve("HTTP/1.1 304 Not Modified\r\nContent-Length: 0\r\n\r\n");
        let client = ReqwestHttpClient::new().unwrap();
        runtime().block_on(async {
            let resp = client
                .send_no_follow(&Url::parse(&url).unwrap(), HeaderMap::new())
                .await
                .unwrap();
            assert!(matches!(resp, SendResponse::NotModified));
        });
    }

    #[test]
    fn redirect_returns_location_header() {
        let url = serve(
      "HTTP/1.1 301 Moved Permanently\r\nLocation: https://example.com/new\r\nContent-Length: 0\r\n\r\n",
    );
        let client = ReqwestHttpClient::new().unwrap();
        runtime().block_on(async {
            let resp = client
                .send_no_follow(&Url::parse(&url).unwrap(), HeaderMap::new())
                .await
                .unwrap();
            match resp {
                SendResponse::Redirect(headers) => {
                    assert_eq!(headers.get(LOCATION).unwrap(), "https://example.com/new");
                }
                other => panic!("expected redirect, got {other:?}"),
            }
        });
    }

    #[test]
    fn success_returns_body() {
        let url =
            serve("HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Type: text/plain\r\n\r\nhello");
        let client = ReqwestHttpClient::new().unwrap();
        runtime().block_on(async {
            let resp = client
                .send_no_follow(&Url::parse(&url).unwrap(), HeaderMap::new())
                .await
                .unwrap();
            match resp {
                SendResponse::Success(headers, body) => {
                    assert_eq!(body, b"hello");
                    assert_eq!(headers.get("content-type").unwrap(), "text/plain");
                }
                other => panic!("expected success, got {other:?}"),
            }
        });
    }

    #[test]
    fn not_found_is_an_error() {
        let url = serve("HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
        let client = ReqwestHttpClient::new().unwrap();
        runtime().block_on(async {
            let err = client
                .send_no_follow(&Url::parse(&url).unwrap(), HeaderMap::new())
                .await
                .unwrap_err();
            assert!(matches!(err, SendError::NotFound));
        });
    }

    #[test]
    fn other_error_status_is_reported() {
        // A 5xx is retried up to 3 attempts; once the attempts are exhausted
        // the status itself is reported. Connection: close forces a fresh
        // connection per attempt so the mock serves exactly one response each.
        let five_hundred =
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let url = serve_many(vec![five_hundred, five_hundred, five_hundred]);
        let client = ReqwestHttpClient::new().unwrap();
        runtime().block_on(async {
            let err = client
                .send_no_follow(&Url::parse(&url).unwrap(), HeaderMap::new())
                .await
                .unwrap_err();
            match err {
                SendError::StatusCode(status) => {
                    assert_eq!(status, http::StatusCode::INTERNAL_SERVER_ERROR)
                }
                other => panic!("expected status code error, got {other:?}"),
            }
        });
    }

    #[test]
    fn body_over_module_cap_is_an_error() {
        // P2: an oversized stream (unknown length, as chunked responses
        // arrive on the wire) trips the module cap without transferring
        // 256MiB+ over a socket. reqwest::Response::from infers
        // content_length from a concrete Vec body, so the stream keeps the
        // test honest about the chunked (no Content-Length) case.
        use futures_util::stream;
        use reqwest::Body;
        let oversized = http::Response::builder()
            .status(200)
            .body(Body::wrap_stream(stream::iter([Ok::<_, std::io::Error>(
                bytes::Bytes::from(vec![0u8; (256 << 20) + 1]),
            )])))
            .unwrap();
        let mut response = reqwest::Response::from(oversized);
        runtime().block_on(async {
            let err = read_body_limited(&mut response, MAX_RESPONSE_BODY_BYTES)
                .await
                .unwrap_err();
            assert!(err.contains("limit"), "unexpected error: {err}");
        });
    }

    #[test]
    fn body_budget_requires_explicit_tgz_path() {
        // A `.tgz` path is the only evidence that permits the larger budget;
        // Content-Length is deliberately ignored for tier selection.
        let large = Some(u64::MAX);
        assert_eq!(npm_body_limit(true, large), MAX_TARBALL_BODY_BYTES);
        assert_eq!(npm_body_limit(false, large), MAX_RESPONSE_BODY_BYTES);
        assert_eq!(
            npm_body_limit(false, Some((256 << 20) as u64 + 1)),
            MAX_RESPONSE_BODY_BYTES
        );
        // An explicitly named tarball remains eligible even when chunked.
        assert_eq!(npm_body_limit(true, None), MAX_TARBALL_BODY_BYTES);
    }

    #[test]
    fn read_body_limited_respects_the_passed_limit() {
        // The limit is a caller-provided parameter now (module fetches and npm
        // downloads diverge): a small custom limit must trip on a small body.
        let small = http::Response::builder()
            .status(200)
            .body(reqwest::Body::from(vec![0u8; 16]))
            .unwrap();
        let mut response = reqwest::Response::from(small);
        runtime().block_on(async {
            let err = read_body_limited(&mut response, 4).await.unwrap_err();
            assert!(err.contains("limit"), "unexpected error: {err}");
        });
    }

    #[test]
    fn npm_download_follows_redirect() {
        // A registry 302 to a relative Location must be followed to the target
        // and yield the final body (the client itself uses Policy::none).
        let url = serve_then(
            "HTTP/1.1 302 Found\r\nLocation: /target\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Type: application/octet-stream\r\n\r\nhello",
        );
        let client = ReqwestHttpClient::new().unwrap();
        runtime().block_on(async {
            let resp = client
                .download_with_retries_on_any_tokio_runtime(
                    Url::parse(&url).unwrap(),
                    None,
                    None,
                    None,
                )
                .await
                .unwrap();
            match resp {
                NpmCacheHttpClientResponse::Bytes(bytes) => assert_eq!(bytes.bytes, b"hello"),
                _ => panic!("expected npm registry bytes response"),
            }
        });
    }

    #[test]
    fn npm_download_preserves_tarball_identity_through_opaque_redirect() {
        let mut body = vec![0x1f, 0x8b]; // gzip magic
        body.extend_from_slice(&[0u8; 8]);
        body.extend_from_slice(&((1u32 << 30) + 1).to_le_bytes()); // inflated ISIZE
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(&body);
        let url = serve_then_bytes(
            "HTTP/1.1 302 Found\r\nLocation: /cdn/opaque\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            response,
        );
        let client = ReqwestHttpClient::new().unwrap();
        let original_url = Url::parse(&format!("{url}pkg/-/pkg-1.0.0.tgz")).unwrap();
        runtime().block_on(async {
            let result = client
                .download_with_retries_on_any_tokio_runtime(original_url, None, None, None)
                .await;
            let err = match result {
                Err(err) => err,
                Ok(_) => panic!("expected the inflated tarball trailer to be rejected"),
            };
            let message = err.to_string();
            assert!(
                message.contains("decompressed bytes") && message.contains("budget"),
                "unexpected error: {message}"
            );
        });
    }

    #[test]
    fn isize_guard_rejects_inflated_trailer() {
        // A small tarball whose gzip ISIZE trailer claims ~4 GiB must be
        // rejected at download time (upstream extraction would try_reserve
        // that much from a publisher-controlled header).
        let mut bytes = vec![0x1f, 0x8b]; // gzip magic
        bytes.extend_from_slice(&[0u8; 100]);
        bytes.extend_from_slice(&((1u32 << 30) + 1).to_le_bytes()); // ISIZE > 1 GiB budget
        let err = guard_tarball_isize(true, bytes).unwrap_err();
        assert!(
            err.contains("decompressed bytes") && err.contains("budget"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn isize_guard_passes_small_trailer() {
        let mut bytes = vec![0x1f, 0x8b];
        bytes.extend_from_slice(&[0u8; 100]);
        bytes.extend_from_slice(&1024u32.to_le_bytes());
        assert!(guard_tarball_isize(true, bytes).is_ok());
    }

    #[test]
    fn isize_guard_checks_the_last_member_of_a_multi_member_tarball() {
        // ISIZE is a per-member trailer. Keep this pre-check deliberately
        // coarse (the extractor remains responsible for full gzip parsing),
        // but make sure an inflated final member still trips it.
        let mut bytes = vec![0x1f, 0x8b];
        bytes.extend_from_slice(&[0u8; 100]);
        bytes.extend_from_slice(&1024u32.to_le_bytes());
        bytes.extend_from_slice(&[0x1f, 0x8b]);
        bytes.extend_from_slice(&[0u8; 100]);
        bytes.extend_from_slice(&((1u32 << 30) + 1).to_le_bytes());
        assert!(guard_tarball_isize(true, bytes).is_err());
    }

    #[test]
    fn isize_guard_ignores_non_tarball_paths() {
        // Registry metadata (JSON) must never be size-checked: its trailer
        // bytes are not an ISIZE and would spuriously fail.
        let mut bytes = vec![0x1f, 0x8b];
        bytes.extend_from_slice(&[0u8; 100]);
        bytes.extend_from_slice(&((1u32 << 30) + 1).to_le_bytes());
        assert!(
            guard_tarball_isize(false, bytes).is_ok(),
            "non-.tgz responses must pass through"
        );
    }

    #[test]
    fn isize_guard_ignores_non_gzip_bytes() {
        let bytes = vec![0u8; 64]; // no gzip magic
        assert!(guard_tarball_isize(true, bytes).is_ok());
    }
}
