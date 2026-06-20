//! Integration / end-to-end tests for kao-proxy.
//!
//! Each test spins up a wiremock `MockServer` that plays the role of dRPC and
//! exercises the full axum router through `tower::ServiceExt::oneshot` — no TCP
//! port needed, but every layer (routing, validation, method filtering,
//! upstream HTTP) is exercised.

use axum::body::Body;
use axum::extract::DefaultBodyLimit;
use axum::http::{Request, StatusCode};
use axum::routing::{get, post};
use axum::Router;
use http_body_util::BodyExt;
use kao_proxy::config::{default_chains, Key};
use kao_proxy::routes::{self, AppState};
use kao_proxy::upstream::Drpc;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const TEST_KEY: &str = "test-api-key-1234";
const VALID_ADDR: &str = "0xd8da6bf26964af9d7eed9e03e53415d37aa96045";

/// Build the full application router backed by a wiremock dRPC server.
async fn app(method_denylist: bool) -> (Router, MockServer) {
    let mock = MockServer::start().await;
    // No `https_only` — wiremock serves plain HTTP.
    let client = reqwest::Client::builder()
        .user_agent("kao-proxy-test/0.1")
        .build()
        .unwrap();
    let drpc = Drpc::new(client, mock.uri(), Key::new(TEST_KEY));
    let state = Arc::new(AppState {
        drpc,
        chains: default_chains(),
        method_denylist,
    });
    let router = Router::new()
        .route("/healthz", get(routes::health))
        .route("/rpc/{chain}", post(routes::rpc_handler))
        .route("/v1/{chain}/balances/{address}", get(routes::balances_handler))
        .route("/v1/{chain}/history/{address}", get(routes::history_handler))
        .layer(DefaultBodyLimit::max(1 << 20))
        .with_state(state);
    (router, mock)
}

fn rpc_path(chain: &str) -> String {
    format!("/{chain}/{TEST_KEY}")
}

/// Upstream dRPC path for the balances Wallet-API call.
fn balances_path(chain: &str, addr: &str) -> String {
    format!("/{chain}/{TEST_KEY}/lambda/v2/wallets/{addr}/balances")
}

/// Upstream dRPC path for the transaction-history Wallet-API call. History
/// lives under `lambda/v1/transactions/{addr}/history`, NOT `v2/wallets`.
fn history_path(chain: &str, addr: &str) -> String {
    format!("/{chain}/{TEST_KEY}/lambda/v1/transactions/{addr}/history")
}

async fn collect_body(resp: axum::http::Response<Body>) -> Vec<u8> {
    resp.into_body().collect().await.unwrap().to_bytes().to_vec()
}

async fn collect_json(resp: axum::http::Response<Body>) -> Value {
    serde_json::from_slice(&collect_body(resp).await).unwrap()
}

// ===========================================================================
// Health
// ===========================================================================

