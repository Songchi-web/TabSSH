//! The GNU `screen` extension.
//!
//! A connection runs its shell inside GNU `screen` on the host.  Screen
//! sessions are ordinary detached processes, so the client can list them,
//! reattach to them and kill them from the foreground — the F9 manager
//! refreshes the list twice a second.  `screen` is the only persistence
//! mechanism; the host tool only looks at these sessions, it does not run them.
//! Nothing here needs a package on the host beyond what `screen` already is; if
//! screen is not installed the caller falls back to a plain session and says so.

use anyhow::{bail, Result};

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::app::RemoteSession;
use crate::ssh::{self, Ssh};
use crate::tf;

/// One screen session, as reported by `screen -ls`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenSession {
    pub pid: i64,
    pub name: String,
    /// True when some client is currently attached to it.
    pub attached: bool,
}

/// True when `screen` is on the host's PATH.
pub async fn available(ssh: &Ssh) -> bool {
    matches!(ssh.exec("command -v screen").await, Ok(o) if o.ok() && !o.stdout_str().trim().is_empty())
}

/// Parses the output of `screen -ls` into the client's own row type.
///
/// The parsing itself lives in `tabssh-proto`, shared with the host agent, so
/// the two can never disagree about what a listing means.
pub fn parse_ls(out: &str) -> Vec<ScreenSession> {
    tabssh_proto::parse_screen_ls(out)
        .into_iter()
        .map(|s| ScreenSession {
            pid: s.pid,
            name: s.name,
            attached: s.attached,
        })
        .collect()
}

/// Lists the sessions on the host as rows the manager already understands.
///
/// CPU and memory come from `ps`, one call for every session, so the per-second
/// refresh shows the performance overhead of each task without a burst of round
/// trips.  `pcpu` is a lifetime average, which is exactly the "what has this
/// task cost me" a list wants.
pub async fn list(ssh: &Ssh) -> Result<Vec<RemoteSession>> {
    // `screen -ls` exits non-zero when there is nothing; that is not an error.
    let out = ssh.exec("screen -ls 2>/dev/null || true").await?;
    let mut sessions = parse_ls(&out.stdout_str());
    if sessions.is_empty() {
        return Ok(Vec::new());
    }
    // A stable order, so the manager's rows do not jump between the
    // twice-a-second refreshes.
    sessions.sort_by(|a, b| a.name.cmp(&b.name));

    let pids: Vec<String> = sessions.iter().map(|s| s.pid.to_string()).collect();
    let mut perf = std::collections::BTreeMap::new();
    let ps = format!("ps -o pid=,pcpu=,rss= -p {}", pids.join(","));
    if let Ok(o) = ssh.exec(&ps).await {
        for line in o.stdout_str().lines() {
            let mut it = line.split_whitespace();
            if let (Some(p), Some(c), Some(r)) = (it.next(), it.next(), it.next()) {
                if let (Ok(p), Ok(c), Ok(r)) =
                    (p.parse::<i64>(), c.parse::<f32>(), r.parse::<u64>())
                {
                    perf.insert(p, (c, r));
                }
            }
        }
    }

    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok(sessions
        .into_iter()
        .map(|s| {
            let (cpu, rss) = perf.get(&s.pid).copied().unwrap_or((0.0, 0));
            RemoteSession {
                id: s.name,
                shell: "screen".into(),
                pid: s.pid,
                sup_pid: s.pid,
                created,
                alive: true,
                socket: true,
                responsive: true,
                probe: "screen".into(),
                cpu_pct: cpu,
                cpu_pct_max: cpu,
                rss_kb: rss,
                procs: 1,
                // Reuse the verdict colours: an attached session is in use
                // (green), a detached one is waiting at a prompt (cyan).
                verdict: if s.attached { "running" } else { "idle" }.into(),
                ..Default::default()
            }
        })
        .collect())
}

/// The install command to show for each common distribution, as a
/// `(label, command)` pair.  These are meant to be read and copied out of the
/// settings form, not run by the client.
pub fn install_commands() -> &'static [(&'static str, &'static str)] {
    &[
        (
            "Debian/Ubuntu",
            "sudo apt-get update && sudo apt-get install -y screen",
        ),
        ("Fedora/RHEL", "sudo dnf install -y screen"),
        ("CentOS", "sudo yum install -y screen"),
        ("Alpine", "sudo apk add screen"),
        ("openSUSE", "sudo zypper install -y screen"),
        ("Arch", "sudo pacman -S --noconfirm screen"),
    ]
}

