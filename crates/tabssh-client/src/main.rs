//! Tabssh — a small, dependency free Xshell-like ssh client.

mod agent;
mod app;
mod config;
#[cfg(feature = "e2e")]
mod e2e;
mod lang;
mod platform;
mod screen;
mod ssh;
mod transfer;
mod ui;

use std::io::{self, Stdout};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use app::{App, View};

/// The crate version, shown on the F1 help page.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// When this binary was built (`YYYY-MM-DD`), stamped by `build.rs`.
pub const BUILD_TIME: &str = env!("TABSSH_BUILD_TIME");

fn print_help() {
    println!("Tabssh {}", env!("CARGO_PKG_VERSION"));
    println!();
    println!("{}", t!("usage:"));
    println!("{}", t!("  tabssh                 start the terminal ui"));
    println!("{}", t!("  tabssh --version       print the version"));
    println!(
        "{}",
        t!("  tabssh --check         show what server side helpers are bundled")
    );
    println!();
    println!(
        "{}",
        t!("In the ui press F1 for the key list, F2 for the command bar and F9 for the session manager.")
    );
    println!("{}", t!("Connections are stored in:"));
    println!("{}", config::config_dir().display());
}

fn main() -> Result<()> {
    // Before anything is drawn: make a classic `cmd.exe` console speak UTF-8 and
    // find out whether it can draw the interface at all.
    let console = platform::prepare_console();

    let args: Vec<String> = std::env::args().skip(1).collect();

    // The saved preference is read first so that even the built-in help comes
    // out in the right language; `--lang` overrides it for this run.
    let store = config::Store::open();
    let flag = args
        .iter()
        .position(|a| a == "--lang")
        .and_then(|i| args.get(i + 1))
        .cloned();
    lang::init(flag.as_deref().or(store.lang().as_deref()));
    if let Some(warning) = store.take_load_warning() {
        eprintln!("{warning}");
    }

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--version" | "-V" => {
                println!("{}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            "--check" => {
                let arches = agent::bundled_targets();
                println!("Tabssh {}", env!("CARGO_PKG_VERSION"));
                println!("{}", tf!("config dir: {}", config::config_dir().display()));
                println!(
                    "{}",
                    tf!(
                        "language:   {} ({})",
                        lang::lang().name(),
                        lang::lang().code()
                    )
                );
                let console = match platform::console_kind() {
                    Some(platform::ConsoleKind::Modern) => t!("modern terminal"),
                    Some(platform::ConsoleKind::ClassicVt) => {
                        t!("classic console (ANSI enabled)")
                    }
                    _ => t!("no ANSI support"),
                };
                println!("{}", tf!("console:    {}", console));
                if arches.is_empty() {
                    println!(
                        "{}",
                        t!("bundled tabssh-agent: none (build it from crates/tabssh-agent)")
                    );
                } else {
                    println!("{}", tf!("bundled tabssh-agent for: {}", arches.join(", ")));
                }
                return Ok(());
            }
            "--lang" => {
                if let Some(spec) = args.get(i + 1) {
                    match lang::parse_spec(spec) {
                        Some(l) => lang::set(l),
                        None => eprintln!("unknown language '{spec}' — use en or zh"),
                    }
                }
                i += 1;
            }
            "--e2e" => {
                #[cfg(feature = "e2e")]
                {
                    let target = args
                        .get(i + 1)
                        .ok_or_else(|| anyhow::anyhow!("{}", t!("--e2e needs user@host[:port]")))?;
                    let profile = e2e::parse_target(target)?;
                    let password = std::env::var("TABSSH_PASSWORD").ok();
                    let rt = tokio::runtime::Builder::new_multi_thread()
                        .enable_all()
                        .build()?;
                    return rt.block_on(e2e::run(profile, password));
                }
                #[cfg(not(feature = "e2e"))]
                {
                    eprintln!(
                        "{}",
                        t!("this build has no --e2e self-check (rebuild with --features e2e)")
                    );
                    std::process::exit(2);
                }
            }
            _ => {}
        }
        i += 1;
    }

    // A console with no ANSI support cannot show the interface; say so plainly
    // instead of painting escape codes all over it.
    if console == platform::ConsoleKind::NoVt {
        eprintln!(
            "{}",
            t!("this console cannot show the interface — run tabssh in Windows Terminal, or a Windows 10 or newer console")
        );
        std::process::exit(2);
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let guard = runtime.enter();

    let (channels, controller) = app::spawn_controller(store.clone());

    let mut terminal = setup_terminal()?;
    let res = run(&mut terminal, channels, store);
    restore_terminal(&mut terminal)?;
    drop(guard);
    // Quitting sends `Cmd::Shutdown`; wait for the controller to finish ending
    // the plain (non-screen) shells before the runtime is torn down.
    let _ = runtime.block_on(controller);
    res
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(
        out,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    let mut terminal = Terminal::new(CrosstermBackend::new(out))?;
    terminal.clear()?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableBracketedPaste,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ch: app::Channels,
    store: config::Store,
) -> Result<()> {
    let size = terminal.size()?;
    let mut app = App {
        store,
        sessions: Vec::new(),
        active: None,
        view: View::Manager,
        cmd_input: String::new(),
        cmd_focus: false,
        notice: t!("F1 help · F2 command bar · F9 sessions").into(),
        selected: 0,
        should_quit: false,
        tx: ch.cmd,
        events: ch.events,
        remote_sessions: Vec::new(),
        remote_host: String::new(),
        poll_inflight: false,
        screen_ok: std::collections::BTreeMap::new(),
        editor: None,
        session_form: None,
        overlay: None,
        host_keys: Vec::new(),
        selection: None,
        cwd: config::desktop_dir(),
        last_transfer: None,
        completing: false,
        completion_for: None,
        pending_upload: None,
        cols: size.width.saturating_sub(2),
        rows: size.height.saturating_sub(5),
    };
    let mut ui = ui::Ui::default();

    while !app.should_quit {
        app.drain_events();
        // Anything still queued belongs to the same burst, so wait for it.
        let more = event::poll(Duration::from_millis(0))?;
        ui::flush_pending(&mut app, &mut ui, more);
        // Periodic work: the manager's twice-a-second refresh.
        ui::tick(&mut app, &mut ui);

        terminal.draw(|f| ui::draw(f, &app, &mut ui))?;

        if event::poll(Duration::from_millis(40))? {
            match event::read()? {
                Event::Key(k) if k.kind != KeyEventKind::Release => {
                    ui::handle_key(&mut app, &mut ui, k);
                }
                Event::Paste(text) => {
                    if !ui::handle_paste(&mut app, &text) {
                        ui::handle_paste_forward(&mut app, &text);
                    }
                }
                Event::Mouse(m) => ui::handle_mouse(&mut app, &mut ui, m),
                Event::Resize(w, h) => {
                    app.cols = w.saturating_sub(2);
                    app.rows = h.saturating_sub(5);
                    let (c, r) = (app.cols, app.rows);
                    for i in 0..app.sessions.len() {
                        app.sessions[i].resize_vt(c, r);
                        let id = app.sessions[i].id;
                        app.send(app::Cmd::Resize {
                            id,
                            cols: c,
                            rows: r,
                        });
                    }
                }
                _ => {}
            }
        }
    }

    app.send(app::Cmd::Shutdown);
    Ok(())
}
