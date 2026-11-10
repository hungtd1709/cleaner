//! Cleaning: safety re-checks, then delete, move to the Trash, or empty the Trash.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex, mpsc};

use crate::fmt;
use crate::platform;
use crate::rules::{self, Action, Guard};
use crate::scan::{Ident, Item, Shape, Status, mtime_of};

/// The report lists at most this many skipped paths.
const MAX_SKIPPED: usize = 500;

pub struct Job {
    /// Index of the item in the app's list.
    pub index: usize,
    pub item: Item,
}

/// One file of a duplicate group in the app's list, selected or not.
pub struct GroupFile {
    pub group: u32,
    pub path: PathBuf,
    pub modified: i64,
}

#[derive(Default)]
pub struct Progress {
    pub done: AtomicUsize,
    pub total: AtomicUsize,
    pub removed: AtomicU64,
}

#[derive(Debug, Default)]
pub struct Report {
    /// Bytes deleted for good.
    pub removed: u64,
    /// Bytes moved to the Trash. Disk space comes back when the Trash is emptied.
    pub trashed: u64,
    pub free_before: Option<u64>,
    pub free_after: Option<u64>,
    /// Skipped paths with the reason, as display text.
    pub skipped: Vec<(String, String)>,
    pub skipped_more: usize,
    /// Items cleaned completely.
    pub finished: Vec<usize>,
    /// Items cleaned in part, with the bytes removed.
    pub partial: Vec<(usize, u64)>,
    pub stopped: bool,
    /// Why nothing was cleaned at all.
    pub refused: Option<String>,
    /// Items moved to the Trash: (item index, original path, path in the Trash).
    pub moved: Vec<(usize, PathBuf, Option<PathBuf>)>,
    /// When cleaning started, in seconds since 1970. Undo looks for items from then on.
    pub started: i64,
}

pub struct Handle {
    pub progress: Arc<Progress>,
    pub cancel: Arc<AtomicBool>,
    pub rx: mpsc::Receiver<Report>,
}

/// Cleans in the background. `group_files` lists every file of every duplicate group.
pub fn start(
    jobs: Vec<Job>,
    group_files: Vec<GroupFile>,
    low_power: bool,
    never: Vec<PathBuf>,
    repaint: impl Fn() + Send + 'static,
) -> Handle {
    let progress = Arc::new(Progress::default());
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let (p, c) = (progress.clone(), cancel.clone());
    std::thread::Builder::new()
        .name("clean".into())
        .spawn(move || {
            let report = run(jobs, &group_files, low_power, &never, &p, &c);
            let _ = tx.send(report);
            repaint();
        })
        .expect("start the clean thread");
    Handle {
        progress,
        cancel,
        rx,
    }
}

#[derive(Default)]
struct Outcome {
    removed: u64,
    trashed: u64,
    skipped: Vec<(PathBuf, String)>,
    complete: bool,
    /// For a move to the Trash: the original path and where it went.
    moved: Option<(PathBuf, Option<PathBuf>)>,
}

