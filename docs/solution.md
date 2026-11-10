# Cleaner — solution design

Cleaner is one native Rust program with an egui user interface. A UI thread draws the window, and short-lived worker threads do all disk work. This document explains how the app meets [requirements.md](requirements.md).

## 1. Key decisions

| Decision | Choice | Why |
|---|---|---|
| UI | eframe/egui 0.37, default features off. Features: `glow`, `default_fonts`, `system_fonts`, `accesskit`. | One native binary that redraws only on input or progress. `system_fonts` loads CJK and emoji glyphs only when a file name needs them. |
| Renderer | `glow` (OpenGL) | Smaller and faster to start than wgpu, eframe's default. |
| macOS APIs | `libc`; `objc2-foundation` and `objc2-app-kit` 0.3.2 | eframe already links these exact versions. |
| Windows APIs | `windows-sys` 0.61.2, plus `trash` 5.3 for the Recycle Bin | `trash` uses `IFileOperation`. It skips a file the Recycle Bin refuses and says why. `SHFileOperationW` can delete such a file without asking. |
| macOS folder listing | `getattrlistbulk`, with std `read_dir` as fallback | One system call returns size, flags and dates for many entries. |
| Windows folder listing | std `read_dir` | It already returns size and attributes without opening files. |
| Duplicate hash | xxh3-128 from `xxhash-rust` | Very fast and tiny. |
| Build tool | `cargo xtask`, a Rust program in the repo | The same command works on both systems, with no shell scripts and no Node. |
| Windows icon | `build.rs` writes a `.res` file and the linker embeds it | `link.exe` and `lld-link` both accept `.res` input, so no `rc.exe` or `llvm-rc` is needed. |
| Windows C runtime | Static (`+crt-static`) | The `.exe` runs without the VC++ redistributable. |
| Release profile | `opt-level=3`, `lto="fat"`, `codegen-units=1`, `panic="abort"`, `strip=true` | Small and fast. |
| Threads | std scoped threads, no rayon | Fewer dependencies. Threads end with the job. |

**Known limit:** Windows virtual machines without GPU drivers have no OpenGL 2. There the app shows a message box instead of a window.

**Note for maintainers:** in eframe 0.37, `eframe::App` requires `ui(&mut self, ui: &mut egui::Ui, frame: &mut Frame)`. Older examples that use `update()` do not compile.

## 2. Architecture

```
 UI thread (egui) ──Scan / Clean / Cancel──▶ Engine (worker threads)
        ▲                                          │
        └──── reads progress atomics + results ◀───┘
              (repaint every 250 ms only while busy)
```

| Path | Role |
|---|---|
| `src/main.rs` | Window options, icon, process I/O policy, fatal-error message box |
| `src/app.rs` | App state machine and views: header, sidebar, table, dialogs |
| `src/theme.rs` | Dark colours, spacing, corner radius |
| `src/rules.rs` | Category, target and deny-list tables for each OS |
| `src/scan/walk.rs` | Parallel directory walker |
| `src/scan/list_macos.rs` | `getattrlistbulk` folder listing |
| `src/scan/mod.rs` | A scan run: junk targets plus one pass over the home folder |
| `src/scan/dupes.rs` | Duplicate finder |
| `src/clean.rs` | Safety re-checks; delete, move to Trash, empty Trash |
| `src/platform/` | OS calls, see section 5 |
| `src/fmt.rs` | Size and date text |
| `build.rs` | Windows `.res`: icon, manifest, version info |
| `xtask/src/main.rs` | `dist` and `icons` commands |
| `assets/` | `brush.svg`, `icon.png`, `icon.icns`, `icon.ico`, `app.manifest` |

## 3. Scan engine

- **Targets.** Each rule becomes a target with these parts:
  - a root path;
  - a mode: each child of the root is an item, or the whole root is one item;
  - an action;
  - a minimum age;
  - the id of the app that owns it.
- **No double counting.** A path that is the root of one target is skipped by broader targets. The selection is normalised before cleaning: a path inside another selected path is dropped.
- **Walker.** One folder is one job on a shared stack. The pool is halved in Low Power Mode or battery saver.
  - macOS: as many workers as efficiency cores, from 2 to 6.
  - Windows: half the logical cores, from 2 to 6.
- **Listing.**
  - macOS: `getattrlistbulk` reads name, type, flags, file id, link count, allocated size, dates and mount status in bulk.
  - Windows: reads size and attributes from `read_dir` and skips every reparse point.
- **Size.** The app counts on-disk (allocated) size. macOS counts a hard link once. Windows calls `GetCompressedFileSizeW` only for large-file candidates.
- **Energy.** Threads end when their job ends, so nothing runs while the app is idle.
  - macOS: workers run at QoS class *utility*. An `NSProcessInfo` activity holds off App Nap only while a job runs.
  - Windows: workers use EcoQoS.
