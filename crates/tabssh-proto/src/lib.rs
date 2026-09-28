//! Shared JSON types between the `Tabssh` client and the `tabssh-agent` helper.
//!
//! The agent writes these to `sessions/<id>.json`, prints them from `sess list`
//! / `sess monitor`, and the client parses them back.
//!
//! Both crates depend on this module so the JSON form has a single definition.
//! The binary session frame protocol lives with the code that speaks it, in the
//! agent's `sess` module.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// protocol version
// ---------------------------------------------------------------------------

/// Bumped on any incompatible change to the protocol: the JSON types below, the
/// helper's command/verb surface, or its output shape.  The client uploads a
/// fresh helper whenever the running one reports a different value, so an old
/// binary is replaced rather than mis-called.
///
/// v2: the helper became a stateless `screen`-introspecting tool (`list`,
/// `monitor`, `cwd`, `kill`) — the old resident-supervisor verbs are gone.
///
/// v3: the tool lists **all** screen sessions (task names are `<letters><NNN>`,
/// no `tabssh-` prefix any more), so an old tool would hide the new tasks.
pub const PROTOCOL_VERSION: u32 = 3;

// ---------------------------------------------------------------------------
// shared JSON types
// ---------------------------------------------------------------------------

/// The flat metadata written to `sessions/<id>.json` and printed by `sess list`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionMeta {
    pub id: String,
    #[serde(default)]
    pub shell: String,
    #[serde(default)]
    pub pid: i64,
    #[serde(default)]
    pub sup_pid: i64,
    #[serde(default)]
    pub created: u64,
}

/// One `sess monitor` line: the metadata plus the measured fields.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionRecord {
    pub id: String,
    #[serde(default)]
    pub shell: String,
    #[serde(default)]
    pub pid: i64,
    #[serde(default)]
    pub sup_pid: i64,
    #[serde(default)]
    pub created: u64,
    #[serde(default)]
    pub age_secs: u64,
    #[serde(default)]
    pub last_activity_secs: u64,
    #[serde(default)]
    pub samples: u32,
    #[serde(default)]
    pub alive: bool,
    #[serde(default)]
    pub socket: bool,
    #[serde(default)]
    pub responsive: bool,
    #[serde(default = "default_probe")]
    pub probe: String,
    #[serde(default)]
    pub probe_ms: u64,
    #[serde(default)]
    pub cpu_pct: f32,
    #[serde(default)]
    pub cpu_pct_max: f32,
    #[serde(default)]
    pub rss_kb: u64,
    #[serde(default)]
    pub procs: u32,
    #[serde(default)]
    pub agent_version: String,
    #[serde(default)]
    pub verdict: String,
}

fn default_probe() -> String {
    "skipped".into()
}

// ---------------------------------------------------------------------------
// `screen -ls` parsing
// ---------------------------------------------------------------------------

/// One session as listed by `screen -ls`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenSessionLine {
    pub pid: i64,
    pub name: String,
    /// True when some display is currently attached to it.
    pub attached: bool,
}