pub fn run(
    jobs: Vec<Job>,
    group_files: &[GroupFile],
    low_power: bool,
    never: &[PathBuf],
    progress: &Progress,
    cancel: &AtomicBool,
) -> Report {
    let _activity = platform::begin_activity("Cleaning files");
    let mut report = Report {
        started: fmt::now(),
        ..Report::default()
    };
    if platform::is_elevated() {
        report.refused = Some(
            "Cleaner is running as an administrator, so it does not clean anything.".to_owned(),
        );
        return report;
    }
    let Some(env) = platform::env() else {
        report.refused = Some("Cleaner could not find your home folder.".to_owned());
        return report;
    };
    let targets = rules::targets(&env);
    let guard = Guard::new(&env, &targets, never);
    let roots: HashSet<PathBuf> = targets
        .iter()
        .flat_map(|t| t.roots.iter().cloned())
        .collect();
    report.free_before = platform::system_disk().map(|d| d.free);

    let mut outcomes: Vec<(usize, Outcome)> = Vec::new();
    let (jobs, followers) = normalize(jobs, group_files, &mut outcomes);
    progress.total.store(jobs.len(), Relaxed);
    let (deletes, others): (Vec<&Job>, Vec<&Job>) =
        jobs.iter().partition(|j| j.item.action() == Action::Delete);

    // Deletes run in parallel, one item per worker at a time.
    let done = Mutex::new(Vec::new());
    let next = AtomicUsize::new(0);
    let threads = platform::worker_count(low_power).min(deletes.len()).max(1);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                platform::worker_init();
                while let Some(job) = deletes.get(next.fetch_add(1, Relaxed)) {
                    if cancel.load(Relaxed) {
                        break;
                    }
                    let outcome = delete_item(&job.item, &guard, &roots, cancel, progress);
                    progress.done.fetch_add(1, Relaxed);
                    done.lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .push((job.index, outcome));
                }
            });
        }
    });
    outcomes.extend(done.into_inner().unwrap_or_else(|p| p.into_inner()));

    // Trash moves and emptying run one at a time on this thread.
    for job in others {
        if cancel.load(Relaxed) {
            break;
        }
        let outcome = match job.item.action() {
            Action::Trash => trash_item(&job.item, &guard),
            _ => empty_trash(&job.item, &guard, &roots, cancel, progress),
        };
        progress.done.fetch_add(1, Relaxed);
        outcomes.push((job.index, outcome));
    }

    resolve_followers(&jobs, &mut outcomes, followers);
    report.free_after = platform::system_disk().map(|d| d.free);
    report.stopped = cancel.load(Relaxed);
    for (index, outcome) in outcomes {
        report.removed += outcome.removed;
        report.trashed += outcome.trashed;
        if let Some((original, in_trash)) = outcome.moved {
            report.moved.push((index, original, in_trash));
        }
        if outcome.complete {
            report.finished.push(index);
        } else if outcome.removed + outcome.trashed > 0 {
            report
                .partial
                .push((index, outcome.removed + outcome.trashed));
        }
        for (path, why) in outcome.skipped {
            if report.skipped.len() < MAX_SKIPPED {
                report.skipped.push((fmt::path(&path), why));
            } else {
                report.skipped_more += 1;
            }
        }
    }
    report
}

/// Keeps one file of every duplicate group (rule S10), and drops items inside another
/// selected item. A dropped item becomes a "follower": it is done only if the item that
/// covers it is done (see `resolve_followers`).
fn normalize(
    mut jobs: Vec<Job>,
    group_files: &[GroupFile],
    outcomes: &mut Vec<(usize, Outcome)>,
) -> (Vec<Job>, Vec<(usize, PathBuf)>) {
    let keep = keep_last_copies(&jobs, group_files);
    let mut index = 0;
    jobs.retain(|job| {
        let why = keep.get(&index);
        index += 1;
        if let Some(&why) = why {
            let outcome = Outcome {
                skipped: vec![(job.item.paths[0].clone(), why.to_owned())],
                ..Outcome::default()
            };
            outcomes.push((job.index, outcome));
        }
        why.is_none()
    });

    let entries: HashSet<PathBuf> = jobs
        .iter()
        .filter(|j| j.item.shape == Shape::Entry)
        .map(|j| j.item.paths[0].clone())
        .collect();
    let mut seen = HashSet::new();
    let mut followers = Vec::new();
    jobs.retain(|job| {
        if job.item.shape != Shape::Entry {
            return true;
        }
        let path = &job.item.paths[0];
        if let Some(cover) = path.ancestors().skip(1).find(|a| entries.contains(*a)) {
            followers.push((job.index, cover.to_path_buf()));
            return false;
        }
        if !seen.insert(path.clone()) {
            followers.push((job.index, path.clone()));
            return false;
        }
        true
    });
    (jobs, followers)
}

/// Rule S10: one file of every duplicate group stays, whichever category selected the
/// others. A file goes if a job removes its path or a folder that holds it. Returns the
/// jobs (by position) that must not run, with the reason.
fn keep_last_copies(jobs: &[Job], group_files: &[GroupFile]) -> HashMap<usize, &'static str> {
    let mut by_path: HashMap<&Path, Vec<usize>> = HashMap::new();
    for (i, job) in jobs.iter().enumerate() {
        if job.item.shape == Shape::Entry {
            by_path
                .entry(job.item.paths[0].as_path())
                .or_default()
                .push(i);
        }
    }
    let mut groups: BTreeMap<u32, Vec<&GroupFile>> = BTreeMap::new();
    for f in group_files {
        groups.entry(f.group).or_default().push(f);
    }
    let mut dropped: HashMap<usize, &'static str> = HashMap::new();
    for files in groups.values() {
        // The jobs that would remove each file. One file that no job removes is enough.
        let removers: Vec<Vec<usize>> = files
            .iter()
            .map(|f| {
                f.path
                    .ancestors()
                    .filter_map(|a| by_path.get(a))
                    .flatten()
                    .copied()
                    .filter(|j| !dropped.contains_key(j))
                    .collect()
            })
            .collect();
        if removers.iter().any(Vec::is_empty) {
            continue;
        }
        // Keep the oldest file, preferring one that no selected folder holds, so that
        // the folder can still go.
        let Some(keep) = (0..files.len()).min_by_key(|&k| {
            let in_folder = removers[k]
                .iter()
                .any(|&j| jobs[j].item.paths[0] != files[k].path);
            (in_folder, files[k].modified, &files[k].path)
        }) else {
            continue;
        };
        for &j in &removers[keep] {
            let why = if jobs[j].item.paths[0] == files[keep].path {
                "kept as the last copy"
            } else {
                "holds the last copy of a duplicate"
            };
            dropped.insert(j, why);
        }
    }
    dropped
}

