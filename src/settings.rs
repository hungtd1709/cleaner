//! Settings kept between launches, in a small `key=value` text file:
//!
//! ```text
//! large_limit=524288000
//! sort=size_desc
//! never=/Users/me/Projects/keep
//! ```
//!
//! Unknown lines are ignored, so older and newer versions can share the file.

use std::path::{Path, PathBuf};

use crate::fmt;
use crate::platform;

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

    /// Saves quietly: settings are a convenience, so a failure must not stop the app.
    pub fn save(&self) {
        if let Some(dir) = platform::config_dir() {
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(dir.join("settings.txt"), self.text());
        }
    }

    fn parse(text: &str) -> Settings {
        let mut s = Settings::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key.trim() {
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
                "never" => {
                    let path = PathBuf::from(value);
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
            out.push_str("never=");
            out.push_str(&p.to_string_lossy());
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

#[cfg(test)]
mod tests {
    use super::*;

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
