// Minimal HTTP client for deno_cache_dir module and npm fetches.

use deno_cache_dir::file_fetcher::HttpClient;
use deno_cache_dir::file_fetcher::SendError;
use deno_cache_dir::file_fetcher::SendResponse;
#[cfg(feature = "npm")]
use deno_npm_cache::DownloadError;
#[cfg(feature = "npm")]
use deno_npm_cache::NpmCacheHttpClient;
#[cfg(feature = "npm")]
use deno_npm_cache::NpmCacheHttpClientBytesResponse;
#[cfg(feature = "npm")]
use deno_npm_cache::NpmCacheHttpClientResponse;
#[cfg(feature = "npm")]
use deno_npmrc::RegistryConfig;
#[cfg(feature = "npm")]
use http::header::AUTHORIZATION;
#[cfg(feature = "npm")]
use http::header::ETAG;
#[cfg(feature = "npm")]
use http::header::IF_NONE_MATCH;
use http::header::LOCATION;
use http::HeaderMap;
use std::time::Duration;
use std::time::Instant;
use url::Url;

use crate::LibdenoError;

/// Cap on the decompressed response body buffered for module fetches;
/// reqwest decodes HTTP Content-Encoding before this limit is checked.
const MAX_RESPONSE_BODY_BYTES: usize = 256 << 20;
/// Bounded response-body cap for explicit npm `.tgz` downloads, whose bodies
/// remain compressed; this and the ISIZE check do not cap extraction allocations.
#[cfg(feature = "npm")]
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
    /// All request paths share this client and its connection pool.
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

/// Buffers the response body up to the caller-supplied limit.
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
#[cfg(feature = "npm")]
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

/// Default threshold for the trailing-ISIZE hint on npm tarballs, configurable
/// with `LIBDENO_MAX_TARBALL_DECOMPRESSED_BYTES`. This threshold is not a
/// decompressed-output budget.
#[cfg(feature = "npm")]
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
/// mod 2^32) claims a member size above the configured hint threshold.
///
/// This is a coarse guard, not a precise bound: ISIZE only reflects the last
/// gzip member (multi-member tarballs can decompress larger in total) and
/// wraps at 4 GiB. It exists to block the cheap malicious case — a tiny
/// tarball declaring ~4 GiB that would make upstream's `try_reserve` commit
/// that much virtual memory before the real decompressed size is known. This is
/// only a trailer pre-check: upstream's actual decompression/read_to_end path
/// has no hard decompressed-byte budget, so neither this check nor the
/// reqwest-returned response-body cap above establishes a decompressed-memory bound.
#[cfg(feature = "npm")]
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
                        return if headers.contains_key(http::header::IF_NONE_MATCH) {
                            Ok(SendResponse::NotModified)
                        } else {
                            Err(SendError::StatusCode(status))
                        };
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

#[cfg(feature = "npm")]
#[async_trait::async_trait(?Send)]
impl NpmCacheHttpClient for ReqwestHttpClient {
    async fn download_with_retries_on_any_tokio_runtime(
        &self,
        url: Url,
        maybe_auth: Option<String>,
        mut maybe_etag: Option<String>,
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
                        return if maybe_etag.is_some() {
                            Ok(NpmCacheHttpClientResponse::NotModified)
                        } else {
                            Err(err(
                                Some(status.as_u16()),
                                "npm registry returned 304 without a conditional ETag request"
                                    .to_string(),
                            ))
                        };
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
                        if next != url {
                            maybe_etag = None;
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
#[path = "http/tests.rs"]
mod tests;