/// A follower is finished only if the item covering it was cleaned completely.
fn resolve_followers(
    jobs: &[Job],
    outcomes: &mut Vec<(usize, Outcome)>,
    followers: Vec<(usize, PathBuf)>,
) {
    let path_of: HashMap<usize, &PathBuf> = jobs
        .iter()
        .filter(|j| j.item.shape == Shape::Entry)
        .map(|j| (j.index, &j.item.paths[0]))
        .collect();
    let done: HashSet<PathBuf> = outcomes
        .iter()
        .filter(|(_, o)| o.complete)
        .filter_map(|(i, _)| path_of.get(i).map(|p| (*p).clone()))
        .collect();
    for (index, cover) in followers {
        if done.contains(&cover) {
            let outcome = Outcome {
                complete: true,
                ..Outcome::default()
            };
            outcomes.push((index, outcome));
        }
    }
}

/// Same identity, not a link. For user files (`strict`), also the same size and time.
fn ident_ok(path: &Path, ident: Option<Ident>, strict: bool) -> bool {
    let Ok(md) = fs::symlink_metadata(path) else {
        return false;
    };
    if md.file_type().is_symlink() {
        return false;
    }
    let Some(id) = ident else { return true };
    if md.is_dir() != id.dir {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if id.ino != 0 && md.ino() != id.ino {
            return false;
        }
    }
    !(strict && !id.dir && (md.len() != id.len || mtime_of(&md) != id.mtime))
}

fn delete_item(
    item: &Item,
    guard: &Guard,
    roots: &HashSet<PathBuf>,
    cancel: &AtomicBool,
    progress: &Progress,
) -> Outcome {
    let own: HashSet<&PathBuf> = item.paths.iter().collect();
    let exclude: HashSet<PathBuf> = roots.iter().filter(|r| !own.contains(r)).cloned().collect();
    let cutoff = if item.min_age_days > 0 {
        fmt::now() - i64::from(item.min_age_days) * 86_400
    } else {
        0
    };
    let mut d = Deleter {
        cutoff,
        exclude: &exclude,
        guard,
        cancel,
        progress,
        volume: None,
        removed: 0,
        skipped: Vec::new(),
    };
    match item.shape {
        Shape::Contents => {
            for root in &item.paths {
                d.empty_root(root);
            }
        }
        Shape::Entry => {
            let path = &item.paths[0];
            if let Err(why) = guard.check(path, &item.root) {
                d.skip(path, why);
            } else if !ident_ok(path, item.ident, false) {
                d.skip(path, "it changed since the scan");
            } else {
                d.volume = fs::symlink_metadata(&item.root)
                    .ok()
                    .and_then(|m| platform::volume_id(&m));
                d.remove_top(path);
            }
        }
    }
    let complete = d.skipped.is_empty() && !cancel.load(Relaxed);
    Outcome {
        removed: d.removed,
        skipped: d.skipped,
        complete,
        ..Outcome::default()
    }
}

fn trash_item(item: &Item, guard: &Guard) -> Outcome {
    let path = &item.paths[0];
    let fail = |why: String| Outcome {
        skipped: vec![(path.clone(), why)],
        ..Outcome::default()
    };
    if let Err(why) = guard.check(path, &item.root) {
        return fail(why.to_owned());
    }
    // The Trash takes a folder as a whole, so a Never touch item inside would go too.
    if guard.holds_hard(path) {
        return fail("holds a Never touch item".to_owned());
    }
    if !ident_ok(path, item.ident, true) {
        return fail("it changed since the scan".to_owned());
    }
    match platform::move_to_trash(path) {
        Ok(in_trash) => Outcome {
            trashed: item.size,
            complete: true,
            moved: Some((path.clone(), in_trash)),
            ..Outcome::default()
        },
        Err(why) => fail(why),
    }
}

