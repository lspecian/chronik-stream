//! Network transport layer for Raft consensus.

use chronik_common::{Result, Error};
use raft::prelude::Message as RaftMessage;
use protobuf::Message as ProtoMessage;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, error, info, warn};

use super::NodeId;

/// Transport configuration
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

/// Message wrapper for network transport
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransportMessage {
    pub from: NodeId,
    pub to: NodeId,
    pub message: Vec<u8>,
}

/// Connection state for a peer
struct PeerConnection {
    sender: mpsc::UnboundedSender<TransportMessage>,
    #[allow(dead_code)]
    handle: tokio::task::JoinHandle<()>,
}

/// Network transport for Raft messages
pub struct RaftTransport {
    config: TransportConfig,
    connections: Arc<RwLock<HashMap<NodeId, PeerConnection>>>,
    incoming_tx: mpsc::UnboundedSender<TransportMessage>,
    incoming_rx: Arc<RwLock<mpsc::UnboundedReceiver<TransportMessage>>>,
}

impl RaftTransport {
    /// Create a new Raft transport
    pub fn new(config: TransportConfig) -> Self {
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
        
        Self {
            config,
            connections: Arc::new(RwLock::new(HashMap::new())),
            incoming_tx,
            incoming_rx: Arc::new(RwLock::new(incoming_rx)),
        }
    }
    
    /// Start the transport layer
    pub async fn start(self: Arc<Self>) -> Result<()> {
        // Start listener
        let listener = TcpListener::bind(&self.config.listen_addr).await
            .map_err(|e| Error::Network(format!("Failed to bind: {}", e)))?;
        
        info!("Raft transport listening on {}", self.config.listen_addr);
        
        // Accept incoming connections
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
        });
        
        // Connect to peers
        for (&peer_id, &peer_addr) in &self.config.peers {
            let transport_clone = self.clone();
            tokio::spawn(async move {
                loop {
                    if let Err(e) = transport_clone.clone().maintain_peer_connection(peer_id, peer_addr).await {
                        warn!("Failed to maintain connection to peer {}: {}", peer_id, e);
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            });
        }
        
        Ok(())
    }
    
    /// Send a Raft message to a peer
    pub async fn send(&self, to: NodeId, msg: RaftMessage) -> Result<()> {
        let data = <RaftMessage as ProtoMessage>::write_to_bytes(&msg)
            .map_err(|e| Error::Network(format!("Failed to serialize message: {}", e)))?;
        
        let transport_msg = TransportMessage {
            from: self.config.node_id,
            to,
            message: data,
        };
        
        let connections = self.connections.read().await;
        if let Some(conn) = connections.get(&to) {
            conn.sender.send(transport_msg)
                .map_err(|_| Error::Network(format!("Failed to send message to peer {}", to)))?;
        } else {
            return Err(Error::Network(format!("No connection to peer {}", to)));
        }
        
        Ok(())
    }
    
    /// Receive incoming messages
    pub async fn recv(&self) -> Option<TransportMessage> {
        let mut rx = self.incoming_rx.write().await;
        rx.recv().await
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
            
            if len > 10 * 1024 * 1024 {
                return Err(Error::Network("Message too large".into()));
            }
            
            // Read message data
            let mut buf = vec![0u8; len];
            stream.read_exact(&mut buf).await
                .map_err(|e| Error::Network(format!("Failed to read message: {}", e)))?;
            
            // Deserialize and forward message
            match bincode::deserialize::<TransportMessage>(&buf) {
                Ok(msg) => {
                    if msg.to != self.config.node_id {
                        warn!("Received message for wrong node: {} != {}", msg.to, self.config.node_id);
                        continue;
                    }
                    
                    self.incoming_tx.send(msg)
                        .map_err(|_| Error::Internal("Failed to forward message".into()))?;
                }
                Err(e) => {
                    error!("Failed to deserialize message: {}", e);
                }
            }
        }
        
        Ok(())
    }
    
    /// Maintain connection to a peer
    async fn maintain_peer_connection(self: Arc<Self>, peer_id: NodeId, peer_addr: SocketAddr) -> Result<()> {
        // Check if already connected
        {
            let connections = self.connections.read().await;
            if connections.contains_key(&peer_id) {
                return Ok(());
            }
        }
        
        // Connect with timeout
        let stream = tokio::time::timeout(
            self.config.connection_timeout,
            TcpStream::connect(&peer_addr)
        )
        .await
        .map_err(|_| Error::Network("Connection timeout".into()))?
        .map_err(|e| Error::Network(format!("Failed to connect: {}", e)))?;
        
        info!("Connected to peer {} at {}", peer_id, peer_addr);
        
        let (reader, writer) = stream.into_split();
        
        // Create message channel
        let (msg_tx, mut msg_rx) = mpsc::unbounded_channel();
        
        // Spawn writer task
        let node_id = self.config.node_id;
        let writer_handle = tokio::spawn(async move {
            let mut writer = writer;
            
            // Send our node ID
            if let Err(e) = writer.write_u64(node_id).await {
                error!("Failed to send node ID: {}", e);
                return;
            }
            
            // Send messages
            while let Some(msg) = msg_rx.recv().await {
                let data = match bincode::serialize(&msg) {
                    Ok(data) => data,
                    Err(e) => {
                        error!("Failed to serialize message: {}", e);
                        continue;
                    }
                };
                
                if let Err(e) = writer.write_u32(data.len() as u32).await {
                    error!("Failed to write message length: {}", e);
                    break;
                }
                
                if let Err(e) = writer.write_all(&data).await {
                    error!("Failed to write message: {}", e);
                    break;
                }
                
                if let Err(e) = writer.flush().await {
                    error!("Failed to flush: {}", e);
                    break;
                }
            }
        });
        
        // Store connection
        {
            let mut connections = self.connections.write().await;
            connections.insert(peer_id, PeerConnection {
                sender: msg_tx,
                handle: writer_handle,
            });
        }
        
        // Spawn reader task
        let transport = self.clone();
        tokio::spawn(async move {
            let mut reader = reader;
            loop {
                // Read message length
                let len = match reader.read_u32().await {
                    Ok(len) => len as usize,
                    Err(e) => {
                        if e.kind() != std::io::ErrorKind::UnexpectedEof {
                            error!("Failed to read message length: {}", e);
                        }
                        break;
                    }
                };
                
                if len > 10 * 1024 * 1024 {
                    error!("Message too large: {}", len);
                    break;
                }
                
                // Read message
                let mut buf = vec![0u8; len];
                if let Err(e) = reader.read_exact(&mut buf).await {
                    error!("Failed to read message: {}", e);
                    break;
                }
                
                // Deserialize and forward
                match bincode::deserialize::<TransportMessage>(&buf) {
                    Ok(msg) => {
                        if let Err(e) = transport.incoming_tx.send(msg) {
                            error!("Failed to forward message: {}", e);
                            break;
                        }
                    }
                    Err(e) => {
                        error!("Failed to deserialize message: {}", e);
                    }
                }
            }
            
            // Remove connection on exit
            let mut connections = transport.connections.write().await;
            connections.remove(&peer_id);
            info!("Disconnected from peer {}", peer_id);
        });
        
        Ok(())
    }
}