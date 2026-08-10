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
use url::Url;

#[derive(Debug)]
pub struct ReqwestHttpClient {
    client: reqwest::Client,
}

impl Default for ReqwestHttpClient {
    fn default() -> Self {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("libdeno/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("failed to build http client");
        Self { client }
    }
}

#[async_trait::async_trait(?Send)]
impl HttpClient for ReqwestHttpClient {
    async fn send_no_follow(
        &self,
        url: &Url,
        headers: HeaderMap,
    ) -> Result<SendResponse, SendError> {
        let response = self
            .client
            .get(url.clone())
            .headers(headers)
            .send()
            .await
            .map_err(|e| SendError::Failed(e.into()))?;
        let status = response.status();

        // is_redirection() covers 300..=399 including 304, so the 304 check must
        // come first: a 304 (re-validation of a stale cached module) is not a
        // redirect and carries no Location header.
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
            let body = response
                .bytes()
                .await
                .map_err(|e| SendError::Failed(e.into()))?;
            return Ok(SendResponse::Success(headers, body.to_vec()));
        }

        if status == http::StatusCode::NOT_FOUND {
            return Err(SendError::NotFound);
        }
        Err(SendError::StatusCode(status))
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
        let err = |status_code, message: String| DownloadError {
            status_code,
            error: deno_error::JsErrorBox::generic(message),
        };

        // Use the npmrc credentials when the caller did not supply an explicit
        // auth value (private registries).
        let auth = maybe_auth.or_else(|| {
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
                        let bytes = response
                            .bytes()
                            .await
                            .map_err(|e| {
                                err(None, format!("failed to read npm registry response: {e}"))
                            })?
                            .to_vec();
                        return Ok(NpmCacheHttpClientResponse::Bytes(
                            NpmCacheHttpClientBytesResponse { bytes, etag },
                        ));
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
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf); // drain the request line + headers
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://{addr}/")
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
        let client = ReqwestHttpClient::default();
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
        let client = ReqwestHttpClient::default();
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
        let client = ReqwestHttpClient::default();
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
        let client = ReqwestHttpClient::default();
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
        let url = serve("HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n");
        let client = ReqwestHttpClient::default();
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
}
