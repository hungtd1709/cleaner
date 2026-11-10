//! A scan: junk targets first, then one pass over the home folder.

pub mod dupes;
#[cfg(target_os = "macos")]
mod list_macos;
pub mod walk;
pub mod zip;

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
/// A cache that was written to this recently belongs to an app that is still open.
const IN_USE_SECS: i64 = 15 * 60;
const NONE: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Ok,
    /// Its app is running or was just writing to it, so it starts unselected.
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
    /// A short warning shown with the item.
    pub note: String,
    pub paths: Vec<PathBuf>,
    /// The folder an `Entry` item belongs to. Cleaning never leaves it (rule S2).
    pub root: PathBuf,
    pub shape: Shape,
    pub min_age_days: u32,
    pub app_id: Option<String>,
    /// The item counts as in use if a file in it changed in the last few minutes.
    pub watch_recent: bool,
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
    /// Lower-case name and path, for the search box.
    pub search: String,
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
            note: String::new(),
            paths,
            root,
            shape,
            min_age_days: 0,
            app_id: None,
            watch_recent: false,
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
            search: String::new(),
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
        self.search = format!("{}\n{}", self.name, self.path_text).to_lowercase();
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

/// Folder sizes for the Space map: folders in home, each with its own child folders.
#[derive(Clone, Debug, Default)]
pub struct MapEntry {
    pub path: PathBuf,
    pub size: u64,
    pub files: u64,
    pub children: Vec<MapEntry>,
}

pub struct Output {
    pub items: Vec<Item>,
    pub candidates: Vec<dupes::Cand>,
    pub fda: Option<bool>,
    pub notes: Vec<String>,
    /// Folders in home, biggest first. `~/Library` and `AppData` are measured later.
    pub map: Vec<MapEntry>,
    /// Bytes in files that sit directly in the home folder.
    pub home_files: u64,
}

pub struct Handle {
    pub progress: Arc<Progress>,
    pub cancel: Arc<AtomicBool>,
    pub rx: mpsc::Receiver<Msg>,
}

/// Facts the UI thread collects before a scan starts.
pub struct Input {
    /// Running apps, lower case: bundle ids and names on macOS, `.exe` names on Windows.
    pub running: HashSet<String>,
    pub low_power: bool,
    /// The user's Never touch list.
    pub never: Vec<PathBuf>,
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
    /// The item the files count for, or NONE when only the folder size is wanted.
    item: u32,
    /// The cleaning root the files are in; its full size feeds the Space map.
    root: u32,
    /// Files changed after this time are kept. 0 means no limit.
    cutoff: i64,
    /// The job lists one of the item's own roots.
    top: bool,
}

/// The junk side of a scan: items, and the walk jobs that measure them.
struct Junk {
    items: Vec<Item>,
    jobs: Vec<(PathBuf, JunkCtx)>,
    /// Every cleaning root and its full size, whatever the age filter counts.
    roots: Vec<PathBuf>,
    root_sizes: Vec<u64>,
}

