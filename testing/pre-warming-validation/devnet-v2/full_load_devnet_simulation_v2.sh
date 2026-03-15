#!/bin/bash
#===============================================================================
#  @title Full Load Devnet Simulation (v2 - Parallel Transaction Sending)
#  @notice
#  Runs a 2-phase, fresh-datadir benchmark on a local/devnet-style `op-reth` node:
#    - Phase 1: pre-warming OFF  (baseline)
#    - Phase 2: pre-warming ON   (prewarming + prefetch)
#
#  Each phase:
#    1) Starts a fresh dev node (new datadir)
#    2) Generates load by spawning N parallel sender processes
#    3) Waits briefly for blocks to finalize
#    4) Captures key Prometheus metrics (cache hit/miss, block timing, prewarm/prefetch)
#    5) Writes a JSON result file under `.devnet-sim-v2-<timestamp>/`
#
#  Output artifacts (under `.devnet-sim-v2-<timestamp>/`):
#    - node_off.log / node_on.log          : node logs (error-level)
#    - results_off.json / results_on.json  : machine-readable results
#    - summary.txt                         : human-readable summary for sharing
#
#  Notes:
#    - Uses ALWAYS-ON CachedReads metrics to measure execution cache utilization:
#        reth_payloads_cached_reads_hits / reth_payloads_cached_reads_misses
#    - Transaction sending is parallel and uses multiple funded dev accounts.
#    - Sender progress spam is suppressed; per-sender success/failure is printed at end.
#
#  Usage:
#    ./full_load_devnet_simulation_v2.sh --txns 50000 --senders 10 --tx-type eth \
#      --prewarm-workers 12 --prefetch-workers 12
#===============================================================================

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
RETH_DIR="$(cd "$SCRIPT_DIR/../../.." && pwd)"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

# Configuration
TOTAL_TXNS=50000
NUM_SENDERS=10            # Parallel sender processes
BURST_SIZE=100            # Txns per sender per batch
BLOCK_TIME=1
SKIP_BUILD=false
TX_TYPE="eth"             # eth is faster, use for high throughput tests
PREWARM_WORKERS=""
PREFETCH_WORKERS=""

# Parse arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        --txns) TOTAL_TXNS="$2"; shift 2 ;;
        --senders) NUM_SENDERS="$2"; shift 2 ;;
        --burst) BURST_SIZE="$2"; shift 2 ;;
        --tx-type) TX_TYPE="$2"; shift 2 ;;
        --prewarm-workers) PREWARM_WORKERS="$2"; shift 2 ;;
        --prefetch-workers) PREFETCH_WORKERS="$2"; shift 2 ;;
        --skip-build) SKIP_BUILD=true; shift ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

# v2 sender supports eth + (optional) erc20 + mixed loads.
# In erc20/mixed modes, we deploy a minimal ERC20 contract per phase.
if [ "$TX_TYPE" != "eth" ] && [ "$TX_TYPE" != "erc20" ] && [ "$TX_TYPE" != "mixed" ]; then
    echo -e "${RED}Error:${NC} invalid --tx-type '${TX_TYPE}'. Use: eth|erc20|mixed"
    exit 2
fi

# Guard: avoid account reuse nonce-collisions.
# NOTE: DEV_ACCOUNTS is defined below; keep this check in sync with that list.
MAX_FUNDED_SENDERS=10
if [ "$NUM_SENDERS" -gt "$MAX_FUNDED_SENDERS" ]; then
    echo -e "${RED}Error:${NC} --senders (${NUM_SENDERS}) exceeds available funded dev accounts (${MAX_FUNDED_SENDERS})."
    echo -e "  Reduce --senders to <= ${MAX_FUNDED_SENDERS} to avoid nonce collisions and massive failures."
    exit 2
fi

# Resolve worker counts
NUM_CPUS=$(sysctl -n hw.ncpu 2>/dev/null || nproc 2>/dev/null || echo "8")
PREWARM_WORKERS="${PREWARM_WORKERS:-$NUM_CPUS}"
PREFETCH_WORKERS="${PREFETCH_WORKERS:-$NUM_CPUS}"

# Calculate txns per sender
TXNS_PER_SENDER=$((TOTAL_TXNS / NUM_SENDERS))

RESULTS_DIR="$RETH_DIR/.devnet-sim-v2-$(date +%Y%m%d_%H%M%S)"
mkdir -p "$RESULTS_DIR"

echo ""
echo -e "${BOLD}${BLUE}╔══════════════════════════════════════════════════════════════════════════════╗${NC}"
echo -e "${BOLD}${BLUE}║      FULL LOAD DEVNET SIMULATION v2 (Parallel Sending)                      ║${NC}"
echo -e "${BOLD}${BLUE}╚══════════════════════════════════════════════════════════════════════════════╝${NC}"
echo ""
echo -e "  Date:              $(date '+%Y-%m-%d %H:%M:%S')"
echo -e "  Total Transactions: ${TOTAL_TXNS}"
echo -e "  Parallel Senders:  ${NUM_SENDERS}"
echo -e "  Txns per Sender:   ${TXNS_PER_SENDER}"
echo -e "  TX Type:           ${TX_TYPE}"
echo -e "  Prewarm Workers:   ${PREWARM_WORKERS}"
echo -e "  Prefetch Workers:  ${PREFETCH_WORKERS}"
echo -e "  Results Dir:       ${RESULTS_DIR}"
echo ""

