use std::sync::OnceLock;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::{App, View};
use crate::{t, tf};

use super::*;

// ---------------------------------------------------------------------------
// per connection settings form
// ---------------------------------------------------------------------------

/// A profile being edited by the settings form.
pub struct Editor {
    pub profile: crate::config::Profile,
    pub password: String,
    pub field: usize,
    pub editing: bool,
    pub buf: String,
    /// The name the profile had when the form opened, so a rename can move it.
    pub original: Option<String>,
    pub is_new: bool,
}

impl Editor {
    pub fn new(profile: crate::config::Profile, password: String, is_new: bool) -> Editor {
        let original = if is_new {
            None
        } else {
            Some(profile.name.clone())
        };
        Editor {
            profile,
            password,
            field: 0,
            editing: false,
            buf: String::new(),
            original,
            is_new,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum FieldKind {
    Text,
    Password,
    /// Read-only: a value the client fills in, not something you type.
    Status,
    /// Read-only reference text; pressing Enter copies it to the clipboard.
    Copy,
}

pub(super) struct FieldDef {
    pub(super) section: &'static str,
    pub(super) label: &'static str,
    pub(super) kind: FieldKind,
    pub(super) hint: &'static str,
}

pub(super) fn fields() -> &'static [FieldDef] {
    static FIELDS: OnceLock<Vec<FieldDef>> = OnceLock::new();
    FIELDS.get_or_init(|| {
        let mut v = vec![
            FieldDef {
                section: t!("network"),
                label: t!("name"),
                kind: FieldKind::Text,
                hint: t!("label in the session list"),
            },
            FieldDef {
                section: t!("network"),
                label: t!("host"),
                kind: FieldKind::Text,
                hint: t!("hostname or ip"),
            },
            FieldDef {
                section: t!("network"),
                label: t!("port"),
                kind: FieldKind::Text,
                hint: t!("default 22"),
            },
            FieldDef {
                section: t!("network"),
                label: t!("user"),
                kind: FieldKind::Text,
                hint: "",
            },
            FieldDef {
                section: t!("connection"),
                label: t!("key"),
                kind: FieldKind::Text,
                hint: t!("a path, or paste the key itself — optional"),
            },
            FieldDef {
                section: t!("connection"),
                label: t!("password"),
                kind: FieldKind::Password,
                hint: t!("used automatically if the host asks for one"),
            },
            FieldDef {
                section: t!("transfer"),
                label: t!("upload dir"),
                kind: FieldKind::Text,
                hint: t!("remote path — blank = session's current directory, then home"),
            },
            FieldDef {
                section: t!("transfer"),
                label: t!("download dir"),
                kind: FieldKind::Text,
                hint: t!("local path — blank = the command bar's directory (starts at the desktop)"),
            },
            FieldDef {
                section: t!("behaviour"),
                label: t!("screen status"),
                kind: FieldKind::Status,
                hint: t!("tested over a background connection when you connect"),
            },
            FieldDef {
                section: t!("behaviour"),
                label: t!("note"),
                kind: FieldKind::Text,
                hint: t!("your own note — saved with the connection"),
            },
        ];
        // The install commands are their own section: one row per distribution,
        // and pressing Enter on a row copies that command to the clipboard.
        for (os, _) in crate::screen::install_commands() {
            v.push(FieldDef {
                section: t!("install screen"),
                label: os,
                kind: FieldKind::Copy,
                hint: t!("press Enter to copy this command"),
            });
        }
        v
    })
}

/// How many fields come before the copyable install commands.
pub(super) fn fixed_field_count() -> usize {
    fields().len() - crate::screen::install_commands().len()
}

/// The clipboard text a copy-field (index in `fields()`) stands for.
pub(super) fn field_command(i: usize) -> Option<&'static str> {
    let n = i.checked_sub(fixed_field_count())?;
    crate::screen::install_commands().get(n).map(|(_, c)| *c)
}

pub(super) fn opt(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

pub(super) fn field_value(p: &crate::config::Profile, i: usize) -> String {
    match i {
        0 => p.name.clone(),
        1 => p.host.clone(),
        2 => p.port.to_string(),
        3 => p.user.clone(),
        4 => p.key_source().unwrap_or_default().to_string(),
        5 => String::new(), // the password is shown separately
        6 => p.upload_dir.clone().unwrap_or_default(),
        7 => p.download_dir.clone().unwrap_or_default(),
        // The status field is drawn from live state, not from stored text.
        8 => String::new(),
        9 => p.note.clone(),
        _ => String::new(),
    }
}

pub(super) fn field_set(p: &mut crate::config::Profile, i: usize, v: &str) -> Result<(), String> {
    match i {
        0 => p.name = v.trim().to_string(),
        1 => p.host = v.trim().to_string(),
        2 => {
            let port: u16 = v
                .trim()
                .parse()
                .map_err(|_| t!("port must be a whole number 1-65535").to_string())?;
            if port == 0 {
                return Err(t!("port must be a whole number 1-65535").to_string());
            }
            p.port = port;
        }
        3 => p.user = v.trim().to_string(),
        4 => {
            p.key = opt(v);
            p.key_path = None;
        }
        5 => {} // the password is handled by the caller
        6 => p.upload_dir = opt(v),
        7 => p.download_dir = opt(v),
        8 => {} // read-only status
        9 => p.note = v.to_string(),
        _ => {} // the install commands are read-only
    }
    Ok(())
}

/// Commits the text being edited into field `i`.  `Err` keeps the field open —
/// the value did not validate.
fn commit_field(ed: &mut Editor, i: usize, kind: FieldKind) -> Result<(), String> {
    let value = std::mem::take(&mut ed.buf);
    if kind == FieldKind::Password {
        ed.password = value;
        Ok(())
    } else {
        match field_set(&mut ed.profile, i, &value) {
            Ok(()) => Ok(()),
            Err(e) => {
                ed.buf = value;
                Err(e)
            }
        }
    }
}

/// Moves the form by `delta` fields, opening the new one for typing when it is
/// something you can type into.  A read-only row is only landed on.
fn advance_editor(ed: &mut Editor, delta: i32) {
    let last = fields().len().saturating_sub(1) as i32;
    let next = (ed.field as i32 + delta).clamp(0, last) as usize;
    ed.field = next;
    match fields()[next].kind {
        FieldKind::Text => {
            ed.buf = field_value(&ed.profile, next);
            ed.editing = true;
        }
        FieldKind::Password => {
            ed.buf = ed.password.clone();
            ed.editing = true;
        }
        FieldKind::Status | FieldKind::Copy => ed.editing = false,
    }
}

pub fn open_editor(app: &mut App, profile: crate::config::Profile, is_new: bool) {
    let password = app.store.secret(&profile.name).unwrap_or_default();
    // Refresh the screen status while the form is open, so the field is real.
    if !is_new {
        probe_screen(app, &profile);
    }
    app.editor = Some(Editor::new(profile, password, is_new));
    app.view = View::Editor;
}

pub(super) fn draw_editor(f: &mut Frame, app: &App, area: Rect) {
    let Some(ed) = &app.editor else { return };
    // The status field shows the last background screen probe for this
    // connection, kept in a runtime cache (never on the saved profile).
    let screen_ok = app.screen_ok.get(&ed.profile.name).copied();
    let mut lines: Vec<Line> = Vec::new();
    let mut last = "";
    let mut selected_line = 0usize;

    for (i, def) in fields().iter().enumerate() {
        if def.section != last {
            if !lines.is_empty() {
                lines.push(Line::raw(""));
            }
            lines.push(Line::from(Span::styled(
                format!("  {}", def.section),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )));
            last = def.section;
        }

        let selected = i == ed.field;
        let editing = selected && ed.editing;
        let marker = if selected { "▶ " } else { "  " };
        if selected {
            selected_line = lines.len();
        }

        let value_text = if editing {
            format!("{}▏", ed.buf)
        } else if def.kind == FieldKind::Password {
            if ed.password.is_empty() {
                t!("(none)").to_string()
            } else {
                "••••••".to_string()
            }
        } else if def.kind == FieldKind::Status {
            match screen_ok {
                Some(true) => t!("installed").to_string(),
                Some(false) => t!("not installed").to_string(),
                None => t!("not tested yet").to_string(),
            }
        } else if def.kind == FieldKind::Copy {
            field_command(i).unwrap_or_default().to_string()
        } else {
            let v = field_value(&ed.profile, i);
            if v.is_empty() && i == 6 {
                t!("(session's current directory, then home)").into()
            } else if v.is_empty() && i == 7 {
                format!(
                    "({})",
                    crate::config::download_dir_for(&ed.profile).display()
                )
            } else if v.is_empty() {
                t!("(not set)").into()
            } else {
                v
            }
        };

        let value_style = if editing {
            Style::default().fg(Color::Yellow)
        } else if def.kind == FieldKind::Status {
            // Green when screen is there, plain white otherwise.
            match screen_ok {
                Some(true) => Style::default().fg(Color::Green),
                _ => Style::default().fg(Color::White),
            }
        } else if def.kind == FieldKind::Copy {
            // Commands read best in a plain, copyable white.
            Style::default().fg(Color::White)
        } else if selected {
            Style::default().fg(ACCENT)
        } else {
            Style::default().fg(Color::White)
        };

        lines.push(Line::from(vec![
            Span::raw(marker),
            Span::styled(
                pad_cells(def.label, 16),
                Style::default().fg(if selected { ACCENT } else { DIM }),
            ),
            Span::styled(value_text, value_style),
        ]));

        if selected && !def.hint.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("    {}", def.hint),
                Style::default().fg(DIM),
            )));
        }
    }

    let heading = if ed.is_new {
        t!("new connection")
    } else {
        t!("connection settings")
    };

    // Keep the selected field (and its hint) on screen in small terminals.
    let view_h = area.height.saturating_sub(2) as usize;
    let offset = (selected_line + 2).saturating_sub(view_h.max(1)) as u16;

    let body = Paragraph::new(lines).scroll((offset, 0)).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ACCENT))
            .title(format!(
                " {heading}: {} ",
                if ed.profile.name.is_empty() {
                    "…"
                } else {
                    &ed.profile.name
                }
            )),
    );
    f.render_widget(body, area);
}

