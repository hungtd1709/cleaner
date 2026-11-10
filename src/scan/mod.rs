//! A scan: junk targets first, then one pass over the home folder.

pub mod dupes;
#[cfg(target_os = "macos")]
mod list_macos;
pub mod walk;

use std::collections::HashSet;
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, mpsc};

use crate::fmt;
use crate::platform;
use crate::rules::{self, Action, Category, Env, Guard, Mode, Target};
use walk::{Entry, Kind};

/// The smallest "large file"; the menu filters upward from here without a new scan.
pub const LARGE_MIN: u64 = 100 * fmt::MB;
const LARGE_KEEP: usize = 5_000;
const OLD_DAYS: i64 = 90;
const DAY: i64 = 86_400;
const NONE: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Ok,
    /// Its app is running, so it starts unselected.
    Running,
    /// macOS blocked access; it needs Full Disk Access.
    NoAccess,
    /// It can be cleaned, but its size is unknown (the Trash without Full Disk Access).
    Unmeasured,
    /// Listed for information only. Never selectable.
    InfoOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    /// Cleaning empties each path but keeps the folder itself.
    Contents,
    /// Cleaning removes the path itself.
    Entry,
}

/// What the scan saw. Cleaning checks it again first (safety rule S3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ident {
    pub ino: u64,
    pub len: u64,
    pub mtime: i64,
    pub dir: bool,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub category: Category,
    pub name: String,
    /// A short label shown after the name, such as "Logs".
    pub group: String,
    pub paths: Vec<PathBuf>,
    /// The folder an `Entry` item belongs to. Cleaning never leaves it (rule S2).
    pub root: PathBuf,
    pub shape: Shape,
    pub min_age_days: u32,
    pub app_id: Option<String>,
    pub size: u64,
    pub files: u64,
    pub modified: i64,
    pub status: Status,
    pub selected: bool,
    pub ident: Option<Ident>,
    /// Duplicate group, starting at 1. 0 for other items.
    pub dupe_group: u32,
    pub size_text: String,
    pub when_text: String,
    pub path_text: String,
}

impl Item {
    pub fn new(
        category: Category,
        name: impl Into<String>,
        paths: Vec<PathBuf>,
        root: PathBuf,
        shape: Shape,
    ) -> Item {
        Item {
            category,
            name: name.into(),
            group: String::new(),
            paths,
            root,
            shape,
            min_age_days: 0,
            app_id: None,
            size: 0,
            files: 0,
            modified: 0,
            status: Status::Ok,
            selected: false,
            ident: None,
            dupe_group: 0,
            size_text: String::new(),
            when_text: String::new(),
            path_text: String::new(),
        }
    }

    pub fn action(&self) -> Action {
        self.category.action()
    }

    pub fn selectable(&self) -> bool {
        matches!(
            self.status,
            Status::Ok | Status::Running | Status::Unmeasured
        )
    }

    /// Formats the row text once.
    pub fn finish_text(&mut self, now: i64) {
        self.size_text = match self.status {
            Status::Unmeasured => "Size unknown".to_owned(),
            Status::NoAccess => "No access".to_owned(),
            _ => fmt::bytes(self.size),
        };
        self.when_text = fmt::ago(self.modified, now);
        self.path_text = match self.paths.as_slice() {
            [] => String::new(),
            [one] => fmt::path(one),
            [first, rest @ ..] => format!("{} and {} more", fmt::path(first), rest.len()),
        };
    }
}

/// Live totals of one junk item while the walk runs.
#[derive(Default)]
pub struct Live {
    pub size: AtomicU64,
    pub files: AtomicU64,
    pub newest: AtomicI64,
    pub denied: AtomicBool,
}

pub const STAGE_JUNK: u8 = 0;
pub const STAGE_HOME: u8 = 1;

#[derive(Default)]
pub struct Progress {
    pub files: AtomicU64,
    pub bytes: AtomicU64,
    pub stage: AtomicU8,
}

pub enum Msg {
    /// The junk items, sent before their folders are walked so the UI can show live sizes.
    Junk(Vec<Item>, Arc<Vec<Live>>),
    Done(Box<Output>),
    Cancelled,
}

pub struct Output {
    pub items: Vec<Item>,
    pub candidates: Vec<dupes::Cand>,
    pub fda: Option<bool>,
    pub notes: Vec<String>,
}

pub struct Handle {
    pub progress: Arc<Progress>,
    pub cancel: Arc<AtomicBool>,
    pub rx: mpsc::Receiver<Msg>,
}

