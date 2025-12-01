
## test

```aiignore
cargo test -p reth-db-common --features trie-db-ext test_triedb_state_root -- --nocapture
```

## bench
```aiignore
cargo bench -p reth-db-common --features trie-db-ext
```

cargo run --release -p reth-db-common --features trie-db-ext --bin state_root_runner -- traditional 100000 5