use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::{App, Cmd, Row, Selection, View};
use crate::{t, tf};

use super::*;

/// Which part of the interface owns the keyboard for a key press.  The command
/// bar takes precedence, then an open form, then the view underneath.
enum Focus {
    View,
    CommandBar,
    Editor,
    ScreenForm,
}

/// The modal host-key ruling: `y` trusts and saves the key, `n`/Esc refuses.
/// Refusing also ends the window that was waiting on the key — it cannot
/// connect anyway, and the controller's own `Closed` then finds nothing to say.
fn answer_host_key(app: &mut App, key: KeyEvent) {
    let yes = matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y'));
    let no = matches!(
        key.code,
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc
    );
    if !yes && !no {
        return;
    }
    let Some(prompt) = (!app.host_keys.is_empty()).then(|| app.host_keys.remove(0)) else {
        return;
    };
    prompt.answer.answer(yes);
    if yes {
        app.notice(tf!("host key for {} saved", prompt.host));
    } else {
        app.send(Cmd::Close { id: prompt.id });
        app.drop_session(prompt.id);
        app.notice(tf!("host key for {} refused", prompt.host));
    }
}

pub fn handle_key(app: &mut App, ui: &mut Ui, key: KeyEvent) {
    // A host-key prompt is modal: it owns the keyboard until it is answered.
    if !app.host_keys.is_empty() {
        answer_host_key(app, key);
        return;
    }
    // Anything held has to go out before a non-character key does, so that
    // Enter submits the command that is still in hand rather than racing it.
    let plain_char =
        matches!(key.code, KeyCode::Char(_)) && !key.modifiers.contains(KeyModifiers::CONTROL);
    let offered = if !plain_char {
        flush_pending_now(app, ui)
    } else {
        false
    };
    // A dropped file usually arrives with a trailing newline.  That Enter fills
    // the `put` command in but must not run it: the command is left for you to
    // run, so a drop never uploads anything on its own.
    if offered && key.code == KeyCode::Enter {
        return;
    }

    let focus = if app.cmd_focus {
        Focus::CommandBar
    } else if app.editor.is_some() {
        Focus::Editor
    } else if app.session_form.is_some() {
        Focus::ScreenForm
    } else {
        Focus::View
    };
    match focus {
        // The command bar swallows keys while it has focus.
        Focus::CommandBar => {
            // While a completion list is up: it opens with the first item picked,
            // the arrows only move the highlight (previewing the choice on the
            // line), Enter accepts it, and typing again goes back to the line as it
            // was and adds the new input.
            let mut commit: Option<String> = None;
            let mut preview: Option<String> = None;
            let mut cancel_base: Option<String> = None;
            if let Some(o) = app.overlay.as_mut() {
                if o.selectable && !o.items.is_empty() {
                    match key.code {
                        KeyCode::Esc => cancel_base = Some(o.base.clone()),
                        KeyCode::Tab | KeyCode::Down => {
                            o.pick = (o.pick + 1) % o.items.len();
                            preview = o.values.get(o.pick).map(|v| replace_last_word(&o.base, v));
                        }
                        KeyCode::BackTab | KeyCode::Up => {
                            o.pick = (o.pick + o.items.len() - 1) % o.items.len();
                            preview = o.values.get(o.pick).map(|v| replace_last_word(&o.base, v));
                        }
                        KeyCode::Enter => {
                            commit = o.values.get(o.pick).map(|v| replace_last_word(&o.base, v));
                        }
                        _ => {}
                    }
                }
            }
            if let Some(base) = cancel_base {
                // Esc: drop the list and any preview it put on the line.
                app.overlay = None;
                app.cmd_input = base;
                return;
            }
            if let Some(line) = commit {
                app.cmd_input = line;
                app.overlay = None;
                return;
            }
            if let Some(line) = preview {
                app.cmd_input = line;
                return;
            }
            // Typing again: restore the line as it was when the list opened, then
            // reset the list; the new input is appended as usual below.
            if matches!(key.code, KeyCode::Char(_) | KeyCode::Backspace) {
                if let Some(o) = app.overlay.take() {
                    app.cmd_input = o.base;
                }
            }

            match key.code {
                // Ctrl+C abandons the line, the way it does at a shell prompt:
                // once the line is empty, a second press closes the bar.
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.overlay = None;
                    if app.cmd_input.is_empty() {
                        app.cmd_focus = false;
                    } else {
                        app.cmd_input.clear();
                    }
                }
                KeyCode::Esc => {
                    app.cmd_focus = false;
                    app.cmd_input.clear();
                    app.overlay = None;
                }
                KeyCode::Enter => {
                    let line = std::mem::take(&mut app.cmd_input);
                    app.cmd_focus = false;
                    run_command(app, &line);
                }
                KeyCode::Backspace => {
                    app.cmd_input.pop();
                }
                KeyCode::Tab => request_completion(app),
                // Held for an instant: a file dragged onto the bar arrives as a
                // burst of keystrokes, and the whole burst has to be seen before
                // it can be told from typing.
                KeyCode::Char(c) => {
                    ui.pending.push(c);
                    if ui.pending_at.is_none() {
                        ui.pending_at = Some(Instant::now());
                    }
                }
                _ => {}
            }
            return;
        }
        // The settings form owns the keyboard while it is open.
        Focus::Editor => {
            editor_keys(app, key);
            return;
        }
        // So does the screen-session form.
        Focus::ScreenForm => {
            session_form_keys(app, ui, key);
            return;
        }
        Focus::View => {
            // The `ls` listing floats over the view; the keys that move you
            // elsewhere must dismiss it instead of leaving it stuck on screen.
            if app.overlay.is_some() {
                match key.code {
                    KeyCode::Esc => {
                        app.overlay = None;
                        return;
                    }
                    KeyCode::F(3) | KeyCode::F(4) | KeyCode::F(9) => app.overlay = None,
                    _ => {}
                }
            }
        }
    }

    match key.code {
        KeyCode::F(1) => {
            ui.help_scroll = 0;
            app.view = View::Help;
        }
        KeyCode::F(2) => {
            app.cmd_focus = true;
            app.cmd_input.clear();
            // Opening the bar is the moment `put`/`get` may need the shell's
            // current directory, so read it now rather than trusting a stale
            // one from whenever the window was last looked at.
            if let Some(id) = app.active_id() {
                app.send(Cmd::RefreshCwd { id });
            }
        }
        KeyCode::F(3) => switch_tab(app, 1),
        KeyCode::F(4) => switch_tab(app, -1),
        KeyCode::F(5) => {
            if let Some(id) = app.active_id() {
                app.send(Cmd::RefreshCwd { id });
            }
        }
        KeyCode::F(6) => {
            app.cmd_focus = true;
            app.cmd_input = "put ".into();
            if let Some(id) = app.active_id() {
                app.send(Cmd::RefreshCwd { id });
            }
            app.notice(t!("type or drop a local path after 'put '"));
        }
        KeyCode::F(8) => {
            if let Some(id) = app.active_id() {
                app.send(Cmd::Close { id });
            }
        }
        KeyCode::F(9) => app.view = View::Manager,
        KeyCode::F(10) => app.should_quit = true,
        _ => {
            let interactive = app.view == View::Terminal;
            if interactive {
                // PageUp/PageDown scroll the local scrollback, so the shell's
                // earlier output is one key away.  The exception is a full-screen
                // program — vim, less, an editor — which owns the alternate screen
                // and along with it PageUp/PageDown; there they belong to it.
                // Shift always scrolls, whatever the remote is doing.
                let alt = app
                    .active_session()
                    .map(|s| s.parser.screen().alternate_screen())
                    .unwrap_or(false);
                let shift = key.modifiers.contains(KeyModifiers::SHIFT);
                if key.code == KeyCode::PageUp && (shift || !alt) {
                    scroll(app, 10);
                    return;
                }
                if key.code == KeyCode::PageDown && (shift || !alt) {
                    scroll(app, -10);
                    return;
                }
                // Any other key means you are done looking back: snap to the
                // live view before the key reaches the shell, so you never type
                // blind into an old frame.
                if app.active_session().map(|s| s.scroll > 0).unwrap_or(false) {
                    scroll_to_bottom(app);
                }
                // A terminal is a terminal: every key goes straight to the
                // remote shell, with no inspection and no added delay.  Path
                // capture is a command-bar feature only, so characters like
                // `/` or `.` are never mistaken for a dropped file here.
                if let Some(bytes) = key_to_bytes(&key, app) {
                    if let Some(id) = app.active_id() {
                        app.send(Cmd::Input { id, data: bytes });
                    }
                }
            } else if app.view == View::Manager {
                manager_keys(app, &key);
            } else if app.view == View::Help {
                let step = match key.code {
                    KeyCode::Down | KeyCode::Char('j') => 1,
                    KeyCode::Up | KeyCode::Char('k') => -1,
                    KeyCode::PageDown => 10,
                    KeyCode::PageUp => -10,
                    _ => 0,
                };
                if step != 0 {
                    ui.help_scroll = (ui.help_scroll as i32 + step).max(0) as u16;
                }
            }
        }
    }
}

