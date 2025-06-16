//! Network transport layer for Raft consensus.

use async_trait::async_trait;
use chronik_common::{Result, Error};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, error, info};

use crate::raft_node::{NodeId, RaftMessage};

/// Raft transport configuration
#[derive(Debug, Clone)]
pub struct TransportConfig {
    /// Node ID
    pub node_id: NodeId,
    /// Listen address
    pub listen_addr: SocketAddr,
    /// Peer addresses
    pub peers: HashMap<NodeId, SocketAddr>,
    /// Connection timeout
    pub connection_timeout: std::time::Duration,
}

/// Network transport for Raft messages
pub struct RaftTransport {
    config: TransportConfig,
    incoming_sender: mpsc::UnboundedSender<RaftMessage>,
    connections: Arc<RwLock<HashMap<NodeId, mpsc::UnboundedSender<RaftMessage>>>>,
}

impl RaftTransport {
    /// Create a new Raft transport
    pub fn new(
        config: TransportConfig,
        incoming_sender: mpsc::UnboundedSender<RaftMessage>,
    ) -> Self {
        Self {
            config,
            incoming_sender,
            connections: Arc::new(RwLock::new(HashMap::new())),
        }
    }
    
    /// Start the transport layer
    pub async fn start(self: Arc<Self>) -> Result<()> {
        // Start listener
        let listener = TcpListener::bind(&self.config.listen_addr).await
            .map_err(|e| Error::Network(format!("Failed to bind: {}", e)))?;
        
        info!("Raft transport listening on {}", self.config.listen_addr);
        
        // Accept incoming connections
        let accept_handle = {
            let transport = self.clone();
            tokio::spawn(async move {
                loop {
                    match listener.accept().await {
                        Ok((stream, addr)) => {
                            debug!("Accepted Raft connection from {}", addr);
                            let transport = transport.clone();
                            tokio::spawn(async move {
                                if let Err(e) = transport.handle_incoming(stream).await {
                                    error!("Error handling incoming connection: {}", e);
                                }
                            });
                        }
                        Err(e) => {
                            error!("Accept error: {}", e);
                        }
                    }
                }
            })
        };
        
        // Connect to peers
        for (peer_id, peer_addr) in &self.config.peers {
            let transport = self.clone();
            let peer_id = *peer_id;
            let peer_addr = *peer_addr;
            
            tokio::spawn(async move {
                loop {
                    if let Err(e) = transport.connect_to_peer(peer_id, peer_addr).await {
                        error!("Failed to connect to peer {}: {}", peer_id, e);
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    }
                }
            });
        }
        
        accept_handle.await
            .map_err(|e| Error::Internal(format!("Accept task failed: {}", e)))?;
        
        Ok(())
    }
    
    /// Send a message to a peer
    pub async fn send_message(&self, msg: RaftMessage) -> Result<()> {
        let connections = self.connections.read().await;
        
        if let Some(sender) = connections.get(&msg.to) {
            sender.send(msg)
                .map_err(|_| Error::Network("Failed to send message to peer".into()))?;
        } else {
            return Err(Error::Network(format!("No connection to peer {}", msg.to)));
        }
        
        Ok(())
    }
    
    /// Handle an incoming connection
    async fn handle_incoming(&self, mut stream: TcpStream) -> Result<()> {
        // Read peer ID
        let peer_id = stream.read_u64().await
            .map_err(|e| Error::Network(format!("Failed to read peer ID: {}", e)))?;
        
        debug!("Incoming connection from peer {}", peer_id);
        
        // Read messages
        loop {
            // Read message length
            let len = match stream.read_u32().await {
                Ok(len) => len as usize,
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(Error::Network(format!("Failed to read message length: {}", e))),
            };
            
            // Read message data
            let mut buf = vec![0u8; len];
            stream.read_exact(&mut buf).await
                .map_err(|e| Error::Network(format!("Failed to read message: {}", e)))?;
            
            // Deserialize and forward message
            match serde_json::from_slice::<RaftMessage>(&buf) {
                Ok(msg) => {
                    self.incoming_sender.send(msg)
                        .map_err(|_| Error::Internal("Failed to forward message".into()))?;
                }
                Err(e) => {
                    error!("Failed to deserialize message: {}", e);
                }
            }
        }
        
        Ok(())
    }
    
    /// Connect to a peer
    async fn connect_to_peer(&self, peer_id: NodeId, peer_addr: SocketAddr) -> Result<()> {
        // Connect with timeout
        let stream = tokio::time::timeout(
            self.config.connection_timeout,
            TcpStream::connect(&peer_addr)
        )
        .await
        .map_err(|_| Error::Network("Connection timeout".into()))?
        .map_err(|e| Error::Network(format!("Failed to connect: {}", e)))?;
        
        info!("Connected to peer {} at {}", peer_id, peer_addr);
        
        // Split stream
        let (_reader, writer) = stream.into_split();
        
        // Create message channel
        let (msg_sender, mut msg_receiver) = mpsc::unbounded_channel();
        
        // Store connection
        {
            let mut connections = self.connections.write().await;
            connections.insert(peer_id, msg_sender);
        }
        
        // Spawn writer task
        let node_id = self.config.node_id;
        let writer_handle = tokio::spawn(async move {
            let mut writer = writer;
            
            // Send our node ID
            writer.write_u64(node_id).await?;
            
            // Send messages
            while let Some(msg) = msg_receiver.recv().await {
                let data = serde_json::to_vec(&msg)?;
                writer.write_u32(data.len() as u32).await?;
                writer.write_all(&data).await?;
                writer.flush().await?;
            }
            
            Ok::<(), Error>(())
        });
        
        // Handle writer task completion
        match writer_handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                error!("Writer task error: {}", e);
            }
            Err(e) => {
                error!("Writer task panicked: {}", e);
            }
        }
        
        // Remove connection
        {
            let mut connections = self.connections.write().await;
            connections.remove(&peer_id);
        }
        
        Ok(())
    }
}

/// Message transport trait for Raft
#[async_trait]
pub trait Transport: Send + Sync {
    /// Send a message to a peer
    async fn send(&self, msg: RaftMessage) -> Result<()>;
    
    /// Receive messages
    async fn recv(&mut self) -> Result<RaftMessage>;
}

/// Transport implementation using RaftTransport
pub struct TransportImpl {
    transport: Arc<RaftTransport>,
    receiver: mpsc::UnboundedReceiver<RaftMessage>,
}

impl TransportImpl {
    pub fn new(
        transport: Arc<RaftTransport>,
        receiver: mpsc::UnboundedReceiver<RaftMessage>,
    ) -> Self {
        Self { transport, receiver }
    }
}

#[async_trait]
impl Transport for TransportImpl {
    async fn send(&self, msg: RaftMessage) -> Result<()> {
        self.transport.send_message(msg).await
    }
    
    async fn recv(&mut self) -> Result<RaftMessage> {
        self.receiver.recv().await
            .ok_or_else(|| Error::Internal("Transport channel closed".into()))
    }
}