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
use std::sync::Arc; // `Mutex` is already in scope from the generated provider glue.

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

    // ── market (Uniswap prices for held tokens) ──
    fn get_market(&mut self, address: String) -> String;
    /// Refresh prices for held tokens across all chains concurrently (fans out one
    /// `uniswap.get_prices` per chain), caches them, and emits `market_updated`.
    /// `get_market` then reads the cache. Returns immediately.
    fn refresh_market(&mut self, address: String) -> bool;

    // ── send ──
    fn estimate_fee(&mut self, send_json: String) -> String;
    fn send_native(&mut self, send_json: String) -> String;
    fn send_erc20(&mut self, send_json: String) -> String;

    // ── private (RAILGUN) ──
    //
    // ⚠️ UNAUDITED upstream engine; Sepolia-first. Spending/viewing keys never
    // leave `railgun_module`; here we only coordinate.
    /// Initialise the private (RAILGUN) account for `address` on `chain_id`. Derives
    /// the railgun keys from a deterministic `keystore.sign_message` signature (the
    /// EOA must be unlocked) and hands the seed to `railgun_module` (which derives +
    /// holds the keys). Returns `{ ok, address: "0zk…" }`. Idempotent per EOA+chain.
    fn init_private(&mut self, address: String, chain_id: i64) -> String;
    /// The private `0zk…` address (`{ ok, address }`) — requires `init_private`.
    fn get_zk_address(&mut self) -> String;
    /// Sync the shielded state to the latest block (`{ ok }`).
    fn sync_private(&mut self) -> String;
    /// Shielded balance per asset (`{ ok, balances: [...] }`).
    fn get_shielded_balance(&mut self) -> String;
    /// SHIELD public ERC-20 into the pool: `{ from, chainId, asset, amount }`.
    /// Approves the RAILGUN smart wallet then broadcasts the shield tx(s) — both
    /// signed by keystore + broadcast by eth_rpc. Returns `{ ok, hash, approveHash }`.
    fn shield(&mut self, send_json: String) -> String;
    /// PRIVATE SEND (sender-hiding, via the 4337 relayer): `{ from, chainId, to,
    /// asset, amount, memo?, bundlerUrl }`. `to` = `0zk…` → private transfer, `to` =
    /// `0x…` → unshield. `railgun_module` builds+signs (via keystore)+submits the
    /// UserOp through the bundler; returns `{ ok, userOpHash }`.
    fn private_send(&mut self, send_json: String) -> String;

    // ── history ──
    fn get_history(&mut self, address: String) -> String;
    fn refresh_tx_status(&mut self, hash_hex: String, chain_id: i64) -> String;

    fn on_context_ready(&mut self, _ctx: &RustModuleContext) {}
}

pub trait WalletBackendModuleEvents {
    fn balances_updated(&self, address: String);
    fn market_updated(&self, address: String);
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
    /// `Arc<Mutex<_>>` so the async balance-refresh fan-out's completion callback
    /// (a `'static` closure that can't borrow `&self`) can write it; the event
    /// loop is single-threaded, so the lock is effectively uncontended.
    balances: Arc<Mutex<std::collections::HashMap<String, Value>>>,
    /// chainId -> (token address -> (eth, usd)) prices — populated by the
    /// `refresh_market` fan-out, read by `get_market`. Ephemeral (not persisted).
    market_prices: Arc<Mutex<std::collections::HashMap<u64, std::collections::HashMap<String, (Option<f64>, Option<f64>)>>>>,
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

/// `balance / 10^decimals * usd` as a display value (f64 is ample for UI).
fn value_usd(balance: &str, decimals: u8, usd: Option<f64>) -> Option<f64> {
    let usd = usd?;
    let bal: f64 = parse_u256_str(balance).to_string().parse().unwrap_or(0.0);
    Some(bal / 10f64.powi(decimals as i32) * usd)
}

/// Fallback symbol for an unknown token: `0x1234…abcd`.
fn short_addr(a: &str) -> String {
    let t = a.trim_start_matches("0x");
    if t.len() >= 8 {
        format!("0x{}…{}", &t[..4], &t[t.len() - 4..])
    } else {
        a.to_string()
    }
}

fn parse_u256_str(s: &str) -> U256 {
    let t = s.trim();
    if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        U256::from_str_radix(h, 16).unwrap_or(U256::ZERO)
    } else {
        t.parse::<U256>().unwrap_or(U256::ZERO)
    }
}

