//! Windows only: writes a `.res` file (icon, manifest, version info) and asks the linker
//! to embed it. `link.exe` and `lld-link` both accept `.res` input, so no resource
//! compiler is needed. For other targets this script does nothing.

use std::env;
use std::fs;
use std::path::PathBuf;

const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
const RT_VERSION: u16 = 16;
const RT_MANIFEST: u16 = 24;
const LANG_EN_US: u16 = 0x0409;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=assets/app.manifest");
    let windows = env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    let msvc = env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    if !(windows && msvc) {
        return;
    }
    let ico = fs::read("assets/icon.ico").expect("read assets/icon.ico");
    let manifest = fs::read("assets/app.manifest").expect("read assets/app.manifest");
    let version = env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
    let res = resources(&ico, &manifest, &version);
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("cleaner.res");
    fs::write(&out, res).expect("write cleaner.res");
    println!("cargo:rustc-link-arg-bins={}", out.display());
}

/// A 32-bit resource file: an empty marker entry, then one entry per resource.
fn resources(ico: &[u8], manifest: &[u8], version: &str) -> Vec<u8> {
    let mut out = Vec::new();
    // Marker: DataSize 0, HeaderSize 32, type 0, name 0, then zeroes.
    out.extend([
        0, 0, 0, 0, 0x20, 0, 0, 0, 0xFF, 0xFF, 0, 0, 0xFF, 0xFF, 0, 0,
    ]);
    out.extend([0u8; 16]);

    // An .ico file: 6-byte header, then 16-byte entries pointing at the images.
    assert!(
        ico.len() > 6 && ico[2] == 1,
        "assets/icon.ico is not an icon file"
    );
    let count = u16::from_le_bytes([ico[4], ico[5]]);
    let mut group = Vec::new();
    group.extend(0u16.to_le_bytes());
    group.extend(1u16.to_le_bytes());
    group.extend(count.to_le_bytes());
    for i in 0..count {
        let e = &ico[6 + 16 * i as usize..6 + 16 * (i as usize + 1)];
        let size = u32::from_le_bytes([e[8], e[9], e[10], e[11]]) as usize;
        let offset = u32::from_le_bytes([e[12], e[13], e[14], e[15]]) as usize;
        entry(
            &mut out,
            RT_ICON,
            i + 1,
            0x1010,
            &ico[offset..offset + size],
        );
        // Group entry: the first 12 bytes of the icon entry, then the resource id.
        group.extend(&e[..12]);
        group.extend((i + 1).to_le_bytes());
    }
    entry(&mut out, RT_GROUP_ICON, 1, 0x1030, &group);
    entry(&mut out, RT_MANIFEST, 1, 0x1030, manifest);
    entry(&mut out, RT_VERSION, 1, 0x0030, &version_info(version));
    out
}

fn entry(out: &mut Vec<u8>, kind: u16, id: u16, flags: u16, data: &[u8]) {
    out.extend((data.len() as u32).to_le_bytes());
    out.extend(32u32.to_le_bytes());
    out.extend([0xFF, 0xFF]);
    out.extend(kind.to_le_bytes());
    out.extend([0xFF, 0xFF]);
    out.extend(id.to_le_bytes());
    out.extend(0u32.to_le_bytes()); // data version
    out.extend(flags.to_le_bytes());
    out.extend(LANG_EN_US.to_le_bytes());
    out.extend(0u32.to_le_bytes()); // version
    out.extend(0u32.to_le_bytes()); // characteristics
    out.extend(data);
    pad4(out);
}

fn pad4(out: &mut Vec<u8>) {
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
}

enum Value<'a> {
    None,
    Text(&'a str),
    Binary(&'a [u8]),
}

/// One VS_VERSIONINFO node: length, value length, type, key, value, children.
fn node(key: &str, value: Value, children: Vec<Vec<u8>>) -> Vec<u8> {
    let (value_len, kind, bytes): (usize, u16, Vec<u8>) = match value {
        Value::None => (0, 1, Vec::new()),
        Value::Text(s) => {
            let wide: Vec<u16> = s.encode_utf16().chain(Some(0)).collect();
            (
                wide.len(),
                1,
                wide.iter().flat_map(|c| c.to_le_bytes()).collect(),
            )
        }
        Value::Binary(b) => (b.len(), 0, b.to_vec()),
    };
    let mut out = vec![0, 0];
    out.extend((value_len as u16).to_le_bytes());
    out.extend(kind.to_le_bytes());
    for c in key.encode_utf16().chain(Some(0)) {
        out.extend(c.to_le_bytes());
    }
    pad4(&mut out);
    out.extend(bytes);
    for child in children {
        pad4(&mut out);
        out.extend(child);
    }
    let len = out.len() as u16;
    out[..2].copy_from_slice(&len.to_le_bytes());
    out
}

fn version_info(version: &str) -> Vec<u8> {
    let mut parts = version.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let v: [u32; 4] = [
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        0,
    ];
    let ms = (v[0] << 16) | v[1];
    let ls = (v[2] << 16) | v[3];
    // VS_FIXEDFILEINFO: signature, struct version, file and product versions, flags mask,
    // flags, OS (Windows NT), type (application), subtype, date.
    let fixed: Vec<u8> = [
        0xFEEF_04BD,
        0x0001_0000,
        ms,
        ls,
        ms,
        ls,
        0x3F,
        0,
        0x0004_0004,
        1,
        0,
        0,
        0,
    ]
    .iter()
    .flat_map(|x: &u32| x.to_le_bytes())
    .collect();
    let strings = [
        ("CompanyName", "Cleaner"),
        ("FileDescription", "Cleaner"),
        ("FileVersion", version),
        ("InternalName", "cleaner"),
        ("LegalCopyright", "MIT License"),
        ("OriginalFilename", "Cleaner.exe"),
        ("ProductName", "Cleaner"),
        ("ProductVersion", version),
    ]
    .iter()
    .map(|(k, v)| node(k, Value::Text(v), Vec::new()))
    .collect();
    let table = node("040904B0", Value::None, strings);
    let string_info = node("StringFileInfo", Value::None, vec![table]);
    let translation = node(
        "Translation",
        Value::Binary(&[0x09, 0x04, 0xB0, 0x04]),
        Vec::new(),
    );
    let var_info = node("VarFileInfo", Value::None, vec![translation]);
    node(
        "VS_VERSION_INFO",
        Value::Binary(&fixed),
        vec![string_info, var_info],
    )
}
