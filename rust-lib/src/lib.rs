//! wallet_backend_module — the wallet coordinator (+ folded-in tx builder).
//!
//! Holds the central proxy + chain config, fetches multi-chain balances via
//! Multicall3, orchestrates send (build → sign → broadcast → persist), and stores
//! this wallet's own transaction history. The pure pieces (`txbuild`) are
//! unit-tested with `cargo test --no-default-features`; the cross-module
//! orchestration glue is behind the default `logos_module` feature.

mod config;
mod history;
mod txbuild;

pub use config::*;
pub use history::*;
pub use txbuild::*;

#[cfg(feature = "logos_module")]
mod glue;