# Dev accounts (Hardhat/Anvil standard)
# Using multiple accounts for parallel sending
DEV_ACCOUNTS=(
    "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266:0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
    "0x70997970C51812dc3A010C7d01b50e0d17dc79C8:0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d"
    "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC:0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a"
    "0x90F79bf6EB2c4f870365E785982E1f101E93b906:0x7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6"
    "0x15d34AAf54267DB7D7c367839AAf71A00a2C6A65:0x47e179ec197488593b187f80a00eb0da91f1b9d0b13f8733639f19c30a34926a"
    "0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc:0x8b3a350cf5c34c9194ca85829a2df0ec3153be0318b5e2d3348e872092edffba"
    "0x976EA74026E726554dB657fA54763abd0C3a0aa9:0x92db14e403b83dfe3df233f83dfa3a0d7096f21ca9b0d6d6b8d88b2b4ec1564e"
    "0x14dC79964da2C08b23698B3D3cc7Ca32193d9955:0x4bbbf85ce3377467afe5d46f804f221813b2bb87f24d81f60f1fcdbf7cbf4356"
    "0x23618e81E3f5cdF7f54C3d65f7FBc0aBf5B21E8f:0xdbda1821b80551c9d65939329250298aa3472ba22feea921c0cf5d620ea67b97"
    "0xa0Ee7A142d267C1f36714E4a8F75612F20a79720:0x2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6"
)

# Re-check now that DEV_ACCOUNTS is defined (defensive).
if [ "$NUM_SENDERS" -gt "${#DEV_ACCOUNTS[@]}" ]; then
    echo -e "${RED}Error:${NC} --senders (${NUM_SENDERS}) exceeds available funded dev accounts (${#DEV_ACCOUNTS[@]})."
    echo -e "  Reduce --senders to <= ${#DEV_ACCOUNTS[@]} to avoid nonce collisions and massive failures."
    exit 2
fi

#-------------------------------------------------------------------------------
# Helper Functions
#-------------------------------------------------------------------------------
cleanup() {
    echo -e "${CYAN}Cleaning up...${NC}"
    pkill -9 op-reth 2>/dev/null || true
    # Kill any background Python processes
    pkill -f "parallel_sender" 2>/dev/null || true
    sleep 2
}

wait_for_node() {
    echo -e "  ${CYAN}Waiting for node...${NC}"
    for i in {1..30}; do
        if curl -s http://localhost:9001/metrics > /dev/null 2>&1; then
            echo -e "  ${GREEN}✓ Node ready${NC}"
            return 0
        fi
        sleep 1
    done
    echo -e "  ${RED}✗ Node failed${NC}"
    return 1
}

get_metric() {
    local val=$(curl -s "http://localhost:9001/metrics" 2>/dev/null | grep "^$1 " | awk '{print $2}' | head -1)
    echo "${val:-0}"
}

#-------------------------------------------------------------------------------
# ERC20 deployment helpers (used when --tx-type erc20|mixed)
#-------------------------------------------------------------------------------
# Minimal ERC20 bytecode (same as used by full_load_devnet_simulation.sh)
ERC20_BYTECODE="0x608060405234801561001057600080fd5b506b033b2e3c9fd0803ce80000006000803373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff168152602001908152602001600020819055506b033b2e3c9fd0803ce80000006002819055506105d8806100826000396000f3fe608060405234801561001057600080fd5b50600436106100625760003560e01c8063095ea7b31461006757806318160ddd146100cb57806323b872dd146100e957806370a082311461014d578063a9059cbb146101a5578063dd62ed3e14610209575b600080fd5b6100b36004803603604081101561007d57600080fd5b81019080803573ffffffffffffffffffffffffffffffffffffffff16906020019092919080359060200190929190505050610281565b60405180821515815260200191505060405180910390f35b6100d3610373565b6040518082815260200191505060405180910390f35b6101356004803603606081101561010f57600080fd5b81019080803573ffffffffffffffffffffffffffffffffffffffff169060200190929190803573ffffffffffffffffffffffffffffffffffffffff1690602001909291908035906020019092919050505061037d565b60405180821515815260200191505060405180910390f35b61018f6004803603602081101561016357600080fd5b81019080803573ffffffffffffffffffffffffffffffffffffffff169060200190929190505050610540565b6040518082815260200191505060405180910390f35b6101f1600480360360408110156101bb57600080fd5b81019080803573ffffffffffffffffffffffffffffffffffffffff16906020019092919080359060200190929190505050610588565b60405180821515815260200191505060405180910390f35b61026b6004803603604081101561021f57600080fd5b81019080803573ffffffffffffffffffffffffffffffffffffffff169060200190929190803573ffffffffffffffffffffffffffffffffffffffff16906020019092919050505061059c565b6040518082815260200191505060405180910390f35b600081600160003373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff16815260200190815260200160002060008573ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff168152602001908152602001600020819055508273ffffffffffffffffffffffffffffffffffffffff163373ffffffffffffffffffffffffffffffffffffffff167f8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925846040518082815260200191505060405180910390a36001905092915050565b6000600254905090565b60008060008573ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff16815260200190815260200160002060003373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff168152602001908152602001600020548211156103ca57600080fd5b600160008573ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff16815260200190815260200160002060003373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020016000205482111561045357600080fd5b816000808673ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff16815260200190815260200160002060008282540392505081905550816000808573ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020016000206000828254019250508190555081600160008673ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff16815260200190815260200160002060003373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff168152602001908152602001600020600082825403925050819055506001905092915050565b60008060008373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff168152602001908152602001600020549050919050565b600061059533848461037d565b9050919050565b6000600160008473ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff16815260200190815260200160002060008373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff16815260200190815260200160002054905092915050565b"