// ── async balance fan-out (over the concurrency:"multi" eth_rpc) ──────────────
//
// `refresh_balances` fires one async eth_rpc call per chain (a single Multicall3
// `eth_call` when available, else native + per-token via a nested gather) and
// returns immediately. Because eth_rpc is `concurrency: "multi"`, the calls run
// in parallel; `gather` collects them and the final completion writes the cache
// and emits `balances_updated` — the same contract every consumer already uses.

type GatherDone<T> = Box<dyn FnOnce(T) + Send>;
type GatherTask<T> = Box<dyn FnOnce(GatherDone<T>) + Send>;

/// Fire every task; invoke `on_all` with all results (in task order) once the last
/// completes. Each task fires its async dep call(s) and hands its result to `done`.
/// Generic + dependency-free — a candidate to lift into the rust SDK.
#[allow(clippy::type_complexity)]
fn gather<T: Send + 'static>(tasks: Vec<GatherTask<T>>, on_all: impl FnOnce(Vec<T>) + Send + 'static) {
    let n = tasks.len();
    if n == 0 {
        on_all(Vec::new());
        return;
    }
    let state: Arc<Mutex<(Vec<Option<T>>, usize, Option<Box<dyn FnOnce(Vec<T>) + Send>>)>> =
        Arc::new(Mutex::new(((0..n).map(|_| None).collect(), n, Some(Box::new(on_all)))));
    for (i, task) in tasks.into_iter().enumerate() {
        let state = Arc::clone(&state);
        task(Box::new(move |result: T| {
            let mut s = state.lock().unwrap();
            s.0[i] = Some(result);
            s.1 -= 1;
            if s.1 == 0 {
                let results: Vec<T> = s.0.drain(..).map(|o| o.expect("gather slot filled")).collect();
                let cb = s.2.take().expect("gather on_all fires once");
                drop(s);
                cb(results);
            }
        }));
    }
}

/// One chain's balances → `{ chainId, native, tokens }`, fetched async. Common
/// case: a single Multicall3 `eth_call`; else native + per-token via nested gather.
fn fetch_chain_async(
    chain_id: u64,
    holder: Address,
    holder_hex: String,
    multicall: Option<String>,
    tokens: Vec<String>,
    done: GatherDone<Value>,
) {
    if let Some(mc) = multicall {
        if let Some(call_json) = build_multicall_call_json(&mc, holder, &tokens) {
            modules().eth_rpc_module.call_async(chain_id as i64, &call_json, move |res| {
                done(decode_multicall_chain(chain_id, &tokens, res.ok()));
            });
            return;
        }
    }
    fetch_chain_individual_async(chain_id, holder, holder_hex, tokens, done);
}

/// Fallback for chains without Multicall3: native `eth_getBalance` + one `eth_call`
/// per token, gathered into the same chain value.
fn fetch_chain_individual_async(
    chain_id: u64,
    holder: Address,
    holder_hex: String,
    tokens: Vec<String>,
    done: GatherDone<Value>,
) {
    let mut tasks: Vec<GatherTask<(usize, U256)>> = Vec::with_capacity(tokens.len() + 1);
    tasks.push(Box::new(move |d| {
        modules()
            .eth_rpc_module
            .get_balance_async(chain_id as i64, &holder_hex, move |res| d((0, decode_balance_reply(res.ok()))));
    }));
    for (i, t) in tokens.iter().enumerate() {
        let call_json = erc20_balance_call_json(t, holder);
        let slot = i + 1;
        tasks.push(Box::new(move |d| {
            modules()
                .eth_rpc_module
                .call_async(chain_id as i64, &call_json, move |res| d((slot, decode_call_balance_reply(res.ok()))));
        }));
    }
    gather(tasks, move |parts: Vec<(usize, U256)>| {
        let mut by_slot: std::collections::HashMap<usize, U256> = std::collections::HashMap::new();
        for (s, v) in parts {
            by_slot.insert(s, v);
        }
        let native = by_slot.get(&0).copied().unwrap_or(U256::ZERO);
        let token_balances: Vec<Value> = tokens
            .iter()
            .enumerate()
            .map(|(i, t)| json!({ "address": t, "balance": by_slot.get(&(i + 1)).copied().unwrap_or(U256::ZERO).to_string() }))
            .collect();
        done(json!({ "chainId": chain_id, "native": native.to_string(), "tokens": token_balances }));
    });
}

