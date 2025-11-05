use clap::command;
use reth_rpc::RpcTypes;

use std::{collections::HashMap, sync::Arc};

use reth_node_core::args::RessArgs;
use reth_rpc_eth_api::{helpers::EthCall, EthApiTypes};

use alloy_primitives::{
    hex::{FromHex, FromHexError},
    FixedBytes, B256,
};

use jsonrpsee::{
    core::{async_trait, RpcResult},
    proc_macros::rpc,
};

use xlayer_db::{
    internal_transaction_inspector::InternalTransaction,
    utils::{read_table_block, read_table_tx},
};

use reth_ethereum::provider::{BlockReader, TransactionsProvider};
use reth_optimism_node::args::RollupArgs;
use serde::{Deserialize, Serialize};
use tokio::{
    select,
    task::spawn_blocking,
    time::{interval, sleep_until, Duration, Instant, MissedTickBehavior},
};

const INNER_TX_TYPE_V2: &str = "rethv2";
const INNER_TX_OK: i8 = 0;
// const INNER_TX_V2_NOT_ACTIVATED: i8 = 1;
const INNER_TX_INVALID_PARAM: i8 = 2;
const INNER_TX_TX_OR_BLOCK_NOT_EXIST: i8 = 3;
const INNER_TX_INTERNAL_ERROR: i8 = 4;
const INNER_TX_TIMEOUT: i8 = 5;

const TX_HASH_LENGTH: usize = 66;
const TIMEOUT_DURATION_S: u64 = 3;
const INTERVAL_DELAY_MS: u64 = 10;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InnerV2Result {
    code: i8,
    r#type: String,
    data: Option<Vec<InternalTransaction>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockInnerV2Result {
    code: i8,
    r#type: String,
    data: Option<HashMap<String, Vec<InternalTransaction>>>,
}

fn string_to_b256(hex_str: String) -> Result<B256, FromHexError> {
    let hex = hex_str.strip_prefix("0x").unwrap_or(&hex_str);
    let fb: FixedBytes<32> = FixedBytes::from_hex(hex)?;
    Ok(B256::from(fb))
}

#[derive(Debug, Clone, Default, clap::Args)]
pub struct CustomExt {
    #[command(flatten)]
    pub ress: RessArgs,
    #[command(flatten)]
    pub rollup_args: RollupArgs,
}

#[rpc(server, namespace = "eth", server_bounds(
    Net: 'static + RpcTypes,                     // Net itself needs no Serde
    <Net as RpcTypes>::TransactionRequest:
        serde::de::DeserializeOwned + serde::Serialize
))]
pub trait XlayerExtApi<Net: RpcTypes> {
    #[method(name = "getInternalTransactionsV2")]
    async fn get_internal_transactions_v2(&self, tx_hash: String) -> RpcResult<InnerV2Result>;

    #[method(name = "getBlockInternalTransactionsV2")]
    async fn get_block_internal_transactions_v2(
        &self,
        block_hash: String,
    ) -> RpcResult<BlockInnerV2Result>;
}

#[derive(Debug)]
pub struct XlayerExt<T> {
    pub backend: Arc<T>,
}

