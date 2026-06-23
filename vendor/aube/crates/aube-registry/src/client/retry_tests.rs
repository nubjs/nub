//! End-to-end tests for [`RegistryClient::send_with_retry`] via the
//! real fetch entry points. Uses `wiremock` as a local HTTP fixture
//! so we can exercise 5xx / 429 / slow responses without touching
//! the network.
//!
//! Each test spins up a fresh `MockServer` and a `RegistryClient`
//! pointing at it, then asserts request counts + returned values.
//! Timeouts use sub-second values so the suite stays fast.
use super::*;
use crate::config::FetchPolicy;
use crate::{Error, Packument};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client_with(server: &MockServer, policy: FetchPolicy) -> RegistryClient {
    let config = NpmConfig {
        registry: format!("{}/", server.uri()),
        ..Default::default()
    };
    RegistryClient::from_config_with_policy(config, policy)
}

fn client_with_public_npmjs_host(server: &MockServer) -> RegistryClient {
    let config = NpmConfig {
        registry: format!("http://registry.npmjs.org:{}/", server.address().port()),
        ..Default::default()
    };
    let mut client = RegistryClient::from_config(config);
    client.http = reqwest::Client::builder()
        .resolve("registry.npmjs.org", *server.address())
        .build()
        .unwrap();
    client
}

fn make_packument_json() -> serde_json::Value {
    serde_json::json!({
        "name": "demo",
        "versions": {},
        "dist-tags": {},
    })
}

#[tokio::test]
async fn retries_on_503_then_succeeds() {
    let server = MockServer::start().await;
    // Two 503s, then a 200. `retries = 2` allows 3 total attempts,
    // so the third one gets through.
    Mock::given(method("GET"))
        .and(path("/demo"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/demo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(make_packument_json()))
        .mount(&server)
        .await;

    let policy = FetchPolicy {
        timeout_ms: 5_000,
        retries: 2,
        retry_factor: 1,
        retry_min_timeout_ms: 1,
        retry_max_timeout_ms: 1,
        ..FetchPolicy::default()
    };
    let client = client_with(&server, policy);
    let packument = client
        .fetch_packument("demo")
        .await
        .expect("retry recovery");
    assert_eq!(packument.name, "demo");

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3, "expected 3 attempts (2 retries)");
}

