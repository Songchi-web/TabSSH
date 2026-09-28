use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use tokio::sync::mpsc;

use crate::agent;
use crate::config::{Profile, Store};
use crate::screen;
use crate::ssh::{self, Auth, ConnectOpts, Ssh};
use crate::transfer;
use crate::{t, tf};

use super::cmd::{Channels, Cmd, Event, KeyAnswer, ScreenOp};
use super::model::{Backend, Completion, CompletionKind, RemoteSession, SessionKind};
use super::{SessionId, TERM};

// ---------------------------------------------------------------------------
// controller
// ---------------------------------------------------------------------------

type Live = Arc<tokio::sync::Mutex<BTreeMap<SessionId, Arc<Backend>>>>;

/// A window whose connection is still being set up.  It has to be cancellable,
/// otherwise closing it mid-connect does nothing and it sticks.
struct PendingOpen {
    cancel: Arc<AtomicBool>,
}

type Pending = Arc<tokio::sync::Mutex<BTreeMap<SessionId, PendingOpen>>>;

/// Starts the async controller and returns the channels the ui needs, plus the
/// controller's join handle so shutdown can wait for remote cleanup to finish.
pub fn spawn_controller(store: Store) -> (Channels, tokio::task::JoinHandle<()>) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
    let handle = tokio::spawn(controller(store, cmd_rx, ev_tx));
    (
        Channels {
            cmd: cmd_tx,
            events: ev_rx,
        },
        handle,
    )
}

