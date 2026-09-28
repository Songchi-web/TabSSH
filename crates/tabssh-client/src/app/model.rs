use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::Arc;

use russh::client::Msg as RusshMsg;
use russh::ChannelWriteHalf;
use tokio::sync::{mpsc, Mutex as AsyncMutex};

use crate::config::{Profile, Store};
use crate::ssh::Ssh;
use crate::{t, tf};

use super::cmd::{Cmd, Event, KeyAnswer};
use super::{human_age, next_session_id, now_secs, SessionId, View, NEXT_NUMBER};

/// A host-key ruling the ui is showing right now (`Event::HostKey`).  Modal:
/// it owns the keyboard until it is answered.
#[derive(Debug, Clone)]
pub struct HostKeyPrompt {
    pub id: SessionId,
    pub host: String,
    pub fingerprint: String,
    /// The saved fingerprint, when this key replaces one that changed.
    pub previous: Option<String>,
    pub answer: KeyAnswer,
}

/// Which mechanism keeps this window's shell alive on the host.
pub enum SessionKind {
    /// A GNU `screen` task on the host.  The name is shared and mutable: the
    /// manager can rename the session on the host, and every side that acts on
    /// it afterwards — reset, kill, cwd — has to see the new name.
    Screen { name: Arc<std::sync::Mutex<String>> },
    /// A plain interactive shell — no host session to reattach to.
    Plain,
}

/// Live write side of a session, plus what later commands need.
pub struct Backend {
    pub ssh: Ssh,
    pub writer: AsyncMutex<ChannelWriteHalf<RusshMsg>>,
    pub home: String,
    /// Remote directory uploads go to when the profile names one.
    pub upload_dir: Option<String>,
    /// The connection this window came from, so the host can be re-checked
    /// after the window goes away.
    pub profile: Profile,
    /// Which mechanism keeps this window's shell alive on the host.
    pub kind: SessionKind,
    /// The deployed tool path on this host, or `None` when it was not
    /// deployed.  When present it is asked for `cwd`, `list` and `monitor`;
    /// when absent those fall back to `screen` itself (or are skipped).
    pub agent_path: Option<String>,
    /// For a plain shell: the host file its login shell wrote its pid into, so
    /// `put`/`get` can read that shell's current directory out of `/proc`.  A
    /// plain shell has no `screen` task for the tool to observe, so this is how
    /// its `cd` still drives transfers.  `None` when the home directory could
    /// not be resolved.
    pub plain_pid: Option<String>,
    /// The pty size this backend was last told about, so a reset can open the
    /// replacement shell at the same size.
    cols: AtomicU16,
    rows: AtomicU16,
    /// Flipped false when this backend is being replaced (a task reset), so its
    /// read pump stops reporting and leaves the window to its successor.
    pub alive: Arc<AtomicBool>,
}

