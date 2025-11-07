//! XLayer Eth API extension for pre-execution.

use alloy_eips::BlockId;
use alloy_evm::overrides::apply_state_overrides;
use alloy_primitives::{hex, Address, U256};
use alloy_rpc_types_eth::{state::StateOverride, TransactionInfo};
use alloy_rpc_types_trace::geth::call::CallFrame as GethCallFrame;
use alloy_rpc_types_trace::geth::mux::MuxConfig;
use alloy_rpc_types_trace::geth::pre_state::PreStateFrame;
use alloy_rpc_types_trace::geth::{
    CallConfig, GethDebugBuiltInTracerType, GethDebugTracerConfig, PreStateConfig,
};
use jsonrpsee::core::RpcResult;
use op_alloy_rpc_types::OpTransactionRequest;
use reth_rpc_eth_api::{
    ext_xlayer::XLayerEthApiExtServer,
    helpers::{Call, LoadState, SpawnBlocking, Trace},
    EthApiTypes, RpcTypes,
};
use reth_rpc_eth_types::pre_exec_xlayer::{PreExecError, PreExecInnerTx, PreExecResult};
use revm::context_interface::result::ExecutionResult;
use revm::DatabaseCommit;
use revm_inspectors::tracing::MuxInspector;
use serde_json::{Map as JsonMap, Value as JsonValue};

/// Maximum gas limit for pre-execution
const MAX_GAS_LIMIT: u64 = 30_000_000;

/// XLayer Eth API extension implementation.
///
/// Wraps OpEthApi to provide transaction pre-execution functionality.
#[derive(Clone, Debug)]
pub struct XLayerEthApiExt<EthApi> {
    eth_api: EthApi,
}

impl<EthApi> XLayerEthApiExt<EthApi> {
    /// Creates a new [`XLayerEthApiExt`].
    pub fn new(eth_api: EthApi) -> Self {
        Self { eth_api }
    }
}