/// Parses the output of `screen -ls`.
///
/// Each session line's first whitespace token is `<pid>.<name>`; the state and
/// any date column are parenthesised columns after it and can never leak into
/// the name (a date column once made the client miss its own task).  Headers,
/// the "... in ..." footer and "No Sockets found" simply yield no entries.
///
/// Shared by the client and the host agent so the two can never drift apart.
pub fn parse_screen_ls(out: &str) -> Vec<ScreenSessionLine> {
    let mut found = Vec::new();
    for line in out.lines() {
        let Some(head) = line.split_whitespace().next() else {
            continue;
        };
        let Some((pid, name)) = head.split_once('.') else {
            continue;
        };
        let Ok(pid) = pid.parse::<i64>() else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        // The state is the last parenthesised group on the line.
        let attached = match line.rfind('(') {
            Some(i) if line[i..].ends_with(')') => line[i..].contains("Attached"),
            _ => false,
        };
        found.push(ScreenSessionLine {
            pid,
            name: name.to_string(),
            attached,
        });
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_meta_roundtrip() {
        let meta = SessionMeta {
            id: "t\"1".into(),
            shell: "/bin/bash".into(),
            pid: 4321,
            sup_pid: 1200,
            created: 99,
        };
        let line = serde_json::to_string(&meta).unwrap();
        let back: SessionMeta = serde_json::from_str(&line).unwrap();
        assert_eq!(back.id, "t\"1");
        assert_eq!(back.shell, "/bin/bash");
        assert_eq!(back.pid, 4321);
        assert_eq!(back.sup_pid, 1200);
        assert_eq!(back.created, 99);
    }

    #[test]
    fn older_metadata_without_sup_pid_loads() {
        // A line written by a previous agent version must still parse; the
        // missing field defaults to 0.
        let line = "{\"id\":\"old\",\"shell\":\"/bin/bash\",\"pid\":42,\"created\":7}";
        let meta: SessionMeta = serde_json::from_str(line).unwrap();
        assert_eq!(meta.pid, 42);
        assert_eq!(meta.sup_pid, 0);
        assert_eq!(meta.created, 7);
    }

    #[test]
    fn session_record_roundtrip() {
        let rec = SessionRecord {
            id: "web-1".into(),
            shell: "/bin/bash".into(),
            pid: 1234,
            sup_pid: 1200,
            created: 1_737_000_000,
            age_secs: 900,
            last_activity_secs: 3,
            samples: 6,
            alive: true,
            socket: true,
            responsive: true,
            probe: "ok".into(),
            probe_ms: 4,
            cpu_pct: 3.4,
            cpu_pct_max: 12.0,
            rss_kb: 5120,
            procs: 3,
            agent_version: "0.1.0".into(),
            verdict: "running".into(),
        };
        let line = serde_json::to_string(&rec).unwrap();
        let back: SessionRecord = serde_json::from_str(&line).unwrap();
        assert_eq!(back.id, "web-1");
        assert_eq!(back.pid, 1234);
        assert_eq!(back.age_secs, 900);
        assert!(back.alive && back.socket && back.responsive);
        assert_eq!(back.probe, "ok");
        assert!((back.cpu_pct - 3.4).abs() < 1e-6);
        assert!((back.cpu_pct_max - 12.0).abs() < 1e-6);
        assert_eq!(back.verdict, "running");
    }

    #[test]
    fn record_defaults_probe_to_skipped() {
        let rec: SessionRecord = serde_json::from_str("{\"id\":\"x\"}").unwrap();
        assert_eq!(rec.probe, "skipped");
        assert_eq!(rec.id, "x");
    }

    #[test]
    fn parses_a_typical_screen_listing() {
        let out = "There are screens on:\n\
                   \t3024613.qf001\t(Detached)\n\
                   \t3024666.web\t(Attached)\n\
                   2 Sockets in /run/screen/S-alice.\n";
        let got = parse_screen_ls(out);
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[0].pid, 3024613);
        assert_eq!(got[0].name, "qf001");
        assert!(!got[0].attached);
        assert_eq!(got[1].name, "web");
        assert!(got[1].attached);
    }

    #[test]
    fn a_creation_date_column_does_not_leak_into_the_name() {
        let out = "There is a screen on:\n\
                   \t63504.qf001\t(09/16/2026 02:51:08 PM)\t(Detached)\n\
                   1 Socket in /run/screen/S-alice.\n";
        let got = parse_screen_ls(out);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].name, "qf001", "{got:?}");
        assert_eq!(got[0].pid, 63504);
    }

    #[test]
    fn parses_the_empty_and_odd_listings() {
        assert!(parse_screen_ls("No Sockets found in /run/screen/S-me.\n").is_empty());
        assert!(parse_screen_ls("").is_empty());
        assert!(parse_screen_ls("There are screens on:\n1 Socket in /run/screen.\n").is_empty());
        assert!(parse_screen_ls("\tnotapid.name\t(Detached)\n").is_empty());
        assert!(parse_screen_ls("\t99.\t(Detached)\n").is_empty());
        // A multi-display state still counts as attached.
        let got = parse_screen_ls("\t99.name\t(Multi, Attached)\n");
        assert_eq!(got.len(), 1);
        assert!(got[0].attached);
    }
}
