use serde::{Deserialize, Serialize};

/// Kafka error
#[derive(Debug, thiserror::Error)]
pub enum KafkaError {
    #[error("buffer is full, cannot queue message: {0}")]
    BufferFull(String),
    #[error("buffer channel closed: {0}")]
    BufferClosed(String),
    #[error("send message error: {0}")]
    SendMessageError(String),
    #[error("new batch producer error: {0}")]
    NewBatchProducerError(String),
    #[error("unmarshal message error: {0}")]
    SendingMessageError(String),
    #[error(transparent)]
    Rdkafka(#[from] rdkafka::error::KafkaError),
}

/// Producer message
#[derive(Debug)]
pub struct ProducerMessage {
    /// The topic to send the message to
    pub topic: String,
    /// The key to send the message with
    pub key: Option<Vec<u8>>,
    /// The payload to send the message with
    pub payload: Vec<u8>,
}

/// Kafka configuration
#[derive(Clone, Debug)]
pub struct KafkaConfig {
    /// The kafka servers to connect to
    pub bootstrap_servers: Vec<String>,
    /// The topic to send the block info message to
    pub block_topic: String,
    /// The topic to send the transaction message to
    pub tx_topic: String,
    /// The topic to send the error trigger message to
    pub error_topic: String,
    /// The client ID to use for the Kafka producer
    pub client_id: String,
    /// The group ID to use for the Kafka consumer
    pub group_id: String,
}

/// Block info message
#[derive(Debug, Serialize, Deserialize)]
pub struct BlockInfo {
    pub header: String,
}

/// Transaction message
#[derive(Debug, Serialize, Deserialize)]
pub struct TxMsg {
    pub block_number: u64,
}

/// Error message
#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorMsg {
    pub block_number: u64,
}

/// Error trigger message
#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorTriggerMessage {
    pub block_number: u64,
}
