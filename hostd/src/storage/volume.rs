//! Chunk-backed volume layout and IO for the per-VM ublk block storage.
//!
//! A volume is a directory on the storage root (an S3 Files NFS mount in
//! production, any directory in tests):
//!
//! ```text
//! <dir>/meta.json                  {"version", "size_bytes", "chunk_size"}
//! <dir>/chunks/<hi>/<lo>/<idx>     one file per chunk, sharded two levels
//!                                  by chunk index; missing chunk = zeros
//! ```
//!
//! This module is deliberately free of any ublk dependency: `do_io` and
//! `flush` are plain file IO (unit-tested against a tmpdir), driven by the
//! ublk queue threads in `ublk_worker`.
//!
//! Semantics that matter (all validated by the ublk spike):
//!
//! - **Flush = per-dirty-chunk `fdatasync`.** On an NFS backing store that
//!   is one COMMIT per dirty chunk (~9 ms p50 on S3 Files); done in
//!   parallel over a small scoped thread pool. Only data covered by a
//!   completed flush is durable — the same contract a disk gives ext4.
//! - **Dirty fds are never evicted** from the open-fd cache: evicting a
//!   dirty fd makes `close(2)` synchronously write the chunk back, which
//!   (with an undersized cache) collapses random-write throughput.
//! - **Chunk fds are always opened read-write**, even for reads: a cached
//!   read-only fd breaks a later write to the same chunk with EBADF.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Default chunk size (1 MiB): the spike measured 256 KiB chunks losing
/// ~8x on random IO (more NFS CREATEs, more fd-cache churn) while 4 MiB
/// gains little over 1 MiB and quadruples per-chunk re-sync bytes.
pub(crate) const DEFAULT_CHUNK_KB: u32 = 1024;

/// Chunk sizes accepted by the create-VM API.
pub(crate) const ALLOWED_CHUNK_KB: [u32; 5] = [256, 512, 1024, 2048, 4096];

/// Open-fd cache capacity. Sized so a random workload over a few GiB of
/// 1 MiB chunks stays resident; dirty fds are pinned and can push the map
/// past this cap (bounded by the dirty-set size instead).
const FD_CACHE_CAP: usize = 4096;

/// Threads used by the parallel flush.
const FLUSH_THREADS: usize = 8;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct VolumeMeta {
    pub version: u32,
    pub size_bytes: u64,
    pub chunk_size: u64,
}

pub(crate) struct Volume {
    dir: PathBuf,
    meta: VolumeMeta,
    fds: Mutex<FdCache>,
    dirty: Mutex<HashSet<u64>>,
}

/// Generational LRU over open chunk fds: `order` may contain stale
/// `(idx, gen)` entries; only an entry whose generation still matches the
/// map is a live eviction candidate.
struct FdCache {
    map: HashMap<u64, (Arc<File>, u64)>,
    order: VecDeque<(u64, u64)>,
    next_gen: u64,
    cap: usize,
}

impl FdCache {
    fn new(cap: usize) -> Self {
        Self { map: HashMap::new(), order: VecDeque::new(), next_gen: 0, cap }
    }
}

impl Volume {
    /// Create a fresh volume directory with `meta.json`.
    pub(crate) fn create(dir: &Path, size_bytes: u64, chunk_size: u64) -> Result<Self> {
        if !chunk_size.is_power_of_two() || chunk_size < 4096 {
            return Err(Error::storage(format!(
                "chunk size {chunk_size} must be a power of two >= 4096"
            )));
        }
        if size_bytes == 0 || !size_bytes.is_multiple_of(512) {
            return Err(Error::storage(format!(
                "volume size {size_bytes} must be a positive multiple of 512"
            )));
        }
        fs::create_dir_all(dir.join("chunks"))?;
        let meta = VolumeMeta { version: 1, size_bytes, chunk_size };
        fs::write(dir.join("meta.json"), serde_json::to_string_pretty(&meta)?)?;
        Self::open(dir)
    }

