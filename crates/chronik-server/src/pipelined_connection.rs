/// Async pipelined connection for leader forwarding
///
/// This module implements Kafka-style async request pipelining, allowing
/// multiple requests to be in-flight simultaneously on a single TCP connection.
/// Responses are matched back to requests via correlation IDs.
///
/// Key differences from synchronous forwarding:
/// - Synchronous: Send → WAIT → Receive (1 request at a time, 50ms each = 20 req/s)
/// - Pipelined: Send, Send, Send... | Receive, Receive, Receive... (100+ req/s)
///
/// Architecture:
/// ```
/// Client Call                Send Task              Receive Task
///     │                          │                       │
///     ├─ send_request() ────────→│                       │
///     │                          ├─ write_all() ────────→│
///     │                          │                       │
///     ├─ send_request() ────────→│                       │
///     │                          ├─ write_all() ────────→│
///     │                          │                       │
///     │                          │                   ┌───┴─ read_response()
///     │                          │                   │
///     │← response ───────────────┴───────────────────┘
/// ```

use tokio::net::TcpStream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::{timeout, Duration};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use bytes::{Bytes, BytesMut, BufMut};
use tracing::{debug, error, warn, instrument};

use chronik_common::{Error, Result};

/// A request waiting to be sent
struct PendingRequest {
    correlation_id: i32,
    request_bytes: Bytes,
    response_tx: oneshot::Sender<Result<Bytes>>,
}

/// A pipelined connection to a leader broker
///
/// Supports multiple in-flight requests matched by correlation ID.
/// Automatically handles:
/// - Request queueing
/// - Response matching
/// - Timeouts
/// - Connection errors
pub struct PipelinedConnection {
    /// Channel to send requests to the send task
    request_tx: mpsc::Sender<PendingRequest>,

    /// Next correlation ID (atomically incremented)
    next_correlation_id: Arc<AtomicI32>,

    /// Address of the remote leader
    remote_addr: String,
}

impl PipelinedConnection {
    /// Create a new pipelined connection to a leader
    ///
    /// # Arguments
    /// * `remote_addr` - Address of the leader broker (e.g., "localhost:9092")
    /// * `request_queue_size` - Max number of queued requests (default: 1000)
    pub async fn connect(remote_addr: String, request_queue_size: usize) -> Result<Self> {
        debug!("Establishing pipelined connection to {}", remote_addr);

        // Connect to remote leader
        let stream = TcpStream::connect(&remote_addr).await.map_err(|e| {
            Error::Internal(format!("Failed to connect to {}: {}", remote_addr, e))
        })?;

        // Set TCP_NODELAY for low latency
        stream.set_nodelay(true).map_err(|e| {
            Error::Internal(format!("Failed to set TCP_NODELAY: {}", e))
        })?;

        let (read_half, write_half) = stream.into_split();

        // Create request queue channel
        let (request_tx, request_rx) = mpsc::channel(request_queue_size);

        // Shared state for tracking pending responses
        let pending_responses: Arc<Mutex<HashMap<i32, oneshot::Sender<Result<Bytes>>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let next_correlation_id = Arc::new(AtomicI32::new(1));

        // Spawn send task
        let pending_for_send = pending_responses.clone();
        let remote_addr_for_send = remote_addr.clone();
        tokio::spawn(async move {
            if let Err(e) = send_task(write_half, request_rx, pending_for_send).await {
                error!("Send task failed for {}: {}", remote_addr_for_send, e);
            }
        });

        // Spawn receive task
        let pending_for_receive = pending_responses.clone();
        let remote_addr_for_receive = remote_addr.clone();
        tokio::spawn(async move {
            if let Err(e) = receive_task(read_half, pending_for_receive).await {
                error!("Receive task failed for {}: {}", remote_addr_for_receive, e);
            }
        });

        debug!("Pipelined connection established to {}", remote_addr);

        Ok(Self {
            request_tx,
            next_correlation_id,
            remote_addr,
        })
    }

