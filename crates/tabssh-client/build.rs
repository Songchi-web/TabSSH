//! Build steps for the client binary.
//!
//! 1. Embeds the prebuilt linux agent (if present in `dist/`) so the client is
//!    a single self contained binary.  The blob is a static musl binary, so it
//!    is stored zlib-compressed (roughly half its size) and inflated in
//!    `agent.rs` right before it is uploaded.  A missing blob only disables
//!    deployment, it does not break the build.
//! 2. Compiles `app.rc`, so the built `tabssh.exe` carries the application
//!    icon.  This needs `rc.exe` from the Windows SDK; if it cannot be found
//!    the icon is skipped and the build still succeeds, the same graceful
//!    degradation as the agent blob.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    embed_agent(&manifest);
    embed_icon(&manifest);
    embed_build_time();
}

/// Stamps the day of the build into the binary, so the F1 help page can say
/// which build is running.  `SOURCE_DATE_EPOCH` pins it for reproducible
/// packaging; otherwise it is simply today.
fn embed_build_time() {
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    let secs = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        });
    println!("cargo:rustc-env=TABSSH_BUILD_TIME={}", format_utc(secs));
}

/// Formats a Unix timestamp as `YYYY-MM-DD` without pulling in a date crate.
/// The civil-date arithmetic is Howard Hinnant's `civil_from_days`.
fn format_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (y + i64::from(month <= 2), month, day)
}

fn embed_agent(manifest: &str) {
    let dist = Path::new(manifest).join("../../dist");
    println!("cargo:rerun-if-changed={}", dist.display());

    let p = dist.join("tabssh-agent-linux-amd64");
    println!("cargo:rerun-if-changed={}", p.display());
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    let src = match std::fs::read(&p) {
        Ok(bytes) => {
            let mut enc =
                flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
            enc.write_all(&bytes).expect("compress agent blob");
            let packed = enc.finish().expect("finish agent blob");
            let comp = out_dir.join("tabssh-agent-linux-amd64.zlib");
            std::fs::write(&comp, &packed).expect("write compressed agent blob");
            let abs = comp.to_string_lossy().replace('\\', "/");
            format!("pub const LINUX_AMD64: Option<&[u8]> = Some(include_bytes!(r#\"{abs}\"#));\n")
        }
        Err(_) => "pub const LINUX_AMD64: Option<&[u8]> = None;\n".to_string(),
    };
    let out = out_dir.join("blobs.rs");
    std::fs::write(out, src).unwrap();
}

/// Compiles `app.rc` and links the result into the binary, giving `tabssh.exe`
/// its icon.  Only runs when the target is Windows.
fn embed_icon(manifest: &str) {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let rc_file = Path::new(manifest).join("app.rc");
    println!("cargo:rerun-if-changed={}", rc_file.display());
    println!("cargo:rerun-if-changed={}", Path::new(manifest).join("app.ico").display());

    let Some(rc) = find_rc() else {
        println!("cargo:warning=rc.exe not found; building tabssh.exe without an icon");
        return;
    };
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let res = out_dir.join("tabssh.res");
    let ok = Command::new(&rc)
        .current_dir(manifest)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res)
        .arg("app.rc")
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        println!("cargo:rustc-link-arg-bins={}", res.display());
    } else {
        println!("cargo:warning=rc.exe failed; building tabssh.exe without an icon");
    }
}

/// Finds `rc.exe`: on `PATH` first, then the newest Windows Kits installation.
fn find_rc() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            let cand = dir.join("rc.exe");
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    let kits = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut versions: Vec<PathBuf> = std::fs::read_dir(kits)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    versions.sort();
    for version in versions.iter().rev() {
        for arch in ["x64", "x86", "arm64"] {
            let cand = version.join(arch).join("rc.exe");
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}
