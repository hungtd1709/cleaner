//! Duplicate finder. It reads as little as it can: files are compared by size, then by a
//! hash of their first and last 64 KB, then in growing chunks. A file stops being read
//! as soon as it differs from the others.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, mpsc};

use xxhash_rust::xxh3::{Xxh3, xxh3_128};

use crate::platform;

/// Smaller files are not worth the reading.
pub const MIN_SIZE: u64 = 1024 * 1024;
const EDGE: u64 = 64 * 1024;
const CHUNKS: [u64; 4] = [1 << 20, 4 << 20, 16 << 20, 64 << 20];
const READ_BUF: usize = 1 << 20;
const READERS: usize = 2;

#[derive(Clone, Debug)]
pub struct Cand {
    pub path: PathBuf,
    pub len: u64,
    /// File id; 0 when unknown.
    pub ino: u64,
    pub mtime: i64,
}

#[derive(Default)]
pub struct Progress {
    pub read: AtomicU64,
    pub total: AtomicU64,
}

pub struct Handle {
    pub progress: Arc<Progress>,
    pub cancel: Arc<AtomicBool>,
    pub rx: mpsc::Receiver<Option<Vec<Vec<Cand>>>>,
}

/// Searches in the background. Sends `None` if cancelled.
pub fn start(cands: Vec<Cand>, now: i64, repaint: impl Fn() + Send + 'static) -> Handle {
    let progress = Arc::new(Progress::default());
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let (p, c) = (progress.clone(), cancel.clone());
    std::thread::Builder::new()
        .name("duplicates".into())
        .spawn(move || {
            let _activity = platform::begin_activity("Looking for duplicate files");
            let groups = find(cands, now, &c, &p);
            let _ = tx.send((!c.load(Relaxed)).then_some(groups));
            repaint();
        })
        .expect("start the duplicate thread");
    Handle {
        progress,
        cancel,
        rx,
    }
}

/// Groups of identical files, biggest waste first. Each group has two or more files.
pub fn find(
    cands: Vec<Cand>,
    now: i64,
    cancel: &AtomicBool,
    progress: &Progress,
) -> Vec<Vec<Cand>> {
    // 1. Same size. Files changed in the last minute may still be written.
    let mut by_len: HashMap<u64, Vec<Cand>> = HashMap::new();
    for c in cands {
        if c.len >= MIN_SIZE && c.mtime < now - 60 {
            by_len.entry(c.len).or_default().push(c);
        }
    }
    let mut groups: Vec<Vec<Cand>> = by_len
        .into_values()
        .filter_map(|mut g| {
            // 2. Hard links to one file are the same file, not copies.
            g.sort_by(|a, b| a.path.cmp(&b.path));
            let mut seen = HashSet::new();
            g.retain(|c| c.ino == 0 || seen.insert(c.ino));
            (g.len() > 1).then_some(g)
        })
        .collect();
    groups.sort_by_key(|g| std::cmp::Reverse(wasted(g)));
    progress.total.store(
        groups.iter().map(|g| g[0].len * g.len() as u64).sum(),
        Relaxed,
    );

    let queue = Mutex::new(groups.into_iter());
    let found = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..READERS {
            s.spawn(|| {
                platform::worker_init();
                let mut buf = vec![0u8; READ_BUF];
                loop {
                    if cancel.load(Relaxed) {
                        return;
                    }
                    let Some(group) = queue.lock().unwrap_or_else(|p| p.into_inner()).next() else {
                        return;
                    };
                    let same = compare(group, &mut buf, cancel, progress);
                    found.lock().unwrap_or_else(|p| p.into_inner()).extend(same);
                }
            });
        }
    });
    let mut found = found.into_inner().unwrap_or_else(|p| p.into_inner());
    // 3. APFS clones share their disk blocks, so removing one frees nothing.
    for group in &mut found {
        let mut clones = HashSet::new();
        group.retain(|c| platform::clone_id(&c.path).is_none_or(|id| clones.insert(id)));
    }
    found.retain(|g| g.len() > 1);
    found.sort_by_key(|g| std::cmp::Reverse(wasted(g)));
    found
}

fn wasted(group: &[Cand]) -> u64 {
    group.first().map_or(0, |c| c.len) * (group.len() as u64).saturating_sub(1)
}

