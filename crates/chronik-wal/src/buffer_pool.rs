//! Thread-local buffer pool for WAL write operations
//!
//! This module provides an efficient buffer pooling mechanism to reduce allocations
//! during WAL write operations. Buffers are stored in thread-local storage for
//! zero-contention access.

use bytes::{BytesMut, BufMut};
use std::cell::RefCell;
use tracing::{debug, instrument};

/// Default buffer capacity (64KB)
const DEFAULT_BUFFER_CAPACITY: usize = 64 * 1024;

/// Maximum number of buffers to keep in the pool per thread
const MAX_POOL_SIZE: usize = 8;

/// Maximum buffer size before it's discarded instead of returned to pool (1MB)
const MAX_BUFFER_SIZE: usize = 1024 * 1024;

thread_local! {
    /// Thread-local buffer pool
    static BUFFER_POOL: RefCell<BufferPool> = RefCell::new(BufferPool::new());
}

/// Buffer pool statistics
#[derive(Debug, Clone, Default)]
pub struct PoolStats {
    pub buffers_allocated: u64,
    pub buffers_reused: u64,
    pub buffers_discarded: u64,
    pub total_bytes_allocated: u64,
    pub current_pool_size: usize,
}

/// Thread-local buffer pool
struct BufferPool {
    buffers: Vec<BytesMut>,
    stats: PoolStats,
}

impl BufferPool {
    fn new() -> Self {
        Self {
            buffers: Vec::with_capacity(MAX_POOL_SIZE),
            stats: PoolStats::default(),
        }
    }
    
    fn acquire(&mut self, min_capacity: usize) -> BytesMut {
        // Try to find a suitable buffer in the pool
        if let Some(pos) = self.buffers.iter().position(|buf| buf.capacity() >= min_capacity) {
            self.stats.buffers_reused += 1;
            let mut buffer = self.buffers.swap_remove(pos);
            buffer.clear();
            debug!(
                "Reused buffer from pool: capacity={}, pool_size={}",
                buffer.capacity(),
                self.buffers.len()
            );
            return buffer;
        }
        
        // Allocate new buffer
        let capacity = min_capacity.max(DEFAULT_BUFFER_CAPACITY);
        self.stats.buffers_allocated += 1;
        self.stats.total_bytes_allocated += capacity as u64;
        
        debug!(
            "Allocated new buffer: capacity={}, pool_size={}",
            capacity,
            self.buffers.len()
        );
        
        BytesMut::with_capacity(capacity)
    }
    
    fn release(&mut self, mut buffer: BytesMut) {
        // Don't keep oversized buffers
        if buffer.capacity() > MAX_BUFFER_SIZE {
            self.stats.buffers_discarded += 1;
            debug!(
                "Discarded oversized buffer: capacity={}",
                buffer.capacity()
            );
            return;
        }
        
        // Don't exceed pool size
        if self.buffers.len() >= MAX_POOL_SIZE {
            // Replace smallest buffer if this one is larger
            if let Some(min_pos) = self.buffers
                .iter()
                .enumerate()
                .min_by_key(|(_, buf)| buf.capacity())
                .map(|(pos, _)| pos)
            {
                if self.buffers[min_pos].capacity() < buffer.capacity() {
                    self.stats.buffers_discarded += 1;
                    buffer.clear();
                    self.buffers[min_pos] = buffer;
                    debug!("Replaced smaller buffer in pool");
                } else {
                    self.stats.buffers_discarded += 1;
                    debug!("Discarded buffer, pool is full");
                }
            }
            return;
        }
        
        // Add to pool
        buffer.clear();
        self.buffers.push(buffer);
        self.stats.current_pool_size = self.buffers.len();
        debug!("Returned buffer to pool: pool_size={}", self.buffers.len());
    }
    
    fn stats(&self) -> PoolStats {
        let mut stats = self.stats.clone();
        stats.current_pool_size = self.buffers.len();
        stats
    }
}

/// A pooled buffer that automatically returns to the pool when dropped
pub struct PooledBuffer {
    buffer: Option<BytesMut>,
}

impl PooledBuffer {
    /// Get a pooled buffer with at least the specified capacity
    #[instrument(skip_all, fields(min_capacity = min_capacity))]
    pub fn acquire(min_capacity: usize) -> Self {
        let buffer = BUFFER_POOL.with(|pool| {
            pool.borrow_mut().acquire(min_capacity)
        });
        
        Self {
            buffer: Some(buffer),
        }
    }
    