    /// Open an existing volume directory.
    pub(crate) fn open(dir: &Path) -> Result<Self> {
        let meta: VolumeMeta = serde_json::from_str(
            &fs::read_to_string(dir.join("meta.json"))
                .map_err(|e| Error::storage(format!("read {}: {e}", dir.display())))?,
        )?;
        if meta.version != 1 {
            return Err(Error::storage(format!(
                "unsupported volume format version {}",
                meta.version
            )));
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            meta,
            fds: Mutex::new(FdCache::new(FD_CACHE_CAP)),
            dirty: Mutex::new(HashSet::new()),
        })
    }

    pub(crate) fn size_bytes(&self) -> u64 {
        self.meta.size_bytes
    }

    fn chunk_path(&self, idx: u64) -> PathBuf {
        self.dir
            .join("chunks")
            .join(format!("{:02x}", (idx >> 8) & 0xff))
            .join(format!("{:02x}", idx & 0xff))
            .join(idx.to_string())
    }

    /// Get a cached fd for `idx`, opening (and optionally creating) the
    /// chunk file. Returns `None` when the chunk does not exist and
    /// `create` is false. Always opens read-write (see module docs).
    fn chunk_fd(&self, idx: u64, create: bool) -> io::Result<Option<Arc<File>>> {
        let mut cache = self.fds.lock().map_err(|_| io::Error::other("lock poisoned"))?;
        if let Some((file, _)) = cache.map.get(&idx) {
            let file = file.clone();
            cache.next_gen += 1;
            let g = cache.next_gen;
            cache.map.insert(idx, (file.clone(), g));
            cache.order.push_back((idx, g));
            return Ok(Some(file));
        }

        let path = self.chunk_path(idx);
        let file = if create {
            fs::create_dir_all(path.parent().expect("chunk path has parent"))?;
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                // Never truncate: an existing chunk file holds data.
                .truncate(false)
                .open(&path)?
        } else {
            match OpenOptions::new().read(true).write(true).open(&path) {
                Ok(f) => f,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e),
            }
        };

        cache.next_gen += 1;
        let g = cache.next_gen;
        cache.map.insert(idx, (Arc::new(file), g));
        cache.order.push_back((idx, g));

        // Evict clean fds while over capacity. Dirty fds are rotated to
        // the back, never closed (see module docs). Bounded pops guarantee
        // termination; if everything is dirty the cache simply grows.
        let dirty = self.dirty.lock().map_err(|_| io::Error::other("lock poisoned"))?;
        let mut pops = 0;
        while cache.map.len() > cache.cap && pops < 2 * cache.cap {
            pops += 1;
            let Some((victim, vgen)) = cache.order.pop_front() else { break };
            let Some((_, cur_gen)) = cache.map.get(&victim) else { continue };
            if *cur_gen != vgen {
                continue; // stale order entry
            }
            if dirty.contains(&victim) {
                cache.order.push_back((victim, vgen));
                continue;
            }
            cache.map.remove(&victim);
        }
        drop(dirty);

        Ok(Some(cache.map.get(&idx).expect("just inserted").0.clone()))
    }

    /// Read or write `buf` at byte offset `off`, mapping the range onto
    /// chunk files. Reads of missing chunks (or past a chunk file's end)
    /// return zeros. Sector-aligned only, like the kernel requests we serve.
    pub(crate) fn do_io(&self, write: bool, off: u64, buf: &mut [u8]) -> io::Result<usize> {
        let len = buf.len() as u64;
        if !off.is_multiple_of(512) || !len.is_multiple_of(512) || off + len > self.meta.size_bytes {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "unaligned or out of range"));
        }
        if !write {
            buf.fill(0); // missing chunks / short reads read as zeros
        }

        let mut pos = 0u64;
        while pos < len {
            let abs = off + pos;
            let idx = abs / self.meta.chunk_size;
            let in_off = abs % self.meta.chunk_size;
            let n = (self.meta.chunk_size - in_off).min(len - pos);
            let slice = &mut buf[pos as usize..(pos + n) as usize];

            match self.chunk_fd(idx, write)? {
                Some(file) => {
                    if write {
                        file.write_all_at(slice, in_off)?;
                        self.dirty.lock().map_err(|_| io::Error::other("lock poisoned"))?.insert(idx);
                    } else {
                        // Holes and the unwritten tail stay zero-filled.
                        let _ = file.read_at(slice, in_off)?;
                    }
                }
                None => debug_assert!(!write),
            }
            pos += n;
        }
        Ok(len as usize)
    }

    /// fdatasync every dirty chunk (parallel over a scoped thread pool),
    /// then clear the dirty set. On NFS each fdatasync is a COMMIT.
    pub(crate) fn flush(&self) -> io::Result<()> {
        let dirty: Vec<u64> = self.dirty.lock().map_err(|_| io::Error::other("lock poisoned"))?.iter().copied().collect();
        if dirty.is_empty() {
            return Ok(());
        }

        let groups: Vec<Vec<u64>> = {
            let mut g = vec![Vec::new(); FLUSH_THREADS.min(dirty.len())];
            let n = g.len();
            for (i, idx) in dirty.iter().enumerate() {
                g[i % n].push(*idx);
            }
            g
        };
        let results: Vec<io::Result<()>> = std::thread::scope(|s| {
            let handles: Vec<_> = groups
                .into_iter()
                .map(|group| s.spawn(move || self.flush_group(group)))
                .collect();
            handles.into_iter().map(|h| h.join().expect("flush thread panicked")).collect()
        });
        results.into_iter().collect::<io::Result<()>>()
    }

    fn flush_group(&self, group: Vec<u64>) -> io::Result<()> {
        for idx in group {
            let file = match self.chunk_fd(idx, false)? {
                Some(f) => f,
                // Chunk file gone but marked dirty should not happen; be
                // loud rather than silently dropping durability.
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("dirty chunk {idx} has no file"),
                    ))
                }
            };
            file.sync_data()?;
            self.dirty.lock().map_err(|_| io::Error::other("lock poisoned"))?.remove(&idx);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_volume(size: u64, chunk: u64) -> (tempfile::TempDir, Volume) {
        let dir = tempfile::tempdir().unwrap();
        let vol = Volume::create(dir.path(), size, chunk).unwrap();
        (dir, vol)
    }

    #[test]
    fn meta_round_trip() {
        let (dir, vol) = test_volume(1 << 20, 64 << 10);
        assert_eq!(vol.size_bytes(), 1 << 20);
        let reopened = Volume::open(dir.path()).unwrap();
        assert_eq!(reopened.meta.chunk_size, 64 << 10);
        assert_eq!(reopened.meta.size_bytes, 1 << 20);
    }

    #[test]
    fn rejects_bad_geometry() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Volume::create(dir.path(), 1 << 20, 3000).is_err()); // not power of two
        assert!(Volume::create(dir.path(), 0, 4096).is_err()); // empty
        assert!(Volume::create(dir.path(), 1000, 4096).is_err()); // not sector multiple
    }

    #[test]
    fn missing_chunks_read_as_zeros() {
        let (_dir, vol) = test_volume(1 << 20, 64 << 10);
        let mut buf = vec![0xaa; 8192];
        let n = vol.do_io(false, 128 << 10, &mut buf).unwrap();
        assert_eq!(n, 8192);
        assert!(buf.iter().all(|b| *b == 0));
    }

    #[test]
    fn write_then_read_across_chunk_boundary() {
        let (_dir, vol) = test_volume(1 << 20, 64 << 10);
        // Straddle chunks 1 and 2.
        let off = (64 << 10) - 2048;
        let data: Vec<u8> = (0..8192u32).map(|i| (i % 251) as u8).collect();
        let mut wbuf = data.clone();
        vol.do_io(true, off, &mut wbuf).unwrap();
        let mut rbuf = vec![0u8; 8192];
        vol.do_io(false, off, &mut rbuf).unwrap();
        assert_eq!(rbuf, data);
    }

    #[test]
    fn partial_chunk_leaves_zeros() {
        let (_dir, vol) = test_volume(1 << 20, 64 << 10);
        // Write 4 KiB at the start of chunk 3, read the whole chunk back.
        let mut wbuf = vec![0x5a; 4096];
        vol.do_io(true, 3 * (64 << 10), &mut wbuf).unwrap();
        let mut rbuf = vec![0u8; 64 << 10];
        vol.do_io(false, 3 * (64 << 10), &mut rbuf).unwrap();
        assert!(rbuf[..4096].iter().all(|b| *b == 0x5a));
        assert!(rbuf[4096..].iter().all(|b| *b == 0));
    }

    #[test]
    fn out_of_range_rejected() {
        let (_dir, vol) = test_volume(1 << 20, 64 << 10);
        let mut buf = vec![0u8; 4096];
        assert!(vol.do_io(false, (1 << 20) - 2048, &mut buf).is_err()); // overruns end
        assert!(vol.do_io(false, 100, &mut buf).is_err()); // unaligned
    }

    #[test]
    fn flush_clears_dirty() {
        let (dir, vol) = test_volume(4 << 20, 64 << 10);
        let mut wbuf = vec![1u8; 4096];
        for i in 0..16u64 {
            vol.do_io(true, i * (64 << 10), &mut wbuf).unwrap();
        }
        assert_eq!(vol.dirty.lock().unwrap().len(), 16);
        vol.flush().unwrap();
        assert!(vol.dirty.lock().unwrap().is_empty());
        // Data readable through a fresh Volume (new fd cache).
        let vol2 = Volume::open(dir.path()).unwrap();
        let mut rbuf = vec![0u8; 4096];
        vol2.do_io(false, 5 * (64 << 10), &mut rbuf).unwrap();
        assert!(rbuf.iter().all(|b| *b == 1));
    }

    #[test]
    fn dirty_fds_survive_eviction() {
        let dir = tempfile::tempdir().unwrap();
        Volume::create(dir.path(), 4 << 20, 64 << 10).unwrap();
        // Same volume, but with a tiny fd cache to force eviction.
        let meta: VolumeMeta = serde_json::from_str(
            &fs::read_to_string(dir.path().join("meta.json")).unwrap(),
        )
        .unwrap();
        let vol = Volume {
            dir: dir.path().to_path_buf(),
            meta,
            fds: Mutex::new(FdCache::new(8)),
            dirty: Mutex::new(HashSet::new()),
        };
        // Dirty chunk 7, then read enough other chunks to evict everything
        // clean several times over; the dirty fd must stay pinned.
        let mut wbuf = vec![7u8; 4096];
        vol.do_io(true, 7 * (64 << 10), &mut wbuf).unwrap();
        // Pre-create chunk files so reads open real (clean, evictable)
        // fds — reads of missing chunks never touch the cache.
        for i in 0..64u64 {
            let p = vol.chunk_path(i);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            if i != 7 {
                File::create(&p).unwrap();
            }
        }
        let mut rbuf = vec![0u8; 4096];
        for i in 0..64u64 {
            vol.do_io(false, i * (64 << 10), &mut rbuf).unwrap();
        }
        assert!(vol.fds.lock().unwrap().map.len() <= 8 + 1); // cap + pinned dirty
        assert!(vol.fds.lock().unwrap().map.contains_key(&7));
        vol.flush().unwrap();
        assert!(vol.dirty.lock().unwrap().is_empty());
        drop(vol);
        let vol2 = Volume::open(dir.path()).unwrap();
        let mut verify = vec![0u8; 4096];
        vol2.do_io(false, 7 * (64 << 10), &mut verify).unwrap();
        assert!(verify.iter().all(|b| *b == 7));
    }
}
