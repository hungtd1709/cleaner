//! The Space map: a read-only view of where disk space goes.
//!
//! The scan already measured the folders in home, two levels deep. Deeper levels, and
//! `~/Library` (`AppData` on Windows), are measured when the user opens them. Nothing
//! here removes files.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, mpsc};

use crate::platform;
use crate::scan::{MapEntry, walk};

/// One folder in a level of the map.
#[derive(Clone, Debug)]
pub struct Node {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    pub files: u64,
    /// Not walked: privacy protection or cloud sync.
    pub protected: bool,
    /// The size is known.
    pub measured: bool,
}

/// The folders inside one folder, biggest first.
#[derive(Clone, Debug, Default)]
pub struct Level {
    pub path: PathBuf,
    pub nodes: Vec<Node>,
    /// Bytes in files that sit directly in this folder.
    pub loose: u64,
}

impl Level {
    pub fn total(&self) -> u64 {
        self.loose + self.nodes.iter().map(|n| n.size).sum::<u64>()
    }

    fn sort(&mut self) {
        self.nodes
            .sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
    }
}

fn name_of(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// Builds the levels the scan already measured: home, and each folder in home.
/// `skipped` is the folder the home pass leaves out (`Library` or `AppData`).
pub fn from_scan(
    home: &Path,
    map: &[MapEntry],
    home_files: u64,
    skipped: &[PathBuf],
) -> HashMap<PathBuf, Level> {
    let mut levels = HashMap::new();
    let mut top = Level {
        path: home.to_path_buf(),
        nodes: Vec::new(),
        loose: home_files,
    };
    for entry in map {
        top.nodes.push(Node {
            path: entry.path.clone(),
            name: name_of(&entry.path),
            size: entry.size,
            files: entry.files,
            protected: false,
            measured: true,
        });
        let mut level = Level {
            path: entry.path.clone(),
            nodes: Vec::new(),
            loose: entry
                .size
                .saturating_sub(entry.children.iter().map(|c| c.size).sum()),
        };
        for child in &entry.children {
            level.nodes.push(Node {
                path: child.path.clone(),
                name: name_of(&child.path),
                size: child.size,
                files: child.files,
                protected: false,
                measured: true,
            });
        }
        level.sort();
        levels.insert(entry.path.clone(), level);
    }
    for path in skipped {
        if path.is_dir() && !top.nodes.iter().any(|n| &n.path == path) {
            top.nodes.push(Node {
                path: path.clone(),
                name: name_of(path),
                size: 0,
                files: 0,
                protected: false,
                measured: false,
            });
        }
    }
    top.sort();
    levels.insert(home.to_path_buf(), top);
    levels
}

/// Folders the map never walks: walking them could show a privacy prompt, or download
/// cloud files.
pub fn protected(home: &Path, path: &Path) -> bool {
    const LIBRARY: &[&str] = &[
        "Containers",
        "Group Containers",
        "Mobile Documents",
        "CloudStorage",
        "Mail",
        "Messages",
        "Safari",
    ];
    let lib = home.join("Library");
    LIBRARY.iter().any(|p| path == lib.join(p))
}

/// A tip for folders of well-known tools: how to free their space safely.
pub fn hint(home: &Path, path: &Path) -> Option<&'static str> {
    const HINTS: &[(&str, &str)] = &[
        (
            ".rustup",
            "Rust toolchains. Remove unused ones with `rustup toolchain uninstall NAME`.",
        ),
        (
            ".rustup/toolchains",
            "Rust toolchains. Remove unused ones with `rustup toolchain uninstall NAME`.",
        ),
        (
            ".ollama",
            "Ollama models. Remove one with `ollama rm MODEL`.",
        ),
        (
            ".ollama/models",
            "Ollama models. Remove one with `ollama rm MODEL`.",
        ),
        (
            ".docker",
            "Docker data. Free space with `docker system prune`.",
        ),
        (
            ".orbstack",
            "OrbStack data. Free space with `docker system prune`.",
        ),
        (
            ".android/avd",
            "Android emulators. Delete unused ones in Android Studio's Device Manager.",
        ),
        (
            ".vscode/extensions",
            "VS Code extensions. Uninstall unused ones in VS Code.",
        ),
        (
            ".pyenv/versions",
            "Python versions. Remove one with `pyenv uninstall VERSION`.",
        ),
        (
            ".nvm/versions",
            "Node versions. Remove one with `nvm uninstall VERSION`.",
        ),
        (".nuget", "NuGet packages. Listed in Developer caches."),
        (".npm", "npm cache. Listed in Developer caches."),
        (
            ".gradle",
            "Gradle. Its caches are listed in Developer caches.",
        ),
        (".m2", "Maven repository. Listed in Developer caches."),
        (
            ".cargo",
            "Cargo. Its registry is listed in Developer caches.",
        ),
        (
            ".cache/huggingface",
            "Hugging Face models. Listed in Developer caches.",
        ),
        (
            ".local/share/claude",
            "Claude Code. Old versions are listed in App data.",
        ),
        (
            ".claude",
            "Claude Code. Old sessions are listed in App data.",
        ),
        (
            ".claude/projects",
            "Claude Code sessions. Old ones are listed in App data.",
        ),
        (
            "Library/Application Support/Claude",
            "Claude Desktop. Its caches and VM image are listed in App data.",
        ),
        (
            "Library/Application Support/Claude/vm_bundles",
            "Claude's VM image. Listed in App data.",
        ),
        (
            "Library/Developer/CoreSimulator",
            "iOS simulators. Remove unavailable ones with `xcrun simctl delete unavailable`.",
        ),
        (
            "Library/Developer/Xcode",
            "Xcode data. DerivedData and device support are in Developer caches.",
        ),
        (
            "Library/Application Support/MobileSync",
            "iPhone and iPad backups. Manage them in Finder.",
        ),
        (
            "Library/Containers/com.docker.docker",
            "Docker Desktop disk. Free space with `docker system prune`.",
        ),
        (
            "AppData/Local/Docker",
            "Docker Desktop disk. Free space with `docker system prune`.",
        ),
        (
            "AppData/Local/Android/Sdk",
            "Android SDK. Remove unused system images in Android Studio's SDK Manager.",
        ),
    ];
    let rel = path.strip_prefix(home).ok()?;
    HINTS
        .iter()
        .find(|(p, _)| rel == Path::new(p))
        .map(|(_, hint)| *hint)
}

