//! The window: header, sidebar, item lists, Space map, footer and dialogs.
//!
//! The app redraws only on input, and every 250 ms while background work runs.
//! Row text is formatted once, when results arrive.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use eframe::egui::{
    self, Align, Align2, Button, Checkbox, Color32, ComboBox, CornerRadius, FontId, Frame, Id, Key,
    Label, Layout, Margin, Modal, Rect, RichText, ScrollArea, Sense, Stroke, TextEdit,
    TextureHandle, Ui, pos2, vec2,
};
use egui_extras::{Column, TableBuilder};

use crate::clean;
use crate::fmt;
use crate::platform::{self, Disk};
use crate::rules::{Action, CATEGORY_COUNT, Category};
use crate::scan::{self, Item, Shape, Status, dupes};
use crate::settings::{Settings, Sort, SortKey};
use crate::space;
use crate::theme::{
    self, ACCENT, ACCENT_2, BG, BORDER, DANGER, MUTED, ON_ACCENT, PANEL, RAISED, TEXT, WARNING,
};

const SIDEBAR_WIDTH: f32 = 252.0;
const ROW_HEIGHT: f32 = 42.0;
const BUSY_REPAINT: Duration = Duration::from_millis(250);
const THRESHOLDS: [u64; 5] = [
    100 * fmt::MB,
    250 * fmt::MB,
    500 * fmt::MB,
    fmt::GB,
    5 * fmt::GB,
];
/// Build folders of projects untouched this long count as idle.
const IDLE_DAYS: i64 = 30;
/// Why the Never touch list cannot grow right now.
const BUSY_NEVER: &str = "Wait until cleaning or the duplicate search ends.";
/// Categories that the home pass fills; their size is unknown until it ends.
const HOME_CATEGORIES: [Category; 5] = [
    Category::BuildFolders,
    Category::LargeFiles,
    Category::OldDownloads,
    Category::Installers,
    Category::Duplicates,
];

enum Phase {
    Idle,
    Scanning(scan::Handle),
    Cleaning(clean::Handle),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Space,
    Category(Category),
}

#[derive(Clone, Copy)]
enum Row {
    Item(usize),
    /// A duplicate group header; the index points into `group_labels`.
    Group(usize),
}

#[derive(Default, Clone)]
struct Summary {
    total: u64,
    /// Items that "Select all" may select (not running, not blocked) in the whole
    /// category. The sidebar checkbox counts these.
    bulk: u64,
    bulk_selected: u64,
    /// The same, among the rows that match the search box. The list's buttons count these.
    shown: u64,
    shown_selected: u64,
    text: String,
}

impl Summary {
    /// What a click on the category's sidebar checkbox does: select (true) or clear.
    /// Rule S10 keeps one copy of each duplicate group, so Duplicates is never all
    /// selected: a click there clears any selection, or selects all but the oldest.
    fn click_selects(&self, cat: Category) -> bool {
        if cat == Category::Duplicates {
            self.bulk_selected == 0
        } else {
            !(self.bulk > 0 && self.bulk_selected == self.bulk)
        }
    }
}

struct Plan {
    delete: (u64, u64),
    trash: (u64, u64),
    empty: Option<u64>,
}

type RestoreResult = Vec<(PathBuf, Result<(), String>)>;

/// Items the last clean moved to the Trash, ready to be put back.
struct Undo {
    items: Vec<(Item, PathBuf, Option<PathBuf>)>,
    since: i64,
    running: Option<mpsc::Receiver<RestoreResult>>,
    result: Option<String>,
}

/// One row of the Space map, formatted once per level.
struct SpaceRow {
    path: PathBuf,
    name: String,
    size: u64,
    size_text: String,
    fraction: f32,
    note: String,
    note_color: Color32,
    /// Can be opened: a measured folder that is not protected.
    open: bool,
}

#[derive(Default)]
struct SpaceState {
    levels: HashMap<PathBuf, space::Level>,
    /// The breadcrumb: home first, the folder on screen last.
    stack: Vec<PathBuf>,
    measuring: Option<space::Measure>,
    /// A folder to open as soon as it is measured.
    pending: Option<PathBuf>,
    rows: Vec<SpaceRow>,
    rows_for: Option<PathBuf>,
    /// Sizes are from before the last clean.
    stale: bool,
}

pub struct CleanerApp {
    icon: Option<TextureHandle>,
    /// Home and Downloads as the last scan found them. Finding them can be slow, so the
    /// scan thread does it and the window never waits for it.
    home: Option<PathBuf>,
    downloads: Option<PathBuf>,
    disk: Option<(Disk, String)>,
    disk_rx: Option<mpsc::Receiver<Option<Disk>>>,
    phase: Phase,
    live: Option<(Vec<Item>, Arc<Vec<scan::Live>>)>,
    scanned: bool,
    items: Vec<Item>,
    candidates: Vec<dupes::Cand>,
    /// Every file of every duplicate group, as the search found it, until the next search.
    /// Rows leave the list, and files leave the disk or come back from the Trash, but
    /// cleaning checks each file before rule S10 counts it.
    dupe_files: Vec<clean::GroupFile>,
    finder: Option<dupes::Handle>,
    dupes_done: bool,
    view: View,
    rows: Vec<Row>,
    group_labels: Vec<String>,
    /// The oldest copy in each duplicate group.
    oldest: HashSet<usize>,
    summary: [Summary; CATEGORY_COUNT],
    chosen_count: usize,
    chosen_text: String,
    clean_label: String,
    fda: Option<bool>,
    elevated: bool,
    low_power: bool,
    notes: Vec<String>,
    plan: Option<Plan>,
    report: Option<clean::Report>,
    undo: Option<Undo>,
    settings: Settings,
    settings_open: bool,
    never_input: String,
    /// Why the path typed in Settings was not added.
    never_error: Option<&'static str>,
    /// The path typed in Settings, being resolved on a short-lived thread: a path on a
    /// network share can take long. `None` inside means it does not exist.
    never_check: Option<mpsc::Receiver<Option<PathBuf>>>,
    query: String,
    /// The clicked row; Space previews it.
    current: Option<usize>,
    space: SpaceState,
    dirty: bool,
}

impl CleanerApp {
    pub fn new(cc: &eframe::CreationContext<'_>, icon: Option<egui::IconData>) -> Self {
        theme::apply(&cc.egui_ctx);
        let icon = icon.map(|i| {
            let image = egui::ColorImage::from_rgba_unmultiplied(
                [i.width as usize, i.height as usize],
                &i.rgba,
            );
            cc.egui_ctx
                .load_texture("app-icon", image, egui::TextureOptions::LINEAR)
        });
        let mut app = CleanerApp {
            icon,
            ..CleanerApp::blank(Settings::load())
        };
        app.probe_disk(&cc.egui_ctx);
        #[cfg(feature = "autoscan")]
        {
            if let Ok(view) = std::env::var("CLEANER_VIEW") {
                if view.eq_ignore_ascii_case("space map") {
                    app.view = View::Space;
                } else if let Some(cat) = Category::ALL
                    .into_iter()
                    .find(|c| c.name().eq_ignore_ascii_case(&view))
                {
                    app.view = View::Category(cat);
                }
            }
            app.start_scan(&cc.egui_ctx);
        }
        app
    }

    /// The app before its first scan, with no window: `new` adds the window's parts, and
    /// tests use it as it is.
    fn blank(settings: Settings) -> Self {
        CleanerApp {
            icon: None,
            home: None,
            downloads: None,
            disk: None,
            disk_rx: None,
            phase: Phase::Idle,
            live: None,
            scanned: false,
            items: Vec::new(),
            candidates: Vec::new(),
            dupe_files: Vec::new(),
            finder: None,
            dupes_done: false,
            view: View::Category(Category::SystemJunk),
            rows: Vec::new(),
            group_labels: Vec::new(),
            oldest: HashSet::new(),
            summary: Default::default(),
            chosen_count: 0,
            chosen_text: String::new(),
            clean_label: "Clean".to_owned(),
            fda: None,
            elevated: platform::is_elevated(),
            low_power: false,
            notes: Vec::new(),
            plan: None,
            report: None,
            undo: None,
            settings,
            settings_open: false,
            never_input: String::new(),
            never_error: None,
            never_check: None,
            query: String::new(),
            current: None,
            space: SpaceState::default(),
            dirty: true,
        }
    }

    fn cat(&self) -> Option<Category> {
        match self.view {
            View::Category(c) => Some(c),
            View::Space => None,
        }
    }

