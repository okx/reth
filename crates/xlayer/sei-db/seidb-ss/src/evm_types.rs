use seidb_common::{config::StateStoreConfig, evm_keys::EvmKeyKind};

/// EVM module's store key in the Cosmos module system.
pub const EVM_STORE_KEY: &str = "evm";

/// Number of active EVM store types with separate DBs.
pub const NUM_EVM_STORE_TYPES: usize = 5;

/// Returns all active EVM store types.
pub fn all_evm_store_types() -> Vec<EvmKeyKind> {
    vec![
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
        EvmKeyKind::Nonce => "nonce",
        EvmKeyKind::CodeHash => "codehash",
        EvmKeyKind::Code => "code",
        EvmKeyKind::Storage => "storage",
        EvmKeyKind::Legacy => "legacy",
        EvmKeyKind::Empty => "unknown",
    }
}

/// Clone a StateStoreConfig for EVM sub-DB use, forcing use_default_comparer = true.
pub fn sub_db_config(base: &StateStoreConfig) -> StateStoreConfig {
    let mut cfg = base.clone();
    cfg.use_default_comparer = true;
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_evm_store_types() {
        let types = all_evm_store_types();
        assert_eq!(types.len(), 5);
        assert_eq!(types[0], EvmKeyKind::Nonce);
        assert_eq!(types[1], EvmKeyKind::CodeHash);
        assert_eq!(types[2], EvmKeyKind::Code);
        assert_eq!(types[3], EvmKeyKind::Storage);
        assert_eq!(types[4], EvmKeyKind::Legacy);
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
        // Other fields should be preserved
        assert_eq!(sub.backend, base.backend);
        assert_eq!(sub.keep_recent, base.keep_recent);
    }
}
