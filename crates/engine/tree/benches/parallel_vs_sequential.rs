//! Benchmark comparing parallel and sequential transaction execution performance.
//!
//! This benchmark measures the performance difference between parallel and sequential
//! execution under various scenarios:
//! - Independent transactions (no dependencies)
//! - Partially dependent transactions (some dependencies)
//! - Fully dependent transactions (all share addresses)

#![allow(missing_docs)]

use alloy_consensus::TxEip1559;
use alloy_genesis::GenesisAccount;
use alloy_primitives::{Address, B256, TxKind, U256};
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use reth_chainspec::{ChainSpecBuilder, MAINNET};
use reth_db_common::init::init_genesis;
use reth_engine_tree::tree::{
    executor::WorkloadExecutor, ExecutionEnv, ParallelGroupContext, StateProviderBuilder,
};
use reth_ethereum_primitives::{EthPrimitives, Transaction};
use reth_evm::{execute::BlockExecutor, ConfigureEvm, Evm, EvmEnvFor};
use reth_evm_ethereum::EthEvmConfig;
use reth_primitives_traits::{Recovered, SignedTransaction};
use reth_provider::{
    providers::BlockchainProvider,
    test_utils::create_test_provider_factory_with_chain_spec,
    StateProviderFactory,
};
use reth_revm::{
    database::StateProviderDatabase,
    db::State as RevmState,
};
use reth_primitives_traits::crypto::secp256k1::public_key_to_address;
use reth_testing_utils::generators;
use revm::database::states::bundle_state::BundleRetention;
use revm_primitives::HashMap;
use secp256k1::Keypair;
use std::{collections::HashMap as StdHashMap, sync::Arc};

/// Test scenario type
#[derive(Debug, Clone, Copy)]
enum Scenario {
    /// All transactions are independent (different from/to addresses)
    Independent,
    /// Some transactions share addresses (partial dependencies)
    PartiallyDependent,
    /// All transactions share the same from address (full dependencies)
    FullyDependent,
}

/// Creates test transactions based on the scenario
fn create_test_transactions(
    count: usize,
    scenario: Scenario,
    chain_id: u64,
    keypairs: &[(Keypair, Address)],
) -> Vec<(usize, Recovered<alloy_consensus::EthereumTxEnvelope<alloy_consensus::TxEip4844>>)> {
    let mut transactions = Vec::new();
    let mut nonce_map: StdHashMap<Address, u64> = StdHashMap::new();

    match scenario {
        Scenario::Independent => {
            // Each transaction uses different from/to addresses
            for i in 0..count {
                let (keypair, from_addr) = &keypairs[i % keypairs.len()];
                let to_addr = Address::with_last_byte((i + 100) as u8);
                let nonce = *nonce_map.get(from_addr).unwrap_or(&0);
                nonce_map.insert(*from_addr, nonce + 1);

                let tx = Transaction::Eip1559(TxEip1559 {
                    chain_id,
                    nonce,
                    gas_limit: 21000,
                    to: TxKind::Call(to_addr),
                    max_fee_per_gas: 20_000_000_000u128,
                    max_priority_fee_per_gas: 1_000_000_000u128,
                    value: U256::from(1_000_000_000_000_000_000u64), // 1 ETH
                    input: Default::default(),
                    access_list: Default::default(),
                });

                let signed_tx = generators::sign_tx_with_key_pair(keypair.clone(), tx);
                let recovered_tx = signed_tx.with_signer(*from_addr);
                transactions.push((i, recovered_tx));
            }
        }
        Scenario::PartiallyDependent => {
            // Transactions are grouped: first half share from address, second half are independent
            let half = count / 2;
            let shared_from = keypairs[0].1;

            // First half: all share the same from address
            for i in 0..half {
                let to_addr = Address::with_last_byte((i + 100) as u8);
                let (keypair, _) = &keypairs[0];
                let nonce = *nonce_map.get(&shared_from).unwrap_or(&0);
                nonce_map.insert(shared_from, nonce + 1);

                let tx = Transaction::Eip1559(TxEip1559 {
                    chain_id,
                    nonce,
                    gas_limit: 21000,
                    to: TxKind::Call(to_addr),
                    max_fee_per_gas: 20_000_000_000u128,
                    max_priority_fee_per_gas: 1_000_000_000u128,
                    value: U256::from(1_000_000_000_000_000_000u64),
                    input: Default::default(),
                    access_list: Default::default(),
                });

                let signed_tx = generators::sign_tx_with_key_pair(keypair.clone(), tx);
                let recovered_tx = signed_tx.with_signer(shared_from);
                transactions.push((i, recovered_tx));
            }

            // Second half: independent transactions
            for i in half..count {
                let (keypair, from_addr) = &keypairs[(i - half) % keypairs.len()];
                let to_addr = Address::with_last_byte((i + 200) as u8);
                let nonce = *nonce_map.get(from_addr).unwrap_or(&0);
                nonce_map.insert(*from_addr, nonce + 1);

                let tx = Transaction::Eip1559(TxEip1559 {
                    chain_id,
                    nonce,
                    gas_limit: 21000,
                    to: TxKind::Call(to_addr),
                    max_fee_per_gas: 20_000_000_000u128,
                    max_priority_fee_per_gas: 1_000_000_000u128,
                    value: U256::from(1_000_000_000_000_000_000u64),
                    input: Default::default(),
                    access_list: Default::default(),
                });

                let signed_tx = generators::sign_tx_with_key_pair(keypair.clone(), tx);
                let recovered_tx = signed_tx.with_signer(*from_addr);
                transactions.push((i, recovered_tx));
            }
        }
        Scenario::FullyDependent => {
            // All transactions share the same from address
            let (keypair, from_addr) = &keypairs[0];
            for i in 0..count {
                let to_addr = Address::with_last_byte((i + 100) as u8);
                let nonce = *nonce_map.get(from_addr).unwrap_or(&0);
                nonce_map.insert(*from_addr, nonce + 1);

                let tx = Transaction::Eip1559(TxEip1559 {
                    chain_id,
                    nonce,
                    gas_limit: 21000,
                    to: TxKind::Call(to_addr),
                    max_fee_per_gas: 20_000_000_000u128,
                    max_priority_fee_per_gas: 1_000_000_000u128,
                    value: U256::from(1_000_000_000_000_000_000u64),
                    input: Default::default(),
                    access_list: Default::default(),
                });

                let signed_tx = generators::sign_tx_with_key_pair(keypair.clone(), tx);
                let recovered_tx = signed_tx.with_signer(*from_addr);
                transactions.push((i, recovered_tx));
            }
        }
    }

    transactions
}