#[tokio::test]
async fn dist_tag_writes_send_web_auth_for_public_npmjs() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/-/package/demo/dist-tags/beta"))
        .and(header("npm-auth-type", "web"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/-/package/demo/dist-tags/beta"))
        .and(header("npm-auth-type", "web"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let client = client_with_public_npmjs_host(&server);
    client
        .put_dist_tag("demo", "beta", "1.2.3", None)
        .await
        .expect("put dist-tag should succeed");
    client
        .delete_dist_tag("demo", "beta", None)
        .await
        .expect("delete dist-tag should succeed");
}

#[tokio::test]
async fn dist_tag_writes_send_web_auth_and_otp_for_public_npmjs() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/-/package/demo/dist-tags/beta"))
        .and(header("npm-auth-type", "web"))
        .and(header("npm-otp", "123456"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/-/package/demo/dist-tags/beta"))
        .and(header("npm-auth-type", "web"))
        .and(header("npm-otp", "654321"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let client = client_with_public_npmjs_host(&server);
    client
        .put_dist_tag("demo", "beta", "1.2.3", Some("123456"))
        .await
        .expect("put dist-tag should succeed");
    client
        .delete_dist_tag("demo", "beta", Some("654321"))
        .await
        .expect("delete dist-tag should succeed");
}

#[tokio::test]
async fn dist_tag_writes_send_otp_header_without_web_auth_for_custom_registry() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/-/package/demo/dist-tags/beta"))
        .and(header("npm-otp", "123456"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/-/package/demo/dist-tags/beta"))
        .and(header("npm-otp", "654321"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let client = client_with(&server, FetchPolicy::default());
    client
        .put_dist_tag("demo", "beta", "1.2.3", Some("123456"))
        .await
        .expect("put dist-tag should succeed");
    client
        .delete_dist_tag("demo", "beta", Some("654321"))
        .await
        .expect("delete dist-tag should succeed");

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests {
        assert!(
            !request.headers.contains_key("npm-auth-type"),
            "custom registries should not receive npm-auth-type"
        );
    }
}

#[tokio::test]
async fn dist_tag_writes_omit_otp_header_when_absent() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/-/package/demo/dist-tags/beta"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/-/package/demo/dist-tags/beta"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let client = client_with(&server, FetchPolicy::default());
    client
        .put_dist_tag("demo", "beta", "1.2.3", None)
        .await
        .expect("put dist-tag should succeed");
    client
        .delete_dist_tag("demo", "beta", None)
        .await
        .expect("delete dist-tag should succeed");

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests {
        assert!(
            !request.headers.contains_key("npm-otp"),
            "npm-otp should be omitted when no OTP is provided"
        );
    }
}

#[tokio::test]
async fn retry_exhaustion_surfaces_final_5xx() {
    let server = MockServer::start().await;
    // retries=1 ⇒ 2 total attempts, both 503.
    Mock::given(method("GET"))
        .and(path("/demo"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let policy = FetchPolicy {
        timeout_ms: 5_000,
        retries: 1,
        retry_factor: 1,
        retry_min_timeout_ms: 1,
        retry_max_timeout_ms: 1,
        ..FetchPolicy::default()
    };
    let client = client_with(&server, policy);
    let err = client
        .fetch_packument("demo")
        .await
        .expect_err("exhausted retries should error");
    // reqwest surfaces non-2xx as `reqwest::Error` via
    // `error_for_status`, wrapped in our `Error::Http`.
    match err {
        Error::Http(inner) => assert_eq!(inner.status().map(|s| s.as_u16()), Some(503)),
        other => panic!("unexpected error: {other}"),
    }

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2, "retries=1 means 2 total attempts");
}

#[tokio::test]
async fn non_retriable_4xx_does_not_retry() {
    let server = MockServer::start().await;
    // 404 is a terminal signal the caller needs, not a transient
    // failure. The retry helper must short-circuit after one try.
    Mock::given(method("GET"))
        .and(path("/missing"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let policy = FetchPolicy {
        timeout_ms: 5_000,
        retries: 3,
        retry_factor: 1,
        retry_min_timeout_ms: 1,
        retry_max_timeout_ms: 1,
        ..FetchPolicy::default()
    };
    let client = client_with(&server, policy);
    let err = client
        .fetch_packument("missing")
        .await
        .expect_err("404 should surface");
    assert!(matches!(err, Error::NotFound(_)));

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1, "404 must not trigger retries");
}

#[tokio::test]
async fn fetch_packument_rejects_declared_body_over_cap() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let Ok((mut sock, _)) = listener.accept().await else {
            return;
        };
        let mut buf = [0u8; 1024];
        let _ = sock.read(&mut buf).await;
        let _ = sock
            .write_all(
                b"HTTP/1.1 200 OK\r\n\
                  Content-Length: 1048576\r\n\
                  Content-Type: application/json\r\n\r\n\
                  {\"name\":\"demo\",\"versions\":{},\"dist-tags\":{}}",
            )
            .await;
    });

    let policy = FetchPolicy {
        timeout_ms: 5_000,
        packument_max_bytes: 10,
        ..FetchPolicy::default()
    };
    let config = NpmConfig {
        registry: format!("http://{addr}/"),
        ..Default::default()
    };
    let client = RegistryClient::from_config_with_policy(config, policy);
    let err = client
        .fetch_packument("demo")
        .await
        .expect_err("declared oversized packument should be rejected");

    match err {
        Error::Io(inner) => assert!(
            inner
                .to_string()
                .contains("Content-Length 1048576 exceeds cap 10"),
            "unexpected error: {inner}"
        ),
        other => panic!("unexpected error: {other}"),
    }
}

#[tokio::test]
async fn retry_after_header_on_429_overrides_computed_backoff() {
    // Server asks for a 0-second wait explicitly; our default
    // backoff would be >= mintimeout (1ms here, but production
    // defaults are 10s). If the Retry-After header is honored,
    // the test completes essentially instantly; if it's ignored,
    // the test still passes with tight policy but via a different
    // code path. We assert the helper parses the header correctly
    // by also checking a distinct header value routes through.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/demo"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/demo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(make_packument_json()))
        .mount(&server)
        .await;

    // Set the computed backoff extremely high so a test that
    // *ignored* Retry-After would timeout. We then put a short
    // tokio timeout around the call: if Retry-After is honored
    // (0s), the call completes well within 2s; otherwise it hits
    // the 60s default backoff and the timeout fires.
    let policy = FetchPolicy {
        timeout_ms: 5_000,
        retries: 2,
        retry_factor: 1,
        retry_min_timeout_ms: 60_000,
        retry_max_timeout_ms: 60_000,
        ..FetchPolicy::default()
    };
    let client = client_with(&server, policy);
    let packument = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        client.fetch_packument("demo"),
    )
    .await
    .expect("Retry-After should be honored, overriding the 60s default backoff")
    .expect("request should succeed");
    assert_eq!(packument.name, "demo");
}

#[tokio::test]
async fn retries_on_429_rate_limit() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/demo"))
        .respond_with(ResponseTemplate::new(429))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/demo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(make_packument_json()))
        .mount(&server)
        .await;

    let policy = FetchPolicy {
        timeout_ms: 5_000,
        retries: 2,
        retry_factor: 1,
        retry_min_timeout_ms: 1,
        retry_max_timeout_ms: 1,
        ..FetchPolicy::default()
    };
    let client = client_with(&server, policy);
    let packument = client.fetch_packument("demo").await.expect("429 retry");
    assert_eq!(packument.name, "demo");

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
}

#[tokio::test]
async fn tarball_fetch_requests_identity_encoding() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/pkg.tgz"))
        .and(header("accept-encoding", "identity"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"tgz bytes".to_vec()))
        .expect(1)
        .mount(&server)
        .await;

    let policy = FetchPolicy {
        timeout_ms: 5_000,
        retries: 0,
        retry_factor: 1,
        retry_min_timeout_ms: 1,
        retry_max_timeout_ms: 1,
        ..FetchPolicy::default()
    };
    let client = client_with(&server, policy);
    let url = format!("{}/pkg.tgz", server.uri());
    let bytes = client
        .fetch_tarball_bytes(&url)
        .await
        .expect("tarball fetch should succeed");
    assert_eq!(&bytes[..], b"tgz bytes");
}

#[tokio::test]
async fn start_tarball_stream_retries_on_503_then_succeeds() {
    // Streaming tarball fetch used to skip retry entirely — a single
    // 503/connect-time hiccup from the registry would propagate
    // straight back to the caller. The initial-request retry covers
    // 5xx + transport errors before any chunks have streamed, while
    // still leaving mid-stream errors to the caller (which falls
    // back to the buffered fetch_tarball_bytes_streaming_sha512 path).
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/pkg.tgz"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/pkg.tgz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"tgz bytes".to_vec()))
        .mount(&server)
        .await;

    let policy = FetchPolicy {
        timeout_ms: 5_000,
        retries: 2,
        retry_factor: 1,
        retry_min_timeout_ms: 1,
        retry_max_timeout_ms: 1,
        ..FetchPolicy::default()
    };
    let client = client_with(&server, policy);
    let url = format!("{}/pkg.tgz", server.uri());
    let resp = client
        .start_tarball_stream(&url)
        .await
        .expect("retry recovery");
    assert_eq!(resp.status(), 200);

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3, "expected 3 attempts (2 retries)");
}

#[tokio::test]
async fn fetch_timeout_triggers_transport_error() {
    let server = MockServer::start().await;
    // Server delays 500ms; client timeout is 50ms. Every attempt
    // must time out before the body arrives. With retries=0 we get
    // exactly one attempt and a transport error.
    Mock::given(method("GET"))
        .and(path("/demo"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(make_packument_json())
                .set_delay(std::time::Duration::from_millis(500)),
        )
        .mount(&server)
        .await;

    let policy = FetchPolicy {
        timeout_ms: 50,
        retries: 0,
        retry_factor: 1,
        retry_min_timeout_ms: 1,
        retry_max_timeout_ms: 1,
        ..FetchPolicy::default()
    };
    let client = client_with(&server, policy);
    let err = client
        .fetch_packument("demo")
        .await
        .expect_err("timeout should surface");
    match err {
        Error::Http(inner) => assert!(
            inner.is_timeout() || inner.is_request(),
            "expected timeout-shaped reqwest error, got {inner:?}",
        ),
        other => panic!("unexpected error: {other}"),
    }
}

#[tokio::test]
async fn tarball_headers_timeout_retries_at_most_once_even_with_high_retry_budget() {
    // Headers-stage timeout: server delays the entire response past
    // client `fetchTimeout`, so the `Err` arm of `send().await` fires.
    // With `retries=5` the unbounded policy would attempt 6 times;
    // the timeout cap collapses that to 2 (1 initial + 1 retry). The
    // body-read path is covered separately by
    // `tarball_body_read_timeout_retries_at_most_once`.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/pkg.tgz"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(b"unused".to_vec())
                .set_delay(std::time::Duration::from_millis(500)),
        )
        .mount(&server)
        .await;

    let policy = FetchPolicy {
        timeout_ms: 50,
        retries: 5,
        retry_factor: 1,
        retry_min_timeout_ms: 1,
        retry_max_timeout_ms: 1,
        ..FetchPolicy::default()
    };
    let client = client_with(&server, policy);
    let url = format!("{}/pkg.tgz", server.uri());
    let err = client
        .fetch_tarball_bytes(&url)
        .await
        .expect_err("timeout should surface");
    match err {
        Error::Http(inner) => assert!(
            inner.is_timeout() || inner.is_request(),
            "expected timeout-shaped reqwest error, got {inner:?}",
        ),
        other => panic!("unexpected error: {other}"),
    }

    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests.len(),
        2,
        "timeouts must cap retries at 1 regardless of fetchRetries",
    );
}

#[tokio::test]
async fn timeout_cap_counts_only_timeouts_not_other_retries() {
    // Mixed-error reproducer: a non-timeout failure (503) consumes
    // a global retry slot, then a timeout still gets its allowed
    // retry. If the cap were keyed off the global `attempt`
    // counter, the second timeout would surface immediately and
    // the user would get *zero* timeout retries instead of one.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/pkg.tgz"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/pkg.tgz"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(b"unused".to_vec())
                .set_delay(std::time::Duration::from_millis(500)),
        )
        .mount(&server)
        .await;

    let policy = FetchPolicy {
        timeout_ms: 50,
        retries: 5,
        retry_factor: 1,
        retry_min_timeout_ms: 1,
        retry_max_timeout_ms: 1,
        ..FetchPolicy::default()
    };
    let client = client_with(&server, policy);
    let url = format!("{}/pkg.tgz", server.uri());
    let _ = client
        .fetch_tarball_bytes(&url)
        .await
        .expect_err("all attempts fail");

    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests.len(),
        3,
        "expected 1 503 + 1 initial timeout + 1 capped timeout retry; \
             timeout cap must not consume non-timeout retry slots",
    );
}