/// The process-once, two letter prefix shared by every task this client starts.
///
/// A random prefix keeps two clients on one host from colliding, and it is
/// drawn once per process so every task a session creates shares a visible
/// family — `qf001`, `qf002`, …  Seeded from the clock and the pid with a small
/// xorshift, which is plenty for a name that only has to be unlikely to clash.
fn prefix() -> &'static str {
    static PREFIX: OnceLock<String> = OnceLock::new();
    PREFIX.get_or_init(|| {
        let mut seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
            ^ (std::process::id() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        if seed == 0 {
            seed = 0x2545_F491_4F6C_DD1D;
        }
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let a = (next() % 26) as u8;
        let b = (next() % 26) as u8;
        let letters = [b'a' + a, b'a' + b];
        String::from_utf8_lossy(&letters).into_owned()
    })
}

/// The name of the next GNU `screen` task: two random lowercase letters and a
/// three digit sequence, e.g. `qf001`.
///
/// One window, one task: the sequence is handed out process-wide so two windows
/// can never share a name (`screen -d -r` would detach the other window and
/// `screen -dmS` would pile up duplicate sessions).  The counter wraps at 1000,
/// keeping every name the fixed `[a-z]{2}[0-9]{3}` shape the manager addresses.
pub fn task_name() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(1);
    let n = SEQ.fetch_add(1, Ordering::Relaxed) % 1000;
    format!("{}{:03}", prefix(), n)
}

/// The first session on the host whose name matches, from `screen -ls`.
pub async fn find(ssh: &Ssh, name: &str) -> Option<ScreenSession> {
    let out = ssh.exec("screen -ls 2>/dev/null || true").await.ok()?;
    parse_ls(&out.stdout_str())
        .into_iter()
        .find(|s| s.name == name)
}

/// Starts a detached session named `name`, running the user's login shell
/// (`$SHELL`, falling back to `sh` — an Alpine host has no bash).
pub async fn start(ssh: &Ssh, name: &str) -> Result<()> {
    let out = ssh
        .exec(&format!(
            "screen -dmS {} sh -c 'exec \"${{SHELL:-/bin/sh}}\" -l'",
            ssh::shell_quote(name)
        ))
        .await?;
    if !out.ok() {
        bail!("{}", tf!("screen: {}", out.stderr_str().trim()));
    }
    Ok(())
}

/// Kills a session and everything running in it.
pub async fn kill(ssh: &Ssh, name: &str) -> Result<()> {
    let out = ssh
        .exec(&format!("screen -S {} -X quit", ssh::shell_quote(name)))
        .await?;
    if !out.ok() {
        bail!("{}", tf!("screen: {}", out.stderr_str().trim()));
    }
    Ok(())
}

/// Runs one GNU `screen` command against a session: `screen -S <name> -X
/// <verb> [arg]`.  This is the one place the client talks `screen` back at a
/// running session — `sessionname`, `title` and `detach` all go through here.
pub async fn command(ssh: &Ssh, name: &str, verb: &str, arg: Option<&str>) -> Result<()> {
    let mut cmd = format!("screen -S {} -X {}", ssh::shell_quote(name), verb);
    if let Some(arg) = arg {
        cmd.push(' ');
        cmd.push_str(&ssh::shell_quote(arg));
    }
    let out = ssh.exec(&cmd).await?;
    if !out.ok() {
        bail!("{}", tf!("screen: {}", out.stderr_str().trim()));
    }
    Ok(())
}

/// How much scrollback a task this client starts should keep, so a later
/// reattach has more of its past to replay.  Best-effort: a host that refuses
/// the command just keeps its default.
pub const HISTORY_LINES: u32 = 20_000;

/// The exact `screen -S <name> -X scrollback <lines>` command.
pub fn scrollback_command(name: &str, lines: u32) -> String {
    format!(
        "screen -S {} -X scrollback {}",
        ssh::shell_quote(name),
        lines
    )
}

/// The exact `screen -S <name> -X hardcopy -h <path>` command.
pub fn hardcopy_command(name: &str, path: &str) -> String {
    format!(
        "screen -S {} -X hardcopy -h {}",
        ssh::shell_quote(name),
        ssh::shell_quote(path)
    )
}

/// Grows a session's scrollback buffer so more of its history survives on the
/// host.  Best-effort — a failure just leaves the default in place.
pub async fn set_scrollback(ssh: &Ssh, name: &str, lines: u32) -> Result<()> {
    let out = ssh.exec(&scrollback_command(name, lines)).await?;
    if !out.ok() {
        bail!("{}", tf!("screen: {}", out.stderr_str().trim()));
    }
    Ok(())
}

/// Dumps a session's current window — its scrollback included — to `path` on
/// the host, so the client can replay the past text into a window.
pub async fn hardcopy(ssh: &Ssh, name: &str, path: &str) -> Result<()> {
    let out = ssh.exec(&hardcopy_command(name, path)).await?;
    if !out.ok() {
        bail!("{}", tf!("screen: {}", out.stderr_str().trim()));
    }
    Ok(())
}

