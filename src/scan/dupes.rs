//! Duplicate finder. It reads as little as it can, and it never guesses:
//!
//! 1. Same length, and unchanged since the scan.
//! 2. Not the same file (hard links), and not an APFS clone.
//! 3. Same first and last 64 KB. Files whose edges are all zeros are skipped:
//!    preallocated disk images look alike but are not copies.
//! 4. Same xxh3-128 hash of the whole content, read in growing chunks, so a file
//!    stops being read as soon as it differs.
//! 5. Same bytes: every group is compared byte by byte before it is shown.
//!
//! Which files are candidates at all (no tool folders, no code projects) is decided
//! by the home pass in `scan/mod.rs`.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, mpsc};

use xxhash_rust::xxh3::{Xxh3, xxh3_128};

use crate::platform;

/// Smaller files are not worth the reading.
pub const MIN_SIZE: u64 = 1024 * 1024;
const EDGE: u64 = 64 * 1024;
const CHUNKS: [u64; 4] = [1 << 20, 4 << 20, 16 << 20, 64 << 20];
const READ_BUF: usize = 1 << 20;
const READERS: usize = 2;
/// Files open at once while comparing bytes.
const MAX_OPEN: usize = 16;

pub const STAGE_HASH: u8 = 0;
const STAGE_VERIFY: u8 = 1;

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
    pub stage: AtomicU8,
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
    // 1. Same length. Skip files changed since the scan or in the last minute.
    let mut by_len: HashMap<u64, Vec<Cand>> = HashMap::new();
    for c in cands {
        if c.len >= MIN_SIZE && c.mtime < now - 60 && unchanged(&c) {
            by_len.entry(c.len).or_default().push(c);
        }
    }
    let mut groups: Vec<Vec<Cand>> = by_len
        .into_values()
        .filter_map(|mut g| {
            // 2. Hard links to one file are the same file, not copies. Windows listings
            // carry no file id, so read it now, for the few files that share a length.
            g.sort_by(|a, b| a.path.cmp(&b.path));
            let mut seen = HashSet::new();
            g.retain_mut(|c| {
                if c.ino == 0 {
                    c.ino = platform::file_index(&c.path).unwrap_or(0);
                }
                c.ino == 0 || seen.insert(c.ino)
            });
            (g.len() > 1).then_some(g)
        })
        .collect();
    groups.sort_by_key(|g| std::cmp::Reverse(wasted(g)));
    progress.total.store(
        groups.iter().map(|g| g[0].len * g.len() as u64).sum(),
        Relaxed,
    );

    // 3 and 4. Edges, then the whole content, hashed.
    let hashed = in_parallel(groups, cancel, |group, bufs| {
        compare(group, &mut bufs.0, cancel, progress)
    });
    // APFS clones share their disk blocks, so removing one frees nothing.
    let mut groups: Vec<Vec<Cand>> = hashed
        .into_iter()
        .map(|mut group| {
            let mut clones = HashSet::new();
            group.retain(|c| platform::clone_id(&c.path).is_none_or(|id| clones.insert(id)));
            group
        })
        .filter(|g| g.len() > 1)
        .collect();

    // 5. Byte by byte, then once more: unchanged while we read.
    progress.stage.store(STAGE_VERIFY, Relaxed);
    progress.read.store(0, Relaxed);
    progress.total.store(
        groups.iter().map(|g| g[0].len * g.len() as u64).sum(),
        Relaxed,
    );
    groups = in_parallel(groups, cancel, |group, bufs| {
        let (a, b) = bufs;
        let mut same = verify(group, a, b, cancel, progress);
        same.retain(unchanged);
        if same.len() > 1 {
            vec![same]
        } else {
            Vec::new()
        }
    });
    groups.sort_by_key(|g| std::cmp::Reverse(wasted(g)));
    groups
}

/// The file still has the length and modification time the scan saw.
fn unchanged(c: &Cand) -> bool {
    std::fs::symlink_metadata(&c.path)
        .is_ok_and(|m| m.is_file() && m.len() == c.len && crate::scan::mtime_of(&m) == c.mtime)
}

fn wasted(group: &[Cand]) -> u64 {
    group.first().map_or(0, |c| c.len) * (group.len() as u64).saturating_sub(1)
}

/// Runs `work` on each group with `READERS` threads, each with two read buffers.
fn in_parallel<F>(groups: Vec<Vec<Cand>>, cancel: &AtomicBool, work: F) -> Vec<Vec<Cand>>
where
    F: Fn(Vec<Cand>, &mut (Vec<u8>, Vec<u8>)) -> Vec<Vec<Cand>> + Sync,
{
    let queue = Mutex::new(groups.into_iter());
    let found = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..READERS {
            s.spawn(|| {
                platform::worker_init();
                let mut bufs = (vec![0u8; READ_BUF], vec![0u8; READ_BUF]);
                loop {
                    if cancel.load(Relaxed) {
                        return;
                    }
                    let Some(group) = queue.lock().unwrap_or_else(|p| p.into_inner()).next() else {
                        return;
                    };
                    let out = work(group, &mut bufs);
                    found.lock().unwrap_or_else(|p| p.into_inner()).extend(out);
                }
            });
        }
    });
    found.into_inner().unwrap_or_else(|p| p.into_inner())
}

