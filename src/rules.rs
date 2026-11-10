//! What Cleaner looks at: categories, cleaning targets, and paths it must never touch.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

/// The bundle id of this app. Its own cache folder is never listed.
pub const APP_ID: &str = "io.github.hungtd1709.cleaner";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Category {
    SystemJunk,
    OptionalCaches,
    Browsers,
    DevCaches,
    XcodeArchives,
    BuildFolders,
    Trash,
    LargeFiles,
    OldDownloads,
    Duplicates,
}

pub const CATEGORY_COUNT: usize = 10;

impl Category {
    pub const ALL: [Category; CATEGORY_COUNT] = [
        Category::SystemJunk,
        Category::OptionalCaches,
        Category::Browsers,
        Category::DevCaches,
        Category::XcodeArchives,
        Category::BuildFolders,
        Category::Trash,
        Category::LargeFiles,
        Category::OldDownloads,
        Category::Duplicates,
    ];

    pub fn index(self) -> usize {
        self as usize
    }

    /// Categories that exist on this OS.
    pub fn visible(self) -> bool {
        !(cfg!(windows) && self == Category::XcodeArchives)
    }

    pub fn name(self) -> &'static str {
        match self {
            Category::SystemJunk => "System junk",
            Category::OptionalCaches => {
                if cfg!(windows) {
                    "Shader caches"
                } else {
                    "Apple caches"
                }
            }
            Category::Browsers => "Browser caches",
            Category::DevCaches => "Developer caches",
            Category::XcodeArchives => "Xcode Archives",
            Category::BuildFolders => "Build folders",
            Category::Trash => {
                if cfg!(windows) {
                    "Recycle Bin"
                } else {
                    "Trash"
                }
            }
            Category::LargeFiles => "Large files",
            Category::OldDownloads => "Old Downloads",
            Category::Duplicates => "Duplicates",
        }
    }

    /// One or two sentences shown above the list.
    pub fn about(self) -> &'static str {
        match self {
            Category::SystemJunk => {
                "App caches, old logs and old temporary files. Apps re-create them when they need them."
            }
            Category::OptionalCaches => {
                if cfg!(windows) {
                    "DirectX shader caches. Games rebuild them, so the first start may stutter."
                } else {
                    "Caches of Apple's own apps and services. macOS rebuilds them, which can take a while."
                }
            }
            Category::Browsers => {
                "Cached web pages and scripts. Browsers download them again. Quit the browser before cleaning."
            }
            Category::DevCaches => {
                "Package and build caches. Your tools download or rebuild them, which takes time."
            }
            Category::XcodeArchives => {
                "Old app archives. They hold the symbols for crash reports. Selected archives go to the Trash."
            }
            Category::BuildFolders => {
                "node_modules and other build output in your projects. The next install or build re-creates them."
            }
            Category::Trash => "Files you already deleted. Emptying cannot be undone.",
            Category::LargeFiles => {
                "Big files in your home folder. Selected files go to the Trash."
            }
            Category::OldDownloads => {
                "Downloads not added or changed for 90 days. Selected items go to the Trash."
            }
            Category::Duplicates => {
                "Files with identical content. Keep one copy of each; the others go to the Trash."
            }
        }
    }

    pub fn action(self) -> Action {
        match self {
            Category::XcodeArchives
            | Category::LargeFiles
            | Category::OldDownloads
            | Category::Duplicates => Action::Trash,
            Category::Trash => Action::EmptyTrash,
            _ => Action::Delete,
        }
    }
}

/// What cleaning does to an item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Removed for good. Only for data that apps re-create.
    Delete,
    /// Moved to the Trash or Recycle Bin. For the user's own files.
    Trash,
    /// Empties the Trash or Recycle Bin.
    EmptyTrash,
}

/// How a target's roots become items.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Each child of the root is one item. Cleaning removes that child.
    Children,
    /// All roots together are one item. Cleaning empties each root but keeps it.
    Whole,
}

