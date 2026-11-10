//! The window: header, sidebar, item list, footer and dialogs.
//!
//! The app redraws only on input, and every 250 ms while a scan or clean runs.
//! All row text is formatted once, when results arrive.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use eframe::egui::{
    self, Align, Align2, Button, Checkbox, Color32, ComboBox, CornerRadius, FontId, Frame, Id,
    Label, Layout, Margin, Modal, Rect, RichText, ScrollArea, Sense, Stroke, TextureHandle, Ui,
    pos2, vec2,
};
use egui_extras::{Column, TableBuilder};

use crate::clean;
use crate::fmt;
use crate::platform::{self, Disk};
use crate::rules::{Action, CATEGORY_COUNT, Category};
use crate::scan::{self, Item, Shape, Status, dupes};
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
/// Categories that the home pass fills; their size is unknown until it ends.
const HOME_CATEGORIES: [Category; 4] = [
    Category::BuildFolders,
    Category::LargeFiles,
    Category::OldDownloads,
    Category::Duplicates,
];

enum Phase {
    Idle,
    Scanning(scan::Handle),
    Cleaning(clean::Handle),
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
    /// Items that "Select all" may select: not running, not blocked.
    bulk: u64,
    bulk_selected: u64,
    text: String,
}

struct Plan {
    delete: (u64, u64),
    trash: (u64, u64),
    empty: Option<u64>,
}

