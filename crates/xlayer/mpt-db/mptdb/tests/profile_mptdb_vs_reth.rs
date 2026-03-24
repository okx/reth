//! Lightweight one-shot profile runners for B4.1-B4.7.
//!
//! Run with:
//! `PROTOC=/Users/louisliuxiong/golang/bin/protoc cargo test -p mptdb --release --test
//! profile_mptdb_vs_reth profile_b4_7_mainnet_realistic_mpt_only -- --ignored --nocapture --exact`

mod common;

use common::mptdb_vs_reth_support::{
    run_profile_compare, run_profile_mpt_only, run_profile_reth_only, scenario_b4_1, scenario_b4_2,
    scenario_b4_3, scenario_b4_4, scenario_b4_5, scenario_b4_6, scenario_b4_7,
};

macro_rules! define_profile_only_tests {
    ($reth_name:ident, $mpt_name:ident, $scenario_fn:ident) => {
        #[test]
        #[ignore]
        fn $reth_name() {
            run_profile_reth_only($scenario_fn());
        }

        #[test]
        #[ignore]
        fn $mpt_name() {
            run_profile_mpt_only($scenario_fn());
        }
    };
}

#[test]
#[ignore]
fn profile_b4_4_single_run_compare() {
    run_profile_compare(scenario_b4_4());
}

#[test]
#[ignore]
fn profile_b4_5_single_run_compare() {
    run_profile_compare(scenario_b4_5());
}

#[test]
#[ignore]
fn profile_b4_6_single_run_compare() {
    run_profile_compare(scenario_b4_6());
}

define_profile_only_tests!(profile_b4_1_reth_only, profile_b4_1_mpt_only, scenario_b4_1);
define_profile_only_tests!(profile_b4_2_reth_only, profile_b4_2_mpt_only, scenario_b4_2);
define_profile_only_tests!(profile_b4_3_reth_only, profile_b4_3_mpt_only, scenario_b4_3);
define_profile_only_tests!(profile_b4_4_reth_only, profile_b4_4_mpt_only, scenario_b4_4);
define_profile_only_tests!(profile_b4_5_reth_only, profile_b4_5_mpt_only, scenario_b4_5);
define_profile_only_tests!(profile_b4_6_reth_only, profile_b4_6_mpt_only, scenario_b4_6);
define_profile_only_tests!(
    profile_b4_7_mainnet_realistic_reth_only,
    profile_b4_7_mainnet_realistic_mpt_only,
    scenario_b4_7
);