/// Which children of a `Children` root to list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub enum Filter {
    Any,
    /// Only names that start with `com.apple.`.
    Apple,
    /// Every name except those that start with `com.apple.`.
    NotApple,
}

impl Filter {
    pub fn allows(self, name: &OsStr) -> bool {
        let apple = name.to_string_lossy().starts_with("com.apple.");
        match self {
            Filter::Any => true,
            Filter::Apple => apple,
            Filter::NotApple => !apple,
        }
    }
}

/// One cleaning rule.
#[derive(Clone, Debug)]
pub struct Target {
    pub category: Category,
    pub name: String,
    pub roots: Vec<PathBuf>,
    pub mode: Mode,
    pub filter: Filter,
    /// Files changed more recently than this are kept. 0 means no limit.
    pub min_age_days: u32,
    /// The app that owns the data: a bundle id (macOS) or an `.exe` name (Windows).
    pub app_id: Option<String>,
    /// Whether the items start selected.
    pub selected: bool,
}

/// Folders the rules are built from. All paths are absolute and fully resolved.
#[derive(Clone, Debug)]
pub struct Env {
    pub home: PathBuf,
    pub temp: PathBuf,
    pub downloads: PathBuf,
    /// `%LOCALAPPDATA%` on Windows, `~/Library` on macOS.
    pub local: PathBuf,
    /// Roots of cloud-sync folders. Never walked.
    pub cloud: Vec<PathBuf>,
}

/// Returns the folder only if it exists, is a real folder (not a link), and resolves cleanly.
pub fn resolve_dir(path: &Path) -> Option<PathBuf> {
    let md = std::fs::symlink_metadata(path).ok()?;
    if md.file_type().is_symlink() || !md.is_dir() {
        return None;
    }
    std::fs::canonicalize(path).ok()
}

/// Expands a pattern such as `Google/Chrome/User Data/*/Cache` below `base`.
/// A `*` part matches every child folder. Only existing folders are returned.
pub fn glob(base: &Path, pattern: &str) -> Vec<PathBuf> {
    let mut found = vec![base.to_path_buf()];
    for part in pattern.split('/').filter(|p| !p.is_empty()) {
        let mut next = Vec::new();
        for dir in &found {
            if part == "*" {
                let Ok(read) = std::fs::read_dir(dir) else {
                    continue;
                };
                for entry in read.flatten() {
                    if entry.file_type().is_ok_and(|t| t.is_dir()) {
                        next.push(entry.path());
                    }
                }
            } else {
                let p = dir.join(part);
                if std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_dir()) {
                    next.push(p);
                }
            }
        }
        found = next;
    }
    found.sort();
    found
}

struct Builder {
    targets: Vec<Target>,
}

impl Builder {
    fn whole(
        &mut self,
        category: Category,
        name: &str,
        roots: Vec<PathBuf>,
        selected: bool,
        app: Option<&str>,
    ) {
        let mut roots: Vec<PathBuf> = roots.iter().filter_map(|r| resolve_dir(r)).collect();
        roots.sort();
        roots.dedup();
        if roots.is_empty() {
            return;
        }
        self.targets.push(Target {
            category,
            name: name.to_owned(),
            roots,
            mode: Mode::Whole,
            filter: Filter::Any,
            min_age_days: 0,
            app_id: app.map(str::to_owned),
            selected,
        });
    }

    fn children(
        &mut self,
        category: Category,
        name: &str,
        root: PathBuf,
        filter: Filter,
        min_age_days: u32,
        selected: bool,
    ) {
        let Some(root) = resolve_dir(&root) else {
            return;
        };
        self.targets.push(Target {
            category,
            name: name.to_owned(),
            roots: vec![root],
            mode: Mode::Children,
            filter,
            min_age_days,
            app_id: None,
            selected,
        });
    }
}

/// The cleaning targets for this OS. Targets whose folders do not exist are left out.
pub fn targets(env: &Env) -> Vec<Target> {
    let mut b = Builder {
        targets: Vec::new(),
    };
    os_targets(&mut b, env);
    b.targets
}

