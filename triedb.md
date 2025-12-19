rm -rf ~/Library/Application\ Support/reth/dev && rm -rf logs \
&& cargo run --package op-reth --bin op-reth -- node --dev \
  -vvvv \
  --log.file.filter debug \
  --log.file.directory /Users/cliffyang/dev/okx/reth/logs \
  --log.file.name op-reth.log

cast send 0x33f34d8b20696780ba07b1ea89f209b4dc51723a --value 1ether --private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 --rpc-url http://localhost:8545 --gas-price 1000gwei