/// Setup test environment with genesis accounts
fn setup_test_env(
    num_accounts: usize,
) -> (
    Arc<reth_chainspec::ChainSpec>,
    BlockchainProvider<reth_provider::test_utils::MockNodeTypesWithDB>,
    B256,
    Vec<(Keypair, Address)>,
) {
    let mut rng = generators::rng();
    // Create enough keypairs for large transaction counts (at least 1000)
    let keypairs: Vec<_> = (0..num_accounts.max(1000))
        .map(|_| {
            let keypair = generators::generate_key(&mut rng);
            let address = public_key_to_address(keypair.public_key());
            (keypair, address)
        })
        .collect();

    // Create genesis accounts with balances
    let mut genesis_alloc = HashMap::new();
    for (_, addr) in &keypairs {
        genesis_alloc.insert(
            *addr,
            GenesisAccount {
                balance: U256::from(100_000_000_000_000_000_000u128), // 100 ETH
                ..Default::default()
            },
        );
    }

    // Merge with MAINNET genesis
    let mut mainnet_genesis = MAINNET.genesis.clone();
    mainnet_genesis.alloc.extend(genesis_alloc);

    let chain_spec = Arc::new(
        ChainSpecBuilder::default()
            .chain(MAINNET.chain)
            .genesis(mainnet_genesis)
            .paris_activated()
            .build(),
    );

    let factory = create_test_provider_factory_with_chain_spec(chain_spec.clone());
    let genesis_hash = init_genesis(&factory).expect("failed to init genesis");
    let provider = BlockchainProvider::new(factory).expect("failed to create provider");

    (chain_spec, provider, genesis_hash, keypairs)
}

