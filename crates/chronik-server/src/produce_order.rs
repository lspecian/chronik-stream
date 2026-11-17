//! Per-connection write order for pipelined Produce requests.
//!
//! The connection handler runs every request in its own task so that a
//! producer with several requests in flight overlaps their durability and
//! replication waits. Before this module, nothing kept those tasks in the
//! order they were read: request 2 could reach a partition's append lock
//! before request 1 and take the lower offsets. A non-idempotent producer —
//! which has no sequence numbers to repair the order — then had its records
//! stored out of the order it sent them, with no retry involved. Kafka gives
//! this order for free by handling a connection's requests one at a time.
//!
//! [`ProduceOrder`] keeps it without giving up the overlap:
//!
//! - As each Produce request is read, [`ProduceOrder::issue`] queues a ticket
//!   on every partition the request writes to. This runs on the connection's
//!   read loop, so tickets are queued in read order.
//! - A request may enter a partition's append lock only when its ticket is at
//!   the front of that partition's queue ([`PartitionTurn::wait`]). The turn
//!   passes as soon as the append lock is held — the lock is FIFO, so the
//!   offsets are already decided — or when the partition is done for any
//!   other reason (refused, forwarded, failed): [`PartitionTurn`] and
//!   [`ProduceTicket`] pass on drop, so no exit path can strand a queue.
//! - Different partitions do not wait for each other, and nothing waits for
//!   an earlier request's fsync or acknowledgements.
//!
//! Waiting holds a request-concurrency permit, so permits must also be taken
//! in read order (see [`ProduceTicket::permit_order`]): otherwise later
//! requests could hold every permit while waiting for an earlier request that
//! cannot get one.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use tokio::sync::{oneshot, Notify};

type PartitionKey = (String, i32);

/// The ordering state of one connection's Produce requests.
#[derive(Default)]
pub struct ProduceOrder {
    state: Mutex<OrderState>,
    turn_passed: Notify,
}

#[derive(Default)]
struct OrderState {
    next_ticket: u64,
    /// Tickets waiting to enter each partition's append lock, oldest first.
    queues: HashMap<PartitionKey, VecDeque<u64>>,
    /// Completes when the previous ticket holds its concurrency permit.
    previous_permit: Option<oneshot::Receiver<()>>,
}

impl ProduceOrder {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Queue a ticket for a request that writes to `partitions`. Must be
    /// called in the order the connection's requests are read.
    pub fn issue(self: &Arc<Self>, partitions: Vec<PartitionKey>) -> ProduceTicket {
        let mut state = self.state.lock().unwrap();
        let id = state.next_ticket;
        state.next_ticket += 1;
        let pending: HashSet<PartitionKey> = partitions.into_iter().collect();
        for key in &pending {
            state.queues.entry(key.clone()).or_default().push_back(id);
        }
        let (permit_taken, next) = oneshot::channel();
        let previous_permit = state.previous_permit.replace(next);
        ProduceTicket {
            order: Arc::clone(self),
            id,
            pending: Mutex::new(pending),
            previous_permit: Mutex::new(previous_permit),
            permit_taken: Mutex::new(Some(permit_taken)),
        }
    }

    fn is_front(&self, key: &PartitionKey, id: u64) -> bool {
        let state = self.state.lock().unwrap();
        state.queues.get(key).and_then(|q| q.front()) == Some(&id)
    }

    fn remove(&self, key: &PartitionKey, id: u64) {
        let mut state = self.state.lock().unwrap();
        if let Some(queue) = state.queues.get_mut(key) {
            if let Some(i) = queue.iter().position(|t| *t == id) {
                queue.remove(i);
            }
            if queue.is_empty() {
                state.queues.remove(key);
            }
        }
        drop(state);
        self.turn_passed.notify_waiters();
    }

    #[cfg(test)]
    fn queued(&self) -> usize {
        self.state
            .lock()
            .unwrap()
            .queues
            .values()
            .map(VecDeque::len)
            .sum()
    }
}

/// One Produce request's place in its connection's order.
pub struct ProduceTicket {
    order: Arc<ProduceOrder>,
    id: u64,
    /// Partitions whose turn this request has not passed yet.
    pending: Mutex<HashSet<PartitionKey>>,
    previous_permit: Mutex<Option<oneshot::Receiver<()>>>,
    permit_taken: Mutex<Option<oneshot::Sender<()>>>,
}

impl ProduceTicket {
    /// Wait until the previous request of this connection holds its
    /// concurrency permit; call [`Self::permit_held`] once this one does.
    pub async fn permit_order(&self) {
        let previous = self.previous_permit.lock().unwrap().take();
        if let Some(previous) = previous {
            // An error means the previous request is gone: its turn is over.
            let _ = previous.await;
        }
    }

    /// This request holds its permit; the next one may take one.
    pub fn permit_held(&self) {
        if let Some(tx) = self.permit_taken.lock().unwrap().take() {
            let _ = tx.send(());
        }
    }

    /// The turn of this request on one partition. `None` if the request was
    /// not queued on it (it is not in the request), which never waits.
    pub fn partition(self: &Arc<Self>, topic: &str, partition: i32) -> Option<PartitionTurn> {
        let key = (topic.to_string(), partition);
        self.pending
            .lock()
            .unwrap()
            .contains(&key)
            .then(|| PartitionTurn {
                ticket: Arc::clone(self),
                key,
            })
    }

    /// Pass the turn on every partition of `topic` (a topic the request will
    /// not write, e.g. it does not exist).
    pub fn pass_topic(&self, topic: &str) {
        let keys: Vec<PartitionKey> = self
            .pending
            .lock()
            .unwrap()
            .iter()
            .filter(|(t, _)| t == topic)
            .cloned()
            .collect();
        for key in keys {
            self.pass(&key);
        }
    }