    /// Measures the disk on a short-lived thread; the call can take a moment on macOS.
    fn probe_disk(&mut self, ctx: &egui::Context) {
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(platform::system_disk());
            ctx.request_repaint();
        });
        self.disk_rx = Some(rx);
    }

    fn busy(&self) -> bool {
        !matches!(self.phase, Phase::Idle)
            || self.finder.is_some()
            || self.space.measuring.is_some()
            || self.undo.as_ref().is_some_and(|u| u.running.is_some())
            || self.never_check.is_some()
    }

    /// No background work that changes the list is running or about to: the list's
    /// rows keep their positions, which a running clean reports by.
    fn idle(&self) -> bool {
        matches!(self.phase, Phase::Idle) && self.finder.is_none() && self.never_check.is_none()
    }

    // ---- background results -------------------------------------------------------

    fn poll(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.disk_rx
            && let Ok(disk) = rx.try_recv()
        {
            self.disk = disk.map(|d| {
                let text = format!(
                    "{}  ·  {} free of {}",
                    d.name,
                    fmt::bytes(d.free),
                    fmt::bytes(d.total)
                );
                (d, text)
            });
            self.disk_rx = None;
        }

        let (mut done, mut cancelled) = (None, false);
        if let Phase::Scanning(handle) = &self.phase {
            while let Ok(msg) = handle.rx.try_recv() {
                match msg {
                    scan::Msg::Junk(items, live) => self.live = Some((items, live)),
                    scan::Msg::Done(output) => done = Some(output),
                    scan::Msg::Cancelled => cancelled = true,
                }
            }
        }
        if let Some(output) = done {
            self.finish_scan(*output, ctx);
        } else if cancelled {
            self.phase = Phase::Idle;
            self.live = None;
        }

        let report = match &self.phase {
            Phase::Cleaning(handle) => handle.rx.try_recv().ok(),
            _ => None,
        };
        if let Some(report) = report {
            self.apply_report(report, ctx);
        }

        let found = self.finder.as_ref().and_then(|f| f.rx.try_recv().ok());
        if let Some(groups) = found {
            self.finder = None;
            if let Some(groups) = groups {
                self.add_duplicates(groups);
            }
        }

        let restored = self
            .undo
            .as_ref()
            .and_then(|u| u.running.as_ref())
            .and_then(|rx| rx.try_recv().ok());
        if let Some(results) = restored {
            self.apply_restore(results);
        }

        let checked = self.never_check.as_ref().map(|rx| rx.try_recv());
        match checked {
            Some(Ok(path)) => {
                self.never_check = None;
                match path {
                    Some(path) => {
                        self.never_touch(&[path]);
                        self.never_input.clear();
                    }
                    None => {
                        self.never_error =
                            Some("Paste the full path of a folder or file that exists.");
                    }
                }
            }
            Some(Err(mpsc::TryRecvError::Disconnected)) => self.never_check = None,
            Some(Err(mpsc::TryRecvError::Empty)) | None => {}
        }

        let measured = self
            .space
            .measuring
            .as_ref()
            .and_then(|m| m.rx.try_recv().ok());
        if let Some(level) = measured {
            self.space.measuring = None;
            self.space_add_level(level);
            // Only while the map is on screen, so the app stays idle elsewhere.
            if self.view == View::Space {
                self.measure_next(ctx);
            }
        }

        if self.dirty {
            self.recompute();
        }
    }

    fn start_scan(&mut self, ctx: &egui::Context) {
        let input = scan::Input {
            running: platform::running_apps(),
            low_power: platform::low_power(),
            never: self.settings.never.clone(),
        };
        self.low_power = input.low_power;
        let ctx = ctx.clone();
        self.phase = Phase::Scanning(scan::start(input, move || ctx.request_repaint()));
        self.items.clear();
        self.candidates.clear();
        self.dupe_files.clear();
        self.finder = None;
        self.dupes_done = false;
        self.report = None;
        self.undo = None;
        self.current = None;
        if let Some(m) = self.space.measuring.take() {
            m.cancel.store(true, Relaxed);
        }
        self.space = SpaceState::default();
        self.dirty = true;
    }

    fn finish_scan(&mut self, output: scan::Output, ctx: &egui::Context) {
        self.phase = Phase::Idle;
        self.live = None;
        self.scanned = true;
        self.items = output.items;
        self.candidates = output.candidates;
        self.fda = output.fda;
        self.notes = output.notes;
        self.home = output.env.as_ref().map(|e| e.home.clone());
        self.downloads = output.env.as_ref().map(|e| e.downloads.clone());
        if let Some(env) = &output.env {
            let home = &env.home;
            let skipped: Vec<PathBuf> = platform::HOME_SKIP.iter().map(|s| home.join(s)).collect();
            self.space.levels =
                space::from_scan(home, &output.map, output.home_files, &skipped, &env.cloud);
            self.space.stack = vec![home.clone()];
        }
        if self.view == View::Space {
            self.open_space(ctx);
        }
        self.dirty = true;
    }

    fn find_duplicates(&mut self, ctx: &egui::Context) {
        let ctx = ctx.clone();
        self.finder = Some(dupes::start(
            self.candidates.clone(),
            fmt::now(),
            move || ctx.request_repaint(),
        ));
    }

    fn add_duplicates(&mut self, groups: Vec<Vec<dupes::Cand>>) {
        self.items.retain(|it| it.category != Category::Duplicates);
        self.dupe_files.clear();
        let now = fmt::now();
        for (g, group) in groups.into_iter().enumerate() {
            for c in group {
                // The walk root the file came from (rule S2): home, or Downloads outside it.
                let root = match (&self.home, &self.downloads) {
                    (Some(home), _) if c.path.starts_with(home) => home.clone(),
                    (_, Some(downloads)) => downloads.clone(),
                    _ => PathBuf::new(),
                };
                let name = c
                    .path
                    .file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                let mut item = Item::new(
                    Category::Duplicates,
                    name,
                    vec![c.path.clone()],
                    root,
                    Shape::Entry,
                );
                item.size = c.len;
                item.files = 1;
                item.modified = c.mtime;
                item.dupe_group = g as u32 + 1;
                let ident = scan::Ident {
                    ino: c.ino,
                    len: c.len,
                    mtime: c.mtime,
                    dir: false,
                };
                item.ident = Some(ident);
                item.finish_text(now);
                self.dupe_files.push(clean::GroupFile {
                    group: item.dupe_group,
                    path: c.path.clone(),
                    modified: c.mtime,
                    ident,
                });
                self.items.push(item);
            }
        }
        self.dupes_done = true;
        self.dirty = true;
    }

    fn start_clean(&mut self, ctx: &egui::Context) {
        let jobs = self
            .chosen()
            .into_iter()
            .map(|i| clean::Job {
                index: i,
                item: self.items[i].clone(),
            })
            .collect();
        // Every duplicate, listed or not: cleaning keeps one copy of each group even when
        // another category selected the others (rule S10).
        let group_files = self.dupe_files.clone();
        let ctx = ctx.clone();
        self.phase = Phase::Cleaning(clean::start(
            jobs,
            group_files,
            self.low_power,
            self.settings.never.clone(),
            move || ctx.request_repaint(),
        ));
    }

    fn apply_report(&mut self, report: clean::Report, ctx: &egui::Context) {
        self.phase = Phase::Idle;
        let now = fmt::now();
        // Remember what went to the Trash before the rows disappear.
        let moved: Vec<(Item, PathBuf, Option<PathBuf>)> = report
            .moved
            .iter()
            .filter_map(|(i, original, in_trash)| {
                let mut item = self.items.get(*i)?.clone();
                item.selected = false;
                Some((item, original.clone(), in_trash.clone()))
            })
            .collect();
        self.undo = (!moved.is_empty()).then(|| Undo {
            items: moved,
            since: report.started,
            running: None,
            result: None,
        });
        for &(i, removed) in &report.partial {
            if let Some(it) = self.items.get_mut(i) {
                it.size = it.size.saturating_sub(removed);
                it.selected = false;
                it.finish_text(now);
            }
        }
        // Rule S10 would refuse these again, so they no longer count as selected.
        for &i in &report.kept {
            if let Some(it) = self.items.get_mut(i) {
                it.selected = false;
            }
        }
        let mut gone: HashSet<usize> = report.finished.iter().copied().collect();
        refill_trash_rows(&mut self.items, &mut gone, report.trashed, now);
        let gone_paths: HashSet<PathBuf> = gone
            .iter()
            .filter_map(|&i| self.items.get(i))
            .filter(|it| it.shape == Shape::Entry)
            .map(|it| it.paths[0].clone())
            .collect();
        let mut index = 0;
        self.items.retain(|_| {
            let keep = !gone.contains(&index);
            index += 1;
            keep
        });
        // The same file can be listed twice (a large file that is also a duplicate), or sit
        // inside a cleaned folder (a duplicate inside an old Downloads folder).
        let gone_too = |p: &Path| p.ancestors().any(|a| gone_paths.contains(a));
        self.items
            .retain(|it| it.shape != Shape::Entry || !gone_too(&it.paths[0]));
        self.candidates.retain(|c| !gone_too(&c.path));
        self.drop_lonely_duplicates();
        self.current = None;
        self.space.stale = true;
        self.report = Some(report);
        self.dirty = true;
        self.probe_disk(ctx);
    }

    /// A duplicate group with one file left is no longer a group.
    fn drop_lonely_duplicates(&mut self) {
        let mut left: HashMap<u32, usize> = HashMap::new();
        for it in self
            .items
            .iter()
            .filter(|it| it.category == Category::Duplicates)
        {
            *left.entry(it.dupe_group).or_default() += 1;
        }
        self.items
            .retain(|it| it.category != Category::Duplicates || left[&it.dupe_group] > 1);
    }

    fn start_restore(&mut self, ctx: &egui::Context) {
        let Some(undo) = &mut self.undo else { return };
        if undo.running.is_some() {
            return;
        }
        let paths = undo
            .items
            .iter()
            .map(|(_, original, in_trash)| (original.clone(), in_trash.clone()))
            .collect();
        let ctx = ctx.clone();
        undo.running = Some(clean::restore(paths, undo.since, move || {
            ctx.request_repaint()
        }));
    }

    fn apply_restore(&mut self, results: RestoreResult) {
        let Some(mut undo) = self.undo.take() else {
            return;
        };
        undo.running = None;
        let ok: HashSet<PathBuf> = results
            .iter()
            .filter(|(_, r)| r.is_ok())
            .map(|(p, _)| p.clone())
            .collect();
        let now = fmt::now();
        let mut back = 0;
        let mut back_bytes = 0;
        let mut kept = Vec::new();
        for (mut item, original, in_trash) in undo.items {
            if ok.contains(&original) {
                back_bytes += item.size;
                item.finish_text(now);
                self.items.push(item);
                back += 1;
            } else {
                kept.push((item, original, in_trash));
            }
        }
        // What came back left the Trash, so emptying it now deletes less.
        for it in self
            .items
            .iter_mut()
            .filter(|it| it.category == Category::Trash && it.status != Status::Unmeasured)
        {
            it.size = it.size.saturating_sub(back_bytes);
            it.finish_text(now);
        }
        let first_error = results.iter().find_map(|(_, r)| r.as_ref().err().cloned());
        undo.result = Some(match first_error {
            None => format!("Put back {}.", fmt::items(back)),
            Some(why) => format!(
                "Put back {} of {}. {why}",
                fmt::count(back),
                fmt::items(results.len() as u64)
            ),
        });
        undo.items = kept;
        self.undo = Some(undo);
        // A duplicate that comes back alone has no copy left in the list.
        self.drop_lonely_duplicates();
        self.current = None;
        self.dirty = true;
    }

    // ---- selection --------------------------------------------------------------------

    fn visible(&self, it: &Item) -> bool {
        it.category != Category::LargeFiles || it.size >= self.settings.large_limit
    }

    fn matches(&self, it: &Item) -> bool {
        self.query.is_empty() || it.search.contains(&self.query.to_lowercase())
    }

    /// Selected items to clean: nested and repeated paths are dropped.
    fn chosen(&self) -> Vec<usize> {
        let picked: Vec<usize> = (0..self.items.len())
            .filter(|&i| {
                let it = &self.items[i];
                it.selected && it.selectable() && self.visible(it)
            })
            .collect();
        let entries: HashSet<&Path> = picked
            .iter()
            .map(|&i| &self.items[i])
            .filter(|it| it.shape == Shape::Entry)
            .map(|it| it.paths[0].as_path())
            .collect();
        let mut seen = HashSet::new();
        picked
            .into_iter()
            .filter(|&i| {
                let it = &self.items[i];
                if it.shape != Shape::Entry {
                    return true;
                }
                let path = it.paths[0].as_path();
                !path.ancestors().skip(1).any(|a| entries.contains(a)) && seen.insert(path)
            })
            .collect()
    }

    fn recompute(&mut self) {
        self.dirty = false;
        // The Space map notes say how much the lists can clean, so they follow the lists.
        self.space.rows_for = None;
        let mut summary: [Summary; CATEGORY_COUNT] = Default::default();
        let mut groups: HashMap<u32, (u64, u64)> = HashMap::new();
        for it in &self.items {
            if !self.visible(it) {
                continue;
            }
            let s = &mut summary[it.category.index()];
            if it.category == Category::Duplicates {
                let g = groups.entry(it.dupe_group).or_default();
                g.0 += 1;
                g.1 = it.size;
            } else if matches!(it.status, Status::Ok | Status::Running) {
                s.total += it.size;
            }
            if matches!(it.status, Status::Ok | Status::Unmeasured) {
                s.bulk += 1;
                s.bulk_selected += u64::from(it.selected);
                if self.matches(it) {
                    s.shown += 1;
                    s.shown_selected += u64::from(it.selected);
                }
            }
        }
        summary[Category::Duplicates.index()].total = groups
            .values()
            .map(|&(n, size)| size * n.saturating_sub(1))
            .sum();
        for (i, s) in summary.iter_mut().enumerate() {
            let has_items = self.items.iter().any(|it| it.category.index() == i);
            s.text = if s.total > 0 {
                fmt::bytes(s.total)
            } else if has_items {
                "—".to_owned()
            } else {
                String::new()
            };
        }
        self.summary = summary;

        let chosen = self.chosen();
        let size: u64 = chosen.iter().map(|&i| self.items[i].size).sum();
        self.chosen_count = chosen.len();
        self.chosen_text = if chosen.is_empty() {
            "Nothing selected".to_owned()
        } else {
            format!(
                "Selected {}  ·  {}",
                fmt::bytes(size),
                fmt::items(chosen.len() as u64)
            )
        };
        self.clean_label = if size > 0 {
            format!("Clean {}", fmt::bytes(size))
        } else {
            "Clean".to_owned()
        };

        self.rows.clear();
        self.group_labels.clear();
        self.oldest.clear();
        let Some(cat) = self.cat() else { return };
        let sort = self.settings.sort;
        let order = |a: &Item, b: &Item| {
            let o = match sort.key {
                SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                SortKey::Size => a.size.cmp(&b.size),
                SortKey::Changed => a.modified.cmp(&b.modified),
            };
            if sort.desc { o.reverse() } else { o }
        };
        let mut idx: Vec<usize> = (0..self.items.len())
            .filter(|&i| {
                let it = &self.items[i];
                it.category == cat && self.visible(it) && self.matches(it)
            })
            .collect();
        if cat == Category::Duplicates {
            // Groups stay together, biggest waste first; files are sorted inside a group.
            idx.sort_by(|&a, &b| {
                let (x, y) = (&self.items[a], &self.items[b]);
                x.dupe_group.cmp(&y.dupe_group).then_with(|| order(x, y))
            });
            self.oldest = oldest_copies(&self.items).into_values().collect();
            let mut last = 0;
            for i in idx {
                let g = self.items[i].dupe_group;
                if g != last {
                    last = g;
                    let (n, size) = groups.get(&g).copied().unwrap_or((0, 0));
                    self.group_labels.push(format!(
                        "{n} identical files  ·  {} each  ·  {} can be freed",
                        fmt::bytes(size),
                        fmt::bytes(size * n.saturating_sub(1))
                    ));
                    self.rows.push(Row::Group(self.group_labels.len() - 1));
                }
                self.rows.push(Row::Item(i));
            }
        } else {
            idx.sort_by(|&a, &b| order(&self.items[a], &self.items[b]));
            self.rows = idx.into_iter().map(Row::Item).collect();
        }
    }

    /// Selects or clears a category. The list's buttons pass `matching_only`, so they act
    /// on the rows the search box shows. The sidebar acts on the whole category.
    fn select_category(&mut self, cat: Category, select: bool, matching_only: bool) {
        if cat == Category::Duplicates && select {
            self.keep_oldest(matching_only);
            return;
        }
        let query = if matching_only {
            self.query.to_lowercase()
        } else {
            String::new()
        };
        let limit = self.settings.large_limit;
        for it in self.items.iter_mut().filter(|it| it.category == cat) {
            if (cat == Category::LargeFiles && it.size < limit)
                || (!query.is_empty() && !it.search.contains(&query))
            {
                continue;
            }
            if !select {
                it.selected = false;
            } else if matches!(it.status, Status::Ok | Status::Unmeasured) {
                it.selected = true;
            }
        }
        self.dirty = true;
    }

    /// Selects every duplicate except the oldest copy of each group. The oldest is found
    /// among all copies; with `matching_only` (the list's button), only rows that match the
    /// search box change.
    fn keep_oldest(&mut self, matching_only: bool) {
        let oldest = oldest_copies(&self.items);
        let query = self.query.to_lowercase();
        for (i, it) in self
            .items
            .iter_mut()
            .enumerate()
            .filter(|(_, it)| it.category == Category::Duplicates)
        {
            if matching_only && !it.search.contains(&query) {
                continue;
            }
            it.selected =
                it.selectable() && oldest.get(&it.dupe_group).is_some_and(|&keep| keep != i);
        }
        self.dirty = true;
    }

    /// Selects the build folders of projects untouched for `IDLE_DAYS` days, among the rows
    /// that match the search box.
    fn select_idle_projects(&mut self) {
        let cutoff = fmt::now() - IDLE_DAYS * 86_400;
        let query = self.query.to_lowercase();
        for it in self
            .items
            .iter_mut()
            .filter(|it| it.category == Category::BuildFolders && it.search.contains(&query))
        {
            if it.status == Status::Ok && it.modified > 0 && it.modified < cutoff {
                it.selected = true;
            }
        }
        self.dirty = true;
    }

    /// Adds paths to the Never touch list and removes what they cover from the lists.
    fn never_touch(&mut self, paths: &[PathBuf]) {
        let mut changed = false;
        for p in paths {
            changed |= self.settings.add_never(p);
        }
        if !changed {
            return;
        }
        self.settings.save();
        let never = self.settings.never.clone();
        let covered = |p: &Path| never.iter().any(|n| p.starts_with(n));
        // The Trash takes a folder as a whole, so a folder that holds one cannot go there.
        let holds = |p: &Path| never.iter().any(|n| n.starts_with(p));
        self.items.retain(|it| {
            !it.paths
                .iter()
                .any(|p| covered(p) || (it.action() == Action::Trash && holds(p)))
        });
        self.candidates.retain(|c| !covered(&c.path));
        self.drop_lonely_duplicates();
        // The Space map stops listing, and so opening and walking, what is covered (S12).
        let space = &mut self.space;
        space.levels.retain(|path, _| !covered(path));
        for level in space.levels.values_mut() {
            level.nodes.retain(|n| !covered(&n.path));
        }
        if let Some(i) = space.stack.iter().position(|p| covered(p)) {
            space.stack.truncate(i);
        }
        if space.pending.as_ref().is_some_and(|p| covered(p)) {
            space.pending = None;
        }
        space.rows_for = None;
        self.current = None;
        self.dirty = true;
    }

    fn make_plan(&self) -> Plan {
        let mut plan = Plan {
            delete: (0, 0),
            trash: (0, 0),
            empty: None,
        };
        for i in self.chosen() {
            let it = &self.items[i];
            match it.action() {
                Action::Delete => {
                    plan.delete.0 += it.size;
                    plan.delete.1 += 1;
                }
                Action::Trash => {
                    plan.trash.0 += it.size;
                    plan.trash.1 += 1;
                }
                Action::EmptyTrash => plan.empty = Some(plan.empty.unwrap_or(0) + it.size),
            }
        }
        plan
    }

    fn live_totals(&self) -> Option<[u64; CATEGORY_COUNT]> {
        let (items, live) = self.live.as_ref()?;
        let mut totals = [0u64; CATEGORY_COUNT];
        for (it, l) in items.iter().zip(live.iter()) {
            totals[it.category.index()] += l.size.load(Relaxed);
        }
        Some(totals)
    }

    // ---- Space map ------------------------------------------------------------------

    fn open_space(&mut self, ctx: &egui::Context) {
        self.view = View::Space;
        self.current = None;
        let Some(home) = self.home.clone() else {
            return;
        };
        if self.space.stack.is_empty() && self.space.levels.contains_key(&home) {
            self.space.stack = vec![home];
        }
        self.measure_next(ctx);
    }

    /// Measures the next folder in home that the scan left out (`~/Library` or `AppData`),
    /// unless a folder is being measured. It runs again when a measurement ends, because
    /// opening a deeper folder cancels the measurement that was running.
    fn measure_next(&mut self, ctx: &egui::Context) {
        if self.space.measuring.is_some() {
            return;
        }
        let next = self
            .home
            .as_ref()
            .and_then(|home| self.space.levels.get(home))
            .and_then(|l| l.nodes.iter().find(|n| !n.measured && !n.protected))
            .map(|n| n.path.clone());
        if let Some(path) = next {
            self.start_measure(path, false, ctx);
        }
    }

    fn start_measure(&mut self, path: PathBuf, open: bool, ctx: &egui::Context) {
        let Some(home) = self.home.clone() else {
            return;
        };
        if let Some(m) = self.space.measuring.take() {
            m.cancel.store(true, Relaxed);
        }
        let ctx = ctx.clone();
        self.space.pending = open.then(|| path.clone());
        self.space.measuring = Some(space::measure(
            path,
            home,
            self.settings.never.clone(),
            self.low_power,
            move || ctx.request_repaint(),
        ));
    }

    fn space_open(&mut self, path: PathBuf, ctx: &egui::Context) {
        if self.space.levels.contains_key(&path) {
            self.space.stack.push(path);
        } else {
            self.start_measure(path, true, ctx);
        }
    }

    fn space_add_level(&mut self, level: space::Level) {
        let total = level.total();
        for parent in self.space.levels.values_mut() {
            if let Some(node) = parent.nodes.iter_mut().find(|n| n.path == level.path) {
                node.size = total;
                node.measured = true;
            }
        }
        for parent in self.space.levels.values_mut() {
            parent.nodes.sort_by_key(|n| std::cmp::Reverse(n.size));
        }
        if self.space.pending.as_ref() == Some(&level.path) {
            // Open it only if its parent is still on screen: the breadcrumb is a chain.
            if level.path.parent() == self.space.stack.last().map(PathBuf::as_path) {
                self.space.stack.push(level.path.clone());
            }
            self.space.pending = None;
        }
        self.space.levels.insert(level.path.clone(), level);
        self.space.rows_for = None;
    }

    /// Formats the rows of the level on screen, once per level.
    fn space_rows(&mut self) {
        let Some(path) = self.space.stack.last().cloned() else {
            self.space.rows.clear();
            return;
        };
        if self.space.rows_for.as_ref() == Some(&path) {
            return;
        }
        let Some(home) = self.home.clone() else {
            return;
        };
        let Some(level) = self.space.levels.get(&path) else {
            return;
        };
        let max = level
            .nodes
            .iter()
            .map(|n| n.size)
            .max()
            .unwrap_or(0)
            .max(level.loose)
            .max(1);
        let mut rows = Vec::new();
        for n in &level.nodes {
            let (cleanable, category) = self.cleanable_in(&n.path);
            let (note, note_color) = if n.protected {
                (
                    "Not measured: synced to the cloud, or protected by privacy settings."
                        .to_owned(),
                    MUTED,
                )
            } else if !n.measured {
                ("Measuring…".to_owned(), MUTED)
            } else if cleanable > 0 {
                let text = match category {
                    Some(cat) => {
                        format!("{} can be cleaned in {}", fmt::bytes(cleanable), cat.name())
                    }
                    None => format!(
                        "{} can be cleaned, in several categories",
                        fmt::bytes(cleanable)
                    ),
                };
                (text, ACCENT)
            } else if let Some(hint) = space::hint(&home, &n.path) {
                (hint.to_owned(), MUTED)
            } else {
                (String::new(), MUTED)
            };
            rows.push(SpaceRow {
                path: n.path.clone(),
                name: n.name.clone(),
                size: n.size,
                size_text: if n.measured {
                    fmt::bytes(n.size)
                } else {
                    String::new()
                },
                fraction: n.size as f32 / max as f32,
                note,
                note_color,
                open: n.measured && !n.protected,
            });
        }
        if level.loose > 0 {
            rows.push(SpaceRow {
                path: path.clone(),
                name: "Files in this folder".to_owned(),
                size: level.loose,
                size_text: fmt::bytes(level.loose),
                fraction: level.loose as f32 / max as f32,
                note: String::new(),
                note_color: MUTED,
                open: false,
            });
        }
        rows.sort_by_key(|r| std::cmp::Reverse(r.size));
        self.space.rows = rows;
        self.space.rows_for = Some(path);
    }

    /// Bytes inside `path` that the lists can clean, and their category if there is only
    /// one. Items hidden by the large-file limit do not count.
    fn cleanable_in(&self, path: &Path) -> (u64, Option<Category>) {
        let mut by_cat = [0u64; CATEGORY_COUNT];
        for it in &self.items {
            if it.selectable() && self.visible(it) && it.paths.iter().any(|p| p.starts_with(path)) {
                by_cat[it.category.index()] += it.size;
            }
        }
        let used: Vec<usize> = (0..CATEGORY_COUNT).filter(|&i| by_cat[i] > 0).collect();
        let single = (used.len() == 1).then(|| Category::ALL[used[0]]);
        (by_cat.iter().sum(), single)
    }

    // ---- views ---------------------------------------------------------------------------

    fn header(&mut self, ui: &mut Ui) {
        let (mut start, mut cancel, mut gear) = (false, false, false);
        ui.horizontal_centered(|ui| {
            if let Some(icon) = &self.icon {
                ui.add(egui::Image::new(icon).fit_to_exact_size(vec2(26.0, 26.0)));
            }
            ui.label(RichText::new("Cleaner").size(17.0).strong().color(TEXT));
            ui.add_space(20.0);
            if let Some((disk, text)) = &self.disk {
                disk_bar(ui, disk);
                ui.label(RichText::new(text).color(MUTED));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                gear = ui
                    .add(Button::new("Settings").min_size(vec2(84.0, 30.0)))
                    .clicked();
                match &self.phase {
                    Phase::Scanning(_) => {
                        cancel = ui
                            .add(Button::new("Cancel").min_size(vec2(92.0, 30.0)))
                            .clicked()
                    }
                    Phase::Cleaning(_) => {
                        ui.add_enabled(false, Button::new("Scan").min_size(vec2(92.0, 30.0)));
                    }
                    Phase::Idle if self.scanned => {
                        // A new scan drops the result of a Put back that is still running.
                        let restoring = self.undo.as_ref().is_some_and(|u| u.running.is_some());
                        start = ui
                            .add_enabled(
                                self.finder.is_none() && !restoring && self.never_check.is_none(),
                                Button::new("Scan again").min_size(vec2(104.0, 30.0)),
                            )
                            .clicked();
                    }
                    Phase::Idle => {}
                }
            });
        });
        if start {
            self.start_scan(ui.ctx());
        }
        if cancel && let Phase::Scanning(handle) = &self.phase {
            handle.cancel.store(true, Relaxed);
        }
        if gear {
            self.settings_open = true;
        }
    }

    fn sidebar(&mut self, ui: &mut Ui) {
        let live = self.live_totals();
        let scanning = matches!(self.phase, Phase::Scanning(_));
        let idle = self.idle();
        let (mut open, mut toggle, mut space) = (None, None, false);
        ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(6.0);
                ui.label(RichText::new("OVERVIEW").size(11.0).color(MUTED));
                ui.add_space(4.0);
                let total = self
                    .home
                    .as_ref()
                    .and_then(|h| self.space.levels.get(h))
                    .map_or(String::new(), |l| fmt::bytes(l.total()));
                let (row, _) = sidebar_row(
                    ui,
                    "Space map",
                    &total,
                    self.view == View::Space,
                    None,
                    false,
                );
                space = row;
                ui.add_space(10.0);
                ui.label(RichText::new("CATEGORIES").size(11.0).color(MUTED));
                ui.add_space(4.0);
                for cat in Category::ALL.into_iter().filter(|c| c.visible()) {
                    let s = &self.summary[cat.index()];
                    let size = match (&live, scanning) {
                        (_, true) if HOME_CATEGORIES.contains(&cat) => "…".to_owned(),
                        (Some(live), true) => {
                            if live[cat.index()] > 0 {
                                fmt::bytes(live[cat.index()])
                            } else {
                                String::new()
                            }
                        }
                        _ => s.text.clone(),
                    };
                    let all = s.bulk > 0 && s.bulk_selected == s.bulk;
                    let some = s.bulk_selected > 0;
                    let check = Some((all, some, idle && s.bulk > 0));
                    let (row, checked) = sidebar_row(
                        ui,
                        cat.name(),
                        &size,
                        self.view == View::Category(cat),
                        check,
                        true,
                    );
                    if row {
                        open = Some(cat);
                    }
                    if checked {
                        toggle = Some((cat, s.click_selects(cat)));
                    }
                }
            });
        if space {
            self.open_space(ui.ctx());
        }
        if let Some(cat) = open
            && self.view != View::Category(cat)
        {
            self.view = View::Category(cat);
            self.current = None;
            self.dirty = true;
        }
        if let Some((cat, select)) = toggle {
            self.select_category(cat, select, false);
        }
    }

    fn main_view(&mut self, ui: &mut Ui) {
        let scanning = matches!(self.phase, Phase::Scanning(_));
        if scanning && self.items.is_empty() {
            self.scanning_view(ui);
            return;
        }
        if !self.scanned {
            self.welcome(ui);
            return;
        }
        let Some(cat) = self.cat() else {
            self.space_view(ui);
            return;
        };
        self.title_row(ui, cat);
        self.banners(ui, cat);
        if cat == Category::Duplicates && !self.dupes_done {
            self.duplicates_view(ui);
            return;
        }
        if self.rows.is_empty() {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                let text = if !self.query.is_empty() {
                    "No matches."
                } else if cat == Category::Trash {
                    "The Trash is empty."
                } else {
                    "Nothing to clean here."
                };
                ui.label(RichText::new(text).size(15.0).color(MUTED));
            });
            return;
        }
        self.table(ui, cat);
    }

    fn welcome(&mut self, ui: &mut Ui) {
        let mut start = false;
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() * 0.2).max(16.0));
            if let Some(icon) = &self.icon {
                ui.add(egui::Image::new(icon).fit_to_exact_size(vec2(112.0, 112.0)));
            }
            ui.add_space(14.0);
            ui.label(
                RichText::new("Find junk and forgotten files")
                    .size(24.0)
                    .strong()
                    .color(TEXT),
            );
            ui.add_space(6.0);
            ui.add(
                Label::new(
                    RichText::new(
                        "Cleaner looks at caches, logs, temporary files, large files and duplicates.\n\
                         Nothing is removed until you choose it and confirm.",
                    )
                    .color(MUTED),
                )
                .halign(Align::Center),
            );
            ui.add_space(20.0);
            start = ui
                .add_enabled(
                    !self.elevated && self.never_check.is_none(),
                    primary("Scan").min_size(vec2(150.0, 38.0)),
                )
                .clicked();
            if self.elevated {
                ui.add_space(10.0);
                ui.label(
                    RichText::new(
                        "Cleaner is running as an administrator. Start it as a normal user.",
                    )
                    .color(WARNING),
                );
            }
        });
        if start {
            self.start_scan(ui.ctx());
        }
    }

    fn scanning_view(&self, ui: &mut Ui) {
        let Phase::Scanning(handle) = &self.phase else {
            return;
        };
        let p = &handle.progress;
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() * 0.25).max(16.0));
            if let Some(icon) = &self.icon {
                ui.add(egui::Image::new(icon).fit_to_exact_size(vec2(84.0, 84.0)));
            }
            ui.add_space(14.0);
            let stage = if p.stage.load(Relaxed) == scan::STAGE_JUNK {
                "Looking at caches, logs and temporary files…"
            } else {
                "Looking through your home folder…"
            };
            ui.label(RichText::new(stage).size(18.0).color(TEXT));
            ui.add_space(4.0);
            let text = format!(
                "{} files  ·  {} seen",
                fmt::count(p.files.load(Relaxed)),
                fmt::bytes(p.bytes.load(Relaxed))
            );
            ui.label(RichText::new(text).color(MUTED));
        });
    }

    fn title_row(&mut self, ui: &mut Ui, cat: Category) {
        let s = self.summary[cat.index()].clone();
        let idle = self.idle();
        let (mut select, mut keep, mut idle_projects) = (None, false, false);
        let mut limit = self.settings.large_limit;
        let mut query_changed = false;
        ui.horizontal(|ui| {
            ui.label(RichText::new(cat.name()).size(20.0).strong().color(TEXT));
            if !s.text.is_empty() {
                ui.label(RichText::new(&s.text).size(15.0).color(MUTED));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                query_changed = ui
                    .add(
                        TextEdit::singleline(&mut self.query)
                            .hint_text("Search")
                            .desired_width(150.0),
                    )
                    .changed();
                if cat == Category::Duplicates {
                    if self.dupes_done && s.shown > 0 {
                        keep = ui
                            .add_enabled(idle, Button::new("Select all but the oldest"))
                            .clicked();
                        if s.shown_selected > 0
                            && ui.add_enabled(idle, Button::new("Select none")).clicked()
                        {
                            select = Some(false);
                        }
                    }
                } else if s.shown > 0 {
                    let all = s.shown_selected == s.shown;
                    if ui
                        .add_enabled(
                            idle,
                            Button::new(if all { "Select none" } else { "Select all" }),
                        )
                        .clicked()
                    {
                        select = Some(!all);
                    }
                }
                if cat == Category::BuildFolders {
                    idle_projects = ui
                        .add_enabled(
                            idle,
                            Button::new(format!("Select projects idle {IDLE_DAYS}+ days")),
                        )
                        .clicked();
                }
                if cat == Category::LargeFiles {
                    ComboBox::from_id_salt("large-limit")
                        .selected_text(format!("{} or more", short_size(limit)))
                        .width(132.0)
                        .show_ui(ui, |ui| {
                            for t in THRESHOLDS {
                                ui.selectable_value(
                                    &mut limit,
                                    t,
                                    format!("{} or more", short_size(t)),
                                );
                            }
                        });
                }
            });
        });
        ui.label(RichText::new(cat.about()).color(MUTED));
        ui.add_space(10.0);
        if limit != self.settings.large_limit {
            self.settings.large_limit = limit;
            self.settings.save();
            self.dirty = true;
        }
        if query_changed {
            self.dirty = true;
        }
        if let Some(select) = select {
            self.select_category(cat, select, true);
        }
        if keep {
            self.keep_oldest(true);
        }
        if idle_projects {
            self.select_idle_projects();
        }
    }

    fn banners(&mut self, ui: &mut Ui, cat: Category) {
        if self.elevated {
            banner(
                ui,
                WARNING,
                "Cleaner is running as an administrator, so cleaning is off. Start it as a normal user.",
                None,
            );
        }
        let blocked = self.items.iter().any(|it| {
            it.category == cat && matches!(it.status, Status::NoAccess | Status::Unmeasured)
        });
        if blocked
            && self.fda == Some(false)
            && banner(
                ui,
                WARNING,
                "Some items need Full Disk Access. Allow Cleaner in System Settings, then scan again.",
                Some("Open Settings"),
            )
        {
            platform::open_privacy_settings();
        }
        if matches!(
            cat,
            Category::LargeFiles
                | Category::OldDownloads
                | Category::Installers
                | Category::Duplicates
        ) {
            for note in &self.notes {
                banner(ui, WARNING, note, None);
            }
        }
    }

    fn duplicates_view(&mut self, ui: &mut Ui) {
        let (mut go, mut stop) = (false, false);
        let idle = self.idle();
        ui.add_space(30.0);
        ui.vertical_centered(|ui| {
            if let Some(finder) = &self.finder {
                let p = &finder.progress;
                let (read, total) = (p.read.load(Relaxed), p.total.load(Relaxed));
                let stage = if p.stage.load(Relaxed) == dupes::STAGE_HASH {
                    "Comparing files…"
                } else {
                    "Checking matches byte by byte…"
                };
                ui.label(RichText::new(stage).size(17.0).color(TEXT));
                ui.add_space(4.0);
                ui.label(
                    RichText::new(format!(
                        "{} read of {} at most",
                        fmt::bytes(read),
                        fmt::bytes(total)
                    ))
                    .color(MUTED),
                );
                ui.add_space(8.0);
                static_bar(ui, read as f32 / total.max(1) as f32, 280.0);
                ui.add_space(10.0);
                stop = ui.button("Stop").clicked();
            } else {
                let n = self.candidates.len() as u64;
                ui.label(
                    RichText::new(format!(
                        "{} of your files (1 MB or more) can be compared.",
                        fmt::count(n)
                    ))
                    .size(15.0)
                    .color(TEXT),
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "Hidden tool folders, code projects and app environments are left out. \
                         Matches are confirmed byte by byte.",
                    )
                    .color(MUTED),
                );
                ui.add_space(16.0);
                go = ui
                    .add_enabled(
                        idle && n > 1,
                        primary("Find duplicates").min_size(vec2(150.0, 34.0)),
                    )
                    .clicked();
            }
        });
        if go {
            self.find_duplicates(ui.ctx());
        }
        if stop && let Some(finder) = &self.finder {
            finder.cancel.store(true, Relaxed);
        }
    }

    fn table(&mut self, ui: &mut Ui, cat: Category) {
        let idle = self.idle();
        let sort = self.settings.sort;
        let rows = &self.rows;
        let labels = &self.group_labels;
        let oldest = &self.oldest;
        let current = self.current;
        let items = &mut self.items;
        let mut changed = false;
        let mut reveal: Option<PathBuf> = None;
        let mut preview: Option<PathBuf> = None;
        let mut clicked: Option<usize> = None;
        let mut never: Option<Vec<PathBuf>> = None;
        let mut new_sort: Option<Sort> = None;
        TableBuilder::new(ui)
            .id_salt(cat)
            .striped(false)
            .resizable(false)
            .sense(Sense::click())
            .auto_shrink([false, false])
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::exact(30.0))
            .column(Column::remainder().at_least(220.0).clip(true))
            .column(Column::exact(96.0))
            .column(Column::exact(116.0))
            .column(Column::exact(118.0))
            .header(22.0, |mut header| {
                header.col(|_| {});
                for (key, label, right) in [
                    (SortKey::Name, "Name", false),
                    (SortKey::Size, "Size", true),
                    (SortKey::Changed, "Changed", false),
                ] {
                    header.col(|ui| {
                        let arrow = if sort.key == key {
                            if sort.desc { " ▼" } else { " ▲" }
                        } else {
                            ""
                        };
                        let color = if sort.key == key { TEXT } else { MUTED };
                        let label = Label::new(
                            RichText::new(format!("{label}{arrow}"))
                                .small()
                                .color(color),
                        )
                        .sense(Sense::click());
                        let response = if right {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui.add(label))
                                .inner
                        } else {
                            if key == SortKey::Changed {
                                ui.add_space(14.0);
                            }
                            ui.add(label)
                        };
                        if response.on_hover_text("Sort").clicked() {
                            let desc = if sort.key == key {
                                !sort.desc
                            } else {
                                key != SortKey::Name
                            };
                            new_sort = Some(Sort { key, desc });
                        }
                    });
                }
                header.col(|_| {});
            })
            .body(|body| {
                body.rows(ROW_HEIGHT, rows.len(), |mut row| match rows[row.index()] {
                    Row::Group(g) => {
                        row.set_overline(true);
                        row.col(|_| {});
                        row.col(|ui| {
                            ui.label(RichText::new(&labels[g]).small().strong().color(ACCENT_2));
                        });
                        row.col(|_| {});
                        row.col(|_| {});
                        row.col(|_| {});
                    }
                    Row::Item(i) => {
                        let it = &mut items[i];
                        row.set_selected(current == Some(i));
                        row.col(|ui| {
                            let mut selected = it.selected;
                            if ui
                                .add_enabled(
                                    idle && it.selectable(),
                                    Checkbox::without_text(&mut selected),
                                )
                                .changed()
                            {
                                it.selected = selected;
                                changed = true;
                            }
                        });
                        row.col(|ui| name_cell(ui, it, oldest.contains(&i)));
                        row.col(|ui| {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let color = if it.status == Status::NoAccess {
                                    WARNING
                                } else {
                                    TEXT
                                };
                                ui.label(RichText::new(&it.size_text).color(color));
                            });
                        });
                        row.col(|ui| {
                            ui.add_space(14.0);
                            ui.label(RichText::new(&it.when_text).color(MUTED));
                        });
                        row.col(|ui| {
                            if it.paths.is_empty() {
                                return;
                            }
                            if ui
                                .add(
                                    Button::new(RichText::new("Preview").small().color(MUTED))
                                        .frame(false),
                                )
                                .on_hover_text("Quick Look (Space)")
                                .clicked()
                            {
                                preview = Some(it.paths[0].clone());
                            }
                            if ui
                                .add(
                                    Button::new(RichText::new("Show").small().color(MUTED))
                                        .frame(false),
                                )
                                .on_hover_text(&it.path_text)
                                .clicked()
                            {
                                reveal = Some(it.paths[0].clone());
                            }
                        });
                        let response = row.response();
                        if response.clicked() {
                            clicked = Some(i);
                        }
                        response.context_menu(|ui| {
                            if !it.paths.is_empty() {
                                if ui.button("Preview").clicked() {
                                    preview = Some(it.paths[0].clone());
                                    ui.close();
                                }
                                if ui.button(reveal_label()).clicked() {
                                    reveal = Some(it.paths[0].clone());
                                    ui.close();
                                }
                                ui.separator();
                                // A clean in progress reports rows by index, and removing
                                // rows now would shift them.
                                if ui
                                    .add_enabled(idle, Button::new("Never touch this"))
                                    .on_disabled_hover_text(BUSY_NEVER)
                                    .clicked()
                                {
                                    never = Some(it.paths.clone());
                                    ui.close();
                                }
                            }
                        });
                    }
                });
            });
        if changed {
            self.dirty = true;
        }
        if let Some(i) = clicked {
            self.current = Some(i);
        }
        if let Some(sort) = new_sort {
            self.settings.sort = sort;
            self.settings.save();
            self.dirty = true;
        }
        if let Some(path) = reveal {
            platform::reveal(&path);
        }
        if let Some(path) = preview {
            platform::preview(&path);
        }
        if let Some(paths) = never {
            self.never_touch(&paths);
        }
    }

    fn space_view(&mut self, ui: &mut Ui) {
        self.space_rows();
        let (mut open, mut back_to) = (None, None);
        let mut reveal: Option<PathBuf> = None;
        ui.horizontal(|ui| {
            ui.label(RichText::new("Space map").size(20.0).strong().color(TEXT));
            if let Some(total) = self
                .space
                .stack
                .last()
                .and_then(|p| self.space.levels.get(p))
                .map(|l| l.total())
            {
                ui.label(RichText::new(fmt::bytes(total)).size(15.0).color(MUTED));
            }
        });
        // Breadcrumb: home › Library › Application Support
        ui.horizontal_wrapped(|ui| {
            let last = self.space.stack.len().saturating_sub(1);
            for (i, path) in self.space.stack.iter().enumerate() {
                let name = if i == 0 {
                    "Home".to_owned()
                } else {
                    path.file_name()
                        .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
                };
                if i == last {
                    ui.label(RichText::new(name).color(TEXT));
                } else {
                    if ui
                        .add(Button::new(RichText::new(name).color(ACCENT)).frame(false))
                        .clicked()
                    {
                        back_to = Some(i);
                    }
                    ui.label(RichText::new("›").color(MUTED));
                }
            }
        });
        ui.label(
            RichText::new(
                "Where your disk space goes. Click a folder to look inside. Nothing here is removed: \
                 clean from the categories on the left.",
            )
            .color(MUTED),
        );
        ui.add_space(8.0);
        if self.space.stale {
            banner(
                ui,
                WARNING,
                "These sizes are from before the last clean. Scan again to refresh them.",
                None,
            );
        }
        if let Some(m) = &self.space.measuring {
            let name = m
                .path
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            ui.label(
                RichText::new(format!(
                    "Measuring {name}…  {} files",
                    fmt::count(m.files.load(Relaxed))
                ))
                .color(MUTED),
            );
            ui.add_space(6.0);
        }
        if self.space.rows.is_empty() {
            return;
        }
        let rows = &self.space.rows;
        TableBuilder::new(ui)
            .id_salt("space")
            .striped(false)
            .resizable(false)
            .sense(Sense::click())
            .auto_shrink([false, false])
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::remainder().at_least(260.0).clip(true))
            .column(Column::exact(96.0))
            .column(Column::exact(64.0))
            .body(|body| {
                body.rows(ROW_HEIGHT, rows.len(), |mut row| {
                    let r = &rows[row.index()];
                    row.col(|ui| {
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 3.0;
                            let name = if r.open {
                                format!("{}  ›", r.name)
                            } else {
                                r.name.clone()
                            };
                            ui.add(Label::new(RichText::new(name).color(TEXT)).truncate());
                            let (rect, _) = ui.allocate_exact_size(
                                vec2((ui.available_width() - 8.0).max(40.0), 4.0),
                                Sense::hover(),
                            );
                            paint_bar(ui, rect, r.fraction, ACCENT);
                            if !r.note.is_empty() {
                                ui.add(
                                    Label::new(RichText::new(&r.note).small().color(r.note_color))
                                        .truncate(),
                                );
                            }
                        });
                    });
                    row.col(|ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(RichText::new(&r.size_text).color(TEXT));
                        });
                    });
                    row.col(|ui| {
                        if ui
                            .add(
                                Button::new(RichText::new("Show").small().color(MUTED))
                                    .frame(false),
                            )
                            .on_hover_text(fmt::path(&r.path))
                            .clicked()
                        {
                            reveal = Some(r.path.clone());
                        }
                    });
                    if row.response().clicked() && r.open {
                        open = Some(r.path.clone());
                    }
                });
            });
        if let Some(path) = reveal {
            platform::reveal(&path);
        }
        if let Some(i) = back_to {
            self.space.stack.truncate(i + 1);
        }
        if let Some(path) = open {
            self.space_open(path, ui.ctx());
        }
    }

    fn footer(&mut self, ui: &mut Ui) {
        let (mut clean, mut stop) = (false, false);
        ui.horizontal_centered(|ui| {
            match &self.phase {
                Phase::Scanning(handle) => {
                    let p = &handle.progress;
                    let text = format!(
                        "Scanning…  {} files  ·  {}",
                        fmt::count(p.files.load(Relaxed)),
                        fmt::bytes(p.bytes.load(Relaxed))
                    );
                    ui.label(RichText::new(text).color(MUTED));
                }
                Phase::Cleaning(handle) => {
                    let p = &handle.progress;
                    let (done, total) = (p.done.load(Relaxed), p.total.load(Relaxed).max(1));
                    static_bar(ui, done as f32 / total as f32, 160.0);
                    let text = format!(
                        "Cleaning…  {done} of {total}  ·  {} removed",
                        fmt::bytes(p.removed.load(Relaxed))
                    );
                    ui.label(RichText::new(text).color(MUTED));
                    stop = ui.button("Stop").clicked();
                }
                Phase::Idle if self.scanned => {
                    ui.label(RichText::new(&self.chosen_text).color(TEXT));
                }
                Phase::Idle => {}
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                // Put back adds rows when it ends; a clean meanwhile would replace its result.
                let restoring = self.undo.as_ref().is_some_and(|u| u.running.is_some());
                let can = self.idle() && !restoring && self.chosen_count > 0 && !self.elevated;
                clean = ui
                    .add_enabled(can, primary(&self.clean_label).min_size(vec2(130.0, 32.0)))
                    .clicked();
            });
        });
        if clean {
            self.plan = Some(self.make_plan());
        }
        if stop && let Phase::Cleaning(handle) = &self.phase {
            handle.cancel.store(true, Relaxed);
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        self.confirm_dialog(ctx);
        self.report_dialog(ctx);
        self.settings_dialog(ctx);
    }

    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(plan) = &self.plan else { return };
        let (mut go, mut close) = (false, false);
        let total = plan.delete.0 + plan.trash.0 + plan.empty.unwrap_or(0);
        let response = Modal::new(Id::unique("confirm"))
            .frame(dialog_frame())
            .show(ctx, |ui| {
                ui.set_width(440.0);
                ui.label(
                    RichText::new(format!("Clean {}?", fmt::bytes(total)))
                        .size(19.0)
                        .strong()
                        .color(TEXT),
                );
                ui.add_space(10.0);
                if plan.delete.1 > 0 {
                    plan_line(
                        ui,
                        "Delete for good",
                        plan.delete,
                        "Caches, logs, temporary files, app data and build folders. Apps re-create them.",
                    );
                }
                if plan.trash.1 > 0 {
                    plan_line(
                        ui,
                        "Move to the Trash",
                        plan.trash,
                        "Your own files. You can put them back afterwards.",
                    );
                }
                if let Some(size) = plan.empty {
                    let size = if size > 0 {
                        fmt::bytes(size)
                    } else {
                        "everything".to_owned()
                    };
                    plan_line_text(ui, "Empty the Trash", &size, "This cannot be undone.");
                }
                ui.add_space(14.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    go = ui.add(danger("Clean")).clicked();
                    close = ui.button("Cancel").clicked();
                });
            });
        if go {
            self.plan = None;
            self.start_clean(ctx);
        } else if close || response.should_close() {
            self.plan = None;
        }
    }

    fn report_dialog(&mut self, ctx: &egui::Context) {
        let Some(report) = &self.report else { return };
        let (mut close, mut put_back) = (false, false);
        let undo = self.undo.as_ref();
        let response = Modal::new(Id::unique("report"))
            .frame(dialog_frame())
            .show(ctx, |ui| {
                ui.set_width(480.0);
                let title = if report.refused.is_some() {
                    "Nothing was cleaned"
                } else if report.stopped {
                    "Cleaning stopped"
                } else {
                    "Cleaning finished"
                };
                ui.label(RichText::new(title).size(19.0).strong().color(TEXT));
                ui.add_space(10.0);
                if let Some(why) = &report.refused {
                    ui.label(RichText::new(why).color(WARNING));
                }
                if report.removed > 0 {
                    report_line(ui, "Deleted", &fmt::bytes(report.removed));
                }
                if report.trashed > 0 {
                    report_line(ui, "Moved to the Trash", &fmt::bytes(report.trashed));
                    ui.label(
                        RichText::new("The space comes back when you empty the Trash.")
                            .small()
                            .color(MUTED),
                    );
                }
                if let (Some(before), Some(after)) = (report.free_before, report.free_after) {
                    let gain = after.saturating_sub(before);
                    let text = if gain > 0 {
                        format!("{}  (+{})", fmt::bytes(after), fmt::bytes(gain))
                    } else {
                        fmt::bytes(after)
                    };
                    report_line(ui, "Free space now", &text);
                }
                let skipped = report.skipped.len() + report.skipped_more;
                if skipped > 0 {
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new(format!("Skipped {}:", fmt::items(skipped as u64)))
                            .color(TEXT),
                    );
                    ScrollArea::vertical()
                        .max_height(180.0)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            for (path, why) in &report.skipped {
                                ui.add(
                                    Label::new(
                                        RichText::new(format!("{path} — {why}"))
                                            .small()
                                            .color(MUTED),
                                    )
                                    .truncate(),
                                )
                                .on_hover_text(path);
                            }
                            if report.skipped_more > 0 {
                                ui.label(
                                    RichText::new(format!(
                                        "and {} more",
                                        fmt::count(report.skipped_more as u64)
                                    ))
                                    .small()
                                    .color(MUTED),
                                );
                            }
                        });
                }
                if let Some(undo) = undo
                    && let Some(text) = &undo.result
                {
                    ui.add_space(8.0);
                    ui.label(RichText::new(text).color(TEXT));
                }
                ui.add_space(14.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    close = ui
                        .add(primary("OK").min_size(vec2(80.0, 30.0)))
                        .clicked();
                    if let Some(undo) = undo
                        && !undo.items.is_empty()
                    {
                        let label = format!("Put back {}", fmt::items(undo.items.len() as u64));
                        put_back = ui
                            .add_enabled(undo.running.is_none(), Button::new(label))
                            .on_hover_text("Moves the files back from the Trash. Deleted caches cannot come back.")
                            .clicked();
                    }
                });
            });
        if put_back {
            self.start_restore(ctx);
        }
        if close || response.should_close() {
            self.report = None;
        }
    }

    fn settings_dialog(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let idle = self.idle();
        let (mut close, mut remove, mut add) = (false, None, false);
        let response = Modal::new(Id::unique("settings"))
            .frame(dialog_frame())
            .show(ctx, |ui| {
                ui.set_width(520.0);
                ui.label(RichText::new("Settings").size(19.0).strong().color(TEXT));
                ui.add_space(10.0);
                ui.label(RichText::new("Never touch").strong().color(TEXT));
                ui.label(
                    RichText::new(
                        "Cleaner never lists or removes these folders and files. \
                         You can also right-click a row and choose Never touch this.",
                    )
                    .small()
                    .color(MUTED),
                );
                ui.add_space(6.0);
                if self.settings.never.is_empty() {
                    ui.label(RichText::new("Nothing yet.").color(MUTED));
                }
                ScrollArea::vertical()
                    .max_height(180.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for (i, p) in self.settings.never.iter().enumerate() {
                            ui.horizontal(|ui| {
                                if ui.small_button("Remove").clicked() {
                                    remove = Some(i);
                                }
                                ui.add(
                                    Label::new(RichText::new(fmt::path(p)).color(TEXT)).truncate(),
                                );
                            });
                        }
                    });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let edit = ui.add_enabled(
                        self.never_check.is_none(),
                        TextEdit::singleline(&mut self.never_input)
                            .hint_text("Paste a folder or file path")
                            .desired_width(380.0),
                    );
                    let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    let button = ui
                        .add_enabled(idle, Button::new("Add"))
                        .on_disabled_hover_text(BUSY_NEVER);
                    add = idle && (button.clicked() || enter);
                });
                if self.never_check.is_some() {
                    ui.label(RichText::new("Checking the path…").small().color(MUTED));
                } else if let Some(error) = self.never_error {
                    ui.label(RichText::new(error).small().color(WARNING));
                }
                ui.add_space(14.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    close = ui.add(primary("Done").min_size(vec2(80.0, 30.0))).clicked();
                });
            });
        if let Some(i) = remove {
            self.settings.never.remove(i);
            self.settings.save();
        }
        if add {
            let text = self.never_input.trim().trim_matches('"').to_owned();
            let (tx, rx) = mpsc::channel();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                // The scan lists canonical paths: `\\?\C:\…` on Windows, the on-disk letter
                // case on macOS. A path typed any other way would never match.
                let path = Path::new(&text)
                    .is_absolute()
                    .then(|| std::fs::canonicalize(&text).ok())
                    .flatten();
                let _ = tx.send(path);
                ctx.request_repaint();
            });
            self.never_check = Some(rx);
            self.never_error = None;
        }
        if close || response.should_close() {
            self.settings_open = false;
            self.never_error = None;
        }
    }
}

