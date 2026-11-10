//! Build helper.
//!
//! - `cargo xtask dist` builds one file into `dist/`: `Cleaner.dmg` on macOS,
//!   `Cleaner.exe` on Windows.
//! - `cargo xtask dist --windows` cross-builds `Cleaner.exe` on macOS with cargo-xwin.
//! - `cargo xtask icons` renders `assets/brush.svg` into `icon.png`, `icon.icns` and `icon.ico`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const APP: &str = "Cleaner";
const BUNDLE_ID: &str = "io.github.hungtd1709.cleaner";
const WINDOWS_TARGET: &str = "x86_64-pc-windows-msvc";

type Result<T = ()> = std::result::Result<T, String>;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("dist") => dist(args.iter().any(|a| a == "--windows")),
        Some("icons") => icons(),
        _ => Err("usage: cargo xtask dist [--windows] | cargo xtask icons".to_owned()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn cargo() -> Command {
    Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
}

fn run(cmd: &mut Command) -> Result {
    let line: Vec<String> = std::iter::once(cmd.get_program())
        .chain(cmd.get_args())
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    eprintln!("> {}", line.join(" "));
    let status = cmd.status().map_err(|e| format!("{}: {e}", line[0]))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("`{}` failed ({status})", line.join(" ")))
    }
}

fn io(context: impl AsRef<Path>) -> impl FnOnce(std::io::Error) -> String {
    let context = context.as_ref().display().to_string();
    move |e| format!("{context}: {e}")
}

fn version(root: &Path) -> Result<String> {
    let manifest =
        fs::read_to_string(root.join("Cargo.toml")).map_err(io(root.join("Cargo.toml")))?;
    manifest
        .lines()
        .find_map(|l| {
            l.strip_prefix("version = \"")
                .and_then(|r| r.strip_suffix('"'))
        })
        .map(str::to_owned)
        .ok_or_else(|| "no version in Cargo.toml".to_owned())
}

fn dist(windows: bool) -> Result {
    let root = root();
    let dist = root.join("dist");
    if dist.exists() {
        fs::remove_dir_all(&dist).map_err(io(&dist))?;
    }
    fs::create_dir_all(&dist).map_err(io(&dist))?;
    let file = if windows || cfg!(windows) {
        windows_exe(&root, &dist)?
    } else {
        mac_dmg(&root, &dist)?
    };
    let count = fs::read_dir(&dist).map_err(io(&dist))?.count();
    if count != 1 {
        return Err(format!("dist/ holds {count} files; expected exactly 1"));
    }
    let size = fs::metadata(&file).map_err(io(&file))?.len();
    eprintln!(
        "\nBuilt {}  ({:.1} MB)",
        file.strip_prefix(&root).unwrap_or(&file).display(),
        size as f64 / 1e6
    );
    Ok(())
}

fn windows_exe(root: &Path, dist: &Path) -> Result<PathBuf> {
    let built = if cfg!(windows) {
        run(cargo()
            .current_dir(root)
            .args(["build", "--release", "--package", "cleaner"]))?;
        root.join("target/release/cleaner.exe")
    } else {
        // cargo-xwin sets its own RUSTFLAGS, so pass the static C runtime flag here too.
        run(cargo()
            .current_dir(root)
            .args([
                "xwin",
                "build",
                "--release",
                "--package",
                "cleaner",
                "--target",
                WINDOWS_TARGET,
            ])
            .env(
                "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS",
                "-C target-feature=+crt-static",
            ))?;
        root.join("target")
            .join(WINDOWS_TARGET)
            .join("release/cleaner.exe")
    };
    let out = dist.join(format!("{APP}.exe"));
    fs::copy(&built, &out).map_err(io(&built))?;
    Ok(out)
}

#[cfg(target_os = "macos")]
fn mac_dmg(root: &Path, dist: &Path) -> Result<PathBuf> {
    run(cargo()
        .current_dir(root)
        .env("MACOSX_DEPLOYMENT_TARGET", "11.0")
        .args(["build", "--release", "--package", "cleaner"]))?;

    // Stage: Cleaner.app plus an Applications link for drag-and-drop install.
    let stage = root.join("target/dmg");
    if stage.exists() {
        fs::remove_dir_all(&stage).map_err(io(&stage))?;
    }
    let app = stage.join(format!("{APP}.app"));
    let contents = app.join("Contents");
    for dir in ["MacOS", "Resources"] {
        fs::create_dir_all(contents.join(dir)).map_err(io(contents.join(dir)))?;
    }
    let binary = root.join("target/release/cleaner");
    fs::copy(&binary, contents.join("MacOS").join(APP)).map_err(io(&binary))?;
    let icns = root.join("assets/icon.icns");
    fs::copy(&icns, contents.join("Resources/icon.icns")).map_err(io(&icns))?;
    fs::write(contents.join("Info.plist"), info_plist(&version(root)?))
        .map_err(io(contents.join("Info.plist")))?;
    fs::write(contents.join("PkgInfo"), "APPL????").map_err(io(contents.join("PkgInfo")))?;

    // Ad-hoc signature by default. A stable identity keeps privacy permissions across builds.
    let identity = env::var("CODESIGN_IDENTITY").unwrap_or_else(|_| "-".to_owned());
    run(Command::new("codesign")
        .args(["--force", "--sign", &identity, "--timestamp=none"])
        .arg(&app))?;

    std::os::unix::fs::symlink("/Applications", stage.join("Applications")).map_err(io(&stage))?;
    let dmg = dist.join(format!("{APP}.dmg"));
    run(Command::new("hdiutil")
        .args([
            "create",
            "-quiet",
            "-volname",
            APP,
            "-fs",
            "HFS+",
            "-format",
            "ULMO",
            "-ov",
            "-srcfolder",
        ])
        .arg(&stage)
        .arg(&dmg))?;
    Ok(dmg)
}

