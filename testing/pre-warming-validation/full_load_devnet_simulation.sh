#!/bin/bash
#===============================================================================
#  FULL LOAD DEVNET SIMULATION TEST
#===============================================================================
#
#  Simulates devnet testing locally:
#  1. Build and run with pre-warming OFF
#  2. Send high load (configurable, default 50K txns)
#  3. Capture metrics to JSON
#  4. Rebuild and run with pre-warming ON
#  5. Send same load
#  6. Capture metrics to JSON
#  7. Compare results
#
#  Usage: ./full_load_devnet_simulation.sh [--txns N] [--skip-build]
#
#===============================================================================

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
RETH_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

# Configuration
TOTAL_TXNS=50000          # Total transactions to send
BURST_SIZE=500            # Transactions per burst
BURST_DELAY=0.5           # Delay between bursts (seconds)
BLOCK_TIME=1              # Block time in seconds
CAPTURE_DURATION=1        # Minutes to capture after load (1 min is enough since txns already processed)
SKIP_BUILD=false
TX_TYPE="mixed"           # eth, erc20, or mixed
UNIQUE_ADDRESSES=false    # Use unique random addresses for each transaction

# Parse arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        --txns)
            TOTAL_TXNS="$2"
            shift 2
            ;;
        --skip-build)
            SKIP_BUILD=true
            shift
            ;;
        --burst)
            BURST_SIZE="$2"
            shift 2
            ;;
        --unique-addresses)
            UNIQUE_ADDRESSES=true
            shift
            ;;
        --tx-type)
            TX_TYPE="$2"
            shift 2
            ;;
        *)
            echo "Unknown option: $1"
            echo "Usage: $0 [--txns N] [--burst N] [--tx-type eth|erc20|mixed] [--unique-addresses] [--skip-build]"
            exit 1
            ;;
    esac
done

RESULTS_DIR="$RETH_DIR/.devnet-simulation-$(date +%Y%m%d_%H%M%S)"
RESULTS_OFF="$RESULTS_DIR/results_prewarm_OFF.json"
RESULTS_ON="$RESULTS_DIR/results_prewarm_ON.json"
ERROR_LOG="$RESULTS_DIR/errors.log"

# Create results directory
mkdir -p "$RESULTS_DIR"

echo ""
echo -e "${BOLD}${BLUE}╔══════════════════════════════════════════════════════════════════════════════╗${NC}"
echo -e "${BOLD}${BLUE}║          FULL LOAD DEVNET SIMULATION TEST                                    ║${NC}"
echo -e "${BOLD}${BLUE}╚══════════════════════════════════════════════════════════════════════════════╝${NC}"
echo ""
echo -e "  Date:              $(date '+%Y-%m-%d %H:%M:%S')"
echo -e "  Total Transactions: ${TOTAL_TXNS}"
echo -e "  Burst Size:        ${BURST_SIZE} txns"
echo -e "  TX Type:           ${TX_TYPE}"
echo -e "  Unique Addresses:  ${UNIQUE_ADDRESSES}"
echo -e "  Results Dir:       ${RESULTS_DIR}"
echo ""

# Sender addresses (will be funded by dev mode)
SENDER="0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
RECIPIENTS=(
    "0x70997970C51812dc3A010C7d01b50e0d17dc79C8"
    "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC"
    "0x90F79bf6EB2c4f870365E785982E1f101E93b906"
    "0x15d34AAf54267DB7D7c367839AAf71A00a2C6A65"
    "0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc"
)

#-------------------------------------------------------------------------------
# Helper Functions
#-------------------------------------------------------------------------------
cleanup() {
    echo -e "${CYAN}Cleaning up...${NC}"
    pkill -9 op-reth 2>/dev/null || true
    sleep 2
}

wait_for_node() {
    echo -e "  ${CYAN}Waiting for node to start...${NC}"
    for i in {1..30}; do
        if curl -s http://localhost:9001/metrics > /dev/null 2>&1; then
            echo -e "  ${GREEN}✓ Node is ready${NC}"
            return 0
        fi
        sleep 1
    done
    echo -e "  ${RED}✗ Node failed to start${NC}"
    return 1
}

get_metric() {
    local val=$(curl -s "http://localhost:9001/metrics" 2>/dev/null | grep "^$1 " | grep -v "#" | awk '{print $2}' | head -1)
    echo "${val:-0}"
}

