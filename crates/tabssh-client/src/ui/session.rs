//! The form for editing a GNU `screen` session on the host.
//!
//! The manager opens it with `e` on a persistent task, but only when a window
//! on that host is connected: every change goes out as `screen -X` on the
//! running session, so there has to be a connection to carry it.  Ctrl-S saves
//! — only then is anything applied — and the manager's task list is refreshed
//! at once rather than waiting for its next poll.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::{App, Cmd, ScreenOp, View};
use crate::{t, tf};

use super::*;

/// A GNU `screen` session being edited by the session form.
pub struct SessionForm {
    /// The session's name on the host, as it was when the form opened.
    pub remote_id: String,
    /// The connection the changes go over.
    pub profile: crate::config::Profile,
    /// The host shown in the heading.
    pub host: String,
    pub field: usize,
    pub editing: bool,
    pub buf: String,
    /// The name the session should have; starts as the current one.
    pub name: String,
    /// A new window title; blank leaves it alone.
    pub title: String,
    /// Detach every client when saving.
    pub detach: bool,
}

/// Rows in the form: name, title, detach.
const FIELDS: usize = 3;

impl SessionForm {
    pub fn new(remote_id: String, profile: crate::config::Profile, host: String) -> SessionForm {
        SessionForm {
            name: remote_id.clone(),
            remote_id,
            profile,
            host,
            field: 0,
            editing: false,
            buf: String::new(),
            title: String::new(),
            detach: false,
        }
    }
}

/// Opens the session form for a host task, if the host is connected.
pub fn open_screen_form(app: &mut App, remote_id: &str) {
    let Some(profile) = task_profile(app, remote_id) else {
        app.notice(t!("no saved connection for that host"));
        return;
    };
    if !host_is_connected(app, &profile) {
        app.notice(t!("no live connection to that host — open a window to it first"));
        return;
    }
    let host = app.remote_host.clone();
    app.session_form = Some(SessionForm::new(remote_id.to_string(), profile, host));
    app.view = View::ScreenForm;
}

/// True when a window on this very host is connected right now.
pub(super) fn host_is_connected(app: &App, profile: &crate::config::Profile) -> bool {
    app.sessions.iter().any(|s| {
        s.is_live()
            && s.profile.host == profile.host
            && s.profile.port == profile.port
            && s.profile.user == profile.user
    })
}

pub(super) fn draw_session_form(f: &mut Frame, app: &App, area: Rect) {
    let Some(frm) = &app.session_form else { return };
    let labels = [
        t!("session name"),
        t!("window title"),
        t!("detach clients"),
    ];
    let hints = [
        t!("the session's name on the host"),
        t!("the screen window's title — leave blank to keep it"),
        t!("detach every client when you save"),
    ];

    let mut lines: Vec<Line> = Vec::new();
    let mut selected_line = 0usize;
    for i in 0..FIELDS {
        let selected = i == frm.field;
        let editing = selected && frm.editing;
        let marker = if selected { "▶ " } else { "  " };
        if selected {
            selected_line = lines.len();
        }
        let value = if editing {
            format!("{}▏", frm.buf)
        } else {
            match i {
                0 => frm.name.clone(),
                1 => {
                    if frm.title.is_empty() {
                        t!("(unchanged)").to_string()
                    } else {
                        frm.title.clone()
                    }
                }
                _ => {
                    if frm.detach {
                        "[x]".to_string()
                    } else {
                        "[ ]".to_string()
                    }
                }
            }
        };
        let value_style = if editing {
            Style::default().fg(Color::Yellow)
        } else if selected {
            Style::default().fg(ACCENT)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(vec![
            Span::raw(marker),
            Span::styled(
                pad_cells(labels[i], 16),
                Style::default().fg(if selected { ACCENT } else { DIM }),
            ),
            Span::styled(value, value_style),
        ]));
        if selected {
            lines.push(Line::from(Span::styled(
                format!("    {}", hints[i]),
                Style::default().fg(DIM),
            )));
        }
    }

    // Keep the selected row (and its hint) on screen in a short terminal.
    let view_h = area.height.saturating_sub(2) as usize;
    let offset = (selected_line + 2).saturating_sub(view_h.max(1)) as u16;
    let body = Paragraph::new(lines).scroll((offset, 0)).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ACCENT))
            .title(format!(
                " {}: {} @ {} ",
                t!("screen session"),
                frm.remote_id,
                frm.host
            )),
    );
    f.render_widget(body, area);
}