async fn controller(
    store: Store,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
    tx: mpsc::UnboundedSender<Event>,
) {
    let live: Live = Arc::new(tokio::sync::Mutex::new(BTreeMap::new()));
    let pending: Pending = Arc::new(tokio::sync::Mutex::new(BTreeMap::new()));

    while let Some(cmd) = rx.recv().await {
        match cmd {
            Cmd::Shutdown => {
                // Cancel anything still connecting, so a half-open window does
                // not leave a session behind after we are gone.
                {
                    let mut p = pending.lock().await;
                    for po in p.values() {
                        po.cancel.store(true, Ordering::Relaxed);
                    }
                    p.clear();
                }
                // Quitting must not leave a plain shell running on the host;
                // only windows whose shell is kept by `screen` survive.
                let mut live = live.lock().await;
                for b in live.values() {
                    if !b.keeps_alive() {
                        kill_remote(b).await;
                    }
                }
                live.clear();
                break;
            }

            Cmd::Open {
                id,
                profile,
                secret,
                cols,
                rows,
                persistent,
            } => {
                let cancel = Arc::new(AtomicBool::new(false));
                pending.lock().await.insert(
                    id,
                    PendingOpen {
                        cancel: Arc::clone(&cancel),
                    },
                );
                let tx2 = tx.clone();
                let live2 = Arc::clone(&live);
                let pending2 = Arc::clone(&pending);
                let store2 = store.clone();
                tokio::spawn(async move {
                    let res = open_session(
                        id,
                        profile,
                        secret,
                        cols,
                        rows,
                        persistent,
                        tx2.clone(),
                        live2,
                        &store2,
                        cancel,
                    )
                    .await;
                    pending2.lock().await.remove(&id);
                    if let Err(e) = res {
                        // A window must never be left sitting on "connecting…".
                        tx2.send(Event::Closed {
                            id,
                            reason: tf!("could not connect: {}", format!("{e:#}")),
                        })
                        .ok();
                    }
                });
            }

            Cmd::Input { id, data } => {
                // Look up, drop the table lock, then write: a slow channel must
                // not hold `live` hostage for every other command.
                let b = live.lock().await.get(&id).cloned();
                if let Some(b) = b {
                    let w = b.writer.lock().await;
                    if let Err(e) = w.data_bytes(data).await {
                        tx.send(Event::Notice {
                            text: tf!("write failed: {}", e),
                        })
                        .ok();
                    }
                }
            }

            Cmd::Resize { id, cols, rows } => {
                if let Some(b) = live.lock().await.get(&id).cloned() {
                    // Remember the size so a later reset matches the window.
                    b.set_size(cols, rows);
                    // Off to one side: a screen session is attached over a pty,
                    // so the resize is just a window change on that pty.
                    tokio::spawn(async move {
                        let w = b.writer.lock().await;
                        let _ = w.window_change(cols as u32, rows as u32, 0, 0).await;
                    });
                }
            }

            Cmd::RefreshCwd { id } => {
                if let Some(b) = live.lock().await.get(&id).cloned() {
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        if let Some(cwd) = resolve_cwd(&b).await {
                            tx.send(Event::Cwd { id, cwd }).ok();
                        }
                    });
                }
            }

            Cmd::Upload { id, locals } => {
                let b = live.lock().await.get(&id).cloned();
                let tx = tx.clone();
                tokio::spawn(async move {
                    let Some(b) = b else {
                        tx.send(Event::Notice {
                            text: t!("no live session to upload to").into(),
                        })
                        .ok();
                        return;
                    };
                    // A directory configured on the connection wins; otherwise
                    // the session's current directory is used.
                    let cwd = resolve_cwd(&b).await;
                    let want = b.upload_dir.clone().or(cwd);
                    let started = std::time::Instant::now();
                    match transfer::upload(&b.ssh, &locals, want.as_deref(), &b.home).await {
                        Ok(rep) => tx
                            .send(Event::Transfer {
                                text: rep.message(),
                                bytes: rep.transfer.bytes,
                                elapsed: started.elapsed(),
                            })
                            .ok(),
                        Err(e) => tx
                            .send(Event::Notice {
                                text: tf!("upload failed: {}", format!("{e:#}")),
                            })
                            .ok(),
                    };
                });
            }

            Cmd::Download { id, remote, dest } => {
                let b = live.lock().await.get(&id).cloned();
                let tx = tx.clone();
                tokio::spawn(async move {
                    let Some(b) = b else {
                        tx.send(Event::Notice {
                            text: t!("no live session to download from").into(),
                        })
                        .ok();
                        return;
                    };
                    // A relative name means "from where the shell is": ask the
                    // shell where that is, and read it the same way `put`
                    // writes it.  sftp itself starts at the home directory, so
                    // `~` and absolute paths are handled here too.
                    let cwd = resolve_cwd(&b).await;
                    let remote = resolve_remote(&remote, cwd.as_deref(), &b.home);
                    let started = std::time::Instant::now();
                    match transfer::download(&b.ssh, &remote, &dest).await {
                        Ok((path, t)) => tx
                            .send(Event::Transfer {
                                text: tf!(
                                    "downloaded {} file(s), {} dir(s), {} -> {}",
                                    t.files,
                                    t.dirs,
                                    transfer::human(t.bytes),
                                    path.display()
                                ),
                                bytes: t.bytes,
                                elapsed: started.elapsed(),
                            })
                            .ok(),
                        Err(e) => tx
                            .send(Event::Notice {
                                text: tf!("download failed: {}", format!("{e:#}")),
                            })
                            .ok(),
                    };
                });
            }

            Cmd::KillRemote {
                profile,
                secret,
                remote_id,
            } => {
                let tx = tx.clone();
                let live = Arc::clone(&live);
                let store = store.clone();
                tokio::spawn(async move {
                    let msg = match connect(&profile, secret, &store, None).await {
                        Ok(ssh) => {
                            // `screen` is the only mechanism, so a kill is a
                            // screen kill and the whole session ends.
                            let m = match screen::kill(&ssh, &remote_id).await {
                                Ok(()) => tf!("killed screen session {}", remote_id),
                                Err(e) => tf!("kill failed: {}", format!("{e:#}")),
                            };
                            ssh.disconnect().await;
                            m
                        }
                        Err(e) => tf!("kill failed: {}", format!("{e:#}")),
                    };
                    let _ = tx.send(Event::Notice { text: msg });
                    // The task is gone; refresh the listing so it disappears
                    // from the manager instead of lingering as a ghost row.
                    refresh_host(&tx, &profile, &live, &store).await;
                });
            }

            Cmd::ScreenEdit {
                profile,
                remote_id,
                ops,
            } => {
                // Only ever sent when the manager already saw a live connection
                // to this host, so the session is worked on over that connection
                // rather than dialing afresh.
                let reuse = find_live_for_host(&live, &profile).await;
                let tx2 = tx.clone();
                let live2 = Arc::clone(&live);
                let store2 = store.clone();
                tokio::spawn(async move {
                    match edit_screen_session(reuse, &remote_id, &ops, &live2).await {
                        Ok(text) => {
                            tx2.send(Event::Notice { text }).ok();
                        }
                        Err(e) => {
                            tx2.send(Event::Notice {
                                text: tf!("could not change the session: {}", format!("{e:#}")),
                            })
                            .ok();
                        }
                    }
                    // The session may have a new name now, so refresh the
                    // manager's task list at once rather than waiting for its poll.
                    refresh_host(&tx2, &profile, &live2, &store2).await;
                });
            }

            Cmd::Reset { id } => {
                // The window keeps its number, title and connection; only its
                // backend is replaced.  Take the old one out first so nothing
                // else can act on it while the new shell comes up.
                let backend = live.lock().await.remove(&id);
                let Some(b) = backend else {
                    tx.send(Event::Closed {
                        id,
                        reason: t!("the session was not open").into(),
                    })
                    .ok();
                    continue;
                };
                // Its read pump must go quiet: the task it was reporting on is
                // about to die, and that death belongs to no window any more.
                b.alive.store(false, Ordering::Relaxed);
                let profile = b.profile.clone();
                let (cols, rows) = b.size();
                let secret = store.secret(&profile.name);
                // Registered like Open/Attach, so closing the window mid-reset
                // cancels it instead of leaving an ownerless backend behind.
                let cancel = Arc::new(AtomicBool::new(false));
                pending.lock().await.insert(
                    id,
                    PendingOpen {
                        cancel: Arc::clone(&cancel),
                    },
                );
                let tx2 = tx.clone();
                let live2 = Arc::clone(&live);
                let pending2 = Arc::clone(&pending);
                let store2 = store.clone();
                tokio::spawn(async move {
                    let res = reset_window(
                        id,
                        b,
                        profile,
                        secret,
                        cols,
                        rows,
                        tx2.clone(),
                        live2,
                        &store2,
                        cancel,
                    )
                    .await;
                    pending2.lock().await.remove(&id);
                    if let Err(e) = res {
                        let reason = tf!("could not restart the window: {}", format!("{e:#}"));
                        tx2.send(Event::Closed { id, reason }).ok();
                    }
                });
            }

            Cmd::Monitor {
                profile,
                secret,
                quiet,
            } => {
                let tx = tx.clone();
                let reuse = find_live_for_host(&live, &profile).await;
                let store = store.clone();
                tokio::spawn(async move {
                    match monitor_sessions(&profile, secret, reuse, &store).await {
                        Ok((host, rows)) => {
                            tx.send(Event::Monitor { host, rows, quiet }).ok();
                        }
                        Err(e) => {
                            // A quiet poll stays quiet: a refresh that could not
                            // reach the host must not take over the status line.
                            if !quiet {
                                tx.send(Event::Notice {
                                    text: tf!("monitor failed: {}", format!("{e:#}")),
                                })
                                .ok();
                            }
                        }
                    }
                });
            }

            Cmd::ListRemote {
                profile,
                secret,
                screen,
                quiet,
            } => {
                let tx = tx.clone();
                let reuse = find_live_for_host(&live, &profile).await;
                let store = store.clone();
                tokio::spawn(async move {
                    match list_sessions(&profile, secret, reuse, screen, &store).await {
                        Ok((host, items)) => {
                            tx.send(Event::RemoteSessions { host, items, quiet }).ok();
                        }
                        Err(e) => {
                            if !quiet {
                                tx.send(Event::Notice {
                                    text: tf!("listing sessions failed: {}", format!("{e:#}")),
                                })
                                .ok();
                            }
                        }
                    }
                });
            }

            Cmd::ProbeScreen { profile, secret } => {
                let tx = tx.clone();
                let reuse = find_live_for_host(&live, &profile).await;
                let store = store.clone();
                tokio::spawn(async move {
                    // Prefer a connection we already have; otherwise open a
                    // throw-away background connection just for the test.
                    let ok = match reuse {
                        Some(b) => Some(screen::available(&b.ssh).await),
                        None => match connect(&profile, secret, &store, None).await {
                            Ok(ssh) => {
                                let ok = screen::available(&ssh).await;
                                ssh.disconnect().await;
                                Some(ok)
                            }
                            Err(_) => None,
                        },
                    };
                    if let Some(ok) = ok {
                        tx.send(Event::ScreenStatus {
                            profile: profile.name.clone(),
                            ok,
                        })
                        .ok();
                    }
                });
            }

            Cmd::Attach {
                id,
                profile,
                secret,
                remote_id,
                cols,
                rows,
                pid,
            } => {
                // A reattach is a slow background task like any other: register
                // it so closing the window mid-flight can cancel it instead of
                // leaving a half-built window behind.
                let cancel = Arc::new(AtomicBool::new(false));
                pending.lock().await.insert(
                    id,
                    PendingOpen {
                        cancel: Arc::clone(&cancel),
                    },
                );
                let tx2 = tx.clone();
                let live2 = Arc::clone(&live);
                let pending2 = Arc::clone(&pending);
                let store2 = store.clone();
                tokio::spawn(async move {
                    let res = attach_session(
                        id,
                        profile,
                        secret,
                        remote_id,
                        cols,
                        rows,
                        pid,
                        tx2.clone(),
                        live2,
                        &store2,
                        cancel,
                    )
                    .await;
                    pending2.lock().await.remove(&id);
                    if let Err(e) = res {
                        // A failed reattach must not leave a "connecting…" window
                        // behind, so report it as a close.
                        tx2.send(Event::Closed {
                            id,
                            reason: tf!("attach failed: {}", format!("{e:#}")),
                        })
                        .ok();
                    }
                });
            }

            Cmd::Complete { id, kind, word } => {
                let tx = tx.clone();
                let b = live.lock().await.get(&id).cloned();
                tokio::spawn(async move {
                    let candidates = match kind {
                        CompletionKind::Local => complete_local(&word),
                        CompletionKind::Remote => match b {
                            Some(b) => {
                                let cwd = resolve_cwd(&b).await;
                                // `~/…` has to become the real home first: sftp
                                // will not expand it, so listing `~/` finds
                                // nothing and the picker never opens.
                                let word = expand_tilde(&word, &b.home);
                                complete_remote(&b.ssh, &word, cwd.as_deref())
                                    .await
                                    .unwrap_or_default()
                            }
                            None => Vec::new(),
                        },
                        // Names live in the ui, so they never come through here;
                        // nor does the verb list, which the ui completes itself.
                        CompletionKind::Verb
                        | CompletionKind::Profile
                        | CompletionKind::Any => Vec::new(),
                    };
                    tx.send(Event::Completions(Completion {
                        kind,
                        word,
                        candidates,
                    }))
                    .ok();
                });
            }

            Cmd::Detach { id } => {
                cancel_pending(&pending, id).await;
                let backend = live.lock().await.remove(&id);
                let keep = backend.as_ref().map(|b| b.keeps_alive()).unwrap_or(false);
                tx.send(Event::Closed {
                    id,
                    reason: if keep {
                        t!("detached — remote session still running").into()
                    } else {
                        t!("closed").into()
                    },
                })
                .ok();
                if let Some(b) = backend {
                    let live = Arc::clone(&live);
                    let tx = tx.clone();
                    let profile = b.profile.clone();
                    let store = store.clone();
                    tokio::spawn(async move {
                        if !keep {
                            kill_remote(&b).await;
                        }
                        b.ssh.disconnect().await;
                        refresh_host(&tx, &profile, &live, &store).await;
                    });
                }
            }

            Cmd::Close { id } => {
                cancel_pending(&pending, id).await;
                // Closing a window leaves a persistent (screen) task running, so
                // it can be reattached later; `end <task>` is what ends it.
                let backend = live.lock().await.remove(&id);
                let keep = backend.as_ref().map(|b| b.keeps_alive()).unwrap_or(false);
                tx.send(Event::Closed {
                    id,
                    reason: if keep {
                        t!("window closed — the session is still running").into()
                    } else {
                        t!("closed and terminated").into()
                    },
                })
                .ok();
                if let Some(b) = backend {
                    let live = Arc::clone(&live);
                    let tx = tx.clone();
                    let profile = b.profile.clone();
                    let store = store.clone();
                    tokio::spawn(async move {
                        if !keep {
                            kill_remote(&b).await;
                        }
                        b.ssh.disconnect().await;
                        refresh_host(&tx, &profile, &live, &store).await;
                    });
                }
            }
        }
    }
}

