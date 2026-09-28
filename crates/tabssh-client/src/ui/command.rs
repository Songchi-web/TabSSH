use std::path::PathBuf;

use crate::app::{App, Cmd, Overlay, Row, SessionId, View};
use crate::lang::{self, Lang};
use crate::{t, tf};

use super::*;

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

/// Opens a window onto a saved connection.
///
/// `persistent` asks for a new GNU `screen` task behind the window (the
/// manager's Tab key); without it the window is a plain shell, which is what
/// Enter gives you.
pub(super) fn open_profile(app: &mut App, p: &crate::config::Profile, persistent: bool) {
    let id = app.new_session(p);
    let secret = app.store.secret(&p.name);
    app.send(Cmd::Open {
        id,
        profile: p.clone(),
        secret,
        cols: app.cols,
        rows: app.rows,
        persistent,
    });
    // Test the host for `screen` over its own background connection, so the
    // settings form's status field is filled in.
    probe_screen(app, p);
    app.view = View::Terminal;
    app.notice(tf!("connecting to {}…", p.addr()));
}

/// Tests, over a background connection, whether the host has GNU `screen`.
///
/// Sent every time a connection is opened (and when the settings form opens),
/// so the form's status field reflects the server rather than a guess.
pub(super) fn probe_screen(app: &mut App, p: &crate::config::Profile) {
    let secret = app.store.secret(&p.name);
    app.send(Cmd::ProbeScreen {
        profile: p.clone(),
        secret,
    });
}

/// Goes back to a session that is still running on the host.
///
/// If one of our windows already shows that host session, this *switches to
/// it* instead of opening another window onto the same thing — pressing Enter
/// on a host session must never leave two windows fighting over it.
pub(super) fn reattach(app: &mut App, remote_id: &str) {
    if let Some(i) = app
        .sessions
        .iter()
        .position(|s| s.remote_id.as_deref() == Some(remote_id))
    {
        app.active = Some(i);
        app.view = View::Terminal;
        let name = app.sessions[i].name();
        app.notice(tf!("switched to window {}", name));
        return;
    }
    let profile = app
        .active_session()
        .filter(|s| s.host == app.remote_host)
        .map(|s| s.profile.clone())
        .or_else(|| {
            app.sessions
                .iter()
                .find(|s| s.host == app.remote_host)
                .map(|s| s.profile.clone())
        })
        .or_else(|| {
            app.store
                .profiles()
                .iter()
                .find(|p| p.host == app.remote_host)
                .cloned()
        });
    let Some(profile) = profile else {
        app.notice(tf!(
            "no saved connection for {}; save one with F2 'new <name> <user@host>'",
            app.remote_host
        ));
        return;
    };
    // Everything the host lists is a GNU `screen` task now — that is the only
    // persistence mechanism — so reattaching always goes through screen.  The
    // pid disambiguates a name that appears more than once in `screen -ls`, and
    // it has to be the *screen server's* pid: the tool reports its shell's pid
    // in `pid` and the server's in `sup_pid`, while the `screen -ls` fallback
    // carries the server's in both.
    let pid = app
        .remote_sessions
        .iter()
        .find(|r| r.id == remote_id)
        .map(|r| if r.sup_pid > 0 { r.sup_pid } else { r.pid })
        .unwrap_or(0);
    let id = app.new_session(&profile);
    let secret = app.store.secret(&profile.name);
    let (cols, rows) = (app.cols, app.rows);
    probe_screen(app, &profile);
    app.send(Cmd::Attach {
        id,
        profile,
        secret,
        remote_id: remote_id.to_string(),
        cols,
        rows,
        pid,
    });
    app.view = View::Terminal;
    app.notice(tf!("reattaching to {}…", remote_id));
}

/// The three digit number of a window, if that is what this is.
pub(super) fn session_number(arg: &str) -> Option<u64> {
    if arg.len() == 3 && arg.chars().all(|c| c.is_ascii_digit()) {
        arg.parse().ok()
    } else {
        None
    }
}

