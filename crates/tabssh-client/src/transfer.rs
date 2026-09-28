//! File transfer, both directions, over sftp.
//!
//! Uploads land in the session shell's current directory — wherever the ssh
//! prompt has `cd`-ed to — and downloads land in the command bar's current
//! directory on this machine.  When the upload directory is not writable the
//! transfer falls back to the remote home directory and says so.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::FileAttributes;
use tokio::io::AsyncWriteExt;

use crate::ssh::{shell_quote, Ssh};
use crate::{t, tf};

#[derive(Debug, Clone, Default)]
pub struct Transfer {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone)]
pub struct UploadReport {
    pub target: String,
    pub fell_back: bool,
    pub transfer: Transfer,
}

impl UploadReport {
    pub fn message(&self) -> String {
        if self.fell_back {
            tf!(
                "target directory was not writable; uploaded to home instead: {} file(s), {} dir(s), {} -> {}",
                self.transfer.files,
                self.transfer.dirs,
                human(self.transfer.bytes),
                self.target
            )
        } else {
            tf!(
                "uploaded: {} file(s), {} dir(s), {} -> {}",
                self.transfer.files,
                self.transfer.dirs,
                human(self.transfer.bytes),
                self.target
            )
        }
    }
}

/// "1.2 MiB/s · 0.4s" — what the status line shows once a transfer finishes.
///
/// A fast transfer is reported in milliseconds, because "0.0s" tells nobody
/// anything.
pub fn rate(bytes: u64, elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs_f64().max(0.000_5);
    let time = if secs < 1.0 {
        format!("{}ms", elapsed.as_millis())
    } else {
        format!("{secs:.1}s")
    };
    format!("{}/s · {time}", human((bytes as f64 / secs) as u64))
}

pub fn human(n: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} {}", U[0])
    } else {
        format!("{v:.1} {}", U[i])
    }
}

fn basename(p: &str) -> String {
    let t = p.trim_end_matches('/');
    t.rsplit('/').next().unwrap_or(t).to_string()
}

/// A file name that came from the host is trusted only once it cannot escape
/// the directory it is joined into: no separators, no drive letters, no dot
/// names, no control characters.
fn safe_name(name: &str) -> Result<&str> {
    let bad = name.is_empty()
        || name == "."
        || name == ".."
        || name.contains(['/', '\\', ':'])
        || name.chars().any(|c| c.is_control());
    if bad {
        return Err(anyhow!(
            "{}",
            tf!("refusing an unsafe file name from the host: {}", name)
        ));
    }
    Ok(name)
}

/// Joins a remote directory and a file name with a `/`, trimming any trailing
/// separator from the directory first.
fn join_remote(dir: &str, name: &str) -> String {
    format!("{}/{}", dir.trim_end_matches('/'), name)
}

// ---------------------------------------------------------------------------
// download
// ---------------------------------------------------------------------------

/// Downloads `remote` (file or directory tree) into `local_dir`.
/// Returns the local path that was written.
pub async fn download(ssh: &Ssh, remote: &str, local_dir: &Path) -> Result<(PathBuf, Transfer)> {
    let sftp = ssh.sftp().await?;
    let mut t = Transfer::default();
    let base = basename(remote);
    if base.is_empty() {
        return Err(anyhow!(
            "{}",
            t!("refusing to download the filesystem root")
        ));
    }
    let dest = local_dir.join(safe_name(&base)?);

    let md = sftp
        .metadata(remote)
        .await
        .with_context(|| tf!("stat {}", remote))?;

    let mut seen = std::collections::HashSet::new();
    if md.is_dir() {
        download_dir(&sftp, remote, &dest, &mut t, &mut seen).await?;
    } else {
        download_file(&sftp, remote, &dest, &mut t).await?;
    }
    Ok((dest, t))
}