pub(super) fn manager_keys(app: &mut App, key: &KeyEvent) {
    let rows = app.manager_rows();
    let n = rows.len();
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            app.selected = app.selected.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if n > 0 {
                app.selected = (app.selected + 1).min(n - 1);
            }
        }
        KeyCode::Enter => {
            if let Some(row) = rows.get(app.selected).cloned() {
                match row {
                    Row::Profile(p) => open_profile(app, &p, false),
                    Row::Window { index, .. } => {
                        app.active = Some(index);
                        app.view = View::Terminal;
                    }
                    Row::Remote { index, .. } => {
                        if let Some(remote_id) =
                            app.remote_sessions.get(index).map(|r| r.id.clone())
                        {
                            reattach(app, &remote_id);
                        }
                    }
                }
            }
        }
        // Tab opens a saved connection with a new persistent task behind it.
        KeyCode::Tab => {
            if let Some(Row::Profile(p)) = rows.get(app.selected) {
                let p = p.clone();
                open_profile(app, &p, true);
            }
        }
        KeyCode::Char('e') => match rows.get(app.selected) {
            Some(Row::Profile(p)) => {
                let p = p.clone();
                open_editor(app, p, false);
            }
            // On a persistent task, `e` edits the screen session behind it —
            // but only when the host is connected; open_screen_form checks.
            Some(Row::Remote { index, .. }) => {
                if let Some(rid) = app.remote_sessions.get(*index).map(|r| r.id.clone()) {
                    open_screen_form(app, &rid);
                }
            }
            _ => {}
        },
        // `q` closes the selected window (a task behind it keeps running), or
        // ends the selected host task — the same as `quit <thing>` in F2.
        KeyCode::Char('q') => {
            let target = match rows.get(app.selected) {
                Some(Row::Window { index, .. }) => {
                    app.sessions.get(*index).map(|s| format!("{:03}", s.number))
                }
                Some(Row::Remote { index, .. }) => {
                    app.remote_sessions.get(*index).map(|r| r.id.clone())
                }
                _ => None,
            };
            if let Some(arg) = target {
                dispatch(app, Command::Quit(Some(arg)));
            }
        }
        KeyCode::Char('D') => {
            if let Some(Row::Window { index, .. }) = rows.get(app.selected) {
                if let Some(id) = app.sessions.get(*index).map(|s| s.id) {
                    dispatch(app, Command::Detach(Some(id)));
                }
            }
        }
        KeyCode::Esc => app.view = View::Terminal,
        _ => {}
    }
}

