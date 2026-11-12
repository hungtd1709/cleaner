//! macOS calls: Foundation and AppKit through objc2, everything else through libc.

use std::collections::HashSet;
use std::ffi::{CString, OsString};
use std::fs::{File, Metadata};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::ProtocolObject;
use objc2_app_kit::NSWorkspace;
use objc2_foundation::{
    NSActivityOptions, NSArray, NSFileManager, NSNumber, NSObjectProtocol,
    NSOperatingSystemVersion, NSProcessInfo, NSString, NSURL, NSURLIsPackageKey,
    NSURLVolumeAvailableCapacityForImportantUsageKey, NSURLVolumeLocalizedNameKey,
    NSURLVolumeTotalCapacityKey,
};

use super::Disk;
use crate::rules::{self, Env};

// `libc` does not bind `setiopolicy_np`; the values come from <sys/resource.h>.
unsafe extern "C" {
    fn setiopolicy_np(iotype: libc::c_int, scope: libc::c_int, policy: libc::c_int) -> libc::c_int;
}
const IOPOL_TYPE_VFS_ATIME_UPDATES: libc::c_int = 2;
const IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES: libc::c_int = 3;
const IOPOL_SCOPE_PROCESS: libc::c_int = 0;
const IOPOL_ATIME_UPDATES_OFF: libc::c_int = 1;
const IOPOL_MATERIALIZE_DATALESS_FILES_OFF: libc::c_int = 1;

/// Process-wide I/O policy, set once at start-up:
/// reads do not update access times, and a read of a cloud-only file fails
/// instead of downloading it (safety rule S6).
pub fn init_process() {
    // SAFETY: plain system calls with constant arguments.
    unsafe {
        setiopolicy_np(
            IOPOL_TYPE_VFS_ATIME_UPDATES,
            IOPOL_SCOPE_PROCESS,
            IOPOL_ATIME_UPDATES_OFF,
        );
        setiopolicy_np(
            IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES,
            IOPOL_SCOPE_PROCESS,
            IOPOL_MATERIALIZE_DATALESS_FILES_OFF,
        );
    }
}

/// The per-user temporary folder (`/var/folders/…/T`), not the shared `/tmp`.
fn user_temp_dir() -> Option<PathBuf> {
    let mut buf = vec![0u8; 1024];
    // SAFETY: the buffer is valid for `buf.len()` bytes.
    let n = unsafe {
        libc::confstr(
            libc::_CS_DARWIN_USER_TEMP_DIR,
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    if n == 0 || n > buf.len() {
        return None;
    }
    buf.truncate(n - 1);
    Some(PathBuf::from(OsString::from_vec(buf)))
}

pub fn env() -> Option<Env> {
    let home = rules::resolve_dir(&std::env::home_dir()?)?;
    let temp = user_temp_dir()
        .and_then(|t| rules::resolve_dir(&t))
        .unwrap_or_default();
    let local = home.join("Library");
    // File Provider apps sync inside `~/Library`; older sync apps keep a folder in home.
    let mut cloud = vec![local.join("CloudStorage"), local.join("Mobile Documents")];
    cloud.extend(rules::home_sync_folders(&home));
    Some(Env {
        downloads: home.join("Downloads"),
        cloud,
        temp,
        local,
        home,
    })
}

pub fn system_disk() -> Option<Disk> {
    autoreleasepool(|_| {
        let url = NSURL::fileURLWithPath(&NSString::from_str("/"));
        // SAFETY: these are constant NSString keys exported by Foundation.
        let (name_key, total_key, free_key) = unsafe {
            (
                NSURLVolumeLocalizedNameKey,
                NSURLVolumeTotalCapacityKey,
                NSURLVolumeAvailableCapacityForImportantUsageKey,
            )
        };
        let values = url
            .resourceValuesForKeys_error(&NSArray::from_slice(&[name_key, total_key, free_key]))
            .ok()?;
        let number = |key: &NSString| {
            values
                .objectForKey(key)
                .and_then(|v| v.downcast::<NSNumber>().ok())
                .map(|n| n.unsignedLongLongValue())
        };
        let name = values
            .objectForKey(name_key)
            .and_then(|v| v.downcast::<NSString>().ok())
            .map_or_else(|| "Disk".to_owned(), |s| s.to_string());
        let total = number(total_key)?;
        // "Available for important usage" includes purgeable space, like Finder shows.
        let free = number(free_key).filter(|&f| f > 0).or_else(statfs_free)?;
        Some(Disk { name, total, free })
    })
}

fn statfs_free() -> Option<u64> {
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: the path is a valid C string and `st` is a valid out-pointer.
    let r = unsafe { libc::statfs(c"/".as_ptr(), &mut st) };
    (r == 0).then(|| st.f_bavail * u64::from(st.f_bsize))
}

pub fn reveal(path: &Path) {
    let path = path.to_path_buf();
    std::thread::spawn(move || {
        let _ = Command::new("/usr/bin/open").arg("-R").arg(path).status();
    });
}

/// Bundle ids of running apps. Called on the UI thread when a scan starts.
/// Running apps, lower case: each app's bundle id, its name and its executable's name.
pub fn running_apps() -> HashSet<String> {
    autoreleasepool(|_| {
        let mut out = HashSet::new();
        for app in NSWorkspace::sharedWorkspace().runningApplications().iter() {
            if let Some(id) = app.bundleIdentifier() {
                out.insert(id.to_string().to_lowercase());
            }
            if let Some(name) = app.localizedName() {
                out.insert(name.to_string().to_lowercase());
            }
            if let Some(exe) = app.executableURL().and_then(|u| u.lastPathComponent()) {
                out.insert(exe.to_string().to_lowercase());
            }
        }
        out
    })
}

/// Called at the start of each worker thread: QoS class "utility" runs the thread on
/// efficiency cores where possible and lowers its I/O priority.
pub fn worker_init() {
    // SAFETY: sets the QoS class of the calling thread only.
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0);
    }
}