async fn kill_remote(b: &Backend) {
    // `screen` is the only mechanism, so ending a session is a screen quit.
    if let Some(name) = b.remote_id() {
        let _ = screen::kill(&b.ssh, &name).await;
    }
}

/// Ends the window's task and reconnects the window as a fresh plain shell,
/// reusing its connection while it is still good.  Cancellable: a window closed
/// mid-reset takes back whatever this had already set up.
#[allow(clippy::too_many_arguments)]
async fn reset_window(
    id: SessionId,
    old: Arc<Backend>,
    profile: Profile,
    secret: Option<String>,
    cols: u16,
    rows: u16,
    tx: mpsc::UnboundedSender<Event>,
    live: Live,
    store: &Store,
    cancel: Arc<AtomicBool>,
) -> Result<()> {
    if let Some(name) = old.remote_id() {
        let _ = screen::kill(&old.ssh, &name).await;
    }
    if cancel.load(Ordering::Relaxed) {
        old.ssh.disconnect().await;
        return Ok(());
    }
    // Reuse the connection if it is still good; dial again only when it is gone.
    let (be, rd) = match plain_shell(id, &old.ssh, &profile, cols, rows).await {
        Ok(pair) => pair,
        Err(_) => {
            old.ssh.disconnect().await;
            if cancel.load(Ordering::Relaxed) {
                return Ok(());
            }
            let ssh = connect(&profile, secret, store, None).await?;
            plain_shell(id, &ssh, &profile, cols, rows).await?
        }
    };
    live.lock().await.insert(id, Arc::clone(&be));
    // The window may have been closed in the sliver before the insert; a
    // cancelled window must not keep a backend (or its shell) behind.
    if cancel.load(Ordering::Relaxed) {
        live.lock().await.remove(&id);
        be.ssh.disconnect().await;
        return Ok(());
    }
    tx.send(Event::Opened {
        id,
        title: profile.name.clone(),
        agent_ready: be.agent_path.is_some(),
        remote_id: None,
        fallback: Some(t!("task ended — fresh shell").into()),
    })
    .ok();
    spawn_pump(rd, tx.clone(), id, Arc::clone(&live), Arc::clone(&be));
    // The task is gone; refresh the listing so its row drops.
    refresh_host(&tx, &profile, &live, store).await;
    Ok(())
}