pub(super) fn switch_tab(app: &mut App, delta: i32) {
    if app.sessions.is_empty() {
        return;
    }
    let n = app.sessions.len() as i32;
    let cur = app.active.unwrap_or(0) as i32;
    let next = ((cur + delta) % n + n) % n;
    app.active = Some(next as usize);
    app.view = View::Terminal;
}

pub(super) fn scroll(app: &mut App, delta: i32) {
    if let Some(s) = app.active_session_mut() {
        let n = (s.scroll as i32 + delta).max(0) as usize;
        s.parser.set_scrollback(n);
        // Read the offset back: the parser clamps it to what is actually there,
        // so `s.scroll` never drifts past the top of the scrollback.
        s.scroll = s.parser.screen().scrollback();
    }
}

/// Jumps back to the live view, the bottom of the scrollback.
pub(super) fn scroll_to_bottom(app: &mut App) {
    if let Some(s) = app.active_session_mut() {
        s.scroll = 0;
        s.parser.set_scrollback(0);
    }
}

pub(super) fn key_to_bytes(key: &KeyEvent, app: &App) -> Option<Vec<u8>> {
    let app_cursor = app
        .active_session()
        .map(|s| s.parser.screen().application_cursor())
        .unwrap_or(false);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    Some(match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                let b = (c as u32) & 0x1f;
                if b == 0 {
                    return None;
                }
                // Ctrl-2..Ctrl-8 are not representable as control bytes.
                if (c as u32) < 0x20 {
                    return None;
                }
                vec![b as u8]
            } else {
                let mut buf = [0u8; 4];
                c.encode_utf8(&mut buf).as_bytes().to_vec()
            }
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => csi(app_cursor, b'A'),
        KeyCode::Down => csi(app_cursor, b'B'),
        KeyCode::Right => csi(app_cursor, b'C'),
        KeyCode::Left => csi(app_cursor, b'D'),
        KeyCode::Home => csi(app_cursor, b'H'),
        KeyCode::End => csi(app_cursor, b'F'),
        KeyCode::PageUp => vec![0x1b, b'[', b'5', b'~'],
        KeyCode::PageDown => vec![0x1b, b'[', b'6', b'~'],
        KeyCode::Delete => vec![0x1b, b'[', b'3', b'~'],
        KeyCode::Insert => vec![0x1b, b'[', b'2', b'~'],
        KeyCode::F(n) => fkey(n)?,
        _ => return None,
    })
}