/// `C:` or `c:` — a Windows drive.
pub(super) fn has_drive_prefix(a: &str) -> bool {
    let b = a.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// `\server\share`.
pub(super) fn is_unc(a: &str) -> bool {
    a.starts_with(r"\\")
}

/// Turns a local argument into a real path.
///
/// Only a drive letter, `~`, `/` or a UNC share says "a specific place"; the
/// rest — `./x`, `../x`, or a bare name — is taken from the client's current
/// directory.  `.` and `..` are folded away lexically, so what is shown is
/// what you asked for without touching the filesystem.
///
/// `~` here means the desktop, which is the root the local browser starts
/// from: `cd ~` is how you snap the F2 working directory back to the start, so
/// it must land on the same place the client opens on rather than on
/// `%USERPROFILE%`.
pub(super) fn resolve_local(app: &App, arg: &str) -> PathBuf {
    let a = arg.trim();
    if a.is_empty() || a == "~" {
        return crate::config::desktop_dir();
    }
    if let Some(rest) = a.strip_prefix("~/").or_else(|| a.strip_prefix(r"~\")) {
        return fold(crate::config::desktop_dir().join(rest));
    }
    if a.starts_with('~') {
        return crate::config::desktop_dir();
    }
    let p = PathBuf::from(a);
    // A leading `/` counts on Windows too, where `Path::is_absolute` does not
    // recognise it.
    if p.is_absolute() || a.starts_with('/') || has_drive_prefix(a) || is_unc(a) {
        fold(p)
    } else {
        fold(app.cwd.join(p))
    }
}

/// Folds `.` and `..` away lexically, so the path shown is the one you asked
/// for without touching the filesystem.
pub(super) fn fold(p: PathBuf) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

/// Where `cd` should go: nothing means the desktop, which is the root the
/// local browser starts from.
pub(super) fn cd_target(app: &App, arg: &str) -> PathBuf {
    if arg.trim().is_empty() {
        return crate::config::desktop_dir();
    }
    resolve_local(app, arg)
}

/// Where a `get` lands.
///
/// The connection's own download directory wins when it names one; otherwise
/// the file goes to the F2 command bar's current directory, so downloads follow
/// `cd` exactly as `put` follows it for the files it sends.  It starts at the
/// desktop and is never written to disk, so it resets on the next run.
pub(super) fn download_target(app: &App) -> PathBuf {
    if let Some(custom) = app
        .active_session()
        .and_then(|s| s.profile.download_dir.as_ref())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        let p = PathBuf::from(custom);
        if std::fs::create_dir_all(&p).is_ok() {
            return p;
        }
    }
    app.cwd.clone()
}

/// One directory's contents, names first, the way `ls` shows them.
pub(super) fn list_local(dir: &std::path::Path) -> Overlay {
    let mut items = Vec::new();
    match std::fs::read_dir(dir) {
        Ok(rd) => {
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                if is_dir {
                    items.push(format!("{name}/"));
                } else {
                    let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                    items.push(format!("{name}  {}", crate::transfer::human(size)));
                }
            }
            items.sort();
        }
        Err(e) => items.push(tf!("cannot read: {}", e)),
    }
    Overlay::listing(dir.display().to_string(), items)
}

/// Closes the window or connection you name, or ends a task.
///
/// * a window (or nothing: the active one) — the window closes; its GNU `screen`
///   task keeps running, so it can be reattached later;
/// * a task (by name or id) — the task is ended; a window attached to it stays
///   and reconnects as a fresh plain shell;
/// * a saved connection's name — its windows are closed and the connection is
///   forgotten.
///
/// This is the `quit` command.  It never ends the program: closing the window
/// you are looking at, and only that, is all it does.  F10 is the only way out.
pub(super) fn run_quit(app: &mut App, arg: Option<&str>) {
    let Some(arg) = arg else {
        // Nothing named: the window you are looking at.
        match app.active_id() {
            Some(id) => {
                app.send(Cmd::Close { id });
                app.notice(t!("closing the current window"));
            }
            None => app.notice(t!("open a window first")),
        }
        return;
    };

    // A bare three digit number is always one of your windows.
    if let Some(n) = session_number(arg) {
        match app.sessions.iter().find(|s| s.number == n) {
            Some(s) => {
                let id = s.id;
                app.send(Cmd::Close { id });
                app.notice(tf!("closing window {}", arg));
            }
            None => app.notice(tf!("no session {}", arg)),
        }
        return;
    }

    // A task, named or by id: end it, keeping any window that is attached to it.
    if let Some(rid) = named_task(app, arg) {
        end_task(app, &rid);
        return;
    }

    // A saved connection's name means "forget it" — the only thing a name can
    // mean, since windows are addressed by their number.
    if app.store.find(arg).is_some() {
        let ids: Vec<SessionId> = app
            .sessions
            .iter()
            .filter(|s| s.profile_name == arg)
            .map(|s| s.id)
            .collect();
        for id in ids.iter().copied() {
            app.send(Cmd::Close { id });
        }
        app.store.remove(arg);
        let _ = app.store.save();
        let _ = app.store.save_secrets();
        app.notice(if ids.is_empty() {
            tf!("forgot the saved connection {}", arg)
        } else {
            tf!("closed {} window(s) and forgot {}", ids.len(), arg)
        });
        return;
    }

    app.notice(tf!("nothing called {}", arg));
}