/// Applies `screen -X` changes to a host session over a connection that already
/// exists.  Never dials: the manager only offers `e` when the host is connected.
async fn edit_screen_session(
    reuse: Option<Arc<Backend>>,
    remote_id: &str,
    ops: &[ScreenOp],
    live: &Live,
) -> Result<String> {
    let Some(b) = reuse else {
        return Err(anyhow!("{}", t!("no live connection to that host")));
    };
    let ssh = b.ssh.clone();
    let mut current = remote_id.to_string();
    for op in ops {
        match op {
            ScreenOp::Rename(new) => {
                screen::command(&ssh, &current, "sessionname", Some(new)).await?;
                // Every window showing this session carries its own copy of the
                // name; move them so a later reset or kill addresses it right.
                let backs: Vec<Arc<Backend>> = live.lock().await.values().cloned().collect();
                for bk in backs {
                    if bk.remote_id().as_deref() == Some(current.as_str()) {
                        bk.set_screen_name(new);
                    }
                }
                current = new.clone();
            }
            ScreenOp::Title(title) => {
                screen::command(&ssh, &current, "title", Some(title)).await?;
            }
            ScreenOp::Detach => {
                screen::command(&ssh, &current, "detach", None).await?;
            }
        }
    }
    Ok(tf!("session {} updated", current))
}

/// Re-lists a host after one of its windows goes away.
///
/// This is deliberately *not* the sampled `monitor`: it is a single quick
/// listing, so the manager reflects reality rather than waiting on a
/// measurement.  An already-open connection to the same host is reused.
///
/// Always awaited from a detached task: nothing about closing a window may
/// hold up the next command.
async fn refresh_host(
    tx: &mpsc::UnboundedSender<Event>,
    profile: &Profile,
    live: &Live,
    store: &Store,
) {
    let reuse = find_live_for_host(live, profile).await;
    let secret = store.secret(&profile.name).filter(|s| !s.is_empty());
    // `false` means "prefer the tool when it is there"; without one the helper
    // falls back to `screen -ls` internally.
    if let Ok((host, items)) = list_sessions(profile, secret, reuse, false, store).await {
        tx.send(Event::RemoteSessions {
            host,
            items,
            quiet: true,
        })
        .ok();
    }
}

/// An open connection to the same host, so a refresh does not need a new one.
async fn find_live_for_host(live: &Live, profile: &Profile) -> Option<Arc<Backend>> {
    live.lock()
        .await
        .values()
        .find(|b| {
            // The user matters too: two logins on one `host:port` are different
            // hosts as far as sessions are concerned.
            b.profile.host == profile.host
                && b.profile.port == profile.port
                && b.profile.user == profile.user
        })
        .cloned()
}

/// A quick "what is on this host" listing.  No sampling, no probing.
///
/// The tool's `list` is used when it is available; otherwise — and always when
/// `screen_only` is set — GNU `screen` is asked directly.  A live connection to
/// the same host is reused either way, which is what keeps the manager's
/// twice-a-second refresh cheap.
async fn list_sessions(
    profile: &Profile,
    secret: Option<String>,
    reuse: Option<Arc<Backend>>,
    screen_only: bool,
    store: &Store,
) -> Result<(String, Vec<RemoteSession>)> {
    let (ssh, tool, owned) = match reuse {
        Some(b) => {
            let tool = if screen_only { None } else { b.agent_path.clone() };
            (b.ssh.clone(), tool, None)
        }
        None => {
            let ssh = connect(profile, secret, store, None).await?;
            let tool = if screen_only {
                None
            } else {
                agent::ensure(&ssh).await.ok().map(|i| i.remote_path)
            };
            let owned = ssh.clone();
            (ssh, tool, Some(owned))
        }
    };

    let items = match tool {
        Some(tool) => {
            let out = ssh.exec(&format!("{} list", ssh::shell_quote(&tool))).await?;
            parse_remote_list(&out.stdout_str())
        }
        None => screen::list(&ssh).await?,
    };

    if let Some(ssh) = owned {
        ssh.disconnect().await;
    }
    Ok((profile.host.clone(), items))
}

/// Stops an in-flight connection for `id`, if there is one.  Returns true when
/// a connection was actually cancelled.
async fn cancel_pending(pending: &Pending, id: SessionId) -> bool {
    match pending.lock().await.remove(&id) {
        Some(p) => {
            p.cancel.store(true, Ordering::Relaxed);
            true
        }
        None => false,
    }
}

/// Splits a partial word into `(directory to list, name prefix)`.
pub(super) fn split_word(word: &str) -> (String, String) {
    match word.rfind(['/', '\\']) {
        Some(i) => (word[..=i].to_string(), word[i + 1..].to_string()),
        None => (String::new(), word.to_string()),
    }
}