/// The command that attaches this client to the background task.
///
/// `-d -r` resumes the session, detaching any other display first, so it never
/// stops to ask a question.  Addressing it as `<pid>.<name>` (the way `screen
/// -ls` prints it) picks one session when the host has several with the same
/// name — leftovers from earlier builds — instead of listing them and exiting.
pub fn attach_command(name: &str, pid: i64) -> String {
    let plain = format!("screen -d -r {}", ssh::shell_quote(name));
    if pid > 0 {
        let target = ssh::shell_quote(&format!("{pid}.{name}"));
        format!("screen -d -r {target} || {plain}")
    } else {
        plain
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_typical_listing() {
        let out = "There are screens on:\n\
                   \t3024613.tabssh-k07-123\t(Detached)\n\
                   \t3024666.web\t(Attached)\n\
                   2 Sockets in /run/screen/S-alice.\n";
        let got = parse_ls(out);
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[0].pid, 3024613);
        assert_eq!(got[0].name, "tabssh-k07-123");
        assert!(!got[0].attached);
        assert_eq!(got[1].name, "web");
        assert!(got[1].attached);
    }

    #[test]
    fn a_creation_date_column_does_not_leak_into_the_name() {
        // Ubuntu's `screen -ls` inserts a date between the name and the state.
        // The name must be exactly `tabssh-alice`, never with the date stuck on
        // the end (that once made `has()` miss the task and the window hang).
        let out = "There is a screen on:\n\
                   \t63504.tabssh-alice\t(09/16/2026 02:51:08 PM)\t(Detached)\n\
                   1 Socket in /run/screen/S-alice.\n";
        let got = parse_ls(out);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].name, "tabssh-alice", "{got:?}");
        assert_eq!(got[0].pid, 63504);
        assert!(!got[0].attached);
    }

    #[test]
    fn parses_the_empty_and_odd_cases() {
        assert!(parse_ls("No Sockets found in /run/screen/S-me.\n").is_empty());
        assert!(parse_ls("").is_empty());
        assert!(parse_ls("There are screens on:\n1 Socket in /run/screen.\n").is_empty());
        // A multi-display state still counts as attached.
        let got = parse_ls("\t99.name\t(Multi, Attached)\n");
        assert_eq!(got.len(), 1);
        assert!(got[0].attached);
    }

    #[test]
    fn the_attach_command_resumes_deterministically() {
        assert_eq!(attach_command("web", 0), "screen -d -r web");
        assert_eq!(
            attach_command("web", 99),
            "screen -d -r 99.web || screen -d -r web"
        );
        assert_eq!(
            attach_command("a b", 7),
            "screen -d -r '7.a b' || screen -d -r 'a b'"
        );
    }

    #[test]
    fn the_history_commands_address_the_session_and_quote_paths() {
        assert_eq!(
            hardcopy_command("qf001", "/home/me/.tabssh/hc"),
            "screen -S qf001 -X hardcopy -h /home/me/.tabssh/hc"
        );
        assert_eq!(
            hardcopy_command("a b", "/tmp/h c"),
            "screen -S 'a b' -X hardcopy -h '/tmp/h c'"
        );
        assert_eq!(
            scrollback_command("qf001", HISTORY_LINES),
            "screen -S qf001 -X scrollback 20000"
        );
    }

    #[test]
    fn task_names_are_two_letters_and_a_three_digit_sequence() {
        let a = task_name();
        let b = task_name();
        let shape = |s: &str| {
            let bytes = s.as_bytes();
            bytes.len() == 5
                && bytes[..2].iter().all(|c| c.is_ascii_lowercase())
                && bytes[2..].iter().all(|c| c.is_ascii_digit())
        };
        assert!(shape(&a), "{a}");
        assert!(shape(&b), "{b}");
        // Each window gets its own task name, or two windows would fight over
        // one shell and duplicate sessions would pile up.
        assert_ne!(a, b, "names must be unique: {a} == {b}");
        // Both come from the same process-once family.
        assert_eq!(&a[..2], &b[..2], "{a} vs {b}");
    }

    #[test]
    fn the_install_commands_cover_the_common_distributions() {
        let cmds = install_commands();
        assert!(cmds
            .iter()
            .any(|(os, c)| os.contains("Ubuntu") && c.contains("apt-get")));
        assert!(cmds.iter().any(|(_, c)| c.contains("dnf")));
        assert!(cmds.iter().any(|(_, c)| c.contains("apk")));
        assert!(cmds.iter().all(|(_, c)| c.contains("screen")));
    }
}