/// Splits one same-size group into groups of files with the same hash.
fn compare(
    group: Vec<Cand>,
    buf: &mut [u8],
    cancel: &AtomicBool,
    progress: &Progress,
) -> Vec<Vec<Cand>> {
    let len = group[0].len;
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

/// Hash of the first and last 64 KB. `None` if unreadable or all zeros.
fn edge_hash(path: &Path, len: u64, buf: &mut [u8]) -> Option<u128> {
    let mut file = platform::open_for_hash(path).ok()?;
    let edge = EDGE as usize;
    file.read_exact(&mut buf[..edge]).ok()?;
    file.seek(SeekFrom::Start(len - EDGE)).ok()?;
    file.read_exact(&mut buf[edge..2 * edge]).ok()?;
    let edges = &buf[..2 * edge];
    if edges.iter().all(|&b| b == 0) {
        return None;
    }
    Some(xxh3_128(edges))
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

/// Compares every member with the first one, byte by byte. A member that differs,
/// or cannot be read, is dropped. At most `MAX_OPEN` files are open at a time.
fn verify(
    group: Vec<Cand>,
    first_buf: &mut [u8],
    other_buf: &mut [u8],
    cancel: &AtomicBool,
    progress: &Progress,
) -> Vec<Cand> {
    let mut members = group.into_iter();
    let Some(first) = members.next() else {
        return Vec::new();
    };
    let rest: Vec<Cand> = members.collect();
    let mut same = vec![first.clone()];
    for batch in rest.chunks(MAX_OPEN - 1) {
        let Ok(mut reference) = platform::open_for_hash(&first.path) else {
            return Vec::new();
        };
        let mut open: Vec<(usize, File)> = batch
            .iter()
            .enumerate()
            .filter_map(|(i, c)| platform::open_for_hash(&c.path).ok().map(|f| (i, f)))
            .collect();
        let mut left = first.len;
        while left > 0 && !open.is_empty() {
            if cancel.load(Relaxed) {
                return Vec::new();
            }
            let n = (left as usize).min(first_buf.len());
            if reference.read_exact(&mut first_buf[..n]).is_err() {
                return Vec::new();
            }
            let expected = &first_buf[..n];
            open.retain_mut(|(_, f)| {
                f.read_exact(&mut other_buf[..n]).is_ok() && other_buf[..n] == *expected
            });
            progress
                .read
                .fetch_add(n as u64 * (open.len() as u64 + 1), Relaxed);
            left -= n as u64;
        }
        same.extend(open.into_iter().map(|(i, _)| batch[i].clone()));
    }
    same
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes a file and dates it ten minutes back, so it counts as settled.
    fn write_old(path: &Path, data: &[u8]) -> Cand {
        std::fs::write(path, data).unwrap();
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(600);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(past)
            .unwrap();
        cand(path)
    }

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
            mtime: crate::scan::mtime_of(&md),
        }
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8 + 1).collect()
    }

    fn run(cands: Vec<Cand>) -> Vec<Vec<String>> {
        let groups = find(
            cands,
            crate::fmt::now(),
            &AtomicBool::new(false),
            &Progress::default(),
        );
        groups
            .iter()
            .map(|g| {
                let mut names: Vec<String> = g
                    .iter()
                    .map(|c| c.path.file_name().unwrap().to_string_lossy().into_owned())
                    .collect();
                names.sort();
                names
            })
            .collect()
    }

    #[test]
    fn finds_identical_files_only() {
        let dir = crate::rules::tests::tempdir("dupes");
        let a = pattern(3 * 1024 * 1024 + 17);
        let mut b = a.clone();
        let mid = b.len() / 2;
        b[mid] ^= 1; // same size, same edges, different middle
        let mut cands = vec![
            write_old(&dir.join("a1"), &a),
            write_old(&dir.join("a2"), &a),
            write_old(&dir.join("b"), &b),
            write_old(&dir.join("small1"), &[1u8; 1000]),
            write_old(&dir.join("small2"), &[1u8; 1000]),
        ];
        // On Windows the candidate has no file id; the search reads it.
        std::fs::hard_link(dir.join("a1"), dir.join("a1-link")).unwrap();
        cands.push(cand(&dir.join("a1-link")));
        assert_eq!(run(cands), [["a1", "a2"]], "a hard link is not a copy");
    }

    #[test]
    fn skips_zero_filled_files() {
        let dir = crate::rules::tests::tempdir("dupes-zero");
        let zeros = vec![0u8; 2 * 1024 * 1024];
        let cands = vec![
            write_old(&dir.join("disk1.img"), &zeros),
            write_old(&dir.join("disk2.img"), &zeros),
        ];
        assert!(run(cands).is_empty());
    }

    #[test]
    fn drops_files_changed_after_the_scan() {
        let dir = crate::rules::tests::tempdir("dupes-changed");
        let a = pattern(2 * 1024 * 1024);
        let one = write_old(&dir.join("one"), &a);
        let mut two = write_old(&dir.join("two"), &a);
        two.mtime -= 3_600; // the scan saw an older version
        assert!(run(vec![one, two]).is_empty());
    }

    #[test]
    fn byte_check_drops_a_member_that_differs() {
        let dir = crate::rules::tests::tempdir("dupes-verify");
        let a = pattern(2 * 1024 * 1024 + 5);
        let mut c = a.clone();
        c[1_500_000] ^= 0xFF;
        let group = vec![
            write_old(&dir.join("a"), &a),
            write_old(&dir.join("a-copy"), &a),
            write_old(&dir.join("c"), &c),
        ];
        let (mut x, mut y) = (vec![0u8; READ_BUF], vec![0u8; READ_BUF]);
        let same = verify(
            group,
            &mut x,
            &mut y,
            &AtomicBool::new(false),
            &Progress::default(),
        );
        let names: Vec<_> = same
            .iter()
            .map(|c| c.path.file_name().unwrap().to_owned())
            .collect();
        assert_eq!(names, ["a", "a-copy"]);
    }
}