impl eframe::App for CleanerApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll(ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        crate::settings::flush();
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let bar = Frame::new()
            .fill(BG)
            .inner_margin(Margin::symmetric(18, 10));
        egui::Panel::top("header")
            .exact_size(54.0)
            .frame(bar)
            .show_separator_line(false)
            .show(ui, |ui| self.header(ui));
        egui::Panel::bottom("footer")
            .exact_size(56.0)
            .frame(bar)
            .show_separator_line(false)
            .show(ui, |ui| self.footer(ui));
        egui::Panel::left("sidebar")
            .exact_size(SIDEBAR_WIDTH)
            .resizable(false)
            .frame(Frame::new().fill(BG).inner_margin(Margin {
                left: 12,
                right: 10,
                top: 4,
                bottom: 4,
            }))
            .show_separator_line(false)
            .show(ui, |ui| self.sidebar(ui));
        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(PANEL)
                    .stroke(Stroke::new(1.0, BORDER))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(Margin::same(20))
                    .outer_margin(Margin {
                        left: 0,
                        right: 12,
                        top: 0,
                        bottom: 0,
                    }),
            )
            .show(ui, |ui| self.main_view(ui));
        self.dialogs(&ctx);
        // Space previews the clicked row, unless a text field has the keyboard.
        if ctx.input(|i| i.key_pressed(Key::Space))
            && !ctx.egui_wants_keyboard_input()
            && let Some(i) = self.current
            && let Some(path) = self.items.get(i).and_then(|it| it.paths.first())
        {
            platform::preview(path);
        }
        if self.busy() {
            ctx.request_repaint_after(BUSY_REPAINT);
        }
    }
}