impl Backend {
    pub fn new(
        ssh: Ssh,
        writer: ChannelWriteHalf<RusshMsg>,
        profile: Profile,
        kind: SessionKind,
        home: String,
        agent_path: Option<String>,
        cols: u16,
        rows: u16,
    ) -> Backend {
        Backend {
            ssh,
            writer: AsyncMutex::new(writer),
            home,
            upload_dir: profile.upload_dir.clone(),
            profile,
            kind,
            agent_path,
            plain_pid: None,
            cols: AtomicU16::new(cols),
            rows: AtomicU16::new(rows),
            alive: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Records where a plain shell wrote its pid, so its current directory can
    /// be read later.  Only a plain shell needs this; a `screen` task is found
    /// by name through the tool.
    pub fn with_plain_pid(mut self, path: Option<String>) -> Backend {
        self.plain_pid = path;
        self
    }

    /// Remembers the pty size, so a later reset matches the window.
    pub fn set_size(&self, cols: u16, rows: u16) {
        self.cols.store(cols, Ordering::Relaxed);
        self.rows.store(rows, Ordering::Relaxed);
    }

    pub fn size(&self) -> (u16, u16) {
        (self.cols.load(Ordering::Relaxed), self.rows.load(Ordering::Relaxed))
    }

    /// True when closing the window leaves the remote session running: a screen
    /// window survives, a plain shell does not.
    pub fn keeps_alive(&self) -> bool {
        self.is_screen()
    }

    pub fn is_screen(&self) -> bool {
        matches!(self.kind, SessionKind::Screen { .. })
    }

    /// The session's name on the host, or `None` for a plain shell.  Cloned,
    /// because a rename on the host changes it under our feet.
    pub fn remote_id(&self) -> Option<String> {
        match &self.kind {
            SessionKind::Screen { name } => Some(name.lock().unwrap().clone()),
            SessionKind::Plain => None,
        }
    }

    /// Records a rename made on the host, so later commands address the session
    /// by its new name.
    pub fn set_screen_name(&self, new: &str) {
        if let SessionKind::Screen { name } = &self.kind {
            *name.lock().unwrap() = new.to_string();
        }
    }
}

/// What a Tab press is trying to complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    /// The verb itself, while it is still being typed.
    Verb,
    /// A path on the remote host (for `get`), resolved through sftp.
    Remote,
    /// A path on this machine (for `put`), starting at the desktop.
    Local,
    /// A saved connection's label.
    Profile,
    /// A window number, a host task's name/id, or a saved connection's name —
    /// everything the merged `quit` command can address.
    Any,
}

/// A list drawn just above the command bar — shell completions, or the output
/// of `ls`.  This is how the client shows "here are the possibilities" without
/// guessing which one you meant.
#[derive(Debug, Clone, Default)]
pub struct Overlay {
    pub title: String,
    /// What each row shows.
    pub items: Vec<String>,
    /// What to insert when a row is chosen; same length as `items`.
    pub values: Vec<String>,
    /// Highlighted row, when the caller lets you pick one.
    pub pick: usize,
    pub selectable: bool,
    /// The command line as it was when the list opened.  Moving the highlight
    /// previews a candidate on the line, and typing again falls back to this.
    pub base: String,
}

impl Overlay {
    pub fn completions(items: Vec<String>, values: Vec<String>, base: String) -> Overlay {
        Overlay {
            title: t!("tab completion").to_string(),
            items,
            values,
            pick: 0,
            selectable: true,
            base,
        }
    }

    pub fn listing(title: String, items: Vec<String>) -> Overlay {
        Overlay {
            title,
            values: items.clone(),
            items,
            pick: 0,
            selectable: false,
            base: String::new(),
        }
    }
}

/// A mouse drag text selection over the visible page, as `(row, col)` screen
/// cells.  Anything drawn can be selected — terminal output, the manager, help
/// — because the selection reads the composed frame, not any one view's model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    /// Where the drag started.
    pub anchor: (u16, u16),
    /// Where the mouse is now.
    pub head: (u16, u16),
}

impl Selection {
    pub fn at(row: u16, col: u16) -> Selection {
        Selection {
            anchor: (row, col),
            head: (row, col),
        }
    }

    /// The two corners in reading order, so a bottom-up drag reads the same.
    pub fn ordered(&self) -> ((u16, u16), (u16, u16)) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

/// One completion result.
#[derive(Debug, Clone)]
pub struct Completion {
    pub kind: CompletionKind,
    /// The whole word that was being completed.
    pub word: String,
    /// Full replacement words, already sorted.
    pub candidates: Vec<String>,
}

/// A session that exists on the remote host (possibly detached).
///
/// Everything after `created` comes from the tool's `monitor`, which measures
/// the session over a sampling window instead of trusting metadata.
#[derive(Debug, Clone, Default)]
pub struct RemoteSession {
    pub id: String,
    pub shell: String,
    pub pid: i64,
    pub sup_pid: i64,
    pub created: u64,
    pub age_secs: u64,
    pub last_activity_secs: u64,
    pub alive: bool,
    pub socket: bool,
    pub responsive: bool,
    pub probe: String,
    pub cpu_pct: f32,
    pub cpu_pct_max: f32,
    pub rss_kb: u64,
    pub procs: u32,
    pub verdict: String,
}

impl RemoteSession {
    /// True when the monitor actually measured it rather than just listing it.
    pub fn measured(&self) -> bool {
        !matches!(self.verdict.as_str(), "" | "unmeasured")
    }

