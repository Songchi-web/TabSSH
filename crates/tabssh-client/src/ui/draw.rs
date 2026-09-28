use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Tabs, Wrap};
use ratatui::Frame;

use crate::app::{App, Row};
use crate::{t, tf};

use super::*;

pub(super) fn draw_tabs(f: &mut Frame, app: &App, area: Rect) {
    let titles: Vec<Line> = app
        .sessions
        .iter()
        .map(|s| Line::from(format!(" {} ", s.tab())))
        .collect();
    let sel = app.active.unwrap_or(0).min(titles.len().saturating_sub(1));
    if titles.is_empty() {
        let p = Paragraph::new(Line::from(vec![
            Span::styled(
                " tabssh ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                t!("no open windows — F9 manager, F1 help"),
                Style::default().fg(DIM),
            ),
        ]));
        f.render_widget(p, area);
        return;
    }
    let tabs = Tabs::new(titles)
        .select(sel)
        .highlight_style(Style::default().fg(Color::Black).bg(ACCENT))
        .divider("");
    f.render_widget(tabs, area);
}

pub(super) fn draw_manager(f: &mut Frame, app: &App, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(63), Constraint::Percentage(37)])
        .split(area);

    let rows = app.manager_rows();
    let mut lines: Vec<Line> = Vec::new();
    let mut last_kind = "";
    let mut selected_line = 0usize;
    // A ruled line between one group of rows and the next, so windows and the
    // persistent tasks they are decoupled from never read as one list.
    let divider = || {
        Line::from(Span::styled(
            "─".repeat(cols[0].width.saturating_sub(2) as usize),
            Style::default().fg(DIM),
        ))
    };
    for (i, r) in rows.iter().enumerate() {
        let selected = i == app.selected;
        let marker = if selected { "▶ " } else { "  " };
        let line = match r {
            Row::Profile(p) => {
                if last_kind != "profile" {
                    if !last_kind.is_empty() {
                        lines.push(divider());
                    }
                    lines.push(Line::from(Span::styled(
                        t!("  saved connections"),
                        Style::default().fg(DIM),
                    )));
                    last_kind = "profile";
                }
                let spans = vec![
                    Span::raw(marker),
                    Span::styled(
                        format!("{:<16}", truncate(&p.name, 16)),
                        Style::default().fg(if selected { ACCENT } else { Color::White }),
                    ),
                    Span::raw(format!("{} ", p.addr())),
                ];
                Line::from(spans)
            }
            Row::Window {
                index,
                title,
                status,
            } => {
                if last_kind != "window" {
                    if !last_kind.is_empty() {
                        lines.push(divider());
                    }
                    lines.push(Line::from(Span::styled(
                        t!("  open windows"),
                        Style::default().fg(DIM),
                    )));
                    last_kind = "window";
                }
                let live = app
                    .sessions
                    .get(*index)
                    .map(|s| s.is_live())
                    .unwrap_or(false);
                Line::from(vec![
                    Span::raw(marker),
                    Span::styled(
                        format!("{:<16}", truncate(title, 16)),
                        Style::default().fg(if selected { ACCENT } else { Color::White }),
                    ),
                    Span::styled(
                        format!("{} ", truncate(status, 28)),
                        Style::default().fg(if live { Color::Green } else { DIM }),
                    ),
                ])
            }
            Row::Remote {
                index,
                title,
                status,
            } => {
                if last_kind != "remote" {
                    if !last_kind.is_empty() {
                        lines.push(divider());
                    }
                    lines.push(Line::from(Span::styled(
                        t!("  persistent tasks"),
                        Style::default().fg(DIM),
                    )));
                    last_kind = "remote";
                }
                let verdict = app
                    .remote_sessions
                    .get(*index)
                    .map(|r| r.verdict.as_str())
                    .unwrap_or("");
                let colour = match verdict {
                    "running" => Color::Green,
                    "idle" => Color::Cyan,
                    "unmeasured" | "" => Color::Yellow,
                    _ => Color::Red,
                };
                Line::from(vec![
                    Span::raw(marker),
                    Span::styled(
                        format!("{:<8}", truncate(title, 20)),
                        Style::default().fg(if selected { ACCENT } else { Color::White }),
                    ),
                    Span::styled(
                        format!("{} ", truncate(status, 44)),
                        Style::default().fg(colour),
                    ),
                ])
            }
        };
        lines.push(line);
        if selected {
            selected_line = lines.len() - 1;
        }
    }
    if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            t!("  none yet — F2, then new <user@host[:port]>"),
            Style::default().fg(DIM),
        )));
    }

    let list_h = cols[0].height.saturating_sub(2) as usize;
    let list_offset = (selected_line + 1).saturating_sub(list_h.max(1)) as u16;
    let list = Paragraph::new(lines).scroll((list_offset, 0)).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(DIM))
            .title(t!(" sessions ")),
    );
    f.render_widget(list, cols[0]);

    let detail = match rows.get(app.selected) {
        Some(Row::Profile(p)) => vec![
            Line::from(Span::styled(
                &p.name,
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )),
            Line::raw(""),
            Line::from(tf!("  host      {}:{}", p.host, p.port)),
            Line::from(tf!("  user      {}", p.user)),
            Line::raw(""),
            Line::from(Span::styled(
                t!("  Enter   connect"),
                Style::default().fg(DIM),
            )),
            Line::from(Span::styled(
                t!("  e       edit settings (form)"),
                Style::default().fg(DIM),
            )),
        ],
        Some(Row::Window { index, .. }) => {
            let s = &app.sessions[*index];
            // The window's name is its number; the connection is the
            // description, matching the list row and the tab bar.
            vec![
                Line::from(Span::styled(
                    format!("{}  {}", s.name(), s.title),
                    Style::default().fg(ACCENT),
                )),
                Line::raw(""),
                Line::from(tf!("  host   {}", s.host)),
                Line::from(tf!("  size   {}x{}", s.cols, s.rows)),
                Line::from(tf!("  status {}", s.status)),
                Line::from(tf!(
                    "  cwd    {}",
                    s.cwd.clone().unwrap_or_else(|| t!("(unknown)").into())
                )),
                Line::raw(""),
                Line::from(Span::styled(
                    t!("  Enter   switch to window"),
                    Style::default().fg(DIM),
                )),
                Line::from(Span::styled(
                    t!("  q       close the window"),
                    Style::default().fg(DIM),
                )),
            ]
        }
        Some(Row::Remote { index, .. }) => {
            let r = &app.remote_sessions[*index];
            let mut v = vec![
                Line::from(Span::styled(&r.id, Style::default().fg(ACCENT))),
                Line::raw(""),
                Line::from(tf!("  host        {}", app.remote_host)),
            ];
            if r.shell == "screen" {
                // A screen task carries none of the tool's supervisor / liveness
                // machinery, so show only what is real about it.
                let state = if r.verdict == "running" {
                    t!("attached")
                } else {
                    t!("detached")
                };
                v.push(Line::from(format!("  pid         {}", r.pid)));
                v.push(Line::from(tf!("  state       {}", state)));
                v.push(Line::raw(""));
                v.push(Line::from(tf!(
                    "  cpu         {}% (peak {}%)",
                    format!("{:.1}", r.cpu_pct),
                    format!("{:.1}", r.cpu_pct_max)
                )));
                v.push(Line::from(tf!("  memory      {}", r.mem_label())));
            } else {
                let age = crate::app::human_age(r.age_secs);
                v.push(Line::from(format!("  shell       {}", r.shell)));
                v.push(Line::from(format!("  pid         {}", r.pid)));
                v.push(Line::from(tf!("  supervisor  {}", r.sup_pid)));
                v.push(Line::from(tf!("  age         {}", age)));
                if r.measured() {
                    let colour = match r.verdict.as_str() {
                        "running" => Color::Green,
                        "idle" => Color::Cyan,
                        _ => Color::Red,
                    };
                    v.push(Line::raw(""));
                    v.push(Line::from(Span::styled(
                        tf!("  verdict     {}", r.verdict),
                        Style::default().fg(colour).add_modifier(Modifier::BOLD),
                    )));
                    v.push(Line::from(tf!(
                        "  cpu         {}% (peak {}%)",
                        format!("{:.1}", r.cpu_pct),
                        format!("{:.1}", r.cpu_pct_max)
                    )));
                    v.push(Line::from(tf!("  memory      {}", r.mem_label())));
                    v.push(Line::from(tf!("  processes   {}", r.procs)));
                    v.push(Line::from(tf!(
                        "  last output {}s ago",
                        r.last_activity_secs
                    )));
                    v.push(Line::from(tf!(
                        "  socket      {}   responsive {}   probe {}",
                        r.socket,
                        r.responsive,
                        r.probe
                    )));
                    v.push(Line::from(tf!("  shell alive {}", r.alive)));
                } else {
                    v.push(Line::raw(""));
                    v.push(Line::from(Span::styled(
                        t!("  not measured yet — refreshing…"),
                        Style::default().fg(DIM),
                    )));
                }
            }
            v.push(Line::raw(""));
            v.push(Line::from(Span::styled(
                t!("  Enter   reattach"),
                Style::default().fg(DIM),
            )));
            v.push(Line::from(Span::styled(
                t!("  e   edit the session"),
                Style::default().fg(DIM),
            )));
            v.push(Line::from(Span::styled(
                t!("  q   end this task"),
                Style::default().fg(DIM),
            )));
            v
        }
        None => vec![Line::from(Span::styled(
            t!("  nothing selected"),
            Style::default().fg(DIM),
        ))],
    };
    f.render_widget(
        Paragraph::new(detail).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(DIM))
                .title(t!(" details ")),
        ),
        cols[1],
    );
}

