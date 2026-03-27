use mptdb_common::{config::StateStoreConfig, evm_keys::EvmKeyKind};

/// Number of active EVM store types with separate DBs.
pub const NUM_EVM_STORE_TYPES: usize = 6;

/// Returns all active EVM store types.
pub fn all_evm_store_types() -> Vec<EvmKeyKind> {
    vec![
        EvmKeyKind::Account,
        EvmKeyKind::Nonce,
        EvmKeyKind::CodeHash,
        EvmKeyKind::Code,
        EvmKeyKind::Storage,
        EvmKeyKind::Legacy,
    ]
}

/// Returns human-readable directory name for an EVM store type.
pub fn store_type_name(st: EvmKeyKind) -> &'static str {
    match st {
        EvmKeyKind::Account => "account",
        EvmKeyKind::Nonce => "nonce",
        EvmKeyKind::CodeHash => "codehash",
        EvmKeyKind::Code => "code",
        EvmKeyKind::Storage => "storage",
        EvmKeyKind::Legacy => "legacy",
        EvmKeyKind::Empty => "unknown",
    }
}

/// Clone a `StateStoreConfig` for EVM sub-DB use, forcing `use_default_comparer = true`.
pub fn sub_db_config(base: &StateStoreConfig) -> StateStoreConfig {
    let mut cfg = base.clone();
    cfg.use_default_comparer = true;
    cfg.async_write_buffer = 0;
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_evm_store_types() {
        let types = all_evm_store_types();
        assert_eq!(types.len(), 6);
        assert_eq!(types[0], EvmKeyKind::Account); // new: merged (nonce, balance, code_hash)
        assert_eq!(types[1], EvmKeyKind::Nonce);
        assert_eq!(types[2], EvmKeyKind::CodeHash);
        assert_eq!(types[3], EvmKeyKind::Code);
        assert_eq!(types[4], EvmKeyKind::Storage);
        assert_eq!(types[5], EvmKeyKind::Legacy);
    }

    #[test]
    fn test_store_type_name() {
        assert_eq!(store_type_name(EvmKeyKind::Nonce), "nonce");
        assert_eq!(store_type_name(EvmKeyKind::CodeHash), "codehash");
        assert_eq!(store_type_name(EvmKeyKind::Code), "code");
        assert_eq!(store_type_name(EvmKeyKind::Storage), "storage");
        assert_eq!(store_type_name(EvmKeyKind::Legacy), "legacy");
        assert_eq!(store_type_name(EvmKeyKind::Empty), "unknown");
    }

    #[test]
    fn test_sub_db_config() {
        let base = StateStoreConfig { use_default_comparer: false, ..Default::default() };
        let sub = sub_db_config(&base);
        assert!(sub.use_default_comparer);
        assert_eq!(sub.async_write_buffer, 0);
        // Other fields should be preserved
        assert_eq!(sub.backend, base.backend);
        assert_eq!(sub.keep_recent, base.keep_recent);
    }
}