deploy_erc20_contract() {
    # Use account[0] as deployer
    local DEPLOYER_INFO="${DEV_ACCOUNTS[0]}"
    local DEPLOYER_ADDR=$(echo "$DEPLOYER_INFO" | cut -d: -f1)
    local DEPLOYER_PK=$(echo "$DEPLOYER_INFO" | cut -d: -f2)

    python3 << PYEOF
import json, subprocess, time, sys
from eth_account import Account
from web3 import Web3

SENDER = "${DEPLOYER_ADDR}"
PK = "${DEPLOYER_PK}"
BYTECODE = "${ERC20_BYTECODE}"

w3 = Web3()
acct = Account.from_key(PK)

def log_err(msg):
    sys.stderr.write(f"[ERC20 Deploy] {msg}\n")
    sys.stderr.flush()

def rpc(method, params):
    payload = {"jsonrpc":"2.0","method":method,"params":params,"id":1}
    try:
        out = subprocess.run(
            ["curl","-s","-X","POST","http://localhost:8545","-H","Content-Type: application/json","-d",json.dumps(payload)],
            capture_output=True,text=True,timeout=30
        ).stdout
    except Exception as e:
        log_err(f"RPC call failed: {e}")
        return {"error":{"message":str(e)}}
    try:
        return json.loads(out)
    except Exception:
        log_err(f"RPC invalid JSON: {out[:200]}")
        return {"error":{"message":"invalid json"},"raw":out}

nonce_resp = rpc("eth_getTransactionCount", [SENDER, "pending"])
if "error" in nonce_resp:
    log_err(f"Failed to get nonce: {nonce_resp}")
    print("")
    raise SystemExit(1)
nonce = int(nonce_resp.get("result","0x0"), 16)

chain_resp = rpc("eth_chainId", [])
if "error" in chain_resp:
    log_err(f"Failed to get chainId: {chain_resp}")
chain_id = int(chain_resp.get("result","0x0"), 16) or 1337

log_err(f"Deploying with nonce={nonce}, chainId={chain_id}")

# Retry with fresh nonce on each attempt
tx_hash = None
for attempt in range(10):
    # Refresh nonce on each attempt
    if attempt > 0:
        nonce = int(rpc("eth_getTransactionCount", [SENDER, "pending"]).get("result","0x0"), 16)

    tx = {
        "nonce": nonce,
        "gasPrice": 100_000_000_000,
        "gas": 2_000_000,
        "to": None,
        "value": 0,
        "data": BYTECODE,
        "chainId": chain_id,
    }
    signed = acct.sign_transaction(tx)
    raw = signed.raw_transaction.hex()
    if not raw.startswith('0x'):
        raw = '0x' + raw
    resp = rpc("eth_sendRawTransaction", [raw])
    tx_hash = resp.get("result")
    if tx_hash:
        log_err(f"Deploy tx sent (attempt {attempt+1}, nonce={nonce}): {tx_hash}")
        break
    err = resp.get("error", {}).get("message", "")
    log_err(f"Attempt {attempt+1} failed (nonce={nonce}): {err}")
    if "nonce" not in err.lower() and "underpriced" not in err.lower():
        break
    time.sleep(0.5)

if not tx_hash:
    log_err(f"All attempts failed")
    print("")
    raise SystemExit(0)


for i in range(60):
    time.sleep(1)
    receipt = rpc("eth_getTransactionReceipt", [tx_hash]).get("result")
    if receipt and receipt.get("contractAddress"):
        log_err(f"Contract deployed at: {receipt['contractAddress']}")
        print(receipt["contractAddress"])
        raise SystemExit(0)
    if i > 0 and i % 10 == 0:
        log_err(f"Waiting for receipt... {i}s")

log_err("Timeout waiting for contract deployment receipt")
print("")
PYEOF
}