pub(super) fn draw_terminal(f: &mut Frame, app: &App, area: Rect) {
    let Some(s) = app.active_session() else {
        f.render_widget(
            Paragraph::new(t!("no session — F9 to open one"))
                .block(Block::default().borders(Borders::ALL)),
            area,
        );
        return;
    };
    let screen = s.parser.screen();
    let (rows, cols) = screen.size();
    let mut lines: Vec<Line> = Vec::with_capacity(area.height as usize);
    for y in 0..rows.min(area.height) {
        let mut spans: Vec<Span> = Vec::new();
        let mut x = 0u16;
        while x < cols {
            let Some(cell) = screen.cell(y, x) else {
                x += 1;
                continue;
            };
            if cell.is_wide_continuation() {
                x += 1;
                continue;
            }
            let mut style = Style::default();
            style = style.fg(vt_color(cell.fgcolor()));
            style = style.bg(vt_color(cell.bgcolor()));
            if cell.bold() {
                style = style.add_modifier(Modifier::BOLD);
            }
            if cell.italic() {
                style = style.add_modifier(Modifier::ITALIC);
            }
            if cell.underline() {
                style = style.add_modifier(Modifier::UNDERLINED);
            }
            if cell.inverse() {
                style = style.add_modifier(Modifier::REVERSED);
            }
            let text = if cell.has_contents() {
                cell.contents()
            } else {
                " ".into()
            };
            spans.push(Span::styled(text, style));
            x += 1;
        }
        lines.push(Line::from(spans));
    }

    let scroll = if s.scroll > 0 {
        tf!("[scrollback {}]", s.scroll)
    } else {
        String::new()
    };
    // The window is named by its number, with the connection it came from as
    // the description beside it — the same pairing as the tab bar and the
    // manager list.
    let title = format!(
        " {} {} — {} {} ",
        s.name(),
        s.title,
        s.cwd.clone().unwrap_or_else(|| "?".into()),
        scroll
    );
    let p = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(DIM))
            .title(title),
    );
    f.render_widget(p, area);

    // Place the cursor where the remote shell put it.
    if s.scroll == 0 && !screen.hide_cursor() {
        let (cy, cx) = screen.cursor_position();
        let x = area.x + 1 + cx.min(area.width.saturating_sub(2));
        let y = area.y + 1 + cy.min(area.height.saturating_sub(2));
        f.set_cursor_position((x, y));
    }
}