#[tokio::test]
async fn tarball_body_read_timeout_retries_at_most_once() {
    // Body-read timeout: a different code path from headers-stage
    // timeouts. Server sends the 200 status line + headers
    // immediately, then stalls the body. reqwest's `fetchTimeout`
    // fires inside `resp.chunk().await` during `read_body_capped`,
    // surfacing as `Error::Http(reqwest_timeout)` from the Ok-arm of
    // the retry loop — guarded by `timeout_retry_exhausted`. This
    // is the actual reproducer shape: `@cloudflare/workerd-*`
    // tarballs trickling under a degraded CDN edge.
    //
    // wiremock's `set_delay` delays the *whole* response (including
    // headers), so it can't reproduce this. We need a raw TCP
    // listener that splits header-write and body-stall.
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let count = std::sync::Arc::new(AtomicUsize::new(0));
    let count_handle = count.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            count_handle.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                // Drain the request — reqwest waits for headers before
                // returning from `send()`, so we must answer them.
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let _ = sock
                    .write_all(
                        b"HTTP/1.1 200 OK\r\n\
                              Content-Length: 1048576\r\n\
                              Content-Type: application/octet-stream\r\n\r\n",
                    )
                    .await;
                let _ = sock.flush().await;
                // Hold the connection without writing the body so the
                // client times out mid-`chunk()`. Bounded so the test
                // never wedges if the runtime forgets to drop us.
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            });
        }
    });

    let policy = FetchPolicy {
        timeout_ms: 100,
        retries: 5,
        retry_factor: 1,
        retry_min_timeout_ms: 1,
        retry_max_timeout_ms: 1,
        ..FetchPolicy::default()
    };
    let config = NpmConfig {
        registry: format!("http://{addr}/"),
        ..Default::default()
    };
    let client = RegistryClient::from_config_with_policy(config, policy);
    let url = format!("http://{addr}/pkg.tgz");
    let err = client
        .fetch_tarball_bytes(&url)
        .await
        .expect_err("body-read timeout should surface");
    assert!(
        matches!(&err, Error::Http(e) if e.is_timeout() || e.is_request()),
        "expected timeout-shaped error, got {err:?}",
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        2,
        "body-read timeouts must cap retries at 1 regardless of fetchRetries",
    );
}

