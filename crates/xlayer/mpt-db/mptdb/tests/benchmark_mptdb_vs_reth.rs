//! Benchmark-style repeated runners for B4.2-B4.7.
//!
//! Run with:
//! `PROTOC=/Users/louisliuxiong/golang/bin/protoc MPT_BENCH_ITERS=5 cargo test -p mptdb --release
//! --test benchmark_mptdb_vs_reth bench_b4_7_mainnet_realistic_mpt_only -- --ignored --nocapture
//! --exact`

mod common;

#[cfg(all(feature = "jemalloc", unix))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use common::mptdb_vs_reth_support::{
    benchmark_iterations_from_env, print_mpt_benchmark, print_reth_benchmark, run_mpt_benchmark,
    run_reth_benchmark, scenario_b4_1, scenario_b4_2, scenario_b4_3, scenario_b4_4, scenario_b4_5,
    scenario_b4_6, scenario_b4_7,
};

fn run_benchmark_reth_only(scenario: common::mptdb_vs_reth_support::ProfileScenario) {
    let iterations = benchmark_iterations_from_env();
    let summary = run_reth_benchmark(scenario, iterations);
    print_reth_benchmark(scenario, &summary);
}

fn run_benchmark_mpt_only(scenario: common::mptdb_vs_reth_support::ProfileScenario) {
    let iterations = benchmark_iterations_from_env();
    let summary = run_mpt_benchmark(scenario, iterations);
    print_mpt_benchmark(scenario, &summary);
}

macro_rules! define_benchmark_only_tests {
    ($reth_name:ident, $mpt_name:ident, $scenario_fn:ident) => {
        #[test]
        #[ignore]
        fn $reth_name() {
            run_benchmark_reth_only($scenario_fn());
        }

        #[test]
        #[ignore]
        fn $mpt_name() {
            run_benchmark_mpt_only($scenario_fn());
        }
    };
}

define_benchmark_only_tests!(bench_b4_1_reth_only, bench_b4_1_mpt_only, scenario_b4_1);
define_benchmark_only_tests!(bench_b4_2_reth_only, bench_b4_2_mpt_only, scenario_b4_2);
define_benchmark_only_tests!(bench_b4_3_reth_only, bench_b4_3_mpt_only, scenario_b4_3);
define_benchmark_only_tests!(bench_b4_4_reth_only, bench_b4_4_mpt_only, scenario_b4_4);
define_benchmark_only_tests!(bench_b4_5_reth_only, bench_b4_5_mpt_only, scenario_b4_5);
define_benchmark_only_tests!(bench_b4_6_reth_only, bench_b4_6_mpt_only, scenario_b4_6);
define_benchmark_only_tests!(
    bench_b4_7_mainnet_realistic_reth_only,
    bench_b4_7_mainnet_realistic_mpt_only,
    scenario_b4_7
);
