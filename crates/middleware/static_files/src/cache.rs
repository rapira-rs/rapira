//! A file cache for one worker process.
//!
//! `ServeDir` sends every filesystem operation through the `Backend` trait. This module
//! implements that trait. `ServeDir` builds the ETag, evaluates the preconditions, applies
//! Range, and sets the headers. This module supplies only the bytes and the metadata.
//! https://docs.rs/tower-http/0.7.1/tower_http/services/fs/trait.Backend.html
//!
//! An entry is a snapshot of one file at one instant. It holds the bytes and the metadata
//! together. The cache therefore holds no entry for a miss, for a directory, or for a file
//! above the size cap. `ServeDir` reads the validators of a `HEAD` and of a `GET` from the
//! same entry, so the two methods always agree.
//!
//! An entry is fresh for one second. Each forked worker has its own cache. Only a change to a
//! cached file can give stale data.
//!
//! The cache treats a file as changed when the mtime or the length is different. The ETag
//! encodes the same two values.
//!
//! A change of permissions does not stop the cache from serving a file. `stat` needs search
//! permission on the parent directory, not read permission on the file. To stop the cache
//! from serving a file, delete the file or replace it.
//! https://pubs.opengroup.org/onlinepubs/9799919799/functions/stat.html
//!
//! The backend runs `stat` and `open` on a runtime thread. A slow filesystem therefore
//! blocks the runtime. The root must be on local storage.

use std::collections::HashMap;
use std::future::{Ready, ready};
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncSeek, ReadBuf};
use tower_http::services::fs::{Backend, File, Metadata};

/// The cache does not store a larger file. `ServeDir` streams it from disk.
const MAX_FILE: u64 = 256 * 1024;
/// The memory limit for one worker process. Forked workers do not share the cache.
const MAX_TOTAL: usize = 16 * 1024 * 1024;
/// An entry stays fresh for this time. A stat then revalidates it, so a change to a cached
/// file reaches the client after one second.
const TTL: Duration = Duration::from_secs(1);
/// The cache adds this value to the size of each entry. It covers the entry and the map slot.
/// The bodies alone understate the memory of a root that holds many small files.
const ENTRY_OVERHEAD: usize = 256;

#[derive(Clone, Copy)]
pub(crate) struct CachedMeta {
    is_dir: bool,
    modified: Option<SystemTime>,
    len: u64,
}

impl CachedMeta {
    fn new(meta: &std::fs::Metadata) -> Self {
        Self {
            is_dir: meta.is_dir(),
            modified: meta.modified().ok(),
            len: meta.len(),
        }
    }

    /// `ServeDir` builds the ETag from these two values, and `Last-Modified` from the mtime.
    /// Equal values therefore give the client the same validators. The comparison holds no
    /// device number and no inode number. The cache does not detect a replacement that keeps
    /// the mtime and the length.
    /// TODO!: probably here we need to add device and inode numbers to detect better.
    fn same_file(&self, other: &Self) -> bool {
        self.modified == other.modified && self.len == other.len
    }
}

impl Metadata for CachedMeta {
    fn is_dir(&self) -> bool {
        self.is_dir
    }

    /// `ServeDir` calls `.ok()` on this result. An absent mtime gives no ETag and no
    /// `Last-Modified`. This result is `Err` only when `std::fs::Metadata::modified` is also
    /// `Err` for the same file.
    fn modified(&self) -> io::Result<SystemTime> {
        self.modified
            .ok_or_else(|| io::Error::other("modification time is not available"))
    }

    fn len(&self) -> u64 {
        self.len
    }
}

/// Each variant keeps its own metadata, so `File::metadata` does no syscall. The bytes and
/// the metadata of one value always come from the same open file.
pub(crate) enum CachedFile {
    Memory {
        cursor: Cursor<Bytes>,
        meta: CachedMeta,
    },
    Disk {
        file: tokio::fs::File,
        meta: CachedMeta,
    },
}

impl AsyncRead for CachedFile {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            CachedFile::Memory { cursor, .. } => Pin::new(cursor).poll_read(cx, buf),
            CachedFile::Disk { file, .. } => Pin::new(file).poll_read(cx, buf),
        }
    }
}

