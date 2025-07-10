#!/usr/bin/env python3
"""Create a topic in Chronik Stream by making an admin API call."""

import requests
import json
import sys

def create_topic(topic_name, partitions=1, replication_factor=1, host='localhost', port=8080):
    """Create a topic via admin API."""
    url = f"http://{host}:{port}/api/v1/topics"
    
    topic_config = {
        "name": topic_name,
        "partitions": partitions,
        "replication_factor": replication_factor,
        "config": {
            "retention.ms": str(7 * 24 * 60 * 60 * 1000),  # 7 days
            "segment.ms": str(60 * 60 * 1000),  # 1 hour
        }
    }
    
    try:
        response = requests.post(url, json=topic_config, timeout=10)
        
        if response.status_code == 200 or response.status_code == 201:
            print(f"Topic '{topic_name}' created successfully")
            return True
        elif response.status_code == 409:
            print(f"Topic '{topic_name}' already exists")
            return True
        else:
            print(f"Failed to create topic: {response.status_code}")
            print(response.text)
            return False
            
    except requests.exceptions.ConnectionError:
        print(f"Failed to connect to admin API at {host}:{port}")
        print("Is the admin service running?")
        return False
    except Exception as e:
        print(f"Error creating topic: {e}")
        return False

if __name__ == "__main__":
    if len(sys.argv) < 2:
        print("Usage: create_topic.py <topic_name> [partitions] [replication_factor]")
        sys.exit(1)
    
    topic_name = sys.argv[1]
    partitions = int(sys.argv[2]) if len(sys.argv) > 2 else 1
    replication_factor = int(sys.argv[3]) if len(sys.argv) > 3 else 1
    
    success = create_topic(topic_name, partitions, replication_factor)
    sys.exit(0 if success else 1)