/// The oldest copy of each duplicate group, as group → index in `items`. A tie goes to the
/// smaller path, as when cleaning keeps a copy. "Select all but the oldest" and the
/// "oldest" tag both use this, so they always agree.
fn oldest_copies(items: &[Item]) -> HashMap<u32, usize> {
    let mut oldest: HashMap<u32, usize> = HashMap::new();
    for (i, it) in items
        .iter()
        .enumerate()
        .filter(|(_, it)| it.category == Category::Duplicates)
    {
        let keep = oldest.entry(it.dupe_group).or_insert(i);
        let current = &items[*keep];
        if (it.modified, &it.paths[0]) < (current.modified, &current.paths[0]) {
            *keep = i;
        }
    }
    oldest
}

/// Files that a clean moved to the Trash sit there now, so emptying it would delete them
/// for good too. Adds their bytes to the Trash row. A Trash row that the same clean emptied
/// (it is in `gone`) stays, unselected, holding just those files.
fn refill_trash_rows(items: &mut [Item], gone: &mut HashSet<usize>, bytes: u64, now: i64) {
    if bytes == 0 {
        return;
    }
    for (i, it) in items
        .iter_mut()
        .enumerate()
        .filter(|(_, it)| it.category == Category::Trash)
    {
        if gone.remove(&i) {
            it.size = 0;
            it.selected = false;
        }
        if it.status != Status::Unmeasured {
            it.size += bytes;
        }
        it.finish_text(now);
    }
}

