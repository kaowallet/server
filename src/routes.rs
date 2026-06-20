//! HTTP surface.
//!
//! Routes:
//!   GET  /healthz                  liveness, no upstream
//!   POST /rpc/{chain}              JSON-RPC passthrough (chain alias -> dRPC slug)
//!   GET  /v1/portfolio/{address}   balances fanned out across all chains
//!   GET  /v1/history/{address}     transactions fanned out across all chains
//!
//! Portfolio/history return a per-chain *envelope* rather than a merged total.
//! That keeps this proxy schema-agnostic (it forwards dRPC's JSON verbatim per
//! chain), degrades gracefully when one chain errors, and leaves merging to the
//! wallet — which re-verifies balances on-chain via Helios anyway, treating this
//! endpoint as an untrusted discovery source.

use crate::config::ChainSpec;
use crate::upstream::{Drpc, UpstreamError};
use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::task::JoinSet;

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
    let status = StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY);
    Ok((
        status,
        [(header::CONTENT_TYPE, HeaderValue::from_static("application/json"))],
        out,
    )
        .into_response())
}

pub async fn portfolio_handler(
    State(state): State<Arc<AppState>>,
    Path(address): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Json<Envelope>, AppError> {
    if !is_valid_eth_address(&address) {
        return Err(AppError::BadAddress);
    }
    Ok(Json(fan_out(state, Kind::Balances, address, query).await))
}

pub async fn history_handler(
    State(state): State<Arc<AppState>>,
    Path(address): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Json<Envelope>, AppError> {
    if !is_valid_eth_address(&address) {
        return Err(AppError::BadAddress);
    }
    Ok(Json(fan_out(state, Kind::Transactions, address, query).await))
}

// ---- fan-out -----------------------------------------------------------------

#[derive(Clone, Copy)]
enum Kind {
    Balances,
    Transactions,
}

#[derive(Serialize)]
pub struct Envelope {
    address: String,
    results: BTreeMap<String, ChainResult>,
}

#[derive(Serialize)]
pub struct ChainResult {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl ChainResult {
    fn ok(data: Value) -> Self {
        Self { ok: true, data: Some(data), error: None }
    }
    fn err(msg: String) -> Self {
        Self { ok: false, data: None, error: Some(msg) }
    }
}

async fn fan_out(state: Arc<AppState>, kind: Kind, address: String, query: Option<String>) -> Envelope {
    let mut set: JoinSet<(String, Result<Value, UpstreamError>)> = JoinSet::new();

    for chain in &state.chains {
        let st = state.clone();
        let slug = chain.slug.to_string();
        let addr = address.clone();
        let q = query.clone();
        set.spawn(async move {
            let res = match kind {
                Kind::Balances => st.drpc.balances(&slug, &addr, q.as_deref()).await,
                Kind::Transactions => st.drpc.transactions(&slug, &addr, q.as_deref()).await,
            };
            (slug, res)
        });
    }

    let mut results = BTreeMap::new();
    while let Some(joined) = set.join_next().await {
        if let Ok((slug, res)) = joined {
            let entry = match res {
                Ok(data) => ChainResult::ok(data),
                Err(e) => ChainResult::err(e.to_string()),
            };
            results.insert(slug, entry);
        }
        // A JoinError here means a task panicked; we simply omit that chain.
    }

    Envelope { address, results }
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
        AppError::Upstream(match e {
            UpstreamError::Transport => "upstream transport error",
            UpstreamError::Status(_) => "upstream returned error status",
            UpstreamError::Decode => "upstream returned invalid JSON",
        })
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
