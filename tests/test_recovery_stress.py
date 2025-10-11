#!/usr/bin/env python3
"""
Stress test for WAL recovery with large message counts
Tests: 5K, 10K, 50K messages
"""

import subprocess
import time
import sys
from kafka import KafkaProducer, KafkaConsumer
from kafka.errors import KafkaError

def start_server():
    """Start chronik-server"""
    proc = subprocess.Popen(
        ['./target/debug/chronik-server'],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL
    )
    time.sleep(3)  # Wait for server to start
    return proc

def stop_server(proc):
    """Stop server gracefully or forcefully"""
    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()

def test_recovery_at_scale(message_count):
    """Test recovery with specified message count"""
    print(f"\n{'='*60}")
    print(f"🧪 Testing WAL Recovery with {message_count:,} messages")
    print(f"{'='*60}")

    # Step 1: Start server
    print("1️⃣  Starting server...")
    server = start_server()

    try:
        # Step 2: Produce messages
        print(f"2️⃣  Producing {message_count:,} messages with acks=1...")
        producer = KafkaProducer(
            bootstrap_servers='localhost:9092',
            acks=1,
            request_timeout_ms=30000,
            max_block_ms=30000,
            linger_ms=10,  # Batch messages for 10ms
            batch_size=16384,  # 16KB batches
        )

        produce_start = time.time()
        futures = []

        # Send all messages asynchronously (don't wait for each one)
        for i in range(message_count):
            try:
                future = producer.send('stress-test', f'Message {i}'.encode())
                futures.append(future)

                # Progress indicator
                if (i + 1) % 1000 == 0:
                    print(f"   📤 {i + 1:,} messages sent")

            except Exception as e:
                print(f"   ❌ Failed to send message {i}: {e}")
                producer.close()
                stop_server(server)
                return False

        # Now wait for all acks
        print(f"   ⏳ Waiting for all {len(futures):,} acks...")
        acked_count = 0
        for i, future in enumerate(futures):
            try:
                future.get(timeout=10)
                acked_count += 1
                if (i + 1) % 1000 == 0:
                    print(f"   ✅ {i + 1:,} messages acked")
            except Exception as e:
                print(f"   ❌ Failed to get ack for message {i}: {e}")

        producer.flush()
        producer.close()
        produce_duration = time.time() - produce_start

        print(f"   All {acked_count:,} messages produced and acked in {produce_duration:.2f}s")
        print(f"   Throughput: {acked_count / produce_duration:.0f} msgs/sec")

        # Step 3: Simulate crash
        print("3️⃣  💥 Killing server (simulating crash)...")
        stop_server(server)
        time.sleep(2)

        # Step 4: Restart server (recovery happens)
        print("4️⃣  🔄 Restarting server (recovery should happen)...")
        server = start_server()
        time.sleep(5)  # Give more time for recovery of large datasets

        # Step 5: Consume messages
        print("5️⃣  Consuming messages from beginning...")
        consumer = KafkaConsumer(
            'stress-test',
            bootstrap_servers='localhost:9092',
            auto_offset_reset='earliest',
            consumer_timeout_ms=10000,
            max_poll_records=1000
        )

        consume_start = time.time()
        consumed_messages = []
        message_ids = set()
        duplicates = []

        for message in consumer:
            msg = message.value.decode()
            consumed_messages.append(msg)

            # Extract message ID to detect duplicates
            msg_id = msg.replace('Message ', '')
            if msg_id in message_ids:
                duplicates.append(msg)
            else:
                message_ids.add(msg_id)

            # Progress indicator
            if len(consumed_messages) % 1000 == 0:
                print(f"   📥 {len(consumed_messages):,} messages consumed")

        consumer.close()
        consume_duration = time.time() - consume_start

        # Step 6: Verify results
        print(f"\n📊 Results:")
        print(f"   Produced: {acked_count:,} messages")
        print(f"   Consumed: {len(consumed_messages):,} messages")
        print(f"   Unique:   {len(message_ids):,} messages")
        print(f"   Duplicates: {len(duplicates):,}")
        print(f"   Consume throughput: {len(consumed_messages) / consume_duration:.0f} msgs/sec")

        if duplicates:
            print(f"\n❌ DUPLICATE DETECTION:")
            print(f"   First 10 duplicates: {duplicates[:10]}")

        # Calculate loss
        loss = acked_count - len(message_ids)
        loss_percent = (loss / acked_count * 100) if acked_count > 0 else 0

        if loss == 0 and len(duplicates) == 0:
            print(f"✅ SUCCESS: 100% message recovery, ZERO duplicates")
            return True
        elif loss > 0:
            print(f"❌ FAILURE: {loss:,} messages lost ({loss_percent:.1f}% loss)")
            return False
        elif len(duplicates) > 0:
            print(f"❌ FAILURE: {len(duplicates):,} duplicate messages found")
            return False
        else:
            # More messages than produced?
            extra = len(message_ids) - acked_count
            print(f"⚠️  WARNING: {extra:,} extra messages recovered (data corruption?)")
            return False

    finally:
        stop_server(server)
        time.sleep(1)

def main():
    """Run stress tests at multiple scales"""
    test_sizes = [5000, 10000, 50000]
    results = {}

    print("🚀 WAL Recovery Stress Testing")
    print("Testing scales: 5K, 10K, 50K messages")
    print("=" * 60)

    for size in test_sizes:
        success = test_recovery_at_scale(size)
        results[size] = success

        if not success:
            print(f"\n⚠️  Test failed at {size:,} messages. Stopping.")
            break

        # Clean data between tests
        subprocess.run(['rm', '-rf', 'data'], check=False)
        subprocess.run(['mkdir', '-p', 'data'], check=False)
        time.sleep(2)

    # Final summary
    print(f"\n{'='*60}")
    print("📋 FINAL SUMMARY")
    print(f"{'='*60}")
    for size, success in results.items():
        status = "✅ PASS" if success else "❌ FAIL"
        print(f"  {size:>6,} messages: {status}")

    all_passed = all(results.values())
    if all_passed:
        print(f"\n🎉 All stress tests PASSED!")
        return 0
    else:
        print(f"\n❌ Some stress tests FAILED")
        return 1

if __name__ == '__main__':
    sys.exit(main())