# ERC20 contract bytecode (simple token with transfer, transferFrom, approve)
ERC20_BYTECODE="0x608060405234801561001057600080fd5b506040516020806106a08339810180604052602081101561003057600080fd5b5051600080546001600160a01b0319163317808255604080516001600160a01b039290921680835260208301939093528183019290925290517f8be0079c531659141344cd1fd0teleporting9afe36d4da0eb6dec16abd9d4e6d36088d08cca928192903a906060908290030190a1600354600080546001600160a01b03168152600160205260409020556106008061009f6000396000f3fe608060405234801561001057600080fd5b50600436106100885760003560e01c806370a082311161005b57806370a08231146101735780638da5cb5b146101ab578063a9059cbb146101cf578063dd62ed3e1461020d57610088565b8063095ea7b31461008d57806318160ddd146100cd57806323b872dd146100e7578063313ce5671461012d575b600080fd5b6100b9600480360360408110156100a357600080fd5b506001600160a01b03813516906020013561024b565b604080519115158252519081900360200190f35b6100d56102b1565b60408051918252519081900360200190f35b6100b9600480360360608110156100fd57600080fd5b506001600160a01b038135811691602081013590911690604001356102b7565b610135610331565b60405180826001600160a01b03168152602001915050604051809103902060405180910390f35b6100d56004803603602081101561018957600080fd5b50356001600160a01b0316610336565b6101b3610351565b604080516001600160a01b039092168252519081900360200190f35b6100b9600480360360408110156101e557600080fd5b506001600160a01b038135169060200135610360565b6100d56004803603604081101561022357600080fd5b506001600160a01b0381358116916020013516610374565b60006001600160a01b03831661026057600080fd5b3360009081526002602090815260408083206001600160a01b03871684529091529020829055600192915050565b60035490565b6001600160a01b0383166000908152600260209081526040808320338452909152812054828110156102c657600080fd5b6001600160a01b0385166000908152600160205260409020548311156102eb57600080fd5b6001600160a01b0380861660009081526001602052604080822080548790039055918616815220805484019055600192505050949350505050565b601290565b6001600160a01b031660009081526001602052604090205490565b6000546001600160a01b031681565b600061036d33848461039f565b9392505050565b6001600160a01b03918216600090815260026020908152604080832093909416825291909152205490565b6001600160a01b0382166000908152600160205260409020548111156103c457600080fd5b6001600160a01b03808416600090815260016020526040808220805485900390559184168152208054820190555050505056fea265627a7a72315820"

