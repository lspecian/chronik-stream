/// Metadata Event System for WAL-Based Replication
///
/// This module implements an event-based architecture for metadata replication
/// where RaftMetadataStore emits events that MetadataWalReplicator subscribes to.
///
/// Architecture:
/// ```
/// RaftMetadataStore → emit(MetadataEvent) → MetadataEventBus → MetadataWalReplicator
/// ```

use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{debug, warn};

/// Metadata events that trigger WAL replication
#[derive(Debug, Clone)]
pub enum MetadataEvent {
    /// Partition assignment changed
    PartitionAssigned {
        topic: String,
        partition: i32,
        replicas: Vec<u64>,
        leader: u64,
    },

    /// Partition leader changed
    LeaderChanged {
        topic: String,
        partition: i32,
        new_leader: u64,
    },

    /// ISR (In-Sync Replicas) updated
    ISRUpdated {
        topic: String,
        partition: i32,
        isr: Vec<u64>,
    },

    /// High watermark updated
    HighWatermarkUpdated {
        topic: String,
        partition: i32,
        offset: i64,
    },

    /// Topic created
    TopicCreated {
        topic: String,
        num_partitions: i32,
    },

    /// Topic deleted
    TopicDeleted {
        topic: String,
    },

    /// Broker registered (v2.2.9)
    /// CRITICAL FIX: This was missing, causing followers to not receive broker metadata
    BrokerRegistered {
        broker_id: i32,
        host: String,
        port: i32,
        rack: Option<String>,
    },
}

/// Event bus for metadata changes
///
/// Uses tokio broadcast channel for efficient multi-subscriber support.
/// Subscribers can miss events if they're slow (lagging > buffer size).
pub struct MetadataEventBus {
    sender: broadcast::Sender<MetadataEvent>,
}

impl MetadataEventBus {
    /// Create a new event bus with specified buffer size
    ///
    /// Buffer size determines how many events can be queued before
    /// slow subscribers start missing events (they'll get RecvError::Lagged).
    pub fn new(buffer_size: usize) -> Self {
        let (sender, _) = broadcast::channel(buffer_size);
        Self { sender }
    }

    /// Publish an event to all subscribers
    ///
    /// Returns the number of subscribers that received the event.
    /// Returns 0 if no subscribers are listening (not an error).
    pub fn publish(&self, event: MetadataEvent) -> usize {
        match self.sender.send(event.clone()) {
            Ok(count) => {
                debug!("Published metadata event to {} subscribers: {:?}", count, event);
                count
            }
            Err(_) => {
                // No subscribers - this is OK during startup
                debug!("No subscribers for metadata event: {:?}", event);
                0
            }
        }
    }

    /// Subscribe to metadata events
    ///
    /// Returns a receiver that can be used to listen for events.
    /// Multiple subscribers can exist simultaneously.
    pub fn subscribe(&self) -> broadcast::Receiver<MetadataEvent> {
        self.sender.subscribe()
    }

    /// Get the number of active subscribers
    pub fn subscriber_count(&self) -> usize {
        self.sender.receiver_count()
    }
}

