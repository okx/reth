# run
```
export RPC_URL=https://testrpc.xlayer.tech/unlimited/abcd

## OKB transfer
cargo run -p reth-sdk --bin xlayer_cli -- transfer \
  --rpc-url $RPC_URL$ \
  --private-key $PRIVATE_KEY \
  --to X44667e638246762d7ba3dfcedbd753d336e8bc81 \
  --amount 1

## USDC transfer
cargo run -p reth-sdk --bin xlayer_cli -- token-transfer \
  --rpc-url $RPC_URL$ \
  --private-key $PRIVATE_KEY \
  --to X44667e638246762d7ba3dfcedbd753d336e8bc81 \
  --token 0xaf5eb02c7bfa28caf1ec3c30a58dce903162096d \
  --amount 1

## OKB balance
cargo run -p reth-sdk --bin xlayer_cli -- balance --rpc-url $RPC_URL$ --address X33f34D8b20696780Ba07b1ea89F209B4Dc51723A

## USDC balance
cargo run -p reth-sdk --bin xlayer_cli -- token-balance --rpc-url $RPC_URL$ --token 0xaf5eb02c7bfa28caf1ec3c30a58dce903162096d --account X33f34D8b20696780Ba07b1ea89F209B4Dc51723A

## General contract call, use uniswap v3 quoteExactInputSingle as example
cargo run -p reth-sdk --bin xlayer_cli -- eth-call --rpc-url https://maximum-yolo-seed.quiknode.pro/e4a602e14006c812850883f288b1574b36c48ef6 --to 0x61fFE014bA17989E743c5F6cB21bF9697530B21e --sig "quoteExactInputSingle((address,address,uint256,uint24,uint160))(uint256,uint160,uint32,uint256)" --args "(XC02aaa39b223FE8D0A0e5C4F27eAD9083C756Cc2,XdAC17F958D2ee523a2206206994597C13D831ec7,1000000000000000000,3000,0)"
```

# troubleshooting
```
cargo expand -p reth-sdk --bin xlayer_cli > expanded.rs
```
