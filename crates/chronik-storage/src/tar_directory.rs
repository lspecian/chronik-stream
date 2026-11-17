//! A read-only Tantivy directory over an uncompressed tar archive.
//!
//! An archived index segment used to be a `.tar.gz`: to search it, or to serve
//! a fetch from it, the archive was decompressed and unpacked into a directory
//! first — every time it was not already in a cache, and the cache could not
//! hold a node's 6,000–13,000 archives. Archives are now written as plain
//! `.tar` and opened in place: each file inside the tar is a zero-copy slice of
//! the archive's bytes (a memory map of the local file, or the downloaded
//! object), and Tantivy reads it as it would a file on disk. Tantivy's own
//! files are already compressed, so dropping gzip costs 27–47% more disk on
//! real archives, and opening an archive costs a scan of its tar headers.
//!
//! Reading an index never writes, but Tantivy's directory contract includes
//! writes (meta files, locks): they go to an in-memory overlay and vanish with
//! the directory.

use std::collections::HashMap;
use std::io;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tantivy::directory::error::{DeleteError, LockError, OpenReadError, OpenWriteError};
use tantivy::directory::{
    Directory, DirectoryLock, FileHandle, Lock, OwnedBytes, WatchCallback, WatchHandle, WritePtr,
};

/// First two bytes of a gzip stream: an archive that starts with them is a
/// `.tar.gz` written before plain tars.
pub fn is_gzip(bytes: &[u8]) -> bool {
    bytes.len() >= 2 && bytes[0] == 0x1f && bytes[1] == 0x8b
}

/// The files of a tar, as slices of its bytes.
#[derive(Clone)]
pub struct TarDirectory {
    files: Arc<HashMap<PathBuf, OwnedBytes>>,
    overlay: Arc<Mutex<HashMap<PathBuf, Vec<u8>>>>,
}

impl std::fmt::Debug for TarDirectory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TarDirectory").field("files", &self.files.len()).finish()
    }
}

/// A memory map handed to `OwnedBytes`: the mapping does not move when the
/// value does, which is what `StableDeref` promises.
struct MappedFile(memmap2::Mmap);
impl Deref for MappedFile {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}
unsafe impl stable_deref_trait::StableDeref for MappedFile {}

/// `bytes::Bytes` handed to `OwnedBytes`: its buffer does not move either.
struct HeapBytes(bytes::Bytes);
impl Deref for HeapBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}
unsafe impl stable_deref_trait::StableDeref for HeapBytes {}

/// The whole of a local file, memory-mapped.
pub fn map_file(path: &Path) -> io::Result<OwnedBytes> {
    let file = std::fs::File::open(path)?;
    // SAFETY: archives are immutable once written; nothing truncates a file a
    // reader has mapped (a deleted file stays readable while mapped).
    let map = unsafe { memmap2::Mmap::map(&file)? };
    Ok(OwnedBytes::new(MappedFile(map)))
}

/// Downloaded bytes, without copying.
pub fn owned(bytes: bytes::Bytes) -> OwnedBytes {
    OwnedBytes::new(HeapBytes(bytes))
}

impl TarDirectory {
    /// The files under `prefix/` in the tar `bytes`, named without the prefix.
    pub fn open(bytes: OwnedBytes, prefix: &str) -> io::Result<Self> {
        let mut files = HashMap::new();
        for (name, range) in entries(bytes.as_slice())? {
            if let Some(rest) = name.strip_prefix(prefix).and_then(|r| r.strip_prefix('/')) {
                if !rest.is_empty() {
                    files.insert(PathBuf::from(rest), bytes.slice(range));
                }
            }
        }
        Ok(Self { files: Arc::new(files), overlay: Arc::new(Mutex::new(HashMap::new())) })
    }