#[async_trait::async_trait]
impl<EthApi> XLayerEthApiExtServer<OpTransactionRequest> for XLayerEthApiExt<EthApi>
where
    EthApi: Call + LoadState + SpawnBlocking + Trace + EthApiTypes + Clone + Send + Sync + 'static,
    <EthApi as EthApiTypes>::NetworkTypes: RpcTypes<TransactionRequest = OpTransactionRequest>,
{
    async fn transaction_pre_exec(
        &self,
        args: Vec<OpTransactionRequest>,
        block_number: Option<BlockId>,
        state_overrides: Option<StateOverride>,
    ) -> RpcResult<Vec<PreExecResult>> {
        let block_id = block_number.unwrap_or_default();
        let (evm_env, at) = match self.eth_api.evm_env_at(block_id).await {
            Ok(env) => env,
            Err(e) => {
                return Err(e.into());
            }
        };

        let this = self.eth_api.clone();
        self.eth_api
            .spawn_with_state_at_block(at, move |state| {
                let mut db = reth_revm::db::CacheDB::new(
                    reth_revm::database::StateProviderDatabase::new(state),
                );
                let mut results: Vec<PreExecResult> = Vec::with_capacity(args.len());

                if let Some(overrides) = state_overrides {
                    if let Err(e) = apply_state_overrides(overrides, &mut db) {
                        results.push(
                            PreExecError::unknown(format!("state override error: {:?}", e))
                                .into_result(0, evm_env.block_env.number),
                        );
                        return Ok(results);
                    }
                }

                let mut prev: Option<OpTransactionRequest> = None;
                for mut tx_req in args {
                    match pre_args_check(&mut db, &tx_req, prev.as_ref(), results.len()) {
                        Ok(corrected_gas) => {
                            tx_req.as_mut().gas = Some(corrected_gas);
                        }
                        Err(err) => {
                            results.push(err.into_result(0, evm_env.block_env.number));
                            prev = Some(tx_req);
                            continue;
                        }
                    }
                    let current_req_for_next = tx_req.clone();

                    let (current_evm_env, tx_env) = match this.prepare_call_env(
                        evm_env.clone(),
                        tx_req,
                        &mut db,
                        Default::default(),
                    ) {
                        Ok(v) => v,
                        Err(e) => {
                            results.push(
                                PreExecError::unknown(e.to_string())
                                    .into_result(0, evm_env.block_env.number),
                            );
                            prev = Some(current_req_for_next);
                            continue;
                        }
                    };

                    let mux_config = MuxConfig(alloy_primitives::map::HashMap::from_iter([
                        (
                            GethDebugBuiltInTracerType::CallTracer,
                            Some(GethDebugTracerConfig::from(CallConfig::default())),
                        ),
                        (
                            GethDebugBuiltInTracerType::PreStateTracer,
                            Some(GethDebugTracerConfig::from(PreStateConfig {
                                diff_mode: Some(true),
                                disable_code: None,
                                disable_storage: None,
                            })),
                        ),
                    ]));

                    let mut inspector = match MuxInspector::try_from_config(mux_config) {
                        Ok(i) => i,
                        Err(e) => {
                            results.push(
                                PreExecError::unknown(e.to_string())
                                    .into_result(0, evm_env.block_env.number),
                            );
                            prev = Some(current_req_for_next);
                            continue;
                        }
                    };

                    let exec = match this.inspect(
                        &mut db,
                        current_evm_env.clone(),
                        tx_env,
                        &mut inspector,
                    ) {
                        Ok(v) => v,
                        Err(e) => {
                            results.push(
                                classify_error(e.to_string())
                                    .into_result(0, evm_env.block_env.number),
                            );
                            prev = Some(current_req_for_next);
                            continue;
                        }
                    };

                    let gas_used = exec.result.gas_used();
                    // resolve block hash from `at` if available
                    let block_hash_opt = match at.clone() {
                        BlockId::Hash(h) => Some(h.block_hash.into()),
                        _ => None,
                    };
                    let tx_idx = results.len() as u64;
                    let mut pre_exec_res = match process_tracer_results(
                        exec.clone(),
                        inspector,
                        &db,
                        current_evm_env.block_env.number,
                        block_hash_opt,
                        tx_idx,
                    ) {
                        Ok(r) => r,
                        Err(e) => {
                            results.push(e.into_result(gas_used, current_evm_env.block_env.number));
                            prev = Some(current_req_for_next);
                            continue;
                        }
                    };

                    pre_exec_res.error = match exec.result {
                        ExecutionResult::Success { .. } => PreExecError::default(),
                        ExecutionResult::Revert { output, .. } => PreExecError::reverted(format!(
                            "execution reverted: 0x{}",
                            hex::encode(output)
                        )),
                        ExecutionResult::Halt { reason, .. } => {
                            classify_error(format!("{:?}", reason))
                        }
                    };

                    db.commit(exec.state.clone());
                    results.push(pre_exec_res);
                    prev = Some(current_req_for_next);
                }

                Ok(results)
            })
            .await
            .map_err(|e| {
                jsonrpsee::types::ErrorObjectOwned::owned(
                    jsonrpsee::types::error::INTERNAL_ERROR_CODE,
                    e.to_string(),
                    None::<()>,
                )
            })
    }
}

