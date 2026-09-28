//! Deployment of the little server side tool.
//!
//! The client carries a static tool for the host platform it supports and
//! uploads it on first use, then runs it over ssh.  The tool is stateless: it
//! only introspects the host's GNU `screen` sessions (`list`, `monitor`, `cwd`,
//! `kill`), so nothing about a session is stored on the host and nothing else is
//! required there beyond a working `ssh`/`sftp` login.
//!
//! Supported today:
//!   linux x86-64 — session listing and monitoring
//!
//! Anything else is reported as unsupported rather than half-working.

use anyhow::{anyhow, bail, Result};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::FileAttributes;
use tokio::io::AsyncWriteExt;

use crate::ssh::{self, ExecOut, Ssh};
use crate::{t, tf};

mod blobs {
    include!(concat!(env!("OUT_DIR"), "/blobs.rs"));
}

pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const REMOTE_DIR: &str = ".tabssh";

/// The host platform we ship a helper for, as `os-arch`.
pub fn bundled_targets() -> Vec<&'static str> {
    let mut v = Vec::new();
    if blobs::LINUX_AMD64.is_some() {
        v.push("linux-x86_64");
    }
    v
}

/// The bundled agent for this platform, inflated from the compressed form the
/// build script embeds (a static musl binary is roughly half its size
/// compressed).
fn bundled_bytes() -> Option<Vec<u8>> {
    use std::io::Read;
    let packed = blobs::LINUX_AMD64?;
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(packed)
        .read_to_end(&mut out)
        .ok()?;
    Some(out)
}

pub struct DeployInfo {
    /// Absolute path of the installed agent, in the host's own syntax.
    pub remote_path: String,
    /// The login's home directory, used as the upload fallback.
    #[cfg_attr(not(feature = "e2e"), allow(dead_code))]
    pub home: String,
    /// True when the binary was (re)uploaded this time.
    #[cfg_attr(not(feature = "e2e"), allow(dead_code))]
    pub uploaded: bool,
}

/// Uploads raw bytes over sftp.
pub async fn upload_bytes(
    sftp: &SftpSession,
    path: &str,
    data: &[u8],
    mode: Option<u32>,
) -> Result<()> {
    let mut f = sftp
        .create(path)
        .await
        .map_err(|e| anyhow!("{}", tf!("cannot create {}: {}", path, e)))?;
    f.write_all(data).await?;
    f.flush().await?;
    // A full disk often only reports at close; a truncated helper would be
    // "installed" and then fail its version probe in a loop.
    f.shutdown().await?;
    if let Some(m) = mode {
        let attrs = FileAttributes {
            permissions: Some(m),
            ..Default::default()
        };
        let _ = sftp.set_metadata(path, attrs).await;
    }
    Ok(())
}

/// The login's home directory.
pub async fn remote_home(ssh: &Ssh) -> Result<String> {
    let out = ssh.exec("printf %s \"$HOME\"").await?;
    let home = out.stdout_str().trim().to_string();
    if home.is_empty() {
        bail!("{}", t!("could not resolve the remote home directory"));
    }
    Ok(home)
}

/// Runs `<bin> version` and reports whether the helper is this exact crate
/// version *and* speaks the current protocol.  Any failure to run it counts as
/// "not current", which forces a re-upload.
///
/// `version` prints the crate version on line 1 and `proto=<N>` on line 2.  An
/// older agent prints only the first line, so the protocol reads as unknown (0)
/// and the helper is correctly treated as stale.
async fn agent_is_current(ssh: &Ssh, bin: &str) -> bool {
    let cmd = format!("{} version", ssh::shell_quote(bin));
    let Ok(out) = ssh.exec(&cmd).await else {
        return false;
    };
    if !out.ok() {
        return false;
    }
    let stdout = out.stdout_str();
    let mut lines = stdout.lines();
    let version = lines.next().map(str::trim).unwrap_or("");
    let proto = lines
        .find_map(|l| l.trim().strip_prefix("proto="))
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(0);
    version == AGENT_VERSION && proto == tabssh_proto::PROTOCOL_VERSION
}

