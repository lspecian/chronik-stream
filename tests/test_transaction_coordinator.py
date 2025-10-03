#!/usr/bin/env python3
"""Test Transaction Coordinator implementation"""

from kafka import KafkaProducer
from kafka.errors import KafkaError
import sys
import time

def test_transactional_producer():
    """Test the transaction coordinator with a transactional producer"""
    try:
        # Create a transactional producer
        producer = KafkaProducer(
            bootstrap_servers='localhost:9092',
            transactional_id='test-transaction-1',
            transaction_timeout_ms=30000
        )
        
        print("Created transactional producer successfully")
        
        # Initialize transactions (this will call InitProducerIdRequest)
        print("Initializing producer ID...")
        producer.init_transactions()
        print("Producer ID initialized successfully")
        
        # Begin a transaction
        print("Beginning transaction...")
        producer.begin_transaction()
        print("Transaction begun successfully")
        
        # Send a message
        print("Sending transactional message...")
        future = producer.send('test-topic', b'transactional message')
        result = future.get(timeout=10)
        print(f"Message sent: {result}")
        
        # Commit the transaction
        print("Committing transaction...")
        producer.commit_transaction()
        print("Transaction committed successfully")
        
        producer.close()
        print("\nTransaction Coordinator test PASSED!")
        return True
        
    except Exception as e:
        print(f"Transaction test failed: {e}")
        print(f"Error type: {type(e)}")
        return False

if __name__ == "__main__":
    success = test_transactional_producer()
    sys.exit(0 if success else 1)