#[tokio::test]
async fn warn_timeout_is_pure_observability_and_does_not_fail_request() {
    // Server returns a normal 200 after a 50ms delay. With
    // `warn_timeout_ms = 1`, the helper records the slow fetch
    // into the slow-metadata accumulator but the request must
    // still succeed — the setting is advisory, not a hard cutoff
    // (that's `timeout_ms`). This pins the invariant so a future
    // refactor doesn't turn the threshold into an error by
    // accident.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/demo"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(make_packument_json())
                .set_delay(std::time::Duration::from_millis(50)),
        )
        .mount(&server)
        .await;

    let policy = FetchPolicy {
        timeout_ms: 5_000,
        retries: 0,
        retry_factor: 1,
        retry_min_timeout_ms: 1,
        retry_max_timeout_ms: 1,
        warn_timeout_ms: 1,
        min_speed_kibps: 0,
        ..FetchPolicy::default()
    };
    let client = client_with(&server, policy);
    let packument = client
        .fetch_packument("demo")
        .await
        .expect("warn-threshold is advisory — request must still succeed");
    assert_eq!(packument.name, "demo");
}

#[tokio::test]
async fn retries_on_packument_body_decode_error_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/demo"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw("{not valid json", "application/json"),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/demo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(make_packument_json()))
        .mount(&server)
        .await;

    let policy = FetchPolicy {
        timeout_ms: 5_000,
        retries: 2,
        retry_factor: 1,
        retry_min_timeout_ms: 1,
        retry_max_timeout_ms: 1,
        ..FetchPolicy::default()
    };
    let client = client_with(&server, policy);
    let packument = client
        .fetch_packument("demo")
        .await
        .expect("decode error should be retried");
    assert_eq!(packument.name, "demo");

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2, "expected retry after decode error");
}

