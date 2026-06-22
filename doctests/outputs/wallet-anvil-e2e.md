# Full wallet end-to-end against a local Anvil chain

This doc-test drives **every wallet UI functionality** end-to-end against a
**real EVM chain** — a local [Anvil](https://book.getfoundry.sh/anvil/) node —
through the `wallet_backend_module` API exactly as the wallet UI calls it (via
`logoscore`). Unlike the mock-node doc-tests, every read and write is a real
signed transaction or a real `eth_call` against a real chain.

It is also the regression guard for the **qt_remote completion re-entrancy
crash**: `refresh_balances` fans balance reads out to the (`concurrency:"multi"`)
`eth_rpc` module via `call_async` and emits `balances_updated` from the gather
completion. That completion is delivered on QtRO's cross-process read stack, and
running the user callback (event emit + client release) inline there used to
crash the backend with a `SIGSEGV` in `onClientRead`
([logos-protocol#7](https://github.com/logos-co/logos-protocol/pull/7) defers it
off the read stack). A mock node returns instantly and never exercises the real
async-reply timing, so only a real chain catches it — which is the whole point
of this test.

The chain is seeded with a tiny set of mock contracts (a Multicall3, three
ERC-20s, and two settable Uniswap-V2 pools) so **Market** prices resolve locally
with no external dependency.

**What you'll build:** The full 6-module wallet stack (`eth_rpc` [multi] + `keystore` + `token_list` + `uniswap` + `railgun` + `wallet_backend`) driven through a `logoscore` daemon against a local Anvil chain with locally-deployed Multicall3 + Uniswap-V2 mock pools.

**What you'll learn:**

- How the wallet backend drives settings, accounts, balances, market, send, history, and private-enable against a real chain
- How `refresh_balances` fans out per-chain async reads and why that path must run the completion callback off QtRO's read stack
- How to stand up a self-contained EVM test chain (Anvil + a Multicall3 + Uniswap-V2 mock pools) with `solc` + `cast`

## Prerequisites

- **Nix** with flakes enabled. Install from [nixos.org](https://nixos.org/download.html), then enable flakes:

```bash
mkdir -p ~/.config/nix
echo 'experimental-features = nix-command flakes' >> ~/.config/nix/nix.conf
```

- **A Linux or macOS machine.** Foundry (`anvil`, `cast`) and `solc` are pulled in via Nix below — nothing else to install.

---

## Step 1: Build logoscore and lgpm

The daemon resolves a `multi` module's deferred reply on the caller's behalf,
so logoscore is built against the logos-protocol under test (the ``
override pins its protocol — and liblogos's — to the commit being validated).

### 1.1 Build logoscore

```bash
nix build 'github:logos-co/logos-logoscore-cli#cli' --out-link ./logos
```

### 1.2 Build lgpm

```bash
nix build 'github:logos-co/logos-package-manager#cli' -o lgpm
```

---

## Step 2: Pull Foundry (anvil + cast) and solc

Anvil is the local chain; `cast` deploys/calls contracts; `solc` compiles the
mock contracts. All three come straight from nixpkgs — no `svm`/network solc
download, so the build is deterministic.

### 2.1 Build Foundry

```bash
nix build nixpkgs#foundry -o foundry
```

### 2.2 Build solc

```bash
nix build nixpkgs#solc -o solc
```

---

## Step 3: Build the wallet modules as .lgx

Six modules go into one `./modules` dir; `load-module wallet_backend_module`
auto-resolves and loads its dependencies. The `` overrides pin the
builder + SDKs to the commits under test — so the protocol carrying the
completion-dispatch fix is the one exercised.

### 3.1 Build eth_rpc_module (.lgx) — the concurrency:"multi" client

```bash
nix build 'github:logos-co/logos-evm-eth-rpc-module#lgx' -o eth-rpc-lgx
```

```bash
ls eth-rpc-lgx/*.lgx
```

### 3.2 Build keystore_module (.lgx)

```bash
nix build 'github:logos-co/logos-evm-keystore-module#lgx' --no-write-lock-file -o keystore-lgx \
  --override-input logos-module-builder 'github:logos-co/logos-module-builder' \
  --override-input logos-module-builder/logos-rust-sdk 'github:logos-co/logos-rust-sdk' \
  --override-input logos-module-builder/logos-qt-sdk 'github:logos-co/logos-qt-sdk' \
  --override-input logos-module-builder/logos-protocol 'github:logos-co/logos-protocol'

```

### 3.3 Build token_list_module (.lgx)

```bash
nix build 'github:logos-co/logos-evm-token-list-module#lgx' --no-write-lock-file -o token-list-lgx \
  --override-input logos-module-builder 'github:logos-co/logos-module-builder' \
  --override-input logos-module-builder/logos-rust-sdk 'github:logos-co/logos-rust-sdk' \
  --override-input logos-module-builder/logos-qt-sdk 'github:logos-co/logos-qt-sdk' \
  --override-input logos-module-builder/logos-protocol 'github:logos-co/logos-protocol'

```

### 3.4 Build uniswap_module (.lgx)

```bash
nix build 'github:logos-co/logos-evm-uniswap-module#lgx' --no-write-lock-file -o uniswap-lgx \
  --override-input logos-module-builder 'github:logos-co/logos-module-builder' \
  --override-input logos-module-builder/logos-rust-sdk 'github:logos-co/logos-rust-sdk' \
  --override-input logos-module-builder/logos-qt-sdk 'github:logos-co/logos-qt-sdk' \
  --override-input logos-module-builder/logos-protocol 'github:logos-co/logos-protocol'

```

### 3.5 Build railgun_module (.lgx) — private transactions

```bash
nix build 'github:logos-co/logos-evm-railgun-module#lgx' --no-write-lock-file -o railgun-lgx \
  --override-input logos-module-builder 'github:logos-co/logos-module-builder' \
  --override-input logos-module-builder/logos-rust-sdk 'github:logos-co/logos-rust-sdk' \
  --override-input logos-module-builder/logos-qt-sdk 'github:logos-co/logos-qt-sdk' \
  --override-input logos-module-builder/logos-protocol 'github:logos-co/logos-protocol'

```

### 3.6 Build wallet_backend_module (.lgx) — the coordinator

```bash
nix build 'github:logos-co/logos-evm-wallet-backend-module#lgx' --no-write-lock-file -o wallet-backend-lgx \
  --override-input logos-module-builder 'github:logos-co/logos-module-builder' \
  --override-input logos-module-builder/logos-rust-sdk 'github:logos-co/logos-rust-sdk' \
  --override-input logos-module-builder/logos-qt-sdk 'github:logos-co/logos-qt-sdk' \
  --override-input logos-module-builder/logos-protocol 'github:logos-co/logos-protocol'

```

```bash
ls wallet-backend-lgx/*.lgx
```

---

## Step 4: Install the modules

### 4.1 Seed the capability module

```bash
mkdir -p modules
cp -RL ./logos/modules/. ./modules/

```

### 4.2 Install all six .lgx with lgpm

```bash
for f in eth-rpc-lgx keystore-lgx token-list-lgx uniswap-lgx railgun-lgx wallet-backend-lgx; do
  ./lgpm/bin/lgpm --modules-dir ./modules --allow-unsigned install --file "$f"/*.lgx
done

```

---

## Step 5: Start a local Anvil chain and seed it

Anvil runs as chain `11155111` (Sepolia's id, so the railgun module's chain
check passes). The mock contracts deploy from a **separate funded account**
(Anvil's account 9) so the wallet account 0 (`0xf39F…2266`) keeps its full
pre-funded 10000 ETH for the assertions below; their addresses are captured
from the deploy receipts. The two Uniswap-V2 pools get directly settable
reserves: **1 TKN = 0.01 ETH** and **1 WETH = 3000 USDC**.

### 5.1 Start Anvil

```bash
anvil --chain-id 11155111 --port 8545 &
```

```bash
sleep 3
```

### 5.2 Write the mock contracts

```
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.19;

// Minimal ERC20 — enough to be priced, sent, and used as WETH/stablecoin.
contract MockERC20 {
    string public name; string public symbol; uint8 public decimals;
    uint256 public totalSupply;
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;
    event Transfer(address indexed from, address indexed to, uint256 value);
    event Approval(address indexed owner, address indexed spender, uint256 value);
    constructor(string memory n, string memory s, uint8 d) { name = n; symbol = s; decimals = d; }
    function mint(address to, uint256 amt) external { balanceOf[to] += amt; totalSupply += amt; emit Transfer(address(0), to, amt); }
    function transfer(address to, uint256 amt) external returns (bool) { balanceOf[msg.sender] -= amt; balanceOf[to] += amt; emit Transfer(msg.sender, to, amt); return true; }
    function approve(address sp, uint256 amt) external returns (bool) { allowance[msg.sender][sp] = amt; emit Approval(msg.sender, sp, amt); return true; }
    function transferFrom(address f, address to, uint256 amt) external returns (bool) { allowance[f][msg.sender] -= amt; balanceOf[f] -= amt; balanceOf[to] += amt; emit Transfer(f, to, amt); return true; }
}
// The two methods the wallet calls on Multicall3.
contract Multicall3 {
    struct Call3 { address target; bool allowFailure; bytes callData; }
    struct Result { bool success; bytes returnData; }
    function aggregate3(Call3[] calldata calls) external payable returns (Result[] memory returnData) {
        uint256 n = calls.length; returnData = new Result[](n);
        for (uint256 i = 0; i < n; i++) {
            (bool ok, bytes memory ret) = calls[i].target.call(calls[i].callData);
            require(ok || calls[i].allowFailure, "Multicall3: call failed");
            returnData[i] = Result(ok, ret);
        }
    }
    function getEthBalance(address a) external view returns (uint256) { return a.balance; }
}
// A Uniswap-V2-compatible pair with directly-settable reserves.
contract MockV2Pair {
    uint112 private r0; uint112 private r1;
    function setReserves(uint112 _r0, uint112 _r1) external { r0 = _r0; r1 = _r1; }
    function getReserves() external view returns (uint112, uint112, uint32) { return (r0, r1, uint32(block.timestamp)); }
}
// CREATE2-deploys pairs with the SAME salt the pricing core uses.
contract MockV2Factory {
    mapping(address => mapping(address => address)) public getPair;
    function createPair(address a, address b) external returns (address pair) {
        (address t0, address t1) = a < b ? (a, b) : (b, a);
        bytes32 salt = keccak256(abi.encodePacked(t0, t1));
        pair = address(new MockV2Pair{salt: salt}());
        getPair[t0][t1] = pair; getPair[t1][t0] = pair;
    }
    function pairInitCodeHash() external pure returns (bytes32) { return keccak256(type(MockV2Pair).creationCode); }
}
```

### 5.3 Compile, deploy, seed reserves + balances, write configs

`solc` compiles the mocks; `cast` deploys them (deterministic addresses),
creates the two pools, sets reserves, and mints 5000 TKN to account 0. The
pool init-code hash is read back from the factory so `uniswap_module`'s
CREATE2 derivation lands on the deployed pairs.

```bash
export PATH="$PWD/foundry/bin:$PATH"
RPC=http://127.0.0.1:8545
# deploy from account 9 so the wallet's account 0 keeps its full 10000 ETH
PK=0x2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6
ACC0=0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266
./solc/bin/solc --combined-json bin,bin-runtime --optimize Mocks.sol > solc.json
dep(){ local bin; bin=$(jq -r ".contracts[\"Mocks.sol:$1\"].bin" solc.json); cast send --rpc-url $RPC --private-key $PK --create "0x${bin}${2:-}" --json | jq -r .contractAddress; }
MC=$(dep Multicall3)
WETH=$(dep MockERC20 "$(cast abi-encode 'c(string,string,uint8)' 'Wrapped Ether' 'WETH' 18 | sed 's/0x//')")
TKN=$(dep MockERC20 "$(cast abi-encode 'c(string,string,uint8)' 'Test Token' 'TKN' 18 | sed 's/0x//')")
USDC=$(dep MockERC20 "$(cast abi-encode 'c(string,string,uint8)' 'USD Coin' 'USDC' 6 | sed 's/0x//')")
FACTORY=$(dep MockV2Factory)
cast send --rpc-url $RPC --private-key $PK $FACTORY "createPair(address,address)" $TKN $WETH >/dev/null
cast send --rpc-url $RPC --private-key $PK $FACTORY "createPair(address,address)" $USDC $WETH >/dev/null
PAIR_TW=$(cast call --rpc-url $RPC $FACTORY "getPair(address,address)(address)" $TKN $WETH)
PAIR_UW=$(cast call --rpc-url $RPC $FACTORY "getPair(address,address)(address)" $USDC $WETH)
# Reserves are stored in sorted-token order (token0 = lower address), so map
# each asset's reserve to r0/r1 by comparing addresses. Targets: 1 TKN =
# 0.01 ETH (reserveTKN:reserveWETH = 1e6:1e4); 1 WETH = 3000 USDC
# (reserveUSDC:reserveWETH = 3e6:1e3).
lc(){ printf '%s' "$1" | tr 'A-Z' 'a-z'; }
lt(){ [ "$(printf '%s\n%s\n' "$(lc "$1")" "$(lc "$2")" | sort | head -1)" = "$(lc "$1")" ]; }
if lt "$TKN" "$WETH"; then TW0=1000000; TW1=10000; else TW0=10000; TW1=1000000; fi
if lt "$USDC" "$WETH"; then UW0=3000000; UW1=1000; else UW0=1000; UW1=3000000; fi
cast send --rpc-url $RPC --private-key $PK $PAIR_TW "setReserves(uint112,uint112)" $TW0 $TW1 >/dev/null
cast send --rpc-url $RPC --private-key $PK $PAIR_UW "setReserves(uint112,uint112)" $UW0 $UW1 >/dev/null
cast send --rpc-url $RPC --private-key $PK $TKN "mint(address,uint256)" $ACC0 5000000000000000000000 >/dev/null
HASH=$(cast call --rpc-url $RPC $FACTORY "pairInitCodeHash()(bytes32)")
printf '[ { "chainId": 11155111, "name": "Anvil", "rpcUrl": "%s", "nativeSymbol": "ETH", "multicall3": "%s" } ]\n' "$RPC" "$MC" > chains.json
printf '{ "phrase": "test test test test test test test test test test test junk", "accountIndex": 0, "password": "pw" }\n' > mnemonic.json
printf '[ "%s" ]\n' "$TKN" > watch.json
printf '{ "chainId": 11155111, "weth": "%s", "stablecoins": ["%s"], "multicall3": "%s", "v2Factory": "%s", "v2InitCodeHash": "%s" }\n' "$WETH" "$USDC" "$MC" "$FACTORY" "$HASH" > uni.json
printf '{ "from": "%s", "to": "0x70997970C51812dc3A010C7d01b50e0d17dc79C8", "chainId": 11155111, "amount": "0xde0b6b3a7640000" }\n' "$ACC0" > send.json
printf '{ "from": "%s", "to": "0x70997970C51812dc3A010C7d01b50e0d17dc79C8", "chainId": 11155111, "tokenAddress": "%s", "amount": "0x3635c9adc5dea00000" }\n' "$ACC0" "$TKN" > senderc20.json
echo "seeded: TKN=$TKN factory=$FACTORY initCodeHash=$HASH"

```

### 5.4 Confirm the chain is live

```bash
./foundry/bin/cast chain-id --rpc-url http://127.0.0.1:8545
```

---

## Step 6: Start the daemon and load the wallet

### 6.1 Start the daemon

```bash
logoscore -D -m ./modules > logs.txt &
```

```bash
sleep 6
```

### 6.2 Load the backend (its deps auto-load)

```bash
logoscore load-module wallet_backend_module
```

---

## Step 7: Settings: point the wallet at the Anvil chain

### 7.1 Set the chain list

```bash
logoscore call wallet_backend_module set_chains @chains.json
```

### 7.2 Test the RPC endpoint (real eth_chainId)

```bash
logoscore call wallet_backend_module test_endpoint 11155111
```

---

## Step 8: Accounts: import the test mnemonic and unlock

### 8.1 Import account 0 (Foundry's known test mnemonic)

```bash
logoscore call wallet_backend_module import_mnemonic @mnemonic.json main
```

### 8.2 Unlock it

```bash
logoscore call wallet_backend_module unlock <address> pw
```

### 8.3 List accounts

```bash
./logos/bin/logoscore call wallet_backend_module list_accounts
```

---

## Step 9: Balances: refresh against the real chain (the fix)

`refresh_balances` fans one async `eth_call` per chain at the (multi)
`eth_rpc` and completes by writing the cache and emitting `balances_updated`.
Account 0 holds Anvil's pre-funded 10000 ETH and the 5000 TKN we minted.

### 9.1 Watch the TKN token

```bash
logoscore call wallet_backend_module set_watched_tokens 11155111 @watch.json
```

### 9.2 Refresh balances (the async fan-out that used to crash)

```bash
logoscore call wallet_backend_module refresh_balances <address>
```

```bash
sleep 5
```

### 9.3 Read the balances (10000 ETH + 5000 TKN, no crash)

```bash
logoscore call wallet_backend_module get_balances <address>
```

### 9.4 Confirm the backend did not crash

The pre-fix backend SIGSEGV'd here; assert the daemon logged no crash.

```bash
sh -c 'grep -c "Module process crashed" logs.txt || true'
```

---

## Step 10: Market: local Uniswap-V2 prices

`uniswap_module.configure` points the pricing core at the local mock factory
(its CREATE2 derivation lands on our deployed pairs). `refresh_market` prices
every held asset; native ETH is priced via the WETH/USDC pool.

### 10.1 Configure uniswap for the local pools

`uniswap_module` is `concurrency:"multi"`, so the reply is a pending sentinel; the configuration still applies.

```bash
logoscore call uniswap_module configure @uni.json
```

### 10.2 Refresh market

```bash
logoscore call wallet_backend_module refresh_market <address>
```

```bash
sleep 5
```

### 10.3 Read the market (ETH priced via the local pool)

```bash
logoscore call wallet_backend_module get_market <address>
```

---

## Step 11: Send: a real native transfer and an ERC-20 transfer

### 11.1 Estimate the native send fee

```bash
logoscore call wallet_backend_module estimate_fee @send.json
```

### 11.2 Send 1 ETH (real signed tx, mined by Anvil)

```bash
logoscore call wallet_backend_module send_native @send.json
```

```bash
sleep 3
```

### 11.3 Send 1000 TKN (real ERC-20 transfer)

```bash
logoscore call wallet_backend_module send_erc20 @senderc20.json
```

```bash
sleep 3
```

---

## Step 12: History: the sends are recorded

### 12.1 Read the transaction history

```bash
logoscore call wallet_backend_module get_history <address>
```

---

## Step 13: Private: enable RAILGUN and derive the 0zk address

`init_private` boots the RAILGUN engine for the chain and derives the private
`0zk…` address from the unlocked account. On-chain shield/transfer/unshield
need deployed RAILGUN contracts + a bundler (out of scope here); this proves
the private-wallet enable path and address derivation.

### 13.1 Enable private mode

```bash
logoscore call wallet_backend_module init_private <address> 11155111
```

### 13.2 Read the 0zk address

```bash
logoscore call wallet_backend_module get_zk_address
```

---

## Step 14: Shut down

### 14.1 Stop the daemon and Anvil

```bash
trap '' TERM
./logos/bin/logoscore stop || true
pkill -f anvil 2>/dev/null || true
true

```

```bash
sleep 2
```

### 14.2 Confirm the daemon has stopped

```bash
./logos/bin/logoscore status || true
```