#[cfg(target_os = "macos")]
fn os_targets(b: &mut Builder, env: &Env) {
    use Category::*;
    let h = &env.home;
    let lib = &env.local;
    let caches = lib.join("Caches");
    let support = lib.join("Application Support");
    let dev = lib.join("Developer");
    let chromium = |cache: &str, support_dir: &str| {
        let mut roots = vec![caches.join(cache)];
        roots.extend(glob(&support, &format!("{support_dir}/*/Code Cache")));
        roots.extend(glob(&support, &format!("{support_dir}/*/GPUCache")));
        roots
    };

    b.children(
        SystemJunk,
        "App cache",
        caches.clone(),
        Filter::NotApple,
        0,
        true,
    );
    b.children(SystemJunk, "Logs", lib.join("Logs"), Filter::Any, 7, true);
    b.children(
        SystemJunk,
        "Temporary files",
        env.temp.clone(),
        Filter::Any,
        7,
        true,
    );
    b.children(
        OptionalCaches,
        "Apple cache",
        caches.clone(),
        Filter::Apple,
        0,
        false,
    );

    b.whole(
        Browsers,
        "Google Chrome",
        chromium("Google/Chrome", "Google/Chrome"),
        true,
        Some("com.google.Chrome"),
    );
    b.whole(
        Browsers,
        "Microsoft Edge",
        chromium("Microsoft Edge", "Microsoft Edge"),
        true,
        Some("com.microsoft.edgemac"),
    );
    b.whole(
        Browsers,
        "Brave",
        chromium("BraveSoftware/Brave-Browser", "BraveSoftware/Brave-Browser"),
        true,
        Some("com.brave.Browser"),
    );
    b.whole(
        Browsers,
        "Firefox",
        glob(&caches, "Firefox/Profiles/*/cache2"),
        true,
        Some("org.mozilla.firefox"),
    );
    b.whole(
        Browsers,
        "Safari",
        vec![caches.join("com.apple.Safari")],
        true,
        Some("com.apple.Safari"),
    );

    let xcode = Some("com.apple.dt.Xcode");
    b.whole(
        DevCaches,
        "Xcode DerivedData",
        vec![dev.join("Xcode/DerivedData")],
        false,
        xcode,
    );
    b.whole(
        DevCaches,
        "Xcode device support",
        ["iOS", "watchOS", "tvOS", "visionOS"]
            .iter()
            .map(|os| dev.join(format!("Xcode/{os} DeviceSupport")))
            .collect(),
        false,
        xcode,
    );
    b.whole(
        DevCaches,
        "Simulator caches",
        vec![dev.join("CoreSimulator/Caches")],
        false,
        None,
    );
    b.whole(
        DevCaches,
        "npm cache",
        vec![h.join(".npm/_cacache")],
        false,
        None,
    );
    b.whole(
        DevCaches,
        "Yarn cache",
        vec![caches.join("Yarn")],
        false,
        None,
    );
    b.whole(
        DevCaches,
        "pip cache",
        vec![caches.join("pip")],
        false,
        None,
    );
    b.whole(
        DevCaches,
        "Homebrew downloads",
        vec![caches.join("Homebrew")],
        false,
        None,
    );
    b.whole(
        DevCaches,
        "CocoaPods cache",
        vec![caches.join("CocoaPods")],
        false,
        None,
    );
    b.whole(
        DevCaches,
        "Cargo registry",
        vec![
            h.join(".cargo/registry/cache"),
            h.join(".cargo/registry/src"),
        ],
        false,
        None,
    );
    b.whole(
        DevCaches,
        "Go build cache",
        vec![caches.join("go-build")],
        false,
        None,
    );
    b.whole(
        DevCaches,
        "Go modules",
        vec![h.join("go/pkg/mod")],
        false,
        None,
    );
    b.whole(
        DevCaches,
        "Gradle caches",
        vec![h.join(".gradle/caches")],
        false,
        None,
    );
    b.whole(
        DevCaches,
        "Gradle wrappers",
        vec![h.join(".gradle/wrapper/dists")],
        false,
        None,
    );
    b.children(
        DevCaches,
        "Tool cache",
        h.join(".cache"),
        Filter::Any,
        0,
        false,
    );

    b.children(
        XcodeArchives,
        "Archives",
        dev.join("Xcode/Archives"),
        Filter::Any,
        0,
        false,
    );

    b.whole(Trash, "Trash", vec![h.join(".Trash")], false, None);
}

