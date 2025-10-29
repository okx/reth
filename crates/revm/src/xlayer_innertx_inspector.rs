use alloy_primitives::{Address, Bytes, U256};
use revm::{
    context::JournalTr,
    context_interface::ContextTr,
    interpreter::{
        interpreter::EthInterpreter, CallInputs, CallOutcome, CreateInputs, CreateOutcome,
        Interpreter,
    },
    primitives::Log,
    Inspector,
};
use std::collections::HashMap;

/// Inner transaction data structure (equivalent to xlayer-erigon's `InnerTx`)
#[derive(Debug, Clone)]
pub struct InnerTx {
    pub depth: u64,
    pub internal_index: u64,
    pub call_type: String,
    pub name: String,
    pub trace_address: Vec<u64>,
    pub code_address: Option<Address>,
    pub from: Address,
    pub to: Option<Address>,
    pub input: Bytes,
    pub output: Bytes,
    pub is_error: bool,
    pub gas: u64,
    pub gas_used: u64,
    pub value: U256,
    pub value_wei: String,
    pub call_value_wei: String,
    pub error: Option<String>,
}

/// Metadata for tracking inner transactions
#[derive(Debug, Default)]
pub struct InnerTxMeta {
    pub index: u64,
    pub last_depth: u64,
    pub index_map: HashMap<u64, u64>,
    pub inner_txs: Vec<InnerTx>,
}

/// Custom inspector that implements beforeOp/afterOp functionality
#[derive(Debug, Default)]
pub struct InnerTxInspector {
    inner_tx_meta: InnerTxMeta,
    current_depth: u64,
    call_stack: Vec<InnerTx>,
}

impl InnerTxInspector {
    /// Create a new InnerTxInspector
    pub fn new() -> Self {
        Self::default()
    }

    /// Get all collected inner transactions
    pub fn get_inner_txs(&self) -> &[InnerTx] {
        &self.inner_tx_meta.inner_txs
    }

    /// beforeOp equivalent - called before EVM operations
    fn before_op(
        &mut self,
        call_type: &str,
        from: Address,
        to: Option<Address>,
        code_address: Option<Address>,
        input: Bytes,
        gas: u64,
        value: U256,
    ) -> (InnerTx, usize) {
        let mut inner_tx = InnerTx {
            depth: self.current_depth,
            internal_index: 0,
            call_type: call_type.to_string(),
            name: String::new(),
            trace_address: Vec::new(),
            code_address,
            from,
            to,
            input,
            output: Bytes::new(),
            is_error: false,
            gas,
            gas_used: 0,
            value,
            value_wei: value.to_string(),
            call_value_wei: format!("0x{:x}", value),
            error: None,
        };

        // Update index tracking (similar to xlayer-erigon logic)
        if self.current_depth == self.inner_tx_meta.last_depth {
            self.inner_tx_meta.index += 1;
            self.inner_tx_meta.index_map.insert(self.current_depth, self.inner_tx_meta.index);
        } else if self.current_depth < self.inner_tx_meta.last_depth {
            self.inner_tx_meta.index =
                self.inner_tx_meta.index_map.get(&self.current_depth).unwrap_or(&0) + 1;
            self.inner_tx_meta.index_map.insert(self.current_depth, self.inner_tx_meta.index);
            self.inner_tx_meta.last_depth = self.current_depth;
        } else if self.current_depth > self.inner_tx_meta.last_depth {
            self.inner_tx_meta.index = 0;
            self.inner_tx_meta.index_map.insert(self.current_depth, 0);
            self.inner_tx_meta.last_depth = self.current_depth;
        }

        // Build trace address and name
        for i in 1..=self.inner_tx_meta.last_depth {
            if let Some(&idx) = self.inner_tx_meta.index_map.get(&i) {
                inner_tx.trace_address.push(idx);
                inner_tx.name.push_str(&format!("_{}", idx));
            }
        }
        inner_tx.name = format!("{}{}", call_type, inner_tx.name);
        inner_tx.internal_index = self.inner_tx_meta.index;

        // Add to collection
        self.inner_tx_meta.inner_txs.push(inner_tx.clone());
        let new_index = self.inner_tx_meta.inner_txs.len() - 1;

        (inner_tx, new_index)
    }

