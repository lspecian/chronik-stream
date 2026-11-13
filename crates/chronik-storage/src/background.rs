//! Keeping background CPU work from starving request handling.
//!
//! Rebuilding HNSW vector indexes at boot runs on rayon's global pool (inside
//! `instant-distance`). Left to itself that pool takes one thread per core, at
//! normal priority. A node with vector topics then spent its first minutes
//! after every restart with the whole CPU budget on index rebuilds: on a pod
//! with a 16-core limit, 12 of the 16 went to the rebuild, the container was
//! CFS-throttled in 22% of scheduling periods, and `acks=all` produce ran at
//! 619 records/s with a 47 s p99 until it finished. Reproduced on a local
//! three-node cluster seeded with the same index files: p99 went from 11 ms
//! to 101 ms, and a producer writing to a new topic failed partway.
//!
//! Two controls, both applied at startup:
//!
//! - the global pool is sized explicitly ([`BACKGROUND_THREADS_ENV`], default
//!   half the cores the process may use) and its threads are named
//!   `chronik-bg-N`, so they are visible in `top -H`. Before this they
//!   inherited the name of whichever thread first touched rayon, which was the
//!   indexer — 16 anonymous threads reported as `wal-indexer`;
//! - background threads run at a lower scheduling priority
//!   ([`BACKGROUND_NICE`]). On Linux a thread's nice value is inherited by the
//!   threads it spawns, so the threads Tantivy and the blocking pool create
//!   from the indexer runtime are lowered with it. Under contention the
//!   request path is scheduled first; on an idle node the background work
//!   still gets every core it is allowed.

use tracing::{info, warn};

/// Overrides the number of threads in the process-wide rayon pool.
pub const BACKGROUND_THREADS_ENV: &str = "CHRONIK_BACKGROUND_THREADS";

/// Nice value for background threads. Request threads stay at 0.
pub const BACKGROUND_NICE: i32 = 10;

/// Threads for the global pool: the override when it is a positive integer,
/// otherwise half of `parallelism`, never fewer than one.
pub fn background_thread_count(env: Option<&str>, parallelism: usize) -> usize {
    if let Some(n) = env.and_then(|v| v.trim().parse::<usize>().ok()) {
        if n > 0 {
            return n;
        }
    }
    (parallelism / 2).max(1)
}

/// Lower the scheduling priority of the calling thread to [`BACKGROUND_NICE`].
///
/// Linux applies `setpriority(PRIO_PROCESS, tid, ..)` to that one thread, not
/// the whole process. Failure is ignored: the thread simply keeps its normal
/// priority, which is what it had before this existed.
pub fn lower_current_thread_priority() {
    #[cfg(target_os = "linux")]
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        libc::setpriority(libc::PRIO_PROCESS, tid, BACKGROUND_NICE);
    }
}

/// Size, name and de-prioritise rayon's global pool. Call once, first thing
/// in `main`, before anything can touch rayon — the global pool is created on
/// first use and cannot be resized afterwards.
pub fn init_global_pool() {
    let parallelism = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let env = std::env::var(BACKGROUND_THREADS_ENV).ok();
    let threads = background_thread_count(env.as_deref(), parallelism);

    match rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("chronik-bg-{}", i))
        .start_handler(|_| lower_current_thread_priority())
        .build_global()
    {
        Ok(()) => info!(
            threads,
            available = parallelism,
            nice = BACKGROUND_NICE,
            "Background CPU pool sized (override with {})",
            BACKGROUND_THREADS_ENV
        ),
        Err(e) => warn!(
            error = %e,
            "Background CPU pool was already initialised; it keeps one thread per core"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_half_the_cores() {
        assert_eq!(background_thread_count(None, 16), 8);
        assert_eq!(background_thread_count(None, 32), 16);
        assert_eq!(background_thread_count(None, 3), 1);
    }

    #[test]
    fn never_fewer_than_one_thread() {
        assert_eq!(background_thread_count(None, 1), 1);
        assert_eq!(background_thread_count(None, 0), 1);
    }

    #[test]
    fn a_positive_override_wins() {
        assert_eq!(background_thread_count(Some("4"), 16), 4);
        assert_eq!(background_thread_count(Some(" 24 "), 16), 24);
    }

    #[test]
    fn an_invalid_override_falls_back_to_the_default() {
        assert_eq!(background_thread_count(Some("0"), 16), 8);
        assert_eq!(background_thread_count(Some("lots"), 16), 8);
        assert_eq!(background_thread_count(Some(""), 16), 8);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn lowering_priority_affects_only_the_calling_thread() {
        fn nice_of_current_thread() -> i32 {
            unsafe {
                let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
                libc::getpriority(libc::PRIO_PROCESS, tid)
            }
        }
        let before = nice_of_current_thread();
        let lowered = std::thread::spawn(|| {
            lower_current_thread_priority();
            nice_of_current_thread()
        })
        .join()
        .unwrap();
        assert_eq!(lowered, BACKGROUND_NICE.max(before));
        assert_eq!(nice_of_current_thread(), before);
    }
}
