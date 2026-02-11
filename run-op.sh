#!/bin/bash

# Start op-reth node in dev mode
# Uses --dev mode: local PoA consensus, disables network discovery, enables local HTTP server
# Prefunds 20 accounts, each with 10,000 ETH
#
# Configuration:
# - Block time: 1 second
# - Block gas limit: 1,500,000,000 (1.5B gas)
# - Transaction pool max transactions: 10,000,000 per sub-pool
# - Transaction pool max size: 10,000 MB (10 GB) per sub-pool
# - Max account slots: 10,000,000 per account
# - Data directory: ./data-op (separate from regular reth)
# - Log level: debug for engine::tree::payload_validator to see detailed execution timing
#   (removed engine::tree=debug to reduce log noise and I/O overhead)
#
# Note: EIP-1559 baseFee parameters (denominator/elasticity) cannot be adjusted via CLI.
# BaseFee growth rate is controlled by the chainspec and cannot be modified without code changes.
# To slow baseFee growth, ensure blocks use less gas (closer to 50% of gas limit).

# Check if op-reth is compiled
if ! command -v op-reth &> /dev/null; then
    echo "Error: op-reth is not compiled, please run 'make install-op' first"
    exit 1
fi

DATA_DIR="./data-op"

rm -rf $DATA_DIR

# Start op-reth node in dev mode
OP_DEV_EIP1559_DENOMINATOR=100000000 OP_DEV_GAS_LIMIT=3000000000 op-reth node \
    --datadir $DATA_DIR\
    --dev \
    --dev.block-time 1s \
    --builder.gaslimit 1500000000 \
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
    --log.stdout.filter "info,engine::tree::payload_validator=debug" | tee op-reth.log
