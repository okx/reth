//! Transaction tracing module for monitoring transaction lifecycle

use alloy_primitives::B256;
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};

/// Number of log entries to write before forcing a flush
const FLUSH_INTERVAL_WRITES: u64 = 100;

/// Time interval between flushes (in seconds)
const FLUSH_INTERVAL_SECONDS: u64 = 1;

/// Fixed chain name
const CHAIN_NAME: &str = "X Layer";

/// Fixed business name
const BUSINESS_NAME: &str = "X Layer";

/// Node type for identifying sequencer vs RPC node
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeType {
    /// Sequencer node (builds blocks)
    Sequencer,
    /// RPC node (forwards transactions to sequencer)
    Rpc,
    /// Unknown node type (default)
    Unknown,
}

impl NodeType {
    pub fn as_str(&self) -> &'static str {
        match self {
            NodeType::Sequencer => "sequencer",
            NodeType::Rpc => "rpc",
            NodeType::Unknown => "unknown",
        }
    }
}

/// Transaction process ID for tracking different stages and event types
/// Each stage has three event IDs: START and END
/// This ensures each event has a unique process_id
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransactionProcessId {
    // RPC Receive Tx stage
    RpcReceiveTxEnd = 15010,
    
    // SEQ Receive Tx stage - Sequencer node receiving transaction
    SeqReceiveTxEnd = 15030,

    SeqBlockBuildStart = 15032,

    // SEQ Tx Execution stage
    SeqTxExecutionEnd = 15034,

    SeqBlockBuildEnd = 15036,

    // SEQ Block Send stage - Sequencer sending block to RPC
    SeqBlockSendStart = 15042,

    // RPC Block Receive stage - RPC node receiving block from sequencer
    RpcBlockReceiveEnd = 15060,

    // Block Insert stage (50060-50061)
    RpcBlockInsertEnd = 15062,
}

impl TransactionProcessId {
    pub fn as_str(&self) -> &'static str {
        match self {
            TransactionProcessId::RpcReceiveTxEnd => "xlayer_rpc_receive_tx",
            TransactionProcessId::SeqReceiveTxEnd => "xlayer_seq_receive_tx",
            TransactionProcessId::SeqBlockBuildStart => "xlayer_seq_begin_block",
            TransactionProcessId::SeqTxExecutionEnd => "xlayer_seq_package_tx",
            TransactionProcessId::SeqBlockBuildEnd => "xlayer_seq_end_block",
            TransactionProcessId::SeqBlockSendStart => "xlayer_seq_ds_sent",
            TransactionProcessId::RpcBlockReceiveEnd => "xlayer_rpc_receive_block",
            TransactionProcessId::RpcBlockInsertEnd => "xlayer_rpc_finish_block",
        }
    }
}


/// Transaction tracer for monitoring transaction lifecycle
#[derive(Clone)]
pub struct TransactionTracer {
    inner: Arc<TransactionTracerInner>,
}

struct TransactionTracerInner {
    enabled: bool,
    node_type: NodeType,
    output_file: Mutex<Option<File>>,
    // Flush control: track write count and last flush time
    write_count: AtomicU64,
    last_flush_time: Mutex<Instant>,
}

impl TransactionTracer {
    /// Create a new transaction tracer
    pub fn new(enabled: bool, output_path: Option<PathBuf>, node_type: NodeType) -> Self {
        let output_file = if let Some(ref path) = output_path {
            // Determine the actual file path
            // If path ends with separator or doesn't have a file extension, treat as directory
            let file_path = if path.to_string_lossy().ends_with('/') || path.to_string_lossy().ends_with('\\') {
                // Ends with separator, treat as directory
                path.join("trace.log")
            } else if path.extension().is_none() && !path.exists() {
                // No extension and doesn't exist, likely a directory path
                path.join("trace.log")
            } else {
                // Has extension or exists, treat as file path
                path.clone()
            };

            // Create parent directory if needed
            if let Some(parent) = file_path.parent() {
                if let Err(e) = fs::create_dir_all(parent) {
                    tracing::warn!(
                        target: "tx_trace",
                        ?parent,
                        error = %e,
                        "Failed to create transaction trace output directory"
                    );
                }
            }

            // Open file in append mode
            match OpenOptions::new()
                .create(true)
                .append(true)
                .open(&file_path)
            {
                Ok(file) => {
                    tracing::info!(
                        target: "tx_trace",
                        ?file_path,
                        "Transaction trace file opened for appending"
                    );
                    Some(file)
                }
                Err(e) => {
                    tracing::warn!(
                        target: "tx_trace",
                        ?file_path,
                        error = %e,
                        "Failed to open transaction trace file"
                    );
                    None
                }
            }
        } else {
            None
        };

        Self {
            inner: Arc::new(TransactionTracerInner {
                enabled,
                node_type,
                output_file: Mutex::new(output_file),
                write_count: AtomicU64::new(0),
                last_flush_time: Mutex::new(Instant::now()),
            }),
        }
    }