/// Puts back items that a clean moved to the Trash. Runs in the background.
pub fn restore(
    items: Vec<(PathBuf, Option<PathBuf>)>,
    since: i64,
    repaint: impl Fn() + Send + 'static,
) -> mpsc::Receiver<Vec<(PathBuf, Result<(), String>)>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(platform::restore_from_trash(&items, since));
        repaint();
    });
    rx
}

fn empty_trash(
    item: &Item,
    guard: &Guard,
    roots: &HashSet<PathBuf>,
    cancel: &AtomicBool,
    progress: &Progress,
) -> Outcome {
    if item.paths.is_empty() || item.status == Status::Unmeasured {
        // The Windows Recycle Bin, or a macOS Trash the app cannot read: ask the OS.
        let result = if item.paths.is_empty() {
            platform::empty_recycle_bin()
        } else {
            platform::empty_trash_with_finder()
        };
        return match result {
            Ok(()) => Outcome {
                removed: item.size,
                complete: true,
                ..Outcome::default()
            },
            Err(why) => Outcome {
                skipped: vec![(PathBuf::from(item.name.clone()), why)],
                ..Outcome::default()
            },
        };
    }
    delete_item(item, guard, roots, cancel, progress)
}

struct Deleter<'a> {
    /// Files changed after this time are kept. 0 means no limit.
    cutoff: i64,
    /// Roots of other targets, left alone when found inside a removed folder.
    exclude: &'a HashSet<PathBuf>,
    guard: &'a Guard,
    cancel: &'a AtomicBool,
    progress: &'a Progress,
    /// The volume the item lives on. Deletes stop at other volumes.
    volume: Option<u64>,
    removed: u64,
    skipped: Vec<(PathBuf, String)>,
}

impl Deleter<'_> {
    fn skip(&mut self, path: &Path, why: impl Into<String>) {
        self.skipped.push((path.to_path_buf(), why.into()));
    }

    /// Removes what is inside a root. The root itself stays (rule S2).
    fn empty_root(&mut self, root: &Path) {
        match fs::symlink_metadata(root) {
            Ok(md) if md.is_dir() && !md.file_type().is_symlink() => {
                self.volume = platform::volume_id(&md)
            }
            Ok(_) => return self.skip(root, "the folder changed since the scan"),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return,
            Err(e) => return self.skip(root, reason(&e)),
        }
        let children: Vec<PathBuf> = match fs::read_dir(root) {
            Ok(read) => read.filter_map(|e| e.ok().map(|e| e.path())).collect(),
            Err(e) => return self.skip(root, reason(&e)),
        };
        for child in children {
            if self.cancel.load(Relaxed) {
                return;
            }
            match self.guard.check(&child, root) {
                Ok(()) => self.remove_top(&child),
                Err(why) => self.skip(&child, why),
            }
        }
    }

    /// Removes one listed item. On Windows a folder is renamed first: the rename fails
    /// if a file inside is open, so a folder in use is skipped as a whole.
    fn remove_top(&mut self, path: &Path) {
        if platform::RENAME_BEFORE_DELETE && self.cutoff == 0 {
            let is_dir =
                fs::symlink_metadata(path).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink());
            // A rename would move what must stay: another target's root or a Never touch item.
            let holds_root =
                self.exclude.iter().any(|r| r.starts_with(path)) || self.guard.holds_hard(path);
            if is_dir && !holds_root {
                let moved = renamed(path);
                if fs::rename(path, &moved).is_err() {
                    return self.skip(path, "in use by another app");
                }
                self.tree(&moved, 0);
                return;
            }
        }
        self.tree(path, 0);
    }

    /// Removes a file or folder tree. Returns true if it is gone.
    fn tree(&mut self, path: &Path, depth: u32) -> bool {
        if self.cancel.load(Relaxed) {
            return false;
        }
        let md = match fs::symlink_metadata(path) {
            Ok(md) => md,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return true,
            Err(e) => {
                self.skip(path, reason(&e));
                return false;
            }
        };
        if self.exclude.contains(path) || self.guard.hard_denied(path) {
            return false;
        }
        let ft = md.file_type();
        if ft.is_symlink() {
            // Remove the link itself, never what it points to.
            return self.unlink(path, 0, is_dir_link(&md));
        }
        if is_special(&md) {
            self.skip(path, "cloud or special file");
            return false;
        }
        if !ft.is_dir() && !ft.is_file() {
            return false; // sockets and devices are left alone
        }
        if let (Some(volume), Some(here)) = (self.volume, platform::volume_id(&md))
            && volume != here
        {
            self.skip(path, "on another volume");
            return false;
        }
        if ft.is_file() {
            if self.cutoff != 0 && newest(&md) > self.cutoff {
                return false;
            }
            return self.unlink(path, platform::alloc_size(&md), false);
        }
        if depth > 512 {
            self.skip(path, "folders nested too deep");
            return false;
        }
        let old_enough = self.cutoff == 0 || mtime_of(&md) <= self.cutoff;
        let children: Vec<PathBuf> = match fs::read_dir(path) {
            Ok(read) => read.filter_map(|e| e.ok().map(|e| e.path())).collect(),
            Err(e) => {
                self.skip(path, reason(&e));
                return false;
            }
        };
        let mut all_gone = true;
        for child in children {
            all_gone &= self.tree(&child, depth + 1);
        }
        if !all_gone || !old_enough {
            return false;
        }
        self.remove(path, |p| fs::remove_dir(p))
    }

    fn unlink(&mut self, path: &Path, bytes: u64, dir_link: bool) -> bool {
        let gone = if dir_link {
            self.remove(path, |p| fs::remove_dir(p))
        } else {
            self.remove(path, |p| fs::remove_file(p))
        };
        if gone {
            self.removed += bytes;
            self.progress.removed.fetch_add(bytes, Relaxed);
        }
        gone
    }

    /// Runs `op`; on a permission error makes the item removable and tries once more.
    fn remove(&mut self, path: &Path, op: impl Fn(&Path) -> io::Result<()>) -> bool {
        let result = match op(path) {
            Err(e)
                if e.kind() == io::ErrorKind::PermissionDenied
                    && platform::make_removable(path) =>
            {
                op(path)
            }
            other => other,
        };
        match result {
            Ok(()) => true,
            Err(e) if e.kind() == io::ErrorKind::NotFound => true,
            Err(e) => {
                self.skip(path, reason(&e));
                false
            }
        }
    }
}