    /// Get a pooled buffer with default capacity
    pub fn acquire_default() -> Self {
        Self::acquire(DEFAULT_BUFFER_CAPACITY)
    }
    
    /// Take ownership of the inner buffer
    pub fn take(mut self) -> BytesMut {
        self.buffer.take().expect("Buffer already taken")
    }
    
    /// Get a reference to the inner buffer
    pub fn as_ref(&self) -> &BytesMut {
        self.buffer.as_ref().expect("Buffer already taken")
    }
    
    /// Get a mutable reference to the inner buffer
    pub fn as_mut(&mut self) -> &mut BytesMut {
        self.buffer.as_mut().expect("Buffer already taken")
    }
    
    /// Write data to the buffer
    pub fn put_slice(&mut self, data: &[u8]) {
        self.as_mut().put_slice(data);
    }
    
    /// Write a single byte to the buffer
    pub fn put_u8(&mut self, val: u8) {
        self.as_mut().put_u8(val);
    }
    
    /// Write a u16 to the buffer
    pub fn put_u16(&mut self, val: u16) {
        self.as_mut().put_u16(val);
    }
    
    /// Write a u32 to the buffer
    pub fn put_u32(&mut self, val: u32) {
        self.as_mut().put_u32(val);
    }
    
    /// Write a u64 to the buffer  
    pub fn put_u64(&mut self, val: u64) {
        self.as_mut().put_u64(val);
    }
    
    /// Write an i32 to the buffer
    pub fn put_i32(&mut self, val: i32) {
        self.as_mut().put_i32(val);
    }
    
    /// Write an i64 to the buffer
    pub fn put_i64(&mut self, val: i64) {
        self.as_mut().put_i64(val);
    }
    
    /// Get the current length of data in the buffer
    pub fn len(&self) -> usize {
        self.as_ref().len()
    }
    
    /// Check if the buffer is empty
    pub fn is_empty(&self) -> bool {
        self.as_ref().is_empty()
    }
    
    /// Clear the buffer
    pub fn clear(&mut self) {
        self.as_mut().clear();
    }
    
    /// Reserve additional capacity
    pub fn reserve(&mut self, additional: usize) {
        self.as_mut().reserve(additional);
    }
    
    /// Split the buffer at the given index
    pub fn split_to(&mut self, at: usize) -> BytesMut {
        self.as_mut().split_to(at)
    }
    
    /// Freeze the buffer into immutable Bytes
    pub fn freeze(self) -> bytes::Bytes {
        self.take().freeze()
    }
}

impl Drop for PooledBuffer {
    fn drop(&mut self) {
        if let Some(buffer) = self.buffer.take() {
            BUFFER_POOL.with(|pool| {
                pool.borrow_mut().release(buffer);
            });
        }
    }
}

impl AsRef<[u8]> for PooledBuffer {
    fn as_ref(&self) -> &[u8] {
        self.as_ref().as_ref()
    }
}

impl AsMut<[u8]> for PooledBuffer {
    fn as_mut(&mut self) -> &mut [u8] {
        self.as_mut().as_mut()
    }
}

/// Get current pool statistics for the calling thread
pub fn pool_stats() -> PoolStats {
    BUFFER_POOL.with(|pool| pool.borrow().stats())
}

/// Clear all buffers from the pool for the calling thread
pub fn clear_pool() {
    BUFFER_POOL.with(|pool| {
        let mut pool = pool.borrow_mut();
        pool.buffers.clear();
        pool.stats.current_pool_size = 0;
        debug!("Cleared thread-local buffer pool");
    });
}

