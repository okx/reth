//! Transaction tracing module for monitoring transaction lifecycle

use alloy_primitives::B256;
use serde_json::json;
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// Transaction process ID for tracking different stages and event types
/// Each stage has three event IDs: START, PROGRESS (if applicable), and END
/// This ensures each event has a unique process_id
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransactionProcessId {
    // RPC Receive stage (50010-50011)
    RpcReceiveStart = 50010,
    RpcReceiveEnd = 50011,
    
    // TxPool stage (50020-50022)
    TxPoolAddStart = 50020,
    TxPoolValidateStart = 50021,
    TxPoolValidateEnd = 50022,
    TxPoolAddEnd = 50024,

    // Miner Select stage (50030-50032)
    MinerSelectStart = 50030,
    MinerSelectEnd = 50031,
    
    // Tx Execution stage (50033-50035)
    TxExecutionStart = 50040,
    TxExecutionEnd = 50041,
    TxPackagingEnd = 50042,
    
    // State Process stage (50040-50043)
    StateProcessStart = 50050,
    StateProcessEnd = 50051,
    StateCommitEnd = 50052,
    
    // Block Insert stage (50050-50052)
    BlockInsertStart = 50060,
    BlockInsertEnd = 50061,
}

impl TransactionProcessId {
    pub fn as_str(&self) -> &'static str {
        match self {
            TransactionProcessId::RpcReceiveStart => "RPC Receive Start",
            TransactionProcessId::RpcReceiveEnd => "RPC Receive End",
            TransactionProcessId::TxPoolAddStart => "TxPool Add Start",
            TransactionProcessId::TxPoolAddEnd => "TxPool Add End",
            TransactionProcessId::TxPoolValidateStart => "TxPool Validate Progress",
            TransactionProcessId::TxPoolValidateEnd => "TxPool Validate End",
            TransactionProcessId::MinerSelectStart => "Miner Select Start",
            TransactionProcessId::MinerSelectEnd => "Miner Select End",
            TransactionProcessId::TxExecutionStart => "Tx Execution Start",
            TransactionProcessId::TxExecutionEnd => "Tx Execution End",
            TransactionProcessId::TxPackagingEnd => "Tx Packaging End",
            TransactionProcessId::StateProcessStart => "State Process Start",
            TransactionProcessId::StateProcessEnd => "State Process End",
            TransactionProcessId::StateCommitEnd => "State Commit End",
            TransactionProcessId::BlockInsertStart => "Block Insert Start",
            TransactionProcessId::BlockInsertEnd => "Block Insert End",
        }
    }
    
    /// Get the base stage ID (for backward compatibility and grouping)
    pub fn base_stage_id(&self) -> u32 {
        match self {
            TransactionProcessId::RpcReceiveStart | TransactionProcessId::RpcReceiveEnd => 50010,
            TransactionProcessId::TxPoolAddStart | TransactionProcessId::TxPoolValidateStart | TransactionProcessId::TxPoolValidateEnd | TransactionProcessId::TxPoolAddEnd => 50020,
            TransactionProcessId::MinerSelectStart | TransactionProcessId::MinerSelectEnd => 50030,
            TransactionProcessId::TxExecutionStart | TransactionProcessId::TxExecutionEnd | TransactionProcessId::TxPackagingEnd => 50040,
            TransactionProcessId::StateProcessStart | TransactionProcessId::StateProcessEnd | TransactionProcessId::StateCommitEnd => 50050,
            TransactionProcessId::BlockInsertStart | TransactionProcessId::BlockInsertEnd => 50060,
        }
    }
}

/// Transaction trace event status
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionTraceStatus {
    Start,
    Progress,
    Success,
    Failed,
}

/// A single trace event for a transaction
#[derive(Debug, Clone)]
pub struct TransactionTraceEvent {
    pub tx_hash: B256,
    pub process_id: TransactionProcessId,
    pub status: TransactionTraceStatus,
    pub message: String,
    pub timestamp: Instant,
    pub duration: Option<Duration>,
}

/// Transaction trace statistics
#[derive(Debug, Default, Clone)]
pub struct TransactionTraceStats {
    pub events: Vec<TransactionTraceEvent>,
    pub start_time: Option<Instant>,
    pub end_time: Option<Instant>,
}

impl TransactionTraceStats {
    pub fn total_duration(&self) -> Option<Duration> {
        if let (Some(start), Some(end)) = (self.start_time, self.end_time) {
            Some(end.duration_since(start))
        } else {
            None
        }
    }

    pub fn stage_duration(&self, process_id: TransactionProcessId) -> Option<Duration> {
        // Find the END event for this process_id
        self.events
            .iter()
            .find(|e| e.process_id == process_id && e.status == TransactionTraceStatus::Success)
            .and_then(|e| e.duration)
    }
    
