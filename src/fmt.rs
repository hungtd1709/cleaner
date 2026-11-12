//! Text for sizes, counts, ages and paths.
//!
//! The app formats each value once, when results arrive, and never on every frame.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Finder counts 1 KB as 1000 bytes; File Explorer counts it as 1024 bytes.
/// Using the same step as the OS keeps our numbers equal to the file manager's.
pub const STEP: u64 = if cfg!(windows) { 1024 } else { 1000 };

pub const MB: u64 = STEP * STEP;
pub const GB: u64 = MB * STEP;

/// "1.2 GB", "340 MB", "12 bytes".
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    let step = STEP as f64;
    if n < STEP {
        return if n == 1 {
            "1 byte".to_owned()
        } else {
            format!("{n} bytes")
        };
    }
    let mut value = n as f64 / step;
    let mut unit = 0;
    // Step up once rounding would show a full step, so 999.96 KB reads "1.0 MB".
    while value.round() >= step && unit < UNITS.len() - 1 {
        value /= step;
        unit += 1;
    }
    if value >= 99.95 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// "1,204".
pub fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// "1 item", "3 items".
pub fn items(n: u64) -> String {
    if n == 1 {
        "1 item".to_owned()
    } else {
        format!("{} items", count(n))
    }
}

/// Seconds since 1970.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// "Today", "3 days ago", "5 months ago". Empty when the time is unknown.
pub fn ago(when: i64, now: i64) -> String {
    if when <= 0 {
        return String::new();
    }
    let days = (now - when).max(0) / 86_400;
    match days {
        0 => "Today".to_owned(),
        1 => "Yesterday".to_owned(),
        2..=59 => format!("{days} days ago"),
        60..=729 => format!("{} months ago", days / 30),
        _ => format!("{} years ago", days / 365),
    }
}

/// A path as the user knows it: without the Windows `\\?\` prefix.
pub fn path(p: &Path) -> String {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    s.strip_prefix(r"\\?\").unwrap_or(&s).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_bytes() {
        assert_eq!(bytes(0), "0 bytes");
        assert_eq!(bytes(1), "1 byte");
        assert_eq!(bytes(STEP - 1), format!("{} bytes", STEP - 1));
        assert_eq!(bytes(STEP), "1.0 KB");
        assert_eq!(bytes(3 * GB / 2), "1.5 GB");
        assert_eq!(bytes(250 * MB), "250 MB");
        assert_eq!(bytes(MB - 1), "1.0 MB", "not 1000 KB");
        assert_eq!(bytes(GB - 1), "1.0 GB");
    }

    #[test]
    fn formats_counts_and_ages() {
        assert_eq!(count(0), "0");
        assert_eq!(count(1204), "1,204");
        assert_eq!(count(1_000_000), "1,000,000");
        assert_eq!(items(1), "1 item");
        let now = 1_000_000_000;
        assert_eq!(ago(now - 3_600, now), "Today");
        assert_eq!(ago(now - 3 * 86_400, now), "3 days ago");
        assert_eq!(ago(now - 400 * 86_400, now), "13 months ago");
        assert_eq!(ago(0, now), "");
    }

    #[test]
    fn strips_verbatim_prefix() {
        assert_eq!(path(Path::new(r"\\?\C:\Users\a")), r"C:\Users\a");
        assert_eq!(path(Path::new(r"\\?\UNC\srv\share\x")), r"\\srv\share\x");
        assert_eq!(path(Path::new("/Users/a")), "/Users/a");
    }
}