pub struct CleanerApp {
    icon: Option<TextureHandle>,
    disk: Option<(Disk, String)>,
    disk_rx: Option<mpsc::Receiver<Option<Disk>>>,
    phase: Phase,
    live: Option<(Vec<Item>, Arc<Vec<scan::Live>>)>,
    scanned: bool,
    items: Vec<Item>,
    candidates: Vec<dupes::Cand>,
    finder: Option<dupes::Handle>,
    dupes_done: bool,
    cat: Category,
    threshold: u64,
    rows: Vec<Row>,
    group_labels: Vec<String>,
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
            disk: None,
            disk_rx: None,
            phase: Phase::Idle,
            live: None,
            scanned: false,
            items: Vec::new(),
            candidates: Vec::new(),
            finder: None,
            dupes_done: false,
            cat: Category::SystemJunk,
            threshold: 500 * fmt::MB,
            rows: Vec::new(),
            group_labels: Vec::new(),
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
            dirty: true,
        };
        app.probe_disk(&cc.egui_ctx);
        #[cfg(feature = "autoscan")]
        {
            if let Ok(view) = std::env::var("CLEANER_VIEW") {
                app.cat = Category::ALL
                    .into_iter()
                    .find(|c| c.name().eq_ignore_ascii_case(&view))
                    .unwrap_or(app.cat);
            }
            app.start_scan(&cc.egui_ctx);
        }
        app
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
        !matches!(self.phase, Phase::Idle) || self.finder.is_some()
    }

    fn idle(&self) -> bool {
        matches!(self.phase, Phase::Idle) && self.finder.is_none()
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
            self.finish_scan(*output);
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

        if self.dirty {
            self.recompute();
        }
    }

    fn start_scan(&mut self, ctx: &egui::Context) {
        let input = scan::Input {
            running: platform::running_apps(),
            low_power: platform::low_power(),
        };
        self.low_power = input.low_power;
        let ctx = ctx.clone();
        self.phase = Phase::Scanning(scan::start(input, move || ctx.request_repaint()));
        self.items.clear();
        self.candidates.clear();
        self.finder = None;
        self.dupes_done = false;
        self.report = None;
        self.dirty = true;
    }

    fn finish_scan(&mut self, output: scan::Output) {
        self.phase = Phase::Idle;
        self.live = None;
        self.scanned = true;
        self.items = output.items;
        self.candidates = output.candidates;
        self.fda = output.fda;
        self.notes = output.notes;
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
        let env = platform::env();
        let now = fmt::now();
        for (g, group) in groups.into_iter().enumerate() {
            for c in group {
                let root = env
                    .as_ref()
                    .map(|e| {
                        if c.path.starts_with(&e.home) {
                            e.home.clone()
                        } else {
                            e.downloads.clone()
                        }
                    })
                    .unwrap_or_default();
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
                item.ident = Some(scan::Ident {
                    ino: c.ino,
                    len: c.len,
                    mtime: c.mtime,
                    dir: false,
                });
                item.finish_text(now);
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
        let mut groups: HashMap<u32, usize> = HashMap::new();
        for it in &self.items {
            if it.category == Category::Duplicates && it.dupe_group != 0 {
                *groups.entry(it.dupe_group).or_default() += 1;
            }
        }
        let ctx = ctx.clone();
        self.phase = Phase::Cleaning(clean::start(jobs, groups, self.low_power, move || {
            ctx.request_repaint()
        }));
    }

    fn apply_report(&mut self, report: clean::Report, ctx: &egui::Context) {
        self.phase = Phase::Idle;
        let now = fmt::now();
        for &(i, removed) in &report.partial {
            if let Some(it) = self.items.get_mut(i) {
                it.size = it.size.saturating_sub(removed);
                it.selected = false;
                it.finish_text(now);
            }
        }
        let gone: HashSet<usize> = report.finished.iter().copied().collect();
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
        // The same file can be listed twice (a large file that is also a duplicate).
        self.items
            .retain(|it| it.shape != Shape::Entry || !gone_paths.contains(&it.paths[0]));
        self.candidates.retain(|c| !gone_paths.contains(&c.path));
        // A duplicate group with one file left is no longer a group.
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
        self.report = Some(report);
        self.dirty = true;
        self.probe_disk(ctx);
    }

    // ---- selection --------------------------------------------------------------------

    fn visible(&self, it: &Item) -> bool {
        it.category != Category::LargeFiles || it.size >= self.threshold
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
        let mut idx: Vec<usize> = (0..self.items.len())
            .filter(|&i| self.items[i].category == self.cat && self.visible(&self.items[i]))
            .collect();
        if self.cat == Category::Duplicates {
            idx.sort_by(|&a, &b| {
                let (x, y) = (&self.items[a], &self.items[b]);
                x.dupe_group
                    .cmp(&y.dupe_group)
                    .then(x.modified.cmp(&y.modified))
            });
            let mut last = 0;
            for i in idx {
                let g = self.items[i].dupe_group;
                if g != last {
                    last = g;
                    let (n, size) = groups.get(&g).copied().unwrap_or((0, 0));
                    self.group_labels.push(format!(
                        "{n} copies of {}  ·  {} can be freed",
                        fmt::bytes(size),
                        fmt::bytes(size * n.saturating_sub(1))
                    ));
                    self.rows.push(Row::Group(self.group_labels.len() - 1));
                }
                self.rows.push(Row::Item(i));
            }
        } else {
            idx.sort_by(|&a, &b| {
                self.items[b]
                    .size
                    .cmp(&self.items[a].size)
                    .then_with(|| self.items[a].name.cmp(&self.items[b].name))
            });
            self.rows = idx.into_iter().map(Row::Item).collect();
        }
    }

    fn select_category(&mut self, cat: Category, select: bool) {
        if cat == Category::Duplicates && select {
            self.keep_oldest();
            return;
        }
        let threshold = self.threshold;
        for it in self.items.iter_mut().filter(|it| it.category == cat) {
            if cat == Category::LargeFiles && it.size < threshold {
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

    /// Selects every duplicate except the oldest copy of each group.
    fn keep_oldest(&mut self) {
        let mut oldest: HashMap<u32, (i64, usize)> = HashMap::new();
        for (i, it) in self
            .items
            .iter()
            .enumerate()
            .filter(|(_, it)| it.category == Category::Duplicates)
        {
            let entry = oldest.entry(it.dupe_group).or_insert((it.modified, i));
            if it.modified < entry.0 {
                *entry = (it.modified, i);
            }
        }
        for (i, it) in self
            .items
            .iter_mut()
            .enumerate()
            .filter(|(_, it)| it.category == Category::Duplicates)
        {
            it.selected = it.selectable()
                && oldest
                    .get(&it.dupe_group)
                    .is_some_and(|&(_, keep)| keep != i);
        }
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

    // ---- views ---------------------------------------------------------------------------

    fn header(&mut self, ui: &mut Ui) {
        let (mut start, mut cancel) = (false, false);
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
                        start = ui
                            .add_enabled(
                                self.finder.is_none(),
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
    }

    fn sidebar(&mut self, ui: &mut Ui) {
        ui.add_space(6.0);
        ui.label(RichText::new("CATEGORIES").size(11.0).color(MUTED));
        ui.add_space(4.0);
        let live = self.live_totals();
        let scanning = matches!(self.phase, Phase::Scanning(_));
        let idle = self.idle();
        let (mut open, mut toggle) = (None, None);
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
            let (row, check) = category_row(
                ui,
                cat.name(),
                &size,
                self.cat == cat,
                all,
                some,
                idle && s.bulk > 0,
            );
            if row {
                open = Some(cat);
            }
            if check {
                toggle = Some((cat, !all));
            }
        }
        if let Some(cat) = open
            && cat != self.cat
        {
            self.cat = cat;
            self.dirty = true;
        }
        if let Some((cat, select)) = toggle {
            self.select_category(cat, select);
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
        self.title_row(ui);
        self.banners(ui);
        if self.cat == Category::Duplicates && !self.dupes_done {
            self.duplicates_view(ui);
            return;
        }
        if self.rows.is_empty() {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                let text = if self.cat == Category::Trash {
                    "The Trash is empty."
                } else {
                    "Nothing to clean here."
                };
                ui.label(RichText::new(text).size(15.0).color(MUTED));
            });
            return;
        }
        self.table(ui);
    }

    fn welcome(&mut self, ui: &mut Ui) {
        let mut start = false;
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() * 0.2).max(16.0));
            if let Some(icon) = &self.icon {
                ui.add(egui::Image::new(icon).fit_to_exact_size(vec2(112.0, 112.0)));
            }
            ui.add_space(14.0);
            ui.label(RichText::new("Find junk and forgotten files").size(24.0).strong().color(TEXT));
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
            start = ui.add_enabled(!self.elevated, primary("Scan").min_size(vec2(150.0, 38.0))).clicked();
            if self.elevated {
                ui.add_space(10.0);
                ui.label(RichText::new("Cleaner is running as an administrator. Start it as a normal user.").color(WARNING));
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

    fn title_row(&mut self, ui: &mut Ui) {
        let cat = self.cat;
        let s = self.summary[cat.index()].clone();
        let idle = self.idle();
        let (mut select, mut keep) = (None, false);
        let mut threshold = self.threshold;
        ui.horizontal(|ui| {
            ui.label(RichText::new(cat.name()).size(20.0).strong().color(TEXT));
            if !s.text.is_empty() {
                ui.label(RichText::new(&s.text).size(15.0).color(MUTED));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if cat == Category::Duplicates {
                    if self.dupes_done && s.bulk > 0 {
                        keep = ui
                            .add_enabled(idle, Button::new("Select all but the oldest"))
                            .clicked();
                    }
                } else if s.bulk > 0 {
                    let all = s.bulk_selected == s.bulk;
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
                if cat == Category::LargeFiles {
                    ComboBox::from_id_salt("large-limit")
                        .selected_text(format!("{} or more", short_size(threshold)))
                        .width(132.0)
                        .show_ui(ui, |ui| {
                            for t in THRESHOLDS {
                                ui.selectable_value(
                                    &mut threshold,
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
        if threshold != self.threshold {
            self.threshold = threshold;
            self.dirty = true;
        }
        if let Some(select) = select {
            self.select_category(cat, select);
        }
        if keep {
            self.keep_oldest();
        }
    }

    fn banners(&mut self, ui: &mut Ui) {
        if self.elevated {
            banner(
                ui,
                WARNING,
                "Cleaner is running as an administrator, so cleaning is off. Start it as a normal user.",
                None,
            );
        }
        let blocked = self.items.iter().any(|it| {
            it.category == self.cat && matches!(it.status, Status::NoAccess | Status::Unmeasured)
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
            self.cat,
            Category::LargeFiles | Category::OldDownloads | Category::Duplicates
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
                let (read, total) = (finder.progress.read.load(Relaxed), finder.progress.total.load(Relaxed));
                ui.label(RichText::new("Comparing files…").size(17.0).color(TEXT));
                ui.add_space(4.0);
                ui.label(RichText::new(format!("{} read of {} at most", fmt::bytes(read), fmt::bytes(total))).color(MUTED));
                ui.add_space(8.0);
                static_bar(ui, read as f32 / total.max(1) as f32, 280.0);
                ui.add_space(10.0);
                stop = ui.button("Stop").clicked();
            } else {
                let n = self.candidates.len() as u64;
                ui.label(RichText::new(format!("{} files of 1 MB or more can be compared.", fmt::count(n))).size(15.0).color(TEXT));
                ui.add_space(4.0);
                ui.label(
                    RichText::new("Files with a unique size are never read. Others are read only until they differ.")
                        .color(MUTED),
                );
                ui.add_space(16.0);
                go = ui.add_enabled(idle && n > 1, primary("Find duplicates").min_size(vec2(150.0, 34.0))).clicked();
            }
        });
        if go {
            self.find_duplicates(ui.ctx());
        }
        if stop && let Some(finder) = &self.finder {
            finder.cancel.store(true, Relaxed);
        }
    }

    fn table(&mut self, ui: &mut Ui) {
        let idle = self.idle();
        let rows = &self.rows;
        let labels = &self.group_labels;
        let items = &mut self.items;
        let mut changed = false;
        let mut reveal: Option<PathBuf> = None;
        TableBuilder::new(ui)
            .id_salt(self.cat)
            .striped(false)
            .resizable(false)
            .auto_shrink([false, false])
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::exact(30.0))
            .column(Column::remainder().at_least(220.0).clip(true))
            .column(Column::exact(96.0))
            .column(Column::exact(116.0))
            .column(Column::exact(58.0))
            .header(22.0, |mut header| {
                header.col(|_| {});
                header.col(|ui| {
                    ui.label(RichText::new("Name").small().color(MUTED));
                });
                header.col(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(RichText::new("Size").small().color(MUTED));
                    });
                });
                header.col(|ui| {
                    ui.add_space(14.0);
                    ui.label(RichText::new("Changed").small().color(MUTED));
                });
                header.col(|_| {});
            })
            .body(|body| {
                body.rows(ROW_HEIGHT, rows.len(), |mut row| match rows[row.index()] {
                    Row::Group(g) => {
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
                        row.col(|ui| name_cell(ui, it));
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
                            if !it.paths.is_empty()
                                && ui
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
                    }
                });
            });
        if changed {
            self.dirty = true;
        }
        if let Some(path) = reveal {
            platform::reveal(&path);
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
                let can = self.idle() && self.chosen_count > 0 && !self.elevated;
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
        if let Some(plan) = &self.plan {
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
                            "Caches, logs, temporary files and build folders. Apps re-create them.",
                        );
                    }
                    if plan.trash.1 > 0 {
                        plan_line(
                            ui,
                            "Move to the Trash",
                            plan.trash,
                            "Your own files. You can put them back from the Trash.",
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

        if let Some(report) = &self.report {
            let mut close = false;
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
                            .max_height(200.0)
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
                    ui.add_space(14.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        close = ui.add(primary("OK").min_size(vec2(80.0, 30.0))).clicked();
                    });
                });
            if close || response.should_close() {
                self.report = None;
            }
        }
    }
}

impl eframe::App for CleanerApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll(ctx);
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
        if self.busy() {
            ctx.request_repaint_after(BUSY_REPAINT);
        }
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

/// One sidebar row. Returns (row clicked, checkbox clicked).
fn category_row(
    ui: &mut Ui,
    name: &str,
    size: &str,
    current: bool,
    all: bool,
    some: bool,
    enabled: bool,
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
    let check_rect =
        Rect::from_center_size(pos2(rect.left() + 22.0, rect.center().y), vec2(18.0, 18.0));
    let mut checked = all;
    let check = ui
        .add_enabled_ui(enabled, |ui| {
            ui.put(
                check_rect,
                Checkbox::without_text(&mut checked).indeterminate(some && !all),
            )
        })
        .inner;
    let name_color = if current {
        TEXT
    } else {
        Color32::from_rgb(0xC6, 0xCB, 0xD6)
    };
    painter.text(
        pos2(rect.left() + 42.0, rect.center().y),
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
    (response.clicked(), check.clicked())
}

fn name_cell(ui: &mut Ui, it: &Item) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.horizontal(|ui| {
            ui.add(Label::new(RichText::new(&it.name).color(TEXT)).truncate());
            if !it.group.is_empty() {
                ui.label(RichText::new(&it.group).small().color(MUTED));
            }
            let tag = match it.status {
                Status::Running => Some(("running — quit the app first", WARNING)),
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
