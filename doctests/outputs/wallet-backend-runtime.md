# Driving the Wallet Backend (4 modules) Against logoscore

`logos-evm-wallet-backend-module` is the **coordinator** for the Logos
multi-chain EVM wallet. It depends on the eth-rpc, keystore, and token-list
modules and folds in the tx-builder, orchestrating the full send pipeline:
**build → sign → broadcast → record**.

This doc-test loads **all four** modules into a `logoscore` daemon and drives
the coordinator end-to-end against a **local mock JSON-RPC node** (offline,
reproducible): it configures a chain, imports a known account, and sends a
native transfer — watching the backend fetch the nonce/fees, build the tx, sign
it via the keystore, broadcast it via eth-rpc, and record it in local history.

**What you'll build:** All four wallet modules, installed with `lgpm` and driven through a `logoscore` daemon: the coordinator orchestrating a real send across its dependencies.

**What you'll learn:**

- How a coordinator module declares and calls dependency modules over IPC
- How the central chain config is pushed down into the eth-rpc module
- How the send pipeline composes build (alloy) → sign (keystore) → broadcast (eth-rpc) → history

## Prerequisites

- **Nix** with flakes enabled (see [nixos.org](https://nixos.org/download.html)).

- **A Linux or macOS machine** with `python3` available (used to run the local mock JSON-RPC node).

---

## Step 1: Build logoscore and lgpm

### 1.1 Build logoscore

```bash
nix build 'github:logos-co/logos-logoscore-cli#cli' --out-link ./logos
```

### 1.2 Build lgpm

```bash
nix build 'github:logos-co/logos-package-manager#cli' -o lgpm
```

---

## Step 2: Build and install all four modules

The coordinator and its three dependency modules each build to an `.lgx`.
`logoscore`'s `load-module` does not auto-resolve dependencies, so we install
all four and load them explicitly below.

### 2.1 Build the four .lgx packages

```bash
nix build 'github:logos-co/logos-evm-eth-rpc-module#lgx'        -o eth-rpc-lgx
nix build 'github:logos-co/logos-evm-keystore-module#lgx'       -o keystore-lgx
nix build 'github:logos-co/logos-evm-token-list-module#lgx'     -o token-list-lgx
nix build 'github:logos-co/logos-evm-wallet-backend-module#lgx' -o backend-lgx
```

### 2.2 Seed the capability module

```bash
mkdir -p modules
cp -RL ./logos/modules/. ./modules/

```

### 2.3 Install all four .lgx

```bash
for p in eth-rpc-lgx keystore-lgx token-list-lgx backend-lgx; do
  ./lgpm/bin/lgpm --modules-dir ./modules --allow-unsigned install --file "$p"/*.lgx
done

```

### 2.4 Confirm the installs

```bash
./lgpm/bin/lgpm --modules-dir ./modules list
```

---

## Step 3: Start a mock JSON-RPC node

### 3.1 Write the mock node

Answers the methods the send pipeline needs (chainId 1337) with canned values.

```
import http.server, json
RES = {
    "eth_chainId": "0x539",                 # 1337
    "eth_getTransactionCount": "0x0",
    "eth_gasPrice": "0x3b9aca00",
    "eth_estimateGas": "0x5208",
    "eth_sendRawTransaction": "0x" + "ab" * 32,
}
class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        n = int(self.headers.get('content-length', 0))
        req = json.loads(self.rfile.read(n) or b'{}')
        body = json.dumps({"jsonrpc": "2.0", "id": req.get("id", 1),
                           "result": RES.get(req.get("method"), "0x0")}).encode()
        self.send_response(200)
        self.send_header('content-length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *a): pass
http.server.HTTPServer(('127.0.0.1', 8602), H).serve_forever()
```

### 3.2 Start the mock node

```bash
python3 mock_node.py &
```

```bash
sleep 2
```

---

## Step 4: Run the daemon and drive the send pipeline

### 4.1 Write the inputs

```json
[ { "chainId": 1337, "name": "Mock", "rpcUrl": "http://127.0.0.1:8602", "nativeSymbol": "ETH" } ]
```

### 4.2 Write the account import (Foundry's known test mnemonic)

```json
{ "phrase": "test test test test test test test test test test test junk", "accountIndex": 0, "password": "pw" }
```

### 4.3 Write the send request

```json
{
  "from": "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
  "to": "0x70997970C51812dc3A010C7d01b50e0d17dc79C8",
  "chainId": 1337,
  "amount": "0xde0b6b3a7640000"
}
```

### 4.4 Start the daemon

```bash
logoscore -D -m ./modules > logs.txt &
```

```bash
sleep 3
```

### 4.5 Load all four modules

```bash
for m in eth_rpc_module keystore_module token_list_module wallet_backend_module; do
  logoscore load-module "$m"
done
```

### 4.6 Configure the chain (pushed down into eth-rpc)

```bash
logoscore call wallet_backend_module set_chains @chains.json
```

### 4.7 Test the endpoint (backend → eth-rpc → mock node)

```bash
logoscore call wallet_backend_module test_endpoint 1337
```

### 4.8 Import a known account (backend → keystore)

```bash
logoscore call wallet_backend_module import_mnemonic @mnemonic.json main
```

### 4.9 List accounts

```bash
./logos/bin/logoscore call wallet_backend_module list_accounts
```

### 4.10 Unlock the account

```bash
logoscore call wallet_backend_module unlock <address> pw
```

### 4.11 Send a native transfer (build → sign → broadcast → record)

The coordinator fetches the nonce and gas price from eth-rpc, estimates
gas, builds the unsigned EIP-1559 tx (alloy), signs it via the keystore,
broadcasts it via eth-rpc (the mock returns a fixed hash), and records it.

```bash
logoscore call wallet_backend_module send_native @send.json
```

### 4.12 The transaction is in local history

```bash
logoscore call wallet_backend_module get_history <address>
```

### 4.13 Stop the daemon and the mock node

```bash
./logos/bin/logoscore stop
pkill -f mock_node.py 2>/dev/null || true

```

```bash
sleep 2
```

### 4.14 Confirm the daemon has stopped

```bash
./logos/bin/logoscore status || true
```