/// Pre-warm the pool with buffers
pub fn prewarm_pool(count: usize) {
    BUFFER_POOL.with(|pool| {
        let mut pool = pool.borrow_mut();
        for _ in 0..count.min(MAX_POOL_SIZE) {
            if pool.buffers.len() >= MAX_POOL_SIZE {
                break;
            }
            pool.buffers.push(BytesMut::with_capacity(DEFAULT_BUFFER_CAPACITY));
            pool.stats.buffers_allocated += 1;
            pool.stats.total_bytes_allocated += DEFAULT_BUFFER_CAPACITY as u64;
        }
        pool.stats.current_pool_size = pool.buffers.len();
        debug!("Pre-warmed pool with {} buffers", pool.buffers.len());
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_buffer_acquire_and_release() {
        clear_pool();
        
        // First acquisition should allocate
        let stats_before = pool_stats();
        {
            let buffer = PooledBuffer::acquire(1024);
            assert!(buffer.len() == 0);
            assert!(buffer.as_ref().capacity() >= 1024);
        }
        
        // After drop, buffer should be in pool
        let stats_after = pool_stats();
        assert_eq!(stats_after.buffers_allocated, stats_before.buffers_allocated + 1);
        assert_eq!(stats_after.current_pool_size, 1);
        
        // Second acquisition should reuse
        {
            let buffer = PooledBuffer::acquire(1024);
            assert!(buffer.as_ref().capacity() >= 1024);
        }
        
        let stats_reused = pool_stats();
        assert_eq!(stats_reused.buffers_reused, stats_after.buffers_reused + 1);
        assert_eq!(stats_reused.buffers_allocated, stats_after.buffers_allocated);
    }
    
    #[test]
    fn test_buffer_operations() {
        let mut buffer = PooledBuffer::acquire_default();
        
        // Test various write operations
        buffer.put_u8(0x42);
        buffer.put_u32(0xDEADBEEF);
        buffer.put_i64(-12345);
        buffer.put_slice(b"Hello, WAL!");
        
        assert_eq!(buffer.len(), 1 + 4 + 8 + 11);
        assert!(!buffer.is_empty());
        
        // Test clear
        buffer.clear();
        assert_eq!(buffer.len(), 0);
        assert!(buffer.is_empty());
    }
    
    #[test]
    fn test_pool_size_limit() {
        clear_pool();
        
        // Create more buffers than pool size
        let buffers: Vec<_> = (0..MAX_POOL_SIZE + 2)
            .map(|_| PooledBuffer::acquire_default())
            .collect();
        
        // Drop all buffers
        drop(buffers);
        
        // Pool should not exceed max size
        let stats = pool_stats();
        assert!(stats.current_pool_size <= MAX_POOL_SIZE);
    }
    
    #[test]
    fn test_oversized_buffer_discard() {
        clear_pool();
        
        // Create an oversized buffer
        {
            let mut buffer = PooledBuffer::acquire(MAX_BUFFER_SIZE + 1);
            buffer.reserve(MAX_BUFFER_SIZE + 1000);
        }
        
        // Oversized buffer should be discarded
        let stats = pool_stats();
        assert_eq!(stats.current_pool_size, 0);
        assert_eq!(stats.buffers_discarded, 1);
    }
    
    #[test]
    fn test_buffer_reuse_with_different_sizes() {
        clear_pool();
        
        // Create buffer with specific size
        {
            let _buffer = PooledBuffer::acquire(8192);
        }
        
        // Smaller request should reuse the larger buffer
        {
            let buffer = PooledBuffer::acquire(4096);
            assert!(buffer.as_ref().capacity() >= 8192);
        }
        
        let stats = pool_stats();
        assert_eq!(stats.buffers_reused, 1);
    }
    
    #[test]
    fn test_prewarm_pool() {
        clear_pool();
        
        prewarm_pool(4);
        
        let stats = pool_stats();
        assert_eq!(stats.current_pool_size, 4);
        assert_eq!(stats.buffers_allocated, 4);
        
        // Acquisitions should reuse prewarmed buffers
        for _ in 0..4 {
            let _buffer = PooledBuffer::acquire_default();
        }
        
        let stats_after = pool_stats();
        assert_eq!(stats_after.buffers_reused, 4);
        assert_eq!(stats_after.buffers_allocated, 4); // No new allocations
    }
    
    #[test]
    fn test_freeze_buffer() {
        let mut buffer = PooledBuffer::acquire_default();
        buffer.put_slice(b"Frozen data");
        
        let frozen = buffer.freeze();
        assert_eq!(&frozen[..], b"Frozen data");
    }
    
    #[test]
    fn test_split_buffer() {
        let mut buffer = PooledBuffer::acquire_default();
        buffer.put_slice(b"Hello, World!");
        
        let split = buffer.split_to(7);
        assert_eq!(&split[..], b"Hello, ");
        assert_eq!(&buffer.as_ref()[..], b"World!");
    }
}