/// Ends the task `rid`.  A window attached to it stays put and reconnects as a
/// fresh plain shell; a task with no window is just killed on the host.
fn end_task(app: &mut App, rid: &str) {
    // Immediate feedback: the task is being ended, so drop it from the list now
    // rather than waiting for the host round-trip.
    app.remote_sessions.retain(|r| r.id != rid);

    if let Some(i) = app
        .sessions
        .iter()
        .position(|s| s.remote_id.as_deref() == Some(rid))
    {
        let id = app.sessions[i].id;
        let name = app.sessions[i].name();
        // The window keeps its number, title and connection, but the old
        // terminal goes away so the fresh shell starts on a clean screen.
        app.sessions[i].reset_terminal();
        app.send(Cmd::Reset { id });
        app.notice(tf!("restarting window {} — task {} ended", name, rid));
        return;
    }

    // No window holds it: end it on the host the listing came from.
    let Some(profile) = task_profile(app, rid) else {
        app.notice(t!("no saved connection for that host"));
        return;
    };
    let secret = app.store.secret(&profile.name);
    app.send(Cmd::KillRemote {
        profile,
        secret,
        remote_id: rid.to_string(),
    });
    app.notice(tf!("killing task {}", rid));
}

/// The connection a host task belongs to: the window that holds it, else the
/// window (or saved connection) for the host the task was listed for — never a
/// different host that merely happens to be the active window.
pub(super) fn task_profile(app: &App, rid: &str) -> Option<crate::config::Profile> {
    if let Some(s) = app
        .sessions
        .iter()
        .find(|s| s.remote_id.as_deref() == Some(rid))
    {
        return Some(s.profile.clone());
    }
    let host = &app.remote_host;
    app.sessions
        .iter()
        .find(|s| &s.host == host)
        .map(|s| s.profile.clone())
        .or_else(|| {
            app.store
                .profiles()
                .iter()
                .find(|p| &p.host == host)
                .cloned()
        })
        .or_else(|| target_profile(app))
}

/// The host task `arg` names, if it names one: a listed task by its name or id,
/// or a task one of our windows is still attached to.
pub(super) fn named_task(app: &App, arg: &str) -> Option<String> {
    if let Some(r) = app.remote_sessions.iter().find(|r| r.id == arg) {
        return Some(r.id.clone());
    }
    app.sessions
        .iter()
        .find(|s| s.remote_id.as_deref() == Some(arg))
        .and_then(|s| s.remote_id.clone())
}

/// Switches language now and remembers the choice.
pub(super) fn set_language(app: &mut App, want: Option<Lang>) {
    match want {
        Some(l) => lang::set(l),
        None => lang::set(lang::detected()),
    }
    app.store.set_lang(Some(match want {
        Some(l) => l.code().to_string(),
        None => "auto".to_string(),
    }));
    let _ = app.store.save();
    // Said after the switch, so it comes out in the new language.
    app.notice(match want {
        Some(_) => tf!("language set to {}", lang::lang().name()),
        None => tf!("following the system language: {}", lang::lang().name()),
    });
}

