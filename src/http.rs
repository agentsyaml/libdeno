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
/// failing once the accumulated size exceeds the per-request cap instead of
/// buffering an unbounded response. The cap is chosen from the declared
/// Content-Length: a response claiming more than the module cap is a
/// tarball-sized download (npm packages) and gets the larger tarball bound;
/// everything else — and every chunked response without Content-Length —
/// stays on the module cap. Over-limit is an error, never a retry.
async fn read_body_limited(response: &mut reqwest::Response) -> Result<Vec<u8>, String> {
    let limit = match response.content_length() {
        Some(len) if len as usize > MAX_RESPONSE_BODY_BYTES => MAX_TARBALL_BODY_BYTES,
        _ => MAX_RESPONSE_BODY_BYTES,
    };
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
                        let body = read_body_limited(&mut response)
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
                        let bytes = read_body_limited(&mut response).await.map_err(|e| {
                            err(None, format!("failed to read npm registry response: {e}"))
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
}
