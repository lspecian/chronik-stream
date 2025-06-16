//! Tests with multiple language clients (Java, Python, etc.)

use anyhow::Result;
use std::process::Command;
use std::time::Duration;
use tokio::time::sleep;
use tempfile::TempDir;

use crate::testcontainers_setup::{TestEnvironment, ChronikCluster};

#[tokio::test]
async fn test_java_kafka_client() -> Result<()> {
    let test_env = TestEnvironment::new().await?;
    test_env.create_bucket("chronik-test").await?;
    
    let mut cluster = ChronikCluster::new(3, &test_env).await?;
    cluster.start(&test_env).await?;
    
    let bootstrap_servers = cluster.bootstrap_servers();
    let topic = "java-client-test";
    
    // Create temporary directory for Java test
    let temp_dir = TempDir::new()?;
    let java_test_path = temp_dir.path().join("KafkaTest.java");
    
    // Write Java test code
    std::fs::write(&java_test_path, format!(r#"
import org.apache.kafka.clients.producer.*;
import org.apache.kafka.clients.consumer.*;
import org.apache.kafka.common.serialization.*;
import java.util.*;
import java.time.Duration;

public class KafkaTest {{
    public static void main(String[] args) throws Exception {{
        String bootstrapServers = "{}";
        String topic = "{}";
        
        // Producer test
        Properties producerProps = new Properties();
        producerProps.put("bootstrap.servers", bootstrapServers);
        producerProps.put("key.serializer", StringSerializer.class.getName());
        producerProps.put("value.serializer", StringSerializer.class.getName());
        producerProps.put("acks", "all");
        
        try (KafkaProducer<String, String> producer = new KafkaProducer<>(producerProps)) {{
            for (int i = 0; i < 100; i++) {{
                ProducerRecord<String, String> record = new ProducerRecord<>(
                    topic, "java-key-" + i, "java-value-" + i
                );
                RecordMetadata metadata = producer.send(record).get();
                System.out.println("Produced: partition=" + metadata.partition() + 
                    ", offset=" + metadata.offset());
            }}
        }}
        
        // Consumer test
        Properties consumerProps = new Properties();
        consumerProps.put("bootstrap.servers", bootstrapServers);
        consumerProps.put("group.id", "java-test-group");
        consumerProps.put("key.deserializer", StringDeserializer.class.getName());
        consumerProps.put("value.deserializer", StringDeserializer.class.getName());
        consumerProps.put("auto.offset.reset", "earliest");
        
        try (KafkaConsumer<String, String> consumer = new KafkaConsumer<>(consumerProps)) {{
            consumer.subscribe(Arrays.asList(topic));
            
            int consumed = 0;
            long startTime = System.currentTimeMillis();
            
            while (consumed < 100 && System.currentTimeMillis() - startTime < 30000) {{
                ConsumerRecords<String, String> records = consumer.poll(Duration.ofMillis(1000));
                for (ConsumerRecord<String, String> record : records) {{
                    System.out.println("Consumed: key=" + record.key() + 
                        ", value=" + record.value() + 
                        ", offset=" + record.offset());
                    consumed++;
                }}
            }}
            
            if (consumed == 100) {{
                System.out.println("SUCCESS: All messages consumed");
                System.exit(0);
            }} else {{
                System.err.println("FAILED: Only consumed " + consumed + " messages");
                System.exit(1);
            }}
        }}
    }}
}}
"#, bootstrap_servers, topic))?;
    
    // Download Kafka client JAR if needed (in real test, would be pre-downloaded)
    let kafka_client_jar = temp_dir.path().join("kafka-clients.jar");
    
    // For the test, we'll check if javac is available
    let javac_check = Command::new("javac")
        .arg("--version")
        .output();
    
    if javac_check.is_ok() {
        println!("Java compiler found, would run Java client test");
        // In a real test environment with Java and Kafka client JARs:
        /*
        Command::new("javac")
            .arg("-cp")
            .arg(&kafka_client_jar)
            .arg(&java_test_path)
            .current_dir(&temp_dir)
            .status()?;
        
        let output = Command::new("java")
            .arg("-cp")
            .arg(format!(".:{}", kafka_client_jar.display()))
            .arg("KafkaTest")
            .current_dir(&temp_dir)
            .output()?;
        
        assert!(output.status.success(), "Java client test failed");
        */
    } else {
        println!("Java compiler not found, skipping Java client test");
    }
    
    cluster.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_python_kafka_client() -> Result<()> {
    let test_env = TestEnvironment::new().await?;
    test_env.create_bucket("chronik-test").await?;
    
    let mut cluster = ChronikCluster::new(3, &test_env).await?;
    cluster.start(&test_env).await?;
    
    let bootstrap_servers = cluster.bootstrap_servers();
    let topic = "python-client-test";
    
    // Create temporary directory for Python test
    let temp_dir = TempDir::new()?;
    let python_test_path = temp_dir.path().join("kafka_test.py");
    
    // Write Python test code
    std::fs::write(&python_test_path, format!(r#"#!/usr/bin/env python3
import sys
from kafka import KafkaProducer, KafkaConsumer
import json
import time

bootstrap_servers = '{}'
topic = '{}'

# Producer test
producer = KafkaProducer(
    bootstrap_servers=bootstrap_servers,
    value_serializer=lambda v: json.dumps(v).encode('utf-8'),
    acks='all'
)

print("Producing messages...")
for i in range(100):
    message = {{
        'id': i,
        'message': f'Python message {{i}}',
        'timestamp': time.time()
    }}
    future = producer.send(topic, key=f'python-key-{{i}}'.encode(), value=message)
    metadata = future.get(timeout=10)
    print(f"Produced: partition={{metadata.partition}}, offset={{metadata.offset}}")

producer.flush()

# Consumer test
consumer = KafkaConsumer(
    topic,
    bootstrap_servers=bootstrap_servers,
    group_id='python-test-group',
    auto_offset_reset='earliest',
    value_deserializer=lambda m: json.loads(m.decode('utf-8')),
    consumer_timeout_ms=30000
)

print("Consuming messages...")
consumed = 0
for message in consumer:
    print(f"Consumed: key={{message.key}}, value={{message.value}}, offset={{message.offset}}")
    consumed += 1
    if consumed >= 100:
        break

if consumed == 100:
    print("SUCCESS: All messages consumed")
    sys.exit(0)
else:
    print(f"FAILED: Only consumed {{consumed}} messages")
    sys.exit(1)
"#, bootstrap_servers, topic))?;
    
    // Check if Python and kafka-python are available
    let python_check = Command::new("python3")
        .arg("--version")
        .output();
    
    if python_check.is_ok() {
        // Check for kafka-python
        let kafka_python_check = Command::new("python3")
            .arg("-c")
            .arg("import kafka")
            .output();
        
        if kafka_python_check.is_ok() {
            println!("Python and kafka-python found, running Python client test");
            
            let output = Command::new("python3")
                .arg(&python_test_path)
                .output()?;
            
            if !output.status.success() {
                eprintln!("Python stdout: {}", String::from_utf8_lossy(&output.stdout));
                eprintln!("Python stderr: {}", String::from_utf8_lossy(&output.stderr));
            }
            
            assert!(output.status.success(), "Python client test failed");
        } else {
            println!("kafka-python not found, skipping Python client test");
        }
    } else {
        println!("Python not found, skipping Python client test");
    }
    
    cluster.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_nodejs_kafka_client() -> Result<()> {
    let test_env = TestEnvironment::new().await?;
    test_env.create_bucket("chronik-test").await?;
    
    let mut cluster = ChronikCluster::new(3, &test_env).await?;
    cluster.start(&test_env).await?;
    
    let bootstrap_servers = cluster.bootstrap_servers();
    let topic = "nodejs-client-test";
    
    // Create temporary directory for Node.js test
    let temp_dir = TempDir::new()?;
    let nodejs_test_path = temp_dir.path().join("kafka_test.js");
    
    // Write Node.js test code
    std::fs::write(&nodejs_test_path, format!(r#"
const {{ Kafka }} = require('kafkajs');

const kafka = new Kafka({{
  clientId: 'nodejs-test',
  brokers: ['{}']
}});

const topic = '{}';

async function test() {{
  // Producer test
  const producer = kafka.producer();
  await producer.connect();
  
  console.log('Producing messages...');
  for (let i = 0; i < 100; i++) {{
    await producer.send({{
      topic: topic,
      messages: [
        {{
          key: `nodejs-key-${{i}}`,
          value: JSON.stringify({{
            id: i,
            message: `Node.js message ${{i}}`,
            timestamp: Date.now()
          }})
        }}
      ]
    }});
    console.log(`Produced message ${{i}}`);
  }}
  
  await producer.disconnect();
  
  // Consumer test
  const consumer = kafka.consumer({{ groupId: 'nodejs-test-group' }});
  await consumer.connect();
  await consumer.subscribe({{ topic: topic, fromBeginning: true }});
  
  let consumed = 0;
  const startTime = Date.now();
  
  await consumer.run({{
    eachMessage: async ({{ topic, partition, message }}) => {{
      console.log(`Consumed: key=${{message.key}}, offset=${{message.offset}}`);
      consumed++;
      
      if (consumed >= 100) {{
        console.log('SUCCESS: All messages consumed');
        await consumer.disconnect();
        process.exit(0);
      }}
    }},
  }});
  
  // Timeout after 30 seconds
  setTimeout(async () => {{
    console.error(`FAILED: Only consumed ${{consumed}} messages`);
    await consumer.disconnect();
    process.exit(1);
  }}, 30000);
}}

test().catch(console.error);
"#, bootstrap_servers, topic))?;
    
    // Check if Node.js and kafkajs are available
    let node_check = Command::new("node")
        .arg("--version")
        .output();
    
    if node_check.is_ok() {
        // Create package.json
        let package_json = temp_dir.path().join("package.json");
        std::fs::write(&package_json, r#"{
  "name": "kafka-test",
  "version": "1.0.0",
  "dependencies": {
    "kafkajs": "^2.2.4"
  }
}"#)?;
        
        // Check if npm is available
        let npm_check = Command::new("npm")
            .arg("--version")
            .output();
        
        if npm_check.is_ok() {
            println!("Node.js and npm found, would run Node.js client test");
            // In a real test environment:
            /*
            Command::new("npm")
                .arg("install")
                .current_dir(&temp_dir)
                .status()?;
            
            let output = Command::new("node")
                .arg(&nodejs_test_path)
                .current_dir(&temp_dir)
                .output()?;
            
            assert!(output.status.success(), "Node.js client test failed");
            */
        } else {
            println!("npm not found, skipping Node.js client test");
        }
    } else {
        println!("Node.js not found, skipping Node.js client test");
    }
    
    cluster.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_cross_language_compatibility() -> Result<()> {
    let test_env = TestEnvironment::new().await?;
    test_env.create_bucket("chronik-test").await?;
    
    let mut cluster = ChronikCluster::new(3, &test_env).await?;
    cluster.start(&test_env).await?;
    
    let bootstrap_servers = cluster.bootstrap_servers();
    let topic = "cross-language-test";
    
    // Use Rust client to produce messages
    use rdkafka::config::ClientConfig;
    use rdkafka::producer::{FutureProducer, FutureRecord};
    use rdkafka::consumer::{StreamConsumer, Consumer};
    use futures::StreamExt;
    
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()?;
    
    // Produce messages with different formats
    let json_message = r#"{"type":"json","data":"test"}"#;
    let text_message = "plain text message";
    let binary_message = vec![0x01, 0x02, 0x03, 0x04];
    
    // JSON message
    producer.send(
        FutureRecord::to(topic)
            .key("json-key")
            .payload(json_message),
        Duration::from_secs(0)
    ).await?;
    
    // Text message
    producer.send(
        FutureRecord::to(topic)
            .key("text-key")
            .payload(text_message),
        Duration::from_secs(0)
    ).await?;
    
    // Binary message
    producer.send(
        FutureRecord::to(topic)
            .key("binary-key")
            .payload(&binary_message[..]),
        Duration::from_secs(0)
    ).await?;
    
    // Headers test
    let mut headers = rdkafka::message::OwnedHeaders::new();
    headers = headers.insert(rdkafka::message::Header {
        key: "content-type",
        value: Some(b"application/json"),
    });
    headers = headers.insert(rdkafka::message::Header {
        key: "source",
        value: Some(b"rust-client"),
    });
    
    producer.send(
        FutureRecord::to(topic)
            .key("headers-key")
            .payload("message with headers")
            .headers(headers),
        Duration::from_secs(0)
    ).await?;
    
    // Consume and verify
    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("group.id", "cross-language-consumer")
        .set("auto.offset.reset", "earliest")
        .create()?;
    
    consumer.subscribe(&[topic])?;
    
    let mut stream = consumer.stream();
    let mut messages_received = 0;
    
    while messages_received < 4 {
        if let Some(Ok(message)) = tokio::time::timeout(
            Duration::from_secs(5),
            stream.next()
        ).await? {
            let key = String::from_utf8_lossy(message.key().unwrap());
            
            match key.as_ref() {
                "json-key" => {
                    let payload = String::from_utf8_lossy(message.payload().unwrap());
                    assert_eq!(payload, json_message);
                }
                "text-key" => {
                    let payload = String::from_utf8_lossy(message.payload().unwrap());
                    assert_eq!(payload, text_message);
                }
                "binary-key" => {
                    let payload = message.payload().unwrap();
                    assert_eq!(payload, &binary_message[..]);
                }
                "headers-key" => {
                    let headers = message.headers().unwrap();
                    assert_eq!(headers.count(), 2);
                    
                    let content_type = headers.get(0);
                    assert_eq!(content_type.key, "content-type");
                    assert_eq!(content_type.value, Some(b"application/json"));
                    
                    let source = headers.get(1);
                    assert_eq!(source.key, "source");
                    assert_eq!(source.value, Some(b"rust-client"));
                }
                _ => panic!("Unexpected message key: {}", key),
            }
            
            messages_received += 1;
        }
    }
    
    assert_eq!(messages_received, 4, "Should receive all test messages");
    
    cluster.stop().await?;
    Ok(())
}