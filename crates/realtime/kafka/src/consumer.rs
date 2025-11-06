// consumer.rs

use crate::types::{KafkaConfig, KafkaError};
use rdkafka::{
    config::ClientConfig,
    consumer::{CommitMode, Consumer, StreamConsumer},
    Message,
};
use reth_optimism_flashblocks::FlashBlock;
use serde::Deserialize;
use tokio::sync::mpsc;

/// Kafka consumer
pub struct KafkaConsumer {
    consumer: StreamConsumer,
    config: KafkaConfig,
}

impl std::fmt::Debug for KafkaConsumer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KafkaConsumer").field("config", &self.config).finish()
    }
}

impl KafkaConsumer {
    /// Creates a new Kafka consumer
    pub fn new(config: KafkaConfig, latest_flag: bool) -> Result<Self, KafkaError> {
        let offset = if latest_flag { "latest" } else { "earliest" };

        let consumer: StreamConsumer = ClientConfig::new()
            .set("bootstrap.servers", &config.bootstrap_servers.join(","))
            .set("client.id", &config.client_id)
            .set("group.id", &config.group_id)
            .set("auto.offset.reset", offset)
            .set("enable.auto.commit", "false")
            .create()
            .map_err(|e| KafkaError::ConsumerCreation(e.to_string()))?;

        Ok(Self { consumer, config })
    }

    /// Consumes messages from Kafka
    pub async fn consume_kafka<BlockInfo, TxMsg, ErrorMsg>(
        &self,
        mut shutdown_rx: mpsc::Receiver<()>,
        block_tx: mpsc::Sender<BlockInfo>,
        tx_tx: mpsc::Sender<TxMsg>,
        error_tx: mpsc::Sender<ErrorMsg>,
        flashblock_tx: mpsc::Sender<FlashBlock>,
        error_chan: mpsc::Sender<KafkaError>,
    ) where
        BlockInfo: for<'de> Deserialize<'de> + Send + 'static,
        TxMsg: for<'de> Deserialize<'de> + Send + 'static,
        ErrorMsg: for<'de> Deserialize<'de> + Send + 'static,
    {
        let topics = vec![
            self.config.block_topic.as_str(),
            self.config.tx_topic.as_str(),
            self.config.error_topic.as_str(),
            self.config.flashblock_topic.as_str(),
        ];

        if let Err(e) = self.consumer.subscribe(&topics) {
            let _ = error_chan.send(KafkaError::Rdkafka(e)).await;
            return;
        }

        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    tracing::info!(target: "reth::realtime::kafka","[Realtime] shutting down kafka consumption");
                    break;
                }

                res = self.consumer.recv() => {
                    if let Ok(ref message) = res {
                        let topic = message.topic();
                        let payload = match message.payload() {
                            Some(p) => p,
                            None => {
                                tracing::warn!(target: "reth::realtime::kafka", "[Realtime] Empty message payload");
                                continue;
                            }
                        };

                        let send_msg;

                        match topic {
                            t if t == self.config.block_topic => {
                                send_msg = match serde_json::from_slice::<BlockInfo>(payload) {
                                    Ok(block_msg) =>block_tx.send(block_msg).await.map_err(|e| KafkaError::ChannelError(e.to_string())),
                                    Err(e) => {tracing::warn!(target: "reth::realtime::kafka", "[Realtime] Error unmarshalling block info message {:?}", e);
                                    let _ = error_chan.send(KafkaError::Deserialization(e)).await;
                                    continue;
                                }
                                };
                            }
                            t if t == self.config.tx_topic => {
                                send_msg = match serde_json::from_slice::<TxMsg>(payload) {
                                    Ok(tx_msg) => tx_tx.send(tx_msg).await.map_err(|e| KafkaError::ChannelError(e.to_string())),
                                    Err(e) => {tracing::warn!(target: "reth::realtime::kafka", "[Realtime] Error unmarshalling transaction message {:?}", e);
                                    let _ = error_chan.send(KafkaError::Deserialization(e)).await;
                                    continue;
                                }
                                };
                            }
                            t if t == self.config.error_topic => {
                                send_msg = match serde_json::from_slice::<ErrorMsg>(payload) {
                                    Ok(error_msg) => error_tx.send(error_msg).await.map_err(|e| KafkaError::ChannelError(e.to_string())),
                                    Err(e) => {tracing::warn!(target: "reth::realtime::kafka", "[Realtime] Error unmarshalling error message {:?}", e);
                                    let _ = error_chan.send(KafkaError::Deserialization(e)).await;
                                    continue;
                                }
                                };
                            }
                            t if t == self.config.flashblock_topic => {
                                send_msg = match serde_json::from_slice::<FlashBlock>(payload) {
                                    Ok(flashblock_msg) => flashblock_tx.send(flashblock_msg).await.map_err(|e| KafkaError::ChannelError(e.to_string())),
                                    Err(e) => {tracing::warn!(target: "reth::realtime::kafka", "[Realtime] Error unmarshalling flashblock message {:?}", e);
                                    let _ = error_chan.send(KafkaError::Deserialization(e)).await;
                                    continue;
                                }
                                };
                            }
                            _ => {
                                tracing::warn!(target: "reth::realtime::kafka", "[Realtime] unknown topic");
                                continue;
                            }
                        }

                        if send_msg.is_ok() {
                            if let Err(e) = self.consumer.commit_message(&message, CommitMode::Async) {
                                tracing::warn!(target: "reth::realtime::kafka", "[Realtime] failed to commit: {:?}", e);
                                let _ = error_chan.send(KafkaError::CommitMessageError(e.to_string())).await;
                            }
                        }

                    }
                    else {
                        tracing::warn!(target: "reth::realtime::kafka", "[Realtime] Kafka error: {:?}", res.err());
                    }
                }
            }
        }
    }

    /// Closes the Kafka consumer
    pub fn close(self) {
        self.consumer.unsubscribe()
    }
}
