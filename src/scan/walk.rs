//! Folder listing and the parallel walker.
//!
//! One folder is one job. Workers share a stack of jobs, so the walk goes depth-first
//! and the stack stays small. Threads exist only while a walk runs.

use std::ffi::OsString;
use std::fs::Metadata;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    /// Symlink or junction. Never followed.
    Link,
    /// Sockets, devices, and Windows reparse points that are not links.
    Other,
}

/// One entry of a folder listing.
#[derive(Clone, Debug)]
pub struct Entry {
    pub name: OsString,
    pub kind: Kind,
    /// Bytes used on disk.
    pub alloc: u64,
    /// Length of the content in bytes.
    pub len: u64,
    /// Last change, in seconds since 1970.
    pub mtime: i64,
    /// When the file arrived in this folder (macOS) or was created (Windows). 0 if unknown.
    pub added: i64,
    /// File id: the inode number on macOS, 0 on Windows.
    pub ino: u64,
    pub nlink: u32,
    /// A Windows file that is both hidden and system.
    pub system: bool,
    /// A cloud-only file, or a Windows reparse point that is not a link.
    pub cloud: bool,
    /// A folder that is the mount point of another volume.
    pub mount: bool,
}

impl Entry {
    /// The newest of "changed" and "added" — what age limits compare against.
    pub fn newest(&self) -> i64 {
        self.mtime.max(self.added)
    }
}

/// Lists one folder without following links.
pub fn list(dir: &Path) -> io::Result<Vec<Entry>> {
    #[cfg(target_os = "macos")]
    {
        match super::list_macos::list(dir) {
            Err(e) if matches!(e.raw_os_error(), Some(libc::ENOTSUP | libc::EINVAL)) => {}
            result => return result,
        }
    }
    list_std(dir)
}

/// Portable listing through `read_dir`. On Windows the details come with the listing;
/// on macOS each entry costs one `lstat`.
pub fn list_std(dir: &Path) -> io::Result<Vec<Entry>> {
    #[cfg(unix)]
    let parent_dev = {
        use std::os::unix::fs::MetadataExt;
        std::fs::symlink_metadata(dir)?.dev()
    };
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let Ok(entry) = entry else { continue };
        // `DirEntry::metadata` does not follow links.
        let Ok(md) = entry.metadata() else { continue };
        #[allow(unused_mut)]
        let mut e = from_metadata(entry.file_name(), &md);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            e.mount = e.kind == Kind::Dir && md.dev() != parent_dev;
        }
        out.push(e);
    }
    Ok(out)
}

#[cfg(unix)]
pub fn from_metadata(name: OsString, md: &Metadata) -> Entry {
    use std::os::unix::fs::MetadataExt;
    let ft = md.file_type();
    let kind = if ft.is_symlink() {
        Kind::Link
    } else if ft.is_dir() {
        Kind::Dir
    } else if ft.is_file() {
        Kind::File
    } else {
        Kind::Other
    };
    #[cfg(target_os = "macos")]
    let (added, cloud) = {
        use std::os::macos::fs::MetadataExt as _;
        (
            md.st_birthtime(),
            md.st_flags() & super::list_macos::SF_DATALESS != 0,
        )
    };
    #[cfg(not(target_os = "macos"))]
    let (added, cloud) = (0, false);
    Entry {
        name,
        kind,
        alloc: md.blocks() * 512,
        len: md.len(),
        mtime: md.mtime(),
        added,
        ino: md.ino(),
        nlink: md.nlink() as u32,
        system: false,
        cloud,
        mount: false,
    }
}

#[cfg(windows)]
pub fn from_metadata(name: OsString, md: &Metadata) -> Entry {
    use std::os::windows::fs::{FileTypeExt as _, MetadataExt};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS,
        FILE_ATTRIBUTE_RECALL_ON_OPEN, FILE_ATTRIBUTE_REPARSE_POINT,
    };
    let attrs = md.file_attributes();
    let ft = md.file_type();
    let reparse = attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    let link = ft.is_symlink() || ft.is_symlink_dir() || ft.is_symlink_file();
    let kind = if link {
        Kind::Link
    } else if reparse {
        Kind::Other
    } else if ft.is_dir() {
        Kind::Dir
    } else if ft.is_file() {
        Kind::File
    } else {
        Kind::Other
    };
    let recall = FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
        | FILE_ATTRIBUTE_RECALL_ON_OPEN
        | FILE_ATTRIBUTE_OFFLINE;
    Entry {
        name,
        kind,
        alloc: md.file_size(),
        len: md.file_size(),
        mtime: filetime_secs(md.last_write_time()),
        added: filetime_secs(md.creation_time()),
        ino: 0,
        nlink: 1,
        system: crate::platform::is_system_file(md),
        cloud: (reparse && !link) || attrs & recall != 0,
        mount: false,
    }
}