# Deploy ERC20 contract
deploy_erc20() {
    echo -e "  ${CYAN}Deploying ERC20 contract...${NC}" >&2

    # Deploy using Python with web3 signing
    CONTRACT_ADDRESS=$(python3 << 'PYEOF'
import subprocess
import json
import time

# Dev account private key (standard Hardhat/Anvil dev key)
PRIVATE_KEY = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
SENDER = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"

# Simple ERC20 bytecode - includes transfer, transferFrom, approve, balanceOf, allowance
# This is a minimal but complete ERC20 token
BYTECODE = "0x608060405234801561001057600080fd5b506b033b2e3c9fd0803ce80000006000803373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff168152602001908152602001600020819055506b033b2e3c9fd0803ce80000006002819055506105d8806100826000396000f3fe608060405234801561001057600080fd5b50600436106100625760003560e01c8063095ea7b31461006757806318160ddd146100cb57806323b872dd146100e957806370a082311461014d578063a9059cbb146101a5578063dd62ed3e14610209575b600080fd5b6100b36004803603604081101561007d57600080fd5b81019080803573ffffffffffffffffffffffffffffffffffffffff16906020019092919080359060200190929190505050610281565b60405180821515815260200191505060405180910390f35b6100d3610373565b6040518082815260200191505060405180910390f35b6101356004803603606081101561010f57600080fd5b81019080803573ffffffffffffffffffffffffffffffffffffffff169060200190929190803573ffffffffffffffffffffffffffffffffffffffff1690602001909291908035906020019092919050505061037d565b60405180821515815260200191505060405180910390f35b61018f6004803603602081101561016357600080fd5b81019080803573ffffffffffffffffffffffffffffffffffffffff169060200190929190505050610540565b6040518082815260200191505060405180910390f35b6101f1600480360360408110156101bb57600080fd5b81019080803573ffffffffffffffffffffffffffffffffffffffff16906020019092919080359060200190929190505050610588565b60405180821515815260200191505060405180910390f35b61026b6004803603604081101561021f57600080fd5b81019080803573ffffffffffffffffffffffffffffffffffffffff169060200190929190803573ffffffffffffffffffffffffffffffffffffffff16906020019092919050505061059c565b6040518082815260200191505060405180910390f35b600081600160003373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff16815260200190815260200160002060008573ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff168152602001908152602001600020819055508273ffffffffffffffffffffffffffffffffffffffff163373ffffffffffffffffffffffffffffffffffffffff167f8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925846040518082815260200191505060405180910390a36001905092915050565b6000600254905090565b60008060008573ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff168152602001908152602001600020548211156103ca57600080fd5b600160008573ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff16815260200190815260200160002060003373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020016000205482111561045357600080fd5b816000808673ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff16815260200190815260200160002060008282540392505081905550816000808573ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020016000206000828254019250508190555081600160008673ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff16815260200190815260200160002060003373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff168152602001908152602001600020600082825403925050819055506001905092915050565b60008060008373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff168152602001908152602001600020549050919050565b600061059533848461037d565b9050919050565b6000600160008473ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff16815260200190815260200160002060008373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff16815260200190815260200160002054905092915050565b"

try:
    # Try using web3 if available
    from eth_account import Account
    from eth_account.signers.local import LocalAccount

    account: LocalAccount = Account.from_key(PRIVATE_KEY)

    # Get nonce
    nonce_result = subprocess.run(
        ["curl", "-s", "-X", "POST", "http://localhost:8545",
         "-H", "Content-Type: application/json",
         "-d", json.dumps({"jsonrpc": "2.0", "method": "eth_getTransactionCount", "params": [SENDER, "latest"], "id": 1})],
        capture_output=True, text=True, timeout=5
    )
    nonce = int(json.loads(nonce_result.stdout)["result"], 16)

    # Get chain ID
    chain_result = subprocess.run(
        ["curl", "-s", "-X", "POST", "http://localhost:8545",
         "-H", "Content-Type: application/json",
         "-d", json.dumps({"jsonrpc": "2.0", "method": "eth_chainId", "params": [], "id": 1})],
        capture_output=True, text=True, timeout=5
    )
    chain_id = int(json.loads(chain_result.stdout)["result"], 16)

    # Build and sign transaction
    tx = {
        "nonce": nonce,
        "gasPrice": 1000000000,  # 1 gwei
        "gas": 2000000,
        "data": BYTECODE,
        "chainId": chain_id,
    }

    signed = account.sign_transaction(tx)
    raw_tx = signed.raw_transaction.hex()
    if not raw_tx.startswith("0x"):
        raw_tx = "0x" + raw_tx

    # Send raw transaction
    result = subprocess.run(
        ["curl", "-s", "-X", "POST", "http://localhost:8545",
         "-H", "Content-Type: application/json",
         "-d", json.dumps({"jsonrpc": "2.0", "method": "eth_sendRawTransaction", "params": [raw_tx], "id": 1})],
        capture_output=True, text=True, timeout=10
    )
    resp = json.loads(result.stdout)

    if "result" in resp:
        tx_hash = resp["result"]
        # Wait for receipt
        for _ in range(30):
            time.sleep(1)
            receipt_result = subprocess.run(
                ["curl", "-s", "-X", "POST", "http://localhost:8545",
                 "-H", "Content-Type: application/json",
                 "-d", json.dumps({"jsonrpc": "2.0", "method": "eth_getTransactionReceipt", "params": [tx_hash], "id": 1})],
                capture_output=True, text=True, timeout=5
            )
            receipt = json.loads(receipt_result.stdout)
            if receipt.get("result") and receipt["result"].get("contractAddress"):
                print(receipt["result"]["contractAddress"])
                exit(0)
    print("")
except ImportError:
    # Fallback: try eth_sendTransaction anyway
    try:
        nonce_result = subprocess.run(
            ["curl", "-s", "-X", "POST", "http://localhost:8545",
             "-H", "Content-Type: application/json",
             "-d", json.dumps({"jsonrpc": "2.0", "method": "eth_getTransactionCount", "params": [SENDER, "latest"], "id": 1})],
            capture_output=True, text=True, timeout=5
        )
        nonce = int(json.loads(nonce_result.stdout)["result"], 16)

        tx = {
            "from": SENDER,
            "data": BYTECODE,
            "gas": "0x1E8480",
            "gasPrice": "0x3B9ACA00",
            "nonce": hex(nonce)
        }
        payload = {"jsonrpc": "2.0", "method": "eth_sendTransaction", "params": [tx], "id": 1}
        result = subprocess.run(
            ["curl", "-s", "-X", "POST", "http://localhost:8545",
             "-H", "Content-Type: application/json",
             "-d", json.dumps(payload)],
            capture_output=True, text=True, timeout=10
        )
        resp = json.loads(result.stdout)
        if "result" in resp:
            tx_hash = resp["result"]
            for _ in range(30):
                time.sleep(1)
                receipt_result = subprocess.run(
                    ["curl", "-s", "-X", "POST", "http://localhost:8545",
                     "-H", "Content-Type: application/json",
                     "-d", json.dumps({"jsonrpc": "2.0", "method": "eth_getTransactionReceipt", "params": [tx_hash], "id": 1})],
                    capture_output=True, text=True, timeout=5
                )
                receipt = json.loads(receipt_result.stdout)
                if receipt.get("result") and receipt["result"].get("contractAddress"):
                    print(receipt["result"]["contractAddress"])
                    exit(0)
        print("")
    except:
        print("")
except Exception as e:
    print("")
PYEOF
)

    if [ -z "$CONTRACT_ADDRESS" ] || [ "$CONTRACT_ADDRESS" = "" ]; then
        echo -e "  ${YELLOW}⚠ ERC20 deployment failed, using ETH transfers only${NC}" >&2
        echo ""
    else
        echo -e "  ${GREEN}✓ ERC20 deployed at: ${CONTRACT_ADDRESS}${NC}" >&2
        echo "$CONTRACT_ADDRESS"
    fi
}