// ---- small widgets ---------------------------------------------------------------------

fn primary(text: &str) -> Button<'static> {
    Button::new(RichText::new(text.to_owned()).color(ON_ACCENT).strong())
        .fill(ACCENT)
        .min_size(vec2(92.0, 30.0))
}

fn danger(text: &str) -> Button<'static> {
    Button::new(RichText::new(text.to_owned()).color(ON_ACCENT).strong())
        .fill(DANGER)
        .min_size(vec2(92.0, 30.0))
}

fn reveal_label() -> &'static str {
    if cfg!(windows) {
        "Show in File Explorer"
    } else {
        "Show in Finder"
    }
}

fn dialog_frame() -> Frame {
    Frame::new()
        .fill(RAISED)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::same(22))
}

fn short_size(n: u64) -> String {
    if n >= fmt::GB && n.is_multiple_of(fmt::GB) {
        format!("{} GB", n / fmt::GB)
    } else {
        format!("{} MB", n / fmt::MB)
    }
}

/// A used-space bar: accent normally, red when less than a tenth is free.
fn disk_bar(ui: &mut Ui, disk: &Disk) {
    let (rect, _) = ui.allocate_exact_size(vec2(140.0, 8.0), Sense::hover());
    let used = if disk.total > 0 {
        1.0 - disk.free as f32 / disk.total as f32
    } else {
        0.0
    };
    let color = if disk.free < disk.total / 10 {
        DANGER
    } else {
        ACCENT
    };
    paint_bar(ui, rect, used, color);
}