#[cfg(windows)]
fn os_targets(b: &mut Builder, env: &Env) {
    use Category::*;
    let h = &env.home;
    let local = &env.local;
    let chromium = |base: &str| {
        let mut roots = Vec::new();
        for cache in ["Cache", "Code Cache", "GPUCache"] {
            roots.extend(glob(local, &format!("{base}/User Data/*/{cache}")));
        }
        roots
    };

    b.children(
        SystemJunk,
        "Temporary files",
        env.temp.clone(),
        Filter::Any,
        7,
        true,
    );
    b.children(
        SystemJunk,
        "Crash dumps",
        local.join("CrashDumps"),
        Filter::Any,
        7,
        true,
    );
    b.children(
        SystemJunk,
        "Error reports",
        local.join(r"Microsoft\Windows\WER"),
        Filter::Any,
        7,
        true,
    );
    b.whole(
        SystemJunk,
        "Internet cache",
        vec![local.join(r"Microsoft\Windows\INetCache")],
        true,
        None,
    );
    b.whole(
        OptionalCaches,
        "DirectX shader cache",
        [
            "D3DSCache",
            r"NVIDIA\DXCache",
            r"NVIDIA\GLCache",
            r"AMD\DxCache",
            r"AMD\DxcCache",
        ]
        .iter()
        .map(|p| local.join(p))
        .collect(),
        false,
        None,
    );

    b.whole(
        Browsers,
        "Google Chrome",
        chromium("Google/Chrome"),
        true,
        Some("chrome.exe"),
    );
    b.whole(
        Browsers,
        "Microsoft Edge",
        chromium("Microsoft/Edge"),
        true,
        Some("msedge.exe"),
    );
    b.whole(
        Browsers,
        "Brave",
        chromium("BraveSoftware/Brave-Browser"),
        true,
        Some("brave.exe"),
    );
    b.whole(
        Browsers,
        "Firefox",
        glob(local, "Mozilla/Firefox/Profiles/*/cache2"),
        true,
        Some("firefox.exe"),
    );

    b.whole(
        DevCaches,
        "npm cache",
        vec![local.join(r"npm-cache\_cacache")],
        false,
        None,
    );
    b.whole(
        DevCaches,
        "Yarn cache",
        vec![local.join(r"Yarn\Cache")],
        false,
        None,
    );
    b.whole(
        DevCaches,
        "pip cache",
        vec![local.join(r"pip\Cache")],
        false,
        None,
    );
    b.whole(
        DevCaches,
        "NuGet HTTP cache",
        vec![local.join(r"NuGet\v3-cache")],
        false,
        None,
    );
    b.whole(
        DevCaches,
        "Cargo registry",
        vec![
            h.join(r".cargo\registry\cache"),
            h.join(r".cargo\registry\src"),
        ],
        false,
        None,
    );
    b.whole(
        DevCaches,
        "Go build cache",
        vec![local.join("go-build")],
        false,
        None,
    );
    b.whole(
        DevCaches,
        "Go modules",
        vec![h.join(r"go\pkg\mod")],
        false,
        None,
    );
    b.whole(
        DevCaches,
        "Gradle caches",
        vec![h.join(r".gradle\caches")],
        false,
        None,
    );
    b.whole(
        DevCaches,
        "Gradle wrappers",
        vec![h.join(r".gradle\wrapper\dists")],
        false,
        None,
    );
    b.children(
        DevCaches,
        "Tool cache",
        h.join(".cache"),
        Filter::Any,
        0,
        false,
    );

    // The Recycle Bin has no folder we walk; the platform layer measures and empties it.
    b.targets.push(Target {
        category: Trash,
        name: "Recycle Bin".to_owned(),
        roots: Vec::new(),
        mode: Mode::Whole,
        filter: Filter::Any,
        min_age_days: 0,
        app_id: None,
        selected: false,
    });
}

