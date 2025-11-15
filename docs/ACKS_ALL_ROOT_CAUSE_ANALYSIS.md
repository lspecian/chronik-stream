# acks=all Root Cause Analysis

## Summary
acks=all is failing because WAL frames are being sent only partially due to connection issues between leader and followers.

## Evidence from Logs

### Node 1 (Leader) - WAL Frame Sending
```
07:05:45.326368 No connection to follower localhost:9292 for test-acks-debug-2
07:05:45.326373 No connection to follower localhost:9293 for test-acks-debug-2
07:05:55.427320 Sent WAL record for test-acks-debug-2 to follower: localhost:9293
07:05:55.427325 No connection to follower localhost:9292 for test-acks-debug-2
07:06:05.537452 Sent WAL record for test-acks-debug-2 to follower: localhost:9293
07:06:05.537459 No connection to follower localhost:9292 for test-acks-debug-2
```

**Key Finding**: Leader is missing connections to followers. Only localhost:9293 (node 3) is connected, but localhost:9292 (node 2) is NOT connected.

### Node 2 (Follower) - WAL Receiver
```
07:06:20.248545 WAL receiver: Accepted connection from 127.0.0.1:55160
07:06:20.606877 WAL receiver: Accepted connection from 127.0.0.1:55164
07:06:24.215381 WAL receiver: Accepted connection from 127.0.0.1:32940
07:06:24.570376 WAL receiver: Accepted connection from 127.0.0.1:32944
```

**Key Finding**: Node 2's WAL receiver IS accepting connections, but NO "Read X bytes" messages appear, meaning NO WAL frames are being received.

### ACK Readers on Leader
```
Node 2: 07:05:52.563480 🔍 DEBUG: ACK reader started for follower: localhost:9291
Node 2: 07:05:52.563634 🔍 DEBUG: ACK reader started for follower: localhost:9293
Node 3: 07:05:54.568856 🔍 DEBUG: ACK reader started for follower: localhost:9291
Node 3: 07:06:24.570383 🔍 DEBUG: ACK reader started for follower: localhost:9292
```

**Key Finding**: ACK readers are started on each node when it becomes leader, waiting for ACKs from followers.

## Root Cause

The WAL replication architecture uses **bidirectional TCP connections**:

1. **Leader → Follower (Outbound)**: Leader's WalReplicationManager.connection_manager (line 484-535) connects TO followers' WAL receiver ports
2. **Leader ← Follower (Inbound)**: Same TCP stream is used by followers to send ACKs back

**The Problem**:
- `connection_manager` is successfully creating connections to localhost:9293 but FAILING to connect to localhost:9292
- This results in WAL frames being sent to only 1 out of 2 followers
- Without WAL frames reaching both followers, ACKs cannot be sent back
- IsrAckTracker never receives quorum (min_insync_replicas=2) of ACKs
- Producer times out waiting for acks=all

## Why Connections Are Failing

**Hypothesis 1: Port conflicts or binding issues**
- Node 2's WAL receiver is on port 9292
- Leader is trying to connect to `localhost:9292`
- Connections are being rejected or timing out

**Hypothesis 2: Connection manager timing/race condition**
- Connection manager runs every 30 seconds (RECONNECT_DELAY)
- May not be attempting connections frequently enough
- First connection attempt fails, waits 30s before retry

**Hypothesis 3: Network/firewall issues**
- Local firewall blocking specific ports
- SO_REUSEADDR not set correctly
- Port already in use by another process

## Next Steps

1. ✅ Add debug logging for connection attempts (success/failure)
2. ✅ Check if WAL receiver ports are actually listening
3. ✅ Verify no port conflicts
4. Check connection_manager logic for retry behavior
5. Test with netstat/lsof to see active connections
6. Consider reducing RECONNECT_DELAY for testing