#[async_trait]
impl<T, Net> XlayerExtApiServer<Net> for XlayerExt<T>
where
    T: EthCall + EthApiTypes<NetworkTypes = Net> + Send + Sync + 'static,
    Net: RpcTypes + Send + Sync + 'static,
{
    async fn get_internal_transactions_v2(&self, tx_hash: String) -> RpcResult<InnerV2Result> {
        if tx_hash.len() != TX_HASH_LENGTH || !tx_hash.starts_with("0x") {
            return Ok(InnerV2Result {
                code: INNER_TX_INVALID_PARAM,
                r#type: INNER_TX_TYPE_V2.to_string(),
                data: None,
            });
        }

        let convert_result = string_to_b256(tx_hash);
        if convert_result.is_err() {
            return Ok(InnerV2Result {
                code: INNER_TX_INVALID_PARAM,
                r#type: INNER_TX_TYPE_V2.to_string(),
                data: None,
            });
        }

        let hash = convert_result.unwrap();

        match self.backend.provider().transaction_by_hash(hash) {
            Ok(Some(_)) => {}
            Ok(None) => {
                return Ok(InnerV2Result {
                    code: INNER_TX_TX_OR_BLOCK_NOT_EXIST,
                    r#type: INNER_TX_TYPE_V2.to_string(),
                    data: None,
                })
            }
            Err(_) => {
                return Ok(InnerV2Result {
                    code: INNER_TX_INTERNAL_ERROR,
                    r#type: INNER_TX_TYPE_V2.to_string(),
                    data: None,
                })
            }
        }

        let deadline = Instant::now() + Duration::from_secs(TIMEOUT_DURATION_S);
        let mut tick = interval(Duration::from_millis(INTERVAL_DELAY_MS));
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            let read = spawn_blocking(move || read_table_tx(hash))
                .await
                .map_err(|_| ())
                .and_then(|r| r.map_err(|_| ()));

            match read {
                Ok(result) if !result.is_empty() => {
                    return Ok(InnerV2Result {
                        code: INNER_TX_OK,
                        r#type: INNER_TX_TYPE_V2.into(),
                        data: Some(result),
                    })
                }
                Ok(_) => {}
                Err(_) => {
                    return Ok(InnerV2Result {
                        code: INNER_TX_INTERNAL_ERROR,
                        r#type: INNER_TX_TYPE_V2.into(),
                        data: None,
                    })
                }
            }

            select! {
                _ = tick.tick() => {},
                _ = sleep_until(deadline) => return Ok(InnerV2Result {
                    code: INNER_TX_TIMEOUT,
                    r#type: INNER_TX_TYPE_V2.into(),
                    data: None,
                })
            }
        }
    }

    async fn get_block_internal_transactions_v2(
        &self,
        block_hash: String,
    ) -> RpcResult<BlockInnerV2Result> {
        if block_hash.len() != TX_HASH_LENGTH || !block_hash.starts_with("0x") {
            return Ok(BlockInnerV2Result {
                code: INNER_TX_INVALID_PARAM,
                r#type: INNER_TX_TYPE_V2.to_string(),
                data: None,
            });
        }

        let convert_result = string_to_b256(block_hash);
        if convert_result.is_err() {
            return Ok(BlockInnerV2Result {
                code: INNER_TX_INVALID_PARAM,
                r#type: INNER_TX_TYPE_V2.to_string(),
                data: None,
            });
        }

        let hash = convert_result.unwrap();

        match self.backend.provider().block_by_hash(hash) {
            Ok(Some(_)) => {}
            Ok(None) => {
                return Ok(BlockInnerV2Result {
                    code: INNER_TX_TX_OR_BLOCK_NOT_EXIST,
                    r#type: INNER_TX_TYPE_V2.to_string(),
                    data: None,
                })
            }
            Err(_) => {
                return Ok(BlockInnerV2Result {
                    code: INNER_TX_INTERNAL_ERROR,
                    r#type: INNER_TX_TYPE_V2.to_string(),
                    data: None,
                })
            }
        }

        let deadline = Instant::now() + Duration::from_secs(TIMEOUT_DURATION_S);
        let mut tick = interval(Duration::from_millis(INTERVAL_DELAY_MS));
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let block_txs = loop {
            let read = spawn_blocking(move || read_table_block(hash))
                .await
                .map_err(|_| ())
                .and_then(|r| r.map_err(|_| ()));

            match read {
                Ok(result) if !result.is_empty() => break result,
                Ok(_) => {}
                Err(_) => {
                    return Ok(BlockInnerV2Result {
                        code: INNER_TX_INTERNAL_ERROR,
                        r#type: INNER_TX_TYPE_V2.into(),
                        data: None,
                    })
                }
            }

            select! {
                _ = tick.tick() => {},
                _ = sleep_until(deadline) => return Ok(BlockInnerV2Result {
                    code: INNER_TX_TIMEOUT,
                    r#type: INNER_TX_TYPE_V2.into(),
                    data: None,
                })
            }
        };

        let mut result = HashMap::<String, Vec<InternalTransaction>>::default();

        for tx_hash in block_txs {
            let internal_txs_result = read_table_tx(tx_hash);
            if internal_txs_result.is_err() {
                return Ok(BlockInnerV2Result {
                    code: INNER_TX_INTERNAL_ERROR,
                    r#type: INNER_TX_TYPE_V2.into(),
                    data: None,
                });
            }

            result.insert(tx_hash.to_string(), internal_txs_result.unwrap());
        }

        Ok(BlockInnerV2Result {
            code: INNER_TX_OK,
            r#type: INNER_TX_TYPE_V2.into(),
            data: Some(result),
        })
    }
}