/// `user@host[:port]`, any part of which may be left out.
pub(super) fn split_target(target: &str) -> (String, String, u16) {
    let t = target.trim();
    if t.is_empty() {
        return (String::new(), String::new(), 22);
    }
    let (user, hostport) = match t.split_once('@') {
        Some((u, h)) => (u.to_string(), h.to_string()),
        None => (String::new(), t.to_string()),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(22)),
        None => (hostport, 22),
    };
    (user, host, port)
}

/// Picks the connection to inspect: the active window's, else the selected
/// saved connection, else a window (or profile) for the host we last looked at.
///
/// The window carries its own copy of the connection, so a rename cannot leave
/// it pointing at a name that no longer exists.
pub(super) fn target_profile(app: &App) -> Option<crate::config::Profile> {
    if let Some(s) = app.active_session() {
        return Some(s.profile.clone());
    }
    let rows = app.manager_rows();
    if let Some(Row::Profile(p)) = rows.get(app.selected) {
        return Some(p.clone());
    }
    app.sessions
        .iter()
        .find(|s| s.host == app.remote_host)
        .map(|s| s.profile.clone())
        .or_else(|| {
            app.store
                .profiles()
                .iter()
                .find(|p| p.host == app.remote_host)
                .cloned()
        })
        .or_else(|| app.store.profiles().first().cloned())
}

/// Splits a command line the way a shell does: on whitespace, but keeping a
/// quoted run together, so a path with spaces stays one argument.
pub(super) fn split_args(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut started = false;
    for c in line.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            c => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(cur);
    }
    out
}

/// One command from the command bar, with its arguments already parsed.
///
/// The bar's parser and the manager's shortcut keys both build these, so a
/// command is named, parsed and carried out in exactly one place.
pub(super) enum Command {
    /// `quit` — close a window, end a task, or forget a connection.  Never the
    /// program itself; F10 is the only exit.
    Quit(Option<String>),
    Lang(LangRequest),
    New {
        name: Option<String>,
        target: Option<String>,
    },
    Edit(Option<String>),
    /// Detach a window, named by its session id (the manager's `D` key).
    Detach(Option<SessionId>),
    Cd(String),
    Ls(Option<String>),
    Download(String),
    /// `put` / `upload`: the local paths as they were typed.
    Put(Vec<String>),
    /// The exact paths a file drop handed over, ready to upload.
    Upload(Vec<PathBuf>),
    Unknown(String),
}

/// What `lang` was asked to do: set a language (or `auto`), or just report.
pub(super) enum LangRequest {
    Set(Option<Lang>),
    Report,
}

/// Parses one command line into a [`Command`], touching nothing else.
///
/// The old `tabssh:` prefix is still accepted so existing notes and scripts keep
/// working.  Every verb has exactly **one** name — no aliases and no single
/// letter forms — so Tab completion of command names is unambiguous.
pub(super) fn parse_command(line: &str) -> Command {
    let parts = split_args(line.trim());
    let raw = parts.first().map(String::as_str).unwrap_or("");
    // These commands live in the client's own command bar, never in the remote
    // shell, so they cannot collide with anything and need no prefix.
    let cmd = raw.strip_prefix("tabssh:").unwrap_or(raw);
    let rest: Vec<&str> = parts.iter().skip(1).map(String::as_str).collect();
    match cmd {
        // -- sessions on the host ------------------------------------------
        "quit" => Command::Quit(
            rest.iter()
                .find(|a| !a.starts_with("--"))
                .map(|a| a.to_string()),
        ),
        "setlang" => Command::Lang(match rest.first().copied() {
            Some("en") => LangRequest::Set(Some(Lang::En)),
            Some("zh") => LangRequest::Set(Some(Lang::Zh)),
            Some("auto") => LangRequest::Set(None),
            // No argument: say where the setting stands.
            _ => LangRequest::Report,
        }),
        "new" => match rest.len() {
            0 => Command::New {
                name: None,
                target: None,
            },
            1 => Command::New {
                name: None,
                target: Some(rest[0].to_string()),
            },
            _ => Command::New {
                name: Some(rest[0].to_string()),
                target: Some(rest[1].to_string()),
            },
        },
        "edit" => Command::Edit(rest.first().map(|a| a.to_string())),
        "cd" => Command::Cd(rest.first().copied().unwrap_or("").to_string()),
        "ls" => Command::Ls(rest.first().map(|a| a.to_string())),
        "get" => Command::Download(rest.join(" ")),
        "put" => Command::Put(rest.iter().map(|a| a.to_string()).collect()),
        _ => Command::Unknown(cmd.to_string()),
    }
}

