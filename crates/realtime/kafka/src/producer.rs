// producer.rs

use crate::{
    batch_producer::BatchProducer,
    types::{ErrorTriggerMessage, KafkaConfig, KafkaError, ProducerMessage},
};
use serde::Serialize;
use tokio::sync::mpsc;

/// KafkaProducer wraps BatchProducer with domain-specific send methods
pub struct KafkaProducer {
    producer: BatchProducer,
    config: KafkaConfig,
}

impl KafkaProducer {
    pub async fn new(
        config: KafkaConfig,
        success_chan: Option<mpsc::Sender<()>>,
    ) -> Result<Self, KafkaError> {
        let producer: BatchProducer = BatchProducer::new(config.clone(), success_chan).await?;
        tracing::info!(
            "[Realtime] kafka producer created and listening to servers: {:?}",
            config.bootstrap_servers
        );
        Ok(Self { producer, config })
    }

    pub async fn close(self) -> Result<(), KafkaError> {
        self.producer.close().await?;
        Ok(())
    }

    pub async fn send_kafka_transaction<T: Serialize>(
        &self,
        tx_hash: String,
        message: &T,
    ) -> Result<(), KafkaError> {
        let json_data = serde_json::to_vec(message)
            .map_err(|e| KafkaError::SendMessageError(format!("marshalling error: {}", e)))?;

        let kafka_msg = ProducerMessage {
            topic: self.config.tx_topic.clone(),
            key: Some(tx_hash.into_bytes()),
            payload: json_data,
        };

        self.producer.send_message(kafka_msg).await?;
        Ok(())
    }

    pub async fn send_kafka_block_info<T: Serialize>(
        &self,
        block_number: u64,
        message: &T,
    ) -> Result<(), KafkaError> {
        let json_data = serde_json::to_vec(message)
            .map_err(|e| KafkaError::SendMessageError(format!("marshalling error: {}", e)))?;

        let kafka_msg = ProducerMessage {
            topic: self.config.block_topic.clone(),
            key: Some(block_number.to_be_bytes().to_vec()),
            payload: json_data,
        };

        self.producer.send_message(kafka_msg).await?;
        Ok(())
    }

    pub async fn send_kafka_error_trigger(&self, block_number: u64) -> Result<(), KafkaError> {
        let message = ErrorTriggerMessage { block_number };

        let json_data = serde_json::to_vec(&message)
            .map_err(|e| KafkaError::SendMessageError(format!("marshalling error: {}", e)))?;

        let kafka_msg = ProducerMessage {
            topic: self.config.error_topic.clone(),
            key: Some(block_number.to_be_bytes().to_vec()),
            payload: json_data,
        };

        self.producer.send_message(kafka_msg).await?;
        Ok(())
    }
}
