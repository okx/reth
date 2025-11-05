use alloy_rlp::{RlpDecodable, RlpEncodable};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

use alloy_primitives::{Address, Bytes, U256};

use reth_revm::{
    interpreter::{
        interpreter::EthInterpreter, CallInput, CallInputs, CallOutcome, CreateInputs,
        CreateOutcome,
    },
    Inspector,
};
use revm_context_interface::{ContextTr, LocalContextTr};

#[derive(Debug, Clone, Default, RlpEncodable, RlpDecodable, Serialize, Deserialize)]
pub struct InternalTransaction {
    dept: u64,
    internal_index: u64,
    call_type: String,
    name: String,
    trace_address: String,
    code_address: String,
    from: String,
    to: String,
    input: Bytes,
    output: Bytes,
    is_error: bool,
    gas: u64,
    gas_used: u64,
    value_wei: String,
    error: String,
    return_gas: u64,
}

#[derive(Debug, Clone)]
pub struct TraceCollector {
    all_traces: Vec<Vec<InternalTransaction>>,
    traces: Vec<InternalTransaction>,
    // depth
    current_path: Vec<usize>,
    // internal_index
    last_depth: usize,
    sibling_count: Vec<usize>,
    // op 进去的时候收 traces 的 index，出来的时候需要知道哪一个 trace 需要改
    trace_stack: Vec<usize>,
}

impl Default for TraceCollector {
    fn default() -> Self {
        Self {
            all_traces: Vec::<Vec<InternalTransaction>>::default(),
            traces: Vec::<InternalTransaction>::default(),
            current_path: Vec::<usize>::default(),
            last_depth: 0,
            sibling_count: vec![0],
            trace_stack: Vec::<usize>::default(),
        }
    }
}

impl TraceCollector {
    fn init_op(
        &mut self,
        call_type: String,
        from: String,
        to: String,
        input: Bytes,
        mut value_wei: String,
        gas_used: u64,
        code_address: String,
    ) {
        self.traces.push(InternalTransaction::default());

        let txn = self.traces.last_mut().unwrap();
        txn.call_type = call_type;
        txn.from = from.clone();
        txn.input = input;

        if value_wei.is_empty() {
            value_wei = String::from("0");
        }

        match txn.call_type.as_str() {
            "call" | "create" | "create2" | "custom" => {
                txn.value_wei = value_wei;
                txn.gas_used = gas_used;
                txn.to = to;
            }
            "staticcall" => {
                txn.gas_used = gas_used;
                txn.to = to;
            }
            "delegatecall" => {
                txn.value_wei = value_wei;
                txn.gas_used = gas_used;
                txn.from = to;
                txn.to = code_address;
                txn.trace_address = from;
            }
            "callcode" => {
                txn.value_wei = value_wei;
                txn.gas_used = gas_used;
                txn.to = code_address.clone();
                txn.code_address = code_address;
            }
            "selfdestruct" => {
                txn.value_wei = value_wei;
                txn.to = to;
            }
            _ => {}
        }
    }

    fn before_op(&mut self) {
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

        txn.name = txn.call_type.clone() + txn.name.as_str();
    }

    fn after_op(&mut self) {
        self.current_path.pop();
        if self.trace_stack.is_empty() {
            self.all_traces.push(self.traces.clone());
            self.reset();
        }
    }

    pub fn get(&mut self) -> Vec<Vec<InternalTransaction>> {
        self.all_traces.clone()
    }

    pub fn reset(&mut self) {
        self.traces.clear();
        self.current_path.clear();
        self.last_depth = 0;
        self.sibling_count = vec![0];
        self.trace_stack.clear();
    }
}

impl<CTX> Inspector<CTX, EthInterpreter> for TraceCollector
where
    CTX: ContextTr,
{
    fn call(&mut self, ctx: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        let call_type = match inputs.scheme {
            reth_revm::interpreter::CallScheme::Call => "call".to_string(),
            reth_revm::interpreter::CallScheme::CallCode => "callcode".to_string(), // code_address
            reth_revm::interpreter::CallScheme::DelegateCall => "delegatecall".to_string(), /* trace_address (contract caller), code_address (contract to) */
            reth_revm::interpreter::CallScheme::StaticCall => "staticcall".to_string(),
        };

        let call_input = match &inputs.input {
            CallInput::SharedBuffer(range) => ctx
                .local()
                .shared_memory_buffer_slice(range.clone())
                .map(|s| Bytes::from(s.to_vec()))
                .unwrap_or_default(),
            CallInput::Bytes(b) => b.clone(),
        };

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
        let trace_index = self.trace_stack.pop().unwrap_or_default();
        let (_before, after) = self.traces.split_at_mut(trace_index);
        let (txn, remainder) = after.split_first_mut().unwrap();
        txn.gas = outcome.result.gas.limit();
        txn.gas_used = outcome.result.gas.spent();
        txn.return_gas = outcome.result.gas.remaining();
        txn.output = outcome.result.output.clone();
        txn.is_error = !outcome.result.is_ok();
        txn.error =
            if txn.is_error { format!("{:?}", outcome.result.result) } else { String::new() };

        if txn.is_error {
            if outcome.result.is_error() {
                txn.gas_used = txn.gas;
                txn.return_gas = 0;
            }
            for within in remainder {
                within.is_error = txn.is_error;
                within.error = txn.error.clone();
            }
        }

        self.after_op();
    }

    fn create(&mut self, _ctx: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        let call_type = match inputs.scheme {
            reth_revm::interpreter::CreateScheme::Create => "create".to_string(),
            reth_revm::interpreter::CreateScheme::Create2 { salt: _ } => "create2".to_string(), /* code_address */
            reth_revm::interpreter::CreateScheme::Custom { address: _ } => "custom".to_string(),
        };

        self.init_op(
            call_type,
            inputs.caller.to_string(),
            Address::ZERO.to_string(),
            inputs.init_code.clone(),
            inputs.value.to_string(),
            inputs.gas_limit,
            Address::ZERO.to_string(),
        );

        self.before_op();

        None
    }

    fn create_end(&mut self, _ctx: &mut CTX, _inputs: &CreateInputs, outcome: &mut CreateOutcome) {
        let trace_index = self.trace_stack.pop().unwrap_or_default();
        let (_before, after) = self.traces.split_at_mut(trace_index);
        let (txn, remainder) = after.split_first_mut().unwrap();
        if trace_index != 0 {
            txn.to = outcome.address.unwrap_or_default().to_string();
        }
        txn.gas = outcome.result.gas.limit();
        txn.gas_used = outcome.result.gas.spent();
        txn.return_gas = outcome.result.gas.remaining();
        txn.output = outcome.result.output.clone();
        txn.is_error = !outcome.result.is_ok();
        txn.error =
            if txn.is_error { format!("{:?}", outcome.result.result) } else { String::new() };

        if txn.is_error {
            if outcome.result.is_error() {
                txn.gas_used = txn.gas;
                txn.return_gas = 0;
            }
            for within in remainder {
                within.is_error = txn.is_error;
                within.error = txn.error.clone();
            }
        }

        self.after_op();
    }

    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        self.init_op(
            "selfdestruct".to_string(),
            contract.to_string(),
            target.to_string(),
            Bytes::default(),
            value.to_string(),
            0,
            Address::ZERO.to_string(),
        );

        self.before_op();

        self.trace_stack.pop().unwrap_or_default();

        self.after_op();
    }
}