fn sysctl_u32(name: &str) -> Option<u32> {
    let name = CString::new(name).ok()?;
    let mut value: u32 = 0;
    let mut len = std::mem::size_of::<u32>();
    // SAFETY: `value` and `len` are valid out-pointers of the advertised size.
    let r = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&raw mut value).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (r == 0 && len == std::mem::size_of::<u32>()).then_some(value)
}

/// Apple silicon: one worker per efficiency core. Intel: half the logical cores.
pub(super) fn worker_base() -> usize {
    sysctl_u32("hw.perflevel1.logicalcpu")
        .map(|n| n as usize)
        .filter(|&n| n > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get() / 2))
}

pub fn low_power() -> bool {
    autoreleasepool(|_| {
        let info = NSProcessInfo::processInfo();
        let v12 = NSOperatingSystemVersion {
            majorVersion: 12,
            minorVersion: 0,
            patchVersion: 0,
        };
        info.isOperatingSystemAtLeastVersion(v12) && info.isLowPowerModeEnabled()
    })
}

pub fn is_elevated() -> bool {
    // SAFETY: no arguments, cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// `Some(true)` if the app has Full Disk Access, `Some(false)` if macOS blocked a
/// protected path that exists, `None` if no test path exists.
pub fn full_disk_access(env: &Env) -> Option<bool> {
    let probes = [
        env.home.join(".Trash"),
        env.local.join("Mail"),
        env.local.join("Safari"),
        PathBuf::from("/Library/Preferences/com.apple.TimeMachine.plist"),
    ];
    let mut blocked = false;
    for probe in probes {
        let Ok(md) = std::fs::symlink_metadata(&probe) else {
            continue;
        };
        let result = if md.is_dir() {
            std::fs::read_dir(&probe).map(drop)
        } else {
            File::open(&probe).map(drop)
        };
        match result {
            Ok(()) => return Some(true),
            Err(e) if e.raw_os_error() == Some(libc::EPERM) => blocked = true,
            Err(_) => {}
        }
    }
    blocked.then_some(false)
}

pub fn open_privacy_settings() {
    std::thread::spawn(|| {
        let _ = Command::new("/usr/bin/open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles")
            .status();
    });
}

/// Moves one item to the Trash. Never falls back to deleting it (safety rule S5).
/// Returns where the item went in the Trash, so it can be put back.
pub fn move_to_trash(path: &Path) -> Result<Option<PathBuf>, String> {
    let path = path.to_str().ok_or("the name is not valid UTF-8")?;
    autoreleasepool(|_| {
        let url = NSURL::fileURLWithPath(&NSString::from_str(path));
        let mut moved: Option<Retained<NSURL>> = None;
        NSFileManager::defaultManager()
            .trashItemAtURL_resultingItemURL_error(&url, Some(&mut moved))
            .map_err(|e| e.localizedDescription().to_string())?;
        Ok(moved
            .and_then(|u| u.path())
            .map(|p| PathBuf::from(p.to_string())))
    })
}

/// Puts items back from the Trash: `(original path, path in the Trash)`.
/// Without Full Disk Access macOS refuses a plain rename out of the Trash, so Finder
/// moves the item instead.
pub fn restore_from_trash(
    items: &[(PathBuf, Option<PathBuf>)],
    _since: i64,
) -> Vec<(PathBuf, Result<(), String>)> {
    items
        .iter()
        .map(|(original, trashed)| {
            let result = match trashed {
                None => Err("its place in the Trash is unknown".to_owned()),
                Some(_) if std::fs::symlink_metadata(original).is_ok() => {
                    Err("something else now has its name".to_owned())
                }
                Some(trashed) => put_back(trashed, original),
            };
            (original.clone(), result)
        })
        .collect()
}

fn put_back(trashed: &Path, original: &Path) -> Result<(), String> {
    match std::fs::rename(trashed, original) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() != io::ErrorKind::PermissionDenied => return Err(e.to_string()),
        Err(_) => {}
    }
    let parent = original.parent().ok_or("no parent folder")?;
    let quote = |p: &Path| {
        p.to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    };
    let script = format!(
        "tell application \"Finder\" to move (POSIX file \"{}\" as alias) to (POSIX file \"{}\" as alias)",
        quote(trashed),
        quote(parent)
    );
    let out = Command::new("/usr/bin/osascript")
        .args(["-e", &script])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
    }
    // Finder keeps the name the item had in the Trash; give it back its own name.
    let landed = parent.join(trashed.file_name().ok_or("no name")?);
    if landed != original {
        std::fs::rename(&landed, original).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Shows a Quick Look preview. A newer preview replaces the previous one.
pub fn preview(path: &Path) {
    use std::sync::Mutex;
    static CURRENT: Mutex<Option<std::process::Child>> = Mutex::new(None);
    let mut current = CURRENT.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(mut old) = current.take() {
        let _ = old.kill();
        let _ = old.wait();
    }
    *current = Command::new("/usr/bin/qlmanage")
        .arg("-p")
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok();
}

/// Where settings live: `~/Library/Application Support/<bundle id>`.
pub fn config_dir() -> Option<PathBuf> {
    std::env::home_dir().map(|h| h.join("Library/Application Support").join(rules::APP_ID))
}

/// Empties the Trash through Finder. Used when the app cannot read `~/.Trash`.
pub fn empty_trash_with_finder() -> Result<(), String> {
    let out = Command::new("/usr/bin/osascript")
        .args(["-e", "tell application \"Finder\" to empty trash"])
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
    }
}

