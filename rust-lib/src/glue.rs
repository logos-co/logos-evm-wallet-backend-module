//! Logos module glue for `wallet_backend_module` (rust-first authoring) — the
//! coordinator.
//!
//! Depends on `eth_rpc_module`, `keystore_module`, and `token_list_module`
//! (declared in metadata.json `dependencies`), reached as
//! `modules().<dep>.<method>(...)`. Owns the central proxy + chain config and
//! pushes each chain's `{ endpoint, proxy, proxyRequired }` down into eth_rpc.
//! Multi-chain balances are fetched with Multicall3 (one `eth_call` per chain);
//! sends are built (alloy) → signed (keystore) → broadcast (eth_rpc) → recorded.
//!
//! Compiled only with the default `logos_module` feature; the pure cores
//! (`txbuild`, `config`, `history`) are tested with `cargo test --no-default-features`.

use alloy::primitives::{Address, U256};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::{ChainInfo, ConfigStore, ProxySettings};
use crate::history::{now_secs, History, TxRecord};
use crate::{txbuild, txbuild::Fee};

pub trait WalletBackendModule: Send + 'static {
    // ── central config ──
    fn set_proxy_config(&mut self, proxy_json: String) -> bool;
    fn get_proxy_config(&mut self) -> String;
    fn set_chains(&mut self, chains_json: String) -> bool;
    fn get_chains(&mut self) -> String;
    fn test_endpoint(&mut self, chain_id: i64) -> String;

    // ── accounts (signing stays in the keystore) ──
    fn create_account(&mut self, passphrase: String, label: String) -> String;
    fn import_mnemonic(&mut self, phrase_json: String, label: String) -> String;
    fn list_accounts(&mut self) -> String;
    fn unlock(&mut self, address: String, passphrase: String) -> bool;
    fn lock(&mut self, address: String) -> bool;

    // ── watched tokens ──
    fn set_watched_tokens(&mut self, chain_id: i64, addresses_json: String) -> bool;
    fn get_watched_tokens(&mut self, chain_id: i64) -> String;

    // ── tokens passthrough ──
    fn get_tokens(&mut self, chain_id: i64) -> String;
    fn add_custom_token(&mut self, token_json: String) -> bool;

    // ── balances (Multicall3-batched) ──
    fn refresh_balances(&mut self, address: String) -> bool;
    fn get_balances(&mut self, address: String) -> String;

    // ── send ──
    fn estimate_fee(&mut self, send_json: String) -> String;
    fn send_native(&mut self, send_json: String) -> String;
    fn send_erc20(&mut self, send_json: String) -> String;

    // ── history ──
    fn get_history(&mut self, address: String) -> String;
    fn refresh_tx_status(&mut self, hash_hex: String, chain_id: i64) -> String;

    fn on_context_ready(&mut self, _ctx: &RustModuleContext) {}
}

pub trait WalletBackendModuleEvents {
    fn balances_updated(&self, address: String);
    fn tx_status_changed(&self, hash_hex: String);
    fn proxy_error(&self, context: String);
}

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/generated/provider_gen.rs"));

#[derive(Default)]
struct WalletBackendModuleImpl {
    state: Option<State>,
}

struct State {
    cfg: ConfigStore,
    history: History,
    dir: std::path::PathBuf,
    /// chainId -> watched token addresses (persisted in watched.json).
    watched: std::collections::HashMap<u64, Vec<String>>,
    /// address -> last aggregate balances (persisted in balances_cache.json).
    balances: std::collections::HashMap<String, Value>,
}

// ── small helpers ────────────────────────────────────────────────────────────

fn err(e: impl std::fmt::Display) -> String {
    json!({ "ok": false, "error": e.to_string() }).to_string()
}

/// Parse a dependency's `{ ok, ... }` JSON reply, surfacing `{ok:false}` as Err.
fn ok_value(s: String) -> std::result::Result<Value, String> {
    let v: Value = serde_json::from_str(&s).map_err(|e| e.to_string())?;
    if v.get("ok").and_then(Value::as_bool) == Some(false) {
        return Err(v.get("error").and_then(Value::as_str).unwrap_or("dependency error").to_string());
    }
    Ok(v)
}

fn parse_addr(s: &str) -> std::result::Result<Address, String> {
    let t = s.trim();
    let h = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")).unwrap_or(t);
    let b = hex::decode(h).map_err(|e| e.to_string())?;
    if b.len() != 20 {
        return Err(format!("address must be 20 bytes: {s}"));
    }
    Ok(Address::from_slice(&b))
}

