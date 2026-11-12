# Cleaner — requirements

Cleaner frees disk space on macOS and Windows without losing user data. It finds junk and forgotten files and shows their sizes. It removes only what the user confirms.

The design that meets these requirements is in [solution.md](solution.md).

## 1. What the app cleans

**Default rule:** an item starts selected only if apps can re-create it at no real cost and its app is not in use. An app is in use while it runs. Browser and app-data caches also count as in use if they changed in the last 15 minutes. A folder in `~/Library/Caches` starts selected only if its name is a bundle id, such as `com.spotify.client`. Developer tools keep costly downloads there under plain names.

| Category | macOS | Windows | Action | Default |
|---|---|---|---|---|
| System junk | Third-party `~/Library/Caches/*`; logs and crash reports older than 7 days; `$TMPDIR` items older than 7 days | `%TEMP%` items older than 7 days; `CrashDumps` and `WER` older than 7 days; `INetCache` | Delete | Selected, except plain-named cache folders |
| Optional system caches | `com.apple.*` caches that are not on the deny list | DirectX shader cache | Delete | Unselected |
| Browser caches | Cache folders of Chrome, Edge, Brave, Firefox, Safari | Cache, Code Cache, GPUCache and `cache2` of Chrome, Edge, Brave, Firefox; all profiles | Delete | Selected |
| App data: Electron caches | One item per app that keeps Chromium caches (`Code Cache`, `GPUCache`, `Cache` and similar) in `~/Library/Application Support/<App>` | The same, in `%APPDATA%` and `%LOCALAPPDATA%` | Delete | Selected |
| App data: Claude | Old Claude Code versions, except the newest and the one `claude` starts. Claude Code sessions older than `cleanupPeriodDays` (30 days by default). Old Claude Desktop components. The Cowork VM image, about 10 GB. | same | Delete | Old versions selected; sessions and the VM image unselected |
| Developer caches | Xcode DerivedData, iOS DeviceSupport, simulator caches; npm, Yarn, Yarn Berry, pnpm, Bun, pip, Poetry, Homebrew, CocoaPods, Cargo, Go, Gradle, NuGet, Maven, Composer, Dart, Deno, Android, Playwright, Cypress, Electron, node-gyp, SwiftPM, cargo-xwin, ccache, JetBrains IDEs; `~/.cache/*` | npm, Yarn, Yarn Berry, pnpm, Bun, pip, uv, Poetry, Cargo, Go, Gradle, NuGet, Maven, Composer, Dart, Deno, Android, Playwright, Electron; `.cache\*` | Delete | Unselected: rebuilds and downloads cost time |
| Xcode Archives | `~/Library/Developer/Xcode/Archives/*` | — | Move to Trash | Unselected |
| Project build folders | `node_modules` next to a `package.json`, and folders marked with a `CACHEDIR.TAG` file, such as a Rust `target` folder. Only in the user's own folders (rule S13), so never inside a VS Code extension. Each shows when its project last changed. | same | Delete | Unselected; a button selects projects idle for 30+ days |
| Trash | `~/.Trash` | Recycle Bin on all drives | Empty | Unselected |
| Large files | The user's own files and bundles of 500 MB or more (rule S13). A menu sets the limit from 100 MB to 5 GB. | same | Move to Trash | Unselected |
| Old Downloads | Items in Downloads not added or changed for 90 days or more. Hidden files such as `.DS_Store` are ignored. | same | Move to Trash | Unselected |
| Installers | Files at the top of Downloads, of any age: `.dmg`, `.pkg`, `.msi`, `.msix`, `.appx`, setup `.exe`, and zips that hold an app or installer | same | Move to Trash | Unselected |
| Duplicates | Identical files of 1 MB or more among the user's own files (rule S13). Every match is checked byte by byte. The search runs only on request. | same | Move to Trash | Unselected; a helper selects all copies but the oldest |

Some items are listed so the user knows about them, but they can never be selected:

- Virtual machine and disk images: `.vhdx`, `.vmdk`, `.qcow2`, `.utm`, `.pvm`.
- Media libraries: `.photoslibrary`, older photo libraries such as `.photolibrary`, `.musiclibrary`.
- Mail archives: `.pst`, `.ost`.

## 2. Functional requirements

