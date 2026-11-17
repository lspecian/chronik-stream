//! Durable Raft log storage.
//!
//! `RaftLogStore` implements raft-rs's [`Storage`] with the exact in-memory
//! semantics of the upstream `MemStorageCore`, and makes every mutation durable
//! before it becomes visible:
//!
//! ```text
//! <dir>/state              HardState + ConfState   (temp file, fsync, rename)
//! <dir>/snapshot           latest Snapshot          (temp file, fsync, rename)
//! <dir>/log/<index>.seg    entries, one record each: [len u32][crc32 u32][Entry protobuf]
//! ```
//!
//! Rules the previous storage broke, and this one enforces:
//! - **No fabricated entries.** A gap in the log is an error, never padded.
//! - **Memory and disk agree.** Suffix truncation and prefix compaction are
//!   applied to the files first, then to memory.
//! - **Reads never touch disk.** Everything after the snapshot is held in
//!   memory; the snapshot bounds it.
//! - **A torn tail is the only tolerated damage.** A short or corrupt final
//!   record of the last segment (a crash mid-append) is cut off at open;
//!   damage anywhere else fails the open, because the node may have
//!   acknowledged what it can no longer read.
//!
//! Reads are served under a read lock. Writes are made by the single thread
//! that drives `RawNode`, through a clone of the store.

use std::cmp;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use prost::Message;
use raft::eraftpb::{ConfState, Entry, HardState, Snapshot, SnapshotMetadata};
use raft::util::limit_size;
use raft::{Error as RaftError, GetEntriesContext, RaftState, Storage, StorageError};

const STATE_FILE: &str = "state";
const SNAPSHOT_FILE: &str = "snapshot";
const LOG_DIR: &str = "log";
const SEGMENT_SUFFIX: &str = ".seg";
const STATE_MAGIC: &[u8; 4] = b"CRS1";
const SNAPSHOT_MAGIC: &[u8; 4] = b"CRN1";
/// Record header: payload length, then CRC32 of the payload.
const RECORD_HEADER: u64 = 8;
/// Refuse a record claiming more than this; a corrupt length must not allocate gigabytes.
const MAX_RECORD_BYTES: u32 = 256 * 1024 * 1024;

/// Tunables. The defaults suit the cluster catalog.
#[derive(Debug, Clone)]
pub struct LogStoreOptions {
    /// Start a new segment once the active one reaches this size.
    pub segment_bytes: u64,
    /// fsync appended entries, state and snapshots. Only tests turn this off.
    pub sync: bool,
}

impl Default for LogStoreOptions {
    fn default() -> Self {
        Self { segment_bytes: 64 * 1024 * 1024, sync: true }
    }
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

#[derive(Debug, Clone)]
struct Segment {
    /// Index of the first entry written to it; also its file name.
    first_index: u64,
    path: PathBuf,
    /// Bytes of valid records.
    len: u64,
    /// Index the next record appended to it must have.
    next_index: u64,
}

/// Where an in-memory entry's record starts on disk.
#[derive(Debug, Clone, Copy)]
struct Location {
    segment: u64,
    offset: u64,
}

struct Core {
    dir: PathBuf,
    opts: LogStoreOptions,
    raft_state: RaftState,
    /// `entries[i]` has index `snapshot.metadata.index + 1 + i`.
    entries: Vec<Entry>,
    locations: Vec<Location>,
    /// Latest snapshot, data included; served to followers that are too far behind.
    snapshot: Snapshot,
    segments: Vec<Segment>,
}

impl Core {
    fn snapshot_metadata(&self) -> &SnapshotMetadata {
        self.snapshot.get_metadata()
    }

    fn first_index(&self) -> u64 {
        match self.entries.first() {
            Some(e) => e.index,
            None => self.snapshot_metadata().index + 1,
        }
    }

    fn last_index(&self) -> u64 {
        match self.entries.last() {
            Some(e) => e.index,
            None => self.snapshot_metadata().index,
        }
    }

    fn log_dir(&self) -> PathBuf {
        self.dir.join(LOG_DIR)
    }

    fn segment_path(&self, first_index: u64) -> PathBuf {
        self.log_dir().join(format!("{:020}{}", first_index, SEGMENT_SUFFIX))
    }

    // ---- state -------------------------------------------------------------

