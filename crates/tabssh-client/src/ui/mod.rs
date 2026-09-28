//! The terminal user interface: a session manager, embedded terminals and the
//! command bar.

mod command;
mod completion;
mod draw;
mod editor;
mod keys;
mod session;

#[cfg(test)]
mod tests;

use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Style};
use ratatui::Frame;

use crate::app::{App, Selection, View};

use self::command::*;
use self::completion::*;
use self::draw::*;
use self::editor::*;
use self::session::*;

pub use self::command::{handle_paste, handle_paste_forward};
pub use self::completion::apply_completion;
pub use self::editor::{open_editor, Editor};
pub use self::keys::{flush_pending, handle_key, handle_mouse, tick};
pub use self::session::SessionForm;

#[derive(Default)]
pub struct Ui {
    /// How far the help screen is scrolled.
    pub help_scroll: u16,
    /// Printable characters typed into the command bar, held for a moment.  A
    /// file dragged onto a Windows console arrives as a burst of keystrokes
    /// rather than a paste, so while the bar has focus they are held and
    /// checked for being a path before they are appended as text.
    pub pending: String,
    pub pending_at: Option<Instant>,
    /// When the manager's session list was last refreshed, so it is refreshed
    /// at most twice a second.
    pub screen_tick: Option<Instant>,
    /// The last composed frame, kept only while a mouse selection is up so the
    /// text it covers can be read back for the clipboard.
    pub frame: Option<Buffer>,
}

pub(super) const ACCENT: Color = Color::Cyan;
pub(super) const DIM: Color = Color::DarkGray;

// ---------------------------------------------------------------------------
// drawing
// ---------------------------------------------------------------------------

pub fn draw(f: &mut Frame, app: &App, ui: &mut Ui) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // tab bar / title
            Constraint::Min(3),    // body
            Constraint::Length(1), // command bar
            Constraint::Length(1), // status
        ])
        .split(f.area());

    draw_tabs(f, app, chunks[0]);

    match app.view {
        View::Manager => draw_manager(f, app, chunks[1]),
        View::Terminal => draw_terminal(f, app, chunks[1]),
        View::Help => draw_help(f, app, ui, chunks[1]),
        View::Editor => draw_editor(f, app, chunks[1]),
        View::ScreenForm => draw_session_form(f, app, chunks[1]),
    }

    draw_overlay(f, app, chunks[1]);
    draw_host_key(f, app, chunks[1]);
    draw_command_bar(f, app, chunks[2]);
    draw_status(f, app, chunks[3]);

    // While a mouse selection is up: keep the composed text (for the clipboard)
    // and paint the covered cells inverted, over whatever view is showing.
    if let Some(sel) = app.selection {
        ui.frame = Some(f.buffer_mut().clone());
        highlight_selection(f.buffer_mut(), sel);
    } else {
        ui.frame = None;
    }
}

/// Paints the cells a selection covers in reverse video.
fn highlight_selection(buf: &mut Buffer, sel: Selection) {
    let (top, bottom) = sel.ordered();
    let last_row = buf.area.height.saturating_sub(1);
    let last_col = buf.area.width.saturating_sub(1);
    for y in top.0..=bottom.0.min(last_row) {
        let x0 = if y == top.0 { top.1 } else { 0 };
        let x1 = if y == bottom.0 { bottom.1 } else { last_col };
        for x in x0..=x1.min(last_col) {
            buf[(x, y)].set_style(Style::default().fg(Color::Black).bg(ACCENT));
        }
    }
}

/// The text a selection covers: one line per row, trailing blanks trimmed,
/// blank rows at the edges dropped (they are padding, not content).
pub(super) fn selected_text(buf: &Buffer, sel: Selection) -> String {
    let (top, bottom) = sel.ordered();
    let last_row = buf.area.height.saturating_sub(1);
    let last_col = buf.area.width.saturating_sub(1);
    let mut lines: Vec<String> = Vec::new();
    for y in top.0..=bottom.0.min(last_row) {
        let x0 = if y == top.0 { top.1 } else { 0 };
        let x1 = if y == bottom.0 { bottom.1 } else { last_col };
        let mut line = String::new();
        for x in x0..=x1.min(last_col) {
            line.push_str(buf[(x, y)].symbol());
        }
        lines.push(line.trim_end().to_string());
    }
    while lines.first().is_some_and(|l| l.is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// shared text helpers
// ---------------------------------------------------------------------------

pub(super) fn vt_color(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
        vt100::Color::Idx(i) => Color::Indexed(i),
    }
}

pub(super) fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
    }
}

pub(super) fn cell_width(c: char) -> usize {
    let u = c as u32;
    let wide = (0x1100..=0x115F).contains(&u)
        || (0x2E80..=0xA4CF).contains(&u)
        || (0xAC00..=0xD7A3).contains(&u)
        || (0xF900..=0xFAFF).contains(&u)
        || (0xFE30..=0xFE6F).contains(&u)
        || (0xFF00..=0xFF60).contains(&u)
        || (0xFFE0..=0xFFE6).contains(&u)
        || (0x20000..=0x3FFFD).contains(&u);
    if wide {
        2
    } else {
        1
    }
}

/// Pads by terminal cells, so mixed English and Chinese columns line up.
pub(super) fn pad_cells(s: &str, cells: usize) -> String {
    let w: usize = s.chars().map(cell_width).sum();
    if w >= cells {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(cells - w))
    }
}
