//! I/O priority control for WAL operations.
//!
//! v2.2.7: Set higher I/O priority for WAL to prevent disk contention with Tantivy indexing.
//! Uses Linux ioprio_set syscall to prioritize WAL fsyncs over background indexing.

use std::io;

/// I/O priority class (Linux IOPRIO_CLASS_*)
#[derive(Debug, Clone, Copy)]
#[repr(i32)]
pub enum IoPriorityClass {
    /// Realtime I/O (highest priority, requires CAP_SYS_ADMIN)
    RealTime = 1,
    /// Best-effort I/O (default, priority levels 0-7)
    BestEffort = 2,
    /// Idle I/O (lowest priority, only when no other I/O)
    Idle = 3,
}

/// I/O priority level (0 = highest, 7 = lowest)
pub type IoPriorityLevel = u8;

/// Set I/O priority for current thread
///
/// # Arguments
/// * `class` - I/O priority class (realtime, best-effort, idle)
/// * `level` - Priority level within class (0-7, lower = higher priority)
///
/// # Example
/// ```
/// // Set WAL thread to best-effort priority 0 (highest within best-effort)
/// set_io_priority(IoPriorityClass::BestEffort, 0);
/// ```
#[cfg(target_os = "linux")]
pub fn set_io_priority(class: IoPriorityClass, level: IoPriorityLevel) -> io::Result<()> {
    use libc::{syscall, SYS_ioprio_set};

    const IOPRIO_WHO_PROCESS: i32 = 1;  // Target current process/thread
    const IOPRIO_CLASS_SHIFT: i32 = 13;

    // Validate level (must be 0-7)
    if level > 7 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Invalid I/O priority level: {} (must be 0-7)", level),
        ));
    }

    // Construct ioprio value: (class << 13) | level
    let ioprio = ((class as i32) << IOPRIO_CLASS_SHIFT) | (level as i32);

    // Call ioprio_set(IOPRIO_WHO_PROCESS, 0, ioprio)
    // 0 means current thread
    let result = unsafe {
        syscall(SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, ioprio)
    };

    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        tracing::debug!(
            "Set I/O priority: class={:?}, level={} (ioprio=0x{:04x})",
            class,
            level,
            ioprio
        );
        Ok(())
    }
}

/// Set I/O priority for current thread (no-op on non-Linux)
#[cfg(not(target_os = "linux"))]
pub fn set_io_priority(_class: IoPriorityClass, _level: IoPriorityLevel) -> io::Result<()> {
    tracing::warn!("I/O priority control not supported on this platform (Linux only)");
    Ok(())
}

/// Set WAL thread I/O priority (best-effort priority 0 = highest)
pub fn set_wal_priority() -> io::Result<()> {
    set_io_priority(IoPriorityClass::BestEffort, 0)
}

/// Set Tantivy indexing thread I/O priority (best-effort priority 4 = lower)
pub fn set_indexing_priority() -> io::Result<()> {
    set_io_priority(IoPriorityClass::BestEffort, 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_set_io_priority() {
        // Should not fail on any platform (no-op on non-Linux)
        let result = set_wal_priority();
        assert!(result.is_ok() || !cfg!(target_os = "linux"));
    }

    #[test]
    fn test_invalid_level() {
        #[cfg(target_os = "linux")]
        {
            let result = set_io_priority(IoPriorityClass::BestEffort, 8);
            assert!(result.is_err());
        }
    }
}