    /// One word for the quick listing, before a monitor has measured it.
    pub fn remote_state(&self) -> String {
        if self.measured() {
            self.verdict.clone()
        } else {
            t!("not measured").into()
        }
    }

    pub fn cpu_label(&self) -> String {
        if self.cpu_pct_max > 0.5 {
            format!("{:.0}%", self.cpu_pct_max)
        } else {
            format!("{:.1}%", self.cpu_pct)
        }
    }

    pub fn mem_label(&self) -> String {
        crate::transfer::human(self.rss_kb * 1024)
    }
}

impl From<tabssh_proto::SessionRecord> for RemoteSession {
    fn from(r: tabssh_proto::SessionRecord) -> RemoteSession {
        RemoteSession {
            id: r.id,
            shell: r.shell,
            pid: r.pid,
            sup_pid: r.sup_pid,
            created: r.created,
            age_secs: r.age_secs,
            last_activity_secs: r.last_activity_secs,
            alive: r.alive,
            socket: r.socket,
            responsive: r.responsive,
            probe: r.probe,
            cpu_pct: r.cpu_pct,
            cpu_pct_max: r.cpu_pct_max,
            rss_kb: r.rss_kb,
            procs: r.procs,
            verdict: r.verdict,
        }
    }
}

impl From<tabssh_proto::SessionMeta> for RemoteSession {
    /// The cheap facts only; the measured fields stay at their defaults and the
    /// verdict marks it as not yet measured.
    fn from(m: tabssh_proto::SessionMeta) -> RemoteSession {
        RemoteSession {
            id: m.id,
            shell: m.shell,
            pid: m.pid,
            sup_pid: m.sup_pid,
            created: m.created,
            verdict: "unmeasured".into(),
            ..Default::default()
        }
    }
}

/// One open terminal window.
/// The alternate-screen enter/leave sequences a `screen` task sends.  They are
/// dropped from a task's stream so the local `vt100` keeps the whole session on
/// its main grid — the one that has a scrollback — instead of an alternate grid
/// that keeps none.
const ALT_SCREEN_SEQS: [&[u8]; 6] = [
    b"\x1b[?1049h",
    b"\x1b[?1049l",
    b"\x1b[?1047h",
    b"\x1b[?1047l",
    b"\x1b[?47h",
    b"\x1b[?47l",
];

/// Removes the alternate-screen sequences from one chunk of a task's output.
///
/// A sequence may be split across two reads, so when the chunk ends on the
/// start of one it is held back in `pending` and finished on the next call.
fn strip_alt_sequences(data: &[u8], pending: &mut Vec<u8>) -> Vec<u8> {
    let mut buf = std::mem::take(pending);
    buf.extend_from_slice(data);
    let mut out = Vec::with_capacity(buf.len());
    let mut i = 0;
    while i < buf.len() {
        if buf[i] == 0x1b {
            let rest = &buf[i..];
            if let Some(seq) = ALT_SCREEN_SEQS.iter().find(|s| rest.starts_with(s)) {
                i += seq.len();
                continue;
            }
            // A proper prefix of a sequence at the very end is incomplete: hold
            // it back rather than emitting half of it.
            if rest.len() < 8 && ALT_SCREEN_SEQS.iter().any(|s| s.starts_with(rest)) {
                pending.extend_from_slice(rest);
                return out;
            }
        }
        out.push(buf[i]);
        i += 1;
    }
    out
}

pub struct Session {
    pub id: SessionId,
    /// Stable three digit label, assigned once so a window keeps its number.
    pub number: u64,
    pub profile: Profile,
    pub title: String,
    pub profile_name: String,
    pub host: String,
    pub parser: vt100::Parser,
    pub cols: u16,
    pub rows: u16,
    pub status: String,
    pub cwd: Option<String>,
    /// Connected right now.
    pub live: bool,
    /// The tool is available on this host, so the measured session list and the
    /// current-directory tracking can work.  A host where the tool could not be
    /// deployed is false here, and the manager falls back to a plain `screen`
    /// listing.
    pub agent_ready: bool,
    /// The session's id on the host, when it has one.
    pub remote_id: Option<String>,
    pub scroll: usize,
    /// True for a window backed by a GNU `screen` task.  Screen paints over the
    /// alternate screen, which a `vt100` parser keeps no scrollback for, so the
    /// window drops those sequences and keeps the whole session — past and
    /// present — on the main grid where it can be scrolled back to.
    pub strip_alt: bool,
    /// A trailing piece of input that could be the start of an alternate-screen
    /// sequence split across two reads; held back until the next chunk.
    alt_pending: Vec<u8>,
}

impl Session {
    pub fn new(id: SessionId, profile: &Profile, cols: u16, rows: u16) -> Session {
        Session {
            id,
            // Filled in by `App::new_session`, which hands out a number that no
            // other open window is using.
            number: 0,
            profile: profile.clone(),
            title: profile.name.clone(),
            profile_name: profile.name.clone(),
            host: profile.host.clone(),
            parser: vt100::Parser::new(rows, cols, 5000),
            cols,
            rows,
            status: t!("connecting…").into(),
            cwd: None,
            live: false,
            agent_ready: false,
            remote_id: None,
            scroll: 0,
            strip_alt: false,
            alt_pending: Vec::new(),
        }
    }