/// The Windows Recycle Bin has no macOS twin; the Trash is a folder we walk.
pub fn recycle_bin() -> Option<(u64, u64)> {
    None
}

pub fn empty_recycle_bin() -> Result<(), String> {
    Err("not available on macOS".to_owned())
}

pub fn fatal(message: &str) {
    eprintln!("Cleaner: {message}");
}

/// Keeps App Nap from slowing a scan or clean down while the window is hidden.
pub struct Activity(Option<Retained<ProtocolObject<dyn NSObjectProtocol>>>);

pub fn begin_activity(reason: &str) -> Activity {
    let token = NSProcessInfo::processInfo().beginActivityWithOptions_reason(
        NSActivityOptions::UserInitiatedAllowingIdleSystemSleep,
        &NSString::from_str(reason),
    );
    Activity(Some(token))
}

impl Drop for Activity {
    fn drop(&mut self) {
        if let Some(token) = self.0.take() {
            // SAFETY: the token came from `beginActivityWithOptions_reason`.
            unsafe { NSProcessInfo::processInfo().endActivity(&token) };
        }
    }
}

/// Bytes the file uses on disk.
pub fn alloc_size(md: &Metadata) -> u64 {
    md.blocks() * 512
}

/// macOS already reports on-disk size while listing.
pub fn refine_alloc(_path: &Path, alloc: u64) -> u64 {
    alloc
}

/// Id of the volume a file lives on, to stop walks and deletes at mount points.
pub fn volume_id(md: &Metadata) -> Option<u64> {
    Some(md.dev())
}

/// APFS clone id. Files with the same clone id share their disk blocks.
pub fn clone_id(path: &Path) -> Option<u64> {
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut list = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_RETURNED_ATTRS,
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: libc::ATTR_CMNEXT_CLONEID,
    };
    let mut buf = [0u8; 64];
    // SAFETY: `list` and `buf` are valid for the sizes passed.
    let r = unsafe {
        libc::getattrlist(
            c.as_ptr(),
            (&raw mut list).cast(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            libc::FSOPT_NOFOLLOW | libc::FSOPT_ATTR_CMN_EXTENDED,
        )
    };
    if r != 0 {
        return None;
    }
    // Layout: u32 length, attribute_set_t (5 × u32), then the clone id.
    let returned_fork = u32::from_ne_bytes(buf[20..24].try_into().ok()?);
    if returned_fork & libc::ATTR_CMNEXT_CLONEID == 0 {
        return None;
    }
    let id = u64::from_ne_bytes(buf[24..32].try_into().ok()?);
    (id != 0).then_some(id)
}