/// Facts the UI thread collects before a scan starts.
pub struct Input {
    /// Running apps: bundle ids on macOS, lower-case `.exe` names on Windows.
    pub running: HashSet<String>,
    pub low_power: bool,
}

pub fn start(input: Input, repaint: impl Fn() + Send + 'static) -> Handle {
    let progress = Arc::new(Progress::default());
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let (p, c) = (progress.clone(), cancel.clone());
    std::thread::Builder::new()
        .name("scan".into())
        .spawn(move || {
            let send = |msg| {
                let _ = tx.send(msg);
                repaint();
            };
            run(&input, &p, &c, &send);
        })
        .expect("start the scan thread");
    Handle {
        progress,
        cancel,
        rx,
    }
}

#[derive(Clone, Copy)]
struct JunkCtx {
    item: usize,
    /// Files changed after this time are kept. 0 means no limit.
    cutoff: i64,
    /// The job lists one of the item's own roots.
    top: bool,
}

fn run(input: &Input, progress: &Progress, cancel: &AtomicBool, send: &dyn Fn(Msg)) {
    let _activity = platform::begin_activity("Scanning for files to clean");
    let Some(env) = platform::env() else {
        send(Msg::Done(Box::new(Output {
            items: Vec::new(),
            candidates: Vec::new(),
            fda: None,
            notes: vec!["Cleaner could not find your home folder.".to_owned()],
        })));
        return;
    };
    let targets = rules::targets(&env);
    let guard = Guard::new(&env, &targets);
    let claimed: HashSet<PathBuf> = targets
        .iter()
        .flat_map(|t| t.roots.iter().cloned())
        .collect();
    let fda = platform::full_disk_access(&env);
    let threads = platform::worker_count(input.low_power);
    let now = fmt::now();
    let links = Mutex::new(HashSet::<u64>::new());

    // 1. Junk: one item per `Whole` target, one per child of a `Children` target.
    let mut items = Vec::new();
    let mut jobs = Vec::new();
    for t in &targets {
        add_target(t, &claimed, &guard, now, &mut items, &mut jobs);
    }
    let live: Arc<Vec<Live>> = Arc::new(
        items
            .iter()
            .map(|it: &Item| Live {
                size: AtomicU64::new(it.size),
                files: AtomicU64::new(it.files),
                newest: AtomicI64::new(it.modified),
                denied: AtomicBool::new(false),
            })
            .collect(),
    );
    send(Msg::Junk(items.clone(), live.clone()));

    walk::walk(jobs, threads, cancel, |dir, ctx: JunkCtx, listing, push| {
        let entries = match listing {
            Ok(entries) => entries,
            Err(e) => {
                if ctx.top && e.kind() == io::ErrorKind::PermissionDenied {
                    live[ctx.item].denied.store(true, Relaxed);
                }
                return;
            }
        };
        let (mut size, mut files, mut newest) = (0u64, 0u64, 0i64);
        for e in entries {
            match e.kind {
                Kind::Dir if !e.mount && !e.cloud => {
                    let path = dir.join(&e.name);
                    if !claimed.contains(&path) && !guard.hard_denied(&path) {
                        push.push((path, JunkCtx { top: false, ..ctx }));
                    }
                }
                Kind::File if !e.cloud => {
                    if ctx.cutoff != 0 && e.newest() > ctx.cutoff {
                        continue;
                    }
                    if e.nlink > 1
                        && !links
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .insert(e.ino)
                    {
                        continue;
                    }
                    size += e.alloc;
                    files += 1;
                    newest = newest.max(e.mtime);
                }
                _ => {}
            }
        }
        let l = &live[ctx.item];
        l.size.fetch_add(size, Relaxed);
        l.files.fetch_add(files, Relaxed);
        l.newest.fetch_max(newest, Relaxed);
        progress.files.fetch_add(files, Relaxed);
        progress.bytes.fetch_add(size, Relaxed);
    });
    if cancel.load(Relaxed) {
        send(Msg::Cancelled);
        return;
    }
    for (item, l) in items.iter_mut().zip(live.iter()) {
        item.size = l.size.load(Relaxed);
        item.files = l.files.load(Relaxed);
        item.modified = item.modified.max(l.newest.load(Relaxed));
        if l.denied.load(Relaxed) {
            item.status = if item.category == Category::Trash {
                Status::Unmeasured
            } else {
                Status::NoAccess
            };
        }
        if item
            .app_id
            .as_ref()
            .is_some_and(|id| input.running.contains(id))
            && item.status == Status::Ok
        {
            item.status = Status::Running;
        }
        if item.status != Status::Ok && item.status != Status::Unmeasured {
            item.selected = false;
        }
    }
    items.retain(|it| {
        it.size > 0 || it.status == Status::Unmeasured || it.status == Status::NoAccess
    });

    // 2. One pass over the home folder.
    progress.stage.store(STAGE_HOME, Relaxed);
    #[allow(unused_mut)]
    let mut notes = Vec::new();
    #[cfg(target_os = "macos")]
    notes.extend(touch_private_folders(&env));
    let home = Home::new(&env, &guard, &claimed, &links, progress);
    let mut roots = vec![(env.home.clone(), Ctx::ROOT)];
    if !env.downloads.starts_with(&env.home) && env.downloads.is_dir() {
        roots.push((env.downloads.clone(), Ctx::ROOT));
    }
    walk::walk(roots, threads, cancel, |dir, ctx, listing, push| {
        home.visit(dir, ctx, listing, push)
    });
    if cancel.load(Relaxed) {
        send(Msg::Cancelled);
        return;
    }
    let candidates = std::mem::take(&mut *home.cands.lock().unwrap_or_else(|p| p.into_inner()));
    items.extend(home.into_items(now));
    for item in &mut items {
        item.finish_text(now);
    }
    send(Msg::Done(Box::new(Output {
        items,
        candidates,
        fda,
        notes,
    })));
}