    pub fn process(&mut self, data: &[u8]) {
        if self.strip_alt {
            let buf = strip_alt_sequences(data, &mut self.alt_pending);
            self.parser.process(&buf);
        } else {
            self.parser.process(data);
        }
    }

    /// Replays the task's own screen scrollback into the window so its past
    /// text can be scrolled back to.  Screen hands it over as plain lines, so
    /// newlines become the CRLF a terminal expects.
    pub fn seed_history(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let text = text.strip_suffix('\n').unwrap_or(text);
        let mut bytes = Vec::with_capacity(text.len() + text.len() / 8 + 8);
        for line in text.split('\n') {
            let line = line.strip_suffix('\r').unwrap_or(line);
            bytes.extend_from_slice(line.as_bytes());
            bytes.extend_from_slice(b"\r\n");
        }
        self.process(&bytes);
    }

    pub fn resize_vt(&mut self, cols: u16, rows: u16) {
        if cols != 0 && rows != 0 {
            self.cols = cols;
            self.rows = rows;
            self.parser.set_size(rows, cols);
        }
    }

    /// Clears the terminal and the session facts that belong to the old
    /// backend, so a window whose task was ended looks new again — the same
    /// number, title and connection, but a fresh shell.
    pub fn reset_terminal(&mut self) {
        self.parser = vt100::Parser::new(self.rows, self.cols, 5000);
        self.scroll = 0;
        self.strip_alt = false;
        self.alt_pending.clear();
        self.cwd = None;
        self.remote_id = None;
        self.agent_ready = false;
        self.live = false;
        self.status = t!("connecting…").into();
    }

    pub fn is_live(&self) -> bool {
        self.live
    }

    /// The window's own name: its stable three-digit number.  A window is
    /// addressed by this — `quit <n>`, the manager and the tab bar all agree —
    /// and the connection it came from is the description shown beside it,
    /// never its name.
    pub fn name(&self) -> String {
        format!("{:03}", self.number)
    }

