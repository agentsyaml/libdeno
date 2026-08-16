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
use url::Url;

use crate::LibdenoError;

/// Cap on the decompressed response body we buffer for module fetches,
/// guarding against decompression bombs. reqwest auto-decompresses
/// gzip/brotli (enabled in Cargo.toml) at the transport layer before the body
/// reaches us, so this counts true memory usage, not the wire size.
const MAX_RESPONSE_BODY_BYTES: usize = 256 << 20;
/// Cap for npm tarball downloads: some legal packages exceed 256MiB, and
/// registries serve tarballs with an explicit Content-Length, so a response
/// claiming more than the module cap gets this larger bound. Still bounded,
/// so a broken/oversized response fails fast instead of OOMing the host.
///
/// This cap counts only the downloaded (compressed) bytes; the decompressed
/// allocation is NOT size-checked. Upstream tarball_extract reserves space
/// from the gzip stream's ISIZE header, which is written by the publisher —
/// a malicious registry can serve a small tarball (under this cap) whose
/// ISIZE claims ~4GiB, triggering a try_reserve of that size and OOM risk.
/// The registry must therefore be trusted; a hardened build would need to
/// pre-validate ISIZE against a post-decompression budget.
const MAX_TARBALL_BODY_BYTES: usize = 1 << 30;

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
    /// [`read_body_limited`]. Because gzip/brotli auto-decompression runs at the
    /// transport layer, the bytes that helper sees are already decompressed and
    /// the cap doubles as a decompression-bomb guard.
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