    fn pass(&self, key: &PartitionKey) {
        if self.pending.lock().unwrap().remove(key) {
            self.order.remove(key, self.id);
        }
    }
}

impl Drop for ProduceTicket {
    fn drop(&mut self) {
        self.permit_held();
        let keys: Vec<PartitionKey> = self.pending.lock().unwrap().drain().collect();
        for key in keys {
            self.order.remove(&key, self.id);
        }
    }
}

/// A request's turn on one partition. Passes on drop.
pub struct PartitionTurn {
    ticket: Arc<ProduceTicket>,
    key: PartitionKey,
}

impl PartitionTurn {
    /// Wait until every earlier request of this connection has passed its
    /// turn on this partition.
    pub async fn wait(&self) {
        let order = &self.ticket.order;
        loop {
            let passed = order.turn_passed.notified();
            tokio::pin!(passed);
            // Register before checking, so a pass between the check and the
            // await is not missed.
            passed.as_mut().enable();
            if order.is_front(&self.key, self.ticket.id) {
                return;
            }
            passed.await;
        }
    }
}

impl Drop for PartitionTurn {
    fn drop(&mut self) {
        self.ticket.pass(&self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    fn parts(list: &[(&str, i32)]) -> Vec<PartitionKey> {
        list.iter().map(|(t, p)| (t.to_string(), *p)).collect()
    }

    const SHORT: Duration = Duration::from_millis(50);

    #[tokio::test]
    async fn a_later_request_waits_for_the_earlier_one_on_the_same_partition() {
        let order = ProduceOrder::new();
        let first = Arc::new(order.issue(parts(&[("t", 0)])));
        let second = Arc::new(order.issue(parts(&[("t", 0)])));

        let second_turn = second.partition("t", 0).unwrap();
        assert!(
            timeout(SHORT, second_turn.wait()).await.is_err(),
            "overtook"
        );

        let first_turn = first.partition("t", 0).unwrap();
        first_turn.wait().await;
        drop(first_turn);
        timeout(SHORT, second_turn.wait())
            .await
            .expect("still blocked after the pass");
    }

    #[tokio::test]
    async fn other_partitions_do_not_wait() {
        let order = ProduceOrder::new();
        let _first = Arc::new(order.issue(parts(&[("t", 0)])));
        let second = Arc::new(order.issue(parts(&[("t", 1), ("u", 0)])));
        timeout(SHORT, second.partition("t", 1).unwrap().wait())
            .await
            .unwrap();
        timeout(SHORT, second.partition("u", 0).unwrap().wait())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn dropping_a_request_passes_every_partition_it_never_reached() {
        let order = ProduceOrder::new();
        let first = Arc::new(order.issue(parts(&[("t", 0), ("t", 1)])));
        let second = Arc::new(order.issue(parts(&[("t", 0), ("t", 1)])));
        drop(first); // e.g. the handler failed before touching any partition
        timeout(SHORT, second.partition("t", 0).unwrap().wait())
            .await
            .unwrap();
        timeout(SHORT, second.partition("t", 1).unwrap().wait())
            .await
            .unwrap();
        drop(second);
        assert_eq!(order.queued(), 0, "queues must not leak");
    }

    #[tokio::test]
    async fn pass_topic_releases_only_that_topic() {
        let order = ProduceOrder::new();
        let first = Arc::new(order.issue(parts(&[("gone", 0), ("t", 0)])));
        let second = Arc::new(order.issue(parts(&[("gone", 0), ("t", 0)])));
        first.pass_topic("gone");
        timeout(SHORT, second.partition("gone", 0).unwrap().wait())
            .await
            .unwrap();
        assert!(timeout(SHORT, second.partition("t", 0).unwrap().wait())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn a_partition_outside_the_request_has_no_turn() {
        let order = ProduceOrder::new();
        let ticket = Arc::new(order.issue(parts(&[("t", 0)])));
        assert!(ticket.partition("t", 9).is_none());
    }

    #[tokio::test]
    async fn permits_are_taken_in_read_order() {
        let order = ProduceOrder::new();
        let first = order.issue(vec![]);
        let second = order.issue(vec![]);
        first.permit_order().await; // the first never waits
        assert!(timeout(SHORT, second.permit_order()).await.is_err());
        first.permit_held();
        timeout(SHORT, second.permit_order()).await.unwrap();
    }

    #[tokio::test]
    async fn a_dropped_request_does_not_block_the_next_permit() {
        let order = ProduceOrder::new();
        let first = order.issue(vec![]);
        let second = order.issue(vec![]);
        drop(first);
        timeout(SHORT, second.permit_order()).await.unwrap();
    }

    /// Many requests released in reverse order still enter in read order.
    #[tokio::test]
    async fn entries_follow_read_order_whatever_order_tasks_run_in() {
        let order = ProduceOrder::new();
        let tickets: Vec<_> = (0..50)
            .map(|_| Arc::new(order.issue(parts(&[("t", 0)]))))
            .collect();
        let entered = Arc::new(Mutex::new(Vec::new()));
        let mut tasks = Vec::new();
        for (i, ticket) in tickets.into_iter().enumerate().rev() {
            let entered = Arc::clone(&entered);
            tasks.push(tokio::spawn(async move {
                let turn = ticket.partition("t", 0).unwrap();
                turn.wait().await;
                entered.lock().unwrap().push(i);
            }));
        }
        for t in tasks {
            timeout(Duration::from_secs(5), t).await.unwrap().unwrap();
        }
        assert_eq!(*entered.lock().unwrap(), (0..50).collect::<Vec<_>>());
    }
}
