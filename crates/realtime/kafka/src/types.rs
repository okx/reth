use serde::{Deserialize, Serialize};

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

pub struct ProducerMessage {
    pub topic: String,
    pub key: Option<Vec<u8>>,
    pub payload: Vec<u8>,
}

#[derive(Clone)]
pub struct KafkaConfig {
    pub bootstrap_servers: Vec<String>,
    pub block_topic: String,
    pub tx_topic: String,
    pub error_topic: String,
    pub client_id: String,
    pub group_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BlockInfo {
    pub header: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TxMsg {
    pub block_number: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorMsg {
    pub block_number: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorTriggerMessage {
    pub block_number: u64,
}