    /// One top-level file of the tar `bytes` (e.g. `metadata.json`).
    pub fn read_entry(bytes: &OwnedBytes, name: &str) -> io::Result<Option<OwnedBytes>> {
        Ok(entries(bytes.as_slice())?
            .into_iter()
            .find(|(n, _)| n == name)
            .map(|(_, range)| bytes.slice(range)))
    }
}

/// `(name, data range)` of every regular file in a tar. Understands ustar
/// names (with prefix), GNU long names and pax `path` records — enough for
/// any tar the `tar` crate writes.
fn entries(tar: &[u8]) -> io::Result<Vec<(String, std::ops::Range<usize>)>> {
    let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, format!("tar: {what}"));
    let mut out = Vec::new();
    let mut at = 0usize;
    let mut long_name: Option<String> = None;
    while at + 512 <= tar.len() {
        let header = &tar[at..at + 512];
        if header.iter().all(|&b| b == 0) {
            break; // end of archive
        }
        let size = parse_size(&header[124..136]).ok_or_else(|| bad("size"))?;
        let data = at + 512;
        let end = data.checked_add(size).ok_or_else(|| bad("size overflow"))?;
        if end > tar.len() {
            return Err(bad("truncated entry"));
        }
        let typeflag = header[156];
        match typeflag {
            b'L' => long_name = Some(c_str(&tar[data..end])),
            b'x' => {
                if let Some(path) = pax_path(&tar[data..end]) {
                    long_name = Some(path);
                }
            }
            b'0' | 0 => {
                let name = long_name.take().unwrap_or_else(|| {
                    let name = c_str(&header[0..100]);
                    let prefix = if &header[257..262] == b"ustar" { c_str(&header[345..500]) } else { String::new() };
                    if prefix.is_empty() { name } else { format!("{prefix}/{name}") }
                });
                out.push((name.trim_start_matches("./").to_string(), data..end));
            }
            _ => long_name = None,
        }
        at = data + size.div_ceil(512) * 512;
    }
    Ok(out)
}