    /// afterOp equivalent - called after EVM operations
    fn after_op(
        &mut self,
        op_type: &str,
        gas_used: u64,
        new_index: usize,
        inner_tx: &mut InnerTx,
        addr: Option<Address>,
        err: Option<&str>,
        ret: Bytes,
    ) {
        inner_tx.gas_used = gas_used;
        inner_tx.output = ret;

        if let Some(error_msg) = err {
            // Mark all inner txs from this index as errors
            for itx in self.inner_tx_meta.inner_txs.iter_mut().skip(new_index) {
                itx.is_error = true;
            }
            inner_tx.error = Some(error_msg.to_string());
        }

        // Handle specific operation types
        match op_type {
            "create" | "create2" => {
                if let Some(addr) = addr {
                    inner_tx.to = Some(addr);
                }
            }
            _ => {}
        }
    }
}

impl<CTX> Inspector<CTX, EthInterpreter> for InnerTxInspector
where
    CTX: ContextTr,
{
    fn initialize_interp(&mut self, interp: &mut Interpreter, context: &mut CTX) {
        self.current_depth = context.journal().depth() as u64;

        let _ = interp;
        let _ = context;
    }

    fn step(&mut self, interp: &mut Interpreter, context: &mut CTX) {
        let _ = interp;
        let _ = context;
    }

    fn step_end(&mut self, interp: &mut Interpreter, context: &mut CTX) {
        let _ = interp;
        let _ = context;
    }

    fn log(&mut self, interp: &mut Interpreter, context: &mut CTX, log: Log) {
        let _ = interp;
        let _ = context;
        let _ = log;
    }

    fn call(&mut self, context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.current_depth += 1;
        // inputs.scheme; (call/callcode/delegatecall/staticcall)

        let _ = context;
        let _ = inputs;
        None
    }

    fn call_end(&mut self, context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        self.current_depth -= 1;

        let _ = context;
        let _ = inputs;
        let _ = outcome;
    }

    fn create(&mut self, context: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        // inputs.scheme; // (create/create2)
        self.current_depth += 1;

        let _ = context;
        let _ = inputs;
        None
    }

    fn create_end(
        &mut self,
        context: &mut CTX,
        inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        // inputs.scheme; // (create/create2)
        self.current_depth -= 1;

        let _ = context;
        let _ = inputs;
        let _ = outcome;
    }

    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        // NOTE: SUICIDE_TYP = "suicide"
        let _ = contract;
        let _ = target;
        let _ = value;
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use alloy_primitives::{address, bytes, Address, Bytes, U256};
    use reth_chainspec::MAINNET;
    use reth_evm_ethereum::EthEvmConfig;
    use revm::database::CacheDB;
    // use revm::{
    //     context_interface::ContextTr,
    //     interpreter::{
    //         CallInputs, CallOutcome, CallScheme, CreateInputs, CreateOutcome, CreateScheme,
    //         InstructionResult,
    //     },
    // };
    use std::sync::Arc;

    #[test]
    fn test_inner_tx_depth_tracking() {
        // Setup: Create EVM config and database
        let chain_spec = Arc::new(MAINNET.clone());
        let evm_config = EthEvmConfig::new(chain_spec.clone());

        // Create a simple in-memory database
        let mut cache_db = CacheDB::new(revm::database::EmptyDB::default());

        // Setup accounts
        let caller = address!("1000000000000000000000000000000000000001");
        let contract_a = address!("2000000000000000000000000000000000000002");
        let contract_b = address!("3000000000000000000000000000000000000003");
        let contract_c = address!("4000000000000000000000000000000000000004");
    }
}
