//! The only component that talks to dRPC.
//!
//! Privacy invariants enforced here:
//!  - Requests are *originated* by this client, not relayed. No client IP, no
//!    client User-Agent, no client cookies reach dRPC — only the JSON-RPC body,
//!    the address, and the query string the caller asked for.
//!  - URLs contain the API key (dRPC puts it in the path). They are NEVER logged
//!    and NEVER returned in errors.
//!
//! All three calls are passthroughs: once dRPC answers, we return its status
//! and body unchanged. The wallet therefore sees real JSON-RPC error objects,
//! gas estimates, and dRPC's own diagnostics (e.g. the free-tier
//! `{"message":"method is not available on freetier","code":35}`) instead of a
//! status we invented. The only error we raise ourselves is `Transport`, for
//! failures that happen before dRPC produces a response.

use crate::config::Key;
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use reqwest::Client;

pub struct Drpc {
    client: Client,
    base: String,
    key: Key,
}

#[derive(Debug)]
pub enum UpstreamError {
    /// Network/TLS/timeout — anything before we got an HTTP response. Once dRPC
    /// answers, its status and body are passed through verbatim, never
    /// collapsed into an error.
    Transport,
}

impl std::fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpstreamError::Transport => write!(f, "upstream transport error"),
        }
    }
}
impl std::error::Error for UpstreamError {}

impl Drpc {
    pub fn new(client: Client, base: String, key: Key) -> Self {
        Self { client, base, key }
    }

    /// JSON-RPC passthrough. Returns dRPC's status and body unchanged.
    pub async fn rpc(&self, chain: &str, body: &[u8]) -> Result<(u16, Vec<u8>), UpstreamError> {
        let url = format!("{}/{}/{}", self.base, chain, self.key.expose());
        let resp = self
            .client
            .post(&url)
            .header(CONTENT_TYPE, "application/json")
            .body(body.to_vec())
            .send()
            .await
            .map_err(|_| UpstreamError::Transport)?;
        let status = resp.status().as_u16();
        let bytes = resp.bytes().await.map_err(|_| UpstreamError::Transport)?;
        Ok((status, bytes.to_vec()))
    }

    /// Wallet API: balances (native + ERC-20) on one chain. Mirrors dRPC's
    /// `GET …/lambda/v2/wallets/{address}/balances`; status and body verbatim.
    pub async fn balances(
        &self,
        chain: &str,
        address: &str,
        query: Option<&str>,
    ) -> Result<(u16, Vec<u8>), UpstreamError> {
        let mut url = format!(
            "{}/{}/{}/lambda/v2/wallets/{}/balances",
            self.base,
            chain,
            self.key.expose(),
            address,
        );
        append_query(&mut url, query);
        self.get(&url).await
    }

    /// Wallet API: transaction history on one chain. Mirrors dRPC's
    /// `GET …/lambda/v1/transactions/{address}/history` — note the history
    /// endpoint lives under `lambda/v1/transactions`, NOT `v2/wallets`.
    pub async fn transactions(
        &self,
        chain: &str,
        address: &str,
        query: Option<&str>,
    ) -> Result<(u16, Vec<u8>), UpstreamError> {
        let mut url = format!(
            "{}/{}/{}/lambda/v1/transactions/{}/history",
            self.base,
            chain,
            self.key.expose(),
            address,
        );
        append_query(&mut url, query);
        self.get(&url).await
    }

    async fn get(&self, url: &str) -> Result<(u16, Vec<u8>), UpstreamError> {
        let resp = self
            .client
            .get(url)
            .header(ACCEPT, "application/json")
            .send()
            .await
            .map_err(|_| UpstreamError::Transport)?;
        let status = resp.status().as_u16();
        let bytes = resp.bytes().await.map_err(|_| UpstreamError::Transport)?;
        Ok((status, bytes.to_vec()))
    }
}

/// Append `?query` to `url` when the caller supplied a non-empty query string.
fn append_query(url: &mut String, query: Option<&str>) {
    if let Some(q) = query {
        if !q.is_empty() {
            url.push('?');
            url.push_str(q);
        }
    }
}