#[tokio::test]
async fn concurrent_corgi_fetches_for_same_name_coalesce_to_one_request() {
    // Two concurrent `fetch_packument_cached` calls for "demo"
    // must hit the registry exactly once: the second caller
    // waits on the per-name single-flight mutex and re-reads
    // the warmed disk cache on wake-up. The mock injects a
    // 100ms delay so both callers land inside the singleflight
    // window deterministically.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/demo"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_millis(100))
                .set_body_json(make_packument_json()),
        )
        .mount(&server)
        .await;

    let policy = FetchPolicy {
        timeout_ms: 5_000,
        ..FetchPolicy::default()
    };
    let client = std::sync::Arc::new(client_with(&server, policy));
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().to_path_buf();

    let c1 = std::sync::Arc::clone(&client);
    let c2 = std::sync::Arc::clone(&client);
    let d1 = dir.clone();
    let d2 = dir.clone();
    let h1 = tokio::spawn(async move { c1.fetch_packument_cached("demo", &d1).await });
    let h2 = tokio::spawn(async move { c2.fetch_packument_cached("demo", &d2).await });
    let (r1, r2) = tokio::join!(h1, h2);
    let p1 = r1.unwrap().expect("first fetch ok");
    let p2 = r2.unwrap().expect("second fetch ok");
    assert_eq!(p1.name, "demo");
    assert_eq!(p2.name, "demo");

    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests.len(),
        1,
        "expected single-flight to coalesce concurrent fetches into one network call"
    );
}