fn download_dir<'a>(
    sftp: &'a SftpSession,
    remote: &'a str,
    local: &'a Path,
    t: &'a mut Transfer,
    seen: &'a mut std::collections::HashSet<String>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(async move {
        // A symlinked directory tree can point back at itself; without this the
        // walk never ends.  Canonical names make the aliasing visible.
        let canon = sftp
            .canonicalize(remote)
            .await
            .unwrap_or_else(|_| remote.to_string());
        if !seen.insert(canon) {
            return Ok(());
        }
        tokio::fs::create_dir_all(local)
            .await
            .with_context(|| tf!("mkdir {}", local.display()))?;
        t.dirs += 1;
        let rd = sftp
            .read_dir(remote)
            .await
            .with_context(|| tf!("readdir {}", remote))?;
        for entry in rd {
            let raw = entry.file_name();
            let name = safe_name(&raw)?;
            let rp = format!("{}/{}", remote.trim_end_matches('/'), name);
            let lp = local.join(name);
            let md = entry.metadata();
            if md.is_dir() {
                download_dir(sftp, &rp, &lp, t, seen).await?;
            } else if md.is_symlink() {
                // Follow symlinks to regular files; skip dangling ones.
                match sftp.metadata(&rp).await {
                    Ok(m) if m.is_dir() => download_dir(sftp, &rp, &lp, t, seen).await?,
                    Ok(_) => download_file(sftp, &rp, &lp, t).await?,
                    Err(_) => continue,
                }
            } else {
                download_file(sftp, &rp, &lp, t).await?;
            }
        }
        Ok(())
    })
}

async fn download_file(
    sftp: &SftpSession,
    remote: &str,
    local: &Path,
    t: &mut Transfer,
) -> Result<()> {
    if let Some(parent) = local.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut r = sftp
        .open(remote)
        .await
        .with_context(|| tf!("open {}", remote))?;
    // Written to a sibling temp file first: a failed download must not destroy
    // or truncate a file that was already there.
    let tmp = {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let mut tmp = local.as_os_str().to_owned();
        tmp.push(format!(
            ".{}.{}.part",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        PathBuf::from(tmp)
    };
    let copied = async {
        let mut w = tokio::fs::File::create(&tmp)
            .await
            .with_context(|| tf!("create {}", tmp.display()))?;
        let n = tokio::io::copy(&mut r, &mut w).await?;
        w.flush().await?;
        w.shutdown().await?;
        Ok::<u64, anyhow::Error>(n)
    }
    .await;
    let n = match copied {
        Ok(n) => n,
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e);
        }
    };
    if local.exists() {
        tokio::fs::remove_file(local)
            .await
            .with_context(|| tf!("replace {}", local.display()))?;
    }
    tokio::fs::rename(&tmp, local)
        .await
        .with_context(|| tf!("rename {}", local.display()))?;
    t.files += 1;
    t.bytes += n;
    Ok(())
}

// ---------------------------------------------------------------------------
// upload
// ---------------------------------------------------------------------------

/// Uploads local paths into `want_dir`; if that is not writable it falls back
/// to `home`.  Returns a report describing what happened.
pub async fn upload(
    ssh: &Ssh,
    locals: &[PathBuf],
    want_dir: Option<&str>,
    home: &str,
) -> Result<UploadReport> {
    if locals.is_empty() {
        return Err(anyhow!("{}", t!("nothing to upload")));
    }
    let want = want_dir.unwrap_or(home).to_string();

    // A POSIX host always has `test(1)`, so the target is probed directly.
    let writable = ssh
        .exec(&format!("test -d {d} && test -w {d}", d = shell_quote(&want)))
        .await?
        .ok();

    let (target, fell_back) = if writable {
        (want, false)
    } else {
        // Make sure the fallback exists.
        let _ = ssh.exec(&format!("mkdir -p {}", shell_quote(home))).await;
        (home.to_string(), true)
    };

    let sftp = ssh.sftp().await?;
    let mut t = Transfer::default();
    let mut seen = std::collections::HashSet::new();
    for l in locals {
        let name = l
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .ok_or_else(|| anyhow!("{}", tf!("bad local path {}", l.display())))?;
        let remote = join_remote(&target, &name);
        let md = tokio::fs::metadata(l).await?;
        if md.is_dir() {
            upload_dir(&sftp, l, &remote, &mut t, &mut seen).await?;
        } else {
            upload_file(&sftp, l, &remote, md.permissions().readonly(), &mut t).await?;
        }
    }

    Ok(UploadReport {
        target,
        fell_back,
        transfer: t,
    })
}