/// Process tracer results into PreExecResult using typed mux output
fn process_tracer_results<DB, HR>(
    exec: revm::context_interface::result::ResultAndState<HR>,
    inspector: MuxInspector,
    db: &DB,
    block_number: U256,
    block_hash: Option<alloy_primitives::B256>,
    tx_index: u64,
) -> Result<PreExecResult, PreExecError>
where
    DB: revm::DatabaseRef,
    HR: std::fmt::Debug + revm::context_interface::result::HaltReasonTr,
{
    let tx_info = TransactionInfo {
        hash: None,
        index: None,
        block_hash: None,
        block_number: Some(block_number.saturating_to()),
        base_fee: Some(0),
    };
    let mux_frame = inspector
        .try_into_mux_frame(&exec, db, tx_info)
        .map_err(|e| PreExecError::unknown(e.to_string()))?;

    // inner txs
    let inner_txs = if let Some(alloy_rpc_types_trace::geth::GethTrace::CallTracer(call_frame)) =
        mux_frame.0.get(&GethDebugBuiltInTracerType::CallTracer)
    {
        let v = convert_call_tracer_to_inner_txs(call_frame)?;
        serde_json::to_value(&v).unwrap_or_default()
    } else {
        JsonValue::Array(vec![])
    };

    // state diff
    let state_diff = if let Some(alloy_rpc_types_trace::geth::GethTrace::PreStateTracer(
        PreStateFrame::Diff(diff),
    )) = mux_frame.0.get(&GethDebugBuiltInTracerType::PreStateTracer)
    {
        // build balance-only diff map as JSON
        let mut out = JsonMap::new();
        for (addr, post) in &diff.post {
            if let Some(pre) = diff.pre.get(addr) {
                let pre_bal = pre.balance.unwrap_or_default().to_string();
                let post_bal = post.balance.unwrap_or_default().to_string();
                let mut bal = JsonMap::new();
                bal.insert("before".into(), JsonValue::String(pre_bal));
                bal.insert("after".into(), JsonValue::String(post_bal));
                let mut addr_obj = JsonMap::new();
                addr_obj.insert("balance".into(), JsonValue::Object(bal));
                out.insert(addr.to_checksum(None), JsonValue::Object(addr_obj));
            }
        }
        JsonValue::Object(out)
    } else {
        JsonValue::Object(JsonMap::new())
    };

    // gas used
    let gas_used = exec.result.gas_used();

    // get logs
    let logs = {
        let log_vec = exec.result.logs();
        let tx_hash =
            alloy_primitives::B256::from(alloy_primitives::U256::from(tx_index).to_be_bytes());
        let rpc_logs: Vec<alloy_rpc_types_eth::Log> = log_vec
            .iter()
            .enumerate()
            .map(|(idx, log)| alloy_rpc_types_eth::Log {
                inner: log.clone(),
                block_hash,
                block_number: Some(
                    block_number.saturating_sub(alloy_primitives::U256::from(1)).saturating_to(),
                ),
                transaction_hash: Some(tx_hash),
                transaction_index: Some(tx_index),
                log_index: Some(idx as u64),
                ..Default::default()
            })
            .collect();
        serde_json::to_value(rpc_logs).unwrap_or_default()
    };

    Ok(PreExecResult {
        inner_txs: Some(inner_txs),
        logs: Some(logs),
        state_diff: Some(state_diff),
        error: PreExecError::default(),
        gas_used,
        block_number,
    })
}

/// Convert typed CallFrame to inner txs
fn convert_call_tracer_to_inner_txs(
    call_frame: &GethCallFrame,
) -> Result<Vec<PreExecInnerTx>, PreExecError> {
    let mut inner_txs = Vec::new();
    convert_call_frame_recursive(call_frame, &mut inner_txs, 0, 0, "".to_string(), false);
    // filter: only return when deep calls or failed
    let has_deep = inner_txs.iter().any(|it| it.dept > U256::from(0));
    let has_failed = inner_txs.iter().any(|it| it.is_error || !it.error.is_empty());
    if !(has_deep || has_failed) {
        return Ok(Vec::new());
    }
    Ok(inner_txs)
}

