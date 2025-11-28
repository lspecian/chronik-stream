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
        // Default buffer: 1000 events
        // At ~100 bytes per event = ~100KB memory
        Self::new(1000)
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
