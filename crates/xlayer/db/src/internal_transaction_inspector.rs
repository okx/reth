//! XLayer Internal Transaction Inspector
//!
//! This module provides `TraceCollector`, an EVM inspector that traces internal transactions
//! (CALL, CREATE, SELFDESTRUCT operations) during EVM execution.
//!
//! # Implementation Note
//!
//! Due to Rust's type system limitations (lack of specialization), `TraceCollector` provides
//! a generic `Inspector` implementation. For `CallInput::SharedBuffer`, the implementation
//! uses `ContextTr::local().shared_memory_buffer_slice()` when the context implements `ContextTr`.
//!
//! In practice, all EVM contexts in reth (including `OpContext`) implement `ContextTr`,
//! so the shared memory buffer is always accessible correctly.

use alloy_rlp::{RlpDecodable, RlpEncodable};
use serde::{Deserialize, Serialize};
use std::{cell::Ref, cmp::Ordering};

use crate::utils;
use alloy_primitives::{Address, Bytes, U256};

use reth_revm::{
    context_interface::{ContextTr, LocalContextTr},
    interpreter::{
        interpreter::EthInterpreter, CallInput, CallInputs, CallOutcome, CreateInputs,
        CreateOutcome, InstructionResult,
    },
    Inspector,
};

/// Represents a single internal transaction within an EVM execution.
///
/// Internal transactions include CALL, CALLCODE, DELEGATECALL, STATICCALL,
/// CREATE, CREATE2, and SELFDESTRUCT operations.
#[derive(Debug, Clone, Default, RlpEncodable, RlpDecodable, Serialize, Deserialize)]
pub struct InternalTransaction {
    /// Call depth (0 = top-level call)
    dept: u64,
    /// Index within the current depth level
    internal_index: u64,
    /// Type of call (call, callcode, delegatecall, staticcall, create, create2, selfdestruct)
    call_type: String,
    /// Unique name combining call type and trace path (e.g., "call_0_1")
    name: String,
    /// For delegatecall, stores the original caller address
    trace_address: String,
    /// For callcode, stores the code address
    code_address: String,
    /// Caller address
    from: String,
    /// Target address (or created contract address for CREATE)
    to: String,
    /// Call input data
    input: Bytes,
    /// Call output data
    output: Bytes,
    /// Whether the call reverted or failed
    is_error: bool,
    /// Gas limit for this call
    gas: u64,
    /// Actual gas consumed
    gas_used: u64,
    /// Value transferred (legacy format)
    value: String,
    /// Value transferred in wei (decimal string)
    value_wei: String,
    /// Value transferred in wei (hex format)
    call_value_wei: String,
    /// Error message if the call failed
    error: String,
}

impl InternalTransaction {
    /// Sets the gas limit and gas used for the transaction.
    pub fn set_transaction_gas(&mut self, gas_limit: u64, gas_used: u64) {
        self.gas = gas_limit;
        self.gas_used = gas_used;
    }
}

/// `TraceCollector` is an EVM inspector that collects internal transaction traces.
///
/// It records all CALL, CREATE, and SELFDESTRUCT operations during EVM execution,
/// building a tree of internal transactions that can be used for debugging,
/// analytics, or indexing purposes.
///
/// # XLayer Feature
///
/// This inspector is part of XLayer's internal transaction tracking functionality.
/// It can be enabled/disabled via the `is_inner_tx_enabled()` configuration.
#[derive(Debug, Clone)]
pub struct TraceCollector {
    /// Whether tracing is enabled
    enabled: bool,
    /// All completed transaction traces (one per top-level tx)
    all_traces: Vec<Vec<InternalTransaction>>,
    /// Current transaction's traces being built
    traces: Vec<InternalTransaction>,
    /// Current call path (indices at each depth level)
    current_path: Vec<usize>,
    /// Last observed depth (for sibling tracking)
    last_depth: usize,
    /// Count of siblings at each depth level
    sibling_count: Vec<usize>,
    /// Stack of trace indices (for matching call/call_end)
    trace_stack: Vec<usize>,
}

