//! Windows calls through windows-sys. The Recycle Bin goes through the `trash` crate.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString, c_void};
use std::fs::{File, Metadata, OpenOptions};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::process::CommandExt;
use std::path::{Component, Path, PathBuf, Prefix};
use std::process::Command;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_MORE_DATA, ERROR_SUCCESS, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_SYSTEM, FILE_FLAG_SEQUENTIAL_SCAN,
    GetCompressedFileSizeW, GetDiskFreeSpaceExW, GetDriveTypeW, GetVolumeInformationW,
    INVALID_FILE_SIZE,
};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, REG_SZ, RegCloseKey, RegEnumKeyExW, RegEnumValueW,
    RegOpenKeyExW,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentThread, SetThreadInformation, THREAD_POWER_THROTTLING_CURRENT_VERSION,
    THREAD_POWER_THROTTLING_EXECUTION_SPEED, THREAD_POWER_THROTTLING_STATE, ThreadPowerThrottling,
};
use windows_sys::Win32::UI::Shell::{
    FOLDERID_Downloads, IsUserAnAdmin, KF_FLAG_DEFAULT, SHERB_NOCONFIRMATION, SHERB_NOPROGRESSUI,
    SHERB_NOSOUND, SHEmptyRecycleBinW, SHGetKnownFolderPath, SHQUERYRBINFO, SHQueryRecycleBinW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};

use super::Disk;
use crate::fmt;
use crate::rules::{self, Env};

/// `GetDriveTypeW` result for a fixed disk (from `Win32_System_WindowsProgramming`).
const DRIVE_FIXED: u32 = 3;

fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(Some(0)).collect()
}

fn from_wide(buf: &[u16]) -> OsString {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    OsString::from_wide(&buf[..len])
}

pub fn init_process() {}

pub fn env() -> Option<Env> {
    let home = rules::resolve_dir(&std::env::home_dir()?)?;
    let temp = rules::resolve_dir(&std::env::temp_dir()).unwrap_or_default();
    let local = std::env::var_os("LOCALAPPDATA")
        .and_then(|p| rules::resolve_dir(Path::new(&p)))
        .unwrap_or_else(|| home.join(r"AppData\Local"));
    let downloads = known_downloads()
        .and_then(|p| rules::resolve_dir(&p))
        .unwrap_or_else(|| home.join("Downloads"));
    let mut env = Env {
        home,
        temp,
        downloads,
        local,
        cloud: Vec::new(),
    };
    env.cloud = cloud_roots(&env);
    Some(env)
}

/// The Downloads folder; users can move it, so ask the shell.
fn known_downloads() -> Option<PathBuf> {
    let mut raw = null_mut();
    // SAFETY: valid GUID and out-pointer; the buffer is freed below.
    let hr = unsafe {
        SHGetKnownFolderPath(
            &FOLDERID_Downloads,
            KF_FLAG_DEFAULT as u32,
            null_mut(),
            &mut raw,
        )
    };
    let path = if hr >= 0 && !raw.is_null() {
        // SAFETY: the shell returns a NUL-terminated string.
        let len = unsafe { (0..).take_while(|&i| *raw.add(i) != 0).count() };
        // SAFETY: `len` characters were just checked.
        Some(PathBuf::from(OsString::from_wide(unsafe {
            std::slice::from_raw_parts(raw, len)
        })))
    } else {
        None
    };
    // SAFETY: freeing memory the shell allocated (null is allowed).
    unsafe { CoTaskMemFree(raw as *const c_void) };
    path
}

/// Folders kept in sync with a cloud service. The home pass never enters them.
fn cloud_roots(env: &Env) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = ["OneDrive", "OneDriveConsumer", "OneDriveCommercial"]
        .iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .collect();
    for name in ["iCloudDrive", "Dropbox", "Google Drive", "My Drive", "Box"] {
        roots.push(env.home.join(name));
    }
    roots.extend(registry_sync_roots());
    let mut roots: Vec<PathBuf> = roots.iter().filter_map(|p| rules::resolve_dir(p)).collect();
    roots.sort();
    roots.dedup();
    roots
}