/// A free name next to `path` for the rename probe.
fn renamed(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let mut n = 0u32;
    loop {
        let candidate = path.with_file_name(format!("{name}.cleaner-{}-{n}", std::process::id()));
        if fs::symlink_metadata(&candidate).is_err() {
            return candidate;
        }
        n += 1;
    }
}

fn newest(md: &fs::Metadata) -> i64 {
    let created = md
        .created()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64);
    mtime_of(md).max(created)
}

#[cfg(windows)]
fn is_dir_link(md: &fs::Metadata) -> bool {
    use std::os::windows::fs::FileTypeExt;
    md.file_type().is_symlink_dir()
}

#[cfg(not(windows))]
fn is_dir_link(_md: &fs::Metadata) -> bool {
    false
}

/// Windows reparse points that are not links: cloud files and similar. Never touched.
#[cfg(windows)]
fn is_special(md: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 && !md.file_type().is_symlink()
}

#[cfg(not(windows))]
fn is_special(_md: &fs::Metadata) -> bool {
    false
}

fn reason(e: &io::Error) -> String {
    #[cfg(windows)]
    if matches!(e.raw_os_error(), Some(32 | 33)) {
        return "in use by another app".to_owned();
    }
    match e.kind() {
        io::ErrorKind::PermissionDenied => "no permission".to_owned(),
        io::ErrorKind::NotFound => "already gone".to_owned(),
        _ => e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::{Category, Env, Filter, Mode, Target, tests::tempdir};

    fn setup(tag: &str) -> (PathBuf, Guard, HashSet<PathBuf>, PathBuf) {
        let dir = tempdir(tag);
        let home = dir.join("home");
        let root = home.join("Library/Caches");
        std::fs::create_dir_all(&root).unwrap();
        let env = Env {
            home: home.clone(),
            temp: home.join("tmp"),
            downloads: home.join("Downloads"),
            local: home.join("Library"),
            cloud: Vec::new(),
        };
        let target = Target {
            category: Category::SystemJunk,
            name: "App cache".into(),
            roots: vec![root.clone()],
            mode: Mode::Children,
            filter: Filter::Any,
            min_age_days: 0,
            app_id: None,
            selected: true,
            keep: Vec::new(),
            note: "",
            watch_recent: false,
        };
        let guard = Guard::new(&env, std::slice::from_ref(&target), &[]);
        (dir, guard, HashSet::from([root.clone()]), root)
    }

    fn entry_item(root: &Path, name: &str) -> Item {
        let path = root.join(name);
        let mut item = Item::new(
            Category::SystemJunk,
            name,
            vec![path],
            root.to_path_buf(),
            Shape::Entry,
        );
        item.ident = None;
        item
    }

    #[test]
    fn deletes_tree_but_not_link_targets() {
        let (dir, guard, roots, root) = setup("clean-links");
        let outside = dir.join("precious");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep.txt"), b"keep").unwrap();
        let cache = root.join("com.example");
        std::fs::create_dir_all(cache.join("a/b")).unwrap();
        std::fs::write(cache.join("a/b/f"), vec![0u8; 10_000]).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, cache.join("link")).unwrap();

        let out = delete_item(
            &entry_item(&root, "com.example"),
            &guard,
            &roots,
            &AtomicBool::new(false),
            &Progress::default(),
        );
        assert!(out.complete, "{:?}", out.skipped);
        assert!(!cache.exists());
        assert!(
            outside.join("keep.txt").exists(),
            "a link target must survive"
        );
        assert!(root.exists(), "the root must survive");
    }

    #[test]
    #[cfg(unix)]
    fn deletes_read_only_trees() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, guard, roots, root) = setup("clean-readonly");
        let module = root.join("mod/pkg@v1");
        std::fs::create_dir_all(&module).unwrap();
        std::fs::write(module.join("go.mod"), b"module x").unwrap();
        std::fs::set_permissions(
            module.join("go.mod"),
            std::fs::Permissions::from_mode(0o444),
        )
        .unwrap();
        std::fs::set_permissions(&module, std::fs::Permissions::from_mode(0o555)).unwrap();
        let out = delete_item(
            &entry_item(&root, "mod"),
            &guard,
            &roots,
            &AtomicBool::new(false),
            &Progress::default(),
        );
        assert!(out.complete, "{:?}", out.skipped);
        assert!(!root.join("mod").exists());
    }

    #[test]
    fn keeps_young_files_and_refuses_roots() {
        let (_dir, guard, roots, root) = setup("clean-age");
        let logs = root.join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(logs.join("new.log"), b"new").unwrap();
        let old = logs.join("old.log");
        std::fs::write(&old, b"old").unwrap();
        let ten_days_ago =
            std::time::SystemTime::now() - std::time::Duration::from_secs(10 * 86_400);
        let times = std::fs::FileTimes::new().set_modified(ten_days_ago);
        #[cfg(target_os = "macos")]
        let times = std::os::macos::fs::FileTimesExt::set_created(times, ten_days_ago);
        #[cfg(windows)]
        let times = std::os::windows::fs::FileTimesExt::set_created(times, ten_days_ago);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_times(times)
            .unwrap();

        let mut item = entry_item(&root, "logs");
        item.min_age_days = 7;
        delete_item(
            &item,
            &guard,
            &roots,
            &AtomicBool::new(false),
            &Progress::default(),
        );
        assert!(logs.join("new.log").exists());
        assert!(!old.exists());

        let root_item = Item::new(
            Category::SystemJunk,
            "root",
            vec![root.clone()],
            root.clone(),
            Shape::Entry,
        );
        let out = delete_item(
            &root_item,
            &guard,
            &roots,
            &AtomicBool::new(false),
            &Progress::default(),
        );
        assert!(
            !out.skipped.is_empty() && root.exists(),
            "a root is never removed"
        );
    }

    #[test]
    fn refuses_items_that_changed() {
        let (_dir, guard, roots, root) = setup("clean-changed");
        std::fs::write(root.join("f"), b"one").unwrap();
        let mut item = entry_item(&root, "f");
        item.ident = Some(Ident {
            ino: 1,
            len: 3,
            mtime: 0,
            dir: true,
        });
        let out = delete_item(
            &item,
            &guard,
            &roots,
            &AtomicBool::new(false),
            &Progress::default(),
        );
        assert!(!out.skipped.is_empty());
        assert!(root.join("f").exists());
    }

    #[test]
    fn nested_items_follow_the_item_that_covers_them() {
        let job = |index: usize, path: &str, category: Category| Job {
            index,
            item: Item::new(
                category,
                path,
                vec![PathBuf::from(path)],
                PathBuf::from("/h"),
                Shape::Entry,
            ),
        };
        let jobs = [
            job(0, "/h/Downloads/old", Category::OldDownloads),
            job(1, "/h/Downloads/old/big.iso", Category::LargeFiles),
            job(2, "/h/a.bin", Category::LargeFiles),
            job(3, "/h/a.bin", Category::Duplicates),
        ];
        for parent_done in [true, false] {
            let mut outcomes = Vec::new();
            let (kept, followers) = normalize(
                jobs.iter()
                    .map(|j| Job {
                        index: j.index,
                        item: j.item.clone(),
                    })
                    .collect(),
                &[],
                &mut outcomes,
            );
            assert_eq!(kept.iter().map(|j| j.index).collect::<Vec<_>>(), [0, 2]);
            outcomes.push((
                0,
                Outcome {
                    complete: parent_done,
                    ..Outcome::default()
                },
            ));
            outcomes.push((
                2,
                Outcome {
                    complete: true,
                    ..Outcome::default()
                },
            ));
            resolve_followers(&kept, &mut outcomes, followers);
            let finished: HashSet<usize> = outcomes
                .iter()
                .filter(|(_, o)| o.complete)
                .map(|(i, _)| *i)
                .collect();
            assert_eq!(
                finished.contains(&1),
                parent_done,
                "big.iso follows its folder"
            );
            assert!(
                finished.contains(&3),
                "the same file listed twice follows the first"
            );
        }
    }

    #[test]
    fn keeps_one_copy_of_each_duplicate_group() {
        let job = |index: usize, name: &str, modified: i64| {
            let mut item = Item::new(
                Category::Duplicates,
                name,
                vec![PathBuf::from(format!("/x/{name}"))],
                PathBuf::from("/x"),
                Shape::Entry,
            );
            item.dupe_group = 1;
            item.modified = modified;
            Job { index, item }
        };
        let jobs = vec![job(0, "a", 20), job(1, "b", 10), job(2, "c", 30)];
        let files: Vec<GroupFile> = jobs
            .iter()
            .map(|j| GroupFile {
                group: 1,
                path: j.item.paths[0].clone(),
                modified: j.item.modified,
            })
            .collect();
        let mut outcomes = Vec::new();
        let (kept, _) = normalize(jobs, &files, &mut outcomes);
        let names: Vec<_> = kept.iter().map(|j| j.item.name.clone()).collect();
        assert_eq!(names, ["a", "c"], "the oldest copy (b) stays");
    }

    #[test]
    fn keeps_a_copy_that_another_category_selected() {
        let job = |index: usize, path: &str, category: Category| Job {
            index,
            item: Item::new(
                category,
                path,
                vec![PathBuf::from(path)],
                PathBuf::from("/h"),
                Shape::Entry,
            ),
        };
        let file = |path: &str, modified: i64| GroupFile {
            group: 1,
            path: PathBuf::from(path),
            modified,
        };
        // Returns the jobs that run, and the skipped jobs with their reason.
        let run = |jobs: Vec<Job>, files: &[GroupFile]| {
            let mut outcomes = Vec::new();
            let (left, _) = normalize(jobs, files, &mut outcomes);
            let skipped: Vec<(usize, String)> = outcomes
                .iter()
                .map(|(i, o)| (*i, o.skipped[0].1.clone()))
                .collect();
            (left.iter().map(|j| j.index).collect::<Vec<_>>(), skipped)
        };

        // The oldest copy is selected as an installer, the other one as a duplicate.
        let files = [
            file("/h/Downloads/a.dmg", 10),
            file("/h/Documents/a.dmg", 20),
        ];
        let jobs = vec![
            job(0, "/h/Downloads/a.dmg", Category::Installers),
            job(1, "/h/Documents/a.dmg", Category::Duplicates),
        ];
        assert_eq!(
            run(jobs, &files),
            (vec![1], vec![(0, "kept as the last copy".to_owned())])
        );

        // A selected Downloads folder holds the oldest copy: the other copy stays, so
        // the folder can still go.
        let files = [
            file("/h/Downloads/old/a.bin", 10),
            file("/h/Documents/a.bin", 20),
        ];
        let jobs = vec![
            job(0, "/h/Downloads/old", Category::OldDownloads),
            job(1, "/h/Documents/a.bin", Category::Duplicates),
        ];
        assert_eq!(
            run(jobs, &files),
            (vec![0], vec![(1, "kept as the last copy".to_owned())])
        );

        // The folder holds every copy, so the folder stays.
        let files = [
            file("/h/Downloads/old/a.bin", 10),
            file("/h/Downloads/old/b.bin", 20),
        ];
        let jobs = vec![job(0, "/h/Downloads/old", Category::OldDownloads)];
        assert_eq!(
            run(jobs, &files),
            (
                vec![],
                vec![(0, "holds the last copy of a duplicate".to_owned())]
            )
        );

        // A copy that nothing selects stays anyway: every job runs.
        let files = [
            file("/h/Downloads/a.dmg", 10),
            file("/h/Documents/a.dmg", 20),
        ];
        let jobs = vec![job(1, "/h/Documents/a.dmg", Category::Duplicates)];
        assert_eq!(run(jobs, &files), (vec![1], vec![]));
    }

    #[test]
    fn refuses_to_trash_a_folder_that_holds_a_never_touch_item() {
        let dir = tempdir("clean-never");
        let home = dir.join("home");
        let downloads = home.join("Downloads");
        let folder = downloads.join("old");
        std::fs::create_dir_all(folder.join("keep")).unwrap();
        let env = Env {
            home: home.clone(),
            temp: home.join("tmp"),
            downloads: downloads.clone(),
            local: home.join("Library"),
            cloud: Vec::new(),
        };
        let guard = Guard::new(&env, &[], &[folder.join("keep")]);
        let item = Item::new(
            Category::OldDownloads,
            "old",
            vec![folder.clone()],
            downloads,
            Shape::Entry,
        );
        // The refusal comes before any move to the Trash.
        let out = trash_item(&item, &guard);
        assert!(!out.complete);
        assert_eq!(out.skipped[0].1, "holds a Never touch item");
        assert!(folder.join("keep").exists());
    }
}

