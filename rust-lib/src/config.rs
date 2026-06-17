//! Central wallet configuration — the single source of truth for chains and the
//! proxy policy. The backend pushes each chain's `{ endpoint, proxy,
//! proxyRequired }` down into `eth_rpc_module` via `set_chain_config`.
//!
//! Pure Rust, unit-tested with `cargo test`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::txbuild::MULTICALL3;

/// A configured EVM chain.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainInfo {
    pub chain_id: u64,
    pub name: String,
    pub rpc_url: String,
    pub native_symbol: String,
    /// Multicall3 address (defaults to the canonical deployment).
    #[serde(default)]
    pub multicall3: Option<String>,
    #[serde(default)]
    pub explorer: Option<String>,
}

impl ChainInfo {
    pub fn multicall3_addr(&self) -> Option<String> {
        self.multicall3.clone().or_else(|| Some(MULTICALL3.to_string()))
    }
}

/// Proxy policy applied to all outbound traffic.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxySettings {
    #[serde(default)]
    pub proxy: Option<String>,
    #[serde(default)]
    pub proxy_required: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WalletConfig {
    pub chains: Vec<ChainInfo>,
    #[serde(default)]
    pub proxy: ProxySettings,
}

impl Default for WalletConfig {
    fn default() -> Self {
        Self { chains: default_chains(), proxy: ProxySettings::default() }
    }
}

fn chain(chain_id: u64, name: &str, rpc: &str, sym: &str, explorer: &str) -> ChainInfo {
    ChainInfo {
        chain_id,
        name: name.into(),
        rpc_url: rpc.into(),
        native_symbol: sym.into(),
        multicall3: None,
        explorer: Some(explorer.into()),
    }
}

/// A reasonable default multi-chain set (public RPC endpoints; the user can
/// override these — especially to point at their own / Tor-friendly nodes).
pub fn default_chains() -> Vec<ChainInfo> {
    vec![
        chain(1, "Ethereum", "https://eth.llamarpc.com", "ETH", "https://etherscan.io"),
        chain(10, "Optimism", "https://mainnet.optimism.io", "ETH", "https://optimistic.etherscan.io"),
        chain(42161, "Arbitrum One", "https://arb1.arbitrum.io/rpc", "ETH", "https://arbiscan.io"),
        chain(8453, "Base", "https://mainnet.base.org", "ETH", "https://basescan.org"),
        chain(137, "Polygon", "https://polygon-rpc.com", "POL", "https://polygonscan.com"),
        chain(11155111, "Sepolia", "https://ethereum-sepolia-rpc.publicnode.com", "ETH", "https://sepolia.etherscan.io"),
    ]
}

/// Load/save the config from a JSON file, seeding defaults on first run.
pub struct ConfigStore {
    path: Option<PathBuf>,
    config: WalletConfig,
}

impl ConfigStore {
    pub fn with_path(path: PathBuf) -> Self {
        let config = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let mut s = Self { path: Some(path), config };
        s.persist(); // seed defaults if the file was missing
        s
    }

    pub fn config(&self) -> &WalletConfig {
        &self.config
    }

    fn persist(&self) {
        if let Some(p) = &self.path {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(txt) = serde_json::to_string_pretty(&self.config) {
                let _ = std::fs::write(p, txt);
            }
        }
    }

    pub fn set_proxy(&mut self, proxy: ProxySettings) {
        self.config.proxy = proxy;
        self.persist();
    }

    pub fn set_chains(&mut self, chains: Vec<ChainInfo>) {
        self.config.chains = chains;
        self.persist();
    }

    pub fn chain(&self, chain_id: u64) -> Option<&ChainInfo> {
        self.config.chains.iter().find(|c| c.chain_id == chain_id)
    }

    /// The per-chain config payload for `eth_rpc_module.set_chain_config`
    /// (camelCase to match `ChainConfig`).
    pub fn eth_rpc_config(&self, chain_id: u64) -> Option<Value> {
        let c = self.chain(chain_id)?;
        Some(json!({
            "endpoint": c.rpc_url,
            "proxy": self.config.proxy.proxy,
            "proxyRequired": self.config.proxy.proxy_required,
            "timeoutSecs": 30,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_seeded_and_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        {
            let store = ConfigStore::with_path(path.clone());
            assert!(store.config().chains.len() >= 5);
            assert!(store.chain(1).is_some());
        }
        assert!(path.exists()); // seeded on first run
    }

    #[test]
    fn eth_rpc_config_carries_proxy_policy() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = ConfigStore::with_path(dir.path().join("config.json"));
        store.set_proxy(ProxySettings { proxy: Some("socks5h://127.0.0.1:9050".into()), proxy_required: true });
        let cfg = store.eth_rpc_config(1).unwrap();
        assert_eq!(cfg["endpoint"], "https://eth.llamarpc.com");
        assert_eq!(cfg["proxyRequired"], true);
        assert_eq!(cfg["proxy"], "socks5h://127.0.0.1:9050");
    }

    #[test]
    fn multicall3_defaults_to_canonical() {
        let c = &default_chains()[0];
        assert_eq!(c.multicall3_addr().unwrap(), MULTICALL3);
    }
}
