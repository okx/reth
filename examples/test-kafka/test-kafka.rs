// examples/test_kafka.rs
use reth_realtime::kafka::{BatchProducer, KafkaConsumer, KafkaConfig};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let config = KafkaConfig {
        bootstrap_servers: vec!["localhost:9092".into()],
        client_id: "test".into(),
        block_topic: "blocks".into(),
    };

    // Test producer
    let producer = BatchProducer::new(config.clone(), None).await.unwrap();
    
    for i in 0..10 {
        let msg = ProducerMessage {
            topic: "blocks".into(),
            key: None,
            payload: format!(r#"{{"id":{}}}"#, i).into_bytes(),
        };
        producer.send_message(msg).await.unwrap();
        println!("Sent message {}", i);
    }

    tokio::time::sleep(Duration::from_secs(2)).await;
    println!("✓ Producer test complete");
}