# Cleaner

- Dev: `cargo run`
- Build, one file in `dist/`: `cargo xtask dist` — `Cleaner.dmg` on macOS, `Cleaner.exe` on Windows
- Build the Windows `.exe` on macOS: `cargo xtask dist --windows` (needs cargo-xwin and lld-link)