#[cfg(not(any(target_os = "macos", windows)))]
fn os_targets(_b: &mut Builder, _env: &Env) {}

/// `~/Library/Caches` children that must never be listed: removing them makes iCloud,
/// the App Store or system services re-sync, or breaks downloads that are running.
pub const CACHE_DENY: &[&str] = &[
    "CloudKit",
    "FamilyCircle",
    "familycircled",
    "com.apple.akd",
    "com.apple.ap.adprivacyd",
    "com.apple.appstoreagent",
    "com.apple.bird",
    "com.apple.commerce",
    "com.apple.containermanagerd",
    "com.apple.HomeKit",
    "com.apple.homed",
    "com.apple.mediaanalysisd",
    "com.apple.nsurlsessiond",
    "com.apple.photoanalysisd",
    "com.apple.Spotlight",
    "com.apple.storekitagent",
    APP_ID,
];

/// Folder extensions that Finder shows as one file (bundles). We never select files inside them.
pub const BUNDLE_EXTS: &[&str] = &[
    "app",
    "appex",
    "bundle",
    "dsym",
    "fcpbundle",
    "framework",
    "imovielibrary",
    "kext",
    "key",
    "logicx",
    "band",
    "musiclibrary",
    "numbers",
    "pages",
    "photoslibrary",
    "playground",
    "plugin",
    "pvm",
    "rtfd",
    "sparsebundle",
    "tvlibrary",
    "utm",
    "vmwarevm",
    "xcarchive",
    "xcodeproj",
    "xcworkspace",
];

/// Shown so the user knows about them, but never selectable: disk images of virtual
/// machines, media libraries and mail archives.
pub const INFO_ONLY_EXTS: &[&str] = &[
    "fcpbundle",
    "imovielibrary",
    "musiclibrary",
    "ost",
    "photoslibrary",
    "pst",
    "pvm",
    "qcow2",
    "sparsebundle",
    "tvlibrary",
    "utm",
    "vdi",
    "vhd",
    "vhdx",
    "vmdk",
    "vmwarevm",
];

/// Folder names the home-folder pass never enters.
pub const WALK_SKIP: &[&str] = &[
    ".git",
    ".Trash",
    ".Trashes",
    "$RECYCLE.BIN",
    "System Volume Information",
    ".ssh",
    ".gnupg",
];

pub fn ext_in(name: &OsStr, list: &[&str]) -> bool {
    Path::new(name)
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|ext| list.iter().any(|e| e.eq_ignore_ascii_case(ext)))
}

/// Checks every path that cleaning wants to remove (safety rules S2, S7 and S8).
#[derive(Clone, Debug, Default)]
pub struct Guard {
    /// Never removed, and no folder that contains one is removed either.
    anchors: Vec<PathBuf>,
    /// Target roots. A root itself is never removed (rule S2). A root inside a removed
    /// folder is skipped by the deleter, which knows every target root.
    roots: Vec<PathBuf>,
    /// Nothing inside is ever removed.
    hard: Vec<PathBuf>,
    /// Nothing inside is removed, unless the item's own root is inside it too
    /// (for example a browser's `Code Cache` inside `Application Support`).
    soft: Vec<PathBuf>,
}