/// Rewrites a leading `~` / `~/…` to `home`, the way a shell would, so a
/// completion can look inside a directory the user addressed with `~`.
///
/// sftp does not expand `~`, and on this machine `~/x` is not a real path
/// either, so both sides expand it before listing.  A bare `~` picks up a
/// trailing separator so its *contents* are listed rather than a prefix match
/// against the home directory's own name.
pub(crate) fn expand_tilde(word: &str, home: &str) -> String {
    let base = home.trim_end_matches(['/', '\\']);
    if word == "~" {
        return format!("{base}/");
    }
    match word
        .strip_prefix("~/")
        .or_else(|| word.strip_prefix(r"~\"))
    {
        Some(rest) => {
            let sep = if word.starts_with(r"~\") { '\\' } else { '/' };
            format!("{base}{sep}{rest}")
        }
        None => word.to_string(),
    }
}

/// True when a directory part names a place of its own — a leading slash, a
/// Windows drive, or a UNC share — rather than somewhere under the current
/// directory.  A path like `./` or `sub/` is *not* rooted.
pub(super) fn is_rooted(a: &str) -> bool {
    let b = a.as_bytes();
    a.starts_with(['/', '\\']) || (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
}

/// Completes a path on the remote host.
///
/// The directory part is resolved the way `get` will read it: an absolute path
/// as written, and a relative one — including a bare name — under `base`, the
/// shell's current directory (falling back to sftp's own "here", the home).
pub(crate) async fn complete_remote(
    ssh: &Ssh,
    word: &str,
    base: Option<&str>,
) -> Result<Vec<String>> {
    let sftp = ssh.sftp().await?;
    let (dir, base_name) = split_word(word);
    let base = base.filter(|b| !b.is_empty());
    let list_dir: String = if dir.is_empty() {
        base.unwrap_or(".").to_string()
    } else if dir.starts_with('/') {
        dir.clone()
    } else {
        match base {
            // Anchor a relative directory to where the shell is, so `sub/x`
            // lists the same place `get sub/x` will read.
            Some(b) => format!("{}/{}", b.trim_end_matches('/'), dir.trim_start_matches("./")),
            // Nothing to anchor to: let sftp interpret it against the home.
            None => dir.clone(),
        }
    };

    let mut out = Vec::new();
    for entry in sftp.read_dir(&list_dir).await? {
        let name = entry.file_name();
        if name == "." || name == ".." {
            continue;
        }
        if !name.starts_with(&base_name) {
            continue;
        }
        let slash = if entry.metadata().is_dir() { "/" } else { "" };
        // The candidate keeps the text the user typed as its prefix, so the
        // command line reads the way they wrote it.
        out.push(format!("{dir}{name}{slash}"));
    }
    out.sort();
    Ok(out)
}

/// Completes a path on this machine.
///
/// An empty word browses `base` (the command bar's current directory).  A
/// relative directory part — `./`, `sub/` — is read under `base` too, so the
/// listing matches where `put` will actually resolve the path; only an
/// absolute/`~` form stands on its own.
pub(crate) fn complete_local_in(base_dir: &std::path::Path, word: &str) -> Vec<String> {
    // `~` is not a real directory name: expand it to the desktop first, the
    // same place `resolve_local` sends it, or the listing finds nothing.
    let word = expand_tilde(word, &crate::config::desktop_dir().to_string_lossy());
    let (dir, name) = split_word(&word);
    let (list_dir, prefix) = if dir.is_empty() {
        let mut p = base_dir.to_string_lossy().into_owned();
        if !p.ends_with(['/', '\\']) {
            p.push(std::path::MAIN_SEPARATOR);
        }
        (base_dir.to_path_buf(), p)
    } else if is_rooted(&dir) {
        (PathBuf::from(&dir), dir.clone())
    } else {
        (base_dir.join(&dir), dir.clone())
    };

    let Ok(rd) = std::fs::read_dir(&list_dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let name_str = entry.file_name().to_string_lossy().into_owned();
        if !name_str.starts_with(&name) {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let slash = if is_dir {
            std::path::MAIN_SEPARATOR.to_string()
        } else {
            String::new()
        };
        out.push(format!("{prefix}{name_str}{slash}"));
    }
    out.sort();
    out
}

/// The desktop, which is where local browsing starts.
pub(super) fn complete_local(word: &str) -> Vec<String> {
    complete_local_in(&crate::config::desktop_dir(), word)
}

/// Samples the sessions on the host once and parses the report.
///
/// The tool's `monitor` is used when it is available; without one it falls back
/// to the plain `screen` listing, which already reports cpu and memory.  A live
/// connection to the same host is reused when one exists, so the manager's
/// twice-a-second refresh never dials the host again; a throwaway connection is
/// opened only when there is none.
async fn monitor_sessions(
    profile: &Profile,
    secret: Option<String>,
    reuse: Option<Arc<Backend>>,
    store: &Store,
) -> Result<(String, Vec<RemoteSession>)> {
    let (ssh, tool, owned) = match reuse {
        Some(b) => (b.ssh.clone(), b.agent_path.clone(), None),
        None => {
            let ssh = connect(profile, secret, store, None).await?;
            let tool = agent::ensure(&ssh).await.ok().map(|i| i.remote_path);
            let owned = ssh.clone();
            (ssh, tool, Some(owned))
        }
    };

    let rows = match tool {
        Some(tool) => {
            // Two samples are needed: the tool computes cpu% from the *delta*
            // between samples, so a single sample can only ever read 0.0%.
            let cmd = format!(
                "{} monitor --samples 2 --interval-ms 250",
                ssh::shell_quote(&tool)
            );
            let out = ssh.exec(&cmd).await?;
            parse_monitor(&out.stdout_str())
        }
        None => screen::list(&ssh).await?,
    };
    if let Some(ssh) = owned {
        ssh.disconnect().await;
    }
    Ok((profile.host.clone(), rows))
}

/// Parses the one-JSON-object-per-line report from the tool's `monitor`.
pub(super) fn parse_monitor(out: &str) -> Vec<RemoteSession> {
    out.lines()
        .filter_map(|l| serde_json::from_str::<tabssh_proto::SessionRecord>(l.trim()).ok())
        .map(RemoteSession::from)
        .collect()
}

/// Reattaches to a screen session that already exists on the host.
#[allow(clippy::too_many_arguments)]
async fn attach_session(
    id: SessionId,
    profile: Profile,
    secret: Option<String>,
    remote_id: String,
    cols: u16,
    rows: u16,
    pid: i64,
    tx: mpsc::UnboundedSender<Event>,
    live: Live,
    store: &Store,
    cancel: Arc<AtomicBool>,
) -> Result<()> {
    let ssh = connect(&profile, secret, store, Some(ui_ask(&tx, id))).await?;
    if cancel.load(Ordering::Relaxed) {
        ssh.disconnect().await;
        return Ok(());
    }

    // Attaching to a screen session needs a pty; screen keeps it alive.
    let home = agent::remote_home(&ssh).await.unwrap_or_else(|_| "~".into());
    // Give the task a large scrollback if it does not have one yet, so both this
    // reattach and the next one have more history to show.  Best-effort.
    let _ = screen::set_scrollback(&ssh, &remote_id, screen::HISTORY_LINES).await;
    // Pull the task's own scrollback before we attach.  This is the past text a
    // reattach is for — including what was written while no client was here, or
    // from another machine — which the live pty alone would never show.
    let history = screen_history(&ssh, &remote_id, &home).await;
    let (rd, wr) = ssh
        .open_pty_exec(&screen::attach_command(&remote_id, pid), TERM, cols, rows, false)
        .await?;
    if cancel.load(Ordering::Relaxed) {
        ssh.disconnect().await;
        return Ok(());
    }
    // Best-effort: deploy the tool so cwd/list/monitor work on this window.  If
    // it cannot be had, the window keeps working through `screen` alone.
    let agent_path = agent::ensure(&ssh).await.ok().map(|i| i.remote_path);
    let be = Arc::new(Backend::new(
        ssh.clone(),
        wr,
        profile.clone(),
        SessionKind::Screen {
            name: Arc::new(std::sync::Mutex::new(remote_id.clone())),
        },
        home,
        agent_path,
        cols,
        rows,
    ));
    live.lock().await.insert(id, Arc::clone(&be));
    // Opened first, then the history, then live data: the window has to know it
    // is a screen task (so it keeps the whole session on the main grid) before
    // anything is parsed into it.
    tx.send(Event::Opened {
        id,
        title: profile.name.clone(),
        agent_ready: be.agent_path.is_some(),
        remote_id: Some(remote_id.clone()),
        fallback: Some(tf!("reattached to {}", remote_id)),
    })
    .ok();
    if let Some(data) = history {
        tx.send(Event::History { id, data }).ok();
    }
    spawn_pump(rd, tx.clone(), id, Arc::clone(&live), Arc::clone(&be));
    Ok(())
}

/// Pulls a GNU `screen` task's own scrollback off the host, so its past text can
/// be replayed into a window.
///
/// `screen -X hardcopy -h` writes the current window's buffer (scrollback and
/// all) to a file; the client reads it back and deletes it.  Best-effort — no
/// buffer, no `screen`, or a failed copy all simply mean "no history".
async fn screen_history(ssh: &Ssh, name: &str, home: &str) -> Option<Vec<u8>> {
    if home == "~" {
        return None;
    }
    let dir = format!("{}/.tabssh", home.trim_end_matches('/'));
    let path = format!("{dir}/hc.{name}");
    let _ = ssh
        .exec(&format!("mkdir -p {}", ssh::shell_quote(&dir)))
        .await;
    screen::hardcopy(ssh, name, &path).await.ok()?;
    let text = ssh
        .exec(&format!("cat {}", ssh::shell_quote(&path)))
        .await
        .ok()?
        .stdout;
    let _ = ssh
        .exec(&format!("rm -f {}", ssh::shell_quote(&path)))
        .await;
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Parses the JSON lines the tool's `list` prints.  These carry only the cheap
/// facts; `monitor` fills in the measured ones.
pub(super) fn parse_remote_list(out: &str) -> Vec<RemoteSession> {
    out.lines()
        .filter_map(|l| serde_json::from_str::<tabssh_proto::SessionMeta>(l.trim()).ok())
        .map(RemoteSession::from)
        .collect()
}

/// Where the session's shell currently is.
///
/// A `screen` window is observed by the host tool through `/proc`; a plain
/// shell has no task to observe, so it records its own pid and we read that
/// shell's `/proc/<pid>/cwd`.  Either way the answer is the live directory of
/// the shell the user is typing into, which is what transfers should follow.
async fn resolve_cwd(b: &Backend) -> Option<String> {
    match b.kind {
        SessionKind::Screen { .. } => agent_cwd(b).await,
        SessionKind::Plain => plain_cwd(b).await,
    }
}

/// Asks the tool where the `screen` task's shell currently is.  Without the
/// tool the current directory simply cannot be tracked, so this returns `None`.
async fn agent_cwd(b: &Backend) -> Option<String> {
    let tool = b.agent_path.as_deref()?;
    let rid = b.remote_id()?;
    let c = format!(
        "{} cwd --id {}",
        ssh::shell_quote(tool),
        ssh::shell_quote(&rid)
    );
    let out = b.ssh.exec(&c).await.ok()?;
    let s = out.stdout_str().trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Reads a plain shell's current directory from the pid it wrote down.
async fn plain_cwd(b: &Backend) -> Option<String> {
    let pid_file = b.plain_pid.as_deref()?;
    let out = b
        .ssh
        .exec(&format!("cat {} 2>/dev/null", ssh::shell_quote(pid_file)))
        .await
        .ok()?;
    let pid = out.stdout_str().trim().to_string();
    if pid.is_empty() || !pid.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let out = b
        .ssh
        .exec(&format!("readlink /proc/{pid}/cwd"))
        .await
        .ok()?;
    let s = out.stdout_str().trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Turns a remote path the user typed into one sftp can open.
///
/// `~` and `~/x` mean the login's home; an absolute path is taken as written;
/// anything else is relative to `cwd` — the shell's own directory when we could
/// learn it, and the home directory when we could not (sftp's own idea of
/// "here").
pub(crate) fn resolve_remote(path: &str, cwd: Option<&str>, home: &str) -> String {
    let p = path.trim();
    if p.is_empty() {
        return p.to_string();
    }
    if let Some(rest) = p.strip_prefix("~/") {
        return format!("{}/{}", home.trim_end_matches('/'), rest);
    }
    if p == "~" {
        return home.to_string();
    }
    if p.starts_with('/') {
        return p.to_string();
    }
    let base = cwd.filter(|c| !c.is_empty()).unwrap_or(home);
    if base.is_empty() || base == "~" {
        // Nothing to anchor to: leave it for sftp, which starts at home.
        return p.to_string();
    }
    format!("{}/{}", base.trim_end_matches('/'), p)
}

/// Everything the client knows how to try, in the order it should be tried.
///
/// The user is never asked which one to use: an explicit key file wins, then
/// the usual keys and ssh-agent, then the stored secret, and finally an
/// unauthenticated attempt.
pub(crate) fn auth_attempts(profile: &Profile, secret: Option<String>) -> Vec<Auth> {
    let mut out = Vec::new();

    // Whatever the user configured comes first: the key they gave (a path or
    // the key itself) and the password they gave.
    if let Some(key) = profile.key_source() {
        if profile.key_is_inline() {
            out.push(Auth::KeyData {
                data: key.to_string(),
                passphrase: secret.clone(),
            });
        } else {
            out.push(Auth::Key {
                path: PathBuf::from(key),
                passphrase: secret.clone(),
            });
        }
    }
    if let Some(secret) = secret.filter(|s| !s.is_empty()) {
        out.push(Auth::Password(secret));
    }

    // Then anything already on this machine, and finally no auth at all.
    out.push(Auth::DefaultKeys);
    out.push(Auth::None);
    out
}

/// Parks a connection attempt on the ui's yes/no for an unseen or changed host
/// key.  No answer coming back — the ui is gone, or the prompt was dropped —
/// is a no.
fn ui_ask(tx: &mpsc::UnboundedSender<Event>, id: SessionId) -> ssh::AskKey {
    let tx = tx.clone();
    Arc::new(move |check: ssh::KeyCheck| {
        let (answer, wait) = tokio::sync::oneshot::channel();
        tx.send(Event::HostKey {
            id,
            host: check.host,
            fingerprint: check.fingerprint,
            previous: check.previous,
            answer: KeyAnswer::new(answer),
        })
        .ok();
        Box::pin(async move { wait.await.unwrap_or(false) })
    })
}

async fn connect(
    profile: &Profile,
    secret: Option<String>,
    store: &Store,
    ask: Option<ssh::AskKey>,
) -> Result<Ssh> {
    // Only an interactive open may save a key: the user ruled on it.  A
    // background connection (probe/list/monitor) trusts a first key in memory
    // but never persists it, so a silent poll cannot save a key nobody
    // confirmed — nor can it beat the interactive prompt to the save.
    let on_new_key = ask.as_ref().map(|_| {
        let store = store.clone();
        Arc::new(move |host: &str, fp: &str| {
            store.remember_host(host.to_string(), fp.to_string());
            let _ = store.save();
        }) as ssh::OnNewKey
    });
    let opts = ConnectOpts {
        host: profile.host.clone(),
        port: profile.port,
        user: profile.user.clone(),
        auths: auth_attempts(profile, secret),
        known: Arc::new(std::sync::Mutex::new(store.known_hosts())),
        on_new_key,
        ask,
    };
    Ssh::connect(opts).await
}

#[allow(clippy::too_many_arguments)]
async fn open_session(
    id: SessionId,
    profile: Profile,
    secret: Option<String>,
    cols: u16,
    rows: u16,
    persistent: bool,
    tx: mpsc::UnboundedSender<Event>,
    live: Live,
    store: &Store,
    cancel: Arc<AtomicBool>,
) -> Result<()> {
    let ssh = connect(&profile, secret, store, Some(ui_ask(&tx, id))).await?;
    if cancel.load(Ordering::Relaxed) {
        ssh.disconnect().await;
        return Ok(());
    }

    // A persistent window runs inside a named `screen` task the F9 manager can
    // list and reattach to; a plain window is just a shell.  Persistence is
    // asked for explicitly (the manager's Tab key) — it is never guessed.
    let (fallback, backend, rd) = if persistent {
        if screen::available(&ssh).await {
            match start_screen_session(&ssh, &profile, cols, rows).await {
                Ok((be, rd)) => {
                    // Say plainly that the task lives on the server, so a
                    // dropped connection is understood not to touch it.
                    let name = be.remote_id().unwrap_or_default();
                    (
                        Some(tf!(
                            "server screen task {} is running — a dropped connection does not stop it",
                            name
                        )),
                        be,
                        rd,
                    )
                }
                Err(e) => {
                    let (be, rd) = plain_shell(id, &ssh, &profile, cols, rows).await?;
                    (
                        Some(tf!(
                            "screen failed ({}); using a plain session",
                            format!("{e:#}")
                        )),
                        be,
                        rd,
                    )
                }
            }
        } else {
            let (be, rd) = plain_shell(id, &ssh, &profile, cols, rows).await?;
            (
                Some(t!("screen was not found on the host — using a plain session").to_string()),
                be,
                rd,
            )
        }
    } else {
        let (be, rd) = plain_shell(id, &ssh, &profile, cols, rows).await?;
        (None, be, rd)
    };

    if cancel.load(Ordering::Relaxed) {
        // The window was closed while we were still connecting: take back
        // whatever we created on the host and leave no trace.
        if backend.remote_id().is_some() {
            kill_remote(&backend).await;
        }
        ssh.disconnect().await;
        return Ok(());
    }

    live.lock().await.insert(id, Arc::clone(&backend));
    // Re-check after the insert: the window may have been closed in the sliver
    // between the check above and here, and a cancelled window must not leave a
    // backend (or a session) behind.
    if cancel.load(Ordering::Relaxed) {
        live.lock().await.remove(&id);
        if backend.remote_id().is_some() {
            kill_remote(&backend).await;
        }
        ssh.disconnect().await;
        return Ok(());
    }
    // Opened before the pump: the window learns what it is before any output is
    // parsed into it (a screen task must be kept on the main grid from byte one).
    tx.send(Event::Opened {
        id,
        title: profile.name.clone(),
        // The window reports whether the tool is in place; that drives the
        // measured list and the current-directory tracking.
        agent_ready: backend.agent_path.is_some(),
        remote_id: backend.remote_id(),
        fallback,
    })
    .ok();
    spawn_pump(rd, tx.clone(), id, Arc::clone(&live), Arc::clone(&backend));
    Ok(())
}

/// Starts a GNU `screen` session on the host and attaches to it over a pty.
async fn start_screen_session(
    ssh: &Ssh,
    profile: &Profile,
    cols: u16,
    rows: u16,
) -> Result<(Arc<Backend>, russh::ChannelReadHalf)> {
    // One window, one task: the name is unique per window, so opening a second
    // window on the same connection starts a *separate* shell instead of
    // stealing the first window's (a shared name made `screen -d -r` detach the
    // other window and `screen -dmS` pile up duplicate sessions).
    let name = screen::task_name();
    screen::start(ssh, &name).await?;
    // Give the new task a large scrollback, so a later reattach has more of its
    // past to replay.  Best-effort: a host that refuses keeps its default.
    let _ = screen::set_scrollback(ssh, &name, screen::HISTORY_LINES).await;
    let pid = screen::find(ssh, &name).await.map(|s| s.pid).unwrap_or(0);

    let (rd, wr) = match ssh
        .open_pty_exec(&screen::attach_command(&name, pid), TERM, cols, rows, false)
        .await
    {
        Ok(pair) => pair,
        Err(e) => {
            // The task exists but could not be attached: kill it, or it sits on
            // the host forever belonging to no window.
            let _ = screen::kill(ssh, &name).await;
            return Err(e);
        }
    };
    let home = agent::remote_home(ssh).await.unwrap_or_else(|_| "~".into());
    // Best-effort: put the stateless tool on the host so cwd/list/monitor can be
    // asked of it.  If that fails the session still works through screen alone.
    let agent_path = agent::ensure(ssh).await.ok().map(|i| i.remote_path);
    let be = Arc::new(Backend::new(
        ssh.clone(),
        wr,
        profile.clone(),
        SessionKind::Screen {
            name: Arc::new(std::sync::Mutex::new(name)),
        },
        home,
        agent_path,
        cols,
        rows,
    ));
    Ok((be, rd))
}

async fn plain_shell(
    id: SessionId,
    ssh: &Ssh,
    profile: &Profile,
    cols: u16,
    rows: u16,
) -> Result<(Arc<Backend>, russh::ChannelReadHalf)> {
    let home = agent::remote_home(ssh).await.unwrap_or_else(|_| "~".into());
    // A plain shell has no `screen` task for the host tool to observe, so
    // nothing would remember where it has `cd`-ed standing.  It is started
    // through a tiny wrapper that writes down its own pid once; the client then
    // reads its current directory out of `/proc/<pid>/cwd`, so `put`/`get`
    // follow the shell wherever it goes.
    let pid_file = plain_pid_file(&home, id);
    let cmd = match &pid_file {
        Some(f) => plain_shell_command(f),
        None => "\"${SHELL:-/bin/sh}\" -l".to_string(),
    };
    let (rd, wr) = ssh
        .open_pty_exec(&cmd, TERM, cols, rows, true)
        .await
        .map_err(|e| anyhow!("{}", tf!("open shell: {}", e)))?;
    let be = Arc::new(
        Backend::new(
            ssh.clone(),
            wr,
            profile.clone(),
            SessionKind::Plain,
            home,
            None,
            cols,
            rows,
        )
        .with_plain_pid(pid_file),
    );
    Ok((be, rd))
}

/// Where a plain shell writes its pid, or `None` when the home directory is
/// unknown and the file could not be placed sensibly.
pub(crate) fn plain_pid_file(home: &str, id: SessionId) -> Option<String> {
    if home.is_empty() || home == "~" {
        return None;
    }
    Some(format!("{}/.tabssh/plain.{id}", home.trim_end_matches('/')))
}

/// The command that starts a plain login shell through a pid-recording wrapper.
///
/// `exec` keeps the same pid, so the number written down is the shell's own and
/// stays valid for the window's whole life.  The steps are separated by `;`
/// rather than `&&` so a failed `mkdir` still leaves a usable shell behind.
pub(crate) fn plain_shell_command(pid_file: &str) -> String {
    let dir = pid_file.rsplit_once('/').map(|(d, _)| d).unwrap_or(".");
    format!(
        "mkdir -p {d}; echo $$ > {f}; exec \"${{SHELL:-/bin/sh}}\" -l",
        d = ssh::shell_quote(dir),
        f = ssh::shell_quote(pid_file),
    )
}

fn spawn_pump(
    mut rd: russh::ChannelReadHalf,
    tx: mpsc::UnboundedSender<Event>,
    id: SessionId,
    live: Live,
    me: Arc<Backend>,
) -> tokio::task::JoinHandle<()> {
    let alive = Arc::clone(&me.alive);
    tokio::spawn(async move {
        let mut reported = false;
        while let Some(msg) = rd.wait().await {
            // A reset replaces this backend; its successor owns the window, so
            // this pump says nothing more — not the task's dying gasps, not a
            // close for a window that is still open.
            if !alive.load(Ordering::Relaxed) {
                return;
            }
            match msg {
                russh::ChannelMsg::Data { data } => {
                    if tx
                        .send(Event::Data {
                            id,
                            data: data.to_vec(),
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                russh::ChannelMsg::ExtendedData { data, .. } => {
                    if tx
                        .send(Event::Data {
                            id,
                            data: data.to_vec(),
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                russh::ChannelMsg::ExitStatus { exit_status } => {
                    reported = true;
                    let _ = tx.send(Event::Closed {
                        id,
                        reason: tf!("remote shell exited ({})", exit_status),
                    });
                    // The shell is gone; waiting for the channel's close would
                    // keep this dead backend reusable for who knows how long.
                    break;
                }
                russh::ChannelMsg::Close | russh::ChannelMsg::Eof => {
                    if !reported {
                        let _ = tx.send(Event::Closed {
                            id,
                            reason: t!("connection closed").into(),
                        });
                        reported = true;
                    }
                    break;
                }
                _ => {}
            }
        }
        // Report only if this pump is still the window's backend; a late EOF
        // for a window that was already replaced or closed must stay silent.
        let mut guard = live.lock().await;
        let current = guard.get(&id).map(|b| Arc::ptr_eq(b, &me)).unwrap_or(false);
        if !current {
            return;
        }
        guard.remove(&id);
        drop(guard);
        if !reported {
            let _ = tx.send(Event::Closed {
                id,
                reason: t!("session ended").into(),
            });
        }
    })
}