- **macOS I/O policy.** At start-up the app calls `setiopolicy_np` twice: one call stops access-time updates; the other makes a read of a cloud-only file fail instead of downloading it.
- **Privacy prompts.** Before the parallel walk, one thread opens Desktop, Documents and Downloads in turn. macOS then asks once per folder.
- **One pass over the home folder** feeds four categories:
  - Large files: kept if 100 MB or more, top 5,000.
  - Old Downloads.
  - Project build folders.
  - Duplicate candidates: 1 MB or more, grouped by size.

  The pass skips `Library`, `AppData`, sync folders, the insides of bundles, `.git`, the deny list and roots that other targets own.
- **Duplicates on request.** Each stage keeps only files that still match:
  1. Group by size.
  2. Drop entries that are the same file.
  3. Drop APFS clones, which share their disk blocks.
  4. Hash the first and last 64 KB.
  5. Hash the rest in growing chunks (1, 4, 16, then 64 MB), regrouping after each chunk.

  Two reader threads bypass the file cache (`F_NOCACHE`, `FILE_FLAG_SEQUENTIAL_SCAN`). Files changed in the last minute are skipped.
- **Progress.** Workers update atomic counters and call `request_repaint()` once when done. While busy, the UI repaints every 250 ms.

## 4. Clean engine

1. **Prepare.** Normalise the selection, then check rule S3 for each item. Skip any item that changed since the scan.
2. **Delete** caches, logs, temp files and build folders. The app removes the whole tree, keeps going past failures, and never follows links.
   - On a permission error it makes the parent writable or clears the read-only flag, then retries once.
   - On Windows it first renames a folder in place. If the rename fails, a file inside is open, so the folder is skipped.
   - Windows paths use the `\\?\` form, so Win32 rules cannot change file names.
3. **Move to Trash** user files, one item at a time.
   - macOS: `NSFileManager.trashItemAtURL`.
   - Windows: `trash::delete`. It skips names that end in a dot or space, and reserved names such as `CON`, because the shell would change them.
   - A refusal is reported, never retried as a delete.
4. **Empty Trash.**
   - macOS: deletes the contents of `~/.Trash` if the app has Full Disk Access. Otherwise it asks Finder through `osascript`.
   - Windows: calls `SHEmptyRecycleBinW`.
5. **Report.** Show bytes removed, the measured change in free space and skipped items. Refresh the disk bar and drop cleaned rows.

## 5. Platform layer

| Need | macOS | Windows |
|---|---|---|
| Disk space | `statfs` | `GetDiskFreeSpaceExW` |
| Move to Trash | `NSFileManager.trashItemAtURL` | `trash` crate: `IFileOperation` with `FOF_NO_UI`, `FOF_ALLOWUNDO`, `FOF_WANTNUKEWARNING` |
| Trash size / empty | read `~/.Trash` / delete it, or ask Finder | `SHQueryRecycleBinW` / `SHEmptyRecycleBinW` |
| Reveal | `open -R <path>` | `explorer /select,<path>` |
| Running apps | `NSWorkspace.runningApplications` | Toolhelp process snapshot |
| Folders | `~/…`, `$TMPDIR` | env vars; `SHGetKnownFolderPath(Downloads)`; OneDrive env vars; `SyncRootManager` registry key |
| Worker energy | `pthread_set_qos_class_self_np(UTILITY)`; `NSProcessInfo` activity | `SetThreadInformation(ThreadPowerThrottling)` |
| Low power | `NSProcessInfo.isLowPowerModeEnabled` | `GetSystemPowerStatus` |
| Full Disk Access | try to open `~/Library/Safari`, `~/Library/Mail` or `~/.Trash`; an EPERM error on a path that exists means "no" | none |
| Elevated? | `geteuid() == 0` | `IsUserAnAdmin()` |

Crate features:

- `windows-sys` 0.61.2: `Win32_Foundation`, `Win32_UI_Shell`, `Win32_Storage_FileSystem`, `Win32_System_Threading`, `Win32_System_Diagnostics_ToolHelp`, `Win32_UI_WindowsAndMessaging`, `Win32_System_Com`, `Win32_System_Power`, `Win32_System_Registry`.
- `objc2-foundation` 0.3.2: `NSFileManager`, `NSURL`, `NSError`, `NSString`, `NSArray`, `NSProcessInfo`.
- `objc2-app-kit` 0.3.2: `NSWorkspace`, `NSRunningApplication`.
- `trash` 5.3: default features off, `coinit_apartmentthreaded` on.
- `libc` 0.2: `getattrlistbulk`, the `ATTR_*` constants, `F_NOCACHE`, `pthread_set_qos_class_self_np`. It lacks `setiopolicy_np`, so the app declares that call itself.

## 6. User interface

```
┌─────────────────────────────────────────────────────────────────────┐
│ Cleaner     Macintosh HD  ██████████░░░░  182 GB free of 494 GB  [Scan] │
├───────────────────────┬─────────────────────────────────────────────┤
│ [x] System junk 4.2 GB │ App caches                    [x] Select all │
│ [x] Browsers    1.1 GB │ [x] com.spotify.client  1.2 GB  2 days  Reveal│
│ [ ] Developer   6.8 GB │ [ ] com.apple.Safari  Needs Full Disk Access │
│ [ ] Build folders 9 GB │ ...  virtual list: only visible rows drawn   │
│ [ ] Trash       2.0 GB │                                              │
│ [ ] Large files  12 GB │                                              │
│ [ ] Old Downloads 3 GB │                                              │
│ [ ] Duplicates  [Find] │                                              │
├───────────────────────┴─────────────────────────────────────────────┤
│ Selected 12.1 GB · 1,204 items                       [Clean 12.1 GB] │
└─────────────────────────────────────────────────────────────────────┘
```

| Colour token | Hex |
|---|---|
| Background | `#0E1014` |
| Panel | `#151820` |
| Raised | `#1B1F29` |
| Border | `#262B36` |
| Text | `#E7E9EE` |
| Muted | `#8A93A6` |
| Accent | `#3DD6C4` |
| Accent 2 | `#8B5CF6` |
| Danger | `#F2716B` |
| Warning | `#F5B84D` |

