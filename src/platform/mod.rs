//! Calls into macOS and Windows. The rest of the app is OS-neutral.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

/// Space on the disk that holds the system and the user's files.
#[derive(Clone, Debug)]
pub struct Disk {
    pub name: String,
    pub total: u64,
    pub free: u64,
}

/// How many walker threads to start: a few, fewer still in Low Power Mode or battery saver.
pub fn worker_count(low_power: bool) -> usize {
    let n = worker_base().clamp(2, 6);
    if low_power { (n / 2).max(1) } else { n }
}
