//! The only component that talks to dRPC.
//!
//! Privacy invariants enforced here:
//!  - Requests are *originated* by this client, not relayed. No client IP, no
//!    client User-Agent, no client cookies reach dRPC — only the JSON-RPC body,
//!    the address, and the query string the caller asked for.
//!  - URLs contain the API key (dRPC puts it in the path). They are NEVER logged
//!    and NEVER returned in errors.

use crate::config::Key;
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use reqwest::Client;
use serde_json::Value;

pub struct Drpc {
    client: Client,
    base: String,
    key: Key,
}

#[derive(Debug)]
pub enum UpstreamError {
    /// Network/TLS/timeout — anything before we got an HTTP response.
    Transport,
    /// dRPC answered with a non-2xx status (wallet-API calls only; raw RPC
    /// relays the status verbatim instead).
    Status(u16),
    /// Body was not the JSON we expected.
    Decode,
}

impl std::fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpstreamError::Transport => write!(f, "upstream transport error"),
            UpstreamError::Status(c) => write!(f, "upstream status {c}"),
            UpstreamError::Decode => write!(f, "upstream decode error"),
        }
    }
}
impl std::error::Error for UpstreamError {}

impl Drpc {
    pub fn new(client: Client, base: String, key: Key) -> Self {
        Self { client, base, key }
    }

    /// JSON-RPC passthrough. Returns dRPC's status and body unchanged so the
    /// wallet sees real JSON-RPC error objects, gas estimates, etc.
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

    /// Wallet API: current portfolio (native + ERC-20 + DeFi) on one chain.
    /// VERIFY the path/shape against your dashboard (see README).
    pub async fn balances(
        &self,
        chain: &str,
        address: &str,
        query: Option<&str>,
    ) -> Result<Value, UpstreamError> {
        let url = self.wallet_url(chain, address, "balances", query);
        self.get_json(&url).await
    }

    /// Wallet API: transaction history on one chain. VERIFY the suffix
    /// (`transactions`) against the "Get Transactions History" doc.
    pub async fn transactions(
        &self,
        chain: &str,
        address: &str,
        query: Option<&str>,
    ) -> Result<Value, UpstreamError> {
        let url = self.wallet_url(chain, address, "transactions", query);
        self.get_json(&url).await
    }

    fn wallet_url(&self, chain: &str, address: &str, suffix: &str, query: Option<&str>) -> String {
        let mut url = format!(
            "{}/{}/{}/lambda/v2/wallets/{}/{}",
            self.base,
            chain,
            self.key.expose(),
            address,
            suffix
        );
        if let Some(q) = query {
            if !q.is_empty() {
                url.push('?');
                url.push_str(q);
            }
        }
        url
    }

    async fn get_json(&self, url: &str) -> Result<Value, UpstreamError> {
        let resp = self
            .client
            .get(url)
            .header(ACCEPT, "application/json")
            .send()
            .await
            .map_err(|_| UpstreamError::Transport)?;
        let status = resp.status();
        if !status.is_success() {
            return Err(UpstreamError::Status(status.as_u16()));
        }
        resp.json::<Value>().await.map_err(|_| UpstreamError::Decode)
    }
}
