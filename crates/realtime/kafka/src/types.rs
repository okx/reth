use serde::{Deserialize, Serialize};

/// Kafka error
#[derive(Debug, thiserror::Error)]
pub enum KafkaError {
    /// Internal channel for buffering messages is full
    #[error("message buffer is full, cannot accept more messages")]
    BufferFull,

    /// Internal channel error
    #[error("internal channel error: {0}")]
    ChannelError(String),

    /// Failed to forward consumed message to application
    #[error("failed to forward message to application: {0}")]
    MessageForward(String),

    /// Failed to serialize message payload
    #[error("failed to serialize message: {0}")]
    Serialization(serde_json::Error),

    /// Failed to deserialize message payload
    #[error("failed to deserialize message: {0}")]
    Deserialization(serde_json::Error),

    /// Failed to commit message
    #[error("commit message error: {0}")]
    CommitMessageError(String),

    /// Failed to create Kafka producer
    #[error("failed to create Kafka producer: {0}")]
    ProducerCreation(String),

    /// Failed to create Kafka consumer
    #[error("failed to create Kafka consumer: {0}")]
    ConsumerCreation(String),

    /// Underlying rdkafka error.
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