impl Guard {
    pub fn new(env: &Env, targets: &[Target]) -> Guard {
        let h = &env.home;
        let mut g = Guard::default();
        let mut anchors: Vec<PathBuf> = vec![
            h.clone(),
            env.temp.clone(),
            env.downloads.clone(),
            env.local.clone(),
        ];
        g.roots = targets
            .iter()
            .flat_map(|t| t.roots.iter().cloned())
            .collect();
        anchors.extend(env.cloud.iter().cloned());
        for name in [
            "Desktop",
            "Documents",
            "Downloads",
            "Pictures",
            "Movies",
            "Music",
            "Videos",
            "Public",
            "OneDrive",
        ] {
            anchors.push(h.join(name));
        }
        #[cfg(target_os = "macos")]
        {
            for p in [
                "/",
                "/Applications",
                "/Library",
                "/System",
                "/Users",
                "/Volumes",
                "/private",
                "/private/var",
            ] {
                anchors.push(PathBuf::from(p));
            }
            let lib = &env.local;
            for p in [
                "Caches",
                "Logs",
                "Application Support",
                "Developer",
                "Containers",
                "Group Containers",
                "Preferences",
            ] {
                anchors.push(lib.join(p));
            }
            g.hard = vec![
                lib.join("Keychains"),
                h.join(".ssh"),
                h.join(".gnupg"),
                h.join(".docker/config.json"),
                h.join(".npmrc"),
                h.join(".cargo/credentials"),
                h.join(".cargo/credentials.toml"),
                h.join(".gradle/gradle.properties"),
            ];
            g.soft = [
                "Application Support",
                "Containers",
                "Group Containers",
                "Mobile Documents",
                "CloudStorage",
                "Mail",
                "Messages",
                "Safari",
                "Accounts",
            ]
            .iter()
            .map(|p| lib.join(p))
            .collect();
        }
        #[cfg(windows)]
        {
            if let Some(drive) = h.ancestors().last() {
                anchors.push(drive.to_path_buf());
            }
            for var in [
                "SystemRoot",
                "ProgramFiles",
                "ProgramFiles(x86)",
                "ProgramData",
                "PUBLIC",
            ] {
                if let Some(p) = std::env::var_os(var)
                    .map(PathBuf::from)
                    .and_then(|p| resolve_dir(&p))
                {
                    anchors.push(p);
                }
            }
            if let Some(appdata) = env.local.parent() {
                anchors.push(appdata.to_path_buf());
                anchors.push(appdata.join("Roaming"));
                anchors.push(appdata.join("LocalLow"));
                g.soft.push(appdata.join("Roaming"));
            }
            if let Some(users) = h.parent() {
                anchors.push(users.to_path_buf());
            }
            g.soft.push(env.local.join("Packages"));
            g.hard = vec![
                env.local.join(r"Microsoft\WindowsApps"),
                h.join(".ssh"),
                h.join(".gnupg"),
                h.join(r".docker\config.json"),
                h.join(".npmrc"),
                h.join(r".cargo\credentials"),
                h.join(r".cargo\credentials.toml"),
                h.join(r".gradle\gradle.properties"),
            ];
        }
        anchors.sort();
        anchors.dedup();
        g.anchors = anchors;
        g
    }

    /// May cleaning remove `path`, an item that belongs to `root`?
    pub fn check(&self, path: &Path, root: &Path) -> Result<(), &'static str> {
        if !is_plain_absolute(path) {
            return Err("unexpected path");
        }
        if path == root || !path.starts_with(root) {
            return Err("outside the folder it belongs to");
        }
        if self.anchors.iter().any(|a| a.starts_with(path)) || self.roots.iter().any(|r| r == path)
        {
            return Err("protected folder");
        }
        if self.hard.iter().any(|d| path.starts_with(d)) {
            return Err("protected file");
        }
        if self
            .soft
            .iter()
            .any(|d| path.starts_with(d) && !root.starts_with(d))
        {
            return Err("app data");
        }
        if path.components().any(|c| c.as_os_str() == ".git") {
            return Err("inside a Git folder");
        }
        if is_ntuser(path) {
            return Err("protected file");
        }
        Ok(())
    }

    /// Paths that a folder walk must not enter or count.
    pub fn walk_denied(&self, path: &Path) -> bool {
        self.hard.iter().any(|d| path.starts_with(d))
            || self.soft.iter().any(|d| path.starts_with(d))
    }

    /// Paths inside a tree that is being deleted that must be left alone.
    pub fn hard_denied(&self, path: &Path) -> bool {
        self.hard.iter().any(|d| path.starts_with(d)) || is_ntuser(path)
    }
}