/// Ensures a matching agent is present on the host and returns where it is.
///
/// A same-size remote file is only a cheap first signal: it cannot tell a
/// same-size rebuild apart.  So the real check is always `<bin> version`, which
/// both the already-installed path and the post-upload path go through.  A
/// mismatch (or an agent too old to report the protocol version) re-uploads the
/// helper once and verifies again; a mismatch after a fresh upload is a hard
/// error, and the caller falls back to a plain shell.
pub async fn ensure(ssh: &Ssh) -> Result<DeployInfo> {
    let home = remote_home(ssh).await?;
    let bin = format!("{home}/{REMOTE_DIR}/tabssh-agent");

    let blob = bundled_bytes().ok_or_else(|| {
        anyhow!(
            "{}",
            tf!(
                "no tabssh-agent is bundled for this platform (bundled: {})",
                bundled_targets().join(", ")
            )
        )
    })?;

    let sftp = ssh.sftp().await?;
    let remote_size = sftp.metadata(&bin).await.ok().and_then(|m| m.size);

    // Cheap first signal only; the version probe below is what decides.
    let size_matches = remote_size == Some(blob.len() as u64);
    if size_matches && agent_is_current(ssh, &bin).await {
        return Ok(DeployInfo {
            remote_path: bin,
            home,
            uploaded: false,
        });
    }

    // The only blob targets linux x86-64; check before shipping megabytes to a
    // host that cannot run them (the module docs promise "unsupported", not a
    // confusing version mismatch after every connect).
    let uname = ssh.exec("uname -sm").await?;
    let platform = uname.stdout_str().trim().to_string();
    if !uname.ok() || !platform.eq_ignore_ascii_case("linux x86_64") {
        bail!(
            "{}",
            tf!(
                "no tabssh-agent for this host ({}); need linux x86_64",
                if platform.is_empty() {
                    t!("unknown platform")
                } else {
                    &platform
                }
            )
        );
    }

    // Create ~/.tabssh, using the home directory we actually resolved rather than
    // trusting a literal `$HOME` to match it.
    let dir = format!("{home}/{REMOTE_DIR}");
    let mk = ssh
        .exec(&format!(
            "mkdir -p {} && chmod 700 {}",
            ssh::shell_quote(&dir),
            ssh::shell_quote(&dir)
        ))
        .await?;
    if !mk.ok() {
        bail!(
            "{}",
            tf!(
                "cannot create the .tabssh directory on the host: {}",
                mk.stderr_str().trim()
            )
        );
    }

    // Write beside the target and move, so a running agent is replaced
    // atomically rather than truncated under itself.
    let tmp = format!("{bin}.new");
    upload_bytes(&sftp, &tmp, &blob, Some(0o755)).await?;
    let mv = ssh
        .exec(&format!(
            "mv -f {} {}",
            ssh::shell_quote(&tmp),
            ssh::shell_quote(&bin)
        ))
        .await?;
    if !mv.ok() {
        // Do not leave the half-installed `.new` file behind.
        let _ = ssh.exec(&format!("rm -f {}", ssh::shell_quote(&tmp))).await;
        bail!(
            "{}",
            tf!("could not install the helper: {}", mv.stderr_str().trim())
        );
    }

    // We just (re)uploaded, so the helper must now report the right version; a
    // mismatch here means the upload did not take.
    if !agent_is_current(ssh, &bin).await {
        bail!(
            "{}",
            tf!(
                "agent version mismatch: {}",
                format!("expected {AGENT_VERSION}, proto={}", tabssh_proto::PROTOCOL_VERSION)
            )
        );
    }

    Ok(DeployInfo {
        remote_path: bin,
        home,
        uploaded: true,
    })
}

/// Runs the installed agent with the given argument string.
#[cfg_attr(not(feature = "e2e"), allow(dead_code))]
pub async fn run(ssh: &Ssh, remote_path: &str, args: &str) -> Result<ExecOut> {
    ssh.exec(&format!("{} {}", ssh::shell_quote(remote_path), args))
        .await
}