/// Every sync root registered with the Windows Cloud Files API (OneDrive, Dropbox, …).
fn registry_sync_roots() -> Vec<PathBuf> {
    const BASE: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\SyncRootManager";
    let mut out = Vec::new();
    let Some(base) = RegKey::open(BASE) else {
        return out;
    };
    for index in 0.. {
        let mut name = [0u16; 512];
        let mut len = name.len() as u32;
        // SAFETY: buffers are valid for the sizes passed.
        let r = unsafe {
            RegEnumKeyExW(
                base.0,
                index,
                name.as_mut_ptr(),
                &mut len,
                null(),
                null_mut(),
                null_mut(),
                null_mut(),
            )
        };
        if r != ERROR_SUCCESS {
            break;
        }
        let sub = format!(
            r"{BASE}\{}\UserSyncRoots",
            String::from_utf16_lossy(&name[..len as usize])
        );
        let Some(key) = RegKey::open(&sub) else {
            continue;
        };
        for value in 0.. {
            let mut value_name = [0u16; 256];
            let mut name_len = value_name.len() as u32;
            let mut kind = 0u32;
            let mut data = [0u16; 1024];
            let mut data_len = (data.len() * 2) as u32;
            // SAFETY: buffers are valid for the sizes passed.
            let r = unsafe {
                RegEnumValueW(
                    key.0,
                    value,
                    value_name.as_mut_ptr(),
                    &mut name_len,
                    null(),
                    &mut kind,
                    data.as_mut_ptr().cast(),
                    &mut data_len,
                )
            };
            if r == ERROR_MORE_DATA {
                continue;
            }
            if r != ERROR_SUCCESS {
                break;
            }
            if kind == REG_SZ {
                out.push(PathBuf::from(from_wide(
                    &data[..(data_len as usize / 2).min(data.len())],
                )));
            }
        }
    }
    out
}

struct RegKey(HKEY);

impl RegKey {
    fn open(path: &str) -> Option<RegKey> {
        let mut key = null_mut();
        let path = wide(path);
        // SAFETY: valid key name and out-pointer.
        let r = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, path.as_ptr(), 0, KEY_READ, &mut key) };
        (r == ERROR_SUCCESS).then_some(RegKey(key))
    }
}

impl Drop for RegKey {
    fn drop(&mut self) {
        // SAFETY: the key was opened by `RegOpenKeyExW`.
        unsafe { RegCloseKey(self.0) };
    }
}

pub fn system_disk() -> Option<Disk> {
    let drive = std::env::var_os("SystemDrive").unwrap_or_else(|| "C:".into());
    let mut root = drive.clone();
    root.push("\\");
    let root = wide(&root);
    let (mut available, mut total, mut free) = (0u64, 0u64, 0u64);
    // SAFETY: valid root string and out-pointers.
    if unsafe { GetDiskFreeSpaceExW(root.as_ptr(), &mut available, &mut total, &mut free) } == 0 {
        return None;
    }
    let mut label = [0u16; 261];
    // SAFETY: the label buffer is valid; the other outputs are optional.
    let ok = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            label.as_mut_ptr(),
            label.len() as u32,
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
            0,
        )
    } != 0;
    let label = if ok {
        from_wide(&label).to_string_lossy().into_owned()
    } else {
        String::new()
    };
    let drive = drive.to_string_lossy();
    let name = if label.is_empty() {
        format!("Local Disk ({drive})")
    } else {
        format!("{label} ({drive})")
    };
    Some(Disk {
        name,
        total,
        free: available,
    })
}

pub fn reveal(path: &Path) {
    let arg = format!("/select,\"{}\"", fmt::path(path));
    std::thread::spawn(move || {
        let _ = Command::new("explorer.exe").raw_arg(arg).status();
    });
}

