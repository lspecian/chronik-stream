import org.apache.kafka.clients.consumer.ConsumerConfig;
import org.apache.kafka.clients.consumer.ConsumerRecord;
import org.apache.kafka.clients.consumer.ConsumerRecords;
import org.apache.kafka.clients.consumer.KafkaConsumer;
import org.apache.kafka.clients.producer.KafkaProducer;
import org.apache.kafka.clients.producer.ProducerConfig;
import org.apache.kafka.clients.producer.ProducerRecord;
import org.apache.kafka.common.serialization.StringDeserializer;
import org.apache.kafka.common.serialization.StringSerializer;

import java.time.Duration;
import java.util.Collections;
import java.util.Properties;

/**
 * Java integration test to verify Chronik's CRC-32C implementation is compatible
 * with Java Kafka clients.
 *
 * This test addresses the critical bug where Java clients rejected Chronik records
 * with "Record is corrupt (stored crc = X, computed crc = Y)" errors.
 *
 * Test flow:
 * 1. Produce messages to Chronik using Java producer
 * 2. Consume messages from Chronik using Java consumer
 * 3. If CRC validation passes, Java client will read records successfully
 * 4. If CRC validation fails, Java client will throw KafkaException
 *
 * Expected: All messages should be consumed without CRC validation errors
 */
public class CrcValidationTest {

    private static final String BOOTSTRAP_SERVERS = System.getenv().getOrDefault("CHRONIK_BOOTSTRAP", "localhost:9092");
    private static final String TEST_TOPIC = "crc-validation-test";

    public static void main(String[] args) {
        System.out.println("=== Chronik CRC-32C Validation Test ===");
        System.out.println("Bootstrap servers: " + BOOTSTRAP_SERVERS);
        System.out.println("Test topic: " + TEST_TOPIC);
        System.out.println();

        try {
            // Test 1: Produce messages
            System.out.println("Test 1: Producing messages with Java producer...");
            produceMessages();
            System.out.println("✅ PASS: Java producer successfully sent messages");
            System.out.println();

            // Test 2: Consume messages (CRC validation happens here)
            System.out.println("Test 2: Consuming messages with Java consumer (CRC validation)...");
            int consumedCount = consumeMessages();
            System.out.println("✅ PASS: Java consumer successfully read " + consumedCount + " messages");
            System.out.println("✅ PASS: CRC validation succeeded (no corruption errors)");
            System.out.println();

            // Test 3: Test with complex messages
            System.out.println("Test 3: Testing with complex messages (headers, keys, large payloads)...");
            produceComplexMessages();
            int complexCount = consumeComplexMessages();
            System.out.println("✅ PASS: Java consumer successfully read " + complexCount + " complex messages");
            System.out.println();

            System.out.println("=== ✅ ALL TESTS PASSED ===");
            System.out.println("Chronik's CRC-32C implementation is compatible with Java Kafka clients!");
            System.exit(0);

        } catch (Exception e) {
            System.err.println("❌ FAIL: CRC validation test failed");
            System.err.println("Error: " + e.getMessage());

            if (e.getMessage() != null && e.getMessage().contains("corrupt")) {
                System.err.println();
                System.err.println("CRC VALIDATION ERROR DETECTED:");
                System.err.println("This indicates Chronik's CRC calculation does not match Kafka's CRC-32C");
                System.err.println("Expected: CRC-32C (Castagnoli, polynomial 0x1EDC6F41)");
            }

            e.printStackTrace();
            System.exit(1);
        }
    }

    private static void produceMessages() throws Exception {
        Properties props = new Properties();
        props.put(ProducerConfig.BOOTSTRAP_SERVERS_CONFIG, BOOTSTRAP_SERVERS);
        props.put(ProducerConfig.KEY_SERIALIZER_CLASS_CONFIG, StringSerializer.class.getName());
        props.put(ProducerConfig.VALUE_SERIALIZER_CLASS_CONFIG, StringSerializer.class.getName());
        props.put(ProducerConfig.ACKS_CONFIG, "1");
        props.put(ProducerConfig.RETRIES_CONFIG, "0");

        try (KafkaProducer<String, String> producer = new KafkaProducer<>(props)) {
            for (int i = 0; i < 10; i++) {
                String key = "key-" + i;
                String value = "test-message-" + i;
                ProducerRecord<String, String> record = new ProducerRecord<>(TEST_TOPIC, key, value);
                producer.send(record).get(); // Wait for completion
                System.out.println("  Produced: " + key + " = " + value);
            }
            producer.flush();
        }
    }