    /// Tab label: a state mark, the window's name and the connection it came
    /// from as the description.  `*` while connected, `o` otherwise.
    pub fn tab(&self) -> String {
        let mark = if self.is_live() { "*" } else { "o" };
        format!("{mark} {} {}", self.name(), self.profile_name)
    }
}

pub struct App {
    pub store: Store,
    pub sessions: Vec<Session>,
    pub active: Option<usize>,
    pub view: View,
    pub cmd_input: String,
    pub cmd_focus: bool,
    pub notice: String,
    pub selected: usize,
    pub should_quit: bool,
    pub tx: mpsc::UnboundedSender<Cmd>,
    pub events: mpsc::UnboundedReceiver<Event>,
    /// Sessions discovered on the host of the active connection.
    pub remote_sessions: Vec<RemoteSession>,
    pub remote_host: String,
    /// True while a periodic host listing is in flight, so the twice-a-second
    /// refresh never piles requests up on a slow host.
    pub poll_inflight: bool,
    /// Per-connection result of the last background `screen` probe, shown on the
    /// settings form.  Runtime only — it is never written to disk; the form
    /// re-probes on open and this just keeps the last answer visible.
    pub screen_ok: BTreeMap<String, bool>,
    /// Open settings form, if any.
    pub editor: Option<crate::ui::Editor>,
    /// Open screen-session form, if any.
    pub session_form: Option<crate::ui::SessionForm>,
    /// The list drawn above the command bar, if any.
    pub overlay: Option<Overlay>,
    /// Host-key prompts waiting on a yes/no; the first is shown, the rest queue
    /// behind it (two windows can be connecting to the same new host at once).
    pub host_keys: Vec<HostKeyPrompt>,
    /// The mouse text selection over the visible page, while a drag is up.
    pub selection: Option<Selection>,
    /// Where local paths are resolved from — starts at the desktop.
    pub cwd: PathBuf,
    /// Speed and time of the last finished transfer, for the status line.
    pub last_transfer: Option<String>,
    /// True while a Tab completion is waiting for the remote side.
    pub completing: bool,
    /// Which window and command line a remote completion was asked for, so a
    /// late answer for a line the user has since edited (or a window they have
    /// left) is dropped instead of being typed into the wrong place.
    pub completion_for: Option<(SessionId, String)>,
    /// The exact `put` line a drop filled in, together with the paths it meant,
    /// so a dropped name with spaces — or several dropped files — uploads as
    /// exactly those paths when you press Enter, with no quotes on the line.
    pub pending_upload: Option<(String, Vec<PathBuf>)>,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone)]
pub enum Row {
    Profile(Profile),
    Window {
        index: usize,
        title: String,
        status: String,
    },
    Remote {
        index: usize,
        title: String,
        status: String,
    },
}

impl App {
    pub fn active_session(&self) -> Option<&Session> {
        self.active.and_then(|i| self.sessions.get(i))
    }

    pub fn active_session_mut(&mut self) -> Option<&mut Session> {
        self.active.and_then(|i| self.sessions.get_mut(i))
    }

    pub fn active_id(&self) -> Option<SessionId> {
        self.active_session().map(|s| s.id)
    }

    pub fn send(&self, c: Cmd) {
        let _ = self.tx.send(c);
    }

    pub fn session_index(&self, id: SessionId) -> Option<usize> {
        self.sessions.iter().position(|s| s.id == id)
    }

    pub fn notice(&mut self, text: impl Into<String>) {
        self.notice = text.into();
    }

    /// Creates a tab immediately, before the connection is up.
    pub fn new_session(&mut self, profile: &Profile) -> SessionId {
        let id = next_session_id();
        let number = self.next_session_number();
        let mut session = Session::new(id, profile, self.cols, self.rows);
        session.number = number;
        self.sessions.push(session);
        self.active = Some(self.sessions.len() - 1);
        id
    }

    /// A three digit number that no open window is already using.
    ///
    /// The counter is taken modulo 999 and rewrapped, so a very long lived
    /// client never grows a four digit name that `quit` could not address.  The
    /// scan skips numbers still held by a live window, so two windows never
    /// share a name even across a wrap.
    fn next_session_number(&self) -> u64 {
        for _ in 0..1000 {
            let raw = NEXT_NUMBER.fetch_add(1, Ordering::Relaxed);
            let n = (raw - 1) % 999 + 1;
            if !self.sessions.iter().any(|s| s.number == n) {
                return n;
            }
        }
        (1..=999)
            .find(|n| !self.sessions.iter().any(|s| s.number == *n))
            .unwrap_or(1)
    }