/// Lower-case `.exe` names of running processes.
pub fn running_apps() -> HashSet<String> {
    let mut out = HashSet::new();
    // SAFETY: snapshot of all processes; the handle is closed below.
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap == INVALID_HANDLE_VALUE {
        return out;
    }
    // SAFETY: PROCESSENTRY32W is plain data.
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    // SAFETY: valid snapshot handle and entry.
    let mut ok = unsafe { Process32FirstW(snap, &mut entry) } != 0;
    while ok {
        out.insert(
            from_wide(&entry.szExeFile)
                .to_string_lossy()
                .to_ascii_lowercase(),
        );
        // SAFETY: as above.
        ok = unsafe { Process32NextW(snap, &mut entry) } != 0;
    }
    // SAFETY: the handle came from CreateToolhelp32Snapshot.
    unsafe { CloseHandle(snap) };
    out
}

/// EcoQoS: Windows may run the thread on efficiency cores at lower clock speed.
pub fn worker_init() {
    let state = THREAD_POWER_THROTTLING_STATE {
        Version: THREAD_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
        StateMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
    };
    // SAFETY: valid pseudo-handle and a struct of the advertised size.
    unsafe {
        SetThreadInformation(
            GetCurrentThread(),
            ThreadPowerThrottling,
            (&raw const state).cast(),
            std::mem::size_of::<THREAD_POWER_THROTTLING_STATE>() as u32,
        );
    }
}

pub(super) fn worker_base() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get() / 2)
}

/// True while battery saver is on.
pub fn low_power() -> bool {
    // SAFETY: plain data, filled by the call.
    let mut status: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
    // SAFETY: valid out-pointer.
    (unsafe { GetSystemPowerStatus(&mut status) } != 0) && status.SystemStatusFlag == 1
}

pub fn is_elevated() -> bool {
    // SAFETY: no arguments.
    unsafe { IsUserAnAdmin() != 0 }
}

/// Windows has no Full Disk Access setting.
pub fn full_disk_access(_env: &Env) -> Option<bool> {
    None
}

pub fn open_privacy_settings() {}

/// The shell changes names that end in a dot or space, and reserved names such as
/// `CON`, so it could act on a different file. Such items are skipped.
pub fn trash_name_problem(path: &Path) -> Option<&'static str> {
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    for part in path.components() {
        let Component::Normal(name) = part else {
            continue;
        };
        let name = name.to_string_lossy();
        if name.ends_with('.') || name.ends_with(' ') {
            return Some("the name ends with a dot or space");
        }
        let stem = name.split('.').next().unwrap_or("").trim_end();
        if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(stem)) {
            return Some("the name is reserved by Windows");
        }
    }
    None
}

fn drive_root(path: &Path) -> Option<String> {
    match path.components().next()? {
        Component::Prefix(p) => match p.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                Some(format!("{}:\\", letter as char))
            }
            _ => None,
        },
        _ => None,
    }
}

/// Moves one item to the Recycle Bin. If the bin refuses it (too big, no bin on the
/// drive, name problems), the item is skipped. It is never deleted instead (rule S5).
pub fn move_to_trash(path: &Path) -> Result<(), String> {
    if let Some(problem) = trash_name_problem(path) {
        return Err(problem.to_owned());
    }
    let root = drive_root(path).ok_or("this drive has no Recycle Bin")?;
    // SAFETY: valid NUL-terminated string.
    if unsafe { GetDriveTypeW(wide(&root).as_ptr()) } != DRIVE_FIXED {
        return Err("this drive has no Recycle Bin".to_owned());
    }
    trash::delete(path).map_err(|e| e.to_string())
}

pub fn empty_trash_with_finder() -> Result<(), String> {
    Err("not available on Windows".to_owned())
}