/// Splits one same-size group into groups of identical files.
fn compare(
    group: Vec<Cand>,
    buf: &mut [u8],
    cancel: &AtomicBool,
    progress: &Progress,
) -> Vec<Vec<Cand>> {
    let len = group[0].len;
    // 4. First and last 64 KB.
    let mut by_edges: HashMap<u128, Vec<Cand>> = HashMap::new();
    for c in group {
        if cancel.load(Relaxed) {
            return Vec::new();
        }
        if let Some(h) = edge_hash(&c.path, len, buf) {
            progress.read.fetch_add(2 * EDGE, Relaxed);
            by_edges.entry(h).or_default().push(c);
        }
    }
    // 5. The whole file, in growing chunks, regrouping after each one.
    let mut out = Vec::new();
    for same_edges in by_edges.into_values().filter(|g| g.len() > 1) {
        let mut groups: Vec<Vec<(Cand, Xxh3)>> =
            vec![same_edges.into_iter().map(|c| (c, Xxh3::new())).collect()];
        let mut offset = 0u64;
        let mut step = 0;
        while offset < len && !groups.is_empty() {
            let chunk = CHUNKS[step.min(CHUNKS.len() - 1)].min(len - offset);
            let mut next = Vec::new();
            for g in groups {
                let mut by_hash: HashMap<u128, Vec<(Cand, Xxh3)>> = HashMap::new();
                for (c, mut hasher) in g {
                    if cancel.load(Relaxed) {
                        return Vec::new();
                    }
                    if hash_range(&c.path, offset, chunk, &mut hasher, buf).is_ok() {
                        progress.read.fetch_add(chunk, Relaxed);
                        by_hash
                            .entry(hasher.digest128())
                            .or_default()
                            .push((c, hasher));
                    }
                }
                next.extend(by_hash.into_values().filter(|g| g.len() > 1));
            }
            groups = next;
            offset += chunk;
            step += 1;
        }
        out.extend(
            groups
                .into_iter()
                .map(|g| g.into_iter().map(|(c, _)| c).collect()),
        );
    }
    out
}

fn edge_hash(path: &Path, len: u64, buf: &mut [u8]) -> Option<u128> {
    let mut file = platform::open_for_hash(path).ok()?;
    let edge = EDGE as usize;
    file.read_exact(&mut buf[..edge]).ok()?;
    file.seek(SeekFrom::Start(len - EDGE)).ok()?;
    file.read_exact(&mut buf[edge..2 * edge]).ok()?;
    Some(xxh3_128(&buf[..2 * edge]))
}

fn hash_range(
    path: &Path,
    offset: u64,
    len: u64,
    hasher: &mut Xxh3,
    buf: &mut [u8],
) -> std::io::Result<()> {
    let mut file = platform::open_for_hash(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut left = len;
    while left > 0 {
        let n = (left as usize).min(buf.len());
        file.read_exact(&mut buf[..n])?;
        hasher.update(&buf[..n]);
        left -= n as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(path: &Path) -> Cand {
        let md = std::fs::symlink_metadata(path).unwrap();
        #[cfg(unix)]
        let ino = std::os::unix::fs::MetadataExt::ino(&md);
        #[cfg(not(unix))]
        let ino = 0;
        Cand {
            path: path.to_path_buf(),
            len: md.len(),
            ino,
            mtime: 0,
        }
    }

    #[test]
    fn finds_identical_files_only() {
        let dir = crate::rules::tests::tempdir("dupes");
        let mut a = vec![0u8; 3 * 1024 * 1024 + 17];
        for (i, b) in a.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let mut b = a.clone();
        let mid = b.len() / 2;
        b[mid] ^= 1; // same size, same edges, different middle
        std::fs::write(dir.join("a1"), &a).unwrap();
        std::fs::write(dir.join("a2"), &a).unwrap();
        std::fs::write(dir.join("b"), &b).unwrap();
        std::fs::write(dir.join("small1"), vec![1u8; 1000]).unwrap();
        std::fs::write(dir.join("small2"), vec![1u8; 1000]).unwrap();
        #[cfg(unix)]
        std::fs::hard_link(dir.join("a1"), dir.join("a1-link")).unwrap();

        #[allow(unused_mut)]
        let mut cands: Vec<Cand> = ["a1", "a2", "b", "small1", "small2"]
            .iter()
            .map(|n| cand(&dir.join(n)))
            .collect();
        #[cfg(unix)]
        cands.push(cand(&dir.join("a1-link")));
        let groups = find(
            cands,
            crate::fmt::now(),
            &AtomicBool::new(false),
            &Progress::default(),
        );
        assert_eq!(groups.len(), 1, "{groups:?}");
        let mut names: Vec<_> = groups[0]
            .iter()
            .map(|c| c.path.file_name().unwrap().to_owned())
            .collect();
        names.sort();
        assert_eq!(names.len(), 2, "a hard link is not a copy: {names:?}");
        assert!(names[1] == "a2");
    }
}
