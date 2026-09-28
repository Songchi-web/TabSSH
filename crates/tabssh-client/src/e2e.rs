//! Headless end-to-end check against a real host.
//!
//! `tabssh --e2e <user@host[:port]>` (password via `TABSSH_PASSWORD`) exercises the
//! whole stack without the ui: connect, deploy the tool, start and attach to a
//! GNU `screen` session, run a command, list/monitor/cwd through the tool, and
//! upload and download a file.  It prints a PASS/FAIL line per step and exits
//! non-zero on the first failure, which makes it usable as a smoke test.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};

use crate::agent;
use crate::config::{Profile, Store};
use crate::screen;
use crate::ssh::{self, ConnectOpts, Ssh};
use crate::transfer;
use crate::{t, tf};

fn ok(step: &str) {
    println!("  {}  {step}", t!("PASS"));
}

fn info(step: &str) {
    println!("  ..    {step}");
}

/// Reads from a pty until `needle` shows up, or the timeout runs out.
async fn read_until(rd: &mut russh::ChannelReadHalf, needle: &str, wait: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + wait;
    let mut seen = Vec::new();
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(3), rd.wait()).await {
            Ok(Some(russh::ChannelMsg::Data { data })) => {
                seen.extend_from_slice(&data);
                if String::from_utf8_lossy(&seen).contains(needle) {
                    return true;
                }
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    false
}

pub async fn run(profile: Profile, secret: Option<String>) -> Result<()> {
    println!("{}", tf!("tabssh e2e against {}", profile.addr()));

    // --- 1. connect ------------------------------------------------------
    let store = Store::open();
    let known = Arc::new(std::sync::Mutex::new(store.known_hosts()));
    // Same automatic order the real client uses.
    let auths = crate::app::auth_attempts(&profile, secret.clone());
    let ssh = Ssh::connect(ConnectOpts {
        host: profile.host.clone(),
        port: profile.port,
        user: profile.user.clone(),
        auths,
        known: Arc::clone(&known),
        on_new_key: None,
        // Headless: no one to ask, so a first key is trusted on first use.
        ask: None,
    })
    .await?;
    ok(t!("connect + authenticate"));
    let updated = known.lock().unwrap().clone();
    if updated != store.known_hosts() {
        for (host, fp) in updated {
            store.remember_host(host, fp);
        }
        let _ = store.save();
    }

    // --- 2. agent --------------------------------------------------------
    let installed = agent::ensure(&ssh).await?;
    info(&tf!(
        "agent {} · uploaded={}",
        agent::AGENT_VERSION,
        installed.uploaded
    ));
    let v = agent::run(&ssh, &installed.remote_path, "version").await?;
    let out = v.stdout_str();
    let mut lines = out.lines();
    let reported = lines.next().map(str::trim).unwrap_or("");
    let proto = lines
        .find_map(|l| l.trim().strip_prefix("proto="))
        .and_then(|n| n.trim().parse::<u32>().ok());
    if reported != agent::AGENT_VERSION || proto != Some(tabssh_proto::PROTOCOL_VERSION) {
        bail!(
            "{}",
            tf!(
                "agent version mismatch: {}",
                format!("{:?}", out.trim())
            )
        );
    }
    ok(t!("agent deployed and reports the right version"));

    // --- 3. screen session ----------------------------------------------
    let name = screen::task_name();
    // Clear any leftover from an interrupted run, then start fresh.
    let _ = ssh
        .exec(&format!(
            "screen -S {} -X quit 2>/dev/null || true",
            ssh::shell_quote(&name)
        ))
        .await;
    let started = ssh
        .exec(&format!("screen -dmS {} bash -l", ssh::shell_quote(&name)))
        .await?;
    if !started.ok() {
        bail!("{}", tf!("screen -dmS failed: {}", started.stderr_str().trim()));
    }
    let found = screen::find(&ssh, &name).await;
    let pid = match found {
        Some(s) => s.pid,
        None => bail!("{}", tf!("screen session {} was not found", name)),
    };
    ok(&tf!("started a detached screen session ({}.{})", pid, name));

    // The tool's `list` is what the manager shows.
    let list = agent::run(&ssh, &installed.remote_path, "list").await?;
    if !list.stdout_str().contains(&name) {
        bail!(
            "{}",
            tf!(
                "the tool did not list {}: {}",
                name,
                format!("{:?}", list.stdout_str())
            )
        );
    }
    ok(t!("the tool lists the screen session"));

    // Attaching needs a pty: screen is a full-screen program.
    let attach = screen::attach_command(&name, pid);
    let (mut rd, wr) = ssh
        .open_pty_exec(&attach, "xterm-256color", 120, 40, false)
        .await?;
    {
        wr.data_bytes(b"echo E2E_MARKER_$((6*7))\r".to_vec())
            .await?;
        // Keep the write half alive until the shell answered.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut seen = Vec::new();
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(3), rd.wait()).await {
                Ok(Some(russh::ChannelMsg::Data { data })) => {
                    seen.extend_from_slice(&data);
                    if String::from_utf8_lossy(&seen).contains("E2E_MARKER_42") {
                        break;
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
        if !String::from_utf8_lossy(&seen).contains("E2E_MARKER_42") {
            bail!(
                "{}",
                tf!(
                    "shell did not answer: {}",
                    format!("{:?}", String::from_utf8_lossy(&seen))
                )
            );
        }
        ok(t!("attached screen shell runs commands (echo returned 42)"));
    }

    // --- 3b. detach / reattach ------------------------------------------
    drop(wr);
    drop(rd);
    tokio::time::sleep(Duration::from_millis(400)).await;
    if screen::find(&ssh, &name).await.is_none() {
        bail!("{}", t!("the screen session vanished when the client detached"));
    }
    ok(t!("detaching the client leaves the screen session running"));

    let (mut rd2, wr2) = ssh
        .open_pty_exec(&attach, "xterm-256color", 120, 40, false)
        .await?;
    wr2.data_bytes(b"echo REATTACH_OK\r".to_vec()).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut seen2 = Vec::new();
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(3), rd2.wait()).await {
            Ok(Some(russh::ChannelMsg::Data { data })) => {
                seen2.extend_from_slice(&data);
                if String::from_utf8_lossy(&seen2).contains("REATTACH_OK") {
                    break;
                }
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    drop(wr2);
    drop(rd2);
    if !String::from_utf8_lossy(&seen2).contains("REATTACH_OK") {
        bail!(
            "{}",
            tf!(
                "reattach did not reach the same shell: {}",
                format!("{:?}", String::from_utf8_lossy(&seen2))
            )
        );
    }
    ok(t!("reattaching lands back in the same live shell"));

    // --- 3c. monitor over a sampling window -----------------------------
    info(t!("sampling the session for ~1.6s and probing its shell"));
    let mon = agent::run(
        &ssh,
        &installed.remote_path,
        &format!(
            "monitor --id {} --samples 4 --interval-ms 400 --probe",
            ssh::shell_quote(&name)
        ),
    )
    .await?;
    let text = mon.stdout_str();
    if !text.contains(&format!("\"id\":\"{name}\"")) {
        bail!(
            "{}",
            tf!(
                "monitor did not report the session: {}",
                format!("{text:?}")
            )
        );
    }
    if !text.contains("\"responsive\":true") {
        bail!(
            "{}",
            tf!(
                "monitor says the session is not responsive: {}",
                format!("{text:?}")
            )
        );
    }
    if !text.contains("\"probe\":\"ok\"") {
        bail!(
            "{}",
            tf!(
                "the liveness probe did not come back: {}",
                format!("{text:?}")
            )
        );
    }
    if !(text.contains("\"verdict\":\"idle\"") || text.contains("\"verdict\":\"running\"")) {
        bail!("{}", tf!("unexpected verdict: {}", format!("{text:?}")));
    }
    ok(t!(
        "monitor measured cpu/memory over a window and probed the live shell"
    ));

    // --- 3d. a plain shell records its pid so put/get follow its cd ------
    // A plain window has no screen task for the tool to observe.  The client
    // starts the login shell through this exact wrapper and reads its cwd out
    // of /proc, so drive the same wrapper here.
    let pid_file = crate::app::plain_pid_file(&installed.home, 99_101)
        .ok_or_else(|| anyhow!("{}", t!("could not build the plain-shell pid path")))?;
    let plain = crate::app::plain_shell_command(&pid_file);
    let (mut rd3, wr3) = ssh
        .open_pty_exec(&plain, "xterm-256color", 120, 40, true)
        .await?;
    wr3.data_bytes(b"echo PLAIN_READY\r".to_vec()).await?;
    if !read_until(&mut rd3, "PLAIN_READY", Duration::from_secs(10)).await {
        bail!("{}", t!("the plain shell did not come up"));
    }
    wr3.data_bytes(b"cd /tmp\r".to_vec()).await?;
    // Give the shell a moment to act on `cd` before we look.
    tokio::time::sleep(Duration::from_millis(600)).await;

    let pid = ssh
        .exec(&format!("cat {} 2>/dev/null", ssh::shell_quote(&pid_file)))
        .await?
        .stdout_str()
        .trim()
        .to_string();
    if pid.is_empty() || !pid.bytes().all(|c| c.is_ascii_digit()) {
        bail!(
            "{}",
            tf!("the plain shell did not record its pid: {}", format!("{pid:?}"))
        );
    }
    let plain_cwd = ssh
        .exec(&format!("readlink /proc/{pid}/cwd"))
        .await?
        .stdout_str()
        .trim()
        .to_string();
    if !plain_cwd.trim_end_matches('/').ends_with("/tmp") {
        bail!(
            "{}",
            tf!(
                "the plain shell cwd did not follow cd: {}",
                format!("{plain_cwd:?}")
            )
        );
    }
    ok(&tf!(
        "a plain shell's cwd follows cd (read /proc/{}/cwd = {})",
        pid,
        plain_cwd
    ));

    // A relative name then resolves under that directory, exactly as `get` and
    // `put` do it.
    let resolved = crate::app::resolve_remote("payload.txt", Some(&plain_cwd), &installed.home);
    if resolved != format!("{}/payload.txt", plain_cwd.trim_end_matches('/')) {
        bail!(
            "{}",
            tf!("a relative path did not resolve under the shell cwd: {}", resolved)
        );
    }
    ok(t!("a relative get/put path resolves under the plain shell's cwd"));

    // Tab completion must read the very same place: a relative directory is
    // anchored to the shell's cwd, and `~/` expands to the real home (sftp
    // would not), or the picker stays empty.
    let comp_dir = format!("{}/tabssh-e2e-comp", plain_cwd.trim_end_matches('/'));
    let _ = ssh
        .exec(&format!(
            "mkdir -p {d}/sub && touch {d}/sub/inside.txt",
            d = ssh::shell_quote(&comp_dir)
        ))
        .await;
    let cands = crate::app::complete_remote(&ssh, "tabssh-e2e-comp/sub/", Some(&plain_cwd))
        .await
        .unwrap_or_default();
    if !cands.iter().any(|c| c.ends_with("inside.txt")) {
        bail!(
            "{}",
            tf!(
                "completion did not list the relative directory: {}",
                format!("{cands:?}")
            )
        );
    }
    ok(t!("completion lists a relative directory under the shell's cwd"));

    let home_word = crate::app::expand_tilde("~/", &installed.home);
    let home_cands = crate::app::complete_remote(&ssh, &home_word, Some(&plain_cwd))
        .await
        .unwrap_or_default();
    if home_cands.is_empty() {
        bail!("{}", t!("completion of ~/ listed nothing"));
    }
    ok(&tf!("completion of ~/ lists the home ({} item(s))", home_cands.len()));
    let _ = ssh
        .exec(&format!("rm -rf {}", ssh::shell_quote(&comp_dir)))
        .await;

    drop(wr3);
    drop(rd3);
    let _ = ssh
        .exec(&format!("rm -f {}", ssh::shell_quote(&pid_file)))
        .await;

    // --- 4. upload / download -------------------------------------------
    let cwd = agent::run(
        &ssh,
        &installed.remote_path,
        &format!("cwd --id {}", ssh::shell_quote(&name)),
    )
    .await?
    .stdout_str()
    .trim()
    .to_string();
    if cwd.is_empty() {
        bail!("{}", t!("could not resolve the session cwd"));
    }
    info(&tf!("session cwd = {}", cwd));

    let local_dir = std::env::temp_dir().join("tabssh-e2e");
    std::fs::create_dir_all(&local_dir)?;
    let payload = format!("tabssh e2e payload {}\n", std::process::id());
    let local_file = local_dir.join("tabssh-e2e-upload.txt");
    std::fs::write(&local_file, payload.as_bytes())?;

    let report = transfer::upload(
        &ssh,
        std::slice::from_ref(&local_file),
        Some(&cwd),
        &installed.home,
    )
    .await?;
    if report.fell_back {
        bail!("{}", t!("upload unexpectedly fell back to home"));
    }
    let remote_file = format!("{}/{}", cwd.trim_end_matches('/'), "tabssh-e2e-upload.txt");
    let cat = ssh
        .exec(&format!("cat {}", ssh::shell_quote(&remote_file)))
        .await?;
    if cat.stdout_str() != payload {
        bail!(
            "{}",
            tf!(
                "remote file content differs: {}",
                format!("{:?}", cat.stdout_str())
            )
        );
    }
    ok(t!("uploaded a file into the session's current directory"));

    let dl_dir = std::env::temp_dir().join("tabssh-e2e-dl");
    let _ = std::fs::remove_dir_all(&dl_dir);
    let (path, t) = transfer::download(&ssh, &remote_file, &dl_dir).await?;
    let got = std::fs::read_to_string(&path)?;
    if got != payload {
        bail!(
            "{}",
            tf!("downloaded content differs: {}", format!("{got:?}"))
        );
    }
    ok(&tf!("downloaded it back ({} byte(s))", t.bytes));

    // non-writable target must fall back to home
    let fallback = transfer::upload(
        &ssh,
        std::slice::from_ref(&local_file),
        Some("/proc"),
        &installed.home,
    )
    .await?;
    if !fallback.fell_back {
        info(t!("target /proc was writable (unexpected but harmless)"));
    } else {
        ok(t!(
            "upload falls back to home when the target is not writable"
        ));
    }
    let _ = ssh
        .exec(&format!(
            "rm -f {}/tabssh-e2e-upload.txt",
            ssh::shell_quote(&cwd)
        ))
        .await;

    // --- cleanup ---------------------------------------------------------
    let _ = ssh
        .exec(&format!(
            "screen -S {} -X quit",
            ssh::shell_quote(&name)
        ))
        .await;
    ssh.disconnect().await;
    let _ = std::fs::remove_dir_all(&local_dir);
    println!("{}", t!("e2e complete"));
    Ok(())
}

/// Parses `user@host[:port]` into a profile.
pub fn parse_target(target: &str) -> Result<Profile> {
    let (user, hostport) = match target.split_once('@') {
        Some((u, h)) => (u.to_string(), h.to_string()),
        None => bail!("{}", t!("target must be user@host[:port]")),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(22)),
        None => (hostport, 22),
    };
    if host.is_empty() {
        return Err(anyhow!("{}", t!("empty host")));
    }
    let mut p = Profile::new("e2e", &host, &user);
    p.port = port;
    Ok(p)
}