    /// Check if tracing is enabled
    pub fn is_enabled(&self) -> bool {
        self.inner.enabled
    }

    /// Write CSV line to trace file with periodic flush
    fn write_to_file(&self, csv_line: &str) {
        match self.inner.output_file.lock() {
            Ok(mut file_guard) => {
                if let Some(ref mut file) = *file_guard {
                    if let Err(e) = writeln!(file, "{}", csv_line) {
                        tracing::warn!(
                            target: "tx_trace",
                            error = %e,
                            "Failed to write to transaction trace file"
                        );
                    } else {
                        // Increment write count
                        let count = self.inner.write_count.fetch_add(1, Ordering::Relaxed) + 1;
                        
                        // Flush periodically: every FLUSH_INTERVAL_WRITES writes or every FLUSH_INTERVAL_SECONDS seconds
                        let should_flush = {
                            let mut last_flush = self.inner.last_flush_time.lock().unwrap();
                            let now = Instant::now();
                            let time_since_flush = now.duration_since(*last_flush);
                            
                            if count % FLUSH_INTERVAL_WRITES == 0 || time_since_flush.as_secs() >= FLUSH_INTERVAL_SECONDS {
                                *last_flush = now;
                                true
                            } else {
                                false
                            }
                        };
                        
                        if should_flush {
                            if let Err(e) = file.flush() {
                                tracing::warn!(
                                    target: "tx_trace",
                                    error = %e,
                                    "Failed to flush transaction trace file"
                                );
                            }
                        }
                    }
                }
            }
            Err(e) => {
                // Lock acquisition failed (e.g., mutex was poisoned)
                tracing::warn!(
                    target: "tx_trace",
                    error = %e,
                    "Failed to acquire lock for transaction trace file"
                );
            }
        }
    }
    