fn build_multicall_call_json(mc: &str, holder: Address, tokens: &[String]) -> Option<String> {
    let mc_addr = parse_addr(mc).ok()?;
    let mut calls: Vec<(Address, Vec<u8>)> = vec![(mc_addr, txbuild::multicall3_get_eth_balance_calldata(holder))];
    for t in tokens {
        calls.push((parse_addr(t).ok()?, txbuild::erc20_balance_of_calldata(holder)));
    }
    let data = txbuild::multicall3_aggregate3_calldata(&calls);
    Some(json!({ "to": mc, "data": format!("0x{}", hex::encode(data)) }).to_string())
}

fn erc20_balance_call_json(token: &str, holder: Address) -> String {
    let data = txbuild::erc20_balance_of_calldata(holder);
    json!({ "to": token, "data": format!("0x{}", hex::encode(data)) }).to_string()
}

fn empty_chain(chain_id: u64, tokens: &[String]) -> Value {
    let token_balances: Vec<Value> = tokens.iter().map(|t| json!({ "address": t, "balance": "0" })).collect();
    json!({ "chainId": chain_id, "native": "0", "tokens": token_balances })
}

/// Decode a Multicall3 `aggregate3` reply (the `{ ok, result }` string from
/// eth_rpc.call) into `{ chainId, native, tokens }`. Any failure degrades to zeros.
fn decode_multicall_chain(chain_id: u64, tokens: &[String], reply: Option<String>) -> Value {
    let rets = reply
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .filter(|v| v.get("ok").and_then(Value::as_bool) != Some(false))
        .and_then(|v| v["result"].as_str().map(String::from))
        .and_then(|h| hex::decode(h.trim_start_matches("0x")).ok())
        .and_then(|b| txbuild::decode_aggregate3_returns(&b));
    let Some(rets) = rets else {
        return empty_chain(chain_id, tokens);
    };
    let native = rets.first().and_then(|o| o.as_ref()).and_then(|d| txbuild::decode_uint256(d)).unwrap_or(U256::ZERO);
    let token_balances: Vec<Value> = tokens
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let bal = rets.get(i + 1).and_then(|o| o.as_ref()).and_then(|d| txbuild::decode_uint256(d)).unwrap_or(U256::ZERO);
            json!({ "address": t, "balance": bal.to_string() })
        })
        .collect();
    json!({ "chainId": chain_id, "native": native.to_string(), "tokens": token_balances })
}

/// Decode a native `eth_getBalance` reply (`{ ok, result: "0x.." }`) → U256.
fn decode_balance_reply(reply: Option<String>) -> U256 {
    reply
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .filter(|v| v.get("ok").and_then(Value::as_bool) != Some(false))
        .and_then(|v| v["result"].as_str().map(parse_u256_str))
        .unwrap_or(U256::ZERO)
}

/// Decode an ERC20 `balanceOf` `eth_call` reply (`{ ok, result: "0x..32" }`) → U256.
fn decode_call_balance_reply(reply: Option<String>) -> U256 {
    reply
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .filter(|v| v.get("ok").and_then(Value::as_bool) != Some(false))
        .and_then(|v| v["result"].as_str().map(String::from))
        .and_then(|h| hex::decode(h.trim_start_matches("0x")).ok())
        .and_then(|b| txbuild::decode_uint256(&b))
        .unwrap_or(U256::ZERO)
}