/// Carries out one parsed command, the way the command bar used to inline.
pub(super) fn dispatch(app: &mut App, cmd: Command) {
    match cmd {
        Command::Quit(target) => run_quit(app, target.as_deref()),
        Command::Lang(LangRequest::Set(want)) => set_language(app, want),
        Command::Lang(LangRequest::Report) => {
            let state = match app.store.lang().as_deref() {
                Some("en") | Some("zh") => t!("set by you"),
                _ => t!("following the system"),
            };
            app.notice(tf!(
                "language {} ({}) — use setlang auto|en|zh",
                lang::lang().name(),
                state
            ));
        }
        Command::New { name, target } => {
            // The label is assigned for you: one letter and two digits, which
            // can never be confused with a session number.
            let name = name.unwrap_or_else(|| {
                crate::config::make_profile_name(
                    app.store.profiles().iter().map(|p| p.name.clone()),
                )
            });
            let target = target.unwrap_or_default();
            let (user, host, port) = split_target(&target);
            let mut p = crate::config::Profile::new(&name, &host, &user);
            p.port = port;
            open_editor(app, p, true);
            app.notice(t!("check the settings, then Ctrl-S to save"));
        }
        Command::Edit(name) => match name {
            Some(name) => match app.store.find(&name) {
                Some(p) => {
                    open_editor(app, p, false);
                    app.notice(t!("Ctrl-S saves, Esc discards"));
                }
                None => app.notice(tf!("no such connection: {}", name)),
            },
            None => app.notice(t!("usage: edit <name>")),
        },
        Command::Detach(target) => {
            if let Some(id) = target.or_else(|| app.active_id()) {
                app.send(Cmd::Detach { id });
            }
        }
        Command::Cd(arg) => {
            let dir = cd_target(app, &arg);
            if dir.is_dir() {
                app.cwd = dir;
                app.notice(tf!("now in {}", app.cwd.display()));
            } else {
                app.notice(tf!("no such directory: {}", dir.display()));
            }
        }
        Command::Ls(arg) => {
            // Bare `ls` means "where I am", not "home" — otherwise `cd` would
            // have no visible effect.
            let dir = match arg {
                Some(arg) => resolve_local(app, &arg),
                None => app.cwd.clone(),
            };
            app.overlay = Some(list_local(&dir));
            app.notice(format!("{} — {}", dir.display(), t!("local")));
        }
        Command::Download(remote) => {
            // A remote name is one thing too, spaces and all.
            if remote.is_empty() {
                app.notice(t!("usage: get <remote-path>"));
            } else if let Some(id) = app.active_id() {
                let dest = download_target(app);
                app.notice(tf!("downloading {} into {}…", remote, dest.display()));
                app.send(Cmd::Download { id, remote, dest });
            } else {
                app.notice(t!("open a session first"));
            }
        }
        Command::Put(args) => {
            if args.is_empty() {
                app.notice(t!(
                    "usage: put <local-path> … (or drag a file into this bar)"
                ));
                return;
            }
            // A name may contain spaces, and the user should not have to type
            // quotes for it: try the whole remainder as one name first, and
            // only fall back to separate names if that is not a thing.
            let joined = args.join(" ");
            let one = resolve_local(app, &joined);
            let paths: Vec<PathBuf> = if args.len() > 1 && one.exists() {
                vec![one]
            } else {
                args.iter().map(|arg| resolve_local(app, arg)).collect()
            };
            upload(app, paths);
        }
        Command::Upload(paths) => upload(app, paths),
        Command::Unknown(name) => app.notice(tf!("unknown command: {}", name)),
    }
}

pub(super) fn run_command(app: &mut App, line: &str) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    // Completing, listing or running a command all start here, so whatever was
    // on screen from last time goes away.
    app.overlay = None;
    let mut cmd = parse_command(line);
    // A drop leaves the exact paths it meant; take them here and clear them, so
    // only the `put` it produced can use them.  The line a drop filled in is
    // exactly what it offered, so an unchanged line uploads those paths.
    if let Some((offered, paths)) = app.pending_upload.take() {
        if offered.trim() == line && matches!(cmd, Command::Put(ref args) if !args.is_empty()) {
            cmd = Command::Upload(paths);
        }
    }
    dispatch(app, cmd);
}