/// A folder being measured in the background.
pub struct Measure {
    pub path: PathBuf,
    pub files: Arc<AtomicU64>,
    pub cancel: Arc<AtomicBool>,
    pub rx: mpsc::Receiver<Level>,
}

/// Measures the folders inside `dir`. `never` folders are left out.
pub fn measure(
    dir: PathBuf,
    home: PathBuf,
    never: Vec<PathBuf>,
    low_power: bool,
    repaint: impl Fn() + Send + 'static,
) -> Measure {
    let files = Arc::new(AtomicU64::new(0));
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let (f, c, path) = (files.clone(), cancel.clone(), dir.clone());
    std::thread::Builder::new()
        .name("space".into())
        .spawn(move || {
            let _activity = platform::begin_activity("Measuring folders");
            let level = measure_now(&path, &home, &never, low_power, &f, &c);
            if !c.load(Relaxed) {
                let _ = tx.send(level);
            }
            repaint();
        })
        .expect("start the measuring thread");
    Measure {
        path: dir,
        files,
        cancel,
        rx,
    }
}

fn measure_now(
    dir: &Path,
    home: &Path,
    never: &[PathBuf],
    low_power: bool,
    files: &AtomicU64,
    cancel: &AtomicBool,
) -> Level {
    let mut level = Level {
        path: dir.to_path_buf(),
        ..Level::default()
    };
    let skip = |p: &Path| never.iter().any(|n| p.starts_with(n));
    let Ok(entries) = walk::list(dir) else {
        return level;
    };
    let mut jobs = Vec::new();
    for e in entries {
        let path = dir.join(&e.name);
        match e.kind {
            walk::Kind::Dir if !e.mount && !e.cloud && !skip(&path) => {
                let protected = protected(home, &path);
                if !protected {
                    jobs.push((path.clone(), level.nodes.len()));
                }
                level.nodes.push(Node {
                    name: name_of(&path),
                    path,
                    size: 0,
                    files: 0,
                    protected,
                    measured: !protected,
                });
            }
            walk::Kind::File if !e.cloud => level.loose += e.alloc,
            _ => {}
        }
    }
    let sizes: Vec<AtomicU64> = level.nodes.iter().map(|_| AtomicU64::new(0)).collect();
    let counts: Vec<AtomicU64> = level.nodes.iter().map(|_| AtomicU64::new(0)).collect();
    walk::walk(
        jobs,
        platform::worker_count(low_power),
        cancel,
        |d, index: usize, listing, push| {
            let Ok(entries) = listing else { return };
            let (mut size, mut n) = (0u64, 0u64);
            for e in entries {
                match e.kind {
                    walk::Kind::Dir if !e.mount && !e.cloud => {
                        let path = d.join(&e.name);
                        if !skip(&path) && !protected(home, &path) {
                            push.push((path, index));
                        }
                    }
                    walk::Kind::File if !e.cloud => {
                        size += e.alloc;
                        n += 1;
                    }
                    _ => {}
                }
            }
            sizes[index].fetch_add(size, Relaxed);
            counts[index].fetch_add(n, Relaxed);
            files.fetch_add(n, Relaxed);
        },
    );
    for (i, node) in level.nodes.iter_mut().enumerate() {
        node.size = sizes[i].load(Relaxed);
        node.files = counts[i].load(Relaxed);
    }
    level.sort();
    level
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_child_folders() {
        let dir = crate::rules::tests::tempdir("space");
        std::fs::create_dir_all(dir.join("big/inner")).unwrap();
        std::fs::create_dir_all(dir.join("small")).unwrap();
        std::fs::create_dir_all(dir.join("skip")).unwrap();
        std::fs::write(dir.join("big/inner/a"), vec![1u8; 300_000]).unwrap();
        std::fs::write(dir.join("small/b"), vec![1u8; 10_000]).unwrap();
        std::fs::write(dir.join("skip/c"), vec![1u8; 500_000]).unwrap();
        std::fs::write(dir.join("loose"), vec![1u8; 5_000]).unwrap();
        let level = measure_now(
            &dir,
            &dir,
            &[dir.join("skip")],
            false,
            &AtomicU64::new(0),
            &AtomicBool::new(false),
        );
        let names: Vec<&str> = level.nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(
            names,
            ["big", "small"],
            "biggest first, never-touch left out"
        );
        assert!(level.nodes[0].size >= 300_000);
        assert!(level.loose >= 5_000);
        assert_eq!(
            hint(&dir, &dir.join(".rustup")).map(|h| &h[..4]),
            Some("Rust")
        );
        assert!(protected(&dir, &dir.join("Library/Containers")));
    }
}