pub(super) fn draw_help(f: &mut Frame, _app: &App, ui: &Ui, area: Rect) {
    fn two_col(rows: &[(&str, &str)]) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < rows.len() {
            let mut line = format!("  {:<3} {}", rows[i].0, pad_cells(rows[i].1, 24));
            if i + 1 < rows.len() {
                line.push_str(&format!("{:<3} {}", rows[i + 1].0, rows[i + 1].1));
            }
            out.push(Line::from(line));
            i += 2;
        }
        out
    }

    let mut text: Vec<Line> = Vec::new();
    text.push(Line::from(Span::styled(
        t!("tabssh — keys"),
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )));
    // Which build this is, right under the title: the version and the moment it
    // was compiled.  Kept as a literal — a version and a timestamp are the same
    // in every language.
    text.push(Line::from(Span::styled(
        format!("v{} — built {}", crate::VERSION, crate::BUILD_TIME),
        Style::default().fg(DIM),
    )));

    // The two top-level sections sit side by side: what you press, and what you
    // type.  Keys first.
    text.push(Line::raw(""));
    text.push(Line::from(Span::styled(
        t!("quick actions"),
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )));
    text.push(Line::raw(""));
    text.extend(two_col(&[
        ("F1", t!("this help")),
        ("F2", t!("command bar")),
        ("F3", t!("next window")),
        ("F4", t!("previous window")),
        ("F5", t!("refresh remote cwd")),
        ("F6", t!("upload files (or drag & drop)")),
        ("F8", t!("close / detach window")),
        ("F9", t!("session manager")),
        ("F10", t!("quit")),
        ("PgUp/PgDn", t!("scroll the terminal's scrollback")),
        ("mouse drag", t!("select text; releasing copies it to the clipboard")),
        ("right click", t!("paste the clipboard")),
    ]));
    // Opening a connection belongs here, beside the keys: it is Enter and Tab in
    // the manager, not a command-bar verb.
    text.push(Line::raw(""));
    text.push(Line::from(Span::styled(
        format!(" {}", t!("opening a connection")),
        Style::default().fg(ACCENT),
    )));
    for (usage, what) in [
        (
            "Enter",
            t!("open a regular shell connection window"),
        ),
        (
            "Tab",
            t!("create a new persistent task"),
        ),
        (
            "",
            t!("the task keeps running after you disconnect; reattach it from the manager (F9)"),
        ),
    ] {
        text.push(Line::from(format!("  {:<24} {}", usage, what)));
    }

    text.push(Line::raw(""));
    text.push(Line::from(Span::styled(
        t!("command bar — F2"),
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )));
    // Grouped, with a blank line between, so it reads as sections instead of
    // one wall of commands.
    let groups: Vec<(&str, Vec<(&str, &str)>)> = vec![
        (
            t!("connections"),
            vec![
                (
                    "new [<name>] <user@host[:port]>",
                    t!("new connection — the name is chosen for you"),
                ),
                ("edit <name>", t!("change its settings")),
                ("quit <name>", t!("forget it")),
            ],
        ),
        (
            t!("files"),
            vec![
                ("ls [dir]", t!("list a local directory")),
                ("cd [dir]", t!("go to a local directory")),
                ("get <remote-path>", t!("download a file or directory")),
                (
                    "put <local-path> …",
                    t!("upload to the session's current directory"),
                ),
            ],
        ),
        (
            t!("sessions"),
            vec![(
                "quit [n|task|name]",
                t!("close a window, end a task, or forget a connection"),
            )],
        ),
        (
            t!("this program"),
            vec![("setlang auto|en|zh", t!("switch language"))],
        ),
    ];
    for (title, rows) in &groups {
        text.push(Line::raw(""));
        text.push(Line::from(Span::styled(
            format!(" {title}"),
            Style::default().fg(ACCENT),
        )));
        for (usage, what) in rows {
            text.push(Line::from(format!("  {:<24} {}", usage, what)));
        }
    }

    text.push(Line::raw(""));
    text.push(Line::from(Span::styled(
        t!("Tab completes names and paths everywhere"),
        Style::default().fg(DIM),
    )));

    // A short "about" note closes the page: what tabssh is, in one breath.
    // The page does not wrap, so each line is its own string.
    text.push(Line::raw(""));
    text.push(Line::from(Span::styled(
        t!("about"),
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )));
    for line in [
        t!("tabssh is an extremely lightweight terminal SSH client for Windows,"),
        t!("connecting to Linux servers. Through the F2 command bar, Tab completion"),
        t!("and GNU screen persistent-task management, it feels more Linux-native and"),
        t!("makes remote work more efficient."),
    ] {
        text.push(Line::from(line));
    }
    // Scrolling past the end would show a blank screen, so clamp it here.
    let visible = area.height.saturating_sub(2) as usize;
    let offset = ui
        .help_scroll
        .min(text.len().saturating_sub(visible) as u16);
    f.render_widget(
        Paragraph::new(text).scroll((offset, 0)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(DIM))
                .title(t!(" help — scroll with ↑ ↓ ")),
        ),
        area,
    );
}

