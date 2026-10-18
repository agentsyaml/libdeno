use super::*;
use std::io::{Read, Write};
use std::net::TcpListener;
#[cfg(feature = "npm")]
use std::net::TcpStream;
#[cfg(feature = "npm")]
use std::time::{Duration, Instant};

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

#[cfg(feature = "npm")]
// Test scheduling under parallel CI can delay the client after fixture creation.
const HTTP_FIXTURE_TIMEOUT: Duration = Duration::from_secs(15);

#[cfg(feature = "npm")]
fn serve_redirect_with_etag_target() -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut first, _) = accept_with_timeout(&listener).unwrap();
        first.set_read_timeout(Some(HTTP_FIXTURE_TIMEOUT)).unwrap();
        read_request_headers(&mut first).unwrap();
        first.write_all(b"HTTP/1.1 302 Found\r\nLocation: /target\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        let (mut second, _) = accept_with_timeout(&listener).unwrap();
        second.set_read_timeout(Some(HTTP_FIXTURE_TIMEOUT)).unwrap();
        let request = read_request_headers(&mut second).unwrap();
        let response = if String::from_utf8_lossy(&request)
            .to_ascii_lowercase()
            .contains("if-none-match:")
        {
            b"HTTP/1.1 304 Not Modified\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .as_slice()
        } else {
            b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nnew-v2".as_slice()
        };
        second.write_all(response).unwrap();
    });
    (format!("http://{addr}/"), server)
}

#[cfg(feature = "npm")]
fn accept_with_timeout(
    listener: &TcpListener,
) -> std::io::Result<(TcpStream, std::net::SocketAddr)> {
    let deadline = Instant::now() + HTTP_FIXTURE_TIMEOUT;
    loop {
        match listener.accept() {
            Ok((stream, addr)) => {
                stream.set_nonblocking(false)?;
                return Ok((stream, addr));
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(feature = "npm")]
fn read_request_headers(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut headers = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        let count = stream.read(&mut chunk)?;
        if count == 0 || headers.len() + count > 8192 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "incomplete or oversized request headers",
            ));
        }
        headers.extend_from_slice(&chunk[..count]);
        if headers.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            return Ok(headers);
        }
    }
}

/// Like `serve_many`, but accepts two connections in sequence: the first
/// gets `first`, the second gets `second`. Used to simulate a redirect
/// followed by the real response.
#[cfg(feature = "npm")]
fn serve_then(first: &'static str, second: &'static str) -> String {
    serve_many(vec![first, second])
}

#[cfg(feature = "npm")]
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
        let mut headers = HeaderMap::new();
        headers.insert(http::header::IF_NONE_MATCH, "\"v1\"".parse().unwrap());
        let resp = client
            .send_no_follow(&Url::parse(&url).unwrap(), headers)
            .await
            .unwrap();
        assert!(matches!(resp, SendResponse::NotModified));
    });
}

#[test]
fn unsolicited_module_304_is_an_error() {
    let url = serve("HTTP/1.1 304 Not Modified\r\nContent-Length: 0\r\n\r\n");
    let client = ReqwestHttpClient::new().unwrap();
    runtime().block_on(async {
        let error = client
            .send_no_follow(&Url::parse(&url).unwrap(), HeaderMap::new())
            .await
            .unwrap_err();
        assert!(matches!(error, SendError::StatusCode(status) if status == http::StatusCode::NOT_MODIFIED));
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

#[cfg(feature = "npm")]
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

#[cfg(feature = "npm")]
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
            .download_with_retries_on_any_tokio_runtime(Url::parse(&url).unwrap(), None, None, None)
            .await
            .unwrap();
        match resp {
            NpmCacheHttpClientResponse::Bytes(bytes) => assert_eq!(bytes.bytes, b"hello"),
            _ => panic!("expected npm registry bytes response"),
        }
    });
}

#[cfg(feature = "npm")]
#[test]
fn npm_redirect_drops_original_etag_and_fetches_target_content() {
    let (url, server) = serve_redirect_with_etag_target();
    let url = Url::parse(&url).unwrap();
    let client = ReqwestHttpClient::new().unwrap();
    let response = runtime().block_on(async {
        client
            .download_with_retries_on_any_tokio_runtime(
                url,
                None,
                Some("\"old-resource\"".to_string()),
                None,
            )
            .await
    });
    server.join().unwrap();
    match response.unwrap() {
        NpmCacheHttpClientResponse::Bytes(bytes) => assert_eq!(bytes.bytes, b"new-v2"),
        _ => panic!("expected redirected target bytes"),
    }
}

#[cfg(feature = "npm")]
#[test]
fn npm_304_requires_a_conditional_etag_request() {
    let response = "HTTP/1.1 304 Not Modified\r\nETag: \"v1\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let client = ReqwestHttpClient::new().unwrap();
    let url = Url::parse(&serve_many(vec![response, response])).unwrap();
    runtime().block_on(async {
        let error = match client
            .download_with_retries_on_any_tokio_runtime(url.clone(), None, None, None)
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("304 without request ETag was accepted"),
        };
        assert_eq!(error.status_code, Some(304));
        assert!(error.to_string().contains("without a conditional ETag"));
        assert!(matches!(
            client
                .download_with_retries_on_any_tokio_runtime(
                    url,
                    None,
                    Some("\"v1\"".to_string()),
                    None,
                )
                .await
                .unwrap(),
            NpmCacheHttpClientResponse::NotModified
        ));
    });
    let redirect = "HTTP/1.1 302 Found\r\nLocation: /metadata\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let redirected = "HTTP/1.1 304 Not Modified\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let url = Url::parse(&serve_then(redirect, redirected)).unwrap();
    let error = match runtime()
        .block_on(client.download_with_retries_on_any_tokio_runtime(url, None, None, None))
    {
        Err(error) => error,
        Ok(_) => panic!("redirected 304 without request ETag was accepted"),
    };
    assert_eq!(error.status_code, Some(304));
}

#[cfg(feature = "npm")]
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

#[cfg(feature = "npm")]
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

#[cfg(feature = "npm")]
#[test]
fn isize_guard_passes_small_trailer() {
    let mut bytes = vec![0x1f, 0x8b];
    bytes.extend_from_slice(&[0u8; 100]);
    bytes.extend_from_slice(&1024u32.to_le_bytes());
    assert!(guard_tarball_isize(true, bytes).is_ok());
}

#[cfg(feature = "npm")]
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

#[cfg(feature = "npm")]
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

#[cfg(feature = "npm")]
#[test]
fn isize_guard_ignores_non_gzip_bytes() {
    let bytes = vec![0u8; 64]; // no gzip magic
    assert!(guard_tarball_isize(true, bytes).is_ok());
}
