#!/bin/bash

# Start reth node in dev mode
# Uses --dev mode: local PoA consensus, disables network discovery, enables local HTTP server
# Prefunds 20 accounts, each with 10,000 ETH
#
# Configuration:
# - Block time: 1 second
# - Block gas limit: 1,500,000,000 (1.5B gas)
# - Transaction pool max transactions: 10,000,000 per sub-pool
# - Transaction pool max size: 10,000 MB (10 GB) per sub-pool
# - Max account slots: 10,000,000 per account
# - Data directory: ./data (current directory)
# - Log level: debug for engine::tree::payload_validator to see detailed execution timing
#   (removed engine::tree=debug to reduce log noise and I/O overhead)
#

# Check if reth is compiled
if ! command -v reth &> /dev/null; then
    echo "Error: reth is not compiled, please run 'make install' first"
    exit 1
fi

DATA_DIR="./data"

rm -rf "$DATA_DIR"

GAS_LIMIT=5000000000

# Start reth node in dev mode
RETH_DEV_GAS_LIMIT="$GAS_LIMIT" RETH_DEV_BASE_FEE_MAX_CHANGE_DENOMINATOR=1000000 RETH_DEV_BASE_FEE_ELASTICITY_MULTIPLIER=4 \
reth node \
    --datadir "$DATA_DIR" \
    --dev \
    --dev.block-time 1s \
    --builder.gaslimit "$GAS_LIMIT" \
    --txpool.pending-max-count 10000000 \
    --txpool.pending-max-size 10000 \
    --txpool.basefee-max-count 10000000 \
    --txpool.basefee-max-size 10000 \
    --txpool.queued-max-count 10000000 \
    --txpool.queued-max-size 10000 \
    --txpool.blobpool-max-count 10000000 \
    --txpool.blobpool-max-size 10000 \
    --txpool.max-account-slots 10000000 \
    --http \
    --http.api eth,debug,net,web3,txpool \
    --log.stdout.filter "info,engine::tree::payload_validator=debug,engine::persistence=debug" | tee reth.log