impl Default for TraceCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl TraceCollector {
    /// Create a new `TraceCollector` with the enabled state from global configuration.
    pub fn new() -> Self {
        Self {
            enabled: utils::is_inner_tx_enabled(),
            all_traces: Vec::<Vec<InternalTransaction>>::default(),
            traces: Vec::<InternalTransaction>::default(),
            current_path: Vec::<usize>::default(),
            last_depth: 0,
            sibling_count: vec![0],
            trace_stack: Vec::<usize>::default(),
        }
    }

    /// Create a new `TraceCollector` with an explicit enabled state.
    pub fn with_enabled(enabled: bool) -> Self {
        Self {
            enabled,
            all_traces: Vec::<Vec<InternalTransaction>>::default(),
            traces: Vec::<InternalTransaction>::default(),
            current_path: Vec::<usize>::default(),
            last_depth: 0,
            sibling_count: vec![0],
            trace_stack: Vec::<usize>::default(),
        }
    }

    /// Returns whether this collector is enabled.
    #[inline]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Formats an EVM instruction result into a human-readable error message.
    fn format_error(result: &InstructionResult) -> String {
        match result {
            InstructionResult::Revert => "execution reverted".to_string(),
            InstructionResult::CallTooDeep => "max call depth exceeded".to_string(),
            InstructionResult::OutOfGas => "out of gas".to_string(),
            InstructionResult::NonceOverflow => "nonce uint64 overflow".to_string(),
            InstructionResult::InvalidJump => "invalid jump destination".to_string(),
            InstructionResult::CreateCollision => "contract address collision".to_string(),
            InstructionResult::OutOfFunds => "insufficient balance for transfer".to_string(),
            InstructionResult::CreateInitCodeSizeLimit => "max initcode size exceeded".to_string(),
            InstructionResult::OpcodeNotFound => "invalid opcode".to_string(),
            InstructionResult::ReentrancySentryOOG => {
                "not enough gas for reentrancy sentry".to_string()
            }
            InstructionResult::StackUnderflow => "stack underflow".to_string(),
            InstructionResult::StackOverflow => "stack overflow".to_string(),
            InstructionResult::CreateInitCodeStartingEF00 => {
                "CREATE/CREATE2 starts with 0xEF00".to_string()
            }
            InstructionResult::InvalidEOFInitCode => {
                "invalid EVM Object Format (EOF) init code".to_string()
            }
            InstructionResult::InvalidExtDelegateCallTarget => {
                "extDelegateCall calling a non EOF contract".to_string()
            }
            InstructionResult::MemoryOOG => {
                "out of gas error encountered during memory expansion".to_string()
            }
            InstructionResult::MemoryLimitOOG => {
                "the memory limit of the EVM has been exceeded".to_string()
            }
            InstructionResult::PrecompileOOG => {
                "out of gas error encountered during the execution of a precompiled contract"
                    .to_string()
            }
            InstructionResult::InvalidOperandOOG => {
                "out of gas error encountered while calling an invalid operand".to_string()
            }
            InstructionResult::CallNotAllowedInsideStatic => {
                "invalid CALL with value transfer in static context".to_string()
            }
            InstructionResult::StateChangeDuringStaticCall => {
                "invalid state modification in static call".to_string()
            }
            InstructionResult::InvalidFEOpcode => {
                "an undefined bytecode value encountered during execution".to_string()
            }
            InstructionResult::NotActivated => {
                "the feature or opcode is not activated in this version of the EVM".to_string()
            }
            InstructionResult::OutOfOffset => "invalid memory or storage offset".to_string(),
            InstructionResult::OverflowPayment => "payment amount overflow".to_string(),
            InstructionResult::PrecompileError => {
                "error in precompiled contract execution".to_string()
            }
            InstructionResult::CreateContractSizeLimit => {
                "exceeded contract size limit during creation".to_string()
            }
            InstructionResult::CreateContractStartingWithEF => {
                "created contract starts with invalid bytes 0xEF".to_string()
            }
            InstructionResult::FatalExternalError => "fatal external error".to_string(),
            _ => format!("{result:?}"),
        }
    }

