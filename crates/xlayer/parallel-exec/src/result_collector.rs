//! Result collection and merging for parallel execution.
//!
//! Collects [`TxExecutionResult`]s from the Dispatcher, sorts them by original
//! transaction order, and merges state changes into a unified [`EvmState`](revm::state::EvmState).

use crate::dispatcher::TxExecutionResult;
use alloy_primitives::U256;

/// Collect and sort execution results by original transaction index.
///
/// The Dispatcher may return results in arbitrary order (depending on which
/// rayon tasks finish first). This restores the canonical block ordering
/// required for receipt construction and state commitment.
pub fn collect_ordered_results(mut results: Vec<TxExecutionResult>) -> Vec<TxExecutionResult> {
    results.sort_by_key(|r| r.original_index);
    results
}

/// Compute cumulative gas used and total fees from ordered results.
///
/// `base_fee` is accepted for future use when the effective gas price per
/// transaction is available. Currently only cumulative gas is tracked.
pub fn compute_gas_and_fees(results: &[TxExecutionResult], _base_fee: Option<u64>) -> (u64, U256) {
    let mut cumulative_gas = 0u64;
    let total_fees = U256::ZERO;

    for result in results {
        cumulative_gas = cumulative_gas.saturating_add(result.gas_used);
    }

    (cumulative_gas, total_fees)
}

/// Merge all execution results' states into a single [`EvmState`](revm::state::EvmState).
///
/// Processes results in original transaction order to ensure correct
/// state override semantics: later transactions' writes overwrite earlier ones
/// for the same account/slot.
pub fn merge_states(results: &[TxExecutionResult]) -> revm::state::EvmState {
    let mut merged = revm::state::EvmState::default();

    for result in results {
        for (address, account) in &result.state {
            if let Some(existing) = merged.get_mut(address) {
                existing.info = account.info.clone();
                // Later values override earlier ones for the same slot
                for (slot, value) in &account.storage {
                    existing.storage.insert(*slot, value.clone());
                }
                existing.status = account.status;
            } else {
                merged.insert(*address, account.clone());
            }
        }
    }

    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Address;
    use revm_state::{Account, AccountInfo, AccountStatus, EvmStorageSlot};

    fn make_result(index: usize, gas: u64) -> TxExecutionResult {
        TxExecutionResult {
            original_index: index,
            result: revm::context::result::ExecutionResult::Halt {
                reason: revm::context::result::HaltReason::NotActivated,
                gas: revm::context::result::ResultGas::new(gas, 0, 0),
                    logs: std::vec::Vec::new(),
            },
            state: Default::default(),
            gas_used: gas,
        }
    }

    fn make_result_with_state(
        index: usize,
        gas: u64,
        state: revm::state::EvmState,
    ) -> TxExecutionResult {
        TxExecutionResult {
            original_index: index,
            result: revm::context::result::ExecutionResult::Halt {
                reason: revm::context::result::HaltReason::NotActivated,
                gas: revm::context::result::ResultGas::new(gas, 0, 0),
                    logs: std::vec::Vec::new(),
            },
            state,
            gas_used: gas,
        }
    }

    #[test]
    fn test_collect_ordered_results() {
        let results = vec![make_result(3, 100), make_result(1, 200), make_result(2, 150)];

        let ordered = collect_ordered_results(results);

        assert_eq!(ordered[0].original_index, 1);
        assert_eq!(ordered[1].original_index, 2);
        assert_eq!(ordered[2].original_index, 3);
    }

    #[test]
    fn test_merge_states_single() {
        let addr = Address::with_last_byte(0x42);
        let mut state = revm::state::EvmState::default();
        let account = Account {
            info: AccountInfo { balance: U256::from(500), nonce: 3, ..Default::default() },
            original_info: Box::new(AccountInfo::default()),
            status: AccountStatus::Touched,
            storage: Default::default(),
            transaction_id: 0,
        };
        state.insert(addr, account);

        let results = vec![make_result_with_state(0, 21000, state)];
        let merged = merge_states(&results);

        assert_eq!(merged.len(), 1);
        let merged_account = merged.get(&addr).unwrap();
        assert_eq!(merged_account.info.balance, U256::from(500));
        assert_eq!(merged_account.info.nonce, 3);
    }

    #[test]
    fn test_merge_states_override() {
        let addr = Address::with_last_byte(0x42);
        let slot = U256::from(7);

        // First tx writes slot=7 to value 100
        let mut state1 = revm::state::EvmState::default();
        let mut storage1 = revm_state::EvmStorage::default();
        storage1.insert(
            slot,
            EvmStorageSlot {
                original_value: U256::ZERO,
                present_value: U256::from(100),
                ..Default::default()
            },
        );
        state1.insert(
            addr,
            Account {
                info: AccountInfo { balance: U256::from(1000), nonce: 1, ..Default::default() },
                original_info: Box::new(AccountInfo::default()),
                status: AccountStatus::Touched,
                storage: storage1,
                transaction_id: 0,
            },
        );

        // Second tx writes slot=7 to value 999 (should override)
        let mut state2 = revm::state::EvmState::default();
        let mut storage2 = revm_state::EvmStorage::default();
        storage2.insert(
            slot,
            EvmStorageSlot {
                original_value: U256::from(100),
                present_value: U256::from(999),
                ..Default::default()
            },
        );
        state2.insert(
            addr,
            Account {
                info: AccountInfo { balance: U256::from(800), nonce: 2, ..Default::default() },
                original_info: Box::new(AccountInfo::default()),
                status: AccountStatus::Touched,
                storage: storage2,
                transaction_id: 0,
            },
        );

        let results = vec![
            make_result_with_state(0, 21000, state1),
            make_result_with_state(1, 21000, state2),
        ];
        let merged = merge_states(&results);

        let merged_account = merged.get(&addr).unwrap();
        // Later tx's info should win
        assert_eq!(merged_account.info.balance, U256::from(800));
        assert_eq!(merged_account.info.nonce, 2);
        // Later tx's storage value should win
        let merged_slot = merged_account.storage.get(&slot).unwrap();
        assert_eq!(merged_slot.present_value, U256::from(999));
    }

    #[test]
    fn test_compute_gas() {
        let results = vec![make_result(0, 21000), make_result(1, 42000), make_result(2, 63000)];

        let (total_gas, _fees) = compute_gas_and_fees(&results, None);
        assert_eq!(total_gas, 126000);
    }
}
