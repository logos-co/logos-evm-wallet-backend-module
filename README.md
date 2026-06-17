# logos-evm-wallet-backend-module

The **coordinator** for the Logos multi-chain EVM wallet (Rust, rust-first
cdylib). It depends on
[`eth_rpc_module`](https://github.com/logos-co/logos-evm-eth-rpc-module),
[`keystore_module`](https://github.com/logos-co/logos-evm-keystore-module), and
[`token_list_module`](https://github.com/logos-co/logos-evm-token-list-module),
and it also folds in the **tx-builder** (offline alloy ABI/tx construction).

It owns the central proxy + chain config (and pushes each chain's `{endpoint,
proxy, proxyRequired}` down into `eth_rpc`), fetches multi-chain balances with
**Multicall3** (one `eth_call` per chain, falling back to per-call), orchestrates
sends (build → sign → broadcast → record), and stores this wallet's own
transaction history (the only history available without a proprietary indexer).

## Contract (`WalletBackendModule`)

Config: `set_proxy_config`/`get_proxy_config`, `set_chains`/`get_chains`,
`test_endpoint`. Accounts: `create_account`, `import_mnemonic`, `list_accounts`,
`unlock`/`lock`. Watched tokens: `set_watched_tokens`/`get_watched_tokens`.
Tokens: `get_tokens`, `add_custom_token`. Balances: `refresh_balances`,
`get_balances`. Send: `estimate_fee`, `send_native`, `send_erc20`. History:
`get_history`, `refresh_tx_status`. Events: `balances_updated`,
`tx_status_changed`, `proxy_error`.

## Build & test

```bash
cd rust-lib && cargo test --no-default-features   # tx-builder (incl. Multicall3), config, history
nix build .#install                                # -> result/modules/wallet_backend_module/
```

Building the full module pulls the three dependency modules' published `.lidl`
contracts to generate the typed `modules().<dep>` clients. In the Logos workspace
they resolve via flake `follows`/`--override-input` to local checkouts.
