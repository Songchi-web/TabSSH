//! Stateless inspection of GNU `screen` sessions.
//!
//! Persistence is provided by `screen` (the client attaches with `screen -d -r`);
//! this binary is a transient tool invoked over `ssh exec`.  It discovers
//! screen sessions from `screen -ls`, resolves each session's shell
//! through `/proc`, measures the shell, prints JSON and exits.  It keeps no
//! state: no sockets, no heartbeat or metadata files, no resident process.

use std::fs;
use std::io::{self, Write};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tabssh_proto::{SessionMeta, SessionRecord};

/// A session whose shell burns more than this share of a single core during the
/// sampling window is reported `running` instead of `idle`.
pub const RUNNING_CPU_THRESHOLD: f64 = 1.0;

/// Defaults for `monitor`: six samples, half a second apart (~3s window).
pub const MONITOR_DEFAULT_SAMPLES: u32 = 6;
pub const MONITOR_DEFAULT_INTERVAL_MS: u64 = 500;

fn err(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::Other, msg.into())
}

// ---------------------------------------------------------------------------
// screen discovery
// ---------------------------------------------------------------------------

/// Run `screen` with `args`, trying the PATH spelling and the usual absolute
/// locations so a bare `ssh exec` (minimal PATH) still finds it.
fn screen(args: &[&str]) -> Option<std::process::Output> {
    for bin in ["screen", "/usr/bin/screen", "/usr/local/bin/screen", "/bin/screen"] {
        match Command::new(bin).args(args).output() {
            Ok(o) => return Some(o),
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        }
    }
    None
}

/// Parse `screen -ls` into (server pid, name) pairs.
///
/// The parsing is shared with the client through `tabssh-proto`, so the two can
/// never disagree about what a listing means.
fn parse_screen_ls(out: &str) -> Vec<(i32, String)> {
    tabssh_proto::parse_screen_ls(out)
        .into_iter()
        .map(|s| (s.pid as i32, s.name))
        .collect()
}

struct ScreenSession {
    name: String,
    server_pid: i32,
}

/// Every live screen session on the host, whatever its name.
fn screen_sessions() -> Vec<ScreenSession> {
    let Some(out) = screen(&["-ls"]) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    parse_screen_ls(&text)
        .into_iter()
        .map(|(server_pid, name)| ScreenSession { name, server_pid })
        .collect()
}

// ---------------------------------------------------------------------------
// /proc discovery
// ---------------------------------------------------------------------------

/// The pids in `/proc/<pid>/task/<pid>/children`.
fn children_of(pid: i32) -> Vec<i32> {
    let Ok(s) = fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")) else {
        return Vec::new();
    };
    s.split_whitespace()
        .filter_map(|t| t.parse::<i32>().ok())
        .collect()
}

/// The session shell: the screen server's first child.
fn shell_pid_of(server_pid: i32) -> Option<i32> {
    children_of(server_pid).into_iter().next()
}

