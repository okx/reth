// batch_producer.rs

use crate::defaults::DEFAULT_KAFKA_BUFFER_SIZE;
use crate::types::{KafkaConfig, KafkaError, ProducerMessage};
use rdkafka::{
    config::ClientConfig,
    producer::{FutureProducer, FutureRecord},
    util::Timeout,
};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub(crate) struct BatchProducer {
    buffer_tx: mpsc::Sender<ProducerMessage>,
    handle_task: JoinHandle<()>,
    shutdown_tx: mpsc::Sender<()>,
}

impl BatchProducer {
    pub(crate) async fn new(
        config: KafkaConfig,
        success_tx: Option<mpsc::Sender<()>>,
    ) -> Result<Self, KafkaError> {
        let producer: FutureProducer = ClientConfig::new()
            .set("bootstrap.servers", &config.bootstrap_servers.join(","))
            .set("client.id", &config.client_id)
            .set("compression.type", "lz4")
            .set("linger.ms", "3")
            .set("batch.num.messages", "100")
            .create()?;

        let (buffer_tx, buffer_rx) = mpsc::channel::<ProducerMessage>(DEFAULT_KAFKA_BUFFER_SIZE);
        let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>(1);

        let handle_task = tokio::spawn(Self::handle(producer, buffer_rx, shutdown_rx, success_tx));

        Ok(Self { buffer_tx, handle_task, shutdown_tx })
    }

    pub(crate) async fn send_message(&self, msg: ProducerMessage) -> Result<(), KafkaError> {
        self.buffer_tx.send(msg).await.map_err(|e| KafkaError::MessageForward(e.to_string()))
    }

    async fn handle(
        producer: FutureProducer,
        mut buffer_rx: mpsc::Receiver<ProducerMessage>,
        mut shutdown_rx: mpsc::Receiver<()>,
        success_tx: Option<mpsc::Sender<()>>,
    ) {
        loop {
            tokio::select! {
                Some(msg) = buffer_rx.recv() => {
                    let producer_clone = producer.clone();
                    let success_tx_clone = success_tx.clone();

                    tokio::spawn(async move{
                        let mut record = FutureRecord::to(&msg.topic).payload(&msg.payload);

                        if let Some(key) = &msg.key {
                            record = record.key(key);
                        }

                        match producer_clone.send(record, Timeout::After(Duration::from_secs(5))).await {
                            Ok(_) =>{
                                if let Some(tx) = success_tx_clone {
                                    let _ = tx.send(()).await;
                                }
                            }
                            Err((err, _)) => {
                                  tracing::error!("[Realtime] failed to send message: {:?}", KafkaError::Rdkafka(err));
                                }
                        }
                    });
                }
                _ = shutdown_rx.recv() => {
                    tracing::info!(target: "reth::realtime::kafka", "[Realtime] shutting down batch producer");
                    break;
                }
            }
        }
    }

    pub(crate) async fn try_close(self) -> Result<(), KafkaError> {
        self.shutdown_tx.send(()).await.map_err(|e| KafkaError::ChannelError(e.to_string()))?;

        self.handle_task.await.map_err(|e| KafkaError::ChannelError(e.to_string()))?;

        Ok(())
    }
}