/// A plain progress bar. Unlike egui's animated one, it draws only when asked.
fn static_bar(ui: &mut Ui, fraction: f32, width: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(width, 6.0), Sense::hover());
    paint_bar(ui, rect, fraction, ACCENT);
}

fn paint_bar(ui: &Ui, rect: Rect, fraction: f32, color: Color32) {
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(4), RAISED);
    let mut fill = rect;
    fill.set_width((rect.width() * fraction.clamp(0.0, 1.0)).max(rect.height()));
    painter.rect_filled(fill, CornerRadius::same(4), color);
}

/// One sidebar row. `check` is (all selected, some selected, enabled) for categories.
/// Returns (row clicked, checkbox clicked).
fn sidebar_row(
    ui: &mut Ui,
    name: &str,
    size: &str,
    current: bool,
    check: Option<(bool, bool, bool)>,
    indent: bool,
) -> (bool, bool) {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), Sense::click());
    let painter = ui.painter().clone();
    if current {
        painter.rect_filled(rect, CornerRadius::same(8), RAISED);
        let marker = Rect::from_min_size(
            pos2(rect.left(), rect.top() + 9.0),
            vec2(3.0, rect.height() - 18.0),
        );
        painter.rect_filled(marker, CornerRadius::same(2), ACCENT);
    } else if response.hovered() {
        painter.rect_filled(rect, CornerRadius::same(8), PANEL);
    }
    let mut checked_clicked = false;
    if let Some((all, some, enabled)) = check {
        let check_rect =
            Rect::from_center_size(pos2(rect.left() + 22.0, rect.center().y), vec2(18.0, 18.0));
        let mut checked = all;
        checked_clicked = ui
            .add_enabled_ui(enabled, |ui| {
                ui.put(
                    check_rect,
                    Checkbox::without_text(&mut checked).indeterminate(some && !all),
                )
            })
            .inner
            .clicked();
    }
    let name_color = if current {
        TEXT
    } else {
        Color32::from_rgb(0xC6, 0xCB, 0xD6)
    };
    let x = if indent { 42.0 } else { 16.0 };
    painter.text(
        pos2(rect.left() + x, rect.center().y),
        Align2::LEFT_CENTER,
        name,
        FontId::proportional(14.0),
        name_color,
    );
    painter.text(
        pos2(rect.right() - 10.0, rect.center().y),
        Align2::RIGHT_CENTER,
        size,
        FontId::proportional(12.5),
        MUTED,
    );
    (response.clicked(), checked_clicked)
}

