# run
```
https://xlayertestrpc.okx.com/terigon
https://testrpc.xlayer.tech/unlimited/abcd

cargo run -p reth-sdk --bin xlayer_cli -- transfer \
  --rpc-url https://testrpc.xlayer.tech/unlimited/abcd \
  --private-key $PRIVATE_KEY \
  --to 0x44667e638246762d7ba3dfcedbd753d336e8bc81 \
  --amount 1


cargo run -p reth-sdk --bin xlayer_cli -- token-transfer \
  --rpc-url https://testrpc.xlayer.tech/unlimited/abcd \
  --private-key $PRIVATE_KEY \
  --to 0x44667e638246762d7ba3dfcedbd753d336e8bc81 \
  --token 0xaf5eb02c7bfa28caf1ec3c30a58dce903162096d \
  --amount 1
```