#[tokio::test]
async fn concurrent_full_fetches_for_same_name_coalesce_to_one_request() {
    // Mirror of the corgi test for the full-packument path:
    // `fetch_packument_full_cached` must also dedup concurrent
    // calls. The resolver hits this variant whenever
    // `trustPolicy=no-downgrade` or `minimumReleaseAge` requires
    // the `time` field — which is the default for npmjs.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/demo"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_millis(100))
                .set_body_json(serde_json::json!({
                    "name": "demo",
                    "versions": {},
                    "dist-tags": {},
                    "time": {},
                })),
        )
        .mount(&server)
        .await;

    let policy = FetchPolicy {
        timeout_ms: 5_000,
        ..FetchPolicy::default()
    };
    let client = std::sync::Arc::new(client_with(&server, policy));
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().to_path_buf();

    let c1 = std::sync::Arc::clone(&client);
    let c2 = std::sync::Arc::clone(&client);
    let d1 = dir.clone();
    let d2 = dir.clone();
    let h1 = tokio::spawn(async move { c1.fetch_packument_full_cached("demo", &d1).await });
    let h2 = tokio::spawn(async move { c2.fetch_packument_full_cached("demo", &d2).await });
    let (r1, r2) = tokio::join!(h1, h2);
    let v1 = r1.unwrap().expect("first fetch ok");
    let v2 = r2.unwrap().expect("second fetch ok");
    assert_eq!(v1["name"], "demo");
    assert_eq!(v2["name"], "demo");

    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests.len(),
        1,
        "expected single-flight to coalesce concurrent full-packument fetches"
    );
}

#[tokio::test]
async fn concurrent_typed_revalidations_for_same_name_coalesce_to_one_request() {
    // Companion to the corgi/full tests for the typed
    // revalidation path. Seeds a stale full-cache entry so
    // `cached_packument_lookup` hands back `Some(Full(...))`,
    // which routes both concurrent callers through
    // `revalidate_full_packument_typed`. The winner does the
    // conditional GET and writes a fresh cache; the loser
    // re-reads the now-warm cache via the typed deserializer
    // and skips the network entirely.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/demo"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_millis(100))
                .set_body_json(serde_json::json!({
                    "name": "demo",
                    "versions": {},
                    "dist-tags": {},
                    "time": {},
                })),
        )
        .mount(&server)
        .await;

    let policy = FetchPolicy {
        timeout_ms: 5_000,
        ..FetchPolicy::default()
    };
    let client = std::sync::Arc::new(client_with(&server, policy));
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().to_path_buf();

    let seed = Packument {
        name: "demo".to_owned(),
        modified: None,
        versions: BTreeMap::new(),
        dist_tags: BTreeMap::new(),
        time: BTreeMap::new(),
    };
    client.seed_full_packument_cache("demo", &dir, &seed, None, None, false);

    let lookup1 = client.cached_packument_lookup("demo", &dir);
    let lookup2 = client.cached_packument_lookup("demo", &dir);
    assert!(lookup1.stale, "seed should be reported as stale");
    assert!(lookup2.stale, "seed should be reported as stale");

    let c1 = std::sync::Arc::clone(&client);
    let c2 = std::sync::Arc::clone(&client);
    let d1 = dir.clone();
    let d2 = dir.clone();
    let h1 = tokio::spawn(async move {
        c1.fetch_packument_with_time_cached_after_lookup("demo", &d1, lookup1)
            .await
    });
    let h2 = tokio::spawn(async move {
        c2.fetch_packument_with_time_cached_after_lookup("demo", &d2, lookup2)
            .await
    });
    let (r1, r2) = tokio::join!(h1, h2);
    let p1 = r1.unwrap().expect("first revalidate ok");
    let p2 = r2.unwrap().expect("second revalidate ok");
    assert_eq!(p1.name, "demo");
    assert_eq!(p2.name, "demo");

    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests.len(),
        1,
        "expected single-flight to coalesce concurrent typed revalidations into one network call"
    );
}

