use std::path::PathBuf;

use tokio::sync::mpsc;

use crate::config::Profile;

use super::model::{Completion, CompletionKind, RemoteSession};
use super::SessionId;

#[derive(Debug, Clone)]
pub enum Cmd {
    Open {
        id: SessionId,
        profile: Profile,
        secret: Option<String>,
        cols: u16,
        rows: u16,
        /// True to back the window with a new persistent `screen` task; false
        /// for a plain shell.
        persistent: bool,
    },
    Input {
        id: SessionId,
        data: Vec<u8>,
    },
    Resize {
        id: SessionId,
        cols: u16,
        rows: u16,
    },
    /// Disconnect; keep the remote screen session alive.
    Detach {
        id: SessionId,
    },
    /// Close the window; a plain shell ends with it, a `screen` task lives on.
    Close {
        id: SessionId,
    },
    RefreshCwd {
        id: SessionId,
    },
    Upload {
        id: SessionId,
        locals: Vec<PathBuf>,
    },
    Download {
        id: SessionId,
        remote: String,
        /// Where the local copy lands — the F2 command bar's current directory.
        dest: PathBuf,
    },
    KillRemote {
        profile: Profile,
        secret: Option<String>,
        remote_id: String,
    },
    /// Sample the host's sessions and report what is really there.
    Monitor {
        profile: Profile,
        secret: Option<String>,
        /// A periodic refresh updates the list without stealing the status line.
        quiet: bool,
    },
    /// End the task a window is attached to, then reconnect the same window as
    /// a fresh plain shell.  The window stays; only its backend is replaced.
    Reset {
        id: SessionId,
    },
    /// Complete a path for the command bar (Tab).
    Complete {
        id: SessionId,
        kind: CompletionKind,
        word: String,
    },
    /// Lists the sessions that exist on a host (including detached ones).
    ListRemote {
        profile: Profile,
        secret: Option<String>,
        /// List GNU `screen` sessions through `screen -ls` instead of the tool.
        screen: bool,
        /// A periodic refresh updates the list without stealing the status
        /// line.
        quiet: bool,
    },
    /// Reattaches to a session that already exists on the host.
    Attach {
        id: SessionId,
        profile: Profile,
        secret: Option<String>,
        remote_id: String,
        cols: u16,
        rows: u16,
        /// The screen session's pid, so a duplicated name is disambiguated.
        pid: i64,
    },
    /// Tests, over a background connection, whether the host has GNU `screen`.
    ProbeScreen {
        profile: Profile,
        secret: Option<String>,
    },
    /// Runs GNU `screen -X` commands against a host session: rename it, set its
    /// window title, or detach its clients.  The manager sends this only over a
    /// connection that already exists.
    ScreenEdit {
        profile: Profile,
        /// The session's current name on the host.
        remote_id: String,
        ops: Vec<ScreenOp>,
    },
    Shutdown,
}

/// One change to apply to a GNU `screen` session on the host.
#[derive(Debug, Clone)]
pub enum ScreenOp {
    /// `sessionname <name>` — rename the session.
    Rename(String),
    /// `title <title>` — set the current window's title.
    Title(String),
    /// `detach` — detach every client.
    Detach,
}

#[derive(Debug, Clone)]
pub enum Event {
    Opened {
        id: SessionId,
        title: String,
        /// Whether the tool is available on this host.
        agent_ready: bool,
        remote_id: Option<String>,
        /// Set when the host could not do everything, so the user hears about
        /// it once rather than discovering it feature by feature.
        fallback: Option<String>,
    },
    Data {
        id: SessionId,
        data: Vec<u8>,
    },
    /// The task's own screen scrollback, replayed into a window before live
    /// output so its past text — including what was written while detached, or
    /// from another machine — can be scrolled back to.
    History {
        id: SessionId,
        data: Vec<u8>,
    },
    Closed {
        id: SessionId,
        reason: String,
    },
    Cwd {
        id: SessionId,
        cwd: String,
    },
    Notice {
        text: String,
    },
    /// A transfer finished, with what it moved and how long it took.
    Transfer {
        text: String,
        bytes: u64,
        elapsed: std::time::Duration,
    },
    /// Sessions found on a host by a quick listing.
    RemoteSessions {
        host: String,
        items: Vec<RemoteSession>,
        /// True for the automatic post-close refresh, which should update the
        /// list without stealing the status line.
        quiet: bool,
    },
    /// Sessions measured by a sample.
    Monitor {
        host: String,
        rows: Vec<RemoteSession>,
        /// A periodic refresh that must not steal the status line.
        quiet: bool,
    },
    /// A connection is parked on a host-key ruling: the ui shows the prompt and
    /// answers through `answer`.  Dropping it unanswered refuses the key.
    HostKey {
        id: SessionId,
        host: String,
        fingerprint: String,
        /// The saved fingerprint, when this key replaces one that changed.
        previous: Option<String>,
        answer: KeyAnswer,
    },
    /// Tab completion came back.
    Completions(Completion),
    /// The background screen probe answered: does this host have `screen`?
    ScreenStatus {
        profile: String,
        ok: bool,
    },
}

pub struct Channels {
    pub cmd: mpsc::UnboundedSender<Cmd>,
    pub events: mpsc::UnboundedReceiver<Event>,
}

/// The ui's yes/no on a host key; the connecting task is parked on the other
/// end of the oneshot until this is answered.  Dropping the last copy without
/// answering refuses the key.
#[derive(Debug, Clone)]
pub struct KeyAnswer(
    pub std::sync::Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Sender<bool>>>>,
);

impl KeyAnswer {
    pub fn new(tx: tokio::sync::oneshot::Sender<bool>) -> KeyAnswer {
        KeyAnswer(std::sync::Arc::new(std::sync::Mutex::new(Some(tx))))
    }

    pub fn answer(&self, yes: bool) {
        if let Some(tx) = self.0.lock().unwrap().take() {
            let _ = tx.send(yes);
        }
    }
}