/// Benchmark parallel execution
fn bench_parallel(
    c: &mut Criterion,
    scenario: Scenario,
    tx_count: usize,
    chain_spec: &Arc<reth_chainspec::ChainSpec>,
    provider: &BlockchainProvider<reth_provider::test_utils::MockNodeTypesWithDB>,
    genesis_hash: B256,
    keypairs: &[(Keypair, Address)],
) {
    let transactions = create_test_transactions(
        tx_count,
        scenario,
        chain_spec.chain.id(),
        keypairs,
    );

    let provider_builder = StateProviderBuilder::<EthPrimitives, _>::new(
        provider.clone(),
        genesis_hash,
        None,
    );
    let evm_config = EthEvmConfig::new(chain_spec.clone());
    let evm_env = EvmEnvFor::<EthEvmConfig>::default();
    let env = ExecutionEnv {
        evm_env,
        hash: genesis_hash,
        parent_hash: genesis_hash,
    };

    let parallel_ctx = ParallelGroupContext::<EthPrimitives, _, EthEvmConfig> {
        env: env.clone(),
        evm_config: evm_config.clone(),
        provider: provider_builder,
        max_concurrency: 16,
    };

    let workload_executor = WorkloadExecutor::default();

    let scenario_name = match scenario {
        Scenario::Independent => "independent",
        Scenario::PartiallyDependent => "partially_dependent",
        Scenario::FullyDependent => "fully_dependent",
    };

    c.bench_function(
        &format!("parallel/{}_{}tx", scenario_name, tx_count),
        |b| {
            b.iter(|| {
                let transactions = black_box(transactions.clone());
                let result = parallel_ctx
                    .execute_parallel(transactions, &workload_executor)
                    .expect("parallel execution should succeed");
                black_box(result);
            });
        },
    );
}

/// Benchmark sequential execution
fn bench_sequential(
    c: &mut Criterion,
    scenario: Scenario,
    tx_count: usize,
    chain_spec: &Arc<reth_chainspec::ChainSpec>,
    provider: &BlockchainProvider<reth_provider::test_utils::MockNodeTypesWithDB>,
    genesis_hash: B256,
    keypairs: &[(Keypair, Address)],
) {
    let transactions = create_test_transactions(
        tx_count,
        scenario,
        chain_spec.chain.id(),
        keypairs,
    );

    let evm_config = EthEvmConfig::new(chain_spec.clone());
    let evm_env = EvmEnvFor::<EthEvmConfig>::default();
    let env: ExecutionEnv<EthEvmConfig> = ExecutionEnv {
        evm_env: evm_env.clone(),
        hash: genesis_hash,
        parent_hash: genesis_hash,
    };

    let scenario_name = match scenario {
        Scenario::Independent => "independent",
        Scenario::PartiallyDependent => "partially_dependent",
        Scenario::FullyDependent => "fully_dependent",
    };

    c.bench_function(
        &format!("sequential/{}_{}tx", scenario_name, tx_count),
        |b| {
            b.iter(|| {
                let transactions = black_box(transactions.clone());
                
                // Create fresh state provider for each iteration
                let state_provider = provider
                    .state_by_block_hash(genesis_hash)
                    .expect("failed to get state provider");

                let mut db = RevmState::builder()
                    .with_database(StateProviderDatabase::new(&state_provider))
                    .with_bundle_update()
                    .without_state_clear()
                    .build();

                let evm = evm_config.evm_with_env(&mut db, env.evm_env.clone());
                let ctx = reth_evm::eth::EthBlockExecutionCtx {
                    parent_hash: env.parent_hash,
                    parent_beacon_block_root: None,
                    ommers: &[],
                    withdrawals: None,
                };
                let mut executor = evm_config.create_executor(evm, ctx);

                // Execute transactions sequentially
                for (_, tx) in &transactions {
                    executor
                        .execute_transaction(tx.clone())
                        .expect("transaction execution should succeed");
                }

                let (evm, _) = executor.finish().expect("failed to finish execution");
                let db = evm.into_db();
                db.merge_transitions(BundleRetention::Reverts);
                let _bundle_state = black_box(db.take_bundle());
            });
        },
    );
}

fn parallel_vs_sequential_benchmark(c: &mut Criterion) {
    // Setup test environment once
    let (chain_spec, provider, genesis_hash, keypairs) = setup_test_env(100);

    // Test different transaction counts
    let tx_counts = vec![10, 50, 100, 200, 1000];

    // Test different scenarios
    let scenarios = vec![
        Scenario::Independent,
        Scenario::PartiallyDependent,
        Scenario::FullyDependent,
    ];

    for scenario in &scenarios {
        for &tx_count in &tx_counts {
            bench_parallel(
                c,
                *scenario,
                tx_count,
                &chain_spec,
                &provider,
                genesis_hash,
                &keypairs,
            );
            bench_sequential(
                c,
                *scenario,
                tx_count,
                &chain_spec,
                &provider,
                genesis_hash,
                &keypairs,
            );
        }
    }
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(std::time::Duration::from_secs(1))
        .measurement_time(std::time::Duration::from_secs(5))
        .sample_size(10);
    targets = parallel_vs_sequential_benchmark
}

criterion_main!(benches);