#-------------------------------------------------------------------------------
# Parallel Transaction Sender
#-------------------------------------------------------------------------------
send_transactions_parallel() {
    local TOTAL=$1
    local SENDERS=$2
    local TX_TYPE=$3
    local CONTRACT_ADDR="${4:-}"
    local FAIL_LOG="${5:-$RESULTS_DIR/failed_txns.log}"

    # For mixed/erc20 modes, ensure contract is available
    if [ "$TX_TYPE" = "erc20" ] || [ "$TX_TYPE" = "mixed" ]; then
        if [ -z "$CONTRACT_ADDR" ]; then
            echo -e "  ${CYAN}Deploying ERC20 contract for this phase...${NC}"
            CONTRACT_ADDR=$(deploy_erc20_contract)
            if [ -z "$CONTRACT_ADDR" ]; then
                echo -e "  ${RED}✗ ERC20 deployment failed${NC}"
                exit 1
            fi
            echo -e "  ${GREEN}✓ ERC20 deployed at: ${CONTRACT_ADDR}${NC}"
        fi
    fi

    local TXNS_PER_SENDER=$((TOTAL / SENDERS))
    local PIDS=()
    local SENDER_OUT_FILES=()
    local START_TIME=$(date +%s)

    echo -e "  ${CYAN}Starting ${SENDERS} parallel senders (${TXNS_PER_SENDER} txns each)...${NC}"

    # Launch parallel senders
    for i in $(seq 0 $((SENDERS - 1))); do
        local ACCOUNT_INFO="${DEV_ACCOUNTS[$i]}"
        local SENDER_ADDR=$(echo "$ACCOUNT_INFO" | cut -d: -f1)
        local PRIVATE_KEY=$(echo "$ACCOUNT_INFO" | cut -d: -f2)

        local OUT_FILE="$RESULTS_DIR/sender_${i}.out"
        rm -f "$OUT_FILE"
        SENDER_OUT_FILES+=("$OUT_FILE")

        python3 << PYEOF >"$OUT_FILE" 2>&1 &
import subprocess, json, random, time
from eth_account import Account
from web3 import Web3

SENDER_ID = $i
TARGET_TXNS = $TXNS_PER_SENDER
MODE = "${TX_TYPE}"
CONTRACT = "${CONTRACT_ADDR}"

SENDER = "${SENDER_ADDR}"
PK = "${PRIVATE_KEY}"
acct = Account.from_key(PK)

RECIPIENTS = [Account.create().address for _ in range(50)]

def mk_transfer(to_addr, amount_wei):
    to_b = bytes.fromhex(to_addr[2:])
    return '0xa9059cbb' + ('00'*12 + to_b.hex()) + amount_wei.to_bytes(32,'big').hex()

def mk_approve(spender, amount_wei):
    sp_b = bytes.fromhex(spender[2:])
    return '0x095ea7b3' + ('00'*12 + sp_b.hex()) + amount_wei.to_bytes(32,'big').hex()

def mk_transfer_from(src, dst, amount_wei):
    s_b = bytes.fromhex(src[2:])
    d_b = bytes.fromhex(dst[2:])
    return '0x23b872dd' + ('00'*12 + s_b.hex()) + ('00'*12 + d_b.hex()) + amount_wei.to_bytes(32,'big').hex()

def rpc(method, params):
    payload = {"jsonrpc":"2.0","method":method,"params":params,"id":1}
    out = subprocess.run(
        ["curl","-s","-X","POST","http://localhost:8545","-H","Content-Type: application/json","-d",json.dumps(payload)],
        capture_output=True,text=True,timeout=20
    ).stdout
    try:
        return json.loads(out)
    except Exception:
        return {"error":{"message":"invalid json"},"raw":out}

FAIL_LOG_FILE = "${FAIL_LOG:-/dev/null}"

def send_raw(raw, tx_info=""):
    resp = rpc("eth_sendRawTransaction", [raw])
    ok = ("result" in resp) and ("error" not in resp)
    if not ok:
        err_msg = resp.get("error", {}).get("message", "unknown error")
        with open(FAIL_LOG_FILE, "a") as fl:
            fl.write(f"SENDER_{SENDER_ID}|{tx_info}|{err_msg}\n")
    return ok

# Wait for node to be ready
for retry in range(30):
    try:
        nonce_resp = rpc("eth_getTransactionCount", [SENDER, "pending"])
        if "result" in nonce_resp:
            break
    except Exception:
        pass
    import time
    time.sleep(1)
else:
    print(f"SENDER_{SENDER_ID}:SUCCESS:0:FAILED:0:ETH:0:ERC20:0")
    raise SystemExit(1)

nonce = int(nonce_resp.get("result","0x0"), 16)
chain_id = int(rpc("eth_chainId", []).get("result","0x0"), 16) or 1337

success = 0
failed = 0
eth_sent = 0
erc20_sent = 0

# If we need ERC20, reserve 1 tx for approve (so we don't exceed the target).
needs_erc20 = MODE in ("erc20","mixed")
approve_budget = 1 if needs_erc20 else 0

# For sizes 0/1, avoid negative loop count.
main_budget = max(TARGET_TXNS - approve_budget, 0)

if needs_erc20 and approve_budget:
    try:
        spender = Web3.to_checksum_address(SENDER)
        data = mk_approve(spender, 10**30)
        tx = {
            "nonce": nonce,
            "gasPrice": 100_000_000_000,
            "gas": 120000,
            "to": Web3.to_checksum_address(CONTRACT),
            "value": 0,
            "data": data,
            "chainId": chain_id,
        }
        signed = acct.sign_transaction(tx)
        raw = signed.raw_transaction.hex()
        if not raw.startswith('0x'):
            raw = '0x' + raw
        if send_raw(raw, f"approve|nonce={nonce}"):
            success += 1
            erc20_sent += 1
            nonce += 1
        else:
            failed += 1
            nonce = int(rpc("eth_getTransactionCount", [SENDER, "pending"]).get("result","0x0"), 16)
    except Exception as e:
        failed += 1
        with open(FAIL_LOG_FILE, "a") as fl:
            fl.write(f"SENDER_{SENDER_ID}|approve|exception|{str(e)}\n")
        nonce = int(rpc("eth_getTransactionCount", [SENDER, "pending"]).get("result","0x0"), 16)

for _ in range(main_budget):
    # Small random delay to simulate real-world tx arrival (0-20ms)
    time.sleep(random.uniform(0, 0.02))

    kind = "eth"
    if MODE == "erc20":
        kind = "erc20_transfer"
    elif MODE == "mixed":
        r = random.random()
        if r < 0.50:
            kind = "eth"
        elif r < 0.75:
            kind = "erc20_transfer"
        else:
            kind = "erc20_transferFrom"

    if kind == "eth":
        to_addr = Web3.to_checksum_address(random.choice(RECIPIENTS))
        tx = {
            "nonce": nonce,
            "gasPrice": 100_000_000_000,
            "gas": 21000,
            "to": to_addr,
            "value": 1000,
            "chainId": chain_id,
        }
    elif kind == "erc20_transfer":
        to_addr = Web3.to_checksum_address(random.choice(RECIPIENTS))
        data = mk_transfer(to_addr, 1)
        tx = {
            "nonce": nonce,
            "gasPrice": 100_000_000_000,
            "gas": 120000,
            "to": Web3.to_checksum_address(CONTRACT),
            "value": 0,
            "data": data,
            "chainId": chain_id,
        }
    else:
        dst = Web3.to_checksum_address(random.choice(RECIPIENTS))
        data = mk_transfer_from(Web3.to_checksum_address(SENDER), dst, 1)
        tx = {
            "nonce": nonce,
            "gasPrice": 100_000_000_000,
            "gas": 150000,
            "to": Web3.to_checksum_address(CONTRACT),
            "value": 0,
            "data": data,
            "chainId": chain_id,
        }

    # Retry logic for nonce conflicts
    max_retries = 3
    for attempt in range(max_retries):
        try:
            signed = acct.sign_transaction(tx)
            raw = signed.raw_transaction.hex()
            if not raw.startswith('0x'):
                raw = '0x' + raw

            resp = rpc("eth_sendRawTransaction", [raw])
            if ("result" in resp) and ("error" not in resp):
                success += 1
                if kind == "eth":
                    eth_sent += 1
                else:
                    erc20_sent += 1
                nonce += 1  # Increment for next tx
                break
            else:
                err_msg = resp.get("error", {}).get("message", "unknown error")
                if "nonce" in err_msg.lower() or "underpriced" in err_msg.lower():
                    # Refresh nonce and retry
                    nonce = int(rpc("eth_getTransactionCount", [SENDER, "pending"]).get("result","0x0"), 16)
                    tx["nonce"] = nonce
                    if attempt == max_retries - 1:
                        with open(FAIL_LOG_FILE, "a") as fl:
                            fl.write(f"SENDER_{SENDER_ID}|{kind}|nonce={nonce}|{err_msg}\n")
                        failed += 1
                else:
                    with open(FAIL_LOG_FILE, "a") as fl:
                        fl.write(f"SENDER_{SENDER_ID}|{kind}|nonce={nonce}|{err_msg}\n")
                    failed += 1
                    nonce = int(rpc("eth_getTransactionCount", [SENDER, "pending"]).get("result","0x0"), 16)
                    break
        except Exception as e:
            if attempt == max_retries - 1:
                failed += 1
                with open(FAIL_LOG_FILE, "a") as fl:
                    fl.write(f"SENDER_{SENDER_ID}|{kind}|exception|{str(e)}\n")
            nonce = int(rpc("eth_getTransactionCount", [SENDER, "pending"]).get("result","0x0"), 16)
            tx["nonce"] = nonce

print(f"SENDER_{SENDER_ID}:SUCCESS:{success}:FAILED:{failed}:ETH:{eth_sent}:ERC20:{erc20_sent}")
PYEOF
        PIDS+=($!)
    done

    echo -e "  ${CYAN}Waiting for senders to complete...${NC}"
    for pid in "${PIDS[@]}"; do
        wait $pid
    done

    # Summarize results
    local TOTAL_SUCCESS=0
    local TOTAL_FAILED=0
    local TOTAL_ETH=0
    local TOTAL_ERC20=0

    echo -e "  ${CYAN}Sender results:${NC}"
    for out in "${SENDER_OUT_FILES[@]}"; do
        if [ ! -f "$out" ]; then
            continue
        fi
        local line
        line=$(grep -E "^SENDER_[0-9]+:SUCCESS:[0-9]+:FAILED:[0-9]+:ETH:[0-9]+:ERC20:[0-9]+$" "$out" | tail -1 || true)
        if [ -z "$line" ]; then
            echo -e "    ${YELLOW}⚠ No summary found in $out${NC}"
            continue
        fi
        local sid succ fail eth erc
        sid=$(echo "$line" | cut -d: -f1)
        succ=$(echo "$line" | cut -d: -f3)
        fail=$(echo "$line" | cut -d: -f5)
        eth=$(echo "$line" | cut -d: -f7)
        erc=$(echo "$line" | cut -d: -f9)
        TOTAL_SUCCESS=$((TOTAL_SUCCESS + succ))
        TOTAL_FAILED=$((TOTAL_FAILED + fail))
        TOTAL_ETH=$((TOTAL_ETH + eth))
        TOTAL_ERC20=$((TOTAL_ERC20 + erc))
        echo -e "    ${sid}: success=${succ} failed=${fail} (eth=${eth}, erc20=${erc})"
    done

    local END_TIME
    END_TIME=$(date +%s)
    local DURATION=$((END_TIME - START_TIME))
    local TPS=0
    if [ $DURATION -gt 0 ]; then
        TPS=$(python3 -c "print(round(${TOTAL_SUCCESS:-0} / ${DURATION:-1}, 1))" 2>/dev/null || echo "0")
    fi

    LAST_SEND_DURATION_SECS=$DURATION
    LAST_SEND_SUCCESS=$TOTAL_SUCCESS
    LAST_SEND_FAILED=$TOTAL_FAILED
    LAST_SEND_TPS=$TPS
    LAST_SEND_ETH=$TOTAL_ETH
    LAST_SEND_ERC20=$TOTAL_ERC20

    echo -e "  ${GREEN}✓ Completed: success=${TOTAL_SUCCESS}/${TOTAL} failed=${TOTAL_FAILED} in ${DURATION}s (${TPS} TPS)${NC}"
}

