# Cleaner — requirements

Cleaner frees disk space on macOS and Windows without losing user data. It finds junk and forgotten files and shows their sizes. It removes only what the user confirms.

The design that meets these requirements is in [solution.md](solution.md).

## 1. What the app cleans

**Default rule:** an item starts selected only if apps can re-create it at no real cost and the app that owns it is not running.

| Category | macOS | Windows | Action | Default |
|---|---|---|---|---|
| System junk | Third-party `~/Library/Caches/*`; logs and crash reports older than 7 days; `$TMPDIR` items older than 7 days | `%TEMP%` items older than 7 days; `CrashDumps` and `WER` older than 7 days; `INetCache` | Delete | Selected |
| Optional system caches | `com.apple.*` caches that are not on the deny list | DirectX shader cache | Delete | Unselected |
| Browser caches | Cache folders of Chrome, Edge, Brave, Firefox, Safari | Cache, Code Cache, GPUCache and `cache2` of Chrome, Edge, Brave, Firefox; all profiles | Delete | Selected |
| Developer caches | Xcode DerivedData, iOS DeviceSupport, simulator caches; npm, Yarn, pip, Homebrew, CocoaPods, Cargo, Go, Gradle; `~/.cache/*` | npm, Yarn, pip, NuGet HTTP cache, Cargo, Go, Gradle; `.cache\*` | Delete | Unselected: rebuilds and downloads cost time |
| Xcode Archives | `~/Library/Developer/Xcode/Archives/*` | — | Move to Trash | Unselected |
| Project build folders | `node_modules` next to a `package.json`; folders marked with a `CACHEDIR.TAG` file, such as a Rust `target` folder. Each shows the days since its last change. | same | Delete | Unselected |
| Trash | `~/.Trash` | Recycle Bin on all drives | Empty | Unselected |
| Large files | Files and bundles in the home folder of 500 MB or more. A menu sets the limit from 100 MB to 5 GB. | same | Move to Trash | Unselected |
| Old Downloads | Items in Downloads not added or changed for 90 days or more. Hidden files such as `.DS_Store` are ignored. | same | Move to Trash | Unselected |
| Duplicates | Identical files of 1 MB or more in the home folder. The search runs only on request. | same | Move to Trash | Unselected; a helper selects all copies but the oldest |

Some items are listed so the user knows about them, but they can never be selected:

- Virtual machine and disk images: `.vhdx`, `.vmdk`, `.qcow2`, `.utm`, `.pvm`.
- Media libraries: `.photoslibrary`, `.musiclibrary`.
- Mail archives: `.pst`, `.ost`.

## 2. Functional requirements

| ID | Requirement |
|---|---|
| F1 | The header shows total, used and free space of the system disk. |
| F2 | One **Scan** button scans all categories. Sizes update while the scan runs. **Cancel** stops the scan. |
| F3 | The sidebar lists the categories. Each shows its total size and a checkbox. |
| F4 | The main panel lists the items of the chosen category, biggest first. Each row shows a checkbox, name, size, last change and path. |
| F5 | **Reveal** opens Finder or File Explorer at an item. |
| F6 | The footer shows the size and count of selected items, and a **Clean** button. |
| F7 | Clean asks for confirmation. The dialog shows size, count, and which items are deleted and which go to the Trash. |
| F8 | After cleaning, the app shows the bytes removed and the measured change in free space. It lists each skipped item with a reason. |
| F9 | The large-file limit is a menu. A change filters the list at once, with no new scan. |
| F10 | **Find duplicates** runs only when the user clicks it. Groups are sorted by wasted space. |
| F11 | Items of a running app start unselected and show a "running" label. |
| F12 | macOS: items blocked by privacy protection show "Needs Full Disk Access". A button opens that settings page. |

## 3. Safety rules

| ID | Rule |
|---|---|
| S1 | The app runs as a normal user and never asks for admin rights. If it starts as root or elevated, cleaning is turned off. |
| S2 | The app removes only items the scan listed, inside the roots in section 1. It never removes a target's root folder itself. |
| S3 | Just before removal, the app checks each item again: same file identity, not a link, still inside its root. User files must also keep the same size and change time. |
| S4 | The app never follows symlinks, junctions or mount points, and never leaves the volume it started on. |
| S5 | User files (Large files, Old Downloads, Duplicates, Archives) only go to the Trash. If the Trash refuses a file, the app skips it. It never deletes the file instead. |
| S6 | Cloud files are never read or removed. Sync folders are not walked and cloud-only files are skipped. On macOS, a read of a cloud-only file fails instead of downloading it. |
| S7 | A bundle (`.app`, `.photoslibrary` and similar) counts as one item; files inside it are never selected. The app never enters `.git` folders. |
| S8 | A deny list is never shown or removed. See below. |
| S9 | Items that are in use or not permitted are skipped and reported. One failure never stops the clean. |
| S10 | In each duplicate group, at least one copy always stays. |
| S11 | The app makes no network connections and sends no telemetry. |

The S8 deny list:

- **Secrets:** `~/Library/Keychains`, `.ssh`, `.gnupg`, credential files of developer tools.
- **App data:** `Application Support`, `AppData\Roaming`, other apps' sandbox containers, Safari, Mail and Messages data.
- **Windows system files:** `NTUSER.DAT*`, `WindowsApps`.
- **iCloud and App Store caches:** `com.apple.bird`, `CloudKit`, `com.apple.nsurlsessiond` and similar.

## 4. Quality targets

| Area | Target | How we check |
|---|---|---|
| Size | Binary 10 MB or less; `.dmg` 6 MB or less | `ls -lh dist/` |
| Output | Exactly one file in `dist/` | `ls dist/` |
| Start-up | Window ready in under 0.5 s | second launch |
| Idle energy | 0 % CPU and no timers when the app is not working | Activity Monitor → Energy |
| Memory | Under 150 MB during a scan of the home folder | Activity Monitor |
| Scan speed | Junk scan under 5 s; 500,000 files under 20 s on an SSD | timing in a debug build |
| UI | Never freezes. No disk work on the UI thread. At most 4 redraws per second while busy. | manual |
| Platforms | macOS 11 or later on Apple Silicon; Windows 10 and 11 on x64 | build and launch |
| Look | Dark theme only; brush icon | visual check |

## 5. Not in version 1

- Places that need admin rights: Windows Update cache, `C:\Windows\Temp`, `/Library/Caches`, Time Machine snapshots.
- Other apps' sandbox containers, Xcode simulators, iOS backups, IPSW files, Mail attachments.
- Scheduled or background cleaning, a menu-bar helper, auto-update.
- App uninstaller, Windows registry cleaning, "RAM boosters", start-up items.
- Linux, an Intel Mac build, paid code signing and notarization, installers (MSI, PKG).
- Saved settings: limits go back to their defaults at each launch.

## Terms and short names

| Term | Meaning |
|---|---|
| Bundle | A macOS folder that Finder shows as one file, such as `.app` or `.photoslibrary` |
| `CACHEDIR.TAG` | A standard file that marks a folder as a cache that is safe to delete |
| Cloud-only file | A file whose name shows on disk but whose content is still in the cloud. macOS calls it "dataless"; Windows calls it a "placeholder". |
| Full Disk Access | A macOS setting that lets one app read folders that privacy protection blocks |
| Root | The folder a cleaning rule starts from, such as `~/Library/Caches` |
| Target | One cleaning rule: a root, an action and a default selection |