/// Cleans test data on this machine through the real rules, guard and Trash.
/// Run with `cargo test --release real_clean -- --ignored --nocapture`.
#[cfg(test)]
mod real {
    use super::*;
    use crate::rules::Category;
    use crate::scan::Ident;

    fn ident(path: &Path) -> Ident {
        let md = fs::symlink_metadata(path).unwrap();
        #[cfg(unix)]
        let ino = std::os::unix::fs::MetadataExt::ino(&md);
        #[cfg(not(unix))]
        let ino = 0;
        Ident {
            ino,
            len: md.len(),
            mtime: mtime_of(&md),
            dir: md.is_dir(),
        }
    }

    #[test]
    #[ignore]
    fn real_clean() {
        let env = platform::env().unwrap();
        // 1. A fake app cache, deleted for good.
        let caches = rules::resolve_dir(&env.local.join("Caches")).unwrap();
        let cache = caches.join(format!("{}.test", rules::APP_ID));
        fs::create_dir_all(cache.join("a/b")).unwrap();
        fs::write(cache.join("a/b/data.bin"), vec![1u8; 100_000]).unwrap();
        fs::write(cache.join("index"), b"x").unwrap();
        let mut junk = Item::new(
            Category::SystemJunk,
            "test cache",
            vec![cache.clone()],
            caches.clone(),
            Shape::Entry,
        );
        junk.ident = Some(ident(&cache));
        junk.size = 100_000;
        // 2. A user file, moved to the Trash.
        let file = env
            .home
            .join(format!("cleaner-trash-test-{}.bin", std::process::id()));
        fs::write(&file, vec![2u8; 4096]).unwrap();
        let mut user = Item::new(
            Category::LargeFiles,
            "test file",
            vec![file.clone()],
            env.home.clone(),
            Shape::Entry,
        );
        user.ident = Some(ident(&file));
        user.size = 4096;

        let jobs = vec![
            Job {
                index: 0,
                item: junk,
            },
            Job {
                index: 1,
                item: user,
            },
        ];
        let report = run(
            jobs,
            &[],
            false,
            &[],
            &Progress::default(),
            &AtomicBool::new(false),
        );
        println!("{report:#?}");
        assert!(report.refused.is_none());
        assert_eq!(report.finished, vec![0, 1]);
        assert!(!cache.exists(), "the test cache is gone");
        assert!(caches.exists(), "~/Library/Caches itself stays");
        assert!(!file.exists(), "the test file went to the Trash");
        // Undo: put the test file back and check its content.
        let moved: Vec<_> = report
            .moved
            .iter()
            .map(|(_, o, t)| (o.clone(), t.clone()))
            .collect();
        let back = platform::restore_from_trash(&moved, report.started);
        println!("restore: {back:?}");
        assert!(back.iter().all(|(_, r)| r.is_ok()), "{back:?}");
        assert_eq!(fs::read(&file).unwrap(), vec![2u8; 4096]);
        fs::remove_file(&file).unwrap();
        assert_eq!(report.trashed, 4096);
    }
}