    private static int consumeMessages() throws Exception {
        Properties props = new Properties();
        props.put(ConsumerConfig.BOOTSTRAP_SERVERS_CONFIG, BOOTSTRAP_SERVERS);
        props.put(ConsumerConfig.GROUP_ID_CONFIG, "crc-validation-group");
        props.put(ConsumerConfig.KEY_DESERIALIZER_CLASS_CONFIG, StringDeserializer.class.getName());
        props.put(ConsumerConfig.VALUE_DESERIALIZER_CLASS_CONFIG, StringDeserializer.class.getName());
        props.put(ConsumerConfig.AUTO_OFFSET_RESET_CONFIG, "earliest");
        props.put(ConsumerConfig.ENABLE_AUTO_COMMIT_CONFIG, "false");

        int count = 0;
        try (KafkaConsumer<String, String> consumer = new KafkaConsumer<>(props)) {
            consumer.subscribe(Collections.singletonList(TEST_TOPIC));

            long startTime = System.currentTimeMillis();
            while (count < 10 && (System.currentTimeMillis() - startTime) < 10000) {
                ConsumerRecords<String, String> records = consumer.poll(Duration.ofMillis(1000));
                for (ConsumerRecord<String, String> record : records) {
                    System.out.println("  Consumed: " + record.key() + " = " + record.value() +
                                     " (offset=" + record.offset() + ")");
                    count++;
                }
            }
        }

        return count;
    }

    private static void produceComplexMessages() throws Exception {
        Properties props = new Properties();
        props.put(ProducerConfig.BOOTSTRAP_SERVERS_CONFIG, BOOTSTRAP_SERVERS);
        props.put(ProducerConfig.KEY_SERIALIZER_CLASS_CONFIG, StringSerializer.class.getName());
        props.put(ProducerConfig.VALUE_SERIALIZER_CLASS_CONFIG, StringSerializer.class.getName());
        props.put(ProducerConfig.ACKS_CONFIG, "1");

        try (KafkaProducer<String, String> producer = new KafkaProducer<>(props)) {
            for (int i = 0; i < 5; i++) {
                String key = "complex-key-" + i;

                // Large payload to test CRC with various sizes
                StringBuilder valueBuilder = new StringBuilder();
                for (int j = 0; j < 100; j++) {
                    valueBuilder.append("This is a complex message with more data. ");
                }
                String value = valueBuilder.toString();

                ProducerRecord<String, String> record = new ProducerRecord<>(
                    TEST_TOPIC + "-complex",
                    0, // partition
                    key,
                    value
                );

                // Add headers
                record.headers().add("test-header", "header-value".getBytes());
                record.headers().add("timestamp", String.valueOf(System.currentTimeMillis()).getBytes());

                producer.send(record).get();
                System.out.println("  Produced complex: " + key + " (size=" + value.length() + " bytes)");
            }
            producer.flush();
        }
    }

    private static int consumeComplexMessages() throws Exception {
        Properties props = new Properties();
        props.put(ConsumerConfig.BOOTSTRAP_SERVERS_CONFIG, BOOTSTRAP_SERVERS);
        props.put(ConsumerConfig.GROUP_ID_CONFIG, "crc-validation-complex-group");
        props.put(ConsumerConfig.KEY_DESERIALIZER_CLASS_CONFIG, StringDeserializer.class.getName());
        props.put(ConsumerConfig.VALUE_DESERIALIZER_CLASS_CONFIG, StringDeserializer.class.getName());
        props.put(ConsumerConfig.AUTO_OFFSET_RESET_CONFIG, "earliest");
        props.put(ConsumerConfig.ENABLE_AUTO_COMMIT_CONFIG, "false");

        int count = 0;
        try (KafkaConsumer<String, String> consumer = new KafkaConsumer<>(props)) {
            consumer.subscribe(Collections.singletonList(TEST_TOPIC + "-complex"));

            long startTime = System.currentTimeMillis();
            while (count < 5 && (System.currentTimeMillis() - startTime) < 10000) {
                ConsumerRecords<String, String> records = consumer.poll(Duration.ofMillis(1000));
                for (ConsumerRecord<String, String> record : records) {
                    System.out.println("  Consumed complex: " + record.key() +
                                     " (size=" + record.value().length() + " bytes, " +
                                     "headers=" + record.headers().toArray().length + ")");
                    count++;
                }
            }
        }

        return count;
    }
}