/// `comm` (field 2 of `/proc/<pid>/stat`, i.e. the process name).
fn proc_comm(pid: i32) -> Option<String> {
    let s = fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

struct SessionInfo {
    name: String,
    server_pid: i32,
    shell_pid: i32,
    shell: String,
    pgrp: i32,
    created: u64,
}

/// Resolve a screen session to its shell.  The shell pid is required; the stat
/// fields are best-effort so a transient `/proc` race does not drop the row.
fn resolve(s: &ScreenSession) -> Option<SessionInfo> {
    let shell_pid = shell_pid_of(s.server_pid)?;
    let (pgrp, created) = match proc_stat(shell_pid) {
        Some(st) => (st.pgrp, created_of(&st)),
        None => (0, 0),
    };
    Some(SessionInfo {
        name: s.name.clone(),
        server_pid: s.server_pid,
        shell_pid,
        shell: proc_comm(shell_pid).unwrap_or_default(),
        pgrp,
        created,
    })
}

fn find_session(id: &str) -> Option<SessionInfo> {
    let s = screen_sessions().into_iter().find(|s| s.name == id)?;
    resolve(&s)
}

// ---------------------------------------------------------------------------
// process introspection
// ---------------------------------------------------------------------------

fn clk_tck() -> u64 {
    let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if v > 0 {
        v as u64
    } else {
        100
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The parts of `/proc/<pid>/stat` we care about.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ProcStat {
    pid: i32,
    state: char,
    pgrp: i32,
    session: i32,
    utime: u64,
    stime: u64,
    starttime: u64,
}

/// Parse a `/proc/<pid>/stat` line.  `comm` (field 2) may contain spaces and
/// parentheses, so the fixed fields are only located after the last `)`.
fn parse_stat(s: &str) -> Option<ProcStat> {
    let rparen = s.rfind(')')?;
    let pid = s[..rparen].split_whitespace().next()?.parse::<i32>().ok()?;
    let rest: Vec<&str> = s[rparen + 1..].split_whitespace().collect();
    // After `)`: state(3) ppid(4) pgrp(5) session(6) ... utime(14) stime(15)
    // ... starttime(22).
    let state = rest.first()?.chars().next()?;
    let pgrp = rest.get(2)?.parse::<i32>().ok()?;
    let session = rest.get(3)?.parse::<i32>().ok()?;
    let utime = rest.get(11)?.parse::<u64>().ok()?;
    let stime = rest.get(12)?.parse::<u64>().ok()?;
    let starttime = rest.get(19)?.parse::<u64>().ok()?;
    Some(ProcStat {
        pid,
        state,
        pgrp,
        session,
        utime,
        stime,
        starttime,
    })
}

fn proc_stat(pid: i32) -> Option<ProcStat> {
    let s = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat(&s)
}

/// `btime` from `/proc/stat`: boot time in unix seconds.
fn boot_time() -> Option<u64> {
    let s = fs::read_to_string("/proc/stat").ok()?;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("btime ") {
            return rest.trim().parse().ok();
        }
    }
    None
}

/// Real start time of a process: boot time plus its `starttime` ticks.
fn created_of(st: &ProcStat) -> u64 {
    let hz = clk_tck();
    if hz == 0 {
        return 0;
    }
    boot_time().unwrap_or(0) + st.starttime / hz
}

/// `kill(pid, 0)`, treating EPERM as "exists".
fn pid_exists(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// A pid still around and not a zombie.  `/proc` may be unreadable for a
/// foreign pid; assume it is live then rather than misreporting it as gone.
fn pid_alive(pid: i32) -> bool {
    if !pid_exists(pid) {
        return false;
    }
    match proc_stat(pid) {
        Some(st) => st.state != 'Z',
        None => true,
    }
}

/// `VmRSS` in kB from `/proc/<pid>/status`.
fn vm_rss_kb(pid: i32) -> u64 {
    let Ok(s) = fs::read_to_string(format!("/proc/{pid}/status")) else {
        return 0;
    };
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
        }
    }
    0
}

/// Number of processes in process group `pgrp`.
fn count_pgrp(pgrp: i32) -> u32 {
    if pgrp <= 0 {
        return 0;
    }
    let mut n = 0u32;
    let Ok(dir) = fs::read_dir("/proc") else {
        return 0;
    };
    for e in dir.flatten() {
        let Some(name) = e.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Ok(pid) = name.parse::<i32>() else {
            continue;
        };
        if let Some(st) = proc_stat(pid) {
            if st.pgrp == pgrp {
                n += 1;
            }
        }
    }
    n
}

/// Every descendant of `pid`, found through the `children` files.
fn descendants(pid: i32) -> Vec<i32> {
    let mut out = Vec::new();
    let mut stack = vec![pid];
    while let Some(p) = stack.pop() {
        for c in children_of(p) {
            out.push(c);
            stack.push(c);
        }
    }
    out
}

/// Percent of one core from a tick delta over an elapsed wall-clock window.
fn cpu_pct_from_ticks(delta_ticks: u64, elapsed_secs: f64, hz: u64) -> f64 {
    if hz == 0 || elapsed_secs <= 0.0 {
        return 0.0;
    }
    (delta_ticks as f64 / hz as f64) / elapsed_secs * 100.0
}

/// Pick the verdict from the measured signals.  `socket` no longer has a
/// meaning (there is no socket); callers pass `true`.
fn verdict_of(
    alive: bool,
    socket: bool,
    responsive: bool,
    probe_ok: bool,
    cpu_max: f64,
) -> &'static str {
    if !socket {
        "dead"
    } else if !alive || !responsive {
        "unresponsive"
    } else if probe_ok || cpu_max >= RUNNING_CPU_THRESHOLD {
        "running"
    } else {
        "idle"
    }
}