fn run(input: &Input, progress: &Progress, cancel: &AtomicBool, send: &dyn Fn(Msg)) {
    let _activity = platform::begin_activity("Scanning for files to clean");
    let Some(env) = platform::env() else {
        send(Msg::Done(Box::new(Output {
            items: Vec::new(),
            candidates: Vec::new(),
            fda: None,
            notes: vec!["Cleaner could not find your home folder.".to_owned()],
            map: Vec::new(),
            home_files: 0,
        })));
        return;
    };
    let targets = rules::targets(&env);
    let guard = Guard::new(&env, &targets, &input.never);
    let claimed: HashSet<PathBuf> = targets
        .iter()
        .flat_map(|t| t.roots.iter().cloned())
        .collect();
    let fda = platform::full_disk_access(&env);
    let threads = platform::worker_count(input.low_power);
    let now = fmt::now();
    let links = Mutex::new(HashSet::<u64>::new());

    // 1. Junk: one item per `Whole` target, one per child of a `Children` target.
    let mut junk = Junk {
        items: Vec::new(),
        jobs: Vec::new(),
        roots: Vec::new(),
        root_sizes: Vec::new(),
    };
    for t in &targets {
        add_target(t, &claimed, &guard, now, &mut junk);
    }
    let Junk {
        mut items,
        jobs,
        roots,
        root_sizes,
    } = junk;
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
    let root_totals: Vec<AtomicU64> = root_sizes.into_iter().map(AtomicU64::new).collect();
    send(Msg::Junk(items.clone(), live.clone()));

    walk::walk(jobs, threads, cancel, |dir, ctx: JunkCtx, listing, push| {
        let entries = match listing {
            Ok(entries) => entries,
            Err(e) => {
                if ctx.top && ctx.item != NONE && e.kind() == io::ErrorKind::PermissionDenied {
                    live[ctx.item as usize].denied.store(true, Relaxed);
                }
                return;
            }
        };
        let (mut size, mut files, mut newest, mut all) = (0u64, 0u64, 0i64, 0u64);
        for e in entries {
            match e.kind {
                Kind::Dir if !e.mount && !e.cloud => {
                    let path = dir.join(&e.name);
                    if !claimed.contains(&path) && !guard.hard_denied(&path) {
                        push.push((path, JunkCtx { top: false, ..ctx }));
                    }
                }
                Kind::File if !e.cloud => {
                    all += e.alloc;
                    if ctx.item == NONE || (ctx.cutoff != 0 && e.newest() > ctx.cutoff) {
                        continue;
                    }
                    if e.nlink > 1 && !lock(&links).insert(e.ino) {
                        continue;
                    }
                    size += e.alloc;
                    files += 1;
                    newest = newest.max(e.mtime);
                }
                _ => {}
            }
        }
        root_totals[ctx.root as usize].fetch_add(all, Relaxed);
        if ctx.item != NONE {
            let l = &live[ctx.item as usize];
            l.size.fetch_add(size, Relaxed);
            l.files.fetch_add(files, Relaxed);
            l.newest.fetch_max(newest, Relaxed);
        }
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
        let running = item
            .app_id
            .as_ref()
            .is_some_and(|id| input.running.contains(&id.to_lowercase()));
        let busy = item.watch_recent && item.modified > now - IN_USE_SECS;
        if (running || busy) && item.status == Status::Ok {
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
    let mut walk_roots = vec![(env.home.clone(), Ctx::ROOT)];
    if !env.downloads.starts_with(&env.home) && env.downloads.is_dir() {
        walk_roots.push((env.downloads.clone(), Ctx::ROOT));
    }
    walk::walk(walk_roots, threads, cancel, |dir, ctx, listing, push| {
        home.visit(dir, ctx, listing, push)
    });
    if cancel.load(Relaxed) {
        send(Msg::Cancelled);
        return;
    }
    let candidates = std::mem::take(&mut *lock(&home.cands));
    let totals: Vec<(PathBuf, u64)> = roots
        .into_iter()
        .zip(root_totals.iter().map(|t| t.load(Relaxed)))
        .collect();
    let (map, home_files) = home.take_map(&totals);
    items.extend(home.into_items(now));
    // The Trash takes a folder as a whole, so a folder that holds a Never touch item
    // cannot go there (rule S12).
    items
        .retain(|it| it.action() != Action::Trash || !it.paths.iter().any(|p| guard.holds_hard(p)));
    for item in &mut items {
        item.finish_text(now);
    }
    send(Msg::Done(Box::new(Output {
        items,
        candidates,
        fda,
        notes,
        map,
        home_files,
    })));
}

fn add_target(t: &Target, claimed: &HashSet<PathBuf>, guard: &Guard, now: i64, junk: &mut Junk) {
    let cutoff = if t.min_age_days > 0 {
        now - i64::from(t.min_age_days) * DAY
    } else {
        0
    };
    let new_root = |junk: &mut Junk, path: &Path| {
        junk.roots.push(path.to_path_buf());
        junk.root_sizes.push(0);
        (junk.roots.len() - 1) as u32
    };
    let item_for = |name: String, paths: Vec<PathBuf>, root: PathBuf, shape: Shape| {
        let mut item = Item::new(t.category, name, paths, root, shape);
        item.app_id = t.app_id.clone();
        item.selected = t.selected;
        item.min_age_days = t.min_age_days;
        item.note = t.note.to_owned();
        item.watch_recent = t.watch_recent;
        item
    };
    match t.mode {
        Mode::Whole => {
            let roots: Vec<PathBuf> = t
                .roots
                .iter()
                .filter(|r| !guard.hard_denied(r))
                .cloned()
                .collect();
            let mut item = item_for(
                t.name.clone(),
                roots.clone(),
                PathBuf::new(),
                Shape::Contents,
            );
            if t.roots.is_empty() {
                // The Windows Recycle Bin: the platform layer measures it.
                match platform::recycle_bin() {
                    Some((size, count)) if count > 0 => {
                        item.size = size;
                        item.files = count;
                    }
                    _ => return,
                }
            } else if roots.is_empty() {
                return;
            }
            let index = junk.items.len() as u32;
            junk.items.push(item);
            for root in &roots {
                let r = new_root(junk, root);
                junk.jobs.push((
                    root.clone(),
                    JunkCtx {
                        item: index,
                        root: r,
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
            let r = new_root(junk, root);
            for e in entries {
                let path = root.join(&e.name);
                if claimed.contains(&path)
                    || !t.filter.allows(&e.name)
                    || guard.hard_denied(&path)
                    || rules::CACHE_DENY.iter().any(|d| e.name == **d)
                {
                    continue;
                }
                // Kept children (such as the newest version) are only measured.
                if t.keep.contains(&path) {
                    match e.kind {
                        Kind::Dir if !e.mount && !e.cloud => junk.jobs.push((
                            path,
                            JunkCtx {
                                item: NONE,
                                root: r,
                                cutoff: 0,
                                top: true,
                            },
                        )),
                        Kind::File => junk.root_sizes[r as usize] += e.alloc,
                        _ => {}
                    }
                    continue;
                }
                let name = e.name.to_string_lossy().into_owned();
                let mut item =
                    item_for(name.clone(), vec![path.clone()], root.clone(), Shape::Entry);
                item.group = t.name.clone();
                if t.app_id.is_none() && cfg!(target_os = "macos") {
                    item.app_id = Some(name);
                }
                item.ident = Some(Ident {
                    ino: e.ino,
                    len: e.len,
                    mtime: e.mtime,
                    dir: e.kind == Kind::Dir,
                });
                match e.kind {
                    Kind::Dir if !e.mount && !e.cloud => {
                        junk.jobs.push((
                            path,
                            JunkCtx {
                                item: junk.items.len() as u32,
                                root: r,
                                cutoff,
                                top: true,
                            },
                        ));
                        junk.items.push(item);
                    }
                    Kind::File if !e.cloud => {
                        junk.root_sizes[r as usize] += e.alloc;
                        if cutoff != 0 && e.newest() > cutoff {
                            continue;
                        }
                        item.size = e.alloc;
                        item.files = 1;
                        item.modified = e.mtime;
                        junk.items.push(item);
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

// Scope flags of the home pass. Files under any of these are not the user's own files.
/// Inside a hidden (dot) folder, such as `~/.rustup` or `project/.venv`.
const HIDDEN: u8 = 1;
/// Inside a Git work tree below home.
const REPO: u8 = 2;
/// Inside an environment or dependency folder (`pyvenv.cfg`, `site-packages`, …).
const ENV: u8 = 4;
/// Inside a tool install in home, such as `~/go` or `~/miniconda3`.
const TOOL: u8 = 8;
/// Inside a `.git` folder: measured for the Space map, nothing else.
const MAP_ONLY: u8 = 16;

/// Where a file's size goes during the home pass, and what it may become.
#[derive(Clone, Copy)]
struct Ctx {
    /// The top-level Downloads item this folder is in, or NONE.
    dl: u32,
    /// The build folder or bundle this folder is in, or NONE.
    unit: u32,
    /// Space map: the folder in home this folder is in, or NONE.
    map1: u32,
    /// Space map: the folder two levels below home this folder is in, or NONE.
    map2: u32,
    flags: u8,
}

impl Ctx {
    const ROOT: Ctx = Ctx {
        dl: NONE,
        unit: NONE,
        map1: NONE,
        map2: NONE,
        flags: 0,
    };

    fn has(self, flags: u8) -> bool {
        self.flags & flags != 0
    }
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

/// A Space map folder being measured.
struct MapNode {
    path: PathBuf,
    parent: u32,
    size: u64,
    files: u64,
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
    installers: Mutex<Vec<FileHit>>,
    cands: Mutex<Vec<dupes::Cand>>,
    map: Mutex<Vec<MapNode>>,
    home_files: AtomicU64,
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

/// When the project around a build folder last changed: the newest of its other entries.
fn project_time(entries: &[Entry], build: &OsStr) -> i64 {
    entries
        .iter()
        .filter(|e| e.name != build)
        .map(|e| e.mtime)
        .max()
        .unwrap_or(0)
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
            installers: Mutex::new(Vec::new()),
            cands: Mutex::new(Vec::new()),
            map: Mutex::new(Vec::new()),
            home_files: AtomicU64::new(0),
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

    fn map_node(&self, path: PathBuf, parent: u32) -> u32 {
        let mut m = lock(&self.map);
        m.push(MapNode {
            path,
            parent,
            size: 0,
            files: 0,
        });
        (m.len() - 1) as u32
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
        let map_only = ctx.has(MAP_ONLY);
        if !map_only && !at_home {
            if entries.iter().any(|e| e.name == ".git") {
                ctx.flags |= REPO;
            }
            if entries
                .iter()
                .any(|e| e.kind == Kind::File && e.name == "pyvenv.cfg")
            {
                ctx.flags |= ENV;
            }
        }
        // A folder with a CACHEDIR.TAG is a cache that tools re-create, such as Rust's `target`.
        // Build folders and bundles count only in the user's own folders: a `node_modules`
        // inside a VS Code extension or a tool install is part of that tool.
        let own_scope = !ctx.has(HIDDEN | ENV | TOOL);
        if !map_only
            && own_scope
            && ctx.unit == NONE
            && !at_home
            && !at_downloads
            && entries
                .iter()
                .any(|e| e.kind == Kind::File && e.name == "CACHEDIR.TAG")
            && cachedir_tag(dir)
        {
            let own = std::fs::symlink_metadata(dir).map_or(0, |m| mtime_of(&m));
            let project = match (dir.parent(), dir.file_name()) {
                (Some(parent), Some(name)) => {
                    walk::list(parent).map_or(0, |e| project_time(&e, name))
                }
                _ => 0,
            };
            let ident = Ident {
                ino: 0,
                len: 0,
                mtime: own,
                dir: true,
            };
            ctx.unit = self.bucket(
                BucketKind::Build,
                dir.to_path_buf(),
                own.max(project),
                ident,
                false,
            );
        }
        let package_json = entries
            .iter()
            .any(|e| e.kind == Kind::File && e.name == "package.json");
        let (mut unit, mut dl) = (Sums::default(), Sums::default());
        let (mut map1, mut map2) = (Sums::default(), Sums::default());
        let (mut large, mut old, mut installers, mut cands) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for e in &entries {
            match e.kind {
                Kind::Dir => {
                    if e.mount || e.cloud {
                        continue;
                    }
                    let git = e.name == ".git";
                    if !git && rules::WALK_SKIP.iter().any(|s| e.name == **s) {
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
                    let mut child = ctx;
                    if git {
                        child.flags |= MAP_ONLY;
                    }
                    if dot_name(&e.name) {
                        child.flags |= HIDDEN;
                    }
                    if rules::ENV_DIRS.iter().any(|d| e.name == **d) {
                        child.flags |= ENV;
                    }
                    if at_home && rules::TOOL_ROOTS.iter().any(|d| e.name == **d) {
                        child.flags |= TOOL;
                    }
                    if at_home {
                        child.map1 = self.map_node(path.clone(), NONE);
                        child.map2 = NONE;
                    } else if ctx.map1 != NONE && ctx.map2 == NONE {
                        child.map2 = self.map_node(path.clone(), ctx.map1);
                    }
                    if !child.has(MAP_ONLY) {
                        let ident = Ident {
                            ino: e.ino,
                            len: 0,
                            mtime: e.mtime,
                            dir: true,
                        };
                        let info_only = rules::ext_in(&e.name, rules::INFO_ONLY_EXTS);
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
                        if child.unit == NONE && own_scope {
                            if cfg!(target_os = "macos")
                                && rules::ext_in(&e.name, rules::BUNDLE_EXTS)
                            {
                                child.unit = self.bucket(
                                    BucketKind::Bundle,
                                    path.clone(),
                                    e.mtime,
                                    ident,
                                    info_only,
                                );
                            } else if e.name == "node_modules" && package_json {
                                let changed = e.mtime.max(project_time(&entries, &e.name));
                                child.unit = self.bucket(
                                    BucketKind::Build,
                                    path.clone(),
                                    changed,
                                    ident,
                                    false,
                                );
                            }
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
                    if ctx.map1 != NONE {
                        map1.add(e.alloc, 0);
                    }
                    if ctx.map2 != NONE {
                        map2.add(e.alloc, 0);
                    }
                    if at_home {
                        self.home_files.fetch_add(e.alloc, Relaxed);
                    }
                    if map_only {
                        continue;
                    }
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
                    let hit = |size: u64, newest: i64| FileHit {
                        path: dir.join(&e.name),
                        size,
                        newest,
                        ident,
                        info_only,
                    };
                    if at_downloads && !dot_name(&e.name) {
                        old.push(hit(e.alloc, e.newest()));
                        let installer = rules::ext_in(&e.name, rules::INSTALLER_EXTS)
                            || rules::is_setup_exe(&e.name)
                            || (rules::ext_in(&e.name, &["zip"])
                                && zip::is_installer(&dir.join(&e.name)));
                        if installer {
                            installers.push(hit(e.alloc, e.newest()));
                        }
                    }
                    if ctx.unit == NONE {
                        let own_file = !ctx.has(HIDDEN | ENV | TOOL);
                        if own_file && e.alloc.max(e.len) >= LARGE_MIN {
                            large.push(hit(e.alloc, e.mtime));
                        }
                        // Sparse files (allocated less than half their length) are disk
                        // images and the like; identical bytes there are not copies.
                        let sparse = e.alloc.saturating_mul(2) < e.len;
                        if own_file
                            && !ctx.has(REPO)
                            && e.len >= dupes::MIN_SIZE
                            && !dot_name(&e.name)
                            && !info_only
                            && !sparse
                        {
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
        if map1.files > 0 {
            let mut m = lock(&self.map);
            for (index, sums) in [(ctx.map1, &map1), (ctx.map2, &map2)] {
                if index != NONE && sums.files > 0 {
                    m[index as usize].size += sums.size;
                    m[index as usize].files += sums.files;
                }
            }
        }
        let never = |hits: &mut Vec<FileHit>| hits.retain(|h| !self.guard.hard_denied(&h.path));
        for (list, target) in [
            (&mut large, &self.large),
            (&mut old, &self.old_files),
            (&mut installers, &self.installers),
        ] {
            if !list.is_empty() {
                never(list);
                lock(target).append(list);
            }
        }
        if !cands.is_empty() {
            cands.retain(|c| !self.guard.hard_denied(&c.path));
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

    /// The Space map of home. The home pass skipped cleaning roots, so their full sizes,
    /// measured by the junk pass, are added to the folders that hold them.
    fn take_map(&self, roots: &[(PathBuf, u64)]) -> (Vec<MapEntry>, u64) {
        let nodes = std::mem::take(&mut *lock(&self.map));
        let mut map: Vec<MapEntry> = Vec::new();
        let mut index_of: Vec<Option<usize>> = vec![None; nodes.len()];
        for (i, n) in nodes.iter().enumerate() {
            if n.parent == NONE {
                index_of[i] = Some(map.len());
                map.push(MapEntry {
                    path: n.path.clone(),
                    size: n.size,
                    files: n.files,
                    children: Vec::new(),
                });
            }
        }
        for n in &nodes {
            if n.parent != NONE
                && let Some(Some(top)) = index_of.get(n.parent as usize)
            {
                map[*top].children.push(MapEntry {
                    path: n.path.clone(),
                    size: n.size,
                    files: n.files,
                    children: Vec::new(),
                });
            }
        }
        let home = &self.env.home;
        for (root, size) in roots {
            let Ok(rel) = root.strip_prefix(home) else {
                continue;
            };
            let mut parts = rel.components();
            let Some(first) = parts.next() else { continue };
            if platform::HOME_SKIP.iter().any(|s| first.as_os_str() == *s) {
                continue;
            }
            let top_path = home.join(first);
            let top = match map.iter().position(|e| e.path == top_path) {
                Some(i) => i,
                None => {
                    map.push(MapEntry {
                        path: top_path,
                        ..MapEntry::default()
                    });
                    map.len() - 1
                }
            };
            map[top].size += size;
            if let Some(second) = parts.next() {
                let child_path = map[top].path.join(second);
                let children = &mut map[top].children;
                match children.iter_mut().find(|c| c.path == child_path) {
                    Some(c) => c.size += size,
                    None => children.push(MapEntry {
                        path: child_path,
                        size: *size,
                        ..MapEntry::default()
                    }),
                }
            }
        }
        for entry in &mut map {
            entry.children.sort_by_key(|c| std::cmp::Reverse(c.size));
        }
        map.sort_by_key(|e| std::cmp::Reverse(e.size));
        (map, self.home_files.load(Relaxed))
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
            if self.guard.hard_denied(&b.path) {
                continue;
            }
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
        for hit in lock(&self.installers).iter() {
            items.push(user_item(
                Category::Installers,
                hit,
                self.env.downloads.clone(),
            ));
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
    use std::fs;

    /// Runs the home pass over a fake home folder.
    fn home_pass(home: &Path) -> (Vec<dupes::Cand>, Vec<Item>, Vec<MapEntry>) {
        let env = Env {
            home: home.to_path_buf(),
            temp: home.join("tmp"),
            downloads: home.join("Downloads"),
            local: home.join("Library"),
            cloud: Vec::new(),
        };
        let guard = Guard::new(&env, &[], &[home.join("Keep")]);
        let claimed = HashSet::new();
        let links = Mutex::new(HashSet::new());
        let progress = Progress::default();
        let pass = Home::new(&env, &guard, &claimed, &links, &progress);
        walk::walk(
            vec![(home.to_path_buf(), Ctx::ROOT)],
            2,
            &AtomicBool::new(false),
            |d, c, l, p| pass.visit(d, c, l, p),
        );
        let cands = std::mem::take(&mut *lock(&pass.cands));
        let (map, _) = pass.take_map(&[]);
        (cands, pass.into_items(fmt::now()), map)
    }

    #[test]
    fn duplicate_candidates_are_only_the_users_own_files() {
        let home = crate::rules::tests::tempdir("scope").join("home");
        let big = vec![7u8; 2 * 1024 * 1024];
        for p in [
            "Documents/a.bin",
            "Pictures/b.bin",
            ".rustup/toolchains/x/lib.dylib",
            "proj/.venv/lib/x.so",
            "env/lib/y.so",
            "repo/data.bin",
            "go/pkg/z.bin",
            "Keep/k.bin",
        ] {
            let path = home.join(p);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, &big).unwrap();
        }
        fs::write(home.join("env/pyvenv.cfg"), b"home = /usr/bin").unwrap();
        fs::create_dir_all(home.join("repo/.git")).unwrap();
        fs::create_dir_all(home.join("Downloads")).unwrap();
        fs::write(home.join("Downloads/App-1.0.dmg"), b"x").unwrap();
        fs::write(home.join("Downloads/notes.txt"), b"x").unwrap();
        for project in ["code/app", ".vscode/extensions/ext-1.0"] {
            fs::create_dir_all(home.join(project).join("node_modules/dep")).unwrap();
            fs::write(home.join(project).join("package.json"), b"{}").unwrap();
            fs::write(home.join(project).join("node_modules/dep/index.js"), b"x").unwrap();
        }

        let (cands, items, map) = home_pass(&home);
        let mut names: Vec<String> = cands
            .iter()
            .map(|c| {
                c.path
                    .strip_prefix(&home)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        assert_eq!(names, ["Documents/a.bin", "Pictures/b.bin"]);

        let installers: Vec<&str> = items
            .iter()
            .filter(|i| i.category == Category::Installers)
            .map(|i| i.name.as_str())
            .collect();
        assert_eq!(installers, ["App-1.0.dmg"]);

        let builds: Vec<&str> = items
            .iter()
            .filter(|i| i.category == Category::BuildFolders)
            .map(|i| i.name.as_str())
            .collect();
        assert_eq!(
            builds,
            ["app/node_modules"],
            "an extension's node_modules is not a build folder"
        );

        // The Space map still measures hidden folders, but never the Never touch list.
        assert!(
            map.iter()
                .any(|e| e.path.ends_with(".rustup") && e.size >= big.len() as u64)
        );
        assert!(!map.iter().any(|e| e.path.ends_with("Keep")));
    }

    /// Find duplicates on this machine, read-only. Checks the scope rules and spot-checks
    /// groups byte by byte. Run with
    /// `cargo test --release real_duplicates -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_duplicates() {
        use std::io::Read;
        let input = Input {
            running: HashSet::new(),
            low_power: false,
            never: Vec::new(),
        };
        let output = Mutex::new(None);
        run(
            &input,
            &Progress::default(),
            &AtomicBool::new(false),
            &|msg| {
                if let Msg::Done(out) = msg {
                    *output.lock().unwrap() = Some(out);
                }
            },
        );
        let out = output.into_inner().unwrap().expect("scan finished");
        let home = platform::env().unwrap().home;
        let started = std::time::Instant::now();
        let groups = dupes::find(
            out.candidates.clone(),
            fmt::now(),
            &AtomicBool::new(false),
            &dupes::Progress::default(),
        );
        println!(
            "{} candidates, {} groups in {:.1?}",
            out.candidates.len(),
            groups.len(),
            started.elapsed()
        );
        for g in groups.iter().take(12) {
            println!("  {} x {}", g.len(), fmt::bytes(g[0].len));
            for c in g {
                println!("      {}", fmt::path(&c.path));
            }
        }
        for c in groups.iter().flatten() {
            let rel = c.path.strip_prefix(&home).unwrap_or(&c.path);
            assert!(
                !rel.components()
                    .any(|p| p.as_os_str().to_string_lossy().starts_with('.')),
                "inside a hidden folder: {:?}",
                c.path
            );
            assert!(
                !c.path
                    .ancestors()
                    .skip(1)
                    .take_while(|a| *a != home)
                    .any(|a| a.join(".git").exists()),
                "inside a Git project: {:?}",
                c.path
            );
        }
        let same = |a: &Path, b: &Path| {
            let (mut fa, mut fb) = (fs::File::open(a).unwrap(), fs::File::open(b).unwrap());
            let (mut x, mut y) = (vec![0u8; 1 << 20], vec![0u8; 1 << 20]);
            loop {
                let n = fa.read(&mut x).unwrap();
                let m = fb.read(&mut y).unwrap();
                if n != m || x[..n] != y[..m] {
                    return false;
                }
                if n == 0 {
                    return true;
                }
            }
        };
        for g in groups.iter().take(5) {
            assert!(same(&g[0].path, &g[1].path), "not identical: {g:?}");
        }
    }

    /// A full, read-only scan of this machine. Run it with
    /// `cargo test --release real_scan -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_scan() {
        let input = Input {
            running: platform::running_apps(),
            low_power: platform::low_power(),
            never: Vec::new(),
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
            for it in top
                .iter()
                .take(if cat == Category::AppData { 30 } else { 3 })
            {
                println!(
                    "      {:>10}  {:?}  {}  {}",
                    it.size_text, it.status, it.name, it.note
                );
            }
        }
        println!("space map (top 8):");
        for e in out.map.iter().take(8) {
            println!("      {:>10}  {}", fmt::bytes(e.size), fmt::path(&e.path));
        }
        for note in &out.notes {
            println!("note: {note}");
        }
    }
}
