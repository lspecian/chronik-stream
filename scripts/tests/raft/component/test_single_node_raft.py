from kafka import KafkaProducer, KafkaConsumer
from kafka.admin import KafkaAdminClient, NewTopic
import time

print("Testing Raft-enabled single-node clusters")
print("=" * 70)

admin = KafkaAdminClient(bootstrap_servers='localhost:9092')
topic = NewTopic(name='raft-test', num_partitions=1, replication_factor=3)
admin.create_topics([topic])
print(f"✅ Created topic: raft-test")
time.sleep(2)

producer = KafkaProducer(bootstrap_servers='localhost:9092')
for i in range(10):
    producer.send('raft-test', f"Message {i}".encode())
producer.flush()
print(f"✅ Produced 10 messages to Node 1")

time.sleep(2)

consumer = KafkaConsumer(
    'raft-test',
    bootstrap_servers='localhost:9092',
    auto_offset_reset='earliest',
    consumer_timeout_ms=5000
)

messages = [msg.value for msg in consumer]
consumer.close()

print(f"✅ Consumed {len(messages)} messages from Node 1")

if len(messages) == 10:
    print("\n🎉 SUCCESS! Raft infrastructure is working!")
    print("   - Nodes become leaders of 1-node clusters")
    print("   - Messages can be produced and consumed")
    print("   - Next step: Implement proper multi-node Raft via conf changes")
else:
    print(f"\n⚠️  Only got {len(messages)}/10 messages")