// ---------------------------------------------------------------------------
// verbs
// ---------------------------------------------------------------------------

/// One `SessionMeta` JSON per screen session.
pub fn list() -> io::Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for s in screen_sessions() {
        let Some(info) = resolve(&s) else {
            continue;
        };
        let meta = SessionMeta {
            id: info.name,
            shell: info.shell,
            pid: info.shell_pid as i64,
            sup_pid: info.server_pid as i64,
            created: info.created,
        };
        let line = serde_json::to_string(&meta).map_err(|e| err(e.to_string()))?;
        writeln!(out, "{line}")?;
    }
    Ok(())
}

/// Print the session shell's working directory, nothing else.
pub fn cwd(id: &str) -> io::Result<()> {
    let info = find_session(id).ok_or_else(|| err(format!("no such session: {id}")))?;
    let path = fs::read_link(format!("/proc/{}/cwd", info.shell_pid))?;
    println!("{}", path.display());
    Ok(())
}

/// Ask screen to quit the session; with `--force` additionally SIGKILL the
/// screen server and its descendants in case it does not honour the request.
pub fn kill(id: &str, force: bool) -> io::Result<()> {
    let found = screen_sessions().into_iter().find(|s| s.name == id);
    let Some(session) = found else {
        return Err(err(format!("no such session: {id}")));
    };
    // `screen -X quit` reports failure on its exit status; without --force that
    // failure is the answer, or the caller is told a dead session is gone.
    let quit_ok = screen(&["-S", id, "-X", "quit"])
        .map(|out| out.status.success())
        .unwrap_or(false);
    if force {
        let mut pids = descendants(session.server_pid);
        pids.push(session.server_pid);
        for p in pids {
            unsafe { libc::kill(p, libc::SIGKILL) };
        }
        return Ok(());
    }
    if !quit_ok {
        return Err(err(format!("screen refused to quit {id}")));
    }
    Ok(())
}

pub struct MonitorOpts {
    pub id: Option<String>,
    pub samples: u32,
    pub interval_ms: u64,
    pub probe: bool,
}

struct Measured {
    ticks: u64,
    rss_kb: u64,
    procs: u32,
}

fn measure(info: &SessionInfo) -> Measured {
    let ticks = proc_stat(info.shell_pid)
        .map(|st| st.utime + st.stime)
        .unwrap_or(0);
    Measured {
        ticks,
        rss_kb: vm_rss_kb(info.shell_pid),
        procs: count_pgrp(info.pgrp),
    }
}

struct Load {
    first: u64,
    prev: u64,
    max: f64,
    last: Measured,
}

impl Default for Load {
    fn default() -> Load {
        Load {
            first: 0,
            prev: 0,
            max: 0.0,
            last: Measured {
                ticks: 0,
                rss_kb: 0,
                procs: 0,
            },
        }
    }
}