    fn persist_state(&self, hs: &HardState, cs: &ConfState) -> io::Result<()> {
        let hs_bytes = hs.encode_to_vec();
        let cs_bytes = cs.encode_to_vec();
        let mut buf = Vec::with_capacity(16 + hs_bytes.len() + cs_bytes.len());
        buf.extend_from_slice(STATE_MAGIC);
        buf.extend_from_slice(&(hs_bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(&hs_bytes);
        buf.extend_from_slice(&(cs_bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(&cs_bytes);
        let crc = crc32fast::hash(&buf);
        buf.extend_from_slice(&crc.to_le_bytes());
        write_atomically(&self.dir, STATE_FILE, &buf, self.opts.sync)
    }

    fn set_hardstate(&mut self, hs: HardState) -> io::Result<()> {
        if hs == self.raft_state.hard_state {
            return Ok(());
        }
        self.persist_state(&hs, &self.raft_state.conf_state)?;
        self.raft_state.hard_state = hs;
        Ok(())
    }

    fn set_conf_state(&mut self, cs: ConfState) -> io::Result<()> {
        if cs == self.raft_state.conf_state {
            return Ok(());
        }
        self.persist_state(&self.raft_state.hard_state, &cs)?;
        self.raft_state.conf_state = cs;
        Ok(())
    }

    // ---- appends -----------------------------------------------------------

    fn append(&mut self, ents: &[Entry]) -> io::Result<()> {
        if ents.is_empty() {
            return Ok(());
        }
        let first_new = ents[0].index;
        if first_new < self.first_index() {
            return Err(invalid(format!(
                "append would overwrite compacted entries: compacted through {}, append starts at {}",
                self.first_index() - 1,
                first_new
            )));
        }
        if first_new > self.last_index() + 1 {
            return Err(invalid(format!(
                "append would leave a gap: last index {}, append starts at {}",
                self.last_index(),
                first_new
            )));
        }
        for pair in ents.windows(2) {
            if pair[1].index != pair[0].index + 1 {
                return Err(invalid(format!(
                    "appended entries are not contiguous: {} then {}",
                    pair[0].index, pair[1].index
                )));
            }
        }

        if first_new <= self.last_index() {
            self.truncate_suffix(first_new)?;
        }
        self.write_entries(ents)?;
        Ok(())
    }

    /// Remove every entry with index >= `from`, on disk then in memory.
    fn truncate_suffix(&mut self, from: u64) -> io::Result<()> {
        let pos = (from - self.first_index()) as usize;
        let loc = self.locations[pos];
        // Later segments go first, newest first: a crash part-way leaves a
        // shorter prefix of the old log, never a log with a hole in it.
        while let Some(last) = self.segments.last() {
            if last.first_index <= loc.segment {
                break;
            }
            fs::remove_file(&last.path)?;
            self.segments.pop();
        }
        sync_dir(&self.log_dir(), self.opts.sync)?;
        let seg = self
            .segments
            .last_mut()
            .filter(|s| s.first_index == loc.segment)
            .ok_or_else(|| invalid(format!("segment {} for index {} is missing", loc.segment, from)))?;
        let file = OpenOptions::new().write(true).open(&seg.path)?;
        file.set_len(loc.offset)?;
        if self.opts.sync {
            file.sync_all()?;
        }
        seg.len = loc.offset;
        seg.next_index = from;
        self.entries.truncate(pos);
        self.locations.truncate(pos);
        Ok(())
    }

    fn write_entries(&mut self, ents: &[Entry]) -> io::Result<()> {
        // A new segment also when the active one does not end just before
        // these entries: after a snapshot install interrupted by a crash, a
        // leftover segment can end below the snapshot, and writing after it
        // would leave a hole inside one file.
        let needs_segment = match self.segments.last() {
            None => true,
            Some(s) => s.len >= self.opts.segment_bytes || s.next_index != ents[0].index,
        };
        if needs_segment {
            self.open_segment(ents[0].index)?;
        }
        let seg = self.segments.last_mut().expect("segment opened above");
        let mut buf = Vec::new();
        let mut locs = Vec::with_capacity(ents.len());
        let mut offset = seg.len;
        for e in ents {
            let payload = e.encode_to_vec();
            if payload.len() as u64 > MAX_RECORD_BYTES as u64 {
                return Err(invalid(format!("entry {} is {} bytes, over the record limit", e.index, payload.len())));
            }
            locs.push(Location { segment: seg.first_index, offset });
            buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            buf.extend_from_slice(&crc32fast::hash(&payload).to_le_bytes());
            buf.extend_from_slice(&payload);
            offset += RECORD_HEADER + payload.len() as u64;
        }
        let mut file = OpenOptions::new().write(true).open(&seg.path)?;
        file.seek(SeekFrom::Start(seg.len))?;
        if let Err(e) = file.write_all(&buf).and_then(|_| if self.opts.sync { file.sync_data() } else { Ok(()) }) {
            // Leave the file as memory describes it; a partial write would
            // otherwise be read back as a torn tail at the next open, or worse,
            // be followed by records appended after it.
            let _ = file.set_len(seg.len);
            return Err(e);
        }
        seg.len = offset;
        seg.next_index = ents[ents.len() - 1].index + 1;
        self.entries.extend_from_slice(ents);
        self.locations.extend(locs);
        Ok(())
    }

    fn open_segment(&mut self, first_index: u64) -> io::Result<()> {
        let path = self.segment_path(first_index);
        File::create(&path)?;
        sync_dir(&self.log_dir(), self.opts.sync)?;
        self.segments.push(Segment { first_index, path, len: 0, next_index: first_index });
        Ok(())
    }

    // ---- snapshots and compaction -----------------------------------------

    fn persist_snapshot(&self, snapshot: &Snapshot) -> io::Result<()> {
        let bytes = snapshot.encode_to_vec();
        let mut buf = Vec::with_capacity(16 + bytes.len());
        buf.extend_from_slice(SNAPSHOT_MAGIC);
        buf.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        buf.extend_from_slice(&bytes);
        let crc = crc32fast::hash(&buf);
        buf.extend_from_slice(&crc.to_le_bytes());
        write_atomically(&self.dir, SNAPSHOT_FILE, &buf, self.opts.sync)
    }

    /// Install a snapshot received from the leader: it replaces the whole log.
    fn apply_snapshot(&mut self, mut snapshot: Snapshot) -> io::Result<Result<(), RaftError>> {
        let index = snapshot.get_metadata().index;
        if self.first_index() > index {
            return Ok(Err(RaftError::Store(StorageError::SnapshotOutOfDate)));
        }
        let mut hs = self.raft_state.hard_state.clone();
        hs.term = cmp::max(hs.term, snapshot.get_metadata().term);
        hs.commit = index;
        let cs = snapshot.mut_metadata().take_conf_state();
        snapshot.mut_metadata().set_conf_state(cs.clone());

        // Snapshot first: once it is durable, recovery discards everything it
        // covers, so a crash before the segments go cannot resurrect them.
        self.persist_snapshot(&snapshot)?;
        self.persist_state(&hs, &cs)?;
        for seg in self.segments.drain(..) {
            fs::remove_file(&seg.path)?;
        }
        sync_dir(&self.log_dir(), self.opts.sync)?;

        self.snapshot = snapshot;
        self.raft_state.hard_state = hs;
        self.raft_state.conf_state = cs;
        self.entries.clear();
        self.locations.clear();
        Ok(Ok(()))
    }

    /// Record a snapshot this node made of its own applied state at `index`,
    /// then compact the log through it.
    fn create_snapshot(&mut self, index: u64, conf_state: ConfState, data: Vec<u8>) -> io::Result<()> {
        if index <= self.snapshot_metadata().index {
            return Ok(());
        }
        if index > self.raft_state.hard_state.commit {
            return Err(invalid(format!(
                "snapshot at {} is beyond the commit index {}",
                index, self.raft_state.hard_state.commit
            )));
        }
        let term = self.entries[(index - self.first_index()) as usize].term;
        let mut snapshot = Snapshot::default();
        snapshot.data = data.into();
        let meta = snapshot.mut_metadata();
        meta.index = index;
        meta.term = term;
        meta.set_conf_state(conf_state);
        self.persist_snapshot(&snapshot)?;
        self.snapshot = snapshot;
        self.compact(index + 1)
    }

    /// Drop entries below `compact_index`. Only whole segments leave the disk;
    /// recovery skips what the snapshot covers in the first one kept.
    fn compact(&mut self, compact_index: u64) -> io::Result<()> {
        if compact_index <= self.first_index() {
            return Ok(());
        }
        if compact_index > self.snapshot_metadata().index + 1 {
            return Err(invalid(format!(
                "compaction to {} would discard entries the snapshot at {} does not cover",
                compact_index,
                self.snapshot_metadata().index
            )));
        }
        let drop = (compact_index - self.first_index()) as usize;
        self.entries.drain(..drop);
        self.locations.drain(..drop);
        let mut removed = false;
        while self.segments.len() > 1 && self.segments[1].first_index <= compact_index {
            let seg = self.segments.remove(0);
            fs::remove_file(&seg.path)?;
            removed = true;
        }
        if removed {
            sync_dir(&self.log_dir(), self.opts.sync)?;
        }
        Ok(())
    }

    fn snapshot_for(&self, request_index: u64) -> Result<Snapshot, RaftError> {
        if self.snapshot_metadata().index < request_index || self.snapshot_metadata().index == 0 {
            return Err(RaftError::Store(StorageError::SnapshotTemporarilyUnavailable));
        }
        Ok(self.snapshot.clone())
    }
}

/// Durable Raft storage. Cheap to clone; clones share state.
#[derive(Clone)]
pub struct RaftLogStore {
    core: Arc<RwLock<Core>>,
    /// Set when a follower needed a snapshot newer than the stored one.
    snapshot_requested: Arc<AtomicBool>,
}

impl RaftLogStore {
    /// Open (creating if absent) the store in `dir` and recover its contents.
    pub fn open(dir: impl AsRef<Path>, opts: LogStoreOptions) -> io::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(dir.join(LOG_DIR))?;
        // A temp file is a write that never got renamed: it was never durable.
        for name in [STATE_FILE, SNAPSHOT_FILE] {
            let _ = fs::remove_file(dir.join(format!("{}.tmp", name)));
        }

        let snapshot = match read_framed(&dir.join(SNAPSHOT_FILE), SNAPSHOT_MAGIC, 8)? {
            Some(bytes) => Snapshot::decode(bytes.as_slice()).map_err(|e| invalid(format!("snapshot: {}", e)))?,
            None => Snapshot::default(),
        };
        let raft_state = match read_framed(&dir.join(STATE_FILE), STATE_MAGIC, 0)? {
            Some(bytes) => decode_state(&bytes)?,
            None => RaftState::default(),
        };

        let mut core = Core {
            dir,
            opts,
            raft_state,
            entries: Vec::new(),
            locations: Vec::new(),
            snapshot,
            segments: Vec::new(),
        };
        recover_segments(&mut core)?;
        Ok(Self { core: Arc::new(RwLock::new(core)), snapshot_requested: Arc::new(AtomicBool::new(false)) })
    }

    fn rl(&self) -> RwLockReadGuard<'_, Core> {
        self.core.read().unwrap_or_else(|p| p.into_inner())
    }

    fn wl(&self) -> RwLockWriteGuard<'_, Core> {
        self.core.write().unwrap_or_else(|p| p.into_inner())
    }

    /// Persist and append entries from a `Ready`, replacing any conflicting suffix.
    pub fn append(&self, ents: &[Entry]) -> io::Result<()> {
        self.wl().append(ents)
    }

    /// Persist a `HardState` from a `Ready`. A no-op when unchanged.
    pub fn set_hardstate(&self, hs: HardState) -> io::Result<()> {
        self.wl().set_hardstate(hs)
    }

    /// Persist only a new commit index (a `LightReady` carries no full `HardState`).
    pub fn set_commit(&self, commit: u64) -> io::Result<()> {
        let mut core = self.wl();
        let mut hs = core.raft_state.hard_state.clone();
        hs.commit = commit;
        core.set_hardstate(hs)
    }

    /// Persist the configuration after a ConfChange is applied.
    pub fn set_conf_state(&self, cs: ConfState) -> io::Result<()> {
        self.wl().set_conf_state(cs)
    }

    /// Install a snapshot received from the leader. The outer error is I/O
    /// (fatal); the inner one is raft's verdict on the snapshot.
    pub fn apply_snapshot(&self, snapshot: Snapshot) -> io::Result<Result<(), RaftError>> {
        self.wl().apply_snapshot(snapshot)
    }

    /// Save a snapshot of this node's applied state at `index` (≤ commit) and
    /// compact the log through it.
    pub fn create_snapshot(&self, index: u64, conf_state: ConfState, data: Vec<u8>) -> io::Result<()> {
        self.snapshot_requested.store(false, Ordering::Relaxed);
        self.wl().create_snapshot(index, conf_state, data)
    }

    /// Whether raft asked for a snapshot newer than the stored one since the
    /// last `create_snapshot`. The driver should snapshot its applied state.
    pub fn snapshot_requested(&self) -> bool {
        self.snapshot_requested.load(Ordering::Relaxed)
    }

    /// Index and term of the stored snapshot (0, 0 when none).
    pub fn snapshot_index_term(&self) -> (u64, u64) {
        let core = self.rl();
        let m = core.snapshot_metadata();
        (m.index, m.term)
    }

    /// Data of the stored snapshot, for rebuilding the state machine at boot.
    pub fn snapshot_data(&self) -> Option<Vec<u8>> {
        let core = self.rl();
        (core.snapshot_metadata().index > 0).then(|| core.snapshot.data.to_vec())
    }

    /// Number of entries held after the snapshot.
    pub fn entry_count(&self) -> usize {
        self.rl().entries.len()
    }
}

impl Storage for RaftLogStore {
    fn initial_state(&self) -> raft::Result<RaftState> {
        Ok(self.rl().raft_state.clone())
    }