    pub fn manager_rows(&self) -> Vec<Row> {
        // Saved connections are labelled with a letter and two digits; the
        // sessions they open are numbered with three digits, so the two lists
        // can never be confused with each other.
        let mut rows: Vec<Row> = self
            .store
            .profiles()
            .iter()
            .cloned()
            .map(Row::Profile)
            .collect();

        // Windows and tasks are listed by name so a stable order survives the
        // twice-a-second refresh, and each row shows its name right before the
        // host it lives on.
        let mut windows: Vec<usize> = (0..self.sessions.len()).collect();
        windows.sort_by_key(|&i| self.sessions[i].number);
        for i in windows {
            let s = &self.sessions[i];
            rows.push(Row::Window {
                index: i,
                title: s.name(),
                // The window's number is its name; the connection it came from
                // and its host are the description here.
                status: format!("{}  {}", s.profile_name, s.host),
            });
        }

        let mut remotes: Vec<usize> = (0..self.remote_sessions.len()).collect();
        remotes.sort_by(|&a, &b| {
            self.remote_sessions[a]
                .id
                .cmp(&self.remote_sessions[b].id)
        });
        for i in remotes {
            let r = &self.remote_sessions[i];
            let age = if r.age_secs > 0 {
                human_age(r.age_secs)
            } else {
                human_age(now_secs().saturating_sub(r.created))
            };
            let measured = if r.measured() {
                tf!(
                    "{} · cpu {} · mem {} · {} · {} proc",
                    r.verdict,
                    r.cpu_label(),
                    r.mem_label(),
                    age,
                    r.procs
                )
            } else {
                format!("{} · {}", r.remote_state(), age)
            };
            // A task is addressed by its name, which comes right before the
            // host it runs on.
            let host = if self.remote_host.is_empty() {
                String::new()
            } else {
                format!("{}  ", self.remote_host)
            };
            rows.push(Row::Remote {
                index: i,
                title: r.id.clone(),
                status: format!("{host}{measured}"),
            });
        }
        rows
    }

    /// Removes a window, fixing up the active selection.  Used by `Closed`
    /// events and by the ui itself when it ends a window (a refused host key)
    /// without waiting for the controller.
    pub fn drop_session(&mut self, id: SessionId) {
        let Some(i) = self.session_index(id) else {
            return;
        };
        let was_active = self.active == Some(i);
        self.sessions.remove(i);
        self.active = if self.sessions.is_empty() {
            None
        } else if was_active {
            Some(i.min(self.sessions.len() - 1))
        } else {
            self.active.map(|a| if a > i { a - 1 } else { a })
        };
        if self.sessions.is_empty() {
            self.view = View::Manager;
        }
    }

    /// Pull whatever the controller produced, without blocking.
    pub fn drain_events(&mut self) {
        while let Ok(ev) = self.events.try_recv() {
            self.apply(ev);
        }
    }

    /// The host whose task list the manager is watching: the active live
    /// window's, else any live window's.  `None` when nothing is connected.
    fn watching_host(&self) -> Option<&str> {
        self.active_session()
            .filter(|s| s.is_live())
            .or_else(|| self.sessions.iter().find(|s| s.is_live()))
            .map(|s| s.host.as_str())
    }