fn parse_hex_u64(s: &str) -> u64 {
    let t = s.trim();
    let h = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")).unwrap_or(t);
    u64::from_str_radix(h, 16).unwrap_or(0)
}

fn parse_u256_str(s: &str) -> U256 {
    let t = s.trim();
    if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        U256::from_str_radix(h, 16).unwrap_or(U256::ZERO)
    } else {
        t.parse::<U256>().unwrap_or(U256::ZERO)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendParams {
    from: String,
    to: String,
    chain_id: u64,
    #[serde(default)]
    amount: String, // wei (native) or token base units (erc20)
    #[serde(default)]
    token_address: String, // erc20 only
}

impl WalletBackendModuleImpl {
    fn st(&mut self) -> std::result::Result<&mut State, String> {
        self.state.as_mut().ok_or_else(|| "backend not initialized (context not ready)".to_string())
    }

    fn save_watched(st: &State) {
        let _ = std::fs::write(
            st.dir.join("watched.json"),
            serde_json::to_string_pretty(&st.watched).unwrap_or_default(),
        );
    }

    fn save_balances(st: &State) {
        let _ = std::fs::write(
            st.dir.join("balances_cache.json"),
            serde_json::to_string_pretty(&st.balances).unwrap_or_default(),
        );
    }

    /// Push every configured chain's RPC + proxy config into eth_rpc.
    fn push_chain_configs(st: &State) {
        for c in &st.cfg.config().chains {
            if let Some(cfg) = st.cfg.eth_rpc_config(c.chain_id) {
                let _ = modules().eth_rpc_module.set_chain_config(c.chain_id as i64, &cfg.to_string());
            }
        }
    }

    /// Fetch native + watched-token balances for one chain with a single
    /// Multicall3 `eth_call`. Falls back to per-call `eth_getBalance` if the
    /// chain has no Multicall3 or the batch fails.
    fn fetch_chain(&mut self, chain_id: u64, holder: &str) -> std::result::Result<Value, String> {
        let (mc, tokens): (Option<String>, Vec<String>) = {
            let st = self.st()?;
            let chain = st.cfg.chain(chain_id).ok_or_else(|| format!("no chain {chain_id}"))?;
            (chain.multicall3_addr(), st.watched.get(&chain_id).cloned().unwrap_or_default())
        };
        let holder_addr = parse_addr(holder)?;

        if let Some(mc) = mc {
            if let Ok(v) = self.fetch_chain_multicall(chain_id, holder_addr, &mc, &tokens) {
                return Ok(v);
            }
        }
        self.fetch_chain_individual(chain_id, holder, &tokens)
    }

    fn fetch_chain_multicall(
        &mut self,
        chain_id: u64,
        holder: Address,
        mc: &str,
        tokens: &[String],
    ) -> std::result::Result<Value, String> {
        let mc_addr = parse_addr(mc)?;
        let mut calls: Vec<(Address, Vec<u8>)> =
            vec![(mc_addr, txbuild::multicall3_get_eth_balance_calldata(holder))];
        for t in tokens {
            calls.push((parse_addr(t)?, txbuild::erc20_balance_of_calldata(holder)));
        }
        let data = txbuild::multicall3_aggregate3_calldata(&calls);
        let call_json = json!({ "to": mc, "data": format!("0x{}", hex::encode(data)) }).to_string();
        let resp = ok_value(modules().eth_rpc_module.call(chain_id as i64, &call_json).map_err(|e| e.to_string())?)?;
        let result_hex = resp["result"].as_str().ok_or("multicall: no result")?;
        let bytes = hex::decode(result_hex.trim_start_matches("0x")).map_err(|e| e.to_string())?;
        let rets = txbuild::decode_aggregate3_returns(&bytes).ok_or("multicall: decode failed")?;

        let native = rets.first().and_then(|o| o.as_ref()).and_then(|d| txbuild::decode_uint256(d)).unwrap_or(U256::ZERO);
        let mut token_balances = Vec::new();
        for (i, t) in tokens.iter().enumerate() {
            let bal = rets.get(i + 1).and_then(|o| o.as_ref()).and_then(|d| txbuild::decode_uint256(d)).unwrap_or(U256::ZERO);
            token_balances.push(json!({ "address": t, "balance": bal.to_string() }));
        }
        Ok(json!({ "chainId": chain_id, "native": native.to_string(), "tokens": token_balances }))
    }

    fn fetch_chain_individual(
        &mut self,
        chain_id: u64,
        holder: &str,
        tokens: &[String],
    ) -> std::result::Result<Value, String> {
        let native_resp = ok_value(
            modules().eth_rpc_module.get_balance(chain_id as i64, holder).map_err(|e| e.to_string())?,
        )?;
        let native = parse_u256_str(native_resp["result"].as_str().unwrap_or("0x0"));
        let mut token_balances = Vec::new();
        for t in tokens {
            let data = txbuild::erc20_balance_of_calldata(parse_addr(holder)?);
            let call_json = json!({ "to": t, "data": format!("0x{}", hex::encode(data)) }).to_string();
            let bal = match ok_value(modules().eth_rpc_module.call(chain_id as i64, &call_json).map_err(|e| e.to_string())?) {
                Ok(v) => v["result"]
                    .as_str()
                    .and_then(|h| txbuild::decode_uint256(&hex::decode(h.trim_start_matches("0x")).ok()?))
                    .unwrap_or(U256::ZERO),
                Err(_) => U256::ZERO,
            };
            token_balances.push(json!({ "address": t, "balance": bal.to_string() }));
        }
        Ok(json!({ "chainId": chain_id, "native": native.to_string(), "tokens": token_balances }))
    }

    /// Build → sign → broadcast → record. `erc20` carries (token, amount) when set.
    fn do_send(&mut self, p: &SendParams, erc20: Option<(Address, U256)>) -> std::result::Result<Value, String> {
        let from = p.from.clone();
        let chain_id = p.chain_id;

        // nonce + a simple EIP-1559 fee derived from gasPrice
        let nonce = parse_hex_u64(
            ok_value(modules().eth_rpc_module.get_transaction_count(chain_id as i64, &from).map_err(|e| e.to_string())?)?
                ["result"].as_str().unwrap_or("0x0"),
        );
        let gas_price = parse_u256_str(
            ok_value(modules().eth_rpc_module.gas_price(chain_id as i64).map_err(|e| e.to_string())?)?
                ["result"].as_str().unwrap_or("0x0"),
        );
        let fee = Fee::Eip1559 {
            max_fee_per_gas: gas_price.saturating_mul(U256::from(2)),
            max_priority_fee_per_gas: gas_price,
        };

        // estimate gas against a from/to/value/data shape
        let to_addr = parse_addr(&p.to)?;
        let (est_to, est_value, est_data, default_gas): (String, String, String, u64) = match &erc20 {
            Some((token, amount)) => {
                let data = txbuild::erc20_transfer_calldata(to_addr, *amount);
                (format!("{token}"), "0x0".into(), format!("0x{}", hex::encode(data)), 90_000)
            }
            None => (p.to.clone(), format!("0x{:x}", parse_u256_str(&p.amount)), "0x".into(), 21_000),
        };
        let est_tx = json!({ "from": from, "to": est_to, "value": est_value, "data": est_data }).to_string();
        let gas_limit = match modules().eth_rpc_module.estimate_gas(chain_id as i64, &est_tx) {
            Ok(s) => ok_value(s).ok().and_then(|v| v["result"].as_str().map(parse_hex_u64)).unwrap_or(default_gas),
            Err(_) => default_gas,
        };

        // build the unsigned tx
        let unsigned = match &erc20 {
            Some((token, amount)) => txbuild::unsigned_erc20_tx(*token, to_addr, *amount, nonce, gas_limit, &fee),
            None => txbuild::unsigned_native_tx(to_addr, parse_u256_str(&p.amount), nonce, gas_limit, &fee),
        };

        // sign (keystore) → broadcast (eth_rpc)
        let signed = ok_value(
            modules().keystore_module.sign_transaction(&from, &unsigned.to_string(), chain_id as i64).map_err(|e| e.to_string())?,
        )?;
        let raw = signed["raw"].as_str().ok_or("keystore: no raw tx")?.to_string();
        let bcast = ok_value(
            modules().eth_rpc_module.send_raw_transaction(chain_id as i64, &raw).map_err(|e| e.to_string())?,
        )?;
        let hash = bcast["hash"].as_str().ok_or("broadcast: no hash")?.to_string();

        // record + notify
        let (kind, token, value) = match &erc20 {
            Some((token, amount)) => ("erc20", Some(format!("{token}")), amount.to_string()),
            None => ("native", None, parse_u256_str(&p.amount).to_string()),
        };
        let record = TxRecord {
            hash: hash.clone(),
            chain_id,
            from: from.clone(),
            to: p.to.clone(),
            value,
            kind: kind.into(),
            token,
            status: "pending".into(),
            timestamp: now_secs(),
        };
        if let Ok(st) = self.st() {
            st.history.add(&from, record);
        }
        emit_tx_status_changed(&hash);
        Ok(json!({ "ok": true, "hash": hash }))
    }
}

impl WalletBackendModule for WalletBackendModuleImpl {
    fn on_context_ready(&mut self, ctx: &RustModuleContext) {
        let dir = std::path::PathBuf::from(&ctx.instance_persistence_path);
        let cfg = ConfigStore::with_path(dir.join("config.json"));
        let history = History::new(dir.clone());
        let watched = std::fs::read_to_string(dir.join("watched.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let balances = std::fs::read_to_string(dir.join("balances_cache.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let st = State { cfg, history, dir, watched, balances };
        Self::push_chain_configs(&st);
        self.state = Some(st);
    }

    fn set_proxy_config(&mut self, proxy_json: String) -> bool {
        let proxy: ProxySettings = match serde_json::from_str(&proxy_json) {
            Ok(p) => p,
            Err(_) => return false,
        };
        match self.st() {
            Ok(st) => {
                st.cfg.set_proxy(proxy);
                Self::push_chain_configs(st);
                true
            }
            Err(_) => false,
        }
    }

    fn get_proxy_config(&mut self) -> String {
        match self.st() {
            Ok(st) => json!({ "ok": true, "proxy": st.cfg.config().proxy }).to_string(),
            Err(e) => err(e),
        }
    }

    fn set_chains(&mut self, chains_json: String) -> bool {
        let chains: Vec<ChainInfo> = match serde_json::from_str(&chains_json) {
            Ok(c) => c,
            Err(_) => return false,
        };
        match self.st() {
            Ok(st) => {
                st.cfg.set_chains(chains);
                Self::push_chain_configs(st);
                true
            }
            Err(_) => false,
        }
    }

    fn get_chains(&mut self) -> String {
        match self.st() {
            Ok(st) => json!({ "ok": true, "chains": st.cfg.config().chains }).to_string(),
            Err(e) => err(e),
        }
    }

    fn test_endpoint(&mut self, chain_id: i64) -> String {
        match modules().eth_rpc_module.verify_chain_id(chain_id) {
            Ok(s) => s,
            Err(e) => err(e),
        }
    }

    fn create_account(&mut self, passphrase: String, label: String) -> String {
        let resp = match modules().keystore_module.new_account(&passphrase) {
            Ok(s) => s,
            Err(e) => return err(e),
        };
        self.label_new_account(resp, label)
    }

    fn import_mnemonic(&mut self, phrase_json: String, label: String) -> String {
        let resp = match modules().keystore_module.import_mnemonic(&phrase_json) {
            Ok(s) => s,
            Err(e) => return err(e),
        };
        self.label_new_account(resp, label)
    }

    fn list_accounts(&mut self) -> String {
        match modules().keystore_module.list_accounts() {
            Ok(s) => s,
            Err(e) => err(e),
        }
    }

    fn unlock(&mut self, address: String, passphrase: String) -> bool {
        modules().keystore_module.unlock(&address, &passphrase).unwrap_or(false)
    }

    fn lock(&mut self, address: String) -> bool {
        modules().keystore_module.lock(&address).unwrap_or(false)
    }

    fn set_watched_tokens(&mut self, chain_id: i64, addresses_json: String) -> bool {
        let addrs: Vec<String> = match serde_json::from_str(&addresses_json) {
            Ok(a) => a,
            Err(_) => return false,
        };
        match self.st() {
            Ok(st) => {
                st.watched.insert(chain_id as u64, addrs);
                Self::save_watched(st);
                true
            }
            Err(_) => false,
        }
    }

    fn get_watched_tokens(&mut self, chain_id: i64) -> String {
        match self.st() {
            Ok(st) => json!({ "ok": true, "tokens": st.watched.get(&(chain_id as u64)).cloned().unwrap_or_default() }).to_string(),
            Err(e) => err(e),
        }
    }

    fn get_tokens(&mut self, chain_id: i64) -> String {
        match modules().token_list_module.get_tokens(chain_id) {
            Ok(s) => s,
            Err(e) => err(e),
        }
    }

    fn add_custom_token(&mut self, token_json: String) -> bool {
        modules().token_list_module.add_custom_token(&token_json).unwrap_or(false)
    }

    fn refresh_balances(&mut self, address: String) -> bool {
        let chain_ids: Vec<u64> = match self.st() {
            Ok(st) => st.cfg.config().chains.iter().map(|c| c.chain_id).collect(),
            Err(_) => return false,
        };
        let mut per_chain = Vec::new();
        for chain_id in chain_ids {
            if let Ok(v) = self.fetch_chain(chain_id, &address) {
                per_chain.push(v);
            }
        }
        let aggregate = json!({ "address": address, "chains": per_chain });
        if let Ok(st) = self.st() {
            st.balances.insert(address.clone(), aggregate);
            Self::save_balances(st);
        }
        emit_balances_updated(&address);
        true
    }

    fn get_balances(&mut self, address: String) -> String {
        match self.st() {
            Ok(st) => match st.balances.get(&address) {
                Some(v) => json!({ "ok": true, "balances": v }).to_string(),
                None => json!({ "ok": true, "balances": { "address": address, "chains": [] } }).to_string(),
            },
            Err(e) => err(e),
        }
    }

    fn estimate_fee(&mut self, send_json: String) -> String {
        let p: SendParams = match serde_json::from_str(&send_json) {
            Ok(p) => p,
            Err(e) => return err(e),
        };
        let gas_price = match modules().eth_rpc_module.gas_price(p.chain_id as i64) {
            Ok(s) => ok_value(s).ok().and_then(|v| v["result"].as_str().map(parse_u256_str)).unwrap_or(U256::ZERO),
            Err(e) => return err(e),
        };
        let gas = if p.token_address.is_empty() { 21_000u64 } else { 90_000u64 };
        let fee = gas_price.saturating_mul(U256::from(gas));
        json!({ "ok": true, "gasPrice": gas_price.to_string(), "gasLimit": gas, "feeWei": fee.to_string() }).to_string()
    }

    fn send_native(&mut self, send_json: String) -> String {
        let p: SendParams = match serde_json::from_str(&send_json) {
            Ok(p) => p,
            Err(e) => return err(e),
        };
        match self.do_send(&p, None) {
            Ok(v) => v.to_string(),
            Err(e) => err(e),
        }
    }

    fn send_erc20(&mut self, send_json: String) -> String {
        let p: SendParams = match serde_json::from_str(&send_json) {
            Ok(p) => p,
            Err(e) => return err(e),
        };
        let token = match parse_addr(&p.token_address) {
            Ok(a) => a,
            Err(e) => return err(e),
        };
        let amount = parse_u256_str(&p.amount);
        match self.do_send(&p, Some((token, amount))) {
            Ok(v) => v.to_string(),
            Err(e) => err(e),
        }
    }

    fn get_history(&mut self, address: String) -> String {
        match self.st() {
            Ok(st) => json!({ "ok": true, "history": st.history.list(&address) }).to_string(),
            Err(e) => err(e),
        }
    }

    fn refresh_tx_status(&mut self, hash_hex: String, chain_id: i64) -> String {
        let receipt = match modules().eth_rpc_module.get_transaction_receipt(chain_id, &hash_hex) {
            Ok(s) => s,
            Err(e) => return err(e),
        };
        let v = match ok_value(receipt) {
            Ok(v) => v,
            Err(e) => return err(e),
        };
        // null result => still pending
        let status = match v.get("result") {
            Some(Value::Null) | None => "pending",
            Some(r) => {
                if r.get("status").and_then(Value::as_str) == Some("0x1") {
                    "confirmed"
                } else {
                    "failed"
                }
            }
        };
        // update the owning account's record (search all known history files is
        // overkill; the UI passes the address-scoped call separately if needed).
        if status != "pending" {
            if let Ok(st) = self.st() {
                // best-effort: update across the sender's file when present
                for entry in std::fs::read_dir(st.dir.join("history")).into_iter().flatten().flatten() {
                    if let Some(name) = entry.file_name().to_str().and_then(|n| n.strip_suffix(".json")) {
                        if st.history.update_status(name, &hash_hex, status) {
                            break;
                        }
                    }
                }
            }
        }
        emit_tx_status_changed(&hash_hex);
        json!({ "ok": true, "status": status }).to_string()
    }
}

impl WalletBackendModuleImpl {
    /// Persist an address->label after the keystore creates/imports it.
    fn label_new_account(&mut self, keystore_reply: String, label: String) -> String {
        let v = match ok_value(keystore_reply.clone()) {
            Ok(v) => v,
            Err(e) => return err(e),
        };
        if let Some(addr) = v.get("address").and_then(Value::as_str) {
            if let Ok(st) = self.st() {
                let p = st.dir.join("labels.json");
                let mut labels: std::collections::HashMap<String, String> =
                    std::fs::read_to_string(&p).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
                labels.insert(addr.to_lowercase(), label);
                let _ = std::fs::write(p, serde_json::to_string_pretty(&labels).unwrap_or_default());
            }
        }
        keystore_reply
    }
}

#[no_mangle]
pub extern "Rust" fn logos_module_install() {
    install::<WalletBackendModuleImpl>();
}