    /// Send a request and wait for response (with timeout)
    ///
    /// # Arguments
    /// * `request_bytes` - Encoded Kafka request (length-prefixed frame with correlation_id=0)
    /// * `timeout_ms` - Timeout in milliseconds
    ///
    /// # Returns
    /// Response bytes (length-prefixed frame) or error
    ///
    /// # Note
    /// This method PATCHES the correlation ID into the request before sending.
    /// The caller should set correlation_id=0 in the request header.
    #[instrument(skip(self, request_bytes), fields(addr = %self.remote_addr, bytes = request_bytes.len()))]
    pub async fn send_request(
        &self,
        request_bytes: Bytes,
        timeout_ms: u64,
    ) -> Result<Bytes> {
        // Allocate correlation ID
        let correlation_id = self.next_correlation_id.fetch_add(1, Ordering::SeqCst);

        debug!(
            correlation_id,
            "Sending pipelined request ({} bytes) to {}",
            request_bytes.len(),
            self.remote_addr
        );

        // Patch correlation ID into request bytes
        // Kafka request format: [length:4][api_key:2][api_version:2][correlation_id:4][...]
        // Correlation ID is at offset 8-11 (after length prefix)
        let mut patched_bytes = BytesMut::with_capacity(request_bytes.len());
        patched_bytes.extend_from_slice(&request_bytes[..8]); // length + api_key + api_version
        patched_bytes.put_i32(correlation_id); // Patch correlation ID
        patched_bytes.extend_from_slice(&request_bytes[12..]); // Rest of request
        let patched_bytes = patched_bytes.freeze();

        // Create oneshot channel for response
        let (response_tx, response_rx) = oneshot::channel();

        // Queue request for sending
        let pending_request = PendingRequest {
            correlation_id,
            request_bytes: patched_bytes,
            response_tx,
        };

        self.request_tx.send(pending_request).await.map_err(|_| {
            Error::Internal("Send task has shut down".into())
        })?;

        // Wait for response with timeout
        let result = timeout(Duration::from_millis(timeout_ms), response_rx).await;

        match result {
            Ok(Ok(response)) => {
                debug!(correlation_id, "Received pipelined response");
                response
            }
            Ok(Err(_)) => {
                // Oneshot was dropped (send/receive task died)
                Err(Error::Internal("Connection closed".into()))
            }
            Err(_) => {
                warn!(correlation_id, "Request timed out after {}ms", timeout_ms);
                Err(Error::Internal(format!("Request timeout after {}ms", timeout_ms)))
            }
        }
    }

    /// Get the remote address of this connection
    pub fn remote_addr(&self) -> &str {
        &self.remote_addr
    }
}

/// Send task: reads requests from queue and writes to TCP stream
async fn send_task(
    mut write_half: tokio::net::tcp::OwnedWriteHalf,
    mut request_rx: mpsc::Receiver<PendingRequest>,
    pending_responses: Arc<Mutex<HashMap<i32, oneshot::Sender<Result<Bytes>>>>>,
) -> Result<()> {
    while let Some(pending_request) = request_rx.recv().await {
        let correlation_id = pending_request.correlation_id;

        // Register pending response BEFORE sending
        {
            let mut pending = pending_responses.lock().await;
            pending.insert(correlation_id, pending_request.response_tx);
        }

        // Send request bytes
        if let Err(e) = write_half.write_all(&pending_request.request_bytes).await {
            error!("Failed to write request {}: {}", correlation_id, e);

            // Remove from pending and notify caller
            let mut pending = pending_responses.lock().await;
            if let Some(tx) = pending.remove(&correlation_id) {
                let _ = tx.send(Err(Error::Internal(format!("Write failed: {}", e))));
            }

            return Err(Error::Internal(format!("Write failed: {}", e)));
        }

        debug!("Sent request {}", correlation_id);
    }

    Ok(())
}