impl AsyncSeek for CachedFile {
    fn start_seek(self: Pin<&mut Self>, position: SeekFrom) -> io::Result<()> {
        match self.get_mut() {
            CachedFile::Memory { cursor, .. } => Pin::new(cursor).start_seek(position),
            CachedFile::Disk { file, .. } => Pin::new(file).start_seek(position),
        }
    }

    fn poll_complete(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        match self.get_mut() {
            CachedFile::Memory { cursor, .. } => Pin::new(cursor).poll_complete(cx),
            CachedFile::Disk { file, .. } => Pin::new(file).poll_complete(cx),
        }
    }
}

impl File for CachedFile {
    type Metadata = CachedMeta;
    type MetadataFuture<'a> = Ready<io::Result<CachedMeta>>;

    fn metadata(&self) -> Self::MetadataFuture<'_> {
        let meta = match self {
            CachedFile::Memory { meta, .. } | CachedFile::Disk { meta, .. } => *meta,
        };
        ready(Ok(meta))
    }
}

struct Entry {
    body: Bytes,
    meta: CachedMeta,
    checked: Instant,
}

#[derive(Default)]
struct Store {
    map: HashMap<PathBuf, Entry>,
    bytes: usize,
    #[cfg(test)]
    reads: usize,
}

impl Store {
    fn fresh(&self, path: &Path, now: Instant) -> Option<&Entry> {
        self.map
            .get(path)
            .filter(|e| now.duration_since(e.checked) < TTL)
    }

    fn take(&mut self, path: &Path) {
        if let Some(entry) = self.map.remove(path) {
            self.bytes -= Self::footprint(path, entry.body.len());
        }
    }

    fn footprint(path: &Path, body: usize) -> usize {
        body + path.as_os_str().len() + ENTRY_OVERHEAD
    }

    /// A replacement first releases the size of the old entry. A reload of the same size
    /// therefore always fits.
    fn fits(&self, path: &Path, body: u64) -> bool {
        let reclaimed = self
            .map
            .get(path)
            .map_or(0, |e| Self::footprint(path, e.body.len()));
        self.bytes - reclaimed + Self::footprint(path, body as usize) <= MAX_TOTAL
    }

    /// Removes every entry that no stat or fill confirmed in the last second.
    fn drop_expired(&mut self, now: Instant) {
        let mut freed = 0;
        self.map.retain(|path, entry| {
            if now.duration_since(entry.checked) < TTL {
                return true;
            }
            freed += Self::footprint(path, entry.body.len());
            false
        });
        self.bytes -= freed;
    }

    /// A full cache still serves its entries. It removes the stale entries before it refuses
    /// a new file. A cache that holds only fresh entries refuses every new file until the load
    /// decreases.
    fn make_room(&mut self, path: &Path, body: u64, now: Instant) -> bool {
        if self.fits(path, body) {
            return true;
        }
        self.drop_expired(now);
        self.fits(path, body)
    }

    /// Refreshes an entry that still matches the file. Removes an entry that does not match.
    /// A path with no entry stays out of the map until `fill` has a body for it.
    fn revalidate(&mut self, path: &Path, meta: &CachedMeta, now: Instant) {
        match self.map.get_mut(path) {
            Some(entry) if entry.meta.same_file(meta) => entry.checked = now,
            Some(_) => self.take(path),
            None => {}
        }
    }

    /// The capacity check runs before the removal, so a refused entry leaves the resident one
    /// in place.
    fn put(&mut self, path: PathBuf, body: Bytes, meta: CachedMeta, now: Instant) {
        if !self.make_room(&path, body.len() as u64, now) {
            return;
        }
        self.take(&path);
        self.bytes += Self::footprint(&path, body.len());
        self.map.insert(
            path,
            Entry {
                body,
                meta,
                checked: now,
            },
        );
    }
}

#[derive(Clone, Default)]
pub(crate) struct CachingBackend {
    store: Arc<Mutex<Store>>,
}