send_load() {
    local TOTAL=$1
    local LOG_FILE=$2
    local CONTRACT_ADDR="${3:-}"

    echo -e "  ${CYAN}Sending ${TOTAL} transactions (type: ${TX_TYPE})...${NC}"

    python3 << PYEOF
import subprocess
import json
import time
import sys
import secrets

TOTAL = $TOTAL
BURST_SIZE = $BURST_SIZE
BURST_DELAY = $BURST_DELAY
TX_TYPE = "$TX_TYPE"
CONTRACT_ADDR = "$CONTRACT_ADDR"
UNIQUE_ADDRESSES = "$UNIQUE_ADDRESSES" == "true"

# Dev account
PRIVATE_KEY = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
SENDER = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
STATIC_RECIPIENTS = ["0x70997970C51812dc3A010C7d01b50e0d17dc79C8",
              "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC",
              "0x90F79bf6EB2c4f870365E785982E1f101E93b906",
              "0x15d34AAf54267DB7D7c367839AAf71A00a2C6A65",
              "0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc"]

def generate_random_address():
    """Generate a random Ethereum address"""
    return "0x" + secrets.token_hex(20)

def get_recipient(index):
    """Get recipient address - random if UNIQUE_ADDRESSES, else cycle through static list"""
    if UNIQUE_ADDRESSES:
        return generate_random_address()
    return STATIC_RECIPIENTS[index % len(STATIC_RECIPIENTS)]

# Try to use eth_account for signing
try:
    from eth_account import Account
    from web3 import Web3
    USE_SIGNED = True
    account = Account.from_key(PRIVATE_KEY)

    # Get chain ID
    chain_result = subprocess.run(
        ["curl", "-s", "-X", "POST", "http://localhost:8545",
         "-H", "Content-Type: application/json",
         "-d", json.dumps({"jsonrpc": "2.0", "method": "eth_chainId", "params": [], "id": 1})],
        capture_output=True, text=True, timeout=5
    )
    CHAIN_ID = int(json.loads(chain_result.stdout)["result"], 16)

    # Checksum the contract address if provided
    if CONTRACT_ADDR:
        CONTRACT_ADDR = Web3.to_checksum_address(CONTRACT_ADDR)
except ImportError:
    USE_SIGNED = False
    CHAIN_ID = 1

success = 0
failed = 0
start_time = time.time()

num_bursts = TOTAL // BURST_SIZE
remainder = TOTAL % BURST_SIZE

def send_raw_tx(raw_tx):
    """Send a raw signed transaction"""
    try:
        result = subprocess.run(
            ["curl", "-s", "-X", "POST", "http://localhost:8545",
             "-H", "Content-Type: application/json",
             "-d", json.dumps({"jsonrpc": "2.0", "method": "eth_sendRawTransaction", "params": [raw_tx], "id": 1})],
            capture_output=True, text=True, timeout=5
        )
        resp = json.loads(result.stdout)
        return "result" in resp
    except:
        return False

def send_eth_tx(to_addr, nonce):
    """Send ETH transfer"""
    if USE_SIGNED:
        to_checksum = Web3.to_checksum_address(to_addr)
        tx = {
            "nonce": nonce,
            "gasPrice": 1000000000,
            "gas": 21000,
            "to": to_checksum,
            "value": 0x1000,
            "chainId": CHAIN_ID,
        }
        signed = account.sign_transaction(tx)
        raw_tx = signed.raw_transaction.hex()
        if not raw_tx.startswith("0x"):
            raw_tx = "0x" + raw_tx
        return send_raw_tx(raw_tx)
    else:
        tx = {
            "from": SENDER,
            "to": to_addr,
            "value": "0x1000",
            "gas": "0x5208",
            "gasPrice": "0x3B9ACA00",
            "nonce": hex(nonce)
        }
        payload = {"jsonrpc": "2.0", "method": "eth_sendTransaction", "params": [tx], "id": nonce}
        try:
            result = subprocess.run(
                ["curl", "-s", "-X", "POST", "http://localhost:8545",
                 "-H", "Content-Type: application/json",
                 "-d", json.dumps(payload)],
                capture_output=True, text=True, timeout=5
            )
            resp = json.loads(result.stdout)
            return "result" in resp
        except:
            return False

def send_erc20_transfer(to_addr, nonce, contract):
    """Send ERC20 transfer - triggers simulation heuristics"""
    # transfer(address,uint256) = 0xa9059cbb
    to_padded = to_addr[2:].lower().zfill(64)
    amount = "00000000000000000000000000000000000000000000000000000000000003e8"  # 1000 wei - tiny amount to avoid exhaustion
    data = bytes.fromhex("a9059cbb" + to_padded + amount)

    if USE_SIGNED:
        contract_checksum = Web3.to_checksum_address(contract)
        tx = {
            "nonce": nonce,
            "gasPrice": 1000000000,
            "gas": 100000,
            "to": contract_checksum,
            "value": 0,
            "data": data,
            "chainId": CHAIN_ID,
        }
        signed = account.sign_transaction(tx)
        raw_tx = signed.raw_transaction.hex()
        if not raw_tx.startswith("0x"):
            raw_tx = "0x" + raw_tx
        return send_raw_tx(raw_tx)
    else:
        tx = {
            "from": SENDER,
            "to": contract,
            "data": "0xa9059cbb" + to_padded + amount,
            "gas": "0x15F90",
            "gasPrice": "0x3B9ACA00",
            "nonce": hex(nonce)
        }
        payload = {"jsonrpc": "2.0", "method": "eth_sendTransaction", "params": [tx], "id": nonce}
        try:
            result = subprocess.run(
                ["curl", "-s", "-X", "POST", "http://localhost:8545",
                 "-H", "Content-Type: application/json",
                 "-d", json.dumps(payload)],
                capture_output=True, text=True, timeout=5
            )
            resp = json.loads(result.stdout)
            return "result" in resp
        except:
            return False

def send_erc20_approve(spender, nonce, contract):
    """Send ERC20 approve - triggers simulation heuristics"""
    # approve(address,uint256) = 0x095ea7b3
    spender_padded = spender[2:].lower().zfill(64)
    amount = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
    data = bytes.fromhex("095ea7b3" + spender_padded + amount)

    if USE_SIGNED:
        contract_checksum = Web3.to_checksum_address(contract)
        tx = {
            "nonce": nonce,
            "gasPrice": 1000000000,
            "gas": 100000,
            "to": contract_checksum,
            "value": 0,
            "data": data,
            "chainId": CHAIN_ID,
        }
        signed = account.sign_transaction(tx)
        raw_tx = signed.raw_transaction.hex()
        if not raw_tx.startswith("0x"):
            raw_tx = "0x" + raw_tx
        return send_raw_tx(raw_tx)
    else:
        tx = {
            "from": SENDER,
            "to": contract,
            "data": "0x095ea7b3" + spender_padded + amount,
            "gas": "0x15F90",
            "gasPrice": "0x3B9ACA00",
            "nonce": hex(nonce)
        }
        payload = {"jsonrpc": "2.0", "method": "eth_sendTransaction", "params": [tx], "id": nonce}
        try:
            result = subprocess.run(
                ["curl", "-s", "-X", "POST", "http://localhost:8545",
                 "-H", "Content-Type: application/json",
                 "-d", json.dumps(payload)],
                capture_output=True, text=True, timeout=5
            )
            resp = json.loads(result.stdout)
            return "result" in resp
        except:
            return False

# Get initial nonce
nonce_result = subprocess.run(
    ["curl", "-s", "-X", "POST", "http://localhost:8545",
     "-H", "Content-Type: application/json",
     "-d", json.dumps({"jsonrpc": "2.0", "method": "eth_getTransactionCount", "params": [SENDER, "latest"], "id": 1})],
    capture_output=True, text=True, timeout=5
)
nonce = int(json.loads(nonce_result.stdout)["result"], 16)

# For mixed/erc20 mode, setup approvals first (use static recipients for approvals)
if TX_TYPE in ["erc20", "mixed"] and CONTRACT_ADDR:
    print(f"    Setting up approvals...")
    for recipient in STATIC_RECIPIENTS:
        if send_erc20_approve(recipient, nonce, CONTRACT_ADDR):
            nonce += 1
            success += 1
        else:
            failed += 1
            nonce += 1
        time.sleep(0.1)

for burst in range(num_bursts + (1 if remainder > 0 else 0)):
    burst_count = BURST_SIZE if burst < num_bursts else remainder
    if burst_count == 0:
        break

    for i in range(burst_count):
        to_addr = get_recipient(success + i)  # Use success+i to get unique index across bursts

        if TX_TYPE == "eth":
            ok = send_eth_tx(to_addr, nonce)
        elif TX_TYPE == "erc20" and CONTRACT_ADDR:
            ok = send_erc20_transfer(to_addr, nonce, CONTRACT_ADDR)
        elif TX_TYPE == "mixed" and CONTRACT_ADDR:
            # Alternate between ETH and ERC20
            if i % 2 == 0:
                ok = send_eth_tx(to_addr, nonce)
            else:
                ok = send_erc20_transfer(to_addr, nonce, CONTRACT_ADDR)
        else:
            # Fallback to ETH if no contract
            ok = send_eth_tx(to_addr, nonce)

        if ok:
            success += 1
        else:
            failed += 1
        nonce += 1

    # Progress
    progress = (burst + 1) * 100 // (num_bursts + (1 if remainder > 0 else 0))
    elapsed = time.time() - start_time
    tps = success / elapsed if elapsed > 0 else 0
    print(f"\r    [{('#' * (progress // 2)):<50}] {progress}% | {success}/{TOTAL} sent | {tps:.1f} TPS", end="", flush=True)

    if BURST_DELAY > 0:
        time.sleep(BURST_DELAY)

elapsed = time.time() - start_time
final_tps = success / elapsed if elapsed > 0 else 0

print(f"\n  ✓ Completed: {success}/{TOTAL} transactions in {elapsed:.1f}s ({final_tps:.1f} TPS)")
print(f"    Failed: {failed}")

# Return success count
print(f"SUCCESS:{success}")
PYEOF
}

