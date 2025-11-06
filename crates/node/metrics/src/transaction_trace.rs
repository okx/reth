//! Transaction tracing module for monitoring transaction lifecycle

use alloy_primitives::B256;
use serde_json::json;
use std::{
    collections::HashMap,
    fs::{self, File},
    io::Write,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// Transaction process ID for tracking different stages and event types
/// Each stage has three event IDs: START, PROGRESS (if applicable), and END
/// This ensures each event has a unique process_id
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransactionProcessId {
    // RPC Receive stage (50010-50012)
    RpcReceiveStart = 50010,
    RpcReceiveProgress = 50011,
    RpcReceiveEnd = 50012,
    
    // TxPool Add stage (50020-50022)
    TxPoolAddStart = 50020,
    TxPoolAddProgress = 50021,
    TxPoolAddEnd = 50022,
    
    // TxPool Validate stage (50023-50024, no START as it's a sub-stage)
    TxPoolValidateProgress = 50023,
    TxPoolValidateEnd = 50024,
    
    // Miner Select stage (50030-50032)
    MinerSelectStart = 50030,
    MinerSelectProgress = 50031,
    MinerSelectEnd = 50032,
    
    // Tx Execution stage (50033-50035)
    TxExecutionStart = 50033,
    TxExecutionProgress = 50034,
    TxExecutionEnd = 50035,
    
    // Tx Packaging stage (50036-50037, no START as it's part of block building)
    TxPackagingProgress = 50036,
    TxPackagingEnd = 50037,
    
    // Block End stage (50038-50039)
    BlockEndStart = 50038,
    BlockEndEnd = 50039,
    
    // State Process stage (50040-50042)
    StateProcessStart = 50040,
    StateProcessProgress = 50041,
    StateProcessEnd = 50042,
    
    // State Apply stage (50043, no START/PROGRESS as it's part of commit)
    StateApplyProgress = 50043,
    
    // Receipt Generation stage (50044-50045)
    ReceiptGenProgress = 50044,
    ReceiptGenEnd = 50045,
    
    // State Commit stage (50046-50047)
    StateCommitStart = 50046,
    StateCommitEnd = 50047,
    
    // Block Insert stage (50050-50052)
    BlockInsertStart = 50050,
    BlockInsertProgress = 50051,
    BlockInsertEnd = 50052,
    
    // Block Validate stage (50053-50054)
    BlockValidateStart = 50053,
    BlockValidateEnd = 50054,
    
    // Block Confirm stage (50055-50056)
    BlockConfirmStart = 50055,
    BlockConfirmEnd = 50056,
}

impl TransactionProcessId {
    pub fn as_str(&self) -> &'static str {
        match self {
            TransactionProcessId::RpcReceiveStart => "RPC Receive Start",
            TransactionProcessId::RpcReceiveProgress => "RPC Receive Progress",
            TransactionProcessId::RpcReceiveEnd => "RPC Receive End",
            TransactionProcessId::TxPoolAddStart => "TxPool Add Start",
            TransactionProcessId::TxPoolAddProgress => "TxPool Add Progress",
            TransactionProcessId::TxPoolAddEnd => "TxPool Add End",
            TransactionProcessId::TxPoolValidateProgress => "TxPool Validate Progress",
            TransactionProcessId::TxPoolValidateEnd => "TxPool Validate End",
            TransactionProcessId::MinerSelectStart => "Miner Select Start",
            TransactionProcessId::MinerSelectProgress => "Miner Select Progress",
            TransactionProcessId::MinerSelectEnd => "Miner Select End",
            TransactionProcessId::TxExecutionStart => "Tx Execution Start",
            TransactionProcessId::TxExecutionProgress => "Tx Execution Progress",
            TransactionProcessId::TxExecutionEnd => "Tx Execution End",
            TransactionProcessId::TxPackagingProgress => "Tx Packaging Progress",
            TransactionProcessId::TxPackagingEnd => "Tx Packaging End",
            TransactionProcessId::BlockEndStart => "Block End Start",
            TransactionProcessId::BlockEndEnd => "Block End End",
            TransactionProcessId::StateProcessStart => "State Process Start",
            TransactionProcessId::StateProcessProgress => "State Process Progress",
            TransactionProcessId::StateProcessEnd => "State Process End",
            TransactionProcessId::StateApplyProgress => "State Apply Progress",
            TransactionProcessId::ReceiptGenProgress => "Receipt Generation Progress",
            TransactionProcessId::ReceiptGenEnd => "Receipt Generation End",
            TransactionProcessId::StateCommitStart => "State Commit Start",
            TransactionProcessId::StateCommitEnd => "State Commit End",
            TransactionProcessId::BlockInsertStart => "Block Insert Start",
            TransactionProcessId::BlockInsertProgress => "Block Insert Progress",
            TransactionProcessId::BlockInsertEnd => "Block Insert End",
            TransactionProcessId::BlockValidateStart => "Block Validate Start",
            TransactionProcessId::BlockValidateEnd => "Block Validate End",
            TransactionProcessId::BlockConfirmStart => "Block Confirm Start",
            TransactionProcessId::BlockConfirmEnd => "Block Confirm End",
        }
    }
    
    /// Get the base stage ID (for backward compatibility and grouping)
    pub fn base_stage_id(&self) -> u32 {
        match self {
            TransactionProcessId::RpcReceiveStart | TransactionProcessId::RpcReceiveProgress | TransactionProcessId::RpcReceiveEnd => 50010,
            TransactionProcessId::TxPoolAddStart | TransactionProcessId::TxPoolAddProgress | TransactionProcessId::TxPoolAddEnd => 50020,
            TransactionProcessId::TxPoolValidateProgress | TransactionProcessId::TxPoolValidateEnd => 50022,
            TransactionProcessId::MinerSelectStart | TransactionProcessId::MinerSelectProgress | TransactionProcessId::MinerSelectEnd => 50030,
            TransactionProcessId::TxExecutionStart | TransactionProcessId::TxExecutionProgress | TransactionProcessId::TxExecutionEnd => 50032,
            TransactionProcessId::TxPackagingProgress | TransactionProcessId::TxPackagingEnd => 50034,
            TransactionProcessId::BlockEndStart | TransactionProcessId::BlockEndEnd => 50036,
            TransactionProcessId::StateProcessStart | TransactionProcessId::StateProcessProgress | TransactionProcessId::StateProcessEnd => 50040,
            TransactionProcessId::StateApplyProgress => 50042,
            TransactionProcessId::ReceiptGenProgress | TransactionProcessId::ReceiptGenEnd => 50044,
            TransactionProcessId::StateCommitStart | TransactionProcessId::StateCommitEnd => 50046,
            TransactionProcessId::BlockInsertStart | TransactionProcessId::BlockInsertProgress | TransactionProcessId::BlockInsertEnd => 50050,
            TransactionProcessId::BlockValidateStart | TransactionProcessId::BlockValidateEnd => 50052,
            TransactionProcessId::BlockConfirmStart | TransactionProcessId::BlockConfirmEnd => 50054,
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
    traces: Mutex<HashMap<B256, TransactionTraceStats>>,
}

impl TransactionTracer {
    /// Create a new transaction tracer
    pub fn new(enabled: bool, output_path: Option<PathBuf>) -> Self {
        // Create output directory if specified
        if let Some(ref path) = output_path {
            if let Err(e) = fs::create_dir_all(path) {
                tracing::warn!(
                    target: "tx_trace",
                    ?path,
                    error = %e,
                    "Failed to create transaction trace output directory"
                );
            }
        }

        Self {
            inner: Arc::new(TransactionTracerInner {
                enabled,
                output_path,
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
        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let log_json = json!({
            "trace_type": "TX_TRACE",
            "event_kind": "START",
            "tx_hash": format!("{:#x}", tx_hash),
            "process_id": process_id as u32,
            "process_name": process_id.as_str(),
            "timestamp_ms": timestamp_ms,
            "message": message
        });
        tracing::info!(
            target: "tx_trace",
            "{}",
            serde_json::to_string(&log_json).unwrap_or_default()
        );

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

    /// Log transaction progress
    pub fn log_transaction_progress(
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
            status: TransactionTraceStatus::Progress,
            message: message.to_string(),
            timestamp: Instant::now(),
            duration: None,
        };

        // Log to console as JSON format for easy parsing
        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let log_json = json!({
            "trace_type": "TX_TRACE",
            "event_kind": "PROGRESS",
            "tx_hash": format!("{:#x}", tx_hash),
            "process_id": process_id as u32,
            "process_name": process_id.as_str(),
            "timestamp_ms": timestamp_ms,
            "message": message
        });
        tracing::info!(
            target: "tx_trace",
            "{}",
            serde_json::to_string(&log_json).unwrap_or_default()
        );

        // Store in memory
        let mut traces = self.inner.traces.lock().unwrap();
        if let Some(stats) = traces.get_mut(&tx_hash) {
            stats.events.push(event);
        }
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
        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let log_json = if let Some(duration_val) = duration {
            let duration_ms = duration_val.as_millis();
            json!({
                "trace_type": "TX_TRACE",
                "event_kind": "END",
                "tx_hash": format!("{:#x}", tx_hash),
                "process_id": process_id as u32,
                "process_name": process_id.as_str(),
                "status": status,
                "timestamp_ms": timestamp_ms,
                "duration_ms": duration_ms,
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
                "message": message
            })
        };
        tracing::info!(
            target: "tx_trace",
            "{}",
            serde_json::to_string(&log_json).unwrap_or_default()
        );

        // Store in memory
        let mut traces = self.inner.traces.lock().unwrap();
        if let Some(stats) = traces.get_mut(&tx_hash) {
            stats.events.push(event);

            // If this is the final confirmation, write to file
            if process_id == TransactionProcessId::BlockConfirmEnd && success {
                stats.end_time = Some(Instant::now());
                self.write_trace_file(&tx_hash, stats);
            }
        }
    }

    /// Write trace to file
    fn write_trace_file(&self, tx_hash: &B256, stats: &TransactionTraceStats) {
        if let Some(ref output_path) = self.inner.output_path {
            let filename = format!("{}.trace", tx_hash);
            let file_path = output_path.join(filename);

            if let Ok(mut file) = File::create(&file_path) {
                writeln!(file, "Transaction Trace: {}", tx_hash).ok();
                writeln!(file, "=").ok();
                writeln!(file, "").ok();

                if let Some(total_duration) = stats.total_duration() {
                    writeln!(file, "Total Duration: {} ms", total_duration.as_millis()).ok();
                }
                writeln!(file, "").ok();

                writeln!(file, "Events:").ok();
                writeln!(file, "------").ok();

                for event in &stats.events {
                    let status_str = match event.status {
                        TransactionTraceStatus::Start => "START",
                        TransactionTraceStatus::Progress => "PROGRESS",
                        TransactionTraceStatus::Success => "SUCCESS",
                        TransactionTraceStatus::Failed => "FAILED",
                    };

                    let duration_str = if let Some(duration) = event.duration {
                        format!(" ({} ms)", duration.as_millis())
                    } else {
                        String::new()
                    };

                    let elapsed = if let Some(start) = stats.start_time {
                        event.timestamp.duration_since(start).as_millis()
                    } else {
                        0
                    };
                    writeln!(
                        file,
                        "[{}] ProcessId: {} ({}) | Status: {} | Message: {}{}",
                        elapsed,
                        event.process_id as u32,
                        event.process_id.as_str(),
                        status_str,
                        event.message,
                        duration_str
                    )
                    .ok();
                }

                writeln!(file, "").ok();
                writeln!(file, "Stage Durations:").ok();
                writeln!(file, "---------------").ok();

                // Group by base stage ID and calculate duration
                let base_stages = [
                    (50010, "RPC Receive"),
                    (50020, "TxPool Add"),
                    (50022, "TxPool Validate"),
                    (50030, "Miner Select"),
                    (50032, "Tx Execution"),
                    (50034, "Tx Packaging"),
                    (50036, "Block End"),
                    (50040, "State Process"),
                    (50042, "State Apply"),
                    (50044, "Receipt Generation"),
                    (50046, "State Commit"),
                    (50050, "Block Insert"),
                    (50052, "Block Validate"),
                    (50054, "Block Confirm"),
                ];
                
                for (base_id, stage_name) in base_stages {
                    if let Some(duration) = stats.stage_duration_by_base(base_id) {
                        writeln!(
                            file,
                            "{} ({}): {} ms",
                            stage_name,
                            base_id,
                            duration.as_millis()
                        )
                        .ok();
                    }
                }

                file.flush().ok();
            } else {
                tracing::warn!(
                    target: "tx_trace",
                    ?file_path,
                    "Failed to create transaction trace file"
                );
            }
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

        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let log_json = json!({
            "trace_type": "BLOCK_TRACE",
            "event_kind": "START",
            "block_hash": format!("{:#x}", block_hash),
            "block_number": block_number,
            "process_id": process_id as u32,
            "process_name": process_id.as_str(),
            "timestamp_ms": timestamp_ms,
            "message": message
        });
        tracing::info!(
            target: "tx_trace",
            "{}",
            serde_json::to_string(&log_json).unwrap_or_default()
        );
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

        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let log_json = json!({
            "trace_type": "BLOCK_TRACE",
            "event_kind": "PROGRESS",
            "block_hash": format!("{:#x}", block_hash),
            "block_number": block_number,
            "process_id": process_id as u32,
            "process_name": process_id.as_str(),
            "timestamp_ms": timestamp_ms,
            "message": message
        });
        tracing::info!(
            target: "tx_trace",
            "{}",
            serde_json::to_string(&log_json).unwrap_or_default()
        );
    }

    /// Log block end event
    pub fn log_block_end(
        &self,
        block_hash: B256,
        block_number: u64,
        process_id: TransactionProcessId,
        success: bool,
        duration_ms: Option<u128>,
        message: &str,
    ) {
        if !self.inner.enabled {
            return;
        }

        let status = if success { "SUCCESS" } else { "FAILED" };
        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let log_json = if let Some(duration) = duration_ms {
            json!({
                "trace_type": "BLOCK_TRACE",
                "event_kind": "END",
                "block_hash": format!("{:#x}", block_hash),
                "block_number": block_number,
                "process_id": process_id as u32,
                "process_name": process_id.as_str(),
                "status": status,
                "timestamp_ms": timestamp_ms,
                "duration_ms": duration,
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
                "message": message
            })
        };
        tracing::info!(
            target: "tx_trace",
            "{}",
            serde_json::to_string(&log_json).unwrap_or_default()
        );
    }
}