    /// Get stage duration by base stage ID (for grouping related events)
    pub fn stage_duration_by_base(&self, base_stage_id: u32) -> Option<Duration> {
        // Find any END event for this base stage
        self.events
            .iter()
            .find(|e| e.process_id.base_stage_id() == base_stage_id && e.status == TransactionTraceStatus::Success)
            .and_then(|e| e.duration)
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
    traces: Mutex<HashMap<B256, TransactionTraceStats>>,
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
                traces: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Check if tracing is enabled
    pub fn is_enabled(&self) -> bool {
        self.inner.enabled
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

        let event = TransactionTraceEvent {
            tx_hash,
            process_id,
            status: TransactionTraceStatus::Start,
            message: message.to_string(),
            timestamp: Instant::now(),
            duration: None,
        };

        // Log to console as JSON format for easy parsing
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
        
        // Log to console
        tracing::info!(
            target: "tx_trace",
            "{}",
            json_str
        );

        // Write to file if enabled (one JSON per line)
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
                    let _ = file.flush();
                }
            }
        }

        // Store in memory
        let mut traces = self.inner.traces.lock().unwrap();
        let stats = traces.entry(tx_hash).or_insert_with(|| {
            TransactionTraceStats {
                events: Vec::new(),
                start_time: Some(Instant::now()),
                end_time: None,
            }
        });
        stats.events.push(event);
    }

    /// Log transaction end (success or failure)
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

        // Find the corresponding start event
        // For END events, we need to find the matching START event by base stage ID
        let duration = {
            let mut traces = self.inner.traces.lock().unwrap();
            if let Some(stats) = traces.get_mut(&tx_hash) {
                let base_stage_id = process_id.base_stage_id();
                // Find the start event for this base stage
                if let Some(start_event) = stats.events.iter().find(|e| {
                    e.process_id.base_stage_id() == base_stage_id && e.status == TransactionTraceStatus::Start
                }) {
                    let duration = Instant::now().duration_since(start_event.timestamp);
                    Some(duration)
                } else {
                    None
                }
            } else {
                None
            }
        };

        let event = TransactionTraceEvent {
            tx_hash,
            process_id,
            status: if success {
                TransactionTraceStatus::Success
            } else {
                TransactionTraceStatus::Failed
            },
            message: message.to_string(),
            timestamp: Instant::now(),
            duration,
        };

        // Log to console as JSON format for easy parsing
        let status = if success { "SUCCESS" } else { "FAILED" };
        let timestamp_duration = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let timestamp_ms = timestamp_duration.as_millis();
        let timestamp_us = timestamp_duration.as_micros();
        let log_json = if let Some(duration_val) = duration {
            let duration_ms = duration_val.as_millis();
            let duration_us = duration_val.as_micros();
            json!({
                "trace_type": "TX_TRACE",
                "event_kind": "END",
                "tx_hash": format!("{:#x}", tx_hash),
                "process_id": process_id as u32,
                "process_name": process_id.as_str(),
                "status": status,
                "timestamp_ms": timestamp_ms,
                "timestamp_us": timestamp_us,
                "duration_ms": duration_ms,
                "duration_us": duration_us,
                "message": message
            })
        } else {
            json!({
                "trace_type": "TX_TRACE",
                "event_kind": "END",
                "tx_hash": format!("{:#x}", tx_hash),
                "process_id": process_id as u32,
                "process_name": process_id.as_str(),
                "status": status,
                "timestamp_ms": timestamp_ms,
                "timestamp_us": timestamp_us,
                "message": message
            })
        };
        let json_str = serde_json::to_string(&log_json).unwrap_or_default();
        
        // Log to console
        tracing::info!(
            target: "tx_trace",
            "{}",
            json_str
        );

        // Write to file if enabled (one JSON per line)
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
                    let _ = file.flush();
                }
            }
        }

        // Store in memory
        let mut traces = self.inner.traces.lock().unwrap();
        if let Some(stats) = traces.get_mut(&tx_hash) {
            stats.events.push(event);
        }
    }


    /// Get trace statistics for a transaction
    pub fn get_trace_stats(&self, tx_hash: &B256) -> Option<TransactionTraceStats> {
        let traces = self.inner.traces.lock().unwrap();
        traces.get(tx_hash).cloned()
    }

    /// Clear old traces (call periodically to prevent memory leak)
    pub fn clear_old_traces(&self, older_than: Duration) {
        let mut traces = self.inner.traces.lock().unwrap();
        let now = Instant::now();
        traces.retain(|_, stats| {
            stats.start_time.map_or(false, |start| {
                now.duration_since(start) < older_than
            })
        });
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
        
        // Log to console
        tracing::info!(
            target: "tx_trace",
            "{}",
            json_str
        );

        // Write to file if enabled (one JSON per line)
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
                    let _ = file.flush();
                }
            }
        }
    }

    /// Log block progress event
    pub fn log_block_progress(
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
            "event_kind": "PROGRESS",
            "block_hash": format!("{:#x}", block_hash),
            "block_number": block_number,
            "process_id": process_id as u32,
            "process_name": process_id.as_str(),
            "timestamp_ms": timestamp_ms,
            "timestamp_us": timestamp_us,
            "message": message
        });
        let json_str = serde_json::to_string(&log_json).unwrap_or_default();
        
        // Log to console
        tracing::info!(
            target: "tx_trace",
            "{}",
            json_str
        );

        // Write to file if enabled (one JSON per line)
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
                    let _ = file.flush();
                }
            }
        }
    }

    /// Log block end event
    pub fn log_block_end(
        &self,
        block_hash: B256,
        block_number: u64,
        process_id: TransactionProcessId,
        success: bool,
        duration: Option<Duration>,
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
        let log_json = if let Some(duration_val) = duration {
            let duration_ms = duration_val.as_millis();
            let duration_us = duration_val.as_micros();
            json!({
                "trace_type": "BLOCK_TRACE",
                "event_kind": "END",
                "block_hash": format!("{:#x}", block_hash),
                "block_number": block_number,
                "process_id": process_id as u32,
                "process_name": process_id.as_str(),
                "status": status,
                "timestamp_ms": timestamp_ms,
                "timestamp_us": timestamp_us,
                "duration_ms": duration_ms,
                "duration_us": duration_us,
                "message": message
            })
        } else {
            json!({
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
            })
        };
        let json_str = serde_json::to_string(&log_json).unwrap_or_default();
        
        // Log to console
        tracing::info!(
            target: "tx_trace",
            "{}",
            json_str
        );

        // Write to file if enabled (one JSON per line)
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
                    let _ = file.flush();
                }
            }
        }
    }
}

