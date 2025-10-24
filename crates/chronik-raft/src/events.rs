//! Raft events for notifying server layer of state changes.
//!
//! This module provides an event-driven mechanism for the Raft layer to communicate
//! state changes to the server layer without creating circular dependencies.
//! Events are sent via async channels and processed asynchronously by listeners.

use serde::{Deserialize, Serialize};

/// Events emitted by Raft replicas for state changes
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RaftEvent {
    /// A replica became the leader for its partition
    BecameLeader {
        /// Topic name
        topic: String,
        /// Partition index
        partition: u32,
        /// Node ID of the new leader
        node_id: u64,
        /// Raft term when leadership was acquired
        term: u64,
    },

    /// A replica stepped down from leadership (became follower)
    BecameFollower {
        /// Topic name
        topic: String,
        /// Partition index
        partition: u32,
        /// Node ID that stepped down
        node_id: u64,
        /// ID of the new leader (if known)
        new_leader: Option<u64>,
        /// Raft term
        term: u64,
    },

    /// A replica's leader changed (follower tracking new leader)
    LeaderChanged {
        /// Topic name
        topic: String,
        /// Partition index
        partition: u32,
        /// Node ID of this follower
        node_id: u64,
        /// ID of the new leader
        new_leader: u64,
        /// Raft term
        term: u64,
    },
}

impl RaftEvent {
    /// Get the topic for this event
    pub fn topic(&self) -> &str {
        match self {
            RaftEvent::BecameLeader { topic, .. } => topic,
            RaftEvent::BecameFollower { topic, .. } => topic,
            RaftEvent::LeaderChanged { topic, .. } => topic,
        }
    }

    /// Get the partition for this event
    pub fn partition(&self) -> u32 {
        match self {
            RaftEvent::BecameLeader { partition, .. } => *partition,
            RaftEvent::BecameFollower { partition, .. } => *partition,
            RaftEvent::LeaderChanged { partition, .. } => *partition,
        }
    }

    /// Get the node ID for this event
    pub fn node_id(&self) -> u64 {
        match self {
            RaftEvent::BecameLeader { node_id, .. } => *node_id,
            RaftEvent::BecameFollower { node_id, .. } => *node_id,
            RaftEvent::LeaderChanged { node_id, .. } => *node_id,
        }
    }

    /// Get the Raft term for this event
    pub fn term(&self) -> u64 {
        match self {
            RaftEvent::BecameLeader { term, .. } => *term,
            RaftEvent::BecameFollower { term, .. } => *term,
            RaftEvent::LeaderChanged { term, .. } => *term,
        }
    }

    /// Get a short description of the event for logging
    pub fn description(&self) -> String {
        match self {
            RaftEvent::BecameLeader { topic, partition, node_id, term } => {
                format!("Node {} became leader for {}/{} at term {}", node_id, topic, partition, term)
            }
            RaftEvent::BecameFollower { topic, partition, node_id, new_leader, term } => {
                if let Some(leader) = new_leader {
                    format!("Node {} became follower for {}/{} (new leader: {}) at term {}",
                            node_id, topic, partition, leader, term)
                } else {
                    format!("Node {} became follower for {}/{} at term {}",
                            node_id, topic, partition, term)
                }
            }
            RaftEvent::LeaderChanged { topic, partition, node_id, new_leader, term } => {
                format!("Node {} detected new leader {} for {}/{} at term {}",
                        node_id, new_leader, topic, partition, term)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_accessors() {
        let event = RaftEvent::BecameLeader {
            topic: "test-topic".to_string(),
            partition: 5,
            node_id: 42,
            term: 100,
        };

        assert_eq!(event.topic(), "test-topic");
        assert_eq!(event.partition(), 5);
        assert_eq!(event.node_id(), 42);
        assert_eq!(event.term(), 100);
    }

    #[test]
    fn test_event_description() {
        let event = RaftEvent::BecameLeader {
            topic: "test".to_string(),
            partition: 0,
            node_id: 1,
            term: 5,
        };

        let desc = event.description();
        assert!(desc.contains("Node 1"));
        assert!(desc.contains("became leader"));
        assert!(desc.contains("test/0"));
        assert!(desc.contains("term 5"));
    }
}