pub(super) fn editor_keys(app: &mut App, key: KeyEvent) {
    let mut message: Option<String> = None;

    // The borrow of `app.editor` must not overlap `app.notice`, so each branch
    // only records a message and it is applied at the end.
    if let Some(ed) = app.editor.as_mut() {
        let field = ed.field;
        let kind = fields()[field].kind;

        // Set when Ctrl-S committed a field for saving from inside it; the key
        // then falls through to the normal Ctrl-S dispatch below.
        let mut saved = false;
        let was_editing = ed.editing;
        if was_editing {
            match key.code {
                KeyCode::Esc => ed.editing = false,
                KeyCode::Backspace => {
                    ed.buf.pop();
                }
                // Ctrl-S applies from inside a field too — commit the field
                // first, and only save when its value validates.
                KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    match commit_field(ed, field, kind) {
                        Ok(()) => {
                            ed.editing = false;
                            saved = true;
                        }
                        Err(e) => message = Some(e),
                    }
                }
                KeyCode::Char(c) => ed.buf.push(c),
                // Enter confirms this field and leaves the cursor on it.
                KeyCode::Enter => match commit_field(ed, field, kind) {
                    Ok(()) => ed.editing = false,
                    Err(e) => message = Some(e),
                },
                // Tab confirms it and moves on, opening the next field to type.
                KeyCode::Tab => match commit_field(ed, field, kind) {
                    Ok(()) => advance_editor(ed, 1),
                    Err(e) => message = Some(e),
                },
                _ => {}
            }
        }

        // While a field was being edited its keys are consumed — except the
        // Ctrl-S that just committed it, which still saves.
        if !was_editing || saved {
            match key.code {
                KeyCode::F(10) => app.should_quit = true,
                KeyCode::Up | KeyCode::BackTab => {
                    ed.field = field.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Tab => {
                    ed.field = (field + 1).min(fields().len() - 1);
                }
                KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    // handled after the borrow ends
                    message = Some("\u{1}save".into());
                }
                KeyCode::Esc => {
                    message = Some("\u{1}close".into());
                }
                KeyCode::Char(' ') | KeyCode::Enter => match kind {
                    FieldKind::Password => {
                        ed.buf = ed.password.clone();
                        ed.editing = true;
                    }
                    FieldKind::Text => {
                        ed.buf = field_value(&ed.profile, field);
                        ed.editing = true;
                    }
                    // A status is shown, not edited.
                    FieldKind::Status => {}
                    // An install command row: copy it to the clipboard.
                    FieldKind::Copy => {
                        if let Some(cmd) = field_command(field) {
                            message = Some(format!("\u{2}{cmd}"));
                        }
                    }
                },
                _ => {}
            }
        }
    }

    match message.as_deref() {
        Some("\u{1}save") => save_editor(app),
        Some("\u{1}close") => {
            app.editor = None;
            app.view = View::Manager;
            app.notice(t!("settings discarded"));
        }
        Some(m) if m.starts_with('\u{2}') => copy_command(app, &m[1..]),
        Some(m) => app.notice(m),
        None => {}
    }
}