/// Windows FILETIME (100 ns since 1601) to seconds since 1970.
#[cfg(windows)]
pub(crate) fn filetime_secs(ft: u64) -> i64 {
    (ft / 10_000_000) as i64 - 11_644_473_600
}

/// Walks folders in parallel. `visit` gets each listed folder, its context and its
/// listing, and pushes the subfolders to walk next. Returns early when `cancel` is set.
pub fn walk<C, F>(jobs: Vec<(PathBuf, C)>, threads: usize, cancel: &AtomicBool, visit: F)
where
    C: Send,
    F: Fn(&Path, C, io::Result<Vec<Entry>>, &mut Vec<(PathBuf, C)>) + Sync,
{
    struct State<C> {
        stack: Vec<(PathBuf, C)>,
        active: usize,
    }
    let state = Mutex::new(State {
        stack: jobs,
        active: 0,
    });
    let wake = Condvar::new();
    std::thread::scope(|scope| {
        for _ in 0..threads.max(1) {
            scope.spawn(|| {
                crate::platform::worker_init();
                let mut found = Vec::new();
                loop {
                    let (dir, ctx) = {
                        let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                        loop {
                            if cancel.load(Ordering::Relaxed) {
                                wake.notify_all();
                                return;
                            }
                            if let Some(job) = st.stack.pop() {
                                st.active += 1;
                                break job;
                            }
                            if st.active == 0 {
                                wake.notify_all();
                                return;
                            }
                            // The timeout only matters for Cancel; walks are short-lived.
                            st = wake
                                .wait_timeout(st, Duration::from_millis(100))
                                .unwrap_or_else(|e| e.into_inner())
                                .0;
                        }
                    };
                    let listing = list(&dir);
                    visit(&dir, ctx, listing, &mut found);
                    let pushed = found.len();
                    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                    st.active -= 1;
                    st.stack.append(&mut found);
                    let finished = st.active == 0 && st.stack.is_empty();
                    drop(st);
                    if finished || pushed > 1 {
                        wake.notify_all();
                    } else if pushed == 1 {
                        wake.notify_one();
                    }
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    #[test]
    fn walks_every_folder_once() {
        let dir = crate::rules::tests::tempdir("walk");
        for i in 0..20 {
            let sub = dir.join(format!("a{i}/b/c"));
            std::fs::create_dir_all(&sub).unwrap();
            std::fs::write(sub.join("f"), vec![1u8; 100]).unwrap();
        }
        let files = AtomicU64::new(0);
        let bytes = AtomicU64::new(0);
        let cancel = AtomicBool::new(false);
        walk(
            vec![(dir.clone(), ())],
            4,
            &cancel,
            |d, (), listing, push| {
                for e in listing.unwrap() {
                    match e.kind {
                        Kind::Dir => push.push((d.join(&e.name), ())),
                        Kind::File => {
                            files.fetch_add(1, Ordering::Relaxed);
                            bytes.fetch_add(e.len, Ordering::Relaxed);
                        }
                        _ => {}
                    }
                }
            },
        );
        assert_eq!(files.load(Ordering::Relaxed), 20);
        assert_eq!(bytes.load(Ordering::Relaxed), 2000);
    }

    #[test]
    #[cfg(unix)]
    fn does_not_follow_links() {
        let dir = crate::rules::tests::tempdir("links");
        let outside = dir.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("big"), vec![0u8; 4096]).unwrap();
        let inside = dir.join("inside");
        std::fs::create_dir_all(&inside).unwrap();
        std::os::unix::fs::symlink(&outside, inside.join("link")).unwrap();
        let entries = list(&inside).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, Kind::Link);
    }
}
