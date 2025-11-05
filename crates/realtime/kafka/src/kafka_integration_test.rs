// docker run -d --name kafka -p 19092:19092 -p 9092:9092 \
//  -e KAFKA_NODE_ID=1 -e KAFKA_PROCESS_ROLES=broker,controller \
//  -e KAFKA_LISTENERS=PLAINTEXT_INTERNAL://0.0.0.0:9092,PLAINTEXT_EXTERNAL://0.0.0.0:19092,CONTROLLER://0.0.0.0:9093 \
//  -e KAFKA_ADVERTISED_LISTENERS=PLAINTEXT_INTERNAL://kafka:9092,PLAINTEXT_EXTERNAL://localhost:19092 \
//  -e KAFKA_LISTENER_SECURITY_PROTOCOL_MAP=PLAINTEXT_INTERNAL:PLAINTEXT,PLAINTEXT_EXTERNAL:PLAINTEXT,CONTROLLER:PLAINTEXT \
//  -e KAFKA_INTER_BROKER_LISTENER_NAME=PLAINTEXT_INTERNAL \
//  -e KAFKA_CONTROLLER_LISTENER_NAMES=CONTROLLER \
//  -e KAFKA_CONTROLLER_QUORUM_VOTERS=1@localhost:9093 \
//  -e KAFKA_OFFSETS_TOPIC_REPLICATION_FACTOR=1 \
//  -e KAFKA_TRANSACTION_STATE_LOG_REPLICATION_FACTOR=1 \
//  -e KAFKA_TRANSACTION_STATE_LOG_MIN_ISR=1 \
//  -e KAFKA_GROUP_INITIAL_REBALANCE_DELAY_MS=0 \
//  -e KAFKA_AUTO_CREATE_TOPICS_ENABLE=true \
//  -e KAFKA_CLUSTER_ID=5L6g3nShT-eMCtK--X86sw \
//  apache/kafka:latest

#[cfg(test)]
mod tests {
    use crate::{
        consumer::KafkaConsumer,
        producer::KafkaProducer,
        types::{ErrorMsg, KafkaConfig, KafkaError},
    };
    use serde::{Deserialize, Serialize};
    use std::time::Duration;
    use tokio::sync::mpsc;
    use tracing_subscriber::{fmt, EnvFilter};

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct TestBlockMessage {
        number: u64,
        hash: String,
    }

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct TestTxMessage {
        tx_hash: String,
        block_number: u64,
    }

    #[tokio::test]
    async fn test_producer_consumer() {
        let _ = fmt().with_env_filter(EnvFilter::from_default_env()).with_test_writer().try_init();

        let bootstrap = "localhost:19092".to_string();

        let group_id = format!("test-group",);

        let producer_config = KafkaConfig {
            bootstrap_servers: vec![bootstrap.clone()],
            client_id: "test-producer".into(),
            group_id: group_id.clone(),
            block_topic: "test-blocks".into(),
            error_topic: "test-errors".into(),
            tx_topic: "test-txs".into(),
        };

        let (success_tx, mut success_rx) = mpsc::channel(100);

        let consumer_config = KafkaConfig {
            bootstrap_servers: vec![bootstrap.clone()],
            client_id: "test-consumer".into(),
            group_id,
            block_topic: "test-blocks".into(),
            error_topic: "test-errors".into(),
            tx_topic: "test-txs".into(),
        };

        let consumer = KafkaConsumer::new(consumer_config, false).unwrap();

        // Channels for receiving messages
        let (block_tx, mut block_rx) = mpsc::channel::<TestBlockMessage>(10);
        let (tx_tx, mut tx_rx) = mpsc::channel::<TestTxMessage>(10);
        let (error_tx, mut error_rx) = mpsc::channel::<ErrorMsg>(10);
        let (err_chan, _) = mpsc::channel::<KafkaError>(10);
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

        // Spawn consumer
        tokio::spawn(async move {
            consumer.consume_kafka(shutdown_rx, block_tx, tx_tx, error_tx, err_chan).await;
        });

        tokio::time::sleep(Duration::from_secs(2)).await;

        let producer = KafkaProducer::new(producer_config, Some(success_tx)).await.unwrap();

        // Send block message
        let block_msg = TestBlockMessage { number: 12345, hash: "0xabc123".into() };

        producer.send_kafka_block_info(12345, &block_msg).await.unwrap();

        // Wait for success callback
        tokio::time::timeout(Duration::from_secs(2), success_rx.recv()).await.unwrap();

        // Verify block received
        let received_block =
            tokio::time::timeout(Duration::from_secs(2), block_rx.recv()).await.unwrap().unwrap();
        assert_eq!(received_block, block_msg);
        println!("Block message successfully received");

        // Send transaction message
        let tx_msg = TestTxMessage { tx_hash: "0xdef456".into(), block_number: 12345 };

        producer.send_kafka_transaction("0xdef456".to_string(), &tx_msg).await.unwrap();

        // Wait for success callback
        tokio::time::timeout(Duration::from_secs(2), success_rx.recv()).await.unwrap();

        // Verify transaction received
        let received_tx =
            tokio::time::timeout(Duration::from_secs(2), tx_rx.recv()).await.unwrap().unwrap();
        assert_eq!(received_tx, tx_msg);
        println!("Transaction message successfully received");

        // Send error trigger
        producer.send_kafka_error_trigger(12345).await.unwrap();

        // Wait for success callback
        tokio::time::timeout(Duration::from_secs(2), success_rx.recv()).await.unwrap();

        let received_error =
            tokio::time::timeout(Duration::from_secs(2), error_rx.recv()).await.unwrap().unwrap();

        assert_eq!(received_error.block_number, 12345);
        println!("Error trigger successfully received");

        // Cleanup
        shutdown_tx.send(()).await.unwrap();
        producer.close().await.unwrap();
        println!("Kafka test passed");
    }
}