| ID | Requirement |
|---|---|
| F1 | The header shows total, used and free space of the system disk. |
| F2 | One **Scan** button scans all categories. Sizes update while the scan runs. **Cancel** stops the scan. |
| F3 | The sidebar lists the categories. Each shows its total size and a checkbox. |
| F4 | The main panel lists the items of the chosen category, biggest first. Each row shows a checkbox, name, size, last change and path. |
| F5 | **Show** opens Finder or File Explorer at an item. |
| F6 | The footer shows the size and count of selected items, and a **Clean** button. A file listed in two categories counts once. |
| F7 | Clean asks for confirmation. The dialog shows size, count, and which items are deleted and which go to the Trash. |
| F8 | After cleaning, the app shows the bytes removed and the measured change in free space. It lists each skipped item with a reason. |
| F9 | The large-file limit is a menu. A change filters the list at once, with no new scan. |
| F10 | **Find duplicates** runs only when the user clicks it. Groups are sorted by wasted space. |
| F11 | Items of an app in use start unselected and show "in use — quit the app first". |
| F12 | macOS: items blocked by privacy protection show "needs Full Disk Access". A button opens that settings page. |
| F13 | **Space map** lists the folders in home, hidden ones included, biggest first. A click opens a folder; a breadcrumb goes back. Each folder shows how much the lists can clean in it, or a tip for known tools. The map never removes anything. |
| F14 | A search box filters the list by name or path. The list's selection buttons apply to the matching rows. The sidebar checkbox applies to the whole category. |
| F15 | Clicking Name, Size or Changed sorts the list. The choice is saved. |
| F16 | **Preview** shows a file with Quick Look (macOS) or opens it (Windows). Space previews the clicked row. Windows only shows programs, installers and scripts in File Explorer, because opening them would run them. |
| F17 | After cleaning, **Put back** returns every item that went to the Trash. Deleted caches cannot come back. |
| F18 | **Never touch:** a right-click on a row, or the Settings dialog, adds a folder or file that Cleaner never lists and never removes. The list and the large-file limit are saved. |
| F19 | In Build folders, **Select projects idle 30+ days** selects the build folders of projects that have not changed for 30 days. |

## 3. Safety rules

| ID | Rule |
|---|---|
| S1 | The app runs as a normal user and never asks for admin rights. If it starts as root or elevated, cleaning is turned off. |
| S2 | The app removes only items the scan listed, inside the roots in section 1. It never removes a target's root folder itself. |
| S3 | Just before removal, the app checks each item again: same file identity, not a link, still inside its root. User files must also keep the same size and change time. |
| S4 | The app never follows symlinks, junctions or mount points, and never leaves the volume it started on. |
| S5 | User files (Large files, Old Downloads, Duplicates, Archives) only go to the Trash. If the Trash refuses a file, the app skips it. It never deletes the file instead. |
| S6 | Cloud files are never read or removed. Sync folders are not walked and cloud-only files are skipped. On macOS, a read of a cloud-only file fails instead of downloading it. |
| S7 | A bundle (`.app`, `.photoslibrary` and similar) counts as one item; files inside it are never selected. Nothing inside a `.git` folder is listed; the Space map only measures it. |
| S8 | A deny list is never shown or removed. See below. |
| S9 | Items that are in use or not permitted are skipped and reported. One failure never stops the clean. |
| S10 | In each duplicate group, at least one copy always stays. |
| S11 | The app makes no network connections and sends no telemetry. |
| S12 | Paths on the Never touch list are not walked, listed or removed. |
| S13 | Large files, Duplicates and Build folders come only from the user's own folders. Hidden folders, environments and tool installs are left out. Duplicates also skip Git projects. |

The S8 deny list:

- **Secrets:** `~/Library/Keychains`, `.ssh`, `.gnupg`, credential files of developer tools, the Hugging Face login token.
- **Claude Code memory:** `~/.claude/projects/*/memory`, the notes that Claude Code keeps for each project.
- **Tool data inside caches:** Deno's `location_data` (local storage and KV databases), and the `LocalHistory` of JetBrains IDEs and Android Studio.
- **Sync folders:** nothing inside them, wherever they are.
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

## Terms and short names

| Term | Meaning |
|---|---|
| App data | Data that apps keep in their own folders: caches, old versions and old sessions |
| Bundle | A macOS folder that Finder shows as one file, such as `.app` or `.photoslibrary` |
| Bundle id | The reverse-domain name that identifies a macOS app, such as `com.spotify.client` |
| `CACHEDIR.TAG` | A standard file that marks a folder as a cache that is safe to delete |
| Cloud-only file | A file whose name shows on disk but whose content is still in the cloud. macOS calls it "dataless"; Windows calls it a "placeholder". |
| Cowork VM | The virtual machine image that Claude Desktop uses for Cowork and code execution |
| Electron app | A desktop app built on Chromium, such as VS Code, Slack or Claude Desktop |
| Environment | A folder of installed packages, such as a Python `venv`, `site-packages` or `vendor` |
| Full Disk Access | A macOS setting that lets one app read folders that privacy protection blocks |
| Hidden folder | A folder whose name starts with a dot, such as `~/.rustup` |
| Never touch list | The user's list of folders and files that Cleaner leaves alone |
| Root | The folder a cleaning rule starts from, such as `~/Library/Caches` |
| Space map | A read-only view of folder sizes |
| Target | One cleaning rule: a root, an action and a default selection |
| Tool install | A tool's own folder in home, such as `~/go` or `~/miniconda3` |