impl CachingBackend {
    /// The critical section contains only map operations and integer arithmetic. A poisoned
    /// lock therefore cannot mean that the store is in a bad state.
    fn lock(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn stat(&self, path: &Path) -> io::Result<CachedMeta> {
        let now = Instant::now();
        if let Some(entry) = self.lock().fresh(path, now) {
            return Ok(entry.meta);
        }
        let meta = match std::fs::metadata(path) {
            Ok(meta) => CachedMeta::new(&meta),
            Err(err) => {
                self.lock().take(path);
                return Err(err);
            }
        };
        self.lock().revalidate(path, &meta, now);
        Ok(meta)
    }

    fn hit(&self, path: &Path) -> Option<CachedFile> {
        let store = self.lock();
        let entry = store.fresh(path, Instant::now())?;
        Some(CachedFile::Memory {
            cursor: Cursor::new(entry.body.clone()),
            meta: entry.meta,
        })
    }

    fn stream(file: std::fs::File, meta: CachedMeta) -> CachedFile {
        CachedFile::Disk {
            file: tokio::fs::File::from_std(file),
            meta,
        }
    }

    async fn fill(self, path: PathBuf) -> io::Result<CachedFile> {
        let file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(err) => {
                self.lock().take(&path);
                return Err(err);
            }
        };
        let meta = CachedMeta::new(&file.metadata()?);

        // The path became a directory between the stat and the open. `open` on a directory
        // succeeds on Unix, and the read then fails. The backend must therefore report an
        // error before `ServeDir` writes a head.
        // https://man7.org/linux/man-pages/man2/open.2.html
        if meta.is_dir {
            return Err(io::Error::from(io::ErrorKind::IsADirectory));
        }

        // `ServeDir` streams a file that the cache cannot store.
        if meta.len > MAX_FILE || !self.lock().make_room(&path, meta.len, Instant::now()) {
            return Ok(Self::stream(file, meta));
        }

        #[cfg(test)]
        {
            self.lock().reads += 1;
        }

        let (mut file, buf, reread) = tokio::task::spawn_blocking(move || {
            let mut buf = Vec::with_capacity(meta.len as usize);
            // The size check above used the stat from before the read. A bounded read stops
            // a file that grows during the read from filling the heap. The length check below
            // then discards that file.
            (&file).take(meta.len + 1).read_to_end(&mut buf)?;
            let reread = file.metadata()?;
            io::Result::Ok((file, buf, CachedMeta::new(&reread)))
        })
        .await
        .map_err(io::Error::other)??;

        // The second stat detects a write during the read. The cache must not keep the new
        // metadata with the old bytes. A revalidation would compare the new metadata with
        // itself and find no change. The cache would serve the old bytes until the next write.
        if !reread.same_file(&meta) || buf.len() as u64 != meta.len {
            self.lock().take(&path);
            file.seek(SeekFrom::Start(0))?;
            return Ok(Self::stream(file, reread));
        }

        let body = Bytes::from(buf);
        self.lock().put(path, body.clone(), meta, Instant::now());
        Ok(CachedFile::Memory {
            cursor: Cursor::new(body),
            meta,
        })
    }
}

impl Backend for CachingBackend {
    type File = CachedFile;
    type Metadata = CachedMeta;
    type OpenFuture = Pin<Box<dyn Future<Output = io::Result<CachedFile>> + Send>>;
    type MetadataFuture = Ready<io::Result<CachedMeta>>;

    /// A stat is short. The blocking pool would cost more than the syscall, so a miss does
    /// the stat inline.
    fn metadata(&self, path: PathBuf) -> Self::MetadataFuture {
        ready(self.stat(&path))
    }

    fn open(&self, path: PathBuf) -> Self::OpenFuture {
        if let Some(file) = self.hit(&path) {
            return Box::pin(ready(Ok(file)));
        }
        Box::pin(self.clone().fill(path))
    }
}

#[cfg(test)]
impl CachingBackend {
    pub(crate) fn accounted(&self) -> usize {
        self.lock().bytes
    }

    /// Adds the size of every entry in the map. The result must equal the running total.
    pub(crate) fn recomputed(&self) -> usize {
        self.lock()
            .map
            .iter()
            .map(|(path, entry)| Store::footprint(path, entry.body.len()))
            .sum()
    }

    pub(crate) fn entries(&self) -> usize {
        self.lock().map.len()
    }

    /// The number of files that the cache read into memory. It does not count a refused file.
    pub(crate) fn reads(&self) -> usize {
        self.lock().reads
    }
}
