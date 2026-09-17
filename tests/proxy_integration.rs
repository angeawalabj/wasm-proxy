//! Tests de bout en bout : proxy réel (port éphémère) + backend(s) réels
//! (aussi sur des ports éphémères), requêtes envoyées par un vrai client
//! HTTP. Couvre les scénarios décrits dans le README ("Comportements
//! couverts").

mod common;

use common::*;
use http_body_util::Full;
use hyper::{Response, StatusCode};
use std::time::{Duration, Instant};
use wasm_proxy::config::Route;

#[tokio::test]
async fn admin_route_without_key_is_blocked() {
    let backend = spawn_backend(|_req| async {
        Response::new(Full::new(hyper::body::Bytes::from_static(b"should not be reached")))
    })
    .await;

    let config = config_with_routes(vec![Route {
        path_prefix: "/".to_string(),
        backend: format!("http://{backend}"),
    }]);
    let proxy = spawn_proxy(config, &[&wasm_fixture("admin_guard.wasm")]).await;

    let resp = get(proxy, "/admin").await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN);
    assert_eq!(resp.body, "missing or invalid api key");
}

#[tokio::test]
async fn admin_route_with_wrong_key_is_blocked() {
    let backend = spawn_backend(|_req| async {
        Response::new(Full::new(hyper::body::Bytes::from_static(b"should not be reached")))
    })
    .await;

    let config = config_with_routes(vec![Route {
        path_prefix: "/".to_string(),
        backend: format!("http://{backend}"),
    }]);
    let proxy = spawn_proxy(config, &[&wasm_fixture("admin_guard.wasm")]).await;

    let resp = request(proxy, "GET", "/admin", &[("x-api-key", "wrong")], hyper::body::Bytes::new()).await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn admin_route_with_correct_key_passes_through() {
    let backend = spawn_backend(|_req| async {
        Response::new(Full::new(hyper::body::Bytes::from_static(b"admin panel")))
    })
    .await;

    let config = config_with_routes(vec![Route {
        path_prefix: "/".to_string(),
        backend: format!("http://{backend}"),
    }]);
    let proxy = spawn_proxy(config, &[&wasm_fixture("admin_guard.wasm")]).await;

    let resp = request(
        proxy,
        "GET",
        "/admin",
        &[("x-api-key", "secret123")],
        hyper::body::Bytes::new(),
    )
    .await;

    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(resp.body, "admin panel");
    assert_eq!(resp.headers.get("x-plugin-checked").unwrap(), "admin_guard");
}

#[tokio::test]
async fn non_admin_route_is_unaffected_by_admin_guard() {
    let backend = spawn_backend(|_req| async {
        Response::new(Full::new(hyper::body::Bytes::from_static(b"users list")))
    })
    .await;

    let config = config_with_routes(vec![Route {
        path_prefix: "/".to_string(),
        backend: format!("http://{backend}"),
    }]);
    let proxy = spawn_proxy(config, &[&wasm_fixture("admin_guard.wasm")]).await;

    let resp = get(proxy, "/api/users").await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(resp.body, "users list");
}

#[tokio::test]
async fn backend_5xx_is_masked_by_error_masker() {
    let backend = spawn_backend(|_req| async {
        Response::builder()
            .status(503)
            .body(Full::new(hyper::body::Bytes::from_static(
                b"panic: internal stack trace at db.rs:42",
            )))
            .unwrap()
    })
    .await;

    let config = config_with_routes(vec![Route {
        path_prefix: "/".to_string(),
        backend: format!("http://{backend}"),
    }]);
    let proxy = spawn_proxy(config, &[&wasm_fixture("error_masker.wasm")]).await;

    let resp = get(proxy, "/anything").await;
    assert_eq!(resp.status, StatusCode::BAD_GATEWAY);
    assert_eq!(resp.body, "upstream error");
}

#[tokio::test]
async fn unmatched_route_returns_404() {
    let backend = spawn_backend(|_req| async { Response::new(Full::new(hyper::body::Bytes::new())) }).await;

    let config = config_with_routes(vec![Route {
        path_prefix: "/api".to_string(),
        backend: format!("http://{backend}"),
    }]);
    let proxy = spawn_proxy(config, &[]).await;

    let resp = get(proxy, "/other").await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unreachable_backend_returns_502() {
    let dead = unreachable_addr().await;

    let config = config_with_routes(vec![Route {
        path_prefix: "/".to_string(),
        backend: format!("http://{dead}"),
    }]);
    let proxy = spawn_proxy(config, &[]).await;

    let resp = get(proxy, "/anything").await;
    assert_eq!(resp.status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn slow_backend_times_out_with_504() {
    let backend = spawn_backend(|_req| async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        Response::new(Full::new(hyper::body::Bytes::from_static(b"too late")))
    })
    .await;

    let mut config = config_with_routes(vec![Route {
        path_prefix: "/".to_string(),
        backend: format!("http://{backend}"),
    }]);
    config.request_timeout_ms = 100;
    let proxy = spawn_proxy(config, &[]).await;

    let start = Instant::now();
    let resp = get(proxy, "/anything").await;
    let elapsed = start.elapsed();

    assert_eq!(resp.status, StatusCode::GATEWAY_TIMEOUT);
    assert!(elapsed < Duration::from_millis(400), "délai observé: {elapsed:?}");
}

#[tokio::test]
async fn oversized_request_body_is_rejected() {
    let backend = spawn_backend(|_req| async { Response::new(Full::new(hyper::body::Bytes::new())) }).await;

    let mut config = config_with_routes(vec![Route {
        path_prefix: "/".to_string(),
        backend: format!("http://{backend}"),
    }]);
    config.max_body_bytes = 10;
    let proxy = spawn_proxy(config, &[]).await;

    let big_body = hyper::body::Bytes::from(vec![b'x'; 1024]);
    let resp = request(proxy, "POST", "/upload", &[], big_body).await;

    assert_eq!(resp.status, StatusCode::PAYLOAD_TOO_LARGE);
}