#[tokio::test]
async fn full_packument_cached_retries_on_body_decode_error_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/demo"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw("{not valid json", "application/json"),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/demo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "name": "demo",
            "versions": {},
            "dist-tags": {},
            "time": {},
        })))
        .mount(&server)
        .await;

    let policy = FetchPolicy {
        timeout_ms: 5_000,
        retries: 2,
        retry_factor: 1,
        retry_min_timeout_ms: 1,
        retry_max_timeout_ms: 1,
        ..FetchPolicy::default()
    };
    let client = client_with(&server, policy);
    let temp = tempfile::tempdir().unwrap();
    let packument = client
        .fetch_packument_full_cached("demo", temp.path())
        .await
        .expect("decode error should be retried on full packument path");
    assert_eq!(packument["name"], "demo");

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2, "expected retry after decode error");
}

#[tokio::test]
async fn body_decode_retry_does_not_multiply_total_attempt_count() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/demo"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw("{not valid json", "application/json"),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/demo"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let policy = FetchPolicy {
        timeout_ms: 5_000,
        retries: 1,
        retry_factor: 1,
        retry_min_timeout_ms: 1,
        retry_max_timeout_ms: 1,
        ..FetchPolicy::default()
    };
    let client = client_with(&server, policy);
    let err = client
        .fetch_packument("demo")
        .await
        .expect_err("retry budget should be exhausted after two total attempts");
    match err {
        Error::Http(inner) => assert_eq!(inner.status().map(|s| s.as_u16()), Some(503)),
        other => panic!("unexpected error: {other}"),
    }

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2, "expected total attempts to stay capped");
}

#[tokio::test]
async fn scoped_packument_request_is_url_encoded() {
    // Artifactory's npm remote rejects the literal `@scope/pkg`
    // path form with 406 and only accepts `@scope%2Fpkg`. The
    // corgi Accept header must include `application/json` and
    // `*/*` fallbacks for the same reason. wiremock normalizes
    // `%2F` to `/` in its path matcher, so match on any GET and
    // assert the raw request line instead.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "name": "@scope/pkg",
            "versions": {},
            "dist-tags": {},
        })))
        .mount(&server)
        .await;

    let client = client_with(&server, FetchPolicy::default());
    let packument = client
        .fetch_packument("@scope/pkg")
        .await
        .expect("scoped packument fetch must succeed");
    assert_eq!(packument.name, "@scope/pkg");

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let raw = requests[0].url.as_str();
    assert!(
        raw.contains("/@scope%2Fpkg"),
        "expected %2F-encoded scope separator, got {raw}"
    );
    let accept = requests[0]
        .headers
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert_eq!(
        accept, "application/vnd.npm.install-v1+json; q=1.0, application/json; q=0.8, */*",
        "corgi Accept header must include JSON and */* fallbacks",
    );
}
