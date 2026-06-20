//! HTTP surface.
//!
//! Routes:
//!   GET  /healthz                        liveness, no upstream
//!   POST /rpc/{chain}                     JSON-RPC passthrough (alias -> dRPC slug)
//!   GET  /v1/{chain}/balances/{address}   Wallet-API balances for one chain
//!   GET  /v1/{chain}/history/{address}    Wallet-API tx history for one chain
//!
//! The wallet-API routes are a thin per-chain mirror of dRPC's Wallet API: the
//! proxy resolves the chain alias to a dRPC slug, injects the shared key,
//! forwards the caller's query string verbatim, and returns dRPC's status and
//! body unchanged. It does NOT merge chains or reshape the payload — the wallet
//! calls one chain at a time (its `Indexer` is per-chain) and re-verifies
//! balances on-chain via Helios anyway, treating this as an untrusted discovery
//! source.

use crate::config::ChainSpec;
use crate::upstream::{Drpc, UpstreamError};
use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::Value;
use std::sync::Arc;

pub struct AppState {
    pub drpc: Drpc,
    pub chains: Vec<ChainSpec>,
    pub method_denylist: bool,
}

impl AppState {
    /// Map a caller-supplied chain name/alias to a canonical dRPC slug.
    fn resolve_chain(&self, input: &str) -> Option<&'static str> {
        let s = input.to_ascii_lowercase();
        self.chains
            .iter()
            .find(|c| c.slug == s.as_str() || c.aliases.contains(&s.as_str()))
            .map(|c| c.slug)
    }
}

pub async fn health() -> &'static str {
    "ok"
}

pub async fn rpc_handler(
    State(state): State<Arc<AppState>>,
    Path(chain): Path<String>,
    body: Bytes,
) -> Result<Response, AppError> {
    let slug = state.resolve_chain(&chain).ok_or(AppError::UnknownChain)?;
    if state.method_denylist {
        reject_denied_methods(&body)?;
    }
    let (code, out) = state.drpc.rpc(slug, &body).await?;
    Ok(json_passthrough(code, out))
}

pub async fn balances_handler(
    State(state): State<Arc<AppState>>,
    Path((chain, address)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Result<Response, AppError> {
    let slug = state.resolve_chain(&chain).ok_or(AppError::UnknownChain)?;
    if !is_valid_eth_address(&address) {
        return Err(AppError::BadAddress);
    }
    let (code, body) = state.drpc.balances(slug, &address, query.as_deref()).await?;
    Ok(json_passthrough(code, body))
}

pub async fn history_handler(
    State(state): State<Arc<AppState>>,
    Path((chain, address)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Result<Response, AppError> {
    let slug = state.resolve_chain(&chain).ok_or(AppError::UnknownChain)?;
    if !is_valid_eth_address(&address) {
        return Err(AppError::BadAddress);
    }
    let (code, body) = state
        .drpc
        .transactions(slug, &address, query.as_deref())
        .await?;
    Ok(json_passthrough(code, body))
}

/// Build a passthrough response: upstream status (clamped to a valid code),
/// `application/json`, and the upstream body verbatim.
fn json_passthrough(code: u16, body: Vec<u8>) -> Response {
    let status = StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY);
    (
        status,
        [(header::CONTENT_TYPE, HeaderValue::from_static("application/json"))],
        body,
    )
        .into_response()
}

// ---- validation & method filtering ------------------------------------------

fn is_valid_eth_address(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 42 && &b[0..2] == b"0x" && b[2..].iter().all(u8::is_ascii_hexdigit)
}

/// Deny methods that are expensive or meaningless over HTTP, to limit how badly
/// a leaked endpoint could abuse the shared key. Handles batch arrays too.
fn reject_denied_methods(body: &[u8]) -> Result<(), AppError> {
    let v: Value =
        serde_json::from_slice(body).map_err(|_| AppError::BadRequest("invalid JSON-RPC body"))?;
    match &v {
        Value::Object(_) => check_one(&v),
        Value::Array(items) => items.iter().try_for_each(check_one),
        _ => Err(AppError::BadRequest("invalid JSON-RPC body")),
    }
}

fn check_one(v: &Value) -> Result<(), AppError> {
    let m = v
        .get("method")
        .and_then(Value::as_str)
        .ok_or(AppError::BadRequest("missing method"))?;
    let denied = m.starts_with("debug_")
        || m.starts_with("trace_")
        || m.starts_with("arbtrace_")
        || m.starts_with("txpool_")
        || m == "eth_subscribe"
        || m == "eth_unsubscribe";
    if denied {
        Err(AppError::MethodNotAllowed)
    } else {
        Ok(())
    }
}

// ---- errors ------------------------------------------------------------------

pub enum AppError {
    BadAddress,
    UnknownChain,
    MethodNotAllowed,
    BadRequest(&'static str),
    Upstream(&'static str),
}

impl From<UpstreamError> for AppError {
    fn from(e: UpstreamError) -> Self {
        match e {
            UpstreamError::Transport => AppError::Upstream("upstream transport error"),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            AppError::BadAddress => (StatusCode::BAD_REQUEST, "invalid ethereum address"),
            AppError::UnknownChain => (StatusCode::NOT_FOUND, "unknown chain"),
            AppError::MethodNotAllowed => (StatusCode::FORBIDDEN, "rpc method not allowed"),
            AppError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            AppError::Upstream(m) => (StatusCode::BAD_GATEWAY, m),
        };
        (status, Json(serde_json::json!({ "error": { "message": msg } }))).into_response()
    }
}
