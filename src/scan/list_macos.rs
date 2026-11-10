//! Folder listing with `getattrlistbulk`: one system call returns the details of many
//! entries, instead of one `lstat` call per entry.
//!
//! Record layout (see `man getattrlistbulk`): a `u32` length, the returned
//! `attribute_set_t`, then `ATTR_CMN_ERROR` if present, then the other attributes in bit
//! order: common, directory, file. Each attribute is aligned to 4 bytes.

use std::cell::RefCell;
use std::ffi::{CString, OsString};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

use super::walk::{Entry, Kind};

/// `st_flags` bit of a cloud-only ("dataless") file.
pub const SF_DATALESS: u32 = 0x4000_0000;
// Not in the `libc` crate; values from <sys/attr.h> and <sys/vnode.h>.
const ATTR_CMN_ERROR: u32 = 0x2000_0000;
const VREG: u32 = 1;
const VDIR: u32 = 2;
const VLNK: u32 = 5;
const BUF_SIZE: usize = 256 * 1024;

thread_local! {
    /// One buffer per worker thread, reused for every folder.
    static BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

struct Fd(libc::c_int);

impl Drop for Fd {
    fn drop(&mut self) {
        // SAFETY: the descriptor was opened by `list` and is closed once.
        unsafe { libc::close(self.0) };
    }
}

pub fn list(dir: &Path) -> io::Result<Vec<Entry>> {
    let path = CString::new(dir.as_os_str().as_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: valid C string. O_NOFOLLOW: never list through a link.
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = Fd(fd);
    let mut attrs = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_RETURNED_ATTRS
            | libc::ATTR_CMN_NAME
            | ATTR_CMN_ERROR
            | libc::ATTR_CMN_OBJTYPE
            | libc::ATTR_CMN_MODTIME
            | libc::ATTR_CMN_FLAGS
            | libc::ATTR_CMN_FILEID
            | libc::ATTR_CMN_ADDEDTIME,
        volattr: 0,
        dirattr: libc::ATTR_DIR_MOUNTSTATUS,
        fileattr: libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_TOTALSIZE | libc::ATTR_FILE_ALLOCSIZE,
        forkattr: 0,
    };
    BUF.with_borrow_mut(|buf| {
        if buf.len() < BUF_SIZE {
            buf.resize(BUF_SIZE, 0);
        }
        let mut out = Vec::new();
        loop {
            // SAFETY: `attrs` and `buf` are valid for the sizes passed.
            let n = unsafe {
                libc::getattrlistbulk(
                    fd.0,
                    (&raw mut attrs).cast(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    0,
                )
            };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            if n == 0 {
                return Ok(out);
            }
            let mut at = 0usize;
            for _ in 0..n {
                let len = buf
                    .get(at..at + 4)
                    .map_or(0, |b| u32::from_ne_bytes([b[0], b[1], b[2], b[3]]))
                    as usize;
                if len < 24 || at + len > buf.len() {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                if let Some(entry) = parse(&buf[at..at + len]) {
                    out.push(entry);
                }
                at += len;
            }
        }
    })
}

struct Reader<'a> {
    rec: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let bytes = self.rec.get(self.at..self.at + N)?;
        self.at += N;
        bytes.try_into().ok()
    }
    fn u32(&mut self) -> Option<u32> {
        self.take::<4>().map(u32::from_ne_bytes)
    }
    fn i32(&mut self) -> Option<i32> {
        self.take::<4>().map(i32::from_ne_bytes)
    }
    fn u64(&mut self) -> Option<u64> {
        self.take::<8>().map(u64::from_ne_bytes)
    }
    /// A `timespec`: seconds, then nanoseconds. Only the seconds are kept.
    fn secs(&mut self) -> Option<i64> {
        let secs = i64::from_ne_bytes(self.take::<8>()?);
        self.take::<8>()?;
        Some(secs)
    }
}

fn parse(rec: &[u8]) -> Option<Entry> {
    let mut r = Reader { rec, at: 4 };
    let common = r.u32()?;
    let _vol = r.u32()?;
    let dir = r.u32()?;
    let file = r.u32()?;
    let _fork = r.u32()?;
    if common & ATTR_CMN_ERROR != 0 && r.u32()? != 0 {
        return None;
    }
    if common & libc::ATTR_CMN_NAME == 0 {
        return None;
    }
    let name_at = r.at;
    let name_off = r.i32()?;
    let name_len = r.u32()? as usize;
    let start = usize::try_from(name_at as i64 + i64::from(name_off)).ok()?;
    let raw = rec.get(start..start + name_len)?;
    let name = OsString::from_vec(raw.strip_suffix(&[0]).unwrap_or(raw).to_vec());

    let read = |bit: u32, set: u32| set & bit != 0;
    let objtype = if read(libc::ATTR_CMN_OBJTYPE, common) {
        r.u32()?
    } else {
        0
    };
    let mtime = if read(libc::ATTR_CMN_MODTIME, common) {
        r.secs()?
    } else {
        0
    };
    let flags = if read(libc::ATTR_CMN_FLAGS, common) {
        r.u32()?
    } else {
        0
    };
    let ino = if read(libc::ATTR_CMN_FILEID, common) {
        r.u64()?
    } else {
        0
    };
    let added = if read(libc::ATTR_CMN_ADDEDTIME, common) {
        r.secs()?
    } else {
        0
    };
    let mount =
        read(libc::ATTR_DIR_MOUNTSTATUS, dir) && r.u32()? & libc::DIR_MNTSTATUS_MNTPOINT != 0;
    let nlink = if read(libc::ATTR_FILE_LINKCOUNT, file) {
        r.u32()?
    } else {
        1
    };
    let len = if read(libc::ATTR_FILE_TOTALSIZE, file) {
        r.u64()?
    } else {
        0
    };
    let alloc = if read(libc::ATTR_FILE_ALLOCSIZE, file) {
        r.u64()?
    } else {
        0
    };
    let kind = match objtype {
        VREG => Kind::File,
        VDIR => Kind::Dir,
        VLNK => Kind::Link,
        _ => Kind::Other,
    };
    Some(Entry {
        name,
        kind,
        alloc,
        len,
        mtime,
        added,
        ino,
        nlink,
        system: false,
        cloud: flags & SF_DATALESS != 0,
        mount,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::walk::list_std;

    #[test]
    fn bulk_listing_matches_lstat() {
        let dir = crate::rules::tests::tempdir("bulk");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("small"), b"hello").unwrap();
        std::fs::write(dir.join("big"), vec![7u8; 300_000]).unwrap();
        std::fs::hard_link(dir.join("big"), dir.join("big-link")).unwrap();
        std::os::unix::fs::symlink(dir.join("sub"), dir.join("link")).unwrap();
        std::fs::write(dir.join("ünïcödé 名前.txt"), b"x").unwrap();

        let mut bulk = list(&dir).unwrap();
        let mut std = list_std(&dir).unwrap();
        bulk.sort_by(|a, b| a.name.cmp(&b.name));
        std.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(bulk.len(), 6);
        assert_eq!(bulk.len(), std.len());
        for (a, b) in bulk.iter().zip(&std) {
            assert_eq!(a.name, b.name);
            assert_eq!(a.kind, b.kind, "{:?}", a.name);
            assert_eq!(a.mtime, b.mtime, "{:?}", a.name);
            assert_eq!(a.ino, b.ino, "{:?}", a.name);
            assert!(!a.mount);
            if a.kind == Kind::File {
                assert_eq!(a.len, b.len, "{:?}", a.name);
                assert_eq!(a.alloc, b.alloc, "{:?}", a.name);
                assert_eq!(a.nlink, b.nlink, "{:?}", a.name);
            }
        }
    }
}