pub(super) fn csi(app_cursor: bool, ch: u8) -> Vec<u8> {
    if app_cursor {
        vec![0x1b, b'O', ch]
    } else {
        vec![0x1b, b'[', ch]
    }
}

pub(super) fn fkey(n: u8) -> Option<Vec<u8>> {
    Some(match n {
        1 => vec![0x1b, b'O', b'P'],
        2 => vec![0x1b, b'O', b'Q'],
        3 => vec![0x1b, b'O', b'R'],
        4 => vec![0x1b, b'O', b'S'],
        5 => vec![0x1b, b'[', b'1', b'5', b'~'],
        6 => vec![0x1b, b'[', b'1', b'7', b'~'],
        7 => vec![0x1b, b'[', b'1', b'8', b'~'],
        8 => vec![0x1b, b'[', b'1', b'9', b'~'],
        9 => vec![0x1b, b'[', b'2', b'0', b'~'],
        10 => vec![0x1b, b'[', b'2', b'1', b'~'],
        11 => vec![0x1b, b'[', b'2', b'3', b'~'],
        12 => vec![0x1b, b'[', b'2', b'4', b'~'],
        _ => return None,
    })
}

pub fn handle_mouse(app: &mut App, ui: &mut Ui, m: crossterm::event::MouseEvent) {
    use crossterm::event::{MouseButton, MouseEventKind};
    match m.kind {
        MouseEventKind::ScrollUp if app.view == View::Terminal => scroll(app, 3),
        MouseEventKind::ScrollDown if app.view == View::Terminal => scroll(app, -3),
        // Drag to select text anywhere on the page; releasing copies it to the
        // clipboard, the Xshell way.  A plain click (no movement) selects
        // nothing.  This is how copying works at all: the console's own
        // selection is off while the app holds mouse capture.
        MouseEventKind::Down(MouseButton::Left) => {
            app.selection = Some(Selection::at(m.row, m.column));
        }
        // Right-click pastes the clipboard where a bracketed paste would go:
        // an open command bar or form field, otherwise into the terminal.
        MouseEventKind::Down(MouseButton::Right) => paste_clipboard(app),
        MouseEventKind::Drag(MouseButton::Left) => {
            if let Some(sel) = &mut app.selection {
                sel.head = (m.row, m.column);
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            let Some(sel) = app.selection.take() else {
                return;
            };
            if sel.anchor == sel.head {
                return;
            }
            let text = ui
                .frame
                .as_ref()
                .map(|f| selected_text(f, sel))
                .unwrap_or_default();
            if text.is_empty() {
                return;
            }
            match crate::platform::copy_to_clipboard(&text) {
                Ok(()) => app.notice(tf!("copied {} chars", text.chars().count())),
                Err(e) => app.notice(tf!("could not copy: {}", e)),
            }
        }
        _ => {}
    }
}

/// Right-click paste: reads the clipboard directly (no helper process, so it
/// feels instant) and feeds it through the bracketed-paste path.  Windows
/// clipboards carry CRLF; a pty wants CR for Enter.
fn paste_clipboard(app: &mut App) {
    let text = crate::platform::clipboard_text()
        .filter(|t| !t.is_empty())
        .map(|t| t.replace("\r\n", "\r"));
    let Some(text) = text else {
        app.notice(t!("no text on the clipboard"));
        return;
    };
    if !handle_paste(app, &text) {
        handle_paste_forward(app, &text);
    }
}

/// Periodic work the ui does on its own, once per loop.
///
/// The F9 manager keeps the remote session list fresh twice a second, but only
/// while it is on screen and only for the connection currently under watch.  A
/// screen window whose host has the tool is sampled with the tool's `monitor`,
/// one short sample at a time; a screen window without the tool falls back to
/// the quick `screen -ls` listing, which already reports cpu and memory.  A
/// plain shell has nothing on the host to watch.  Every refresh is quiet: a
/// periodic poll must never take over the status line.
pub fn tick(app: &mut App, ui: &mut Ui) {
    // Only the F9 session view needs a live task list; nowhere else is polled.
    if app.view != View::Manager {
        return;
    }
    // A steady twice-a-second refresh of the host's task list, so the manager
    // always shows current data and an ended task never lingers as a ghost row.
    let due = ui
        .screen_tick
        .map(|t| t.elapsed() >= Duration::from_millis(500))
        .unwrap_or(true);
    if !due {
        return;
    }
    // Never pile requests up on a slow host: wait for the outstanding one, but
    // retry if it has been long enough that the reply is probably lost.
    if app.poll_inflight
        && ui
            .screen_tick
            .map(|t| t.elapsed() < Duration::from_secs(2))
            .unwrap_or(false)
    {
        return;
    }
    let Some(session) = watched_session(app) else {
        return;
    };
    let profile = session.profile.clone();
    let tool = session.agent_ready;
    ui.screen_tick = Some(Instant::now());
    app.poll_inflight = true;
    let secret = app.store.secret(&profile.name);
    if tool {
        app.send(Cmd::Monitor {
            profile,
            secret,
            quiet: true,
        });
    } else {
        app.send(Cmd::ListRemote {
            profile,
            secret,
            screen: true,
            quiet: true,
        });
    }
}

/// The window whose sessions the manager should keep fresh: a live screen
/// window, preferring the active one.
///
/// A plain shell — a host without `screen` — has no host session to watch, so
/// it is skipped rather than polled fruitlessly.
pub(super) fn watched_session(app: &App) -> Option<&crate::app::Session> {
    // Any live window can drive the refresh: tasks are decoupled from windows,
    // so a plain shell's connection is still used to list the host's tasks.
    if let Some(s) = app.active_session() {
        if s.is_live() {
            return Some(s);
        }
    }
    app.sessions.iter().find(|s| s.is_live())
}

/// How long a burst has to pause before it is judged.  A drag-and-drop arrives
/// with no gaps at all, so this only has to be longer than zero.
pub(super) const BURST_GAP: Duration = Duration::from_millis(30);

/// Judges a burst of characters typed into the command bar once it goes quiet.
///
/// `more_input` says whether the terminal has another event already waiting.
/// While it does, the burst is still arriving and must not be judged yet — a
/// path dragged onto the bar arrives as many keystrokes back to back.  Only the
/// bar ever holds characters, so this only runs while it has focus.
pub fn flush_pending(app: &mut App, ui: &mut Ui, more_input: bool) {
    if more_input {
        return;
    }
    let Some(at) = ui.pending_at else { return };
    if at.elapsed() < BURST_GAP {
        return;
    }
    let text = std::mem::take(&mut ui.pending);
    ui.pending_at = None;
    deliver(app, text);
}

/// Sends whatever is held, right now.
///
/// Returns true when the burst was turned into a `put` offer, so the caller can
/// keep the Enter that trails a dropped path from running it: the command is
/// filled in for *you* to run, never fired off on your behalf.
pub(super) fn flush_pending_now(app: &mut App, ui: &mut Ui) -> bool {
    if ui.pending.is_empty() {
        return false;
    }
    let text = std::mem::take(&mut ui.pending);
    ui.pending_at = None;
    deliver(app, text)
}

/// Sends a completed burst wherever it belongs.
///
/// Path capture belongs to the command bar and nowhere else, and even there a
/// burst counts as a drop only when it is *anchored* — it carries a separator, a
/// drive letter, a UNC prefix or `~`.  A lone `.`, `/` or `~` is ordinary typing
/// and is appended as text, so it can never wipe the line or start an upload on
/// its own.  With the bar closed a terminal behaves exactly like a plain
/// terminal.
pub(super) fn deliver(app: &mut App, text: String) -> bool {
    if app.cmd_focus {
        let paths = dropped_paths(&text);
        if !paths.is_empty() {
            offer_upload(app, &paths);
            true
        } else {
            app.cmd_input.push_str(&text);
            false
        }
    } else if app.view == View::Terminal {
        if !text.is_empty() {
            if let Some(id) = app.active_id() {
                app.send(Cmd::Input {
                    id,
                    data: text.into_bytes(),
                });
            }
        }
        false
    } else {
        false
    }
}

/// Paths in a burst that is anchored enough to be a dropped file rather than
/// someone typing `.`, `/` or `~`.
pub(super) fn dropped_paths(text: &str) -> Vec<PathBuf> {
    path_candidates(text)
        .into_iter()
        .filter(|s| is_anchored(s))
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .collect()
}

/// True when a path is written from a fixed starting point — a separator, a
/// drive letter, a UNC share or `~` — so a stray keystroke cannot look like one.
pub(super) fn is_anchored(s: &str) -> bool {
    s.chars().count() >= 2
        && (s.contains('/')
            || s.contains('\\')
            || s.starts_with('~')
            || has_drive_prefix(s)
            || is_unc(s))
}