fn name_cell(ui: &mut Ui, it: &Item, oldest: bool) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.horizontal(|ui| {
            ui.add(Label::new(RichText::new(&it.name).color(TEXT)).truncate());
            if !it.group.is_empty() {
                ui.label(RichText::new(&it.group).small().color(MUTED));
            }
            if oldest {
                ui.label(RichText::new("oldest").small().color(ACCENT_2));
            }
            let tag = match it.status {
                Status::Running => Some(("in use — quit the app first", WARNING)),
                Status::InfoOnly => Some(("shown for information", MUTED)),
                Status::NoAccess if cfg!(target_os = "macos") => {
                    Some(("needs Full Disk Access", WARNING))
                }
                Status::NoAccess => Some(("no access", WARNING)),
                Status::Unmeasured => Some(("emptied through Finder", MUTED)),
                Status::Ok => None,
            };
            if let Some((text, color)) = tag {
                ui.label(RichText::new(text).small().color(color));
            }
            if !it.note.is_empty() {
                ui.add(Label::new(RichText::new(&it.note).small().color(WARNING)).truncate())
                    .on_hover_text(&it.note);
            }
        });
        ui.add(Label::new(RichText::new(&it.path_text).small().color(MUTED)).truncate());
    });
}

/// A note above the list. Returns true if its button was clicked.
fn banner(ui: &mut Ui, color: Color32, text: &str, button: Option<&str>) -> bool {
    let mut clicked = false;
    Frame::new()
        .fill(color.gamma_multiply(0.10))
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.45)))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.add(Label::new(RichText::new(text).color(TEXT)).wrap());
                if let Some(label) = button {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        clicked = ui.button(label).clicked()
                    });
                }
            });
        });
    ui.add_space(8.0);
    clicked
}

fn plan_line(ui: &mut Ui, what: &str, (size, count): (u64, u64), note: &str) {
    plan_line_text(
        ui,
        what,
        &format!("{}  ·  {}", fmt::bytes(size), fmt::items(count)),
        note,
    );
}

fn plan_line_text(ui: &mut Ui, what: &str, value: &str, note: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(what).color(TEXT).strong());
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(value).color(TEXT))
        });
    });
    ui.label(RichText::new(note).small().color(MUTED));
    ui.add_space(8.0);
}