/// The list drawn over the bottom of the body: tab completions, or `ls`.
pub(super) fn draw_overlay(f: &mut Frame, app: &App, area: Rect) {
    let Some(o) = &app.overlay else { return };
    if o.items.is_empty() || area.height < 4 || area.width < 16 {
        return;
    }
    let rows = o.items.len().min(12);
    let height = rows as u16 + 2;
    let width = area.width.saturating_sub(4).min(90);
    let rect = Rect {
        x: area.x + 2,
        y: area.y + area.height.saturating_sub(height),
        width,
        height,
    };

    // Keep the highlighted row in view.
    let offset = o.pick.saturating_sub(rows.saturating_sub(1)) as u16;
    let lines: Vec<Line> = o
        .items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let style = if o.selectable && i == o.pick {
                Style::default().fg(Color::Black).bg(ACCENT)
            } else {
                Style::default()
            };
            Line::from(Span::styled(format!(" {item}"), style))
        })
        .collect();

    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(lines).scroll((offset, 0)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT))
                .title(format!(" {} ", o.title)),
        ),
        rect,
    );
}

/// The modal host-key ruling, drawn over everything else until it is answered.
pub(super) fn draw_host_key(f: &mut Frame, app: &App, area: Rect) {
    let Some(p) = &app.host_keys.first() else {
        return;
    };
    let changed = p.previous.is_some();
    let warn = if changed { Color::Red } else { ACCENT };

    let mut lines: Vec<Line> = Vec::new();
    if changed {
        lines.push(Line::from(Span::styled(
            tf!("WARNING: the host key for {} changed", p.host).to_string(),
            Style::default().fg(Color::Red),
        )));
        lines.push(Line::from(tf!(
            "saved:   {}",
            p.previous.as_deref().unwrap_or("")
        )));
        lines.push(Line::from(tf!("offered: {}", p.fingerprint)));
    } else {
        lines.push(Line::from(tf!("new host key for {}", p.host).to_string()));
        lines.push(Line::from(p.fingerprint.clone()));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        t!("y trust & save · n refuse").to_string(),
        Style::default().fg(DIM),
    )));

    let height = lines.len() as u16 + 2;
    if area.width < 34 || area.height < height + 2 {
        return;
    }
    let line_cells = |l: &Line| -> usize {
        l.spans
            .iter()
            .map(|s| s.content.chars().map(cell_width).sum::<usize>())
            .sum()
    };
    let width = (lines.iter().map(line_cells).max().unwrap_or(0) as u16 + 4)
        .clamp(30, area.width.saturating_sub(4));
    let rect = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(warn))
                .title(format!(" {} ", t!("host key"))),
        ),
        rect,
    );
}