fn c_str(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// An octal size, or GNU base-256 for sizes over 8 GB.
fn parse_size(field: &[u8]) -> Option<usize> {
    if field[0] & 0x80 != 0 {
        let mut n: u64 = 0;
        for &b in &field[1..] {
            n = n.checked_mul(256)?.checked_add(b as u64)?;
        }
        return usize::try_from(n).ok();
    }
    let s = c_str(field);
    let s = s.trim();
    if s.is_empty() {
        return Some(0);
    }
    usize::from_str_radix(s, 8).ok()
}

/// The `path` of a pax extended header (`"<len> path=<value>\n"` records).
fn pax_path(data: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(data);
    text.lines()
        .filter_map(|line| line.split_once(' ').map(|(_, kv)| kv))
        .find_map(|kv| kv.strip_prefix("path=").map(str::to_string))
}

impl Directory for TarDirectory {
    fn get_file_handle(&self, path: &Path) -> Result<Arc<dyn FileHandle>, OpenReadError> {
        if let Some(bytes) = self.files.get(path) {
            return Ok(Arc::new(bytes.clone()));
        }
        if let Some(data) = self.overlay.lock().unwrap().get(path) {
            return Ok(Arc::new(OwnedBytes::new(data.clone())));
        }
        Err(OpenReadError::FileDoesNotExist(path.to_path_buf()))
    }

    fn delete(&self, path: &Path) -> Result<(), DeleteError> {
        self.overlay.lock().unwrap().remove(path);
        Ok(())
    }

    fn exists(&self, path: &Path) -> Result<bool, OpenReadError> {
        Ok(self.files.contains_key(path) || self.overlay.lock().unwrap().contains_key(path))
    }

    fn open_write(&self, path: &Path) -> Result<WritePtr, OpenWriteError> {
        Err(OpenWriteError::IoError {
            io_error: Arc::new(io::Error::new(io::ErrorKind::Unsupported, "an archive is read-only")),
            filepath: path.to_path_buf(),
        })
    }

    fn atomic_read(&self, path: &Path) -> Result<Vec<u8>, OpenReadError> {
        if let Some(data) = self.overlay.lock().unwrap().get(path) {
            return Ok(data.clone());
        }
        match self.files.get(path) {
            Some(bytes) => Ok(bytes.as_slice().to_vec()),
            None => Err(OpenReadError::FileDoesNotExist(path.to_path_buf())),
        }
    }

    fn atomic_write(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        self.overlay.lock().unwrap().insert(path.to_path_buf(), data.to_vec());
        Ok(())
    }

    /// Nothing writes an archive, so there is nothing to lock out.
    fn acquire_lock(&self, _lock: &Lock) -> Result<DirectoryLock, LockError> {
        Ok(DirectoryLock::from(Box::new(())))
    }

    fn sync_directory(&self) -> io::Result<()> {
        Ok(())
    }

    fn watch(&self, _watch_callback: WatchCallback) -> tantivy::Result<WatchHandle> {
        Ok(WatchHandle::empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tar_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (name, data) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, name, *data).unwrap();
        }
        b.into_inner().unwrap()
    }

    #[test]
    fn lists_files_under_the_prefix_including_long_names() {
        let long = format!("index/{}.store", "x".repeat(150));
        let tar = tar_of(&[("metadata.json", b"{}"), ("index/meta.json", b"m"), (&long, b"payload")]);
        let bytes = OwnedBytes::new(tar);
        let dir = TarDirectory::open(bytes.clone(), "index").unwrap();
        assert_eq!(dir.atomic_read(Path::new("meta.json")).unwrap(), b"m");
        let handle = dir.get_file_handle(Path::new(&long["index/".len()..])).unwrap();
        assert_eq!(handle.read_bytes(0..7).unwrap().as_slice(), b"payload");
        assert!(!dir.exists(Path::new("metadata.json")).unwrap(), "outside the prefix");
        assert_eq!(TarDirectory::read_entry(&bytes, "metadata.json").unwrap().unwrap().as_slice(), b"{}");
    }

    #[test]
    fn rejects_a_truncated_archive() {
        let mut tar = tar_of(&[("index/a", &[7u8; 2000])]);
        tar.truncate(1200);
        assert!(TarDirectory::open(OwnedBytes::new(tar), "index").is_err());
    }

    #[test]
    fn a_real_index_opens_from_a_tar_with_no_unpacking() {
        use tantivy::schema::{Schema, STORED, TEXT};
        let tmp = tempfile::tempdir().unwrap();
        let mut sb = Schema::builder();
        let body = sb.add_text_field("body", TEXT | STORED);
        let index = tantivy::Index::create_in_dir(tmp.path(), sb.build()).unwrap();
        let mut w: tantivy::IndexWriter = index.writer(15_000_000).unwrap();
        w.add_document(tantivy::doc!(body => "the quick brown fox")).unwrap();
        w.add_document(tantivy::doc!(body => "lazy dogs sleep")).unwrap();
        w.commit().unwrap();
        w.wait_merging_threads().unwrap();

        let mut b = tar::Builder::new(Vec::new());
        b.append_dir_all("index", tmp.path()).unwrap();
        let path = tmp.path().with_extension("tar");
        std::fs::write(&path, b.into_inner().unwrap()).unwrap();

        let dir = TarDirectory::open(map_file(&path).unwrap(), "index").unwrap();
        let opened = tantivy::Index::open(dir).unwrap();
        let reader = opened.reader_builder().reload_policy(tantivy::ReloadPolicy::Manual).try_into().unwrap();
        let searcher: tantivy::Searcher = reader.searcher();
        let q = tantivy::query::QueryParser::for_index(&opened, vec![body]).parse_query("fox").unwrap();
        assert_eq!(searcher.search(&q, &tantivy::collector::Count).unwrap(), 1);
        std::fs::remove_file(&path).unwrap();
    }
}