/// Reads the full (already auto-decompressed) response body into a Vec,
/// failing once the accumulated size exceeds the caller-supplied `limit`
/// instead of buffering an unbounded response. The caller picks the bound:
/// module fetches pin the module cap, npm registry downloads tier by declared
/// Content-Length via [`npm_body_limit`]. Over-limit is an error, never a
/// retry.
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
        if body.len() + chunk.len() > limit {
            return Err(format!("response body exceeded the {limit}-byte limit"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Body cap for npm registry downloads: responses declaring more than the
/// module cap are tarball-sized and get the larger tarball bound; everything
/// else — and every chunked response without Content-Length — stays on the
/// module cap.
///
/// ponytail: still trusts the *declared* Content-Length for tiering — a
/// malicious or compromised registry can claim a huge length on any response
/// (metadata included) and win the 1 GiB budget, so a decompression bomb up
/// to 1 GiB remains possible. The cap itself is never exceeded. Tighten to
/// tarball-path checks only if this is abused in practice.
fn npm_body_limit(content_length: Option<u64>) -> usize {
    match content_length {
        Some(len) if len as usize > MAX_RESPONSE_BODY_BYTES => MAX_TARBALL_BODY_BYTES,
        _ => MAX_RESPONSE_BODY_BYTES,
    }
}

/// Default post-decompression budget for npm tarballs, and the env override
/// (`LIBDENO_MAX_TARBALL_DECOMPRESSED_BYTES`). Downstream extraction reserves
/// from the gzip ISIZE header, which the publisher writes — a malicious
/// registry could serve a small tarball whose ISIZE claims ~4 GiB and make
/// the extractor try_reserve that much. This pre-check rejects such tarballs
/// at download time; the budget is deliberately configurable so hosts with
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
fn guard_tarball_isize(path: &str, bytes: Vec<u8>) -> Result<Vec<u8>, String> {
    // Only .tgz URLs carry gzip-compressed tar data; registry metadata
    // (JSON, possibly transport-gzip-encoded) must not be size-checked here —
    // its trailer bytes are not an ISIZE and would spuriously fail.
    if !path.ends_with(".tgz") || bytes.len() < 8 || bytes[..2] != [0x1f, 0x8b] {
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
        loop {
            attempt += 1;
            match self
                .client
                .get(url.clone())
                .headers(headers.clone())
                .send()
                .await
            {
                Ok(response) => {
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
                        let body = read_body_limited(&mut response, MAX_RESPONSE_BODY_BYTES)
                            .await
                            .map_err(|e| SendError::Failed(e.into()))?;
                        return Ok(SendResponse::Success(headers, body));
                    }

                    if status == http::StatusCode::NOT_FOUND {
                        return Err(SendError::NotFound);
                    }
                    if (status.as_u16() == 429 || status.is_server_error())
                        && attempt < max_attempts
                    {
                        tokio::time::sleep(std::time::Duration::from_millis(200 * attempt)).await;
                        continue;
                    }
                    return Err(SendError::StatusCode(status));
                }
                Err(_) if attempt < max_attempts => {
                    tokio::time::sleep(std::time::Duration::from_millis(200 * attempt)).await;
                    continue;
                }
                Err(e) => return Err(SendError::Failed(e.into())),
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
        loop {
            attempt += 1;
            let mut request = self.client.get(url.clone());
            if let Some(auth) = &auth {
                request = request.header(AUTHORIZATION, auth);
            }
            if let Some(etag) = &maybe_etag {
                request = request.header(IF_NONE_MATCH, etag);
            }

            match request.send().await {
                Ok(response) => {
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
                        let limit = npm_body_limit(response.content_length());
                        let bytes = read_body_limited(&mut response, limit).await.map_err(|e| {
                            err(None, format!("failed to read npm registry response: {e}"))
                        })?;
                        let bytes = guard_tarball_isize(url.path(), bytes).map_err(|e| {
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
                        let delay = std::time::Duration::from_millis(200 * attempt);
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Err(err(
                        Some(status.as_u16()),
                        format!("npm registry request failed with status {status}"),
                    ));
                }
                Err(e) => {
                    if attempt < max_attempts {
                        let delay = std::time::Duration::from_millis(200 * attempt);
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Err(err(None, format!("npm registry request failed: {e}")));
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
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for response in responses {
                if let Ok((mut stream, _)) = listener.accept() {
                    let mut buf = [0u8; 2048];
                    let _ = stream.read(&mut buf); // drain the request line + headers
                    let _ = stream.write_all(response.as_bytes());
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
    fn declared_large_content_length_gets_tarball_cap() {
        // P2: Content-Length tiering now lives only in `npm_body_limit`, for
        // npm registry downloads: a response claiming more than the module cap
        // gets the 1GiB tarball bound, everything else — and every chunked
        // response without Content-Length — stays on the module cap.
        //
        // `npm_body_limit` takes the raw content length because
        // `reqwest::Response::from` re-derives it from a concrete Vec body and
        // drops a declared header (a stream body yields None), so no mock
        // response can carry an arbitrary declared length into the helper.
        //
        // The module-fetch path no longer reads Content-Length to pick its
        // bound: `send_no_follow` pins every module request to the module cap,
        // so a malicious module server declaring a huge Content-Length cannot
        // buy itself a 1GiB budget. A network test proving that pin would need
        // to push >256MiB over a socket, so it is asserted here structurally
        // (the fixed call site) instead.
        let size = ((256 << 20) + 1) as u64;
        assert_eq!(npm_body_limit(Some(size)), MAX_TARBALL_BODY_BYTES);
        // A module-cap-sized declaration stays on the module cap.
        assert_eq!(npm_body_limit(Some(1024)), MAX_RESPONSE_BODY_BYTES);
        // Chunked (no Content-Length) stays on the module cap.
        assert_eq!(npm_body_limit(None), MAX_RESPONSE_BODY_BYTES);
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
    fn isize_guard_rejects_inflated_trailer() {
        // A small tarball whose gzip ISIZE trailer claims ~4 GiB must be
        // rejected at download time (upstream extraction would try_reserve
        // that much from a publisher-controlled header).
        let mut bytes = vec![0x1f, 0x8b]; // gzip magic
        bytes.extend_from_slice(&[0u8; 100]);
        bytes.extend_from_slice(&((1u32 << 30) + 1).to_le_bytes()); // ISIZE > 1 GiB budget
        let err = guard_tarball_isize("/pkg/-/pkg-1.0.0.tgz", bytes).unwrap_err();
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
        assert!(guard_tarball_isize("/pkg/-/pkg-1.0.0.tgz", bytes).is_ok());
    }

    #[test]
    fn isize_guard_ignores_non_tarball_paths() {
        // Registry metadata (JSON) must never be size-checked: its trailer
        // bytes are not an ISIZE and would spuriously fail.
        let mut bytes = vec![0x1f, 0x8b];
        bytes.extend_from_slice(&[0u8; 100]);
        bytes.extend_from_slice(&((1u32 << 30) + 1).to_le_bytes());
        assert!(
            guard_tarball_isize("/pkg/pkg-1.0.0", bytes).is_ok(),
            "non-.tgz responses must pass through"
        );
    }

    #[test]
    fn isize_guard_ignores_non_gzip_bytes() {
        let bytes = vec![0u8; 64]; // no gzip magic
        assert!(guard_tarball_isize("/pkg/-/pkg-1.0.0.tgz", bytes).is_ok());
    }
}