    /// Initializes a new internal operation trace.
    fn init_op(
        &mut self,
        call_type: String,
        from: String,
        to: String,
        input: Bytes,
        value_wei: String,
        gas_limit: u64,
        code_address: String,
    ) {
        if !self.enabled {
            return;
        }
        let mut txn = InternalTransaction::default();
        txn.call_type = call_type;
        txn.from = from.clone();
        txn.input = input;
        txn.is_error = false;
        txn.gas = gas_limit;
        txn.value_wei = if value_wei.is_empty() { "0" } else { &value_wei }.to_string();
        txn.call_value_wei = match value_wei.parse::<u128>() {
            Ok(value) => format!("0x{value:x}"),
            _ => String::from("0x0"),
        };

        txn.to = to.clone();
        match txn.call_type.as_str() {
            "delegatecall" => {
                txn.from = to;
                txn.to = code_address;
                txn.trace_address = txn.from.clone();
            }
            "callcode" => {
                txn.code_address = code_address;
            }
            _ => {}
        }

        self.traces.push(txn);
    }

    /// Called before processing an operation to set up depth tracking.
    fn before_op(&mut self) {
        if !self.enabled {
            return;
        }
        let depth = self.current_path.len();

        match depth.cmp(&self.last_depth) {
            Ordering::Greater => {
                self.sibling_count.push(0);
            }
            Ordering::Less => {
                self.sibling_count.truncate(depth + 1);
            }
            Ordering::Equal => {}
        }
        self.last_depth = depth;

        let internal_index = self.sibling_count[depth];
        let trace_index = self.traces.len() - 1;
        self.trace_stack.push(trace_index);

        let txn = &mut self.traces[trace_index];
        txn.dept = depth as u64;
        txn.internal_index = internal_index as u64;

        self.sibling_count[depth] += 1;

        self.current_path.push(internal_index);
        if self.current_path.len() > 1 {
            let suffix =
                self.current_path[1..].iter().map(|s| s.to_string()).collect::<Vec<_>>().join("_");

            txn.name.reserve(1 + suffix.len());
            txn.name.push('_');
            txn.name.push_str(&suffix);
        }

        txn.name = txn.call_type.clone() + &txn.name;
    }

    /// Called after processing an operation to update tracking state.
    fn after_op(&mut self) {
        if !self.enabled {
            return;
        }
        self.current_path.pop();
        if self.trace_stack.is_empty() {
            self.all_traces.push(self.traces.clone());
            self.reset();
        }
    }

    /// Returns all collected internal transaction traces.
    ///
    /// Each inner `Vec` represents the traces for a single top-level transaction.
    pub fn get(&mut self) -> Vec<Vec<InternalTransaction>> {
        if !self.enabled {
            return Vec::new();
        }
        self.all_traces.clone()
    }

    /// Resets the collector state for reuse.
    pub fn reset(&mut self) {
        self.traces.clear();
        self.current_path.clear();
        self.last_depth = 0;
        self.sibling_count = vec![0];
        self.trace_stack.clear();
    }