#-------------------------------------------------------------------------------
# Metrics Capture
#-------------------------------------------------------------------------------
capture_metrics() {
    local OUTPUT_FILE=$1
    local PREWARM_MODE=$2
    local MODE_TX_TYPE=$3

    echo -e "  ${CYAN}Capturing metrics...${NC}"

    # Use ALWAYS-ON CachedReads metrics
    local HITS=$(get_metric "reth_payloads_cached_reads_hits")
    local MISSES=$(get_metric "reth_payloads_cached_reads_misses")
    local SIMS=$(get_metric "reth_txpool_pre_warming_simulations_completed")
    local PREFETCH_OPS=$(get_metric "reth_txpool_pre_warming_prefetch_operations")
    local PREFETCH_ACCTS=$(get_metric "reth_txpool_pre_warming_prefetch_accounts")

    local BUILD_EXEC_SUM=$(get_metric "reth_block_timing_build_exec_mempool_transactions_sum")
    local BUILD_EXEC_COUNT=$(get_metric "reth_block_timing_build_exec_mempool_transactions_count")
    local STATE_ROOT_SUM=$(get_metric "reth_block_timing_build_calc_state_root_sum")
    local STATE_ROOT_COUNT=$(get_metric "reth_block_timing_build_calc_state_root_count")

    local BLOCK=$(curl -s "http://localhost:8545" -X POST -H "Content-Type: application/json" \
        -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' 2>/dev/null | \
        python3 -c "import sys,json; print(int(json.load(sys.stdin).get('result','0x0'),16))" 2>/dev/null || echo "0")

    local TOTAL=$((HITS + MISSES))
    local HIT_RATE=0
    if [ $TOTAL -gt 0 ]; then
        HIT_RATE=$(python3 -c "print(round($HITS * 100 / $TOTAL, 1))" 2>/dev/null || echo "0")
    fi

    local BLOCK_EXEC_MS=0
    local STATE_ROOT_MS=0
    if [ "${BUILD_EXEC_COUNT:-0}" != "0" ] && [ "${BUILD_EXEC_COUNT:-0}" != "" ]; then
        BLOCK_EXEC_MS=$(python3 -c "print(round(float('${BUILD_EXEC_SUM:-0}') / float('${BUILD_EXEC_COUNT:-1}') * 1000, 4))" 2>/dev/null || echo "0")
    fi
    if [ "${STATE_ROOT_COUNT:-0}" != "0" ] && [ "${STATE_ROOT_COUNT:-0}" != "" ]; then
        STATE_ROOT_MS=$(python3 -c "print(round(float('${STATE_ROOT_SUM:-0}') / float('${STATE_ROOT_COUNT:-1}') * 1000, 4))" 2>/dev/null || echo "0")
    fi

    # Tx breakdown from sender aggregation (works for eth/erc20/mixed)
    local ETH_TXNS=${LAST_SEND_ETH:-0}
    local ERC20_TXNS=${LAST_SEND_ERC20:-0}

    # Ensure all values have defaults
    HITS=${HITS:-0}
    MISSES=${MISSES:-0}
    HIT_RATE=${HIT_RATE:-0}
    BLOCK_EXEC_MS=${BLOCK_EXEC_MS:-0}
    STATE_ROOT_MS=${STATE_ROOT_MS:-0}
    SIMS=${SIMS:-0}
    PREFETCH_OPS=${PREFETCH_OPS:-0}
    PREFETCH_ACCTS=${PREFETCH_ACCTS:-0}
    BLOCK=${BLOCK:-0}

    python3 << PYEOF || echo "Warning: Failed to write metrics JSON"
import json
from datetime import datetime

results = {
    "timestamp": datetime.now().isoformat(),
    "prewarm_mode": "${PREWARM_MODE}",
    "tx_type": "${MODE_TX_TYPE}",
    "target_total_txns": ${TOTAL_TXNS:-0},
    "sent_success": ${LAST_SEND_SUCCESS:-0},
    "sent_failed": ${LAST_SEND_FAILED:-0},
    "send_duration_secs": ${LAST_SEND_DURATION_SECS:-0},
    "tps": ${LAST_SEND_TPS:-0},
    "sent_eth": ${ETH_TXNS:-0},
    "sent_erc20": ${ERC20_TXNS:-0},
    "parallel_senders": ${NUM_SENDERS:-1},
    "blocks": ${BLOCK:-0},
    "cache_hits": ${HITS:-0},
    "cache_misses": ${MISSES:-0},
    "cache_hit_rate": ${HIT_RATE:-0},
    "block_execution_ms": ${BLOCK_EXEC_MS:-0},
    "state_root_ms": ${STATE_ROOT_MS:-0},
    "simulations": ${SIMS:-0},
    "prefetch_ops": ${PREFETCH_OPS:-0},
    "prefetch_accounts": ${PREFETCH_ACCTS:-0}
}

with open("${OUTPUT_FILE}", "w") as f:
    json.dump(results, f, indent=2)
PYEOF

    echo -e "    Cache Hits: ${HITS} | Misses: ${MISSES} | Rate: ${HIT_RATE}%"
}

#-------------------------------------------------------------------------------
# Build
#-------------------------------------------------------------------------------
if [ "$SKIP_BUILD" = false ]; then
    echo -e "${CYAN}Building op-reth with pre-warming...${NC}"
    cd "$RETH_DIR"
    cargo build --release --package op-reth --features pre-warming 2>&1 | tail -3
    echo -e "${GREEN}✓ Build complete${NC}"
fi

trap cleanup EXIT

#===============================================================================
# PHASE 1: Pre-warming OFF
#===============================================================================
echo ""
echo -e "${BOLD}${BLUE}══════════════════════════════════════════════════════════════════════════════${NC}"
echo -e "${BOLD}  PHASE 1: Pre-warming DISABLED${NC}"
echo -e "${BOLD}${BLUE}══════════════════════════════════════════════════════════════════════════════${NC}"
echo ""

DATA_DIR_OFF="$RESULTS_DIR/data-off"
rm -rf "$DATA_DIR_OFF"

echo -e "  ${CYAN}Starting node (pre-warming OFF)...${NC}"
"$RETH_DIR/target/release/op-reth" node \
    --datadir "$DATA_DIR_OFF" \
    --dev --dev.block-time ${BLOCK_TIME}s \
    --http --http.api eth,debug,net,web3,txpool \
    --metrics 0.0.0.0:9001 \
    --txpool.pre-warming false \
    --log.stdout.filter error > "$RESULTS_DIR/node_off.log" 2>&1 &

NODE_PID=$!
echo "  Node PID: $NODE_PID"

wait_for_node || exit 1
sleep 3

# Send transactions in parallel
PHASE1_START=$(date +%s)
send_transactions_parallel $TOTAL_TXNS $NUM_SENDERS "$TX_TYPE" "" "$RESULTS_DIR/failed_txns_off.log"
PHASE1_END=$(date +%s)

# Wait for processing
echo -e "  ${CYAN}Waiting for blocks to finalize...${NC}"
sleep 15

capture_metrics "$RESULTS_DIR/results_off.json" "OFF" "$TX_TYPE"

PHASE1_DURATION=$((PHASE1_END - PHASE1_START))
echo -e "  ${GREEN}Phase 1 complete in ${PHASE1_DURATION}s${NC}"

cleanup

#===============================================================================
# PHASE 2: Pre-warming ON
#===============================================================================
echo ""
echo -e "${BOLD}${BLUE}══════════════════════════════════════════════════════════════════════════════${NC}"
echo -e "${BOLD}  PHASE 2: Pre-warming ENABLED${NC}"
echo -e "${BOLD}${BLUE}══════════════════════════════════════════════════════════════════════════════${NC}"
echo ""

DATA_DIR_ON="$RESULTS_DIR/data-on"
rm -rf "$DATA_DIR_ON"

echo -e "  ${CYAN}Starting node (pre-warming ON)...${NC}"
"$RETH_DIR/target/release/op-reth" node \
    --datadir "$DATA_DIR_ON" \
    --dev --dev.block-time ${BLOCK_TIME}s \
    --http --http.api eth,debug,net,web3,txpool \
    --metrics 0.0.0.0:9001 \
    --txpool.pre-warming true \
    --txpool.pre-warming-workers $PREWARM_WORKERS \
    --txpool.pre-fetch-workers $PREFETCH_WORKERS \
    --log.stdout.filter error > "$RESULTS_DIR/node_on.log" 2>&1 &

NODE_PID=$!
echo "  Node PID: $NODE_PID"

wait_for_node || exit 1
sleep 3

# Send transactions in parallel
PHASE2_START=$(date +%s)
send_transactions_parallel $TOTAL_TXNS $NUM_SENDERS "$TX_TYPE" "" "$RESULTS_DIR/failed_txns_on.log"
PHASE2_END=$(date +%s)

# Wait for processing
echo -e "  ${CYAN}Waiting for blocks to finalize...${NC}"
sleep 15

capture_metrics "$RESULTS_DIR/results_on.json" "ON" "$TX_TYPE"

PHASE2_DURATION=$((PHASE2_END - PHASE2_START))
echo -e "  ${GREEN}Phase 2 complete in ${PHASE2_DURATION}s${NC}"

#===============================================================================
# COMPARISON REPORT
#===============================================================================
echo ""
echo -e "${BOLD}${BLUE}══════════════════════════════════════════════════════════════════════════════${NC}"
echo -e "${BOLD}  COMPARISON REPORT${NC}"
echo -e "${BOLD}${BLUE}══════════════════════════════════════════════════════════════════════════════${NC}"
echo ""

python3 << PYEOF
import json

with open("$RESULTS_DIR/results_off.json") as f:
    off = json.load(f)
with open("$RESULTS_DIR/results_on.json") as f:
    on = json.load(f)

hit_off = off['cache_hit_rate']
hit_on = on['cache_hit_rate']
hit_change = hit_on - hit_off

tps_off = off['tps']
tps_on = on['tps']
tps_change = ((tps_on - tps_off) / tps_off * 100) if tps_off > 0 else 0

exec_off = off['block_execution_ms']
exec_on = on['block_execution_ms']
exec_change = ((exec_on - exec_off) / exec_off * 100) if exec_off > 0 else 0

state_off = off['state_root_ms']
state_on = on['state_root_ms']
state_change = ((state_on - state_off) / state_off * 100) if state_off > 0 else 0

print("┌──────────────────────────────────────────────────────────────────────────────┐")
print("│  TPS (Transactions Per Second)                                              │")
print("├──────────────────────────────────────────────────────────────────────────────┤")
print(f"│  Pre-warming OFF:   {tps_off:>10.1f} TPS                                      │")
print(f"│  Pre-warming ON:    {tps_on:>10.1f} TPS                                      │")
print(f"│  Change:            {tps_change:>+8.1f}%                                        │")
print("└──────────────────────────────────────────────────────────────────────────────┘")
print("")

print("┌──────────────────────────────────────────────────────────────────────────────┐")
print("│  CACHE HIT RATE                                                             │")
print("├──────────────────────────────────────────────────────────────────────────────┤")
print(f"│  Pre-warming OFF:   {hit_off:>8.1f}%                                          │")
print(f"│  Pre-warming ON:    {hit_on:>8.1f}%                                          │")
print(f"│  IMPROVEMENT:       {hit_change:>+8.1f}%                                          │")
print("└──────────────────────────────────────────────────────────────────────────────┘")
print("")

print("┌──────────────────────────────────────────────────────────────────────────────┐")
print("│  BLOCK TIMING                                                               │")
print("├──────────────────────────────────────────────────────────────────────────────┤")
print(f"│  Block Exec (OFF):  {exec_off:>10.4f} ms                                      │")
print(f"│  Block Exec (ON):   {exec_on:>10.4f} ms                                      │")
print(f"│  Change:            {exec_change:>+8.1f}%                                        │")
print("│                                                                              │")
print(f"│  State Root (OFF):  {state_off:>10.4f} ms                                      │")
print(f"│  State Root (ON):   {state_on:>10.4f} ms                                      │")
print(f"│  Change:            {state_change:>+8.1f}%                                        │")
print("└──────────────────────────────────────────────────────────────────────────────┘")
print("")

if on['simulations'] > 0:
    print("┌──────────────────────────────────────────────────────────────────────────────┐")
    print("│  PRE-WARMING STATS                                                          │")
    print("├──────────────────────────────────────────────────────────────────────────────┤")
    print(f"│  Simulations:       {on['simulations']:>10}                                     │")
    print(f"│  Prefetch Ops:      {on['prefetch_ops']:>10}                                     │")
    print(f"│  Accounts Fetched:  {on['prefetch_accounts']:>10}                                     │")
    print("└──────────────────────────────────────────────────────────────────────────────┘")
    print("")

print("══════════════════════════════════════════════════════════════════════════════")
print("  SUMMARY")
print("══════════════════════════════════════════════════════════════════════════════")
print(f"  TPS: {tps_off:.1f} → {tps_on:.1f} ({tps_change:+.1f}%)")
print(f"  Cache Hit Rate: {hit_off:.1f}% → {hit_on:.1f}% ({hit_change:+.1f}%)")
if hit_change > 10:
    print("  ✓ PRE-WARMING SIGNIFICANTLY IMPROVES CACHE HIT RATE")
elif hit_change > 0:
    print("  ~ Pre-warming shows marginal improvement")
else:
    print("  ✗ Pre-warming shows no improvement")
print("══════════════════════════════════════════════════════════════════════════════")
PYEOF

echo ""
echo -e "${GREEN}Results saved to: ${RESULTS_DIR}${NC}"

# Write a clean top-level summary (devnet-comparison style)
python3 << PYEOF
import json
from pathlib import Path

root = Path("$RESULTS_DIR")
with (root / "results_off.json").open() as f:
    off = json.load(f)
with (root / "results_on.json").open() as f:
    on = json.load(f)

lines = []
lines.append("FULL LOAD DEVNET SIMULATION v2 - SUMMARY")
lines.append("=")
lines.append("")
lines.append(f"Results dir: {root}")
lines.append(f"TX type: {on.get('tx_type','unknown')}")
lines.append("")

lines.append("PHASE OFF (pre-warming disabled)")
lines.append(f"  Sent success: {off.get('sent_success',0)}")
lines.append(f"  Sent failed:  {off.get('sent_failed',0)}")
lines.append(f"  TPS:          {off.get('tps',0)}")
lines.append(f"  Cache hit %:  {off.get('cache_hit_rate',0)}")
lines.append("")

lines.append("PHASE ON (pre-warming enabled)")
lines.append(f"  Sent success: {on.get('sent_success',0)}")
lines.append(f"  Sent failed:  {on.get('sent_failed',0)}")
lines.append(f"  TPS:          {on.get('tps',0)}")
lines.append(f"  Cache hit %:  {on.get('cache_hit_rate',0)}")
lines.append(f"  Simulations:  {on.get('simulations',0)}")
lines.append(f"  Prefetch ops: {on.get('prefetch_ops',0)}")
lines.append("")

# Deltas
hit_off = float(off.get('cache_hit_rate',0) or 0)
hit_on = float(on.get('cache_hit_rate',0) or 0)
lines.append("DELTA")
lines.append(f"  Cache hit change: {hit_on-hit_off:+.1f} points")

(root / "summary.txt").write_text("\n".join(lines) + "\n")
PYEOF
