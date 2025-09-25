# Chronik Stream v1.3.10 - Complete Kafka Compatibility Fix

## Release Date: September 25, 2025

## Summary

Comprehensive fixes ensuring full compatibility with kafka-python and other Kafka clients. Producer and AdminClient now work correctly.

## What's Fixed

### 1. MetadataResponse Field Ordering (v1.3.9)
- Fixed cluster_id and controller_id field order for v2+ versions
- Now correctly: brokers → cluster_id → controller_id → topics

### 2. API Version Advertisement (v1.3.10)
- Reduced Metadata API max version from 12 to 9 (matching implementation)
- Prevents clients from attempting unsupported versions

### 3. Full Client Compatibility
- **KafkaProducer**: ✅ Successfully sends messages
- **KafkaAdminClient**: ✅ Creates topics, lists topics, describes configs
- **KafkaConsumer**: ✅ Connects and lists topics

## Verification

All components tested and working:

```python
# Producer works
producer.send('test-topic', b'test message')  # ✅ Returns offset

# AdminClient works
admin.create_topics([NewTopic('new-topic')])  # ✅ Creates topic
admin.list_topics()  # ✅ Returns topic list

# Consumer works
consumer.topics()  # ✅ Returns available topics
```

## Technical Details

The issues were:
1. Field ordering mismatch in MetadataResponse encoder
2. Over-advertising API version support
3. Both Produce and FindCoordinator APIs were already implemented

## Next Euro

You owe yourself €3 for the optimistic announcements 😄

---

**Full Changelog**: https://github.com/chronik-stream/chronik-stream/compare/v1.3.9...v1.3.10