pub(super) fn session_form_keys(app: &mut App, ui: &mut Ui, key: KeyEvent) {
    let mut message: Option<String> = None;

    if let Some(frm) = app.session_form.as_mut() {
        let field = frm.field;

        if frm.editing {
            // Set when Ctrl-S committed the field for a save; then the key
            // falls through to the normal Ctrl-S dispatch below.
            let mut saved = false;
            match key.code {
                KeyCode::Esc => frm.editing = false,
                KeyCode::Backspace => {
                    frm.buf.pop();
                }
                // Ctrl-S applies from inside a field too: commit it first.
                KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    let value = std::mem::take(&mut frm.buf).trim().to_string();
                    if field == 0 {
                        frm.name = value;
                    } else {
                        frm.title = value;
                    }
                    frm.editing = false;
                    saved = true;
                }
                KeyCode::Char(c) => frm.buf.push(c),
                KeyCode::Enter => {
                    let value = std::mem::take(&mut frm.buf).trim().to_string();
                    if field == 0 {
                        frm.name = value;
                    } else {
                        frm.title = value;
                    }
                    frm.editing = false;
                }
                _ => {}
            }
            if !saved {
                return;
            }
        }

        match key.code {
            KeyCode::F(10) => app.should_quit = true,
            KeyCode::Up | KeyCode::BackTab => frm.field = field.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => frm.field = (field + 1).min(FIELDS - 1),
            // Nothing is applied until Ctrl-S.
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                message = Some("\u{1}save".into());
            }
            KeyCode::Esc => message = Some("\u{1}close".into()),
            KeyCode::Char(' ') | KeyCode::Enter => match field {
                // The detach row is a checkbox.
                2 => frm.detach = !frm.detach,
                _ => {
                    frm.buf = if field == 0 {
                        frm.name.clone()
                    } else {
                        frm.title.clone()
                    };
                    frm.editing = true;
                }
            },
            _ => {}
        }
    }

    match message.as_deref() {
        Some("\u{1}save") => save_session_form(app, ui),
        Some("\u{1}close") => {
            app.session_form = None;
            app.view = View::Manager;
            app.notice(t!("settings discarded"));
        }
        Some(m) => app.notice(m),
        None => {}
    }
}

/// Turns the form into `screen -X` operations, hands them to the controller and
/// has the manager refresh at once.
pub(super) fn save_session_form(app: &mut App, ui: &mut Ui) {
    let Some(frm) = app.session_form.as_ref() else {
        return;
    };
    let remote_id = frm.remote_id.clone();
    let profile = frm.profile.clone();
    let new_name = frm.name.trim().to_string();
    let title = frm.title.trim().to_string();
    let detach = frm.detach;

    let mut ops: Vec<ScreenOp> = Vec::new();
    let rename = if !new_name.is_empty() && new_name != remote_id {
        ops.push(ScreenOp::Rename(new_name.clone()));
        Some(new_name.clone())
    } else {
        None
    };
    if !title.is_empty() {
        ops.push(ScreenOp::Title(title));
    }
    if detach {
        ops.push(ScreenOp::Detach);
    }

    app.session_form = None;
    app.view = View::Manager;

    if ops.is_empty() {
        app.notice(t!("nothing changed"));
        return;
    }

    // A window showing this session carries its own copy of the name; move it
    // now so the tab bar and the manager agree with the host.
    if let Some(new) = &rename {
        for s in app.sessions.iter_mut() {
            if s.remote_id.as_deref() == Some(remote_id.as_str()) {
                s.remote_id = Some(new.clone());
            }
        }
    }

    let shown = rename.unwrap_or_else(|| remote_id.clone());
    app.send(Cmd::ScreenEdit {
        profile,
        remote_id,
        ops,
    });
    // The change lands a moment from now; let the manager poll the instant it
    // does rather than waiting for its half-second tick.
    ui.screen_tick = None;
    app.poll_inflight = false;
    app.notice(tf!("updating session {}…", shown));
}
