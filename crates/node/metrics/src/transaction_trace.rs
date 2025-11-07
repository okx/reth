//! Transaction tracing module for monitoring transaction lifecycle

use alloy_primitives::B256;
use serde_json::json;
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::{Arc, Mutex},
};

/// Transaction process ID for tracking different stages and event types
/// Each stage has three event IDs: START and END
/// This ensures each event has a unique process_id
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransactionProcessId {
    // RPC Receive stage (50010-50011)
    RpcReceiveStart = 50010,
    RpcReceiveEnd = 50011,
    
    // RPC Forward stage (50012-50013)
    RpcForwardStart = 50012,
    RpcForwardEnd = 50013,
    
    // TxPool stage (50020-50024)
    TxPoolAddStart = 50020,
    TxPoolValidateStart = 50021,
    TxPoolValidateEnd = 50022,
    TxPoolAddEnd = 50023,

    // Miner Select stage (50030-50032)
    MinerSelectStart = 50030,
    MinerSelectEnd = 50031,
    
    // Tx Execution stage (50040-50042)
    TxExecutionStart = 50040,
    TxExecutionEnd = 50041,
    TxPackagingEnd = 50042,
    
    // RPC Tx Process stage (50050-50052)
    RPCTxExecutionStart = 50050,
    RPCTxExecutionEnd = 50051,
    RPCTxCommitEnd = 50052,
    
    // Block Insert stage (50060-50061)
    BlockInsertStart = 50060,
    BlockInsertEnd = 50061,
}

