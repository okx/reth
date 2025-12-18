rm -rf ~/Library/Application\ Support/reth/dev && rm -rf logs \
&& cargo run --package op-reth --bin op-reth -- node --dev \
  -vvvv \
  --log.file.filter debug \
  --log.file.directory /Users/cliffyang/dev/okx/reth/logs \
  --log.file.name op-reth.log