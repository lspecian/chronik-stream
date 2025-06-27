//! File watcher for automatic configuration reloading.

use crate::{Result, ConfigError};
use notify::{
    Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::Mutex;
use tracing::{debug, error, info};

/// Configuration file watcher
pub struct ConfigWatcher {
    path: PathBuf,
    watcher: Arc<Mutex<RecommendedWatcher>>,
}

impl ConfigWatcher {
    /// Create a new configuration watcher
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        
        // Create a dummy watcher for now
        let watcher = notify::recommended_watcher(|_| {})?;
        
        Ok(Self {
            path,
            watcher: Arc::new(Mutex::new(watcher)),
        })
    }
    
    /// Start watching for changes
    pub fn watch<F>(&self, mut callback: F)
    where
        F: FnMut() + Send + 'static,
    {
        let path = self.path.clone();
        let watcher = self.watcher.clone();
        
        tokio::spawn(async move {
            // Create event handler
            let (tx, mut rx) = tokio::sync::mpsc::channel(100);
            
            // Create watcher with event handler
            let mut watcher = match notify::recommended_watcher(move |res: notify::Result<Event>| {
                if let Ok(event) = res {
                    let _ = tx.blocking_send(event);
                }
            }) {
                Ok(w) => w,
                Err(e) => {
                    error!("Failed to create watcher: {}", e);
                    return;
                }
            };
            
            // Start watching
            if let Err(e) = watcher.watch(&path, RecursiveMode::NonRecursive) {
                error!("Failed to watch {:?}: {}", path, e);
                return;
            }
            
            info!("Started watching {:?}", path);
            
            // Process events
            let mut last_reload = std::time::Instant::now();
            let debounce_duration = Duration::from_millis(500);
            
            while let Some(event) = rx.recv().await {
                match event.kind {
                    EventKind::Modify(_) | EventKind::Create(_) => {
                        // Debounce rapid changes
                        let now = std::time::Instant::now();
                        if now.duration_since(last_reload) > debounce_duration {
                            debug!("File change detected: {:?}", event.paths);
                            callback();
                            last_reload = now;
                        }
                    }
                    _ => {}
                }
            }
        });
    }
    
    /// Stop watching
    pub async fn stop(&self) -> Result<()> {
        // In a real implementation, we'd properly stop the watcher
        Ok(())
    }
    
    /// Get the watched path
    pub fn path(&self) -> &Path {
        &self.path
    }
}