    fn entries(
        &self,
        low: u64,
        high: u64,
        max_size: impl Into<Option<u64>>,
        _context: GetEntriesContext,
    ) -> raft::Result<Vec<Entry>> {
        let core = self.rl();
        if low < core.first_index() {
            return Err(RaftError::Store(StorageError::Compacted));
        }
        if high > core.last_index() + 1 {
            return Err(RaftError::Store(StorageError::Unavailable));
        }
        let offset = core.first_index();
        let mut ents = core.entries[(low - offset) as usize..(high - offset) as usize].to_vec();
        limit_size(&mut ents, max_size.into());
        Ok(ents)
    }

    fn term(&self, idx: u64) -> raft::Result<u64> {
        let core = self.rl();
        if idx == core.snapshot_metadata().index {
            return Ok(core.snapshot_metadata().term);
        }
        let offset = core.first_index();
        if idx < offset {
            return Err(RaftError::Store(StorageError::Compacted));
        }
        if idx > core.last_index() {
            return Err(RaftError::Store(StorageError::Unavailable));
        }
        Ok(core.entries[(idx - offset) as usize].term)
    }

    fn first_index(&self) -> raft::Result<u64> {
        Ok(self.rl().first_index())
    }

    fn last_index(&self) -> raft::Result<u64> {
        Ok(self.rl().last_index())
    }

