//! tabssh-agent: the small helper the client uploads to a plain
//! `openssh-server` box.  It is a transient tool invoked over `ssh exec`:
//! persistence is provided by GNU `screen`, so the agent only discovers,
//! measures and reports — it never stays resident.

#[cfg(unix)]
mod sess;

use std::process::ExitCode;

#[cfg(unix)]
use std::io;

#[cfg(unix)]
fn usage() -> ExitCode {
    eprintln!(
        "tabssh-agent {version}

usage:
  tabssh-agent version
  tabssh-agent list
  tabssh-agent monitor [--id NAME] [--probe] [--samples N] [--interval-ms MS]
  tabssh-agent cwd --id NAME
  tabssh-agent kill --id NAME [--force]

`list` and `monitor` print one JSON object per line for each GNU screen
session.  The agent keeps no state; sessions live in `screen`.",
        version = env!("CARGO_PKG_VERSION")
    );
    ExitCode::from(2)
}

pub struct Args {
    pub positional: Vec<String>,
    pub flags: Vec<(String, String)>,
}

impl Args {
    pub fn parse(argv: impl Iterator<Item = String>) -> Args {
        let mut positional = Vec::new();
        let mut flags = Vec::new();
        let mut it = argv.peekable();
        while let Some(a) = it.next() {
            if let Some(name) = a.strip_prefix("--") {
                let (k, v) = match name.split_once('=') {
                    Some((k, v)) => (k.to_string(), v.to_string()),
                    None => {
                        // boolean flag if the next token is another flag/eof
                        let v = match it.peek() {
                            Some(n) if !n.starts_with("--") => it.next().unwrap(),
                            _ => "true".to_string(),
                        };
                        (name.to_string(), v)
                    }
                };
                flags.push((k, v));
            } else {
                positional.push(a);
            }
        }
        Args { positional, flags }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.flags
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn num<T: std::str::FromStr>(&self, key: &str, default: T) -> T {
        self.get(key)
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }

    pub fn has(&self, key: &str) -> bool {
        self.flags.iter().any(|(k, _)| k == key)
    }
}

fn main() -> ExitCode {
    let args = Args::parse(std::env::args().skip(1));

    let cmd = args.positional.first().map(|s| s.as_str()).unwrap_or("");

    match cmd {
        "version" => {
            // Line 1: the crate version.  Line 2: the wire/JSON protocol
            // version.  The client uploads a fresh agent whenever either
            // differs, so both are printed here.  Platform-independent.
            println!("{}", env!("CARGO_PKG_VERSION"));
            println!("proto={}", tabssh_proto::PROTOCOL_VERSION);
            ExitCode::SUCCESS
        }
        _ => dispatch(cmd, &args),
    }
}

#[cfg(unix)]
fn report(res: io::Result<()>) -> ExitCode {
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tabssh-agent: {e}");
            ExitCode::from(1)
        }
    }
}

#[cfg(unix)]
fn require_id(args: &Args) -> io::Result<String> {
    args.get("id")
        .map(str::to_string)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "--id is required"))
}

#[cfg(unix)]
fn dispatch(cmd: &str, args: &Args) -> ExitCode {
    let res: io::Result<()> = match cmd {
        "list" => sess::list(),
        "monitor" => sess::monitor(&sess::MonitorOpts {
            id: args.get("id").map(str::to_string),
            samples: args.num("samples", sess::MONITOR_DEFAULT_SAMPLES),
            interval_ms: args.num("interval-ms", sess::MONITOR_DEFAULT_INTERVAL_MS),
            probe: args.has("probe"),
        }),
        "cwd" => match require_id(args) {
            Ok(id) => sess::cwd(&id),
            Err(e) => Err(e),
        },
        "kill" => match require_id(args) {
            Ok(id) => sess::kill(&id, args.has("force")),
            Err(e) => Err(e),
        },
        _ => return usage(),
    };
    report(res)
}

/// Screen sessions and `/proc` introspection are unix-only.  The client only
/// uploads this tool to a Linux host, so on Windows there is nothing to do;
/// exit non-zero with a readable line rather than working badly or panicking.
#[cfg(not(unix))]
fn dispatch(_cmd: &str, _args: &Args) -> ExitCode {
    eprintln!("tabssh-agent: screen-backed sessions are not supported on this platform");
    ExitCode::from(1)
}