capture_metrics() {
    local OUTPUT_FILE=$1
    local PREWARM_MODE=$2

    echo -e "  ${CYAN}Capturing final metrics...${NC}"

    # Get metrics - Use CachedReads metrics (reth_txpool_pre_warming_cache_*) NOT ExecutionCache metrics
    # The CachedReads cache is what prefetch populates and execution uses in payload builder
    # ExecutionCache (reth_sync_caching_*) is a separate cache in the engine tree
    local FINAL_HITS=$(get_metric "reth_txpool_pre_warming_cache_hits")
    local FINAL_MISSES=$(get_metric "reth_txpool_pre_warming_cache_misses")
    local FINAL_SIMS=$(get_metric "reth_txpool_pre_warming_simulations_completed")
    local FINAL_PREFETCH=$(get_metric "reth_txpool_pre_warming_prefetch_operations")
    local PREFETCH_ACCOUNTS=$(get_metric "reth_txpool_pre_warming_prefetch_accounts")
    local PREFETCH_STORAGE=$(get_metric "reth_txpool_pre_warming_prefetch_storage_slots")
    local SIMS_FAILED=$(get_metric "reth_txpool_pre_warming_simulations_failed")

    local BUILD_EXEC_SUM=$(get_metric "reth_block_timing_build_exec_mempool_transactions_sum")
    local BUILD_EXEC_COUNT=$(get_metric "reth_block_timing_build_exec_mempool_transactions_count")
    local STATE_ROOT_SUM=$(get_metric "reth_block_timing_build_calc_state_root_sum")
    local STATE_ROOT_COUNT=$(get_metric "reth_block_timing_build_calc_state_root_count")

    local FINAL_BLOCK=$(curl -s "http://localhost:8545" -X POST -H "Content-Type: application/json" \
        -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' 2>/dev/null | \
        python3 -c "import sys,json; print(int(json.load(sys.stdin).get('result','0x0'),16))" 2>/dev/null || echo "0")

    # Calculate values
    local TOTAL_ACCESS=$((FINAL_HITS + FINAL_MISSES))
    local HIT_RATE=0
    if [ $TOTAL_ACCESS -gt 0 ]; then
        HIT_RATE=$(python3 -c "print(round($FINAL_HITS * 100 / $TOTAL_ACCESS, 1))")
    fi

    # Block timing
    local BLOCK_EXEC_MS=0
    local STATE_ROOT_MS=0
    BUILD_EXEC_SUM="${BUILD_EXEC_SUM:-0}"
    BUILD_EXEC_COUNT="${BUILD_EXEC_COUNT:-0}"
    STATE_ROOT_SUM="${STATE_ROOT_SUM:-0}"
    STATE_ROOT_COUNT="${STATE_ROOT_COUNT:-0}"

    if [ "$BUILD_EXEC_COUNT" != "0" ] && [ -n "$BUILD_EXEC_COUNT" ]; then
        BLOCK_EXEC_MS=$(python3 -c "print(round(float('$BUILD_EXEC_SUM') / float('$BUILD_EXEC_COUNT') * 1000, 4))")
    fi
    if [ "$STATE_ROOT_COUNT" != "0" ] && [ -n "$STATE_ROOT_COUNT" ]; then
        STATE_ROOT_MS=$(python3 -c "print(round(float('$STATE_ROOT_SUM') / float('$STATE_ROOT_COUNT') * 1000, 4))")
    fi

    # Save to JSON
    python3 << PYEOF
import json
from datetime import datetime

results = {
    "timestamp": datetime.now().isoformat(),
    "prewarm_mode": "${PREWARM_MODE}",
    "total_txns_sent": ${TOTAL_TXNS},
    "blocks_processed": ${FINAL_BLOCK},
    "cache_hits": ${FINAL_HITS},
    "cache_misses": ${FINAL_MISSES},
    "cache_hit_rate": ${HIT_RATE},
    "block_execution_ms": ${BLOCK_EXEC_MS},
    "state_root_ms": ${STATE_ROOT_MS},
    "simulations_completed": ${FINAL_SIMS:-0},
    "simulations_failed": ${SIMS_FAILED:-0},
    "prefetch_ops": ${FINAL_PREFETCH:-0},
    "prefetch_accounts": ${PREFETCH_ACCOUNTS:-0},
    "prefetch_storage": ${PREFETCH_STORAGE:-0}
}

with open("${OUTPUT_FILE}", "w") as f:
    json.dump(results, f, indent=2)
PYEOF

    echo -e "  ${GREEN}✓ Metrics saved to ${OUTPUT_FILE}${NC}"
    echo -e "    Cache Hit Rate: ${HIT_RATE}%"
    echo -e "    Blocks: ${FINAL_BLOCK}"
    echo -e "    Simulations: ${FINAL_SIMS:-0}"
    echo -e "    Prefetch Ops: ${FINAL_PREFETCH:-0}"
}

#===============================================================================
# PHASE 1: Pre-warming OFF
#===============================================================================
echo ""
echo -e "${BOLD}${BLUE}══════════════════════════════════════════════════════════════════════════════${NC}"
echo -e "${BOLD}  PHASE 1: Pre-warming DISABLED${NC}"
echo -e "${BOLD}${BLUE}══════════════════════════════════════════════════════════════════════════════${NC}"
echo ""

cleanup

if [ "$SKIP_BUILD" = false ]; then
    echo -e "  ${CYAN}Building op-reth with pre-warming feature...${NC}"
    cd "$RETH_DIR"
    cargo build --release --package op-reth --features pre-warming 2>&1 | tail -3
    echo -e "  ${GREEN}✓ Build complete${NC}"
fi

# Start node WITHOUT pre-warming
echo -e "  ${CYAN}Starting node (pre-warming OFF)...${NC}"
DATA_DIR_OFF="$RESULTS_DIR/data-off"
rm -rf "$DATA_DIR_OFF"

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
sleep 5  # Extra stabilization time

# Deploy ERC20 if needed
CONTRACT_ADDRESS_OFF=""
if [ "$TX_TYPE" = "erc20" ] || [ "$TX_TYPE" = "mixed" ]; then
    CONTRACT_ADDRESS_OFF=$(deploy_erc20)
fi

# Send load
echo ""
echo -e "  ${BOLD}Sending load...${NC}"
send_load $TOTAL_TXNS "$ERROR_LOG" "$CONTRACT_ADDRESS_OFF" 2>&1 | tee -a "$ERROR_LOG"

# Wait for transactions to be processed
echo -e "  ${CYAN}Waiting for transactions to be processed...${NC}"
sleep 30

# Capture metrics
capture_metrics "$RESULTS_OFF" "OFF"

cleanup

#===============================================================================
# PHASE 2: Pre-warming ON
#===============================================================================
echo ""
echo -e "${BOLD}${BLUE}══════════════════════════════════════════════════════════════════════════════${NC}"
echo -e "${BOLD}  PHASE 2: Pre-warming ENABLED${NC}"
echo -e "${BOLD}${BLUE}══════════════════════════════════════════════════════════════════════════════${NC}"
echo ""

# Start node WITH pre-warming
echo -e "  ${CYAN}Starting node (pre-warming ON)...${NC}"
DATA_DIR_ON="$RESULTS_DIR/data-on"
rm -rf "$DATA_DIR_ON"

NUM_CPUS=$(sysctl -n hw.ncpu 2>/dev/null || nproc 2>/dev/null || echo "8")

"$RETH_DIR/target/release/op-reth" node \
    --datadir "$DATA_DIR_ON" \
    --dev --dev.block-time ${BLOCK_TIME}s \
    --http --http.api eth,debug,net,web3,txpool \
    --metrics 0.0.0.0:9001 \
    --txpool.pre-warming true \
    --txpool.pre-warming-workers $NUM_CPUS \
    --txpool.pre-fetch-workers $NUM_CPUS \
    --log.stdout.filter error > "$RESULTS_DIR/node_on.log" 2>&1 &

NODE_PID=$!
echo "  Node PID: $NODE_PID"

wait_for_node || exit 1
sleep 5  # Extra stabilization time

# Deploy ERC20 if needed (fresh deployment for new node)
CONTRACT_ADDRESS_ON=""
if [ "$TX_TYPE" = "erc20" ] || [ "$TX_TYPE" = "mixed" ]; then
    CONTRACT_ADDRESS_ON=$(deploy_erc20)
fi

# Send load
echo ""
echo -e "  ${BOLD}Sending load...${NC}"
send_load $TOTAL_TXNS "$ERROR_LOG" "$CONTRACT_ADDRESS_ON" 2>&1 | tee -a "$ERROR_LOG"

# Wait for transactions to be processed
echo -e "  ${CYAN}Waiting for transactions to be processed...${NC}"
sleep 30

# Capture metrics
capture_metrics "$RESULTS_ON" "ON"

cleanup

#===============================================================================
# PHASE 3: Compare Results
#===============================================================================
echo ""
echo -e "${BOLD}${BLUE}══════════════════════════════════════════════════════════════════════════════${NC}"
echo -e "${BOLD}  PHASE 3: COMPARISON${NC}"
echo -e "${BOLD}${BLUE}══════════════════════════════════════════════════════════════════════════════${NC}"
echo ""

"$SCRIPT_DIR/devnet_comparison.sh" --compare "$RESULTS_OFF" "$RESULTS_ON"

#===============================================================================
# Save Summary
#===============================================================================
SUMMARY_FILE="$RESULTS_DIR/summary.md"

cat > "$SUMMARY_FILE" << EOF
# Full Load Devnet Simulation Results

**Date:** $(date '+%Y-%m-%d %H:%M:%S')

## Test Configuration

| Parameter | Value |
|-----------|-------|
| Total Transactions | $TOTAL_TXNS |
| Burst Size | $BURST_SIZE |
| Block Time | ${BLOCK_TIME}s |
| Capture Duration | $CAPTURE_DURATION minutes |
| Workers (ON mode) | $NUM_CPUS |

## Results

$(cat "$RESULTS_OFF" | python3 -c "
import json, sys
d = json.load(sys.stdin)
print(f'''### Pre-warming OFF
- Cache Hit Rate: {d['cache_hit_rate']}%
- Block Execution: {d['block_execution_ms']:.4f} ms
- State Root: {d['state_root_ms']:.4f} ms
- Blocks Processed: {d['blocks_processed']}
''')
")

$(cat "$RESULTS_ON" | python3 -c "
import json, sys
d = json.load(sys.stdin)
print(f'''### Pre-warming ON
- Cache Hit Rate: {d['cache_hit_rate']}%
- Block Execution: {d['block_execution_ms']:.4f} ms
- State Root: {d['state_root_ms']:.4f} ms
- Blocks Processed: {d['blocks_processed']}
- Simulations Completed: {d['simulations_completed']}
- Prefetch Operations: {d['prefetch_ops']}
''')
")

## Files

- \`results_prewarm_OFF.json\` - Metrics without pre-warming
- \`results_prewarm_ON.json\` - Metrics with pre-warming
- \`node_off.log\` - Node logs (OFF mode)
- \`node_on.log\` - Node logs (ON mode)
- \`errors.log\` - Transaction errors

EOF

echo ""
echo -e "${GREEN}══════════════════════════════════════════════════════════════════════════════${NC}"
echo -e "${GREEN}  TEST COMPLETE${NC}"
echo -e "${GREEN}══════════════════════════════════════════════════════════════════════════════${NC}"
echo ""
echo -e "  Results saved to: ${RESULTS_DIR}"
echo -e "  Summary: ${SUMMARY_FILE}"
echo ""
echo -e "  Files:"
echo -e "    - ${RESULTS_OFF}"
echo -e "    - ${RESULTS_ON}"
echo ""