pub(super) fn upload(app: &mut App, paths: Vec<PathBuf>) {
    let Some(id) = app.active_id() else {
        app.notice(t!("open a session first"));
        return;
    };
    let missing: Vec<String> = paths
        .iter()
        .filter(|p| !p.exists())
        .map(|p| p.display().to_string())
        .collect();
    if !missing.is_empty() {
        app.notice(tf!(
            "no such local path: {} — press Tab to list what is here, or cd first",
            missing.join(", ")
        ));
        return;
    }
    app.notice(tf!("uploading {} item(s)…", paths.len()));
    app.send(Cmd::Upload { id, locals: paths });
}

/// Puts `put <file> …` into the command bar and leaves it there for you to run.
///
/// Uploading on a drop was too clever: it hides what is about to happen and it
/// depends on telling a drop apart from typing, which is never entirely
/// reliable.  Showing the command costs one key press and can be read first.
pub(super) fn offer_upload(app: &mut App, paths: &[PathBuf]) {
    let mut line = String::from("put");
    for p in paths {
        line.push(' ');
        // No quotes: a name is taken as written, and the exact paths ride along
        // in `pending_upload` so spaces never need quoting.
        line.push_str(&p.display().to_string());
    }
    app.overlay = None;
    app.pending_upload = Some((line.clone(), paths.to_vec()));
    app.cmd_input = line;
    app.cmd_focus = true;
    app.notice(t!("press Enter to upload this"));
}

/// Handles a bracketed paste while the command bar has focus, or while a
/// settings field is being edited.
///
/// Returns true when the paste was consumed.  The bar is the only place a paste
/// is interpreted as a path to upload; an open field just takes the text, so a
/// path — or a whole pasted key — can be dropped straight into it.  With neither
/// open this returns false and the caller forwards the paste to the terminal
/// untouched.
pub fn handle_paste(app: &mut App, text: &str) -> bool {
    if app.cmd_focus {
        // A dropped file is offered, not fired off: you see the command, you
        // press Enter.  Nothing happens behind your back.
        let paths = paste_paths(text);
        if !paths.is_empty() {
            offer_upload(app, &paths);
        } else {
            app.cmd_input.push_str(text);
        }
        return true;
    }
    // A field that is being edited takes the text as-is — that is how a pasted
    // key, or a path to upload from, gets in.
    if let Some(ed) = app.editor.as_mut() {
        if ed.editing {
            ed.buf.push_str(text);
            return true;
        }
    }
    // The screen-session form's fields take pasted text the same way.
    if let Some(frm) = app.session_form.as_mut() {
        if frm.editing {
            frm.buf.push_str(text);
            return true;
        }
    }
    false
}

/// Pulls path candidates out of pasted or dropped text.
///
/// When the text is quoted (a Windows drop usually is) every quoted run is a
/// path, so a name with spaces — or several dropped files — survives intact.
/// With no quotes, each line is one path, which is how a dragged path arrives
/// as a burst of keystrokes.
pub(super) fn path_candidates(text: &str) -> Vec<String> {
    if text.contains('"') {
        quoted_runs(text)
    } else {
        text.lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect()
    }
}

/// The text inside each pair of double quotes.
pub(super) fn quoted_runs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut inside = false;
    for c in text.chars() {
        match c {
            '"' => {
                if inside && !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                inside = !inside;
            }
            c if inside => cur.push(c),
            _ => {}
        }
    }
    out
}

/// A drop usually arrives as one absolute path per line.
pub(super) fn paste_paths(text: &str) -> Vec<PathBuf> {
    path_candidates(text)
        .into_iter()
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .collect()
}

pub fn handle_paste_forward(app: &mut App, text: &str) {
    if let Some(id) = app.active_id() {
        if app.view == View::Terminal {
            app.send(Cmd::Input {
                id,
                data: text.as_bytes().to_vec(),
            });
        }
    }
}
