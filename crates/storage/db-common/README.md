
## test

```aiignore
cargo test -p reth-db-common --features trie-db-ext test_triedb_state_root -- --nocapture
```

## bench
```aiignore
cargo bench -p reth-db-common --features trie-db-ext
cargo bench -p reth-db-common --features trie-db-ext --bench state_root_comparison -- state_root_with_account_overlay
cargo bench -p reth-db-common --features trie-db-ext --bench state_root_comparison -- bench_state_root_mdbx_with_overlay
```

cargo run --release -p reth-db-common --features trie-db-ext --bin state_root_runner -- traditional 100000 5