/// Receive task: reads responses from TCP stream and matches to pending requests
async fn receive_task(
    mut read_half: tokio::net::tcp::OwnedReadHalf,
    pending_responses: Arc<Mutex<HashMap<i32, oneshot::Sender<Result<Bytes>>>>>,
) -> Result<()> {
    loop {
        // Read response length (4 bytes)
        let mut length_buf = [0u8; 4];
        if let Err(e) = read_half.read_exact(&mut length_buf).await {
            error!("Failed to read response length: {}", e);

            // Notify all pending requests of failure
            let mut pending = pending_responses.lock().await;
            for (correlation_id, tx) in pending.drain() {
                let _ = tx.send(Err(Error::Internal("Connection closed".into())));
                warn!("Notified correlation_id {} of connection closure", correlation_id);
            }

            return Err(Error::Internal(format!("Read failed: {}", e)));
        }

        let response_length = i32::from_be_bytes(length_buf) as usize;

        if response_length > 100_000_000 {
            error!("Response length too large: {} bytes", response_length);
            return Err(Error::Internal("Response too large".into()));
        }

        // Read response body
        let mut response_buf = vec![0u8; response_length];
        if let Err(e) = read_half.read_exact(&mut response_buf).await {
            error!("Failed to read response body: {}", e);

            // Notify all pending requests of failure
            let mut pending = pending_responses.lock().await;
            for (correlation_id, tx) in pending.drain() {
                let _ = tx.send(Err(Error::Internal("Connection closed".into())));
            }

            return Err(Error::Internal(format!("Read failed: {}", e)));
        }

        // Parse correlation ID from response (first 4 bytes of response header)
        if response_buf.len() < 4 {
            error!("Response too short to contain correlation ID");
            continue;
        }

        let correlation_id = i32::from_be_bytes([
            response_buf[0],
            response_buf[1],
            response_buf[2],
            response_buf[3],
        ]);

        debug!("Received response for correlation_id {}", correlation_id);

        // Find matching pending request
        let mut pending = pending_responses.lock().await;
        if let Some(tx) = pending.remove(&correlation_id) {
            // Build complete response frame (length + body)
            let mut response_frame = BytesMut::with_capacity(4 + response_length);
            response_frame.put_i32(response_length as i32);
            response_frame.extend_from_slice(&response_buf);

            // Send to waiting caller
            if tx.send(Ok(response_frame.freeze())).is_err() {
                warn!("Caller dropped for correlation_id {}", correlation_id);
            }
        } else {
            warn!("No pending request for correlation_id {}", correlation_id);
        }
    }
}

/// Pool of pipelined connections to leaders
///
/// Maintains one connection per leader address, shared across all requests.
pub struct PipelinedConnectionPool {
    connections: Arc<Mutex<HashMap<String, Arc<PipelinedConnection>>>>,
    request_queue_size: usize,
}

impl PipelinedConnectionPool {
    /// Create a new connection pool
    pub fn new(request_queue_size: usize) -> Self {
        Self {
            connections: Arc::new(Mutex::new(HashMap::new())),
            request_queue_size,
        }
    }

    /// Get or create a connection to a leader
    ///
    /// Connections are shared across all callers to the same address.
    pub async fn get_connection(&self, remote_addr: &str) -> Result<Arc<PipelinedConnection>> {
        let mut connections = self.connections.lock().await;

        if let Some(conn) = connections.get(remote_addr) {
            debug!("Reusing existing pipelined connection to {}", remote_addr);
            return Ok(conn.clone());
        }

        debug!("Creating new pipelined connection to {}", remote_addr);
        let conn: Arc<PipelinedConnection> = Arc::new(
            PipelinedConnection::connect(remote_addr.to_string(), self.request_queue_size).await?
        );

        connections.insert(remote_addr.to_string(), conn.clone());

        Ok(conn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_pipelined_connection_pool() {
        let pool = PipelinedConnectionPool::new(1000);

        // Getting a connection to non-existent server should fail
        let result = pool.get_connection("localhost:99999").await;
        assert!(result.is_err());
    }
}