#[cfg(not(target_os = "macos"))]
fn mac_dmg(_root: &Path, _dist: &Path) -> Result<PathBuf> {
    Err("Cleaner.dmg can only be built on macOS".to_owned())
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn info_plist(version: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>CFBundleDisplayName</key><string>{APP}</string>
  <key>CFBundleExecutable</key><string>{APP}</string>
  <key>CFBundleIconFile</key><string>icon</string>
  <key>CFBundleIdentifier</key><string>{BUNDLE_ID}</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleName</key><string>{APP}</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>{version}</string>
  <key>CFBundleVersion</key><string>{version}</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
  <key>NSHumanReadableCopyright</key><string>MIT License</string>
  <key>NSDesktopFolderUsageDescription</key><string>Cleaner looks for large and duplicate files on your Desktop.</string>
  <key>NSDocumentsFolderUsageDescription</key><string>Cleaner looks for large and duplicate files in Documents.</string>
  <key>NSDownloadsFolderUsageDescription</key><string>Cleaner looks for old, large and duplicate files in Downloads.</string>
  <key>NSAppleEventsUsageDescription</key><string>Cleaner asks Finder to empty the Trash.</string>
</dict>
</plist>
"#
    )
}

/// Without the `icons` feature, run xtask again with it; this keeps `dist` quick to build.
#[cfg(not(feature = "icons"))]
fn icons() -> Result {
    run(cargo().current_dir(root()).args([
        "run",
        "--quiet",
        "--package",
        "xtask",
        "--features",
        "icons",
        "--",
        "icons",
    ]))
}

#[cfg(feature = "icons")]
fn icons() -> Result {
    use resvg::tiny_skia::{Pixmap, Transform};
    use resvg::usvg::{Options, Tree};

    let root = root();
    let assets = root.join("assets");
    let svg = fs::read(assets.join("brush.svg")).map_err(io(assets.join("brush.svg")))?;
    let tree = Tree::from_data(&svg, &Options::default()).map_err(|e| format!("brush.svg: {e}"))?;
    let render = |px: u32| -> Result<Pixmap> {
        let mut pixmap = Pixmap::new(px, px).ok_or("bad icon size")?;
        let scale = px as f32 / tree.size().width();
        resvg::render(
            &tree,
            Transform::from_scale(scale, scale),
            &mut pixmap.as_mut(),
        );
        Ok(pixmap)
    };
    // ICO and ICNS want straight (not premultiplied) RGBA.
    let rgba = |pixmap: &Pixmap| -> Vec<u8> {
        pixmap
            .pixels()
            .iter()
            .flat_map(|p| {
                let c = p.demultiply();
                [c.red(), c.green(), c.blue(), c.alpha()]
            })
            .collect()
    };

    let png = render(256)?.encode_png().map_err(|e| e.to_string())?;
    fs::write(assets.join("icon.png"), png).map_err(io(assets.join("icon.png")))?;

    let mut ico = ico::IconDir::new(ico::ResourceType::Icon);
    for px in [16, 24, 32, 48, 64, 128, 256] {
        let image = ico::IconImage::from_rgba_data(px, px, rgba(&render(px)?));
        ico.add_entry(ico::IconDirEntry::encode(&image).map_err(|e| e.to_string())?);
    }
    let file = fs::File::create(assets.join("icon.ico")).map_err(io(assets.join("icon.ico")))?;
    ico.write(file).map_err(|e| e.to_string())?;

    use icns::IconType::*;
    let mut icns = icns::IconFamily::new();
    for (px, kind) in [
        (16, RGBA32_16x16),
        (32, RGBA32_16x16_2x),
        (32, RGBA32_32x32),
        (64, RGBA32_32x32_2x),
        (128, RGBA32_128x128),
        (256, RGBA32_128x128_2x),
        (256, RGBA32_256x256),
        (512, RGBA32_256x256_2x),
        (512, RGBA32_512x512),
        (1024, RGBA32_512x512_2x),
    ] {
        let image = icns::Image::from_data(icns::PixelFormat::RGBA, px, px, rgba(&render(px)?))
            .map_err(|e| e.to_string())?;
        icns.add_icon_with_type(&image, kind)
            .map_err(|e| e.to_string())?;
    }
    let file = fs::File::create(assets.join("icon.icns")).map_err(io(assets.join("icon.icns")))?;
    icns.write(file).map_err(|e| e.to_string())?;

    // A large preview, outside the repo, for checking the artwork by eye.
    let preview = root.join("target/icon-preview.png");
    let _ = fs::create_dir_all(root.join("target"));
    fs::write(
        &preview,
        render(512)?.encode_png().map_err(|e| e.to_string())?,
    )
    .map_err(io(&preview))?;
    eprintln!("Wrote assets/icon.png, icon.ico and icon.icns (preview: target/icon-preview.png)");
    Ok(())
}
