//! `XLayer` inner-transaction inspector.
//!
//! This module defines `InnerTxInspector`, a custom inspector built on top of
//! `revm`'s `Inspector` trait that records inner transactions (calls and
//! creates) observed during EVM execution. It tracks:
//!
//! - Call/create depth and ordering
//! - Trace addresses (per-depth indices) similar to Erigon's `InnerTx`
//! - Call type (call, delegatecall, staticcall, callcode, create, create2)
//! - From/to/code addresses, input/output, gas and gas used
//! - Error propagation for failing subcalls
//!
//! The collected entries are exposed via `get_inner_txs()` for downstream use
//! (e.g., RPC trace-like responses or analytics).
//!
//! Integration notes:
//! - Reth uses `alloy-evm` as a higher-level facade but executes via `revm`. This inspector can be
//!   provided when constructing the EVM using `evm_with_env_and_inspector(...)` so it runs during
//!   transaction and block execution.
//! - See `examples/custom-inspector` for an example of wiring an inspector into RPC execution
//!   paths.
//!
//! This implementation mirrors parts of xlayer-erigon's inner-tx semantics to
//! ease compatibility with existing tooling.

use alloy_primitives::{Address, Bytes, U256};
use revm::{
    context::CreateScheme,
    context_interface::ContextTr,
    interpreter::{
        interpreter::EthInterpreter, CallInputs, CallOutcome, CallScheme, CreateInputs,
        CreateOutcome, Interpreter,
    },
    primitives::Log,
    Inspector,
};
use std::collections::HashMap;

/// Trait for converting call/create schemes to their string representation.
trait AsSchemeStr {
    fn as_str(&self) -> &'static str;
}

impl AsSchemeStr for CallScheme {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Call => "call",
            Self::CallCode => "callcode",
            Self::DelegateCall => "delegatecall",
            Self::StaticCall => "staticcall",
        }
    }
}

impl AsSchemeStr for CreateScheme {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Create2 { .. } => "create2",
            Self::Custom { .. } => "custom",
        }
    }
}

/// Inner transaction data structure (equivalent to xlayer-erigon's `InnerTx`)
#[derive(Debug, Clone)]
pub struct InnerTx {
    /// Call depth in the execution stack (0 = top-level transaction)
    pub depth: u64,
    /// Internal index within the current depth level
    pub internal_index: u64,
    /// Type of call/create operation (e.g., "call", "delegatecall", "create", "create2")
    pub call_type: String,
    /// Human-readable name constructed from call type and trace address indices
    pub name: String,
    /// Trace address representing the path in the execution tree
    pub trace_address: Vec<u64>,
    /// Address of the code being executed (may differ from `to` for delegatecall)
    pub code_address: Option<Address>,
    /// Sender address of the inner transaction
    pub from: Address,
    /// Recipient address (None for CREATE operations until address is known)
    pub to: Option<Address>,
    /// Input data passed to the call/create
    pub input: Bytes,
    /// Output data returned from the call/create
    pub output: Bytes,
    /// Whether the operation resulted in an error
    pub is_error: bool,
    /// Gas limit allocated for the operation
    pub gas: u64,
    /// Actual gas used by the operation
    pub gas_used: u64,
    /// Value transferred (in wei)
    pub value: U256,
    /// Value transferred as a decimal string
    pub value_wei: String,
    /// Call value as a hexadecimal string with 0x prefix
    pub call_value_wei: String,
    /// Error message if the operation failed, None otherwise
    pub error: Option<String>,
}

/// Metadata for tracking inner transactions
#[derive(Debug, Default)]
pub(crate) struct InnerTxMeta {
    /// Current index counter for tracking inner transactions at each depth
    pub index: u64,
    /// The deepest call depth encountered so far
    pub last_depth: u64,
    /// Mapping of depth level to its current index counter
    pub index_map: HashMap<u64, u64>,
    /// Collected inner transactions in execution order
    pub inner_txs: Vec<InnerTx>,
}

/// Custom inspector that implements beforeOp/afterOp functionality
#[derive(Debug, Default)]
pub struct InnerTxInspector {
    /// Metadata for tracking and indexing inner transactions
    inner_tx_meta: InnerTxMeta,
    /// Current call depth in the execution stack
    current_depth: u64,
    /// Stack of pending inner transactions with their collection indices
    call_stack: Vec<(InnerTx, usize)>,
}

// Public
impl InnerTxInspector {
    /// Create a new `InnerTxInspector`
    pub fn new() -> Self {
        Self::default()
    }

    /// Get all collected inner transactions
    pub fn get_inner_txs(&self) -> &[InnerTx] {
        &self.inner_tx_meta.inner_txs
    }
}

// Private
impl InnerTxInspector {
    /// beforeOp equivalent - called before EVM operations
    #[allow(clippy::too_many_arguments)]
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
        inner_tx.internal_index = self.inner_tx_meta.index;

        // Build trace address and name
        for i in 1..=self.inner_tx_meta.last_depth {
            if let Some(&idx) = self.inner_tx_meta.index_map.get(&i) {
                inner_tx.trace_address.push(idx);
                inner_tx.name.push_str(&format!("_{}", idx));
            }
        }
        inner_tx.name = format!("{}{}", call_type, inner_tx.name);