/// Size and item count of the Recycle Bin on all drives.
pub fn recycle_bin() -> Option<(u64, u64)> {
    let mut info = SHQUERYRBINFO {
        cbSize: std::mem::size_of::<SHQUERYRBINFO>() as u32,
        i64Size: 0,
        i64NumItems: 0,
    };
    // SAFETY: null root means all drives; `info` has its size set.
    let hr = unsafe { SHQueryRecycleBinW(null(), &mut info) };
    (hr >= 0).then(|| (info.i64Size.max(0) as u64, info.i64NumItems.max(0) as u64))
}

pub fn empty_recycle_bin() -> Result<(), String> {
    // SAFETY: no window, all drives, no UI.
    let hr = unsafe {
        SHEmptyRecycleBinW(
            null_mut(),
            null(),
            SHERB_NOCONFIRMATION | SHERB_NOPROGRESSUI | SHERB_NOSOUND,
        )
    };
    if hr >= 0 || recycle_bin().is_some_and(|(_, items)| items == 0) {
        Ok(())
    } else {
        Err(format!(
            "Windows could not empty the Recycle Bin (0x{:08X})",
            hr as u32
        ))
    }
}

pub fn fatal(message: &str) {
    let text = wide(message);
    let title = wide("Cleaner");
    // SAFETY: valid NUL-terminated strings.
    unsafe {
        MessageBoxW(
            null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        )
    };
}

/// Windows has no App Nap.
pub struct Activity;

pub fn begin_activity(_reason: &str) -> Activity {
    Activity
}

pub fn alloc_size(md: &Metadata) -> u64 {
    md.file_size()
}

/// Bytes on disk of a compressed or sparse file. Called only for large-file candidates.
pub fn refine_alloc(path: &Path, alloc: u64) -> u64 {
    let name = wide(path);
    let mut high = 0u32;
    // SAFETY: valid string and out-pointer.
    let low = unsafe { GetCompressedFileSizeW(name.as_ptr(), &mut high) };
    if low == INVALID_FILE_SIZE && io::Error::last_os_error().raw_os_error().unwrap_or(0) != 0 {
        return alloc;
    }
    (u64::from(high) << 32) | u64::from(low)
}

/// Mount points are reparse points, which walks and deletes never follow.
pub fn volume_id(_md: &Metadata) -> Option<u64> {
    None
}

pub fn clone_id(_path: &Path) -> Option<u64> {
    None
}

pub fn open_for_hash(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_SEQUENTIAL_SCAN)
        .open(path)
}

pub fn running_in_bundle() -> bool {
    false
}

/// Folders inside the home folder that the home pass never enters.
pub const HOME_SKIP: &[&str] = &["AppData"];

/// Hidden system files (`NTUSER.DAT`, `desktop.ini`, …) are never listed.
pub fn is_system_file(md: &Metadata) -> bool {
    let a = md.file_attributes();
    a & FILE_ATTRIBUTE_HIDDEN != 0 && a & FILE_ATTRIBUTE_SYSTEM != 0
}

/// Windows refuses to rename a folder while a file inside it is open. Renaming first
/// therefore tells us whether the folder is in use, before anything is deleted.
pub const RENAME_BEFORE_DELETE: bool = true;

/// Clears the read-only flag so the file can be removed.
pub fn make_removable(path: &Path) -> bool {
    let Ok(md) = std::fs::symlink_metadata(path) else {
        return false;
    };
    let mut perms = md.permissions();
    if !perms.readonly() {
        return false;
    }
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(path, perms).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_names_the_shell_would_change() {
        assert!(trash_name_problem(Path::new(r"C:\a\b.txt")).is_none());
        assert!(trash_name_problem(Path::new(r"C:\a\b.")).is_some());
        assert!(trash_name_problem(Path::new(r"C:\a\con.txt")).is_some());
        assert!(trash_name_problem(Path::new(r"C:\a\console.txt")).is_none());
    }
}