/// Decode a uniswap `get_prices` reply (`{ prices: [{address, eth, usd}] }`) into
/// a `token address -> (eth, usd)` map. Any failure yields an empty map.
fn decode_uniswap_prices(reply: Option<String>) -> std::collections::HashMap<String, (Option<f64>, Option<f64>)> {
    let mut out = std::collections::HashMap::new();
    let Some(v) = reply.and_then(|s| serde_json::from_str::<Value>(&s).ok()) else {
        return out;
    };
    if let Some(arr) = v.get("prices").and_then(Value::as_array) {
        for p in arr {
            if let Some(addr) = p.get("address").and_then(Value::as_str) {
                let eth = p.get("eth").and_then(Value::as_f64);
                let usd = p.get("usd").and_then(Value::as_f64);
                out.insert(addr.to_string(), (eth, usd));
            }
        }
    }
    out
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShieldParams {
    from: String,
    chain_id: i64,
    asset: String,  // ERC-20 token address
    amount: String, // token base units
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrivateSendParams {
    from: String, // the EOA owner of the 7702 smart account
    chain_id: i64,
    to: String,    // `0zk…` (private transfer) or `0x…` (unshield)
    asset: String, // ERC-20 token address
    amount: String,
    #[serde(default)]
    memo: String,
    bundler_url: String,
}

/// Fixed message the EOA signs (deterministically) to derive its RAILGUN keys.
/// The signature is the seed handed to `railgun_module.init_from_seed`; it must
/// be kept secret (it is equivalent to the private RAILGUN keys it derives).
const RAILGUN_SEED_MSG: &str =
    "Logos Wallet — RAILGUN private account key derivation (v1). Signing this message \
     derives your private (0zk) keys; it does not authorize any transaction. Keep it secret.";

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
            serde_json::to_string_pretty(&*st.balances.lock().unwrap()).unwrap_or_default(),
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

    /// Combine cached balances (tokens with balance > 0) with Uniswap prices for
    /// the wallet's **Market** view. Per chain: pull held tokens from the cached
    /// aggregate, look up symbol/decimals from `token_list`, ask `uniswap_module`
    /// for token→ETH/USD prices (best-rate, one Multicall3 eth_call), and attach
    /// a `valueUsd` to each holding (and to native ETH). Pricing failures degrade
    /// gracefully to null prices — the holding still shows.
    fn build_market(&mut self, address: &str) -> std::result::Result<Value, String> {
        let cached = {
            let st = self.st()?;
            st.balances.lock().unwrap().get(address).cloned()
        };
        let Some(cached) = cached else {
            return Ok(json!({ "ok": true, "address": address, "chains": [] }));
        };
        let empty = Vec::new();
        let chains = cached.get("chains").and_then(Value::as_array).unwrap_or(&empty);

        let mut chains_out = Vec::new();
        for chain in chains {
            let chain_id = chain.get("chainId").and_then(Value::as_u64).unwrap_or(0);
            if chain_id == 0 {
                continue;
            }

            // symbol/decimals lookup (lowercased address -> (symbol, decimals)).
            let meta = self.token_meta(chain_id as i64);

            // Held tokens (balance > 0) → the set we price.
            let empty_toks = Vec::new();
            let toks = chain.get("tokens").and_then(Value::as_array).unwrap_or(&empty_toks);
            let mut held: Vec<(String, u8)> = Vec::new();
            for t in toks {
                let addr = t.get("address").and_then(Value::as_str).unwrap_or("");
                let bal = t.get("balance").and_then(Value::as_str).unwrap_or("0");
                if addr.is_empty() || parse_u256_str(bal).is_zero() {
                    continue;
                }
                let dec = meta.get(&addr.to_lowercase()).map(|m| m.1).unwrap_or(18);
                held.push((addr.to_string(), dec));
            }

            // Ask Uniswap for prices (held tokens; module adds stablecoins itself).
            let prices = self.uniswap_prices(chain_id as i64, &held);

            // Native ETH item.
            let native_bal = chain.get("native").and_then(Value::as_str).unwrap_or("0");
            let eth_usd = prices.get("ETH").and_then(|p| p.1);
            let mut items = vec![json!({
                "address": "native",
                "symbol": "ETH",
                "decimals": 18,
                "balance": native_bal,
                "eth": 1.0,
                "usd": eth_usd,
                "valueUsd": value_usd(native_bal, 18, eth_usd),
            })];

            for (addr, dec) in &held {
                let (symbol, _) = meta.get(&addr.to_lowercase()).cloned().unwrap_or_else(|| (short_addr(addr), *dec));
                let bal = toks
                    .iter()
                    .find(|t| t.get("address").and_then(Value::as_str) == Some(addr.as_str()))
                    .and_then(|t| t.get("balance").and_then(Value::as_str))
                    .unwrap_or("0");
                let (eth, usd) = prices.get(addr.as_str()).copied().unwrap_or((None, None));
                items.push(json!({
                    "address": addr,
                    "symbol": symbol,
                    "decimals": dec,
                    "balance": bal,
                    "eth": eth,
                    "usd": usd,
                    "valueUsd": value_usd(bal, *dec, usd),
                }));
            }

            chains_out.push(json!({ "chainId": chain_id, "items": items }));
        }
        Ok(json!({ "ok": true, "address": address, "chains": chains_out }))
    }

    /// `token_list.get_tokens(chainId)` → lowercased address -> (symbol, decimals).
    fn token_meta(&mut self, chain_id: i64) -> std::collections::HashMap<String, (String, u8)> {
        let mut map = std::collections::HashMap::new();
        if let Ok(s) = modules().token_list_module.get_tokens(chain_id) {
            if let Ok(v) = serde_json::from_str::<Value>(&s) {
                if let Some(arr) = v.get("tokens").and_then(Value::as_array) {
                    for t in arr {
                        if let Some(addr) = t.get("address").and_then(Value::as_str) {
                            let sym = t.get("symbol").and_then(Value::as_str).unwrap_or("?").to_string();
                            let dec = t.get("decimals").and_then(Value::as_u64).unwrap_or(18) as u8;
                            map.insert(addr.to_lowercase(), (sym, dec));
                        }
                    }
                }
            }
        }
        map
    }

    /// Ask `uniswap_module` for token→(eth, usd) prices, keyed by the address
    /// string we passed (plus an `"ETH"` entry for native). Empty on failure.
    /// Read the cached per-chain prices that `refresh_market` populated. `held` is
    /// unused now — the cache already holds every priced token for the chain.
    fn uniswap_prices(&mut self, chain_id: i64, _held: &[(String, u8)]) -> std::collections::HashMap<String, (Option<f64>, Option<f64>)> {
        self.st()
            .ok()
            .and_then(|st| st.market_prices.lock().unwrap().get(&(chain_id as u64)).cloned())
            .unwrap_or_default()
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

    /// Build (gas from eth_rpc) → sign (keystore) → broadcast (eth_rpc) a pre-built
    /// `(to, value, data)` call at `nonce`. Returns the tx hash. Shared by the
    /// RAILGUN shield path (an `approve` then the shield `TxData`).
    fn sign_and_broadcast_call(
        &self,
        from: &str,
        chain_id: i64,
        to: Address,
        value: U256,
        data: &[u8],
        nonce: u64,
        default_gas: u64,
    ) -> std::result::Result<String, String> {
        let gas_price = parse_u256_str(
            ok_value(modules().eth_rpc_module.gas_price(chain_id).map_err(|e| e.to_string())?)?
                ["result"].as_str().unwrap_or("0x0"),
        );
        let fee = Fee::Eip1559 {
            max_fee_per_gas: gas_price.saturating_mul(U256::from(2)),
            max_priority_fee_per_gas: gas_price,
        };
        let est_tx = json!({
            "from": from,
            "to": format!("{to}"),
            "value": format!("0x{value:x}"),
            "data": format!("0x{}", hex::encode(data)),
        })
        .to_string();
        let gas_limit = match modules().eth_rpc_module.estimate_gas(chain_id, &est_tx) {
            Ok(s) => ok_value(s).ok().and_then(|v| v["result"].as_str().map(parse_hex_u64)).unwrap_or(default_gas),
            Err(_) => default_gas,
        };
        let unsigned = txbuild::unsigned_call_tx(to, value, data, nonce, gas_limit, &fee);
        let signed = ok_value(
            modules().keystore_module.sign_transaction(from, &unsigned.to_string(), chain_id).map_err(|e| e.to_string())?,
        )?;
        let raw = signed["raw"].as_str().ok_or("keystore: no raw tx")?.to_string();
        let bcast = ok_value(
            modules().eth_rpc_module.send_raw_transaction(chain_id, &raw).map_err(|e| e.to_string())?,
        )?;
        Ok(bcast["hash"].as_str().ok_or("broadcast: no hash")?.to_string())
    }

    /// SHIELD coordinator: prepare the shield TxData from `railgun_module`, approve
    /// the RAILGUN smart wallet for the ERC-20, then broadcast each shield call.
    fn do_shield(&mut self, p: &ShieldParams) -> std::result::Result<Value, String> {
        let chain_id = p.chain_id;
        let asset = parse_addr(&p.asset)?;
        let amount = parse_u256_str(&p.amount);

        // 1) Ask railgun for the shield TxData(s) (pure calldata — no proof).
        let prep_params = json!({ "asset": p.asset, "amount": amount.to_string() }).to_string();
        let prep = ok_value(
            modules().railgun_module.prepare_shield(&prep_params).map_err(|e| e.to_string())?,
        )?;
        let txs = prep["txs"].as_array().cloned().unwrap_or_default();
        if txs.is_empty() {
            return Err("railgun: shield produced no transactions".into());
        }
        // The shield call's `to` is the RAILGUN smart wallet — the approve spender.
        let spender = parse_addr(txs[0]["to"].as_str().ok_or("railgun: shield tx missing `to`")?)?;

        let from = p.from.clone();
        let mut nonce = parse_hex_u64(
            ok_value(modules().eth_rpc_module.get_transaction_count(chain_id, &from).map_err(|e| e.to_string())?)?
                ["result"].as_str().unwrap_or("0x0"),
        );

        // 2) approve(spender, amount) on the ERC-20.
        let approve_data = txbuild::erc20_approve_calldata(spender, amount);
        let approve_hash = self.sign_and_broadcast_call(&from, chain_id, asset, U256::ZERO, &approve_data, nonce, 90_000)?;
        nonce += 1;

        // 3) each shield TxData (queued behind the approve via sequential nonces).
        let mut last_hash = approve_hash.clone();
        for tx in &txs {
            let to = parse_addr(tx["to"].as_str().ok_or("railgun: shield tx missing `to`")?)?;
            let value = parse_u256_str(tx["value"].as_str().unwrap_or("0x0"));
            let data = hex::decode(tx["data"].as_str().unwrap_or("").trim_start_matches("0x"))
                .map_err(|e| format!("railgun: bad shield calldata: {e}"))?;
            last_hash = self.sign_and_broadcast_call(&from, chain_id, to, value, &data, nonce, 300_000)?;
            nonce += 1;
        }

        // 4) record + notify (the shield tx is the user-facing one).
        let record = TxRecord {
            hash: last_hash.clone(),
            chain_id: chain_id as u64,
            from: from.clone(),
            to: format!("{spender}"),
            value: amount.to_string(),
            kind: "shield".into(),
            token: Some(p.asset.clone()),
            status: "pending".into(),
            timestamp: now_secs(),
        };
        if let Ok(st) = self.st() {
            st.history.add(&from, record);
        }
        emit_tx_status_changed(&last_hash);
        Ok(json!({ "ok": true, "hash": last_hash, "approveHash": approve_hash }))
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
        let balances = Arc::new(Mutex::new(
            std::fs::read_to_string(dir.join("balances_cache.json"))
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or_default(),
        ));
        let market_prices = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let st = State { cfg, history, dir, watched, balances, market_prices };
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
        let holder = match parse_addr(&address) {
            Ok(a) => a,
            Err(_) => return false,
        };
        // Snapshot what the fan-out needs from State, then drop the borrow: the
        // async callbacks fire later (on the event loop) and can't touch `&self`.
        let (specs, cache, dir) = {
            let st = match self.st() {
                Ok(s) => s,
                Err(_) => return false,
            };
            let chains: Vec<(u64, Option<String>)> =
                st.cfg.config().chains.iter().map(|c| (c.chain_id, c.multicall3_addr())).collect();
            let specs: Vec<(u64, Option<String>, Vec<String>)> = chains
                .into_iter()
                .map(|(id, mc)| (id, mc, st.watched.get(&id).cloned().unwrap_or_default()))
                .collect();
            (specs, Arc::clone(&st.balances), st.dir.clone())
        };

        // One concurrent task per chain (eth_rpc is concurrency:"multi", so they
        // overlap instead of serializing). Fire-and-return; the final completion
        // writes the cache and emits `balances_updated` — contract unchanged.
        let tasks: Vec<GatherTask<Value>> = specs
            .into_iter()
            .map(|(chain_id, mc, tokens)| {
                let holder_hex = address.clone();
                let t: GatherTask<Value> =
                    Box::new(move |done| fetch_chain_async(chain_id, holder, holder_hex, mc, tokens, done));
                t
            })
            .collect();

        let addr = address;
        gather(tasks, move |chains: Vec<Value>| {
            let aggregate = json!({ "address": addr, "chains": chains });
            {
                let mut map = cache.lock().unwrap();
                map.insert(addr.clone(), aggregate);
                let _ = std::fs::write(
                    dir.join("balances_cache.json"),
                    serde_json::to_string_pretty(&*map).unwrap_or_default(),
                );
            }
            emit_balances_updated(&addr);
        });
        true
    }

    fn refresh_market(&mut self, address: String) -> bool {
        let cached = {
            let st = match self.st() {
                Ok(s) => s,
                Err(_) => return false,
            };
            st.balances.lock().unwrap().get(&address).cloned()
        };
        let Some(cached) = cached else {
            // No balances yet — nothing to price; signal done so the UI doesn't wait.
            emit_market_updated(&address);
            return true;
        };
        // Pass 1: pull (chainId, tokens) from the cached aggregate (no &self borrow).
        let chain_data: Vec<(u64, Vec<Value>)> = cached
            .get("chains")
            .and_then(Value::as_array)
            .map(|chains| {
                chains
                    .iter()
                    .filter_map(|c| {
                        let id = c.get("chainId").and_then(Value::as_u64)?;
                        if id == 0 {
                            return None;
                        }
                        Some((id, c.get("tokens").and_then(Value::as_array).cloned().unwrap_or_default()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        // Pass 2: per chain, attach decimals (token_list) to the held tokens → the
        // uniswap price-request JSON.
        let preps: Vec<(u64, String)> = chain_data
            .into_iter()
            .map(|(chain_id, toks)| {
                let meta = self.token_meta(chain_id as i64);
                let held: Vec<Value> = toks
                    .iter()
                    .filter_map(|t| {
                        let addr = t.get("address").and_then(Value::as_str)?;
                        let bal = t.get("balance").and_then(Value::as_str).unwrap_or("0");
                        if addr.is_empty() || parse_u256_str(bal).is_zero() {
                            return None;
                        }
                        let dec = meta.get(&addr.to_lowercase()).map(|m| m.1).unwrap_or(18);
                        Some(json!({ "address": addr, "decimals": dec }))
                    })
                    .collect();
                (chain_id, json!({ "tokens": held }).to_string())
            })
            .collect();
        let cache = {
            let st = match self.st() {
                Ok(s) => s,
                Err(_) => return false,
            };
            Arc::clone(&st.market_prices)
        };

        // One concurrent uniswap.get_prices per chain (uniswap is concurrency:"multi",
        // so they overlap). Fire-and-return; the final completion caches the prices
        // and emits market_updated. get_market then reads the cache.
        let tasks: Vec<GatherTask<(u64, std::collections::HashMap<String, (Option<f64>, Option<f64>)>)>> = preps
            .into_iter()
            .map(|(chain_id, req)| {
                let t: GatherTask<(u64, std::collections::HashMap<String, (Option<f64>, Option<f64>)>)> =
                    Box::new(move |done| {
                        modules().uniswap_module.get_prices_async(chain_id as i64, &req, move |res| {
                            done((chain_id, decode_uniswap_prices(res.ok())));
                        });
                    });
                t
            })
            .collect();

        let addr = address;
        gather(
            tasks,
            move |results: Vec<(u64, std::collections::HashMap<String, (Option<f64>, Option<f64>)>)>| {
                {
                    let mut c = cache.lock().unwrap();
                    for (chain, prices) in results {
                        c.insert(chain, prices);
                    }
                }
                emit_market_updated(&addr);
            },
        );
        true
    }

    fn get_balances(&mut self, address: String) -> String {
        match self.st() {
            Ok(st) => match st.balances.lock().unwrap().get(&address) {
                Some(v) => json!({ "ok": true, "balances": v }).to_string(),
                None => json!({ "ok": true, "balances": { "address": address, "chains": [] } }).to_string(),
            },
            Err(e) => err(e),
        }
    }

    fn get_market(&mut self, address: String) -> String {
        match self.build_market(&address) {
            Ok(v) => v.to_string(),
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

    // ── private (RAILGUN) ──────────────────────────────────────────────────────

    fn init_private(&mut self, address: String, chain_id: i64) -> String {
        // Deterministic EOA signature → seed for railgun's key derivation. The
        // signature never persists here; railgun derives + holds the actual keys.
        let sig = match ok_value(
            match modules().keystore_module.sign_message(&address, RAILGUN_SEED_MSG) {
                Ok(s) => s,
                Err(e) => return err(e),
            },
        ) {
            Ok(v) => v,
            Err(e) => return err(format!("derive seed: {e}")),
        };
        let seed = match sig["signature"].as_str() {
            Some(s) => s.to_string(),
            None => return err("keystore: no signature"),
        };
        let params = json!({ "chainId": chain_id, "seed": seed, "poi": false }).to_string();
        match modules().railgun_module.init_from_seed(&params) {
            Ok(s) => s, // `{ ok, address: "0zk…" }`
            Err(e) => err(e),
        }
    }

    fn get_zk_address(&mut self) -> String {
        match modules().railgun_module.get_zk_address() {
            Ok(s) => s,
            Err(e) => err(e),
        }
    }

    fn sync_private(&mut self) -> String {
        match modules().railgun_module.sync() {
            Ok(s) => s,
            Err(e) => err(e),
        }
    }

    fn get_shielded_balance(&mut self) -> String {
        match modules().railgun_module.get_shielded_balance() {
            Ok(s) => s,
            Err(e) => err(e),
        }
    }

    fn shield(&mut self, send_json: String) -> String {
        let p: ShieldParams = match serde_json::from_str(&send_json) {
            Ok(p) => p,
            Err(e) => return err(e),
        };
        match self.do_shield(&p) {
            Ok(v) => v.to_string(),
            Err(e) => err(e),
        }
    }

    fn private_send(&mut self, send_json: String) -> String {
        let p: PrivateSendParams = match serde_json::from_str(&send_json) {
            Ok(p) => p,
            Err(e) => return err(e),
        };
        // railgun routes 0zk→transfer / 0x→unshield and builds+signs+submits the
        // 4337 UserOp (sender hidden). Returns `{ ok, userOpHash }`.
        let params = json!({
            "to": p.to,
            "asset": p.asset,
            "amount": parse_u256_str(&p.amount).to_string(),
            "memo": p.memo,
            "owner": p.from,
            "bundlerUrl": p.bundler_url,
        })
        .to_string();
        let res = match ok_value(match modules().railgun_module.relayed_send(&params) {
            Ok(s) => s,
            Err(e) => return err(e),
        }) {
            Ok(v) => v,
            Err(e) => return err(e),
        };
        let user_op_hash = res["userOpHash"].as_str().unwrap_or("").to_string();
        // Record as a pending private tx (kind by recipient form).
        let kind = if p.to.starts_with("0zk") { "private_transfer" } else { "unshield" };
        let record = TxRecord {
            hash: user_op_hash.clone(),
            chain_id: p.chain_id as u64,
            from: p.from.clone(),
            to: p.to.clone(),
            value: parse_u256_str(&p.amount).to_string(),
            kind: kind.into(),
            token: Some(p.asset.clone()),
            status: "pending".into(),
            timestamp: now_secs(),
        };
        if let Ok(st) = self.st() {
            st.history.add(&p.from, record);
        }
        emit_tx_status_changed(&user_op_hash);
        json!({ "ok": true, "userOpHash": user_op_hash }).to_string()
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