fn add_target(
    t: &Target,
    claimed: &HashSet<PathBuf>,
    guard: &Guard,
    now: i64,
    items: &mut Vec<Item>,
    jobs: &mut Vec<(PathBuf, JunkCtx)>,
) {
    let cutoff = if t.min_age_days > 0 {
        now - i64::from(t.min_age_days) * DAY
    } else {
        0
    };
    match t.mode {
        Mode::Whole => {
            let mut item = Item::new(
                t.category,
                t.name.clone(),
                t.roots.clone(),
                PathBuf::new(),
                Shape::Contents,
            );
            item.app_id = t.app_id.clone();
            item.selected = t.selected;
            if t.roots.is_empty() {
                // The Windows Recycle Bin: the platform layer measures it.
                match platform::recycle_bin() {
                    Some((size, count)) if count > 0 => {
                        item.size = size;
                        item.files = count;
                    }
                    _ => return,
                }
            }
            let index = items.len();
            items.push(item);
            for root in &t.roots {
                jobs.push((
                    root.clone(),
                    JunkCtx {
                        item: index,
                        cutoff,
                        top: true,
                    },
                ));
            }
        }
        Mode::Children => {
            let Some(root) = t.roots.first() else { return };
            let Ok(entries) = walk::list(root) else {
                return;
            };
            for e in entries {
                let path = root.join(&e.name);
                if claimed.contains(&path)
                    || !t.filter.allows(&e.name)
                    || guard.hard_denied(&path)
                    || rules::CACHE_DENY.iter().any(|d| e.name == **d)
                {
                    continue;
                }
                let name = e.name.to_string_lossy().into_owned();
                let mut item = Item::new(
                    t.category,
                    name.clone(),
                    vec![path.clone()],
                    root.clone(),
                    Shape::Entry,
                );
                item.group = t.name.clone();
                item.min_age_days = t.min_age_days;
                item.selected = t.selected;
                item.app_id = if cfg!(target_os = "macos") {
                    Some(name)
                } else {
                    t.app_id.clone()
                };
                item.ident = Some(Ident {
                    ino: e.ino,
                    len: e.len,
                    mtime: e.mtime,
                    dir: e.kind == Kind::Dir,
                });
                match e.kind {
                    Kind::Dir if !e.mount && !e.cloud => {
                        jobs.push((
                            path,
                            JunkCtx {
                                item: items.len(),
                                cutoff,
                                top: true,
                            },
                        ));
                        items.push(item);
                    }
                    Kind::File if !e.cloud => {
                        if cutoff != 0 && e.newest() > cutoff {
                            continue;
                        }
                        item.size = e.alloc;
                        item.files = 1;
                        item.modified = e.mtime;
                        items.push(item);
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Opens Desktop, Documents and Downloads one at a time before the parallel walk, so
/// macOS asks for permission once per folder instead of from several threads at once.
#[cfg(target_os = "macos")]
fn touch_private_folders(env: &Env) -> Vec<String> {
    let mut notes = Vec::new();
    for name in ["Desktop", "Documents", "Downloads"] {
        if let Err(e) = std::fs::read_dir(env.home.join(name))
            && e.kind() == io::ErrorKind::PermissionDenied
        {
            notes.push(format!(
                "Cleaner cannot see your {name} folder. Allow it in System Settings → Privacy & Security → Files and Folders."
            ));
        }
    }
    notes
}

/// Where a file's size goes during the home pass.
#[derive(Clone, Copy)]
struct Ctx {
    /// The top-level Downloads item this folder is in, or NONE.
    dl: u32,
    /// The build folder or bundle this folder is in, or NONE.
    unit: u32,
}

impl Ctx {
    const ROOT: Ctx = Ctx {
        dl: NONE,
        unit: NONE,
    };
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BucketKind {
    Download,
    Build,
    Bundle,
}

/// A folder whose files are counted as one item.
struct Bucket {
    kind: BucketKind,
    path: PathBuf,
    size: u64,
    files: u64,
    newest: i64,
    ident: Ident,
    info_only: bool,
}

struct FileHit {
    path: PathBuf,
    size: u64,
    newest: i64,
    ident: Ident,
    info_only: bool,
}

#[derive(Default)]
struct Sums {
    size: u64,
    files: u64,
    newest: i64,
}

impl Sums {
    fn add(&mut self, size: u64, time: i64) {
        self.size += size;
        self.files += 1;
        self.newest = self.newest.max(time);
    }
}

struct Home<'a> {
    env: &'a Env,
    guard: &'a Guard,
    claimed: &'a HashSet<PathBuf>,
    links: &'a Mutex<HashSet<u64>>,
    progress: &'a Progress,
    buckets: Mutex<Vec<Bucket>>,
    large: Mutex<Vec<FileHit>>,
    old_files: Mutex<Vec<FileHit>>,
    cands: Mutex<Vec<dupes::Cand>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn dot_name(name: &OsStr) -> bool {
    name.as_encoded_bytes().first() == Some(&b'.')
}

/// True if `dir/CACHEDIR.TAG` starts with the standard signature.
fn cachedir_tag(dir: &Path) -> bool {
    use std::io::Read;
    const SIG: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55";
    let mut head = [0u8; SIG.len()];
    std::fs::File::open(dir.join("CACHEDIR.TAG"))
        .and_then(|mut f| f.read_exact(&mut head))
        .is_ok()
        && head == SIG
}

impl<'a> Home<'a> {
    fn new(
        env: &'a Env,
        guard: &'a Guard,
        claimed: &'a HashSet<PathBuf>,
        links: &'a Mutex<HashSet<u64>>,
        progress: &'a Progress,
    ) -> Self {
        Home {
            env,
            guard,
            claimed,
            links,
            progress,
            buckets: Mutex::new(Vec::new()),
            large: Mutex::new(Vec::new()),
            old_files: Mutex::new(Vec::new()),
            cands: Mutex::new(Vec::new()),
        }
    }

    fn bucket(
        &self,
        kind: BucketKind,
        path: PathBuf,
        newest: i64,
        ident: Ident,
        info_only: bool,
    ) -> u32 {
        let mut b = lock(&self.buckets);
        b.push(Bucket {
            kind,
            path,
            size: 0,
            files: 0,
            newest,
            ident,
            info_only,
        });
        (b.len() - 1) as u32
    }

    fn visit(
        &self,
        dir: &Path,
        mut ctx: Ctx,
        listing: io::Result<Vec<Entry>>,
        push: &mut Vec<(PathBuf, Ctx)>,
    ) {
        let Ok(entries) = listing else { return };
        let at_home = dir == self.env.home;
        let at_downloads = dir == self.env.downloads;
        // A folder with a CACHEDIR.TAG is a cache that tools re-create, such as Rust's `target`.
        if ctx.unit == NONE
            && !at_home
            && !at_downloads
            && entries
                .iter()
                .any(|e| e.kind == Kind::File && e.name == "CACHEDIR.TAG")
            && cachedir_tag(dir)
        {
            let mtime = std::fs::symlink_metadata(dir).map_or(0, |m| mtime_of(&m));
            let ident = Ident {
                ino: 0,
                len: 0,
                mtime,
                dir: true,
            };
            ctx.unit = self.bucket(BucketKind::Build, dir.to_path_buf(), mtime, ident, false);
        }
        let package_json = entries
            .iter()
            .any(|e| e.kind == Kind::File && e.name == "package.json");
        let (mut unit, mut dl) = (Sums::default(), Sums::default());
        let (mut large, mut old, mut cands) = (Vec::new(), Vec::new(), Vec::new());
        for e in &entries {
            match e.kind {
                Kind::Dir => {
                    if e.mount || e.cloud || rules::WALK_SKIP.iter().any(|s| e.name == **s) {
                        continue;
                    }
                    if at_home && platform::HOME_SKIP.iter().any(|s| e.name == **s) {
                        continue;
                    }
                    let path = dir.join(&e.name);
                    if self.claimed.contains(&path)
                        || self.guard.walk_denied(&path)
                        || self.env.cloud.contains(&path)
                    {
                        continue;
                    }
                    let ident = Ident {
                        ino: e.ino,
                        len: 0,
                        mtime: e.mtime,
                        dir: true,
                    };
                    let info_only = rules::ext_in(&e.name, rules::INFO_ONLY_EXTS);
                    let mut child = ctx;
                    // Hidden entries such as `.DS_Store` are Finder's, not downloads.
                    if at_downloads && !dot_name(&e.name) {
                        child.dl = self.bucket(
                            BucketKind::Download,
                            path.clone(),
                            e.newest(),
                            ident,
                            info_only,
                        );
                    }
                    if child.unit == NONE {
                        if cfg!(target_os = "macos") && rules::ext_in(&e.name, rules::BUNDLE_EXTS) {
                            child.unit = self.bucket(
                                BucketKind::Bundle,
                                path.clone(),
                                e.mtime,
                                ident,
                                info_only,
                            );
                        } else if e.name == "node_modules" && package_json {
                            child.unit =
                                self.bucket(BucketKind::Build, path.clone(), e.mtime, ident, false);
                        }
                    }
                    push.push((path, child));
                }
                Kind::File => {
                    if e.cloud || e.system {
                        continue;
                    }
                    if e.nlink > 1 && !lock(self.links).insert(e.ino) {
                        continue;
                    }
                    self.progress.files.fetch_add(1, Relaxed);
                    self.progress.bytes.fetch_add(e.alloc, Relaxed);
                    if ctx.unit != NONE {
                        unit.add(e.alloc, e.mtime);
                    }
                    if ctx.dl != NONE {
                        dl.add(e.alloc, e.newest());
                    }
                    let ident = Ident {
                        ino: e.ino,
                        len: e.len,
                        mtime: e.mtime,
                        dir: false,
                    };
                    let info_only = rules::ext_in(&e.name, rules::INFO_ONLY_EXTS);
                    if at_downloads && !dot_name(&e.name) {
                        old.push(FileHit {
                            path: dir.join(&e.name),
                            size: e.alloc,
                            newest: e.newest(),
                            ident,
                            info_only,
                        });
                    }
                    if ctx.unit == NONE {
                        if e.alloc.max(e.len) >= LARGE_MIN {
                            large.push(FileHit {
                                path: dir.join(&e.name),
                                size: e.alloc,
                                newest: e.mtime,
                                ident,
                                info_only,
                            });
                        }
                        if e.len >= dupes::MIN_SIZE && !dot_name(&e.name) && !info_only {
                            cands.push(dupes::Cand {
                                path: dir.join(&e.name),
                                len: e.len,
                                ino: e.ino,
                                mtime: e.mtime,
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        if unit.files > 0 || dl.files > 0 {
            let mut b = lock(&self.buckets);
            for (index, sums) in [(ctx.unit, &unit), (ctx.dl, &dl)] {
                if index != NONE && sums.files > 0 {
                    let bucket = &mut b[index as usize];
                    bucket.size += sums.size;
                    bucket.files += sums.files;
                    bucket.newest = bucket.newest.max(sums.newest);
                }
            }
        }
        if !large.is_empty() {
            lock(&self.large).append(&mut large);
        }
        if !old.is_empty() {
            lock(&self.old_files).append(&mut old);
        }
        if !cands.is_empty() {
            lock(&self.cands).append(&mut cands);
        }
    }

    /// The walk root an item belongs to (rule S2): home, or Downloads if it lives elsewhere.
    fn root_of(&self, path: &Path) -> PathBuf {
        if path.starts_with(&self.env.home) {
            self.env.home.clone()
        } else {
            self.env.downloads.clone()
        }
    }

    fn into_items(self, now: i64) -> Vec<Item> {
        let mut items = Vec::new();
        let user_item = |category: Category, hit: &FileHit, root: PathBuf| {
            let name = hit
                .path
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            let mut item = Item::new(category, name, vec![hit.path.clone()], root, Shape::Entry);
            item.size = hit.size;
            item.files = 1;
            item.modified = hit.newest;
            item.ident = Some(hit.ident);
            item.status = if hit.info_only {
                Status::InfoOnly
            } else {
                Status::Ok
            };
            item
        };
        let buckets = std::mem::take(&mut *lock(&self.buckets));
        for b in &buckets {
            let hit = FileHit {
                path: b.path.clone(),
                size: b.size,
                newest: b.newest,
                ident: b.ident,
                info_only: b.info_only,
            };
            match b.kind {
                BucketKind::Build if b.files > 0 => {
                    let project = b
                        .path
                        .parent()
                        .and_then(Path::file_name)
                        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                    let folder = b
                        .path
                        .file_name()
                        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                    let mut item = user_item(Category::BuildFolders, &hit, self.root_of(&b.path));
                    item.name = format!("{project}/{folder}");
                    item.files = b.files;
                    item.status = Status::Ok;
                    items.push(item);
                }
                BucketKind::Bundle if b.size >= LARGE_MIN => {
                    let mut item = user_item(Category::LargeFiles, &hit, self.root_of(&b.path));
                    item.files = b.files;
                    items.push(item);
                }
                BucketKind::Download if now - b.newest >= OLD_DAYS * DAY => {
                    let mut item =
                        user_item(Category::OldDownloads, &hit, self.env.downloads.clone());
                    item.files = b.files;
                    items.push(item);
                }
                _ => {}
            }
        }
        let mut large = std::mem::take(&mut *lock(&self.large));
        for hit in &mut large {
            hit.size = platform::refine_alloc(&hit.path, hit.size);
        }
        large.retain(|h| h.size >= LARGE_MIN);
        large.sort_by_key(|h| std::cmp::Reverse(h.size));
        large.truncate(LARGE_KEEP);
        for hit in &large {
            items.push(user_item(
                Category::LargeFiles,
                hit,
                self.root_of(&hit.path),
            ));
        }
        for hit in lock(&self.old_files).iter() {
            if now - hit.newest >= OLD_DAYS * DAY {
                items.push(user_item(
                    Category::OldDownloads,
                    hit,
                    self.env.downloads.clone(),
                ));
            }
        }
        items
    }
}

pub fn mtime_of(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A full, read-only scan of this machine. Run it with
    /// `cargo test --release real_scan -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_scan() {
        let input = Input {
            running: platform::running_apps(),
            low_power: platform::low_power(),
        };
        let progress = Progress::default();
        let output = Mutex::new(None);
        let started = std::time::Instant::now();
        run(&input, &progress, &AtomicBool::new(false), &|msg| {
            if let Msg::Done(out) = msg {
                *output.lock().unwrap() = Some(out);
            }
        });
        let elapsed = started.elapsed();
        let out = output.into_inner().unwrap().expect("scan finished");
        println!(
            "scan: {:.2?}, {} files, {} seen",
            elapsed,
            fmt::count(progress.files.load(Relaxed)),
            fmt::bytes(progress.bytes.load(Relaxed))
        );
        println!(
            "full disk access: {:?}; candidates for duplicates: {}",
            out.fda,
            out.candidates.len()
        );
        for cat in Category::ALL {
            let items: Vec<&Item> = out.items.iter().filter(|i| i.category == cat).collect();
            let size: u64 = items.iter().map(|i| i.size).sum();
            let selected: u64 = items.iter().filter(|i| i.selected).map(|i| i.size).sum();
            println!(
                "{:<18} {:>5} items  {:>10}  (selected {})",
                cat.name(),
                items.len(),
                fmt::bytes(size),
                fmt::bytes(selected)
            );
            let mut top = items.clone();
            top.sort_by_key(|i| std::cmp::Reverse(i.size));
            for it in top.iter().take(3) {
                println!(
                    "      {:>10}  {:?}  {}",
                    it.size_text, it.status, it.path_text
                );
            }
            for it in items.iter().filter(|i| !matches!(i.status, Status::Ok)) {
                println!("      [{:?}] {}", it.status, it.path_text);
            }
        }
        for note in &out.notes {
            println!("note: {note}");
        }
    }
}
