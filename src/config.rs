//! Configuration, secret loading, and the chain registry.
//!
//! The dRPC key is loaded from a file (a podman secret) at startup and wrapped
//! in `Key`, whose `Debug` impl prints `Key(***)` so it can never leak through a
//! `{:?}` log line. The key ends up *in the upstream URL path* (that is how dRPC
//! authenticates), so the rule "never log an upstream URL" is load-bearing — see
//! `upstream.rs`.

use std::net::SocketAddr;
use std::time::Duration;

/// A secret string that refuses to print itself.
pub struct Key(String);

impl Key {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Key(***)")
    }
}

/// One supported chain. `slug` is the dRPC path segment; `aliases` are accepted
/// on our own `/rpc/{chain}` route so callers can say `mainnet`, `op`, `1`, etc.
#[derive(Clone)]
pub struct ChainSpec {
    pub slug: &'static str,
    pub chain_id: u64,
    pub aliases: &'static [&'static str],
}

pub fn default_chains() -> Vec<ChainSpec> {
    vec![
        ChainSpec {
            slug: "ethereum",
            chain_id: 1,
            aliases: &["mainnet", "eth", "1"],
        },
        ChainSpec {
            slug: "optimism",
            chain_id: 10,
            aliases: &["op", "10"],
        },
        ChainSpec {
            slug: "base",
            chain_id: 8453,
            aliases: &["8453"],
        },
    ]
}

pub struct Config {
    pub bind: SocketAddr,
    pub drpc_base: String,
    pub key: Key,
    pub chains: Vec<ChainSpec>,
    pub body_limit: usize,
    /// When true, refuse expensive/nonsensical-over-HTTP JSON-RPC methods so a
    /// leaked endpoint can't burn the shared key on archival traces. Off via
    /// `RPC_METHOD_FILTER=off`.
    pub method_denylist: bool,
    pub upstream_timeout: Duration,
    pub connect_timeout: Duration,
}

impl Config {
    pub fn from_env() -> Result<Config, String> {
        let bind: SocketAddr = std::env::var("BIND_ADDR")
            .unwrap_or_else(|_| "127.0.0.1:8080".into())
            .parse()
            .map_err(|e| format!("BIND_ADDR invalid: {e}"))?;

        // Docs show `https://lb.drpc.live`. Some dashboards hand out `lb.drpc.org`.
        // Override with DRPC_BASE if yours differs. No trailing slash.
        let drpc_base =
            std::env::var("DRPC_BASE").unwrap_or_else(|_| "https://lb.drpc.live".into());

        Ok(Config {
            bind,
            drpc_base: drpc_base.trim_end_matches('/').to_string(),
            key: load_key()?,
            chains: default_chains(),
            body_limit: env_usize("RPC_BODY_LIMIT_BYTES", 1 << 20), // 1 MiB
            method_denylist: std::env::var("RPC_METHOD_FILTER")
                .map(|v| v != "off")
                .unwrap_or(true),
            upstream_timeout: Duration::from_secs(env_u64("UPSTREAM_TIMEOUT_SECS", 20)),
            connect_timeout: Duration::from_secs(env_u64("UPSTREAM_CONNECT_TIMEOUT_SECS", 8)),
        })
    }
}

/// Prefer a file (podman secret). Fall back to the default secret mount, then to
/// an env var for local dev only (env vars are visible to anything that can read
/// the process environment; the file path is the production path).
fn load_key() -> Result<Key, String> {
    if let Ok(path) = std::env::var("DRPC_KEY_FILE") {
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| format!("reading DRPC_KEY_FILE {path}: {e}"))?;
        return Ok(Key(raw.trim().to_string()));
    }
    if let Ok(raw) = std::fs::read_to_string("/run/secrets/drpc_key") {
        return Ok(Key(raw.trim().to_string()));
    }
    if let Ok(k) = std::env::var("DRPC_KEY") {
        return Ok(Key(k.trim().to_string()));
    }
    Err("no dRPC key: mount a podman secret and set DRPC_KEY_FILE (or DRPC_KEY for dev)".into())
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}