fn upload_dir<'a>(
    sftp: &'a SftpSession,
    local: &'a Path,
    remote: &'a str,
    t: &'a mut Transfer,
    seen: &'a mut std::collections::HashSet<PathBuf>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(async move {
        // Windows junctions can point a directory back at itself; without this
        // the walk never ends.  Canonical paths make the aliasing visible.
        let canon = tokio::fs::canonicalize(local)
            .await
            .unwrap_or_else(|_| local.to_path_buf());
        if !seen.insert(canon) {
            return Ok(());
        }
        // create_dir fails if it exists; that is fine.
        let _ = sftp.create_dir(remote).await;
        t.dirs += 1;
        let mut rd = tokio::fs::read_dir(local).await?;
        while let Some(entry) = rd.next_entry().await? {
            let name = entry.file_name().to_string_lossy().into_owned();
            let lp = entry.path();
            let rp = format!("{}/{}", remote.trim_end_matches('/'), name);
            // DirEntry::metadata does not follow symlinks, so a link is uploaded
            // as a file (its target's content) and only real directories recurse.
            let md = entry.metadata().await?;
            if md.is_dir() {
                upload_dir(sftp, &lp, &rp, t, seen).await?;
            } else {
                upload_file(sftp, &lp, &rp, md.permissions().readonly(), t).await?;
            }
        }
        Ok(())
    })
}

async fn upload_file(
    sftp: &SftpSession,
    local: &Path,
    remote: &str,
    readonly: bool,
    t: &mut Transfer,
) -> Result<()> {
    let mut r = tokio::fs::File::open(local)
        .await
        .with_context(|| format!("open {}", local.display()))?;
    let mut w = sftp
        .create(remote)
        .await
        .with_context(|| tf!("create {}", remote))?;
    let n = tokio::io::copy(&mut r, &mut w).await?;
    w.flush().await?;
    // A full disk or a quota often only reports at close; a swallowed error
    // would announce a truncated upload as a success.
    w.shutdown()
        .await
        .with_context(|| tf!("close {}", remote))?;
    let attrs = FileAttributes {
        permissions: Some(if readonly { 0o444 } else { 0o644 }),
        ..Default::default()
    };
    let _ = sftp.set_metadata(remote, attrs).await;
    t.files += 1;
    t.bytes += n;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basenames() {
        assert_eq!(basename("/var/log/syslog"), "syslog");
        assert_eq!(basename("/var/log/"), "log");
        assert_eq!(basename("file.txt"), "file.txt");
        assert_eq!(basename("/"), "");
    }

    #[test]
    fn unsafe_names_from_the_host_are_refused() {
        assert!(safe_name("file.txt").is_ok());
        assert!(safe_name("résumé 2024").is_ok());
        for bad in [
            "", ".", "..", "..\\evil", "a/b", "a\\b", "C:\\x", "nul\u{1}",
        ] {
            assert!(safe_name(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn rates_read_sensibly() {
        use std::time::Duration;
        assert_eq!(
            rate(2 * 1024 * 1024, Duration::from_secs(1)),
            "2.0 MiB/s · 1.0s"
        );
        assert_eq!(rate(1024, Duration::from_millis(500)), "2.0 KiB/s · 500ms");
        assert_eq!(rate(1024, Duration::from_millis(20)), "50.0 KiB/s · 20ms");
        // A zero duration must not divide by zero.
        assert!(
            rate(1024, Duration::from_millis(0)).ends_with("0ms"),
            "no panic, no infinity"
        );
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human(512), "512 B");
        assert_eq!(human(2048), "2.0 KiB");
        assert_eq!(human(5 * 1024 * 1024), "5.0 MiB");
    }
}