pub(super) fn draw_command_bar(f: &mut Frame, app: &App, area: Rect) {
    let prompt = if app.cmd_focus { ":" } else { " " };
    let style = if app.cmd_focus {
        Style::default().fg(Color::White)
    } else {
        Style::default().fg(DIM)
    };
    let hint = if app.cmd_focus {
        t!("enter runs · Ctrl-C clears · Esc closes · drop a file here to fill in a put")
    } else {
        t!("F2 command bar · F9 sessions · F1 help")
    };
    let text = if app.cmd_input.is_empty() && !app.cmd_focus {
        hint.to_string()
    } else {
        format!("{prompt}{}", app.cmd_input)
    };
    f.render_widget(Paragraph::new(Line::from(Span::styled(text, style))), area);
    if app.cmd_focus {
        // Put the (terminal-blinking) cursor at the end of the line, the way a
        // shell does, so it is obvious the bar owns the keyboard.  Columns are
        // counted in terminal cells, so a wide character does not shift it.
        let w: usize = app.cmd_input.chars().map(cell_width).sum();
        let x = area.x + 1 + w as u16;
        let max_x = area.x + area.width.saturating_sub(1);
        f.set_cursor_position((x.min(max_x), area.y));
    }
}

pub(super) fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let left = if !app.notice.is_empty() {
        app.notice.clone()
    } else {
        String::new()
    };

    // Bottom right: where local paths resolve from, and how the last transfer
    // went.  Always present, so the answer to "where am I" is never a guess.
    let mut right: Vec<String> = Vec::new();
    if let Some(rate) = &app.last_transfer {
        right.push(rate.clone());
    }
    right.push(app.cwd.display().to_string());
    let right = right.join("   ");

    let width = area.width as usize;
    let right_len = right.chars().count();
    let text = if right_len + 2 >= width {
        // No room for both: the directory matters more than the message.
        truncate(&right, width)
    } else {
        let left_budget = width - right_len - 1;
        let left = truncate(&left, left_budget);
        let pad = width - left.chars().count() - right_len;
        format!("{left}{}{right}", " ".repeat(pad))
    };

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(text, Style::default().fg(DIM)))),
        area,
    );
}
