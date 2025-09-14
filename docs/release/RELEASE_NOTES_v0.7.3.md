# Release v0.7.3: Consumer Group Coordination & Fetch API Fixes

## Overview
This release delivers critical fixes for Kafka Consumer Group coordination and FetchResponse serialization, enabling full compatibility with standard Kafka clients including kafka-python and confluent-kafka.

## 🎯 Key Improvements

### Consumer Group Coordination
- **Fixed partition assignment logic**: SyncGroup handler now properly computes and distributes partition assignments when the leader provides empty assignments
- **Round-robin partition assignment**: Implemented proper round-robin distribution of partitions among consumer group members
- **OffsetFetch improvements**: Returns default offset 0 for partitions with no committed offsets, ensuring consumers can start from the beginning

### FetchResponse Serialization
- **Fixed client crashes**: Resolved critical issue where kafka-python would crash with `TypeError` when polling past the end of topics
- **Spec-compliant empty responses**: FetchResponse now returns properly encoded empty byte arrays (length 0) instead of NULL for partitions with no data
- **Iterator mode compatibility**: Kafka consumers using iterator patterns no longer crash when no new messages are available

## 🐛 Bug Fixes
- Fixed SyncGroup to compute assignments when leader has empty assignment list
- Fixed OffsetFetch to return all requested partitions with appropriate defaults
- Fixed FetchResponse encoding to prevent NULL records field
- Added comprehensive logging for consumer group debugging

## ✅ Testing
- Validated with kafka-python consumer groups
- Tested iterator mode without crashes
- Confirmed multiple polls past topic end work correctly
- Verified partition assignment distribution

## 🔄 Compatibility
- Full compatibility with kafka-python (tested with v2.0.2)
- Compatible with confluent-kafka
- Maintains backward compatibility with existing Chronik Stream clients

## 📦 What's Changed
- Enhanced consumer group state machine in `consumer_group.rs`
- Improved FetchHandler empty response handling in `fetch_handler.rs`
- Updated protocol handler for proper empty record encoding in `handler.rs`
- Enhanced OffsetFetch implementation in `kafka_handler.rs`

## 🚀 Impact
This release enables Chronik Stream to work seamlessly with standard Kafka consumer libraries, making it a drop-in replacement for Kafka in consumer group scenarios. Applications can now:
- Use standard Kafka clients without modifications
- Leverage consumer groups for scalable message consumption
- Poll past the end of topics without errors
- Implement robust message processing patterns

## Upgrade Notes
No breaking changes. This release is a drop-in replacement for v0.7.2.