    /// Force flush the trace file (used on shutdown to ensure no logs are lost)
    pub fn flush(&self) {
        match self.inner.output_file.lock() {
            Ok(mut file_guard) => {
                if let Some(ref mut file) = *file_guard {
                    if let Err(e) = file.flush() {
                        tracing::warn!(
                            target: "tx_trace",
                            error = %e,
                            "Failed to flush transaction trace file on shutdown"
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    target: "tx_trace",
                    error = %e,
                    "Failed to acquire lock for flushing transaction trace file"
                );
            }
        }
    }

    /// Format CSV line with 23 fields (comma-separated, empty string for missing fields)
    /// Fields: chain,trace,status,serviceName,business,client,chainld,process,processWord,index,innerIndex,currentTime,referld,contractAddress,blockHeight,blockHash,blockTime,depositConfirmHeight,tokenID,mevSupplier,businessHash,transactionType,extJson
    fn format_csv_line(
        &self,
        trace: &str,
        process_id: u32,
        process_word: &str,
        current_time: u128,
        block_hash: Option<B256>,
        block_number: Option<u64>,
    ) -> String {
        // Helper function to escape CSV fields (handle commas and quotes)
        let escape_csv = |s: &str| -> String {
            if s.contains(',') || s.contains('"') || s.contains('\n') {
                format!("\"{}\"", s.replace('"', "\"\""))
            } else {
                s.to_string()
            }
        };

        let chain = CHAIN_NAME; // 链名 (固定为 "X Layer")
        let trace_hash = trace.to_lowercase(); // 交易hash (小写)
        let status_str = "";
        let service_name = self.inner.node_type.as_str(); // 服务名 (节点类型: sequencer/rpc/unknown)
        let business = BUSINESS_NAME; // 业务名 (固定为 "X Layer")
        let client = ""; // 客户端
        let chainld = ""; // 链ID
        let process_str = process_id.to_string(); // 处理阶段(步骤) - process ID 数值
        let process_word_str = process_word; // 处理阶段关键字 - process ID 字符串
        let index = ""; // 代币交易序号
        let inner_index = ""; // 内部交易序号
        let current_time_str = current_time.to_string(); // 当前时间戳(13位)
        let referld = ""; // 统一业务ID
        let contract_address = ""; // 合约地址
        let block_height = block_number.map(|n| n.to_string()).unwrap_or_default(); // 区块高度
        let block_hash_str = block_hash.map(|h| format!("{:#x}", h).to_lowercase()).unwrap_or_default(); // 区块哈希 (小写)
        let block_time = ""; // 区块上链时间(UTC时间,毫秒级)
        let deposit_confirm_height = ""; // 到达充值确认的区块高度
        let token_id = ""; // 主链的id
        let mev_supplier = ""; // mev供应商
        let business_hash = ""; // 业务自定义hash
        let transaction_type = ""; // 交易类型
        let ext_json = ""; // 扩展字段{key:value}
        
        format!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            escape_csv(chain), escape_csv(&trace_hash), escape_csv(status_str), escape_csv(service_name),
            escape_csv(business), escape_csv(client), escape_csv(chainld), escape_csv(&process_str),
            escape_csv(process_word_str), escape_csv(index), escape_csv(inner_index), escape_csv(&current_time_str),
            escape_csv(referld), escape_csv(contract_address), escape_csv(&block_height), escape_csv(&block_hash_str),
            escape_csv(block_time), escape_csv(deposit_confirm_height), escape_csv(token_id), escape_csv(mev_supplier),
            escape_csv(business_hash), escape_csv(transaction_type), escape_csv(ext_json)
        )
    }

    /// Log transaction event at current time point
    /// Records the timestamp and process ID for the transaction
    /// Optionally records block number for transactions that are part of a block
    pub fn log_transaction(
        &self,
        tx_hash: B256,
        process_id: TransactionProcessId,
        block_number: Option<u64>,
    ) {
        if !self.inner.enabled {
            return;
        }

        let timestamp_duration = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let timestamp_ms = timestamp_duration.as_millis();
        let trace_hash = format!("{:#x}", tx_hash);
        let process_id_value = process_id as u32;
        let process_word = process_id.as_str();
        
        let csv_line = self.format_csv_line(
            &trace_hash,
            process_id_value,
            process_word,
            timestamp_ms,
            None,
            block_number,
        );
        
        self.write_to_file(&csv_line);
    }

}

impl Default for TransactionTracer {
    fn default() -> Self {
        Self::new(false, None, NodeType::Unknown)
    }
}

/// Global transaction tracer instance
/// This is a simple approach to make the tracer accessible throughout the codebase
static GLOBAL_TRACER: std::sync::OnceLock<Arc<TransactionTracer>> = std::sync::OnceLock::new();

/// Initialize the global transaction tracer
pub fn init_global_tracer(enabled: bool, output_path: Option<PathBuf>, node_type: NodeType) {
    let tracer = TransactionTracer::new(enabled, output_path, node_type);
    GLOBAL_TRACER.set(Arc::new(tracer)).ok();
}

/// Get the global transaction tracer
pub fn get_global_tracer() -> Option<Arc<TransactionTracer>> {
    GLOBAL_TRACER.get().cloned()
}

/// Flush the global transaction tracer (should be called on shutdown)
pub fn flush_global_tracer() {
    if let Some(tracer) = get_global_tracer() {
        tracer.flush();
    }
}

/// Block tracing functions for monitoring block lifecycle
impl TransactionTracer {
    /// Log block event at current time point
    /// Records the timestamp and process ID for the block
    pub fn log_block(
        &self,
        block_hash: B256,
        block_number: u64,
        process_id: TransactionProcessId,
    ) {
        if !self.inner.enabled {
            return;
        }

        let timestamp_duration = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let timestamp_ms = timestamp_duration.as_millis();
        let trace_hash = format!("{:#x}", block_hash);
        let process_id_value = process_id as u32;
        let process_word = process_id.as_str();
        
        let csv_line = self.format_csv_line(
            &trace_hash,
            process_id_value,
            process_word,
            timestamp_ms,
            Some(block_hash),
            Some(block_number),
        );
        
        self.write_to_file(&csv_line);
    }

}


#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;
    use std::{
        fs,
        io::Read,
        sync::Arc,
        thread,
    };
    use tempfile::TempDir;

    fn create_test_tracer() -> (TransactionTracer, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let trace_file = temp_dir.path().join("trace.log");
        let tracer = TransactionTracer::new(true, Some(trace_file), NodeType::Unknown);
        (tracer, temp_dir)
    }

    #[test]
    fn test_tracer_creation() {
        let tracer = TransactionTracer::new(false, None, NodeType::Unknown);
        assert!(!tracer.is_enabled());

        let (tracer, _temp_dir) = create_test_tracer();
        assert!(tracer.is_enabled());
    }

    #[test]
    fn test_log_transaction() {
        let (tracer, temp_dir) = create_test_tracer();
        let tx_hash = B256::from([1u8; 32]);

        tracer.log_transaction(tx_hash, TransactionProcessId::RpcReceiveTxEnd, None);

        let trace_file = temp_dir.path().join("trace.log");
        assert!(trace_file.exists());

        let mut contents = String::new();
        fs::File::open(&trace_file)
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();

        // CSV format: chain,trace,status,serviceName,business,client,chainld,process,processWord,index,innerIndex,currentTime,referld,contractAddress,blockHeight,blockHash,blockTime,depositConfirmHeight,tokenID,mevSupplier,businessHash,transactionType,extJson
        assert!(contents.contains(","));
        let tx_hash_lower = format!("{:#x}", tx_hash).to_lowercase();
        assert!(contents.contains(&tx_hash_lower));
    }

    #[test]
    fn test_log_block() {
        let (tracer, temp_dir) = create_test_tracer();
        let block_hash = B256::from([2u8; 32]);
        let block_number = 12345u64;

        tracer.log_block(block_hash, block_number, TransactionProcessId::RpcBlockInsertEnd);

        let trace_file = temp_dir.path().join("trace.log");
        let mut contents = String::new();
        fs::File::open(&trace_file)
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();

        // CSV format: chain,trace,status,serviceName,business,client,chainld,process,processWord,index,innerIndex,currentTime,referld,contractAddress,blockHeight,blockHash,blockTime,depositConfirmHeight,tokenID,mevSupplier,businessHash,transactionType,extJson
        assert!(contents.contains(","));
        let block_hash_lower = format!("{:#x}", block_hash).to_lowercase();
        assert!(contents.contains(&block_hash_lower));
        assert!(contents.contains(&block_number.to_string()));
    }

    #[test]
    fn test_concurrent_write_to_file() {
        let (tracer, temp_dir) = create_test_tracer();
        let tracer = Arc::new(tracer);
        let trace_file = temp_dir.path().join("trace.log");

        let num_threads = 10;
        let writes_per_thread = 100;
        let mut handles: Vec<std::thread::JoinHandle<()>> = vec![];

        for thread_id in 0..num_threads {
            let tracer_clone = Arc::clone(&tracer);
            let handle = thread::spawn(move || {
                for _i in 0..writes_per_thread {
                    let tx_hash = B256::from([thread_id as u8; 32]);
                    let process_id = TransactionProcessId::RpcReceiveTxEnd;
                    tracer_clone.log_transaction(tx_hash, process_id, None);
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }

        let mut contents = String::new();
        fs::File::open(&trace_file)
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();

        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), num_threads * writes_per_thread);

        // Verify all lines are valid CSV (23 fields separated by commas)
        for line in lines {
            let fields: Vec<&str> = line.split(',').collect();
            assert!(fields.len() >= 23, "CSV line should have at least 23 fields");
        }
    }


    #[test]
    fn test_concurrent_write_with_different_hashes() {
        let (tracer, temp_dir) = create_test_tracer();
        let tracer = Arc::new(tracer);
        let trace_file = temp_dir.path().join("trace.log");

        let num_threads = 5;
        let mut handles = vec![];

        for thread_id in 0..num_threads {
            let tracer_clone = Arc::clone(&tracer);
            let handle = thread::spawn(move || {
                let mut hash_bytes = [0u8; 32];
                hash_bytes[0] = thread_id as u8;
                let tx_hash = B256::from(hash_bytes);

                for _ in 0..50 {
                    tracer_clone.log_transaction(tx_hash, TransactionProcessId::SeqTxExecutionEnd, None);
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }

        let mut contents = String::new();
        fs::File::open(&trace_file)
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();

        let log_lines: Vec<&str> = contents
            .lines()
            .filter(|line| !line.is_empty())
            .collect();

        // Each thread writes 50 transactions, so total should be num_threads * 50
        assert_eq!(log_lines.len(), num_threads * 50);

        for line in log_lines {
            assert!(
                line.split(',').count() >= 23, // CSV format with 23 fields
                "Invalid CSV in concurrent write test"
            );
        }
    }
}