/// Copies an install command to the Windows clipboard and says so.
pub(super) fn copy_command(app: &mut App, cmd: &str) {
    match crate::platform::copy_to_clipboard(cmd) {
        Ok(()) => app.notice(tf!("copied to the clipboard: {}", cmd)),
        Err(e) => app.notice(tf!("could not copy: {}", e)),
    }
}

/// Validates and stores the profile the form is editing.
pub(super) fn save_editor(app: &mut App) {
    let Some(ed) = app.editor.as_ref() else {
        return;
    };
    let mut profile = ed.profile.clone();
    let password = ed.password.clone();
    let original = ed.original.clone();
    let is_new = ed.is_new;

    profile.tidy();
    if let Err(e) = profile.validate() {
        app.notice(tf!("cannot save: {}", e));
        return;
    }

    if let Some(old) = &original {
        if old != &profile.name {
            // `remove` also drops that connection's stored secret.
            app.store.remove(old);
        }
    }

    let has_secret = !password.is_empty();
    if has_secret {
        app.store.set_secret(&profile.name, password);
    } else {
        app.store.remove_secret(&profile.name);
    }
    let _ = app.store.save_secrets();
    profile.has_secret = has_secret || app.store.secret(&profile.name).is_some();

    // A window that came from this connection describes itself by the old name;
    // move it to the new one so reattach and host-scoped commands keep finding
    // it after a rename.
    if let Some(old) = &original {
        if old != &profile.name {
            for s in app.sessions.iter_mut() {
                if &s.profile_name == old {
                    s.profile_name = profile.name.clone();
                    s.title = profile.name.clone();
                    s.profile.name = profile.name.clone();
                }
            }
        }
    }

    app.store.upsert(profile.clone());
    let _ = app.store.save();
    // Put the cursor on what was just saved, so pressing Enter connects to
    // *this* connection rather than whatever happened to be selected before.
    if let Some(i) = app
        .store
        .profiles()
        .iter()
        .position(|p| p.name == profile.name)
    {
        app.selected = i;
    }
    app.editor = None;
    app.view = View::Manager;
    app.notice(if is_new {
        tf!("saved connection {}", profile.name)
    } else {
        tf!("updated connection {}", profile.name)
    });
}