fn convert_call_frame_recursive(
    frame: &GethCallFrame,
    out: &mut Vec<PreExecInnerTx>,
    depth: i64,
    index: i64,
    depth_index_root: String,
    parent_error: bool,
) {
    let mut is_error = parent_error;
    let mut error_msg = String::new();
    if let Some(err) = &frame.error {
        is_error = true;
        error_msg = err.clone();
    }

    let gas_used = frame.gas_used.saturating_to::<u64>();
    let gas = frame.gas.saturating_to::<u64>();
    let return_gas = gas.saturating_sub(gas_used);

    let output = frame.output.as_ref().map(|b| format!("{:?}", b)).unwrap_or_else(|| "0x".into());
    let value_wei = frame.value.map(|v| v.to_string()).unwrap_or_else(|| "0".into());

    let mut inner = PreExecInnerTx {
        dept: U256::from(depth as u64),
        internal_index: U256::from(index as u64),
        call_type: frame.typ.to_string().to_lowercase(),
        name: String::new(),
        trace_address: String::new(),
        code_address: String::new(),
        from: format!("{:?}", frame.from),
        to: frame.to.map(|a| format!("{:?}", a)).unwrap_or_default(),
        input: format!("{:?}", frame.input),
        gas_used,
        output,
        is_error,
        value: value_wei.clone(),
        value_wei,
        error: error_msg,
        return_gas,
    };

    if depth == 0 {
        inner.name = inner.call_type.clone();
    } else {
        if let Some(stripped) = inner.from.strip_prefix("0x") {
            inner.from = format!("0x000000000000000000000000{}", stripped);
        }
        if let Some(stripped) = inner.to.strip_prefix("0x") {
            inner.to = format!("0x000000000000000000000000{}", stripped);
        }
        if inner.call_type == "callcode" {
            inner.code_address = frame.to.map(|a| format!("{:?}", a)).unwrap_or_default();
        }
        let current_root = if depth_index_root.is_empty() {
            format!("_{}", index)
        } else {
            format!("{}_{}", depth_index_root, index)
        };
        inner.name = format!("{}{}", inner.call_type, current_root);
    }

    out.push(inner);

    if !frame.calls.is_empty() {
        let next_root =
            if depth == 0 { String::new() } else { format!("{}_{}", depth_index_root, index) };
        for (i, nested) in frame.calls.iter().enumerate() {
            let parent_err = out.last().map(|x| x.is_error).unwrap_or(false);
            convert_call_frame_recursive(
                nested,
                out,
                depth + 1,
                i as i64,
                next_root.clone(),
                parent_err,
            );
        }
    }
}

fn pre_args_check<DB>(
    db: &mut DB,
    tx: &OpTransactionRequest,
    prev: Option<&OpTransactionRequest>,
    index: usize,
) -> Result<u64, PreExecError>
where
    DB: revm::Database,
    <DB as revm::Database>::Error: core::fmt::Debug,
{
    let req = tx.as_ref();
    let from: Address = req.from.ok_or_else(|| PreExecError::check_args("from is nil"))?;
    if req.to.is_none() {
        return Err(PreExecError::check_args("to is nil"));
    }

    let msg_nonce =
        req.nonce.ok_or_else(|| PreExecError::check_args(format!("{}, nonce is nil", from)))?;

    if let Some(prev_req) = prev {
        if let (Some(pf), Some(pn)) = (prev_req.as_ref().from, prev_req.as_ref().nonce) {
            if pf == from && msg_nonce <= pn {
                return Err(PreExecError::check_args(format!(
                    "{} nonce decreases, tx index {} has nonce {}, tx index {} has nonce {}",
                    from,
                    index.saturating_sub(1),
                    pn,
                    index,
                    msg_nonce
                )));
            }
        }
    }

    let st_nonce = db
        .basic(from)
        .map_err(|e| PreExecError::unknown(format!("db error: {:?}", e)))?
        .map(|acc| acc.nonce)
        .unwrap_or(0);

    if st_nonce > msg_nonce {
        return Err(PreExecError::check_args(format!(
            "nonce too low: address {}, tx: {} state: {}",
            from, msg_nonce, st_nonce
        )));
    } else if st_nonce.checked_add(1).is_none() {
        return Err(PreExecError::check_args(format!(
            "nonce has max value: address {}, nonce: {}",
            from, st_nonce
        )));
    }

    let corrected_gas = match req.gas {
        Some(g) => {
            if g == 0 || g > MAX_GAS_LIMIT {
                MAX_GAS_LIMIT
            } else {
                g
            }
        }
        None => MAX_GAS_LIMIT,
    };
    Ok(corrected_gas)
}

/// Classify error message to appropriate error type
fn classify_error(msg: String) -> PreExecError {
    let lower = msg.to_lowercase();
    match () {
        _ if lower.contains("out of gas") => PreExecError::reverted("out of gas"),
        _ if lower.contains("insufficient funds") || lower.contains("insufficient balance") => {
            PreExecError::insufficient_balance(msg)
        }
        _ => PreExecError::unknown(msg),
    }
}