    pub(crate) fn apply(&mut self, ev: Event) {
        match ev {
            Event::Opened {
                id,
                title,
                agent_ready,
                remote_id,
                fallback,
            } => {
                // The window may have been closed while it was still connecting.
                // A late event must not resurrect it, nor steal the keyboard
                // from another window.
                let Some(i) = self.session_index(id) else {
                    return;
                };
                self.sessions[i].title = title.clone();
                self.sessions[i].status = t!("connected").into();
                self.sessions[i].live = true;
                self.sessions[i].agent_ready = agent_ready;
                // A screen task paints over the alternate screen; keeping it on
                // the main grid is what gives the window a scrollback to review.
                self.sessions[i].strip_alt = remote_id.is_some();
                self.sessions[i].remote_id = remote_id;
                // Bring it to the front only if the user is still waiting on
                // it — still looking at the terminal.  A connect that finishes
                // while you are in the manager or a form must not yank you
                // away, least of all invisibly: a stolen view leaves an open
                // form eating keys it is not showing.
                if self.view == View::Terminal
                    && (self.active_id() == Some(id) || self.active.is_none())
                {
                    self.active = Some(i);
                    self.view = View::Terminal;
                }
                match fallback {
                    Some(f) => self.notice(f),
                    // No mode talk: it connected, say so.
                    None => self.notice(tf!("{} connected", title)),
                }
            }
            Event::Data { id, data } => {
                if let Some(i) = self.session_index(id) {
                    self.sessions[i].process(&data);
                }
            }
            Event::History { id, data } => {
                if let Some(i) = self.session_index(id) {
                    let text = String::from_utf8_lossy(&data).into_owned();
                    self.sessions[i].seed_history(&text);
                }
            }
            Event::Closed { id, reason } => {
                // A late `Closed` for a window the user already removed must not
                // clobber a more meaningful status line.
                let existed = self.session_index(id).is_some();
                // The window is gone, so take it out of the list instead of
                // leaving a dead row behind that cannot be closed again.
                self.drop_session(id);
                // Its host-key prompt, if one is still up or queued, goes with
                // it; dropping the answer refuses the key.
                self.host_keys.retain(|p| p.id != id);
                if existed {
                    self.notice(reason);
                }
            }
            Event::Cwd { id, cwd } => {
                if let Some(i) = self.session_index(id) {
                    self.sessions[i].cwd = Some(cwd);
                }
            }
            Event::Notice { text } => self.notice(text),
            Event::Transfer {
                text,
                bytes,
                elapsed,
            } => {
                self.last_transfer = Some(crate::transfer::rate(bytes, elapsed));
                self.notice(text);
            }
            Event::RemoteSessions { host, items, quiet } => {
                self.poll_inflight = false;
                // A refresh for a host nobody is watching would repoint the
                // manager's rows — and the selected index — at the wrong host.
                if let Some(watching) = self.watching_host() {
                    if watching != host {
                        return;
                    }
                }
                // A quick listing must not throw away numbers a monitor already
                // measured for sessions that are still there.
                let mut merged = items;
                for row in merged.iter_mut() {
                    if let Some(old) = self.remote_sessions.iter().find(|o| o.id == row.id) {
                        if old.measured() {
                            row.cpu_pct = old.cpu_pct;
                            row.cpu_pct_max = old.cpu_pct_max;
                            row.rss_kb = old.rss_kb;
                            row.procs = old.procs;
                            row.alive = old.alive;
                            row.socket = old.socket;
                            row.responsive = old.responsive;
                            row.probe = old.probe.clone();
                            row.last_activity_secs = old.last_activity_secs;
                            row.verdict = old.verdict.clone();
                        }
                    }
                }
                if !quiet {
                    self.notice(tf!(
                        "{} session(s) on {}",
                        merged.len(),
                        if host.is_empty() {
                            t!("the host")
                        } else {
                            &host
                        }
                    ));
                }
                self.remote_host = host;
                self.remote_sessions = merged;
            }
            Event::Monitor { host, rows, quiet } => {
                self.poll_inflight = false;
                // Same guard as the quick listing: another host's refresh must
                // not take over the manager's rows.
                if let Some(watching) = self.watching_host() {
                    if watching != host {
                        return;
                    }
                }
                if !quiet {
                    let busy = rows.iter().filter(|r| r.verdict == "running").count();
                    let dead = rows
                        .iter()
                        .filter(|r| matches!(r.verdict.as_str(), "dead" | "unresponsive"))
                        .count();
                    self.notice(tf!(
                        "{} session(s) on {} — {} running, {} not responding",
                        rows.len(),
                        if host.is_empty() {
                            t!("the host")
                        } else {
                            &host
                        },
                        busy,
                        dead
                    ));
                }
                self.remote_host = host;
                self.remote_sessions = rows;
            }
            Event::Completions(c) => {
                self.completing = false;
                // A late answer only applies when the line and the window it
                // was asked for are still what the user is looking at.
                let stale = match &self.completion_for {
                    Some((id, line)) => self.active_id() != Some(*id) || self.cmd_input != *line,
                    None => false,
                };
                self.completion_for = None;
                if !stale {
                    crate::ui::apply_completion(self, c);
                }
            }
            Event::HostKey {
                id,
                host,
                fingerprint,
                previous,
                answer,
            } => {
                // A window that went away mid-connect gets no ruling: dropping
                // the answer refuses its key.
                if self.session_index(id).is_none() {
                    return;
                }
                self.host_keys.push(HostKeyPrompt {
                    id,
                    host,
                    fingerprint,
                    previous,
                    answer,
                });
            }
            Event::ScreenStatus { profile, ok } => {
                // Runtime only: remember the probe result for the settings form.
                // It is deliberately not stored on the saved connection — the
                // form re-probes whenever it opens and this just keeps the last
                // answer on screen until the fresh one arrives.
                self.screen_ok.insert(profile, ok);
            }
        }
    }
}
