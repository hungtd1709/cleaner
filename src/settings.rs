//! Settings kept between launches, in a small `key=value` text file:
//!
//! ```text
//! large_limit=524288000
//! sort=size_desc
//! never=/Users/me/Projects/keep
//! ```
//!
//! Unknown lines are ignored, so older and newer versions can share the file. A Never
//! touch path with a line break is written as `never_escaped=`, with `\\`, `\n` and `\r`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use crate::fmt;
use crate::platform;

/// The newest settings waiting to be written: the folder and the file's text.
static PENDING: Mutex<Option<(PathBuf, String)>> = Mutex::new(None);
/// True while a writer thread runs.
static WRITING: AtomicBool = AtomicBool::new(false);

fn pending() -> MutexGuard<'static, Option<(PathBuf, String)>> {
    PENDING.lock().unwrap_or_else(|p| p.into_inner())
}

/// Writes pending settings until none are left. Runs on its own thread.
fn write_pending() {
    loop {
        let next = pending().take();
        if let Some((dir, text)) = next {
            let _ = std::fs::create_dir_all(&dir);
            // A new file, then a rename: quitting midway leaves the old file whole.
            let tmp = dir.join("settings.txt.tmp");
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, dir.join("settings.txt"));
            }
            continue;
        }
        WRITING.store(false, SeqCst);
        // A save may have come in after `take` found nothing. Write it here, unless that
        // save started a writer of its own.
        if pending().is_none() || WRITING.swap(true, SeqCst) {
            return;
        }
    }
}

/// Waits up to two seconds for a running save, so the last change survives quitting.
pub fn flush() {
    for _ in 0..200 {
        if !WRITING.load(SeqCst) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Size,
    Changed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sort {
    pub key: SortKey,
    pub desc: bool,
}

impl Sort {
    fn text(self) -> &'static str {
        match (self.key, self.desc) {
            (SortKey::Name, false) => "name_asc",
            (SortKey::Name, true) => "name_desc",
            (SortKey::Size, false) => "size_asc",
            (SortKey::Size, true) => "size_desc",
            (SortKey::Changed, false) => "changed_asc",
            (SortKey::Changed, true) => "changed_desc",
        }
    }

    fn parse(text: &str) -> Option<Sort> {
        let (key, desc) = match text {
            "name_asc" => (SortKey::Name, false),
            "name_desc" => (SortKey::Name, true),
            "size_asc" => (SortKey::Size, false),
            "size_desc" => (SortKey::Size, true),
            "changed_asc" => (SortKey::Changed, false),
            "changed_desc" => (SortKey::Changed, true),
            _ => return None,
        };
        Some(Sort { key, desc })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub large_limit: u64,
    pub sort: Sort,
    /// Folders and files that Cleaner never lists and never removes.
    pub never: Vec<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            large_limit: 500 * fmt::MB,
            sort: Sort {
                key: SortKey::Size,
                desc: true,
            },
            never: Vec::new(),
        }
    }
}

impl Settings {
    pub fn load() -> Settings {
        platform::config_dir()
            .and_then(|dir| std::fs::read_to_string(dir.join("settings.txt")).ok())
            .map_or_else(Settings::default, |text| Settings::parse(&text))
    }

    /// Saves on a short-lived thread, so a slow disk, such as a settings folder on a network
    /// share, never freezes the window. Only the newest settings are written, and the file
    /// is replaced in one step, so it is never half written. Quiet: settings are a
    /// convenience, so a failure must not stop the app.
    pub fn save(&self) {
        if cfg!(test) {
            return; // Tests must not touch the user's settings.
        }
        let Some(dir) = platform::config_dir() else {
            return;
        };
        *pending() = Some((dir, self.text()));
        if !WRITING.swap(true, SeqCst) {
            std::thread::spawn(write_pending);
        }
    }

    fn parse(text: &str) -> Settings {
        let mut s = Settings::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            match key {
                "large_limit" => {
                    if let Ok(v) = value.trim().parse() {
                        s.large_limit = v;
                    }
                }
                "sort" => {
                    if let Some(sort) = Sort::parse(value.trim()) {
                        s.sort = sort;
                    }
                }
                "never" | "never_escaped" => {
                    let path = PathBuf::from(if key == "never" {
                        value.to_owned()
                    } else {
                        unescape(value)
                    });
                    if path.is_absolute() && !s.never.contains(&path) {
                        s.never.push(path);
                    }
                }
                _ => {}
            }
        }
        s
    }

    fn text(&self) -> String {
        let mut out = format!(
            "large_limit={}\nsort={}\n",
            self.large_limit,
            self.sort.text()
        );
        for p in &self.never {
            let path = p.to_string_lossy();
            // One line per path. Older versions ignore `never_escaped`, as they ignore
            // any unknown key, instead of reading a broken path from a second line.
            if path.contains(['\n', '\r']) {
                out.push_str("never_escaped=");
                out.push_str(&escape(&path));
            } else {
                out.push_str("never=");
                out.push_str(&path);
            }
            out.push('\n');
        }
        out
    }

    /// Adds a path to the Never touch list. Returns false if it is not usable.
    pub fn add_never(&mut self, path: &Path) -> bool {
        if !path.is_absolute() || self.never.iter().any(|p| p == path) {
            return false;
        }
        self.never.push(path.to_path_buf());
        self.never.sort();
        true
    }
}

/// Writes `\` as `\\`, a line feed as `\n` and a carriage return as `\r`.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

/// Reads what `escape` wrote.
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_writer_saves_the_newest_settings_and_stops() {
        let dir = crate::rules::tests::tempdir("settings-writer");
        *pending() = Some((dir.clone(), "large_limit=7\n".to_owned()));
        WRITING.store(true, SeqCst);
        write_pending();
        assert!(!WRITING.load(SeqCst), "the writer ended");
        assert!(pending().is_none());
        assert_eq!(
            std::fs::read_to_string(dir.join("settings.txt")).unwrap(),
            "large_limit=7\n"
        );
        assert!(!dir.join("settings.txt.tmp").exists());
        flush();
    }

    #[test]
    fn keeps_a_path_with_a_line_break_on_one_line() {
        let mut s = Settings::default();
        assert!(s.add_never(Path::new("/Users/me/odd\nname\\x\r")));
        let text = s.text();
        assert_eq!(text.lines().count(), 3, "{text}");
        assert_eq!(Settings::parse(&text), s);
    }

    #[test]
    fn round_trips() {
        let mut s = Settings {
            large_limit: 123,
            sort: Sort {
                key: SortKey::Changed,
                desc: false,
            },
            ..Settings::default()
        };
        assert!(s.add_never(Path::new("/Users/me/keep")));
        assert!(!s.add_never(Path::new("relative/path")));
        assert!(!s.add_never(Path::new("/Users/me/keep")), "no duplicates");
        let back = Settings::parse(&s.text());
        assert_eq!(back, s);
        assert_eq!(
            Settings::parse("junk\nlarge_limit=x\n"),
            Settings::default()
        );
    }
}