impl TransactionProcessId {
    pub fn as_str(&self) -> &'static str {
        match self {
            TransactionProcessId::RpcReceiveStart => "RPC Receive Start",
            TransactionProcessId::RpcReceiveEnd => "RPC Receive End",
            TransactionProcessId::RpcForwardStart => "RPC Forward Start",
            TransactionProcessId::RpcForwardEnd => "RPC Forward End",
            TransactionProcessId::TxPoolAddStart => "TxPool Add Start",
            TransactionProcessId::TxPoolAddEnd => "TxPool Add End",
            TransactionProcessId::TxPoolValidateStart => "TxPool Validate Progress",
            TransactionProcessId::TxPoolValidateEnd => "TxPool Validate End",
            TransactionProcessId::MinerSelectStart => "Miner Select Start",
            TransactionProcessId::MinerSelectEnd => "Miner Select End",
            TransactionProcessId::TxExecutionStart => "Tx Execution Start",
            TransactionProcessId::TxExecutionEnd => "Tx Execution End",
            TransactionProcessId::TxPackagingEnd => "Tx Packaging End",
            TransactionProcessId::RPCTxExecutionStart => "RPC Tx Execution Start",
            TransactionProcessId::RPCTxExecutionEnd => "RPC Tx Execution End",
            TransactionProcessId::RPCTxCommitEnd => "RPC Tx Commit End",
            TransactionProcessId::BlockInsertStart => "Block Insert Start",
            TransactionProcessId::BlockInsertEnd => "Block Insert End",
        }
    }
    
    /// Get the base stage ID (for backward compatibility and grouping)
    pub fn base_stage_id(&self) -> u32 {
        match self {
            TransactionProcessId::RpcReceiveStart | TransactionProcessId::RpcReceiveEnd | TransactionProcessId::RpcForwardStart | TransactionProcessId::RpcForwardEnd => 50010,
            TransactionProcessId::TxPoolAddStart | TransactionProcessId::TxPoolValidateStart | TransactionProcessId::TxPoolValidateEnd | TransactionProcessId::TxPoolAddEnd => 50020,
            TransactionProcessId::MinerSelectStart | TransactionProcessId::MinerSelectEnd => 50030,
            TransactionProcessId::TxExecutionStart | TransactionProcessId::TxExecutionEnd | TransactionProcessId::TxPackagingEnd => 50040,
            TransactionProcessId::RPCTxExecutionStart | TransactionProcessId::RPCTxExecutionEnd | TransactionProcessId::RPCTxCommitEnd => 50050,
            TransactionProcessId::BlockInsertStart | TransactionProcessId::BlockInsertEnd => 50060,
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
    output_path: Option<PathBuf>,
    output_file: Mutex<Option<File>>,
}

impl TransactionTracer {
    /// Create a new transaction tracer
    pub fn new(enabled: bool, output_path: Option<PathBuf>) -> Self {
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
                output_path: output_path.clone(),
                output_file: Mutex::new(output_file),
            }),
        }
    }

    /// Check if tracing is enabled
    pub fn is_enabled(&self) -> bool {
        self.inner.enabled
    }

    /// Write JSON string to trace file
    fn write_to_file(&self, json_str: &str) {
        if let Ok(mut file_guard) = self.inner.output_file.lock() {
            if let Some(ref mut file) = *file_guard {
                if let Err(e) = writeln!(file, "{}", json_str) {
                    tracing::warn!(
                        target: "tx_trace",
                        error = %e,
                        "Failed to write to transaction trace file"
                    );
                } else {
                    // Flush immediately for real-time logging
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

    /// Log transaction start
    pub fn log_transaction_start(
        &self,
        tx_hash: B256,
        process_id: TransactionProcessId,
        message: &str,
    ) {
        if !self.inner.enabled {
            return;
        }

        let timestamp_duration = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let timestamp_ms = timestamp_duration.as_millis();
        let timestamp_us = timestamp_duration.as_micros();
        let log_json = json!({
            "trace_type": "TX_TRACE",
            "event_kind": "START",
            "tx_hash": format!("{:#x}", tx_hash),
            "process_id": process_id as u32,
            "process_name": process_id.as_str(),
            "timestamp_ms": timestamp_ms,
            "timestamp_us": timestamp_us,
            "message": message
        });
        let json_str = serde_json::to_string(&log_json).unwrap_or_default();
        
        self.write_to_file(&json_str);
    }

    /// Log transaction end (success or failure)
    /// Note: duration is not calculated here, should be calculated offline from log files
    pub fn log_transaction_end(
        &self,
        tx_hash: B256,
        process_id: TransactionProcessId,
        success: bool,
        message: &str,
    ) {
        if !self.inner.enabled {
            return;
        }

        let status = if success { "SUCCESS" } else { "FAILED" };
        let timestamp_duration = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let timestamp_ms = timestamp_duration.as_millis();
        let timestamp_us = timestamp_duration.as_micros();
        let log_json = json!({
            "trace_type": "TX_TRACE",
            "event_kind": "END",
            "tx_hash": format!("{:#x}", tx_hash),
            "process_id": process_id as u32,
            "process_name": process_id.as_str(),
            "status": status,
            "timestamp_ms": timestamp_ms,
            "timestamp_us": timestamp_us,
            "message": message
        });
        let json_str = serde_json::to_string(&log_json).unwrap_or_default();
        
        self.write_to_file(&json_str);
    }
}

impl Default for TransactionTracer {
    fn default() -> Self {
        Self::new(false, None)
    }
}

/// Global transaction tracer instance
/// This is a simple approach to make the tracer accessible throughout the codebase
static GLOBAL_TRACER: std::sync::OnceLock<Arc<TransactionTracer>> = std::sync::OnceLock::new();

/// Initialize the global transaction tracer
pub fn init_global_tracer(enabled: bool, output_path: Option<PathBuf>) {
    let tracer = TransactionTracer::new(enabled, output_path);
    GLOBAL_TRACER.set(Arc::new(tracer)).ok();
}

/// Get the global transaction tracer
pub fn get_global_tracer() -> Option<Arc<TransactionTracer>> {
    GLOBAL_TRACER.get().cloned()
}

/// Block tracing functions for monitoring block lifecycle
impl TransactionTracer {
    /// Log block start event
    pub fn log_block_start(
        &self,
        block_hash: B256,
        block_number: u64,
        process_id: TransactionProcessId,
        message: &str,
    ) {
        if !self.inner.enabled {
            return;
        }

        let timestamp_duration = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let timestamp_ms = timestamp_duration.as_millis();
        let timestamp_us = timestamp_duration.as_micros();
        let log_json = json!({
            "trace_type": "BLOCK_TRACE",
            "event_kind": "START",
            "block_hash": format!("{:#x}", block_hash),
            "block_number": block_number,
            "process_id": process_id as u32,
            "process_name": process_id.as_str(),
            "timestamp_ms": timestamp_ms,
            "timestamp_us": timestamp_us,
            "message": message
        });
        let json_str = serde_json::to_string(&log_json).unwrap_or_default();
        
        self.write_to_file(&json_str);
    }

    /// Log block end event
    pub fn log_block_end(
        &self,
        block_hash: B256,
        block_number: u64,
        process_id: TransactionProcessId,
        success: bool,
        message: &str,
    ) {
        if !self.inner.enabled {
            return;
        }

        let status = if success { "SUCCESS" } else { "FAILED" };
        let timestamp_duration = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let timestamp_ms = timestamp_duration.as_millis();
        let timestamp_us = timestamp_duration.as_micros();
        let log_json = json!({
            "trace_type": "BLOCK_TRACE",
            "event_kind": "END",
            "block_hash": format!("{:#x}", block_hash),
            "block_number": block_number,
            "process_id": process_id as u32,
            "process_name": process_id.as_str(),
            "status": status,
            "timestamp_ms": timestamp_ms,
            "timestamp_us": timestamp_us,
            "message": message
        });
        let json_str = serde_json::to_string(&log_json).unwrap_or_default();
        
        self.write_to_file(&json_str);
    }
}