    /// Extracts bytes from `CallInput` using the context to access shared memory.
    ///
    /// This method properly handles `SharedBuffer` by reading from the shared memory buffer
    /// via `ContextTr::local().shared_memory_buffer_slice()`.
    #[inline]
    fn extract_call_input<CTX: ContextTr>(ctx: &CTX, input: &CallInput) -> Bytes {
        match input {
            CallInput::Bytes(b) => b.clone(),
            CallInput::SharedBuffer(range) => ctx
                .local()
                .shared_memory_buffer_slice(range.clone())
                .map(|s: Ref<'_, [u8]>| Bytes::from(s.to_vec()))
                .unwrap_or_default(),
        }
    }
}

/// Generic implementation of `Inspector` for `TraceCollector`.
///
/// This implementation requires `CTX: ContextTr` to properly handle `CallInput::SharedBuffer`.
/// All EVM contexts in reth (including `OpContext`) implement `ContextTr`, so this
/// constraint is always satisfied when used with valid EVM implementations.
impl<CTX> Inspector<CTX, EthInterpreter> for TraceCollector
where
    CTX: ContextTr,
{
    fn call(&mut self, ctx: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        if !self.enabled {
            return None;
        }
        let call_type = match inputs.scheme {
            reth_revm::interpreter::CallScheme::Call => "call",
            reth_revm::interpreter::CallScheme::CallCode => "callcode",
            reth_revm::interpreter::CallScheme::DelegateCall => "delegatecall",
            reth_revm::interpreter::CallScheme::StaticCall => "staticcall",
        }
        .to_string();

        // Properly extract call input using ContextTr
        let call_input = Self::extract_call_input(ctx, &inputs.input);

        self.init_op(
            call_type,
            inputs.caller.to_string(),
            inputs.target_address.to_string(),
            call_input,
            inputs.value.get().to_string(),
            inputs.gas_limit,
            inputs.bytecode_address.to_string(),
        );

        self.before_op();

        None
    }

    fn call_end(&mut self, _ctx: &mut CTX, _inputs: &CallInputs, outcome: &mut CallOutcome) {
        if !self.enabled {
            return;
        }
        let trace_index = self.trace_stack.pop().unwrap_or_default();
        let (_, after) = self.traces.split_at_mut(trace_index);

        if let Some((txn, remainder)) = after.split_first_mut() {
            txn.gas_used = outcome.result.gas.spent();
            txn.output = outcome.result.output.clone();
            txn.is_error = !outcome.result.is_ok();
            txn.error = if txn.is_error {
                Self::format_error(&outcome.result.result)
            } else {
                String::new()
            };
            if txn.is_error {
                for within in remainder {
                    within.is_error = txn.is_error;
                }
            }
        }

        self.after_op();
    }

    fn create(&mut self, _ctx: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        if !self.enabled {
            return None;
        }
        let call_type = match inputs.scheme() {
            reth_revm::interpreter::CreateScheme::Create => "create".to_string(),
            reth_revm::interpreter::CreateScheme::Create2 { salt: _ } => "create2".to_string(),
            reth_revm::interpreter::CreateScheme::Custom { address: _ } => "custom".to_string(),
        };

        self.init_op(
            call_type,
            inputs.caller().to_string(),
            "".to_string(),
            inputs.init_code().clone(),
            inputs.value().to_string(),
            inputs.gas_limit(),
            "".to_string(),
        );

        self.before_op();

        None
    }

    fn create_end(&mut self, _ctx: &mut CTX, _inputs: &CreateInputs, outcome: &mut CreateOutcome) {
        if !self.enabled {
            return;
        }
        let trace_index = self.trace_stack.pop().unwrap_or_default();
        let (_, after) = self.traces.split_at_mut(trace_index);

        if let Some((txn, remainder)) = after.split_first_mut() {
            txn.to = outcome.address.unwrap_or_default().to_string();
            txn.gas_used = outcome.result.gas.spent();
            txn.output = outcome.result.output.clone();
            txn.is_error = !outcome.result.is_ok();
            txn.error = if txn.is_error {
                Self::format_error(&outcome.result.result)
            } else {
                String::new()
            };
            if txn.is_error {
                for within in remainder {
                    within.is_error = txn.is_error;
                }
            }
        }

        self.after_op();
    }

    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        if !self.enabled {
            return;
        }
        self.init_op(
            "selfdestruct".to_string(),
            contract.to_string(),
            target.to_string(),
            Bytes::default(),
            value.to_string(),
            0,
            "".to_string(),
        );

        self.before_op();

        self.trace_stack.pop().unwrap_or_default();

        self.after_op();
    }
}