fn report_line(ui: &mut Ui, what: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(what).color(MUTED));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(value).color(TEXT).strong())
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row of `cat` named `name`, with the text the search box reads.
    fn row(cat: Category, name: &str, selected: bool) -> Item {
        let mut it = Item::new(
            cat,
            name,
            vec![PathBuf::from("/h").join(name)],
            PathBuf::from("/h"),
            Shape::Entry,
        );
        it.size = 1_000;
        it.selected = selected;
        it.finish_text(0);
        it
    }

    /// A copy in a duplicate group.
    fn copy(name: &str, group: u32, modified: i64, selected: bool) -> Item {
        let mut it = row(Category::Duplicates, name, selected);
        it.dupe_group = group;
        it.modified = modified;
        it
    }

    /// The names of the selected rows, sorted.
    fn selected(app: &CleanerApp) -> Vec<String> {
        let mut names: Vec<String> = app
            .items
            .iter()
            .filter(|it| it.selected)
            .map(|it| it.name.clone())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn the_search_leaves_the_sidebar_alone() {
        let mut app = CleanerApp::blank(Settings::default());
        app.items = vec![
            row(Category::SystemJunk, "alpha", true),
            row(Category::SystemJunk, "beta", true),
        ];
        // Typed while another category is on screen.
        app.view = View::Category(Category::OldDownloads);
        app.query = "zzz".to_owned();
        app.recompute();
        let s = &app.summary[Category::SystemJunk.index()];
        assert_eq!((s.bulk, s.bulk_selected, s.shown), (2, 2, 0));
        assert!(!s.click_selects(Category::SystemJunk), "a click clears");
        // The list's button acts on the rows the search shows.
        app.query = "alpha".to_owned();
        app.select_category(Category::SystemJunk, false, true);
        assert_eq!(selected(&app), ["beta"]);
        // The sidebar acts on the whole category.
        app.select_category(Category::SystemJunk, true, false);
        assert_eq!(selected(&app), ["alpha", "beta"]);
    }

    #[test]
    fn the_duplicates_checkbox_selects_then_clears() {
        let mut app = CleanerApp::blank(Settings::default());
        app.items = vec![
            copy("a/x.bin", 1, 10, false),
            copy("b/x.bin", 1, 20, false),
            copy("a/y.bin", 2, 10, false),
            copy("b/y.bin", 2, 20, false),
        ];
        app.recompute();
        let cat = Category::Duplicates;
        assert!(app.summary[cat.index()].click_selects(cat));
        app.select_category(cat, true, false);
        assert_eq!(selected(&app), ["b/x.bin", "b/y.bin"], "all but the oldest");
        app.recompute();
        assert!(
            !app.summary[cat.index()].click_selects(cat),
            "a second click clears"
        );
        app.select_category(cat, false, false);
        assert!(selected(&app).is_empty());
        // "Select all but the oldest" in the list follows the search.
        app.query = "x.bin".to_owned();
        app.keep_oldest(true);
        assert_eq!(selected(&app), ["b/x.bin"]);
    }

    #[test]
    fn idle_projects_follow_the_search() {
        let mut app = CleanerApp::blank(Settings::default());
        let idle = |name: &str| {
            let mut it = row(Category::BuildFolders, name, false);
            it.modified = 1;
            it
        };
        app.items = vec![idle("app/node_modules"), idle("web/node_modules")];
        app.query = "app/".to_owned();
        app.select_idle_projects();
        assert_eq!(selected(&app), ["app/node_modules"]);
    }

    #[test]
    fn never_touch_leaves_the_space_map() {
        let home = crate::rules::tests::tempdir("app-never");
        let documents = home.join("Documents");
        let taxes = documents.join("Taxes");
        let node = |path: &Path| space::Node {
            path: path.to_path_buf(),
            name: String::new(),
            size: 1,
            files: 1,
            protected: false,
            measured: true,
        };
        let level = |path: &Path, nodes| space::Level {
            path: path.to_path_buf(),
            nodes,
            loose: 0,
        };
        let mut app = CleanerApp::blank(Settings::default());
        let other = documents.join("Other");
        app.space.levels.insert(
            documents.clone(),
            level(&documents, vec![node(&taxes), node(&other)]),
        );
        app.space
            .levels
            .insert(taxes.clone(), level(&taxes, Vec::new()));
        app.space.stack = vec![home.clone(), documents.clone(), taxes.clone()];
        app.never_touch(std::slice::from_ref(&taxes));
        assert!(!app.space.levels.contains_key(&taxes));
        let names: Vec<&Path> = app.space.levels[&documents]
            .nodes
            .iter()
            .map(|n| n.path.as_path())
            .collect();
        assert_eq!(names, [other.as_path()]);
        assert_eq!(app.space.stack, [home, documents], "back out of the folder");
    }

    #[test]
    fn a_finished_scan_gives_the_app_its_folders() {
        let mut app = CleanerApp::blank(Settings::default());
        let home = crate::rules::tests::tempdir("app-scan");
        let downloads = home.parent().unwrap().join("elsewhere-downloads");
        let output = scan::Output {
            items: Vec::new(),
            candidates: Vec::new(),
            fda: None,
            notes: Vec::new(),
            map: Vec::new(),
            home_files: 0,
            env: Some(crate::rules::Env {
                home: home.clone(),
                temp: home.join("tmp"),
                downloads: downloads.clone(),
                local: home.join("Library"),
                cloud: vec![home.join("Dropbox")],
            }),
        };
        app.finish_scan(output, &egui::Context::default());
        assert_eq!(app.home.as_ref(), Some(&home));
        assert_eq!(app.space.stack.len(), 1);
        assert_eq!(app.space.stack[0], home);
        assert!(
            app.space.levels[&home].nodes[0].protected,
            "the sync folder"
        );
        // Each duplicate belongs to the walk root it came from (rule S2).
        let cand = |path: PathBuf| dupes::Cand {
            path,
            len: 1,
            ino: 0,
            mtime: 0,
        };
        app.add_duplicates(vec![vec![
            cand(home.join("a.bin")),
            cand(downloads.join("a.bin")),
        ]]);
        let roots: Vec<&Path> = app.items.iter().map(|it| it.root.as_path()).collect();
        assert_eq!(roots, [home.as_path(), downloads.as_path()]);
        assert_eq!(app.dupe_files.len(), 2);
    }

    #[test]
    fn a_clean_report_updates_the_rows() {
        let mut app = CleanerApp::blank(Settings::default());
        let mut trash = Item::new(
            Category::Trash,
            "Trash",
            vec![PathBuf::from("/h/.Trash")],
            PathBuf::new(),
            Shape::Contents,
        );
        trash.size = 120;
        trash.selected = true;
        app.items = vec![
            trash,
            row(Category::LargeFiles, "big.iso", true),
            copy("a/x.bin", 1, 10, true),
            copy("b/x.bin", 1, 20, false),
        ];
        app.dupe_files = app.items[2..]
            .iter()
            .map(|it| clean::GroupFile {
                group: it.dupe_group,
                path: it.paths[0].clone(),
                modified: it.modified,
                ident: scan::Ident {
                    ino: 0,
                    len: 0,
                    mtime: 0,
                    dir: false,
                },
            })
            .collect();
        // The Trash was emptied, big.iso went in, and rule S10 refused the oldest copy.
        let report = clean::Report {
            trashed: 1_000,
            finished: vec![0, 1],
            kept: vec![2],
            moved: vec![(1, PathBuf::from("/h/big.iso"), None)],
            ..clean::Report::default()
        };
        app.apply_report(report, &egui::Context::default());
        assert_eq!(selected(&app), Vec::<String>::new(), "nothing stays ticked");
        assert!(!app.items.iter().any(|it| it.name == "big.iso"));
        let row = app.items.iter().find(|it| it.category == Category::Trash);
        assert_eq!(
            row.map(|it| it.size),
            Some(1_000),
            "the row holds big.iso now"
        );
        assert_eq!(app.dupe_files.len(), 2, "rule S10 still counts every copy");
        assert_eq!(app.undo.as_ref().map(|u| u.items.len()), Some(1));
    }

    #[test]
    fn put_back_shrinks_the_trash_row() {
        let mut app = CleanerApp::blank(Settings::default());
        let mut trash = Item::new(
            Category::Trash,
            "Trash",
            vec![PathBuf::from("/h/.Trash")],
            PathBuf::new(),
            Shape::Contents,
        );
        trash.size = 1_000;
        app.items = vec![trash];
        let big = row(Category::LargeFiles, "big.iso", false);
        let original = big.paths[0].clone();
        app.undo = Some(Undo {
            items: vec![(big, original.clone(), None)],
            since: 0,
            running: None,
            result: None,
        });
        app.apply_restore(vec![(original, Ok(()))]);
        assert_eq!(app.items[0].size, 0, "what came back left the Trash");
        assert!(app.items.iter().any(|it| it.name == "big.iso"));
    }

    #[test]
    fn the_trash_row_grows_by_what_a_clean_moved_there() {
        let mut trash = Item::new(
            Category::Trash,
            "Trash",
            vec![PathBuf::from("/h/.Trash")],
            PathBuf::new(),
            Shape::Contents,
        );
        trash.size = 120;
        let file = Item::new(
            Category::LargeFiles,
            "a.iso",
            vec![PathBuf::from("/h/a.iso")],
            PathBuf::from("/h"),
            Shape::Entry,
        );
        let mut items = vec![trash, file];
        refill_trash_rows(&mut items, &mut HashSet::new(), 1_900, 0);
        assert_eq!(items[0].size, 2_020);
        assert_eq!(items[1].size, 0);

        // The same clean emptied the Trash first: the row stays, holding just the new files.
        items[0].selected = true;
        let mut gone = HashSet::from([0, 1]);
        refill_trash_rows(&mut items, &mut gone, 500, 0);
        assert_eq!(items[0].size, 500);
        assert!(
            !items[0].selected,
            "so the next clean does not empty it again"
        );
        assert_eq!(gone, HashSet::from([1]));
    }

    #[test]
    fn the_oldest_copy_wins_ties_by_path() {
        let copy = |path: &str, modified: i64| {
            let mut it = Item::new(
                Category::Duplicates,
                path,
                vec![PathBuf::from(path)],
                PathBuf::from("/h"),
                Shape::Entry,
            );
            it.dupe_group = 1;
            it.modified = modified;
            it
        };
        // Equal times, as when one archive was unzipped twice: the smaller path wins,
        // wherever the rows sit in the list.
        let items = [
            copy("/h/b/zeta.bin", 10),
            copy("/h/a/alpha.bin", 10),
            copy("/h/c/new.bin", 5_000),
        ];
        assert_eq!(oldest_copies(&items)[&1], 1);
    }
}