        // Add to collection and get the index (before push: len, after push: len - 1)
        let new_index = self.inner_tx_meta.inner_txs.len();
        self.inner_tx_meta.inner_txs.push(inner_tx.clone());

        (inner_tx, new_index)
    }

    /// afterOp equivalent - called after EVM operations
    #[allow(clippy::too_many_arguments)]
    fn after_op(
        &mut self,
        op_type: &str,
        gas_used: u64,
        new_index: u64,
        inner_tx: &mut InnerTx,
        addr: Option<Address>,
        err: Option<&str>,
        ret: Bytes,
    ) {
        inner_tx.gas_used = gas_used;
        inner_tx.output = ret;

        if let Some(error_msg) = err {
            // Mark all inner txs from this index as errors
            for itx in self.inner_tx_meta.inner_txs.iter_mut().skip(new_index as usize) {
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

    fn call(&mut self, inputs: &CallInputs, input_bytes: Bytes) {
        // Determine call type from scheme
        let call_type = inputs.scheme.as_str();
        // Get transfer value (None for static calls)
        let value = inputs.transfer_value().unwrap_or(U256::ZERO);
        // Create inner transaction record
        let (inner_tx, new_index) = self.before_op(
            call_type,
            inputs.caller,
            Some(inputs.target_address),
            Some(inputs.bytecode_address),
            input_bytes,
            inputs.gas_limit,
            value,
        );
        // Push to stack with index for later retrieval in call_end
        self.call_stack.push((inner_tx, new_index));
    }

    fn call_end(&mut self, inputs: &CallInputs, outcome: &CallOutcome) {
        // Pop the corresponding call from stack
        if let Some((mut inner_tx, new_index)) = self.call_stack.pop() {
            let call_type = inputs.scheme.as_str();
            let gas_used = inputs.gas_limit - outcome.result.gas.remaining();
            let error = outcome.result.is_error().then(|| format!("{:?}", outcome.result));

            self.after_op(
                call_type,
                gas_used,
                new_index as u64,
                &mut inner_tx,
                None,
                error.as_deref(),
                outcome.result.output.clone(),
            );
        }

        self.current_depth -= 1;
    }

    fn create(&mut self, inputs: &CreateInputs) {
        self.current_depth += 1;

        // Determine create type from scheme
        let create_type = inputs.scheme.as_str();

        // Create inner transaction record
        let (inner_tx, new_index) = self.before_op(
            create_type,
            inputs.caller,
            None, // CREATE doesn't have a 'to' address initially
            None, // CREATE doesn't have a code_address
            inputs.init_code.clone(),
            inputs.gas_limit,
            inputs.value,
        );

        // Push to stack with index for later retrieval in create_end
        self.call_stack.push((inner_tx, new_index));
    }

    fn create_end(&mut self, inputs: &CreateInputs, outcome: &CreateOutcome) {
        // Pop the corresponding create from stack
        if let Some((mut inner_tx, new_index)) = self.call_stack.pop() {
            let create_type = inputs.scheme.as_str();
            let gas_used = inputs.gas_limit - outcome.result.gas.remaining();
            let error =
                outcome.result.result.is_error().then(|| format!("{:?}", outcome.result.result));

            self.after_op(
                create_type,
                gas_used,
                new_index as u64,
                &mut inner_tx,
                outcome.address, // CREATE operations return the new contract address
                error.as_deref(),
                outcome.result.output.clone(),
            );
        }

        self.current_depth -= 1;
    }

    fn handle_selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        // SELFDESTRUCT doesn't change depth - it happens within current call frame
        let call_type = "suicide";

        // Create inner transaction record for selfdestruct
        let (mut inner_tx, new_index) = self.before_op(
            call_type,
            contract,
            Some(target),
            None,
            Bytes::new(),
            0, // selfdestruct uses remaining gas from current context
            value,
        );

        // Immediately finalize (no _end hook for selfdestruct)
        self.after_op(
            call_type,
            0, // gas_used recorded at transaction level
            new_index as u64,
            &mut inner_tx,
            None,
            None,
            Bytes::new(),
        );
    }
}

impl<CTX> Inspector<CTX, EthInterpreter> for InnerTxInspector
where
    CTX: ContextTr,
{
    fn initialize_interp(&mut self, interp: &mut Interpreter, context: &mut CTX) {
        self.current_depth = 1;

        let _ = interp;
        let _ = context;
    }

    // Ignore
    fn step(&mut self, interp: &mut Interpreter, context: &mut CTX) {
        let _ = interp;
        let _ = context;
    }

    // Ignore
    fn step_end(&mut self, interp: &mut Interpreter, context: &mut CTX) {
        let _ = interp;
        let _ = context;
    }

    // Ignore
    fn log(&mut self, interp: &mut Interpreter, context: &mut CTX, log: Log) {
        let _ = interp;
        let _ = context;
        let _ = log;
    }

    fn call(&mut self, context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.current_depth += 1;
        let input_bytes = inputs.input.bytes(context);
        self.call(inputs, input_bytes);

        None
    }

    fn call_end(&mut self, context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        self.call_end(inputs, outcome);

        let _ = context;
    }

    fn create(&mut self, context: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        self.create(inputs);

        let _ = context;
        None
    }

    fn create_end(
        &mut self,
        context: &mut CTX,
        inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        self.create_end(inputs, outcome);
        let _ = context;
    }

    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        self.handle_selfdestruct(contract, target, value);
    }
}
