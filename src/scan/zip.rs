//! Tells whether a `.zip` file in Downloads is an app installer. It reads only the
//! end of the file: the end-of-archive record and the central directory, which lists
//! the names of the files inside. File contents are never read.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const EOCD_SIG: [u8; 4] = [0x50, 0x4B, 0x05, 0x06];
const EOCD64_LOCATOR_SIG: [u8; 4] = [0x50, 0x4B, 0x06, 0x07];
const EOCD64_SIG: [u8; 4] = [0x50, 0x4B, 0x06, 0x06];
const ENTRY_SIG: [u8; 4] = [0x50, 0x4B, 0x01, 0x02];
/// The end-of-archive record plus the longest possible archive comment.
const TAIL: u64 = 22 + 65_535;
/// Larger central directories are not read: such zips are archives, not installers.
const MAX_DIRECTORY: u64 = 8 * 1024 * 1024;
const MAX_ENTRIES: usize = 5_000;

/// True if the zip holds an app bundle (`*.app/`), or a `.pkg`, `.msi` or setup `.exe`.
pub fn is_installer(path: &Path) -> bool {
    directory_names(path).is_some_and(|names| names.iter().any(|n| installer_name(n)))
}

fn installer_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let first = lower.split('/').next().unwrap_or("");
    let last = lower.rsplit('/').next().unwrap_or("");
    first.ends_with(".app")
        || [".pkg", ".mpkg", ".msi", ".msix", ".dmg"]
            .iter()
            .any(|ext| last.ends_with(ext))
        || (last.ends_with(".exe") && (last.contains("setup") || last.contains("install")))
}

fn u16_at(b: &[u8], at: usize) -> Option<u64> {
    b.get(at..at + 2)
        .map(|s| u64::from(u16::from_le_bytes([s[0], s[1]])))
}

fn u32_at(b: &[u8], at: usize) -> Option<u64> {
    b.get(at..at + 4)
        .map(|s| u64::from(u32::from_le_bytes([s[0], s[1], s[2], s[3]])))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    b.get(at..at + 8)
        .map(|s| u64::from_le_bytes(s.try_into().unwrap_or([0; 8])))
}

/// The file names listed in the zip's central directory.
fn directory_names(path: &Path) -> Option<Vec<String>> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let tail_len = len.min(TAIL);
    let mut tail = vec![0u8; tail_len as usize];
    file.seek(SeekFrom::Start(len - tail_len)).ok()?;
    file.read_exact(&mut tail).ok()?;
    let eocd = tail.windows(4).rposition(|w| w == EOCD_SIG)?;
    let mut size = u32_at(&tail, eocd + 12)?;
    let mut offset = u32_at(&tail, eocd + 16)?;
    if offset == u64::from(u32::MAX) || size == u64::from(u32::MAX) {
        // ZIP64: a locator just before the record points at the 64-bit record.
        let loc = eocd.checked_sub(20)?;
        if tail.get(loc..loc + 4)? != EOCD64_LOCATOR_SIG {
            return None;
        }
        let record_at = u64_at(&tail, loc + 8)?;
        let mut record = [0u8; 56];
        file.seek(SeekFrom::Start(record_at)).ok()?;
        file.read_exact(&mut record).ok()?;
        if record[..4] != EOCD64_SIG {
            return None;
        }
        size = u64_at(&record, 40)?;
        offset = u64_at(&record, 48)?;
    }
    if size == 0 || size > MAX_DIRECTORY || offset.checked_add(size)? > len {
        return None;
    }
    let mut dir = vec![0u8; size as usize];
    file.seek(SeekFrom::Start(offset)).ok()?;
    file.read_exact(&mut dir).ok()?;
    let mut names = Vec::new();
    let mut at = 0usize;
    while at + 46 <= dir.len() && names.len() < MAX_ENTRIES {
        if dir[at..at + 4] != ENTRY_SIG {
            break;
        }
        let name_len = u16_at(&dir, at + 28)? as usize;
        let extra_len = u16_at(&dir, at + 30)? as usize;
        let comment_len = u16_at(&dir, at + 32)? as usize;
        let name = dir.get(at + 46..at + 46 + name_len)?;
        names.push(String::from_utf8_lossy(name).into_owned());
        at += 46 + name_len + extra_len + comment_len;
    }
    Some(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal zip with empty "stored" entries.
    fn zip(names: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for name in names {
            let offset = out.len() as u32;
            out.extend([0x50, 0x4B, 0x03, 0x04, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            out.extend([0u8; 12]); // crc, sizes
            out.extend((name.len() as u16).to_le_bytes());
            out.extend(0u16.to_le_bytes());
            out.extend(name.as_bytes());
            central.extend(ENTRY_SIG);
            central.extend([20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            central.extend([0u8; 12]);
            central.extend((name.len() as u16).to_le_bytes());
            central.extend([0u8; 12]); // extra, comment, disk, attributes
            central.extend(offset.to_le_bytes());
            central.extend(name.as_bytes());
        }
        let cd_offset = out.len() as u32;
        out.extend(&central);
        out.extend(EOCD_SIG);
        out.extend([0u8; 4]);
        out.extend((names.len() as u16).to_le_bytes());
        out.extend((names.len() as u16).to_le_bytes());
        out.extend((central.len() as u32).to_le_bytes());
        out.extend(cd_offset.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out
    }

    #[test]
    fn spots_app_and_installer_zips() {
        let dir = crate::rules::tests::tempdir("zip");
        let app = dir.join("Visual Studio Code.zip");
        std::fs::write(
            &app,
            zip(&[
                "Visual Studio Code.app/",
                "Visual Studio Code.app/Contents/Info.plist",
            ]),
        )
        .unwrap();
        let setup = dir.join("tool.zip");
        std::fs::write(&setup, zip(&["tool/Setup-1.2.exe"])).unwrap();
        let photos = dir.join("photos.zip");
        std::fs::write(&photos, zip(&["trip/1.jpg", "trip/2.jpg"])).unwrap();
        std::fs::write(dir.join("broken.zip"), b"not a zip").unwrap();

        assert!(is_installer(&app));
        assert!(is_installer(&setup));
        assert!(!is_installer(&photos));
        assert!(!is_installer(&dir.join("broken.zip")));
    }
}