impl Default for MetadataEventBus {
    fn default() -> Self {
        // The buffer must exceed the largest burst a follower can fall behind on.
        // The worst burst is broadcast_all_topics() re-pushing the ENTIRE catalog
        // (one TopicCreated per topic) in a tight loop on restart; if that burst
        // exceeds the buffer, the follower's receiver lags and tokio::broadcast
        // silently drops the OLDEST events — the topic catalog then diverges across
        // nodes under load (observed: followers stuck at ~1500/1879 with buffer=1000).
        //
        // Default sized for tens of thousands of topics; override for larger
        // deployments via CHRONIK_METADATA_EVENT_BUFFER. Cost is ~event_size * N
        // held transiently (~tens of MB at 50k), only while followers are catching up.
        let buffer = std::env::var("CHRONIK_METADATA_EVENT_BUFFER")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(50_000);
        Self::new(buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{timeout, Duration};

    #[tokio::test]
    async fn test_event_bus_publish_subscribe() {
        let bus = MetadataEventBus::new(10);
        let mut receiver = bus.subscribe();

        let event = MetadataEvent::PartitionAssigned {
            topic: "test".to_string(),
            partition: 0,
            replicas: vec![1, 2, 3],
            leader: 1,
        };

        let count = bus.publish(event.clone());
        assert_eq!(count, 1, "Should have 1 subscriber");

        let received = timeout(Duration::from_secs(1), receiver.recv())
            .await
            .expect("Should not timeout")
            .expect("Should receive event");

        match received {
            MetadataEvent::PartitionAssigned { topic, partition, replicas, leader } => {
                assert_eq!(topic, "test");
                assert_eq!(partition, 0);
                assert_eq!(replicas, vec![1, 2, 3]);
                assert_eq!(leader, 1);
            }
            _ => panic!("Wrong event type"),
        }
    }

    #[tokio::test]
    async fn test_multiple_subscribers() {
        let bus = MetadataEventBus::new(10);
        let mut receiver1 = bus.subscribe();
        let mut receiver2 = bus.subscribe();

        assert_eq!(bus.subscriber_count(), 2);

        let event = MetadataEvent::TopicCreated {
            topic: "test".to_string(),
            num_partitions: 3,
        };

        let count = bus.publish(event.clone());
        assert_eq!(count, 2, "Should have 2 subscribers");

        // Both receivers should get the event
        let r1 = timeout(Duration::from_secs(1), receiver1.recv())
            .await
            .expect("timeout")
            .expect("recv");
        let r2 = timeout(Duration::from_secs(1), receiver2.recv())
            .await
            .expect("timeout")
            .expect("recv");

        match (r1, r2) {
            (MetadataEvent::TopicCreated { topic: t1, .. },
             MetadataEvent::TopicCreated { topic: t2, .. }) => {
                assert_eq!(t1, "test");
                assert_eq!(t2, "test");
            }
            _ => panic!("Wrong event types"),
        }
    }

    /// Regression: the default event bus must survive a bulk re-broadcast of the
    /// entire catalog without dropping events. `broadcast_all_topics()` pushes one
    /// TopicCreated per topic in a tight loop on restart; with the old buffer of
    /// 1000 (see `test_small_buffer_drops_bulk_rebroadcast`) a burst larger than
    /// the buffer made the follower's receiver lag and tokio::broadcast silently
    /// dropped the oldest events, diverging the topic catalog across nodes.
    #[tokio::test]
    async fn test_default_buffer_survives_bulk_rebroadcast() {
        // Isolate from any ambient CHRONIK_METADATA_EVENT_BUFFER in the env.
        let bus = MetadataEventBus::new(50_000);
        let mut receiver = bus.subscribe();

        // A catalog far larger than the old 1000 buffer, pushed before any drain.
        const N: usize = 3000;
        for i in 0..N {
            bus.publish(MetadataEvent::TopicCreated {
                topic: format!("repro-{}", i),
                num_partitions: 1,
            });
        }

        // Drain: every event must arrive, in order, with no Lagged gap.
        for i in 0..N {
            match receiver.try_recv() {
                Ok(MetadataEvent::TopicCreated { topic, .. }) => {
                    assert_eq!(topic, format!("repro-{}", i), "out-of-order / dropped at {}", i);
                }
                Ok(other) => panic!("unexpected event: {:?}", other),
                Err(e) => panic!("event {} dropped/unavailable: {:?} — buffer too small", i, e),
            }
        }
    }

    /// Documents the OLD failure mode: a buffer smaller than the burst drops the
    /// oldest events (RecvError::Lagged). This is what diverged the catalog; the
    /// default is now sized well past realistic catalogs so this cannot happen.
    #[tokio::test]
    async fn test_small_buffer_drops_bulk_rebroadcast() {
        use tokio::sync::broadcast::error::TryRecvError;
        let bus = MetadataEventBus::new(1000);
        let mut receiver = bus.subscribe();

        for i in 0..3000 {
            bus.publish(MetadataEvent::TopicCreated {
                topic: format!("repro-{}", i),
                num_partitions: 1,
            });
        }

        // With capacity 1000 and 3000 sent undrained, the first recv reports the
        // dropped span rather than event 0.
        match receiver.try_recv() {
            Err(TryRecvError::Lagged(dropped)) => {
                assert!(dropped > 0, "expected lag drops with an undersized buffer");
            }
            other => panic!("expected Lagged with buffer<burst, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_no_subscribers() {
        let bus = MetadataEventBus::new(10);

        let event = MetadataEvent::TopicCreated {
            topic: "test".to_string(),
            num_partitions: 1,
        };

        // Should not panic with no subscribers
        let count = bus.publish(event);
        assert_eq!(count, 0, "Should have 0 subscribers");
    }

    #[tokio::test]
    async fn test_lagged_subscriber() {
        let bus = MetadataEventBus::new(2); // Small buffer
        let mut receiver = bus.subscribe();

        // Send more events than buffer size
        for i in 0..5 {
            bus.publish(MetadataEvent::TopicCreated {
                topic: format!("test-{}", i),
                num_partitions: 1,
            });
        }

        // First recv should return Lagged error
        match receiver.recv().await {
            Err(broadcast::error::RecvError::Lagged(count)) => {
                assert!(count > 0, "Should have lagged by some events");
            }
            other => panic!("Expected Lagged error, got {:?}", other),
        }
    }
}