Rules that keep the UI cheap:

- No spinner and no animated progress bar, because both redraw every frame. Progress is plain text and a static bar.
- The text-cursor blink is off.
- Row text, such as size and date, is formatted once when results arrive, not on every frame.
- Only the visible rows of a list are drawn.

## 7. Icon

- `assets/brush.svg` is drawn on a 1024 px canvas.
  - A dark rounded square holds a diagonal brush.
  - The brush has a light handle, a metal band, teal-to-violet bristles and two small sparkles.
  - The square follows the macOS icon grid: 824 px, centred.
- `cargo xtask icons` renders it with `resvg` into `icon.png` (256 px), `icon.icns` and `icon.ico` (16–256 px).
  - The outputs are committed, so normal builds need no image tools.
  - The `resvg`, `ico` and `icns` crates sit behind an xtask feature, so `dist` stays quick to compile.

## 8. Build and packaging

| Command | Host | Result |
|---|---|---|
| `cargo run` | any | Dev run |
| `cargo xtask dist` | macOS | `dist/Cleaner.dmg` |
| `cargo xtask dist` | Windows | `dist/Cleaner.exe` |
| `cargo xtask dist --windows` | macOS | `dist/Cleaner.exe`, cross-built with cargo-xwin |

- `dist` empties `dist/`, then runs `cargo build --release`.
- On macOS it then makes `Cleaner.app`: Info.plist, `icon.icns`, and a code signature. Next, `hdiutil create -format ULMO` packs the app and an Applications link into one compressed `.dmg`.
- Info.plist sets the bundle id `io.github.hungtd1709.cleaner` and macOS 11.0 as the minimum. It also sets usage texts for Desktop, Documents, Downloads and Apple Events.
- **Signing.** Signing is ad-hoc by default. macOS ties privacy permissions to the signature, so an ad-hoc build loses them on every rebuild. Set `CODESIGN_IDENTITY` to a self-signed certificate to keep them.
- **Windows.** `windows_subsystem = "windows"` hides the console in release builds. The manifest sets `asInvoker`, PerMonitorV2 DPI and `longPathAware`.
- **Dev only.** `cargo run --features autoscan` starts a scan at launch. `CLEANER_VIEW="Large files"` opens that category. `dist` never enables it.

## 9. Measured results

Measured on the reference Mac: Apple silicon, macOS 27.0.1, 1.57 million files in the home folder.

| Check | Target | Result |
|---|---|---|
| macOS binary | 10 MB or less | 4.7 MB |
| `Cleaner.dmg` | 6 MB or less | 2.1 MB |
| `Cleaner.exe` | 10 MB or less | 6.3 MB; imports Windows system DLLs only |
| Full scan | 500,000 files in under 20 s | 1.57 million files in 9 s |
| Sizes | Same as `du` | Same, for example 23.31 GB from `du` and 23.3 GB from Cleaner |
| Start-up | Under 0.5 s | 180–290 ms to the window on screen |
| Idle | 0 % CPU, no timers | 0.0 % CPU, 0 idle wake-ups, 0.0 energy impact |
| Memory | Under 150 MB during a scan | 142 MB peak. The scan engine alone peaks at 39 MB; about 75 MB is macOS OpenGL memory. |

## Terms and short names

| Term | Meaning |
|---|---|
| egui / eframe | Rust UI library / its desktop app wrapper |
| glow | The OpenGL drawing backend of eframe |
| TCC | Transparency, Consent and Control: macOS privacy protection for some folders |
| Full Disk Access | A macOS setting that lifts TCC limits for one app |
| App Nap | macOS slowing down apps whose windows are hidden |
| QoS / EcoQoS | macOS / Windows hint that a thread may run slower to save energy |
| `getattrlistbulk` | macOS call that lists a folder together with each entry's details |
| `IFileOperation` | Windows shell interface that moves files to the Recycle Bin |
| Cloud-only file | A file whose name shows on disk but whose content is still in the cloud |
| xtask | A small Rust program in the repo that runs build steps |
| `.res` | Compiled Windows resource file: icon, manifest, version info |
| xxh3-128 | A fast 128-bit hash used to compare file content |
| DMG | macOS disk image file |