    fn snapshot(&self, request_index: u64, _to: u64) -> raft::Result<Snapshot> {
        let result = self.rl().snapshot_for(request_index);
        if result.is_err() {
            self.snapshot_requested.store(true, Ordering::Relaxed);
        }
        result
    }
}

// ---- recovery ---------------------------------------------------------------

fn decode_state(bytes: &[u8]) -> io::Result<RaftState> {
    let mut cur = bytes;
    let mut take = |n: usize| -> io::Result<&[u8]> {
        if cur.len() < n {
            return Err(invalid("state file is truncated"));
        }
        let (head, rest) = cur.split_at(n);
        cur = rest;
        Ok(head)
    };
    let hs_len = u32::from_le_bytes(take(4)?.try_into().unwrap()) as usize;
    let hs = HardState::decode(take(hs_len)?).map_err(|e| invalid(format!("hard state: {}", e)))?;
    let cs_len = u32::from_le_bytes(take(4)?.try_into().unwrap()) as usize;
    let cs = ConfState::decode(take(cs_len)?).map_err(|e| invalid(format!("conf state: {}", e)))?;
    Ok(RaftState::new(hs, cs))
}

/// Read a `magic | [len u64] | body | crc32` file written by `write_atomically`.
/// Returns the body (for the state file, which has no length, everything
/// between magic and crc). These files are only ever replaced by rename, so
/// any damage is real corruption and fails the open.
fn read_framed(path: &Path, magic: &[u8; 4], len_bytes: usize) -> io::Result<Option<Vec<u8>>> {
    let mut buf = Vec::new();
    match File::open(path) {
        Ok(mut f) => f.read_to_end(&mut buf)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if buf.len() < 8 + len_bytes || &buf[..4] != magic {
        return Err(invalid(format!("{} is not a valid file", path.display())));
    }
    let (content, crc) = buf.split_at(buf.len() - 4);
    if crc32fast::hash(content) != u32::from_le_bytes(crc.try_into().unwrap()) {
        return Err(invalid(format!("{} failed its checksum", path.display())));
    }
    let body = &content[4 + len_bytes..];
    if len_bytes == 8 {
        let len = u64::from_le_bytes(content[4..12].try_into().unwrap()) as usize;
        if len != body.len() {
            return Err(invalid(format!("{} has a wrong length", path.display())));
        }
    }
    Ok(Some(body.to_vec()))
}

fn recover_segments(core: &mut Core) -> io::Result<()> {
    let log_dir = core.log_dir();
    let mut names: Vec<(u64, PathBuf)> = Vec::new();
    for dent in fs::read_dir(&log_dir)? {
        let path = dent?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        let Some(stem) = name.strip_suffix(SEGMENT_SUFFIX) else { continue };
        let first = stem.parse::<u64>().map_err(|_| invalid(format!("unexpected file in raft log: {}", name)))?;
        names.push((first, path));
    }
    names.sort();

    let snap_index = core.snapshot_metadata().index;
    let snap_term = core.snapshot_metadata().term;
    let count = names.len();
    // An entry stored at the snapshot's index under a different term proves
    // that what follows it belongs to a history the snapshot replaced (a crash
    // between installing a leader's snapshot and deleting the old segments).
    let mut replaced_history = false;
    let mut removed_any = false;
    for (i, (first, path)) in names.into_iter().enumerate() {
        let is_last = i + 1 == count;
        let (records, valid_len, torn) = read_segment(&path)?;
        if torn {
            if !is_last {
                return Err(invalid(format!("{} is damaged before the end of the log", path.display())));
            }
            tracing::warn!(
                "Raft log: cutting a torn final record from {} at byte {} (crash during append)",
                path.display(),
                valid_len
            );
            let f = OpenOptions::new().write(true).open(&path)?;
            f.set_len(valid_len)?;
            f.sync_all()?;
        }
        if records.is_empty() && !is_last {
            return Err(invalid(format!("{} is empty but is not the last segment", path.display())));
        }
        if let Some((e, _)) = records.first() {
            if e.index != first {
                return Err(invalid(format!(
                    "{} is named for index {} but starts at {}",
                    path.display(),
                    first,
                    e.index
                )));
            }
        }
        for pair in records.windows(2) {
            if pair[1].0.index != pair[0].0.index + 1 {
                return Err(invalid(format!(
                    "{} is not contiguous: index {} then {}",
                    path.display(),
                    pair[0].0.index,
                    pair[1].0.index
                )));
            }
        }
        let next_index = records.last().map(|(e, _)| e.index + 1).unwrap_or(first);
        // Checked before a covered segment is deleted below: the entry at the
        // snapshot index is usually in one.
        if snap_index > 0 && records.iter().any(|(e, _)| e.index == snap_index && e.term != snap_term) {
            replaced_history = true;
        }

        // Wholly covered by the snapshot: nothing in it is needed again.
        if !is_last && next_index <= snap_index + 1 {
            fs::remove_file(&path)?;
            removed_any = true;
            continue;
        }
        for (entry, offset) in records {
            if entry.index <= snap_index {
                continue;
            }
            let expected = core.entries.last().map(|e| e.index + 1).unwrap_or(snap_index + 1);
            if entry.index != expected {
                return Err(invalid(format!(
                    "raft log is not contiguous after the snapshot at {}: expected index {}, found {} in {}",
                    snap_index,
                    expected,
                    entry.index,
                    path.display()
                )));
            }
            core.entries.push(entry);
            core.locations.push(Location { segment: first, offset });
        }
        core.segments.push(Segment { first_index: first, path, len: valid_len, next_index });
    }
    if removed_any {
        sync_dir(&log_dir, core.opts.sync)?;
    }
    if replaced_history && !core.entries.is_empty() {
        tracing::warn!(
            "Raft log: discarding {} entries after the snapshot at {} — they belong to a history the snapshot replaced",
            core.entries.len(),
            snap_index
        );
        let from = core.entries[0].index;
        core.truncate_suffix(from)?;
    }
    Ok(())
}


/// Records of one segment with their byte offsets, the length of the valid
/// prefix, and whether anything after it was unreadable.
fn read_segment(path: &Path) -> io::Result<(Vec<(Entry, u64)>, u64, bool)> {
    let mut buf = Vec::new();
    File::open(path)?.read_to_end(&mut buf)?;
    let mut records = Vec::new();
    let mut pos = 0usize;
    while pos < buf.len() {
        if buf.len() - pos < RECORD_HEADER as usize {
            return Ok((records, pos as u64, true));
        }
        let len = u32::from_le_bytes(buf[pos..pos + 4].try_into().unwrap());
        let crc = u32::from_le_bytes(buf[pos + 4..pos + 8].try_into().unwrap());
        let start = pos + RECORD_HEADER as usize;
        if len > MAX_RECORD_BYTES || buf.len() - start < len as usize {
            return Ok((records, pos as u64, true));
        }
        let payload = &buf[start..start + len as usize];
        if crc32fast::hash(payload) != crc {
            return Ok((records, pos as u64, true));
        }
        let entry = Entry::decode(payload).map_err(|e| invalid(format!("{}: undecodable entry: {}", path.display(), e)))?;
        records.push((entry, pos as u64));
        pos = start + len as usize;
    }
    Ok((records, pos as u64, false))
}

// ---- file helpers -----------------------------------------------------------

fn write_atomically(dir: &Path, name: &str, data: &[u8], sync: bool) -> io::Result<()> {
    let tmp = dir.join(format!("{}.tmp", name));
    {
        let mut f = File::create(&tmp)?;
        f.write_all(data)?;
        if sync {
            f.sync_all()?;
        }
    }
    fs::rename(&tmp, dir.join(name))?;
    sync_dir(dir, sync)
}

fn sync_dir(dir: &Path, sync: bool) -> io::Result<()> {
    if sync {
        File::open(dir)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn entry(index: u64, term: u64) -> Entry {
        let mut e = Entry::default();
        e.index = index;
        e.term = term;
        e.data = format!("cmd-{}-{}", index, term).into_bytes().into();
        e
    }

    fn snapshot(index: u64, term: u64, voters: Vec<u64>) -> Snapshot {
        let mut s = Snapshot::default();
        s.mut_metadata().index = index;
        s.mut_metadata().term = term;
        s.mut_metadata().mut_conf_state().voters = voters;
        s
    }

    fn opts(segment_bytes: u64) -> LogStoreOptions {
        LogStoreOptions { segment_bytes, sync: true }
    }

    fn open(dir: &TempDir) -> RaftLogStore {
        RaftLogStore::open(dir.path(), opts(1 << 20)).unwrap()
    }

    /// Snapshot at 2 (term 2), then entries 3, 4, 5 with terms 3, 4, 5 — the
    /// upstream MemStorage fixture, expressed through the public API.
    fn fixture(dir: &TempDir) -> RaftLogStore {
        let s = open(dir);
        s.apply_snapshot(snapshot(2, 2, vec![1, 2, 3])).unwrap().unwrap();
        s.append(&[entry(3, 3), entry(4, 4), entry(5, 5)]).unwrap();
        s
    }

    fn all(s: &RaftLogStore) -> Vec<Entry> {
        let first = s.first_index().unwrap();
        let last = s.last_index().unwrap();
        s.entries(first, last + 1, None, GetEntriesContext::empty(false)).unwrap()
    }

    fn segment_files(dir: &TempDir) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = fs::read_dir(dir.path().join(LOG_DIR))
            .unwrap()
            .map(|d| d.unwrap().path())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn term_matches_upstream() {
        let dir = TempDir::new().unwrap();
        let s = fixture(&dir);
        assert_eq!(s.term(1), Err(RaftError::Store(StorageError::Compacted)));
        assert_eq!(s.term(2), Ok(2));
        assert_eq!(s.term(3), Ok(3));
        assert_eq!(s.term(5), Ok(5));
        assert_eq!(s.term(6), Err(RaftError::Store(StorageError::Unavailable)));
    }

    #[test]
    fn entries_respect_bounds_and_size() {
        let dir = TempDir::new().unwrap();
        let s = fixture(&dir);
        s.append(&[entry(6, 6)]).unwrap();
        let ctx = || GetEntriesContext::empty(false);
        assert_eq!(s.entries(2, 6, None, ctx()), Err(RaftError::Store(StorageError::Compacted)));
        assert_eq!(s.entries(3, 4, None, ctx()).unwrap(), vec![entry(3, 3)]);
        assert_eq!(s.entries(4, 7, None, ctx()).unwrap(), vec![entry(4, 4), entry(5, 5), entry(6, 6)]);
        let one = entry(4, 4).encoded_len() as u64;
        assert_eq!(s.entries(4, 7, Some(one), ctx()).unwrap(), vec![entry(4, 4)]);
        // limit_size always returns at least one entry.
        assert_eq!(s.entries(4, 7, Some(0), ctx()).unwrap(), vec![entry(4, 4)]);
        assert_eq!(s.entries(4, 8, None, ctx()), Err(RaftError::Store(StorageError::Unavailable)));
        assert_eq!((s.first_index().unwrap(), s.last_index().unwrap()), (3, 6));
    }

    /// The upstream append table; every outcome must also survive a reopen.
    #[test]
    fn append_matches_upstream_and_persists() {
        let cases: Vec<(Vec<Entry>, Option<Vec<Entry>>)> = vec![
            (vec![entry(3, 3), entry(4, 4), entry(5, 5)], Some(vec![entry(3, 3), entry(4, 4), entry(5, 5)])),
            (vec![entry(3, 3), entry(4, 6), entry(5, 6)], Some(vec![entry(3, 3), entry(4, 6), entry(5, 6)])),
            (
                vec![entry(3, 3), entry(4, 4), entry(5, 5), entry(6, 5)],
                Some(vec![entry(3, 3), entry(4, 4), entry(5, 5), entry(6, 5)]),
            ),
            (vec![entry(2, 3), entry(3, 3), entry(4, 5)], None),
            (vec![entry(4, 5)], Some(vec![entry(3, 3), entry(4, 5)])),
            (vec![entry(6, 6)], Some(vec![entry(3, 3), entry(4, 4), entry(5, 5), entry(6, 6)])),
            // A gap is refused, never padded.
            (vec![entry(7, 6)], None),
        ];
        for (i, (ents, want)) in cases.into_iter().enumerate() {
            let dir = TempDir::new().unwrap();
            let s = fixture(&dir);
            let res = s.append(&ents);
            match want {
                Some(want) => {
                    res.unwrap();
                    assert_eq!(all(&s), want, "case {}", i);
                    drop(s);
                    assert_eq!(all(&open(&dir)), want, "case {} after reopen", i);
                }
                None => {
                    assert!(res.is_err(), "case {} should fail", i);
                    assert_eq!(all(&s), vec![entry(3, 3), entry(4, 4), entry(5, 5)], "case {} unchanged", i);
                }
            }
        }
    }

    #[test]
    fn hard_and_conf_state_persist() {
        let dir = TempDir::new().unwrap();
        let s = fixture(&dir);
        let mut hs = HardState::default();
        hs.term = 5;
        hs.vote = 2;
        hs.commit = 4;
        s.set_hardstate(hs.clone()).unwrap();
        let mut cs = ConfState::default();
        cs.voters = vec![1, 2, 3, 4];
        s.set_conf_state(cs.clone()).unwrap();
        s.set_commit(5).unwrap();
        drop(s);
        let s = open(&dir);
        let st = s.initial_state().unwrap();
        hs.commit = 5;
        assert_eq!(st.hard_state, hs);
        assert_eq!(st.conf_state, cs);
        assert_eq!(all(&s), vec![entry(3, 3), entry(4, 4), entry(5, 5)]);
    }

    #[test]
    fn apply_snapshot_replaces_the_log_and_rejects_stale_ones() {
        let dir = TempDir::new().unwrap();
        let s = fixture(&dir);
        let mut snap = snapshot(10, 7, vec![1, 2, 3]);
        snap.data = b"catalog@10".to_vec().into();
        s.apply_snapshot(snap).unwrap().unwrap();
        assert_eq!((s.first_index().unwrap(), s.last_index().unwrap()), (11, 10));
        assert_eq!(s.term(10), Ok(7));
        assert_eq!(s.initial_state().unwrap().hard_state.commit, 10);
        assert!(segment_files(&dir).is_empty());
        assert_eq!(
            s.apply_snapshot(snapshot(9, 6, vec![1])).unwrap(),
            Err(RaftError::Store(StorageError::SnapshotOutOfDate))
        );
        s.append(&[entry(11, 7)]).unwrap();
        drop(s);
        let s = open(&dir);
        assert_eq!(s.snapshot_index_term(), (10, 7));
        assert_eq!(s.snapshot_data().unwrap(), b"catalog@10".to_vec());
        assert_eq!(s.initial_state().unwrap().conf_state.voters, vec![1, 2, 3]);
        assert_eq!(all(&s), vec![entry(11, 7)]);
    }

    #[test]
    fn snapshot_is_requested_until_one_is_created() {
        let dir = TempDir::new().unwrap();
        let s = open(&dir);
        s.append(&[entry(1, 1), entry(2, 1), entry(3, 2)]).unwrap();
        assert_eq!(s.snapshot(1, 2), Err(RaftError::Store(StorageError::SnapshotTemporarilyUnavailable)));
        assert!(s.snapshot_requested());

        // Not beyond the commit index.
        assert!(s.create_snapshot(2, ConfState::default(), b"x".to_vec()).is_err());
        s.set_commit(3).unwrap();
        let mut cs = ConfState::default();
        cs.voters = vec![1, 2, 3];
        s.create_snapshot(2, cs.clone(), b"state@2".to_vec()).unwrap();
        assert!(!s.snapshot_requested());
        let got = s.snapshot(2, 2).unwrap();
        assert_eq!((got.get_metadata().index, got.get_metadata().term), (2, 1));
        assert_eq!(got.get_metadata().get_conf_state(), &cs);
        assert_eq!(got.data.to_vec(), b"state@2".to_vec());
        // A follower needing more than the stored snapshot asks again.
        assert!(s.snapshot(3, 2).is_err());
        assert!(s.snapshot_requested());
        assert_eq!(all(&s), vec![entry(3, 2)]);
    }

    /// Small segments: every append rotates. Truncation across segment
    /// boundaries and compaction must leave disk and memory identical.
    #[test]
    fn segments_rotate_truncate_and_compact() {
        let dir = TempDir::new().unwrap();
        let s = RaftLogStore::open(dir.path(), opts(1)).unwrap();
        for i in 1..=20 {
            s.append(&[entry(i, 1)]).unwrap();
        }
        assert_eq!(segment_files(&dir).len(), 20);

        // A new leader overwrites from 8: segments 9..=20 go, 8 is rewritten.
        s.append(&[entry(8, 2), entry(9, 2)]).unwrap();
        let want: Vec<Entry> = (1..=7).map(|i| entry(i, 1)).chain([entry(8, 2), entry(9, 2)]).collect();
        assert_eq!(all(&s), want);
        drop(s);
        let s = RaftLogStore::open(dir.path(), opts(1)).unwrap();
        assert_eq!(all(&s), want);
        assert_eq!(segment_files(&dir).len(), 8);

        s.set_commit(9).unwrap();
        s.create_snapshot(5, ConfState::default(), b"s5".to_vec()).unwrap();
        assert_eq!(s.first_index().unwrap(), 6);
        // Segments holding only entries 1..=5 are gone.
        assert_eq!(segment_files(&dir).len(), 3);
        drop(s);
        let s = RaftLogStore::open(dir.path(), opts(1)).unwrap();
        assert_eq!(all(&s), want[5..].to_vec());
        assert_eq!(s.term(5), Ok(1));
        assert_eq!(s.term(4), Err(RaftError::Store(StorageError::Compacted)));
    }

    #[test]
    fn torn_tail_is_cut_and_the_log_keeps_working() {
        let dir = TempDir::new().unwrap();
        let s = fixture(&dir);
        drop(s);
        let seg = segment_files(&dir).pop().unwrap();
        let good_len = fs::metadata(&seg).unwrap().len();
        // Half a record: a header promising more bytes than were written.
        let mut f = OpenOptions::new().append(true).open(&seg).unwrap();
        f.write_all(&100u32.to_le_bytes()).unwrap();
        f.write_all(&[0xAB; 6]).unwrap();
        drop(f);

        let s = open(&dir);
        assert_eq!(all(&s), vec![entry(3, 3), entry(4, 4), entry(5, 5)]);
        assert_eq!(fs::metadata(&seg).unwrap().len(), good_len);
        s.append(&[entry(6, 5)]).unwrap();
        drop(s);
        assert_eq!(all(&open(&dir)).len(), 4);
    }

    #[test]
    fn corrupt_record_before_the_tail_fails_the_open() {
        let dir = TempDir::new().unwrap();
        let s = RaftLogStore::open(dir.path(), opts(1)).unwrap();
        for i in 1..=3 {
            s.append(&[entry(i, 1)]).unwrap();
        }
        drop(s);
        let first = segment_files(&dir).remove(0);
        let mut bytes = fs::read(&first).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        fs::write(&first, bytes).unwrap();
        let err = RaftLogStore::open(dir.path(), opts(1)).err().expect("must refuse");
        assert!(err.to_string().contains("damaged"), "{}", err);
    }

    #[test]
    fn corrupt_state_file_fails_the_open() {
        let dir = TempDir::new().unwrap();
        let s = fixture(&dir);
        let mut hs = HardState::default();
        hs.term = 9;
        s.set_hardstate(hs).unwrap();
        drop(s);
        let path = dir.path().join(STATE_FILE);
        let mut bytes = fs::read(&path).unwrap();
        bytes[5] ^= 0x01;
        fs::write(&path, bytes).unwrap();
        assert!(RaftLogStore::open(dir.path(), opts(1 << 20)).is_err());
    }

    /// A gap left in the files (a lost segment) is never papered over.
    #[test]
    fn missing_middle_segment_fails_the_open() {
        let dir = TempDir::new().unwrap();
        let s = RaftLogStore::open(dir.path(), opts(1)).unwrap();
        for i in 1..=4 {
            s.append(&[entry(i, 1)]).unwrap();
        }
        drop(s);
        fs::remove_file(&segment_files(&dir)[1]).unwrap();
        let err = RaftLogStore::open(dir.path(), opts(1)).err().expect("must refuse");
        assert!(err.to_string().contains("not contiguous"), "{}", err);
    }

    /// Truncation removes later segments before cutting the one it lands in,
    /// so a crash in between leaves a shorter prefix, which is readable.
    #[test]
    fn crash_mid_truncation_leaves_a_valid_prefix() {
        let dir = TempDir::new().unwrap();
        let s = RaftLogStore::open(dir.path(), opts(1)).unwrap();
        for i in 1..=5 {
            s.append(&[entry(i, 1)]).unwrap();
        }
        drop(s);
        // As if truncating from 3: segments 4 and 5 removed, 3 not yet cut.
        let files = segment_files(&dir);
        fs::remove_file(&files[4]).unwrap();
        fs::remove_file(&files[3]).unwrap();
        let s = RaftLogStore::open(dir.path(), opts(1)).unwrap();
        assert_eq!(all(&s), (1..=3).map(|i| entry(i, 1)).collect::<Vec<_>>());
        s.append(&[entry(3, 2), entry(4, 2)]).unwrap();
        drop(s);
        let s = RaftLogStore::open(dir.path(), opts(1)).unwrap();
        assert_eq!(all(&s), vec![entry(1, 1), entry(2, 1), entry(3, 2), entry(4, 2)]);
    }

    /// Plant a durable snapshot file over an existing log, as a crash between
    /// installing a leader's snapshot and deleting the old segments leaves it.
    fn plant_snapshot(dir: &TempDir, index: u64, term: u64) {
        let other = TempDir::new().unwrap();
        let s = open(&other);
        s.apply_snapshot(snapshot(index, term, vec![1, 2, 3])).unwrap().unwrap();
        drop(s);
        fs::copy(other.path().join(SNAPSHOT_FILE), dir.path().join(SNAPSHOT_FILE)).unwrap();
    }

    fn log_1_to_10(dir: &TempDir, term_of: impl Fn(u64) -> u64) {
        let s = RaftLogStore::open(dir.path(), opts(1)).unwrap();
        for i in 1..=10 {
            s.append(&[entry(i, term_of(i))]).unwrap();
        }
    }

    #[test]
    fn crash_after_snapshot_install_beyond_the_old_log() {
        let dir = TempDir::new().unwrap();
        log_1_to_10(&dir, |_| 1);
        plant_snapshot(&dir, 20, 4);
        let s = RaftLogStore::open(dir.path(), opts(1)).unwrap();
        assert_eq!((s.first_index().unwrap(), s.last_index().unwrap()), (21, 20));
        // Old segments wholly covered are cleaned up (the last is kept until replaced).
        assert_eq!(segment_files(&dir).len(), 1);
        // The next append must not land in the leftover segment ending at 10.
        s.append(&[entry(21, 4)]).unwrap();
        drop(s);
        let s = RaftLogStore::open(dir.path(), opts(1)).unwrap();
        assert_eq!(all(&s), vec![entry(21, 4)]);
    }

    #[test]
    fn crash_after_snapshot_install_keeps_a_matching_tail() {
        let dir = TempDir::new().unwrap();
        log_1_to_10(&dir, |_| 1);
        plant_snapshot(&dir, 5, 1);
        let s = RaftLogStore::open(dir.path(), opts(1)).unwrap();
        assert_eq!(all(&s), (6..=10).map(|i| entry(i, 1)).collect::<Vec<_>>());
    }

    #[test]
    fn crash_after_snapshot_install_drops_a_replaced_tail() {
        let dir = TempDir::new().unwrap();
        log_1_to_10(&dir, |_| 1);
        // The leader's history has term 3 at index 5: our 6..=10 are not its.
        plant_snapshot(&dir, 5, 3);
        let s = RaftLogStore::open(dir.path(), opts(1)).unwrap();
        assert_eq!((s.first_index().unwrap(), s.last_index().unwrap()), (6, 5));
        s.append(&[entry(6, 3)]).unwrap();
        drop(s);
        let s = RaftLogStore::open(dir.path(), opts(1)).unwrap();
        assert_eq!(all(&s), vec![entry(6, 3)]);
    }

    /// Drives a real single-voter RawNode through the store: propose, persist,
    /// commit, restart, and find everything where it was.
    #[test]
    fn raw_node_survives_restart() {
        use raft::{Config, RawNode};
        let dir = TempDir::new().unwrap();
        let logger = slog::Logger::root(slog::Discard, slog::o!());
        let config = Config { id: 1, election_tick: 10, heartbeat_tick: 3, ..Default::default() };

        let drive = |node: &mut RawNode<RaftLogStore>, store: &RaftLogStore, applied: &mut Vec<Vec<u8>>| {
            for _ in 0..50 {
                if !node.has_ready() {
                    node.tick();
                    continue;
                }
                let mut ready = node.ready();
                store.append(ready.entries()).unwrap();
                if let Some(hs) = ready.hs() {
                    store.set_hardstate(hs.clone()).unwrap();
                }
                for e in ready.take_committed_entries() {
                    if !e.data.is_empty() {
                        applied.push(e.data.to_vec());
                    }
                }
                let mut light = node.advance(ready);
                if let Some(c) = light.commit_index() {
                    store.set_commit(c).unwrap();
                }
                for e in light.take_committed_entries() {
                    if !e.data.is_empty() {
                        applied.push(e.data.to_vec());
                    }
                }
                node.advance_apply();
            }
        };

        {
            let store = open(&dir);
            let mut cs = ConfState::default();
            cs.voters = vec![1];
            store.set_conf_state(cs).unwrap();
            let mut node = RawNode::new(&config, store.clone(), &logger).unwrap();
            node.campaign().unwrap();
            let mut applied = Vec::new();
            drive(&mut node, &store, &mut applied);
            for i in 0..5 {
                node.propose(vec![], format!("p{}", i).into_bytes()).unwrap();
            }
            drive(&mut node, &store, &mut applied);
            assert_eq!(applied, (0..5).map(|i| format!("p{}", i).into_bytes()).collect::<Vec<_>>());
        }

        let store = open(&dir);
        let state = store.initial_state().unwrap();
        assert!(state.hard_state.commit >= 6, "commit {}", state.hard_state.commit);
        let data: Vec<Vec<u8>> = all(&store).into_iter().filter(|e| !e.data.is_empty()).map(|e| e.data.to_vec()).collect();
        assert_eq!(data, (0..5).map(|i| format!("p{}", i).into_bytes()).collect::<Vec<_>>());
        // Restarted node resumes on the same log and keeps committing.
        let cfg = Config { applied: state.hard_state.commit, ..config.clone() };
        let mut node = RawNode::new(&cfg, store.clone(), &logger).unwrap();
        node.campaign().unwrap();
        let mut applied = Vec::new();
        drive(&mut node, &store, &mut applied);
        node.propose(vec![], b"after-restart".to_vec()).unwrap();
        drive(&mut node, &store, &mut applied);
        assert_eq!(applied, vec![b"after-restart".to_vec()]);
    }
}