/// When the item arrived in its folder ("date added"), in seconds since 1970. The scan's
/// age limits count it, so the deleter must count it too.
pub fn added_time(path: &Path) -> Option<i64> {
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut list = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_RETURNED_ATTRS | libc::ATTR_CMN_ADDEDTIME,
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    let mut buf = [0u8; 64];
    // SAFETY: `list` and `buf` are valid for the sizes passed.
    let r = unsafe {
        libc::getattrlist(
            c.as_ptr(),
            (&raw mut list).cast(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            libc::FSOPT_NOFOLLOW,
        )
    };
    if r != 0 {
        return None;
    }
    // Layout: u32 length, attribute_set_t (5 × u32), then a timespec.
    let returned = u32::from_ne_bytes(buf[4..8].try_into().ok()?);
    if returned & libc::ATTR_CMN_ADDEDTIME == 0 {
        return None;
    }
    Some(i64::from_ne_bytes(buf[24..32].try_into().ok()?))
}

/// The listings already carry each file's inode, so there is nothing more to read.
pub fn file_index(_path: &Path) -> Option<u64> {
    None
}

/// The bulk listing and `lstat` report the same change time, so the deleter needs no
/// second source (the `bulk_listing_matches_lstat` test checks this).
pub fn listed_mtime(_path: &Path) -> Option<i64> {
    None
}

/// True if Finder shows the folder as one file: its extension names a package type, or
/// its bundle bit is set. The scan asks only about names the fixed list does not cover.
pub fn is_package(path: &Path) -> bool {
    let Some(path) = path.to_str() else {
        return false;
    };
    autoreleasepool(|_| {
        let url = NSURL::fileURLWithPath_isDirectory(&NSString::from_str(path), true);
        // SAFETY: a constant NSString key exported by Foundation.
        let key = unsafe { NSURLIsPackageKey };
        url.resourceValuesForKeys_error(&NSArray::from_slice(&[key]))
            .ok()
            .and_then(|values| values.objectForKey(key))
            .and_then(|v| v.downcast::<NSNumber>().ok())
            .is_some_and(|n| n.boolValue())
    })
}

/// Opens a file for hashing without filling the file cache with its content.
pub fn open_for_hash(path: &Path) -> io::Result<File> {
    let file = File::open(path)?;
    // SAFETY: valid descriptor; F_NOCACHE only changes caching for this descriptor.
    unsafe { libc::fcntl(file.as_raw_fd(), libc::F_NOCACHE, 1) };
    Ok(file)
}

/// True when running from `Cleaner.app` (the Dock then uses the bundle's icon).
pub fn running_in_bundle() -> bool {
    std::env::current_exe().is_ok_and(|p| p.to_string_lossy().contains(".app/Contents/MacOS/"))
}

/// Folders inside the home folder that the home pass never enters.
pub const HOME_SKIP: &[&str] = &["Library"];

/// Deletion on macOS needs no rename probe: files in use can still be unlinked.
pub const RENAME_BEFORE_DELETE: bool = false;

/// Makes the parent folder writable, so a file inside it can be removed (Go keeps its
/// module cache read-only).
pub fn make_removable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Some(parent) = path.parent() else {
        return false;
    };
    let Ok(md) = std::fs::symlink_metadata(parent) else {
        return false;
    };
    let mode = md.permissions().mode();
    if mode & 0o200 != 0 {
        return false;
    }
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(mode | 0o700)).is_ok()
}

#[cfg(test)]
mod tests {
    #[test]
    fn worker_base_is_sane() {
        let n = super::worker_base();
        assert!(n >= 1, "got {n}");
    }

    #[test]
    fn user_temp_dir_exists() {
        let t = super::user_temp_dir().expect("temp dir");
        assert!(t.is_dir(), "{t:?}");
    }

    #[test]
    fn asks_the_system_about_packages() {
        let dir = crate::rules::tests::tempdir("package");
        for name in ["Script.scptd", "Notes.rtfd", "Plain.data"] {
            std::fs::create_dir_all(dir.join(name)).unwrap();
        }
        assert!(super::is_package(&dir.join("Script.scptd")));
        assert!(super::is_package(&dir.join("Notes.rtfd")));
        assert!(!super::is_package(&dir.join("Plain.data")));
    }

    #[test]
    fn reads_date_added() {
        let dir = crate::rules::tests::tempdir("added");
        let file = dir.join("f");
        std::fs::write(&file, b"x").unwrap();
        let added = super::added_time(&file).expect("date added");
        assert!((added - crate::fmt::now()).abs() < 60, "{added}");
    }
}