/// One `SessionRecord` JSON per matching session, one per line.  All sessions
/// are sampled in lockstep so the wall-clock window is shared.
pub fn monitor(opts: &MonitorOpts) -> io::Result<()> {
    let infos: Vec<SessionInfo> = match &opts.id {
        Some(id) => find_session(id).into_iter().collect(),
        None => screen_sessions().iter().filter_map(resolve).collect(),
    };

    let n = opts.samples.max(1) as usize;
    let interval = Duration::from_millis(opts.interval_ms);
    let hz = clk_tck();

    let mut loads: Vec<Load> = infos.iter().map(|_| Load::default()).collect();
    let window_start = Instant::now();
    let mut prev = Instant::now();
    for i in 0..n {
        if i > 0 {
            std::thread::sleep(interval);
        }
        let dt = prev.elapsed().as_secs_f64();
        let measured: Vec<Measured> = infos.iter().map(measure).collect();
        prev = Instant::now();
        for (j, m) in measured.into_iter().enumerate() {
            if i == 0 {
                loads[j].first = m.ticks;
            } else {
                let dticks = m.ticks.saturating_sub(loads[j].prev);
                loads[j].max = loads[j].max.max(cpu_pct_from_ticks(dticks, dt, hz));
            }
            loads[j].prev = m.ticks;
            loads[j].last = m;
        }
    }
    let window = window_start.elapsed().as_secs_f64();

    let stdout = io::stdout();
    let mut out = stdout.lock();
    for (info, load) in infos.iter().zip(loads.iter()) {
        let alive = true; // it is listed
        let responsive = pid_alive(info.shell_pid);
        let (probe, probe_ms) = if opts.probe {
            if responsive {
                ("ok", 0u64)
            } else {
                ("timeout", 0u64)
            }
        } else {
            ("skipped", 0u64)
        };
        let now = unix_now();
        let age_secs = if info.created > 0 {
            now.saturating_sub(info.created)
        } else {
            0
        };
        let cpu_pct = cpu_pct_from_ticks(load.last.ticks.saturating_sub(load.first), window, hz);
        let verdict = verdict_of(alive, true, responsive, probe == "ok", load.max);

        let rec = SessionRecord {
            id: info.name.clone(),
            shell: info.shell.clone(),
            pid: info.shell_pid as i64,
            sup_pid: info.server_pid as i64,
            created: info.created,
            age_secs,
            last_activity_secs: 0,
            samples: n as u32,
            alive,
            socket: true,
            responsive,
            probe: probe.to_string(),
            probe_ms,
            cpu_pct: cpu_pct as f32,
            cpu_pct_max: load.max as f32,
            rss_kb: load.last.rss_kb,
            procs: load.last.procs,
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
            verdict: verdict.to_string(),
        };
        let line = serde_json::to_string(&rec).map_err(|e| err(e.to_string()))?;
        writeln!(out, "{line}")?;
    }
    Ok(())
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
        let got = parse_screen_ls(out);
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[0], (3024613, "tabssh-k07-123".to_string()));
        assert_eq!(got[1], (3024666, "web".to_string()));
    }

    #[test]
    fn a_creation_date_column_does_not_leak_into_the_name() {
        // Ubuntu's `screen -ls` inserts a date between the name and the state.
        let out = "There is a screen on:\n\
                   \t63504.tabssh-alice\t(09/16/2026 02:51:08 PM)\t(Detached)\n\
                   1 Socket in /run/screen/S-alice.\n";
        let got = parse_screen_ls(out);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0], (63504, "tabssh-alice".to_string()));
    }

    #[test]
    fn parses_the_empty_and_odd_cases() {
        assert!(parse_screen_ls("No Sockets found in /run/screen/S-me.\n").is_empty());
        assert!(parse_screen_ls("").is_empty());
        assert!(parse_screen_ls("There are screens on:\n1 Socket in /run/screen.\n").is_empty());
        assert!(parse_screen_ls("\tnotapid.name\t(Detached)\n").is_empty());
        assert!(parse_screen_ls("\t99.\t(Detached)\n").is_empty());
    }

    #[test]
    fn parse_stat_reads_pgrp_ticks_and_starttime() {
        // comm contains spaces and parentheses on purpose.
        let line = "1234 (my we(ird) shell) S 1000 1234 1234 34816 1234 4194304 \
12 0 0 0 17 5 0 0 20 0 1 0 555 12345678 500 18446744073709551615 \
0 0 0 0 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0 0 0 0 0 0 0 0";
        let st = parse_stat(line).unwrap();
        assert_eq!(st.pid, 1234);
        assert_eq!(st.state, 'S');
        assert_eq!(st.pgrp, 1234);
        assert_eq!(st.utime, 17);
        assert_eq!(st.stime, 5);
        assert_eq!(st.starttime, 555);
    }

    #[test]
    fn parse_stat_rejects_garbage() {
        assert_eq!(parse_stat(""), None);
        assert_eq!(parse_stat("not a stat line"), None);
        assert_eq!(parse_stat("1234 (x) S 1"), None); // truncated
    }

    #[test]
    fn cpu_pct_from_synthetic_ticks() {
        // 100 Hz, 50 ticks in half a second == one full core.
        assert!((cpu_pct_from_ticks(50, 0.5, 100) - 100.0).abs() < 1e-9);
        // 25 ticks in half a second == half a core.
        assert!((cpu_pct_from_ticks(25, 0.5, 100) - 50.0).abs() < 1e-9);
        // No ticks is idle.
        assert_eq!(cpu_pct_from_ticks(0, 0.5, 100), 0.0);
        // Degenerate inputs must not divide by zero or panic.
        assert_eq!(cpu_pct_from_ticks(10, 0.0, 100), 0.0);
        assert_eq!(cpu_pct_from_ticks(10, 1.0, 0), 0.0);
    }

    #[test]
    fn verdict_thresholds() {
        // No socket: dead, even if the pid lingers.
        assert_eq!(verdict_of(true, false, false, false, 0.0), "dead");
        // Socket answers (it always does now) but the shell is gone.
        assert_eq!(verdict_of(false, true, true, false, 0.0), "unresponsive");
        assert_eq!(verdict_of(true, true, false, false, 0.0), "unresponsive");
        // Alive and quiet: idle, which is normal.
        assert_eq!(verdict_of(true, true, true, false, 0.0), "idle");
        assert_eq!(verdict_of(true, true, true, false, 0.5), "idle");
        // Measured CPU above the threshold, or an explicit probe, means running.
        assert_eq!(
            verdict_of(true, true, true, false, RUNNING_CPU_THRESHOLD),
            "running"
        );
        assert_eq!(verdict_of(true, true, true, true, 0.0), "running");
    }

    #[test]
    fn session_record_roundtrip() {
        let rec = SessionRecord {
            id: "tabssh-web".into(),
            shell: "bash".into(),
            pid: 1234,
            sup_pid: 1200,
            created: 1_737_000_000,
            age_secs: 900,
            last_activity_secs: 0,
            samples: 6,
            alive: true,
            socket: true,
            responsive: true,
            probe: "ok".into(),
            probe_ms: 0,
            cpu_pct: 3.4,
            cpu_pct_max: 12.0,
            rss_kb: 5120,
            procs: 3,
            agent_version: "0.1.0".into(),
            verdict: "running".into(),
        };
        let line = serde_json::to_string(&rec).unwrap();
        let back: SessionRecord = serde_json::from_str(&line).unwrap();
        assert_eq!(back.id, "tabssh-web");
        assert_eq!(back.pid, 1234);
        assert_eq!(back.sup_pid, 1200);
        assert!(back.alive && back.socket && back.responsive);
        assert_eq!(back.probe, "ok");
        assert_eq!(back.verdict, "running");
    }
}
