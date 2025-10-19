from kafka.admin import KafkaAdminClient, NewTopic
import time

admin = KafkaAdminClient(bootstrap_servers='localhost:9092')
topic = NewTopic(name=f'debug-test-{int(time.time())}', num_partitions=1, replication_factor=3)
admin.create_topics([topic])
print(f"Created topic: {topic.name}")
time.sleep(2)