/// `NTUSER.DAT` and its log files hold the Windows user profile.
fn is_ntuser(path: &Path) -> bool {
    path.file_name().is_some_and(|n| {
        n.to_string_lossy()
            .to_ascii_lowercase()
            .starts_with("ntuser.")
    })
}

/// Absolute, and free of `.` and `..` parts.
fn is_plain_absolute(path: &Path) -> bool {
    path.is_absolute()
        && !path
            .components()
            .any(|c| matches!(c, Component::CurDir | Component::ParentDir))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn env_in(dir: &Path) -> Env {
        let home = dir.join("home");
        for p in ["Library/Caches", "Library/Logs", "Downloads", "tmp", ".ssh"] {
            std::fs::create_dir_all(home.join(p)).unwrap();
        }
        let home = std::fs::canonicalize(home).unwrap();
        Env {
            temp: home.join("tmp"),
            downloads: home.join("Downloads"),
            local: home.join("Library"),
            home,
            cloud: Vec::new(),
        }
    }

    #[test]
    fn glob_matches_child_folders() {
        let dir = tempdir("glob");
        for p in ["a/1/Cache", "a/2/Cache", "a/3/Other"] {
            std::fs::create_dir_all(dir.join(p)).unwrap();
        }
        let found = glob(&dir, "a/*/Cache");
        assert_eq!(found, vec![dir.join("a/1/Cache"), dir.join("a/2/Cache")]);
        assert!(glob(&dir, "missing/*").is_empty());
    }

    #[test]
    fn guard_keeps_roots_anchors_and_secrets() {
        let dir = tempdir("guard");
        let env = env_in(&dir);
        let caches = env.local.join("Caches");
        let targets = vec![Target {
            category: Category::SystemJunk,
            name: "App cache".into(),
            roots: vec![caches.clone()],
            mode: Mode::Children,
            filter: Filter::Any,
            min_age_days: 0,
            app_id: None,
            selected: true,
        }];
        let g = Guard::new(&env, &targets);
        assert!(g.check(&caches.join("com.example"), &caches).is_ok());
        assert!(
            g.check(&caches, &caches).is_err(),
            "a root itself is never removed"
        );
        assert!(
            g.check(&env.home.join("x"), &caches).is_err(),
            "outside the root"
        );
        assert!(g.check(&caches.join("../x"), &caches).is_err(), "no `..`");
        assert!(
            g.check(&env.home.join("Downloads"), &env.home).is_err(),
            "anchor"
        );
        assert!(
            g.check(&env.home.join(".ssh/id_ed25519"), &env.home)
                .is_err(),
            "secret"
        );
        assert!(
            g.check(&env.home.join("p/.git/objects/x"), &env.home)
                .is_err(),
            "git"
        );
        assert!(g.check(&env.home.join("p/node_modules"), &env.home).is_ok());
    }

    #[test]
    fn filters_apple_names() {
        assert!(Filter::Apple.allows(OsStr::new("com.apple.Safari")));
        assert!(!Filter::Apple.allows(OsStr::new("com.spotify.client")));
        assert!(Filter::NotApple.allows(OsStr::new("com.spotify.client")));
        assert!(ext_in(
            OsStr::new("Photos Library.photoslibrary"),
            INFO_ONLY_EXTS
        ));
        assert!(!ext_in(OsStr::new("movie.mp4"), INFO_ONLY_EXTS));
    }

    pub(crate) fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cleaner-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(dir).unwrap()
    }
}