#[tokio::test]
async fn health_returns_ok() {
    let (app, _mock) = app(true).await;
    let resp = app
        .oneshot(Request::builder().uri("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(&collect_body(resp).await, b"ok");
}

// ===========================================================================
// RPC passthrough — routing & forwarding
// ===========================================================================

#[tokio::test]
async fn rpc_passthrough_success() {
    let (app, mock) = app(true).await;
    let upstream = json!({"jsonrpc":"2.0","id":1,"result":"0x134e82a"});

    Mock::given(method("POST"))
        .and(path(rpc_path("ethereum")))
        .respond_with(ResponseTemplate::new(200).set_body_json(&upstream))
        .expect(1)
        .mount(&mock)
        .await;

    let body = json!({"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]});
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rpc/ethereum")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = collect_json(resp).await;
    assert_eq!(json["result"], "0x134e82a");
}

#[tokio::test]
async fn rpc_chain_alias_mainnet_resolves_to_ethereum() {
    let (app, mock) = app(true).await;
    let upstream = json!({"jsonrpc":"2.0","id":1,"result":"0x1"});

    Mock::given(method("POST"))
        .and(path(rpc_path("ethereum")))
        .respond_with(ResponseTemplate::new(200).set_body_json(&upstream))
        .expect(1)
        .mount(&mock)
        .await;

    let body = json!({"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]});
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rpc/mainnet")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn rpc_chain_alias_numeric_resolves() {
    let (app, mock) = app(true).await;
    let upstream = json!({"jsonrpc":"2.0","id":1,"result":"0x1"});

    // chain_id "1" → ethereum
    Mock::given(method("POST"))
        .and(path(rpc_path("ethereum")))
        .respond_with(ResponseTemplate::new(200).set_body_json(&upstream))
        .expect(1)
        .mount(&mock)
        .await;

    let body = json!({"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]});
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rpc/1")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn rpc_case_insensitive_chain() {
    let (app, mock) = app(true).await;
    let upstream = json!({"jsonrpc":"2.0","id":1,"result":"0xa"});

    Mock::given(method("POST"))
        .and(path(rpc_path("optimism")))
        .respond_with(ResponseTemplate::new(200).set_body_json(&upstream))
        .expect(1)
        .mount(&mock)
        .await;

    let body = json!({"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]});
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rpc/OPTIMISM")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn rpc_unknown_chain_returns_404() {
    let (app, _mock) = app(true).await;

    let body = json!({"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]});
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rpc/solana")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let json = collect_json(resp).await;
    assert_eq!(json["error"]["message"], "unknown chain");
}

#[tokio::test]
async fn rpc_upstream_status_is_forwarded() {
    let (app, mock) = app(true).await;

    // Upstream returns 429 Too Many Requests.
    Mock::given(method("POST"))
        .and(path(rpc_path("ethereum")))
        .respond_with(
            ResponseTemplate::new(429)
                .set_body_json(json!({"jsonrpc":"2.0","id":1,"error":{"code":-32005,"message":"rate limited"}})),
        )
        .expect(1)
        .mount(&mock)
        .await;

    let body = json!({"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]});
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rpc/ethereum")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    // The proxy relays the status verbatim for RPC passthrough.
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
}

// ===========================================================================
// RPC — method denylist
// ===========================================================================

#[tokio::test]
async fn rpc_denies_debug_method() {
    let (app, _mock) = app(true).await;

    let body = json!({"jsonrpc":"2.0","id":1,"method":"debug_traceTransaction","params":["0xabc"]});
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rpc/ethereum")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let json = collect_json(resp).await;
    assert_eq!(json["error"]["message"], "rpc method not allowed");
}

#[tokio::test]
async fn rpc_denies_trace_method() {
    let (app, _mock) = app(true).await;

    let body = json!({"jsonrpc":"2.0","id":1,"method":"trace_block","params":["latest"]});
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rpc/ethereum")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn rpc_denies_txpool_method() {
    let (app, _mock) = app(true).await;

    let body = json!({"jsonrpc":"2.0","id":1,"method":"txpool_status","params":[]});
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rpc/ethereum")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn rpc_denies_eth_subscribe() {
    let (app, _mock) = app(true).await;

    let body = json!({"jsonrpc":"2.0","id":1,"method":"eth_subscribe","params":["newHeads"]});
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rpc/ethereum")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn rpc_denies_arbtrace_method() {
    let (app, _mock) = app(true).await;

    let body = json!({"jsonrpc":"2.0","id":1,"method":"arbtrace_replayTransaction","params":[]});
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rpc/ethereum")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn rpc_denies_method_in_batch() {
    let (app, _mock) = app(true).await;

    // Batch with one allowed and one denied method — entire batch rejected.
    let body = json!([
        {"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]},
        {"jsonrpc":"2.0","id":2,"method":"debug_traceTransaction","params":["0xabc"]}
    ]);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rpc/ethereum")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn rpc_allows_standard_method() {
    let (app, mock) = app(true).await;
    let upstream = json!({"jsonrpc":"2.0","id":1,"result":"0x5208"});

    Mock::given(method("POST"))
        .and(path(rpc_path("ethereum")))
        .respond_with(ResponseTemplate::new(200).set_body_json(&upstream))
        .expect(1)
        .mount(&mock)
        .await;

    let body = json!({"jsonrpc":"2.0","id":1,"method":"eth_estimateGas","params":[{}]});
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rpc/ethereum")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn rpc_filter_disabled_allows_debug() {
    let (app, mock) = app(false).await;
    let upstream = json!({"jsonrpc":"2.0","id":1,"result":{}});

    Mock::given(method("POST"))
        .and(path(rpc_path("ethereum")))
        .respond_with(ResponseTemplate::new(200).set_body_json(&upstream))
        .expect(1)
        .mount(&mock)
        .await;

    let body = json!({"jsonrpc":"2.0","id":1,"method":"debug_traceTransaction","params":["0xabc"]});
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rpc/ethereum")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    // With filter disabled, the denied method is forwarded.
    assert_eq!(resp.status(), StatusCode::OK);
}

// ===========================================================================
// RPC — input validation
// ===========================================================================

#[tokio::test]
async fn rpc_invalid_json_body() {
    let (app, _mock) = app(true).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rpc/ethereum")
                .header("content-type", "application/json")
                .body(Body::from("not json at all"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = collect_json(resp).await;
    assert_eq!(json["error"]["message"], "invalid JSON-RPC body");
}

#[tokio::test]
async fn rpc_missing_method_field() {
    let (app, _mock) = app(true).await;

    let body = json!({"jsonrpc":"2.0","id":1,"params":[]});
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rpc/ethereum")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = collect_json(resp).await;
    assert_eq!(json["error"]["message"], "missing method");
}

#[tokio::test]
async fn rpc_non_object_non_array_body() {
    let (app, _mock) = app(true).await;

    // A bare JSON string is neither an object nor an array.
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rpc/ethereum")
                .header("content-type", "application/json")
                .body(Body::from("\"hello\""))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ===========================================================================
// Balances — per-chain passthrough
// ===========================================================================

#[tokio::test]
async fn balances_passthrough_returns_upstream_body() {
    let (app, mock) = app(true).await;
    let body = json!({ "data": { "assets": [{ "type": "token" }] } });

    Mock::given(method("GET"))
        .and(path(balances_path("optimism", VALID_ADDR)))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .expect(1)
        .mount(&mock)
        .await;

    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/v1/optimism/balances/{VALID_ADDR}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp.headers().get("content-type").unwrap().to_str().unwrap();
    assert_eq!(ct, "application/json");
    // Body returned verbatim — no envelope wrapping.
    assert_eq!(collect_json(resp).await, body);
}

#[tokio::test]
async fn balances_resolves_chain_alias() {
    let (app, mock) = app(true).await;
    let body = json!({ "data": { "assets": [] } });

    // `mainnet` (alias) must resolve to the `ethereum` dRPC slug.
    Mock::given(method("GET"))
        .and(path(balances_path("ethereum", VALID_ADDR)))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .expect(1)
        .mount(&mock)
        .await;

    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/v1/mainnet/balances/{VALID_ADDR}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn balances_forwards_upstream_error_status_and_body() {
    let (app, mock) = app(true).await;

    // dRPC's free-tier rejection: status + diagnostic body must reach the
    // wallet intact (the wallet surfaces this body in its error string).
    Mock::given(method("GET"))
        .and(path(balances_path("ethereum", VALID_ADDR)))
        .respond_with(
            ResponseTemplate::new(403)
                .set_body_json(json!({"message":"method is not available on freetier","code":35})),
        )
        .expect(1)
        .mount(&mock)
        .await;

    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/v1/ethereum/balances/{VALID_ADDR}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(collect_json(resp).await["code"], 35);
}

#[tokio::test]
async fn balances_forwards_query_string() {
    let (app, mock) = app(true).await;

    Mock::given(method("GET"))
        .and(path(balances_path("base", VALID_ADDR)))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": { "assets": [] } })))
        .expect(1)
        .mount(&mock)
        .await;

    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/v1/base/balances/{VALID_ADDR}?asset_type=TOKEN&include_zero_price_tokens=false"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let received = mock.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    assert_eq!(
        received[0].url.query(),
        Some("asset_type=TOKEN&include_zero_price_tokens=false"),
    );
}

#[tokio::test]
async fn balances_invalid_address_too_short() {
    let (app, _mock) = app(true).await;

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/ethereum/balances/0xdead")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = collect_json(resp).await;
    assert_eq!(json["error"]["message"], "invalid ethereum address");
}

#[tokio::test]
async fn balances_invalid_address_non_hex() {
    let (app, _mock) = app(true).await;

    // 0x + 40 chars, but contains 'zz'.
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/ethereum/balances/0xzz00000000000000000000000000000000000000")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn wallet_unknown_chain_returns_404() {
    let (app, _mock) = app(true).await;

    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/v1/solana/balances/{VALID_ADDR}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let json = collect_json(resp).await;
    assert_eq!(json["error"]["message"], "unknown chain");
}

// ===========================================================================
// History — per-chain passthrough
// ===========================================================================

#[tokio::test]
async fn history_passthrough_uses_v1_transactions_path() {
    let (app, mock) = app(true).await;
    let body = json!({ "data": [{ "hash": "0xabc", "type": "receive" }] });

    // The mock only matches the corrected `lambda/v1/transactions/.../history`
    // path; the request 404s here if the proxy regresses to `v2/wallets`.
    Mock::given(method("GET"))
        .and(path(history_path("ethereum", VALID_ADDR)))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .expect(1)
        .mount(&mock)
        .await;

    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/v1/ethereum/history/{VALID_ADDR}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(collect_json(resp).await, body);
}

#[tokio::test]
async fn history_forwards_limit_query() {
    let (app, mock) = app(true).await;

    Mock::given(method("GET"))
        .and(path(history_path("base", VALID_ADDR)))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": [] })))
        .expect(1)
        .mount(&mock)
        .await;

    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/v1/base/history/{VALID_ADDR}?limit=25"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let received = mock.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].url.query(), Some("limit=25"));
}

#[tokio::test]
async fn history_invalid_address() {
    let (app, _mock) = app(true).await;

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/ethereum/history/not-an-address")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = collect_json(resp).await;
    assert_eq!(json["error"]["message"], "invalid ethereum address");
}

// ===========================================================================
// Privacy — proxy must not leak client headers to upstream
// ===========================================================================

#[tokio::test]
async fn rpc_does_not_leak_client_headers() {
    let (app, mock) = app(true).await;
    let upstream = json!({"jsonrpc":"2.0","id":1,"result":"0x1"});

    Mock::given(method("POST"))
        .and(path(rpc_path("ethereum")))
        .respond_with(ResponseTemplate::new(200).set_body_json(&upstream))
        .expect(1)
        .mount(&mock)
        .await;

    let body = json!({"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]});
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rpc/ethereum")
                .header("content-type", "application/json")
                // Headers a real browser/wallet might send:
                .header("x-forwarded-for", "1.2.3.4")
                .header("cookie", "session=secret")
                .header("origin", "https://evil.com")
                .header("referer", "https://evil.com/page")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The upstream should have received exactly 1 request. Verify that none of
    // the client-identifying headers leaked through.
    let received = mock.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let upstream_req = &received[0];
    assert!(upstream_req.headers.get("x-forwarded-for").is_none());
    assert!(upstream_req.headers.get("cookie").is_none());
    assert!(upstream_req.headers.get("origin").is_none());
    assert!(upstream_req.headers.get("referer").is_none());
    // The proxy sets its own User-Agent.
    let ua = upstream_req.headers.get("user-agent").unwrap().to_str().unwrap();
    assert!(ua.starts_with("kao-proxy"), "unexpected user-agent: {ua}");
}

// ===========================================================================
// Error response format
// ===========================================================================

#[tokio::test]
async fn error_responses_use_json_envelope() {
    let (app, _mock) = app(true).await;

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/ethereum/balances/bad")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let json = collect_json(resp).await;
    // All error responses should have { "error": { "message": "..." } }.
    assert!(json.get("error").is_some(), "missing error key");
    assert!(json["error"].get("message").is_some(), "missing error.message");
    assert!(json["error"]["message"].is_string(), "error.message not a string");
}

// ===========================================================================
// RPC — batch passthrough
// ===========================================================================

#[tokio::test]
async fn rpc_batch_passthrough_success() {
    let (app, mock) = app(true).await;
    let upstream = json!([
        {"jsonrpc":"2.0","id":1,"result":"0x134e82a"},
        {"jsonrpc":"2.0","id":2,"result":"0x1"}
    ]);

    Mock::given(method("POST"))
        .and(path(rpc_path("ethereum")))
        .respond_with(ResponseTemplate::new(200).set_body_json(&upstream))
        .expect(1)
        .mount(&mock)
        .await;

    let body = json!([
        {"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]},
        {"jsonrpc":"2.0","id":2,"method":"eth_chainId","params":[]}
    ]);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rpc/ethereum")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = collect_json(resp).await;
    assert!(json.is_array(), "batch response should be an array");
    assert_eq!(json.as_array().unwrap().len(), 2);
}

// ===========================================================================
// Content-Type headers
// ===========================================================================

#[tokio::test]
async fn rpc_response_has_json_content_type() {
    let (app, mock) = app(true).await;
    let upstream = json!({"jsonrpc":"2.0","id":1,"result":"0x1"});

    Mock::given(method("POST"))
        .and(path(rpc_path("ethereum")))
        .respond_with(ResponseTemplate::new(200).set_body_json(&upstream))
        .expect(1)
        .mount(&mock)
        .await;

    let body = json!({"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]});
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rpc/ethereum")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp.headers().get("content-type").unwrap().to_str().unwrap();
    assert_eq!(ct, "application/json");
}
