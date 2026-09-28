//! Application state and the async controller that owns all SSH work.
//!
//! The ui thread is synchronous (crossterm/ratatui); everything network bound
//! runs in a tokio runtime behind two channels: [`Cmd`] in and [`Event`] out.

mod cmd;
mod controller;
mod model;
#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub use cmd::{Channels, Cmd, ScreenOp};
// `Event`, `KeyAnswer`, `Backend` and `SessionKind` keep their old
// `crate::app::…` path even where only the crate root and tests reach for them.
#[allow(unused_imports)]
pub use cmd::{Event, KeyAnswer};
pub(crate) use controller::complete_local_in;
pub use controller::spawn_controller;
#[allow(unused_imports)]
pub use model::HostKeyPrompt;
pub use model::{App, Completion, CompletionKind, Overlay, RemoteSession, Row, Selection, Session};
#[allow(unused_imports)]
pub use model::{Backend, SessionKind};
// `auth_attempts` is reached through `crate::app::auth_attempts` by the e2e
// build and by the tests only.
#[allow(unused_imports)]
pub(crate) use controller::auth_attempts;
// The e2e build drives a plain shell the same way the controller does, so it
// reaches these helpers through the same path.
#[cfg(feature = "e2e")]
pub(crate) use controller::{
    complete_remote, expand_tilde, plain_pid_file, plain_shell_command, resolve_remote,
};

pub type SessionId = u64;

/// The terminal type the client emulates.  It has to reach the remote shell on
/// every path: full-screen programs such as vi/vim/nano look it up in terminfo
/// to decide whether to use the alternate screen and colours.
const TERM: &str = "xterm-256color";

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_NUMBER: AtomicU64 = AtomicU64::new(1);

pub fn next_session_id() -> SessionId {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// "2h 3m" style age for the session list.
pub fn human_age(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d", secs / 86_400)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Manager,
    Terminal,
    Help,
    /// The per-connection settings form.
    Editor,
    /// The GNU `screen` session form (the manager's `e` on a task).
    ScreenForm,
}
