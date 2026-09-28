    use super::*;
    use crate::app::{App, Cmd, Completion, CompletionKind, Row, SessionId, View};
    use crate::config::Profile;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::time::{Duration, Instant};

    /// Keeps the tests from touching the real user config directory.
    fn isolate_config() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let dir = std::env::temp_dir().join("tabssh-test-config");
            let _ = std::fs::create_dir_all(&dir);
            std::env::set_var("TABSSH_CONFIG_DIR", &dir);
        });
    }

    fn test_app() -> App {
        isolate_config();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        // Keep the runtime alive for the process: the controller task must
        // keep running, and dropping the runtime would abort it.
        let rt = Box::leak(Box::new(rt));
        let _guard = rt.enter();

        let store = crate::config::Store::default();
        let mut p = Profile::new("demo", "example.invalid", "alice");
        p.port = 2222;
        store.upsert(p.clone());
        let (ch, _controller) = crate::app::spawn_controller(store.clone());
        App {
            store,
            sessions: Vec::new(),
            active: None,
            view: View::Manager,
            cmd_input: String::new(),
            cmd_focus: false,
            notice: String::new(),
            selected: 0,
            should_quit: false,
            tx: ch.cmd,
            events: ch.events,
            remote_sessions: Vec::new(),
            remote_host: String::new(),
            poll_inflight: false,
            screen_ok: Default::default(),
            editor: None,
            session_form: None,
            overlay: None,
            host_keys: Vec::new(),
            selection: None,
            cwd: crate::config::desktop_dir(),
            last_transfer: None,
            completing: false,
            completion_for: None,
            pending_upload: None,
            cols: 80,
            rows: 24,
        }
    }

    fn render_sized(app: &App, ui: &mut Ui, w: u16, h: u16) -> String {
        let backend = TestBackend::new(w, h);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, app, ui)).unwrap();
        let buf = term.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn render(app: &App, ui: &mut Ui) -> String {
        render_sized(app, ui, 100, 30)
    }

    /// An app whose outgoing controller commands can be inspected, so a test
    /// can see exactly what was forwarded to the remote shell.
    fn app_with_sink() -> (App, tokio::sync::mpsc::UnboundedReceiver<Cmd>) {
        let mut app = test_app();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        app.tx = tx;
        (app, rx)
    }

    /// Everything the app sent towards the remote shell, concatenated.
    fn forwarded(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Cmd>) -> Vec<u8> {
        let mut out = Vec::new();
        while let Ok(cmd) = rx.try_recv() {
            if let Cmd::Input { data, .. } = cmd {
                out.extend_from_slice(&data);
            }
        }
        out
    }

    fn type_keys(app: &mut App, ui: &mut Ui, text: &str) {
        for c in text.chars() {
            handle_key(app, ui, KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
    }

    #[test]
    fn manager_renders_saved_connections() {
        let app = test_app();
        let mut ui = Ui::default();
        let screen = render(&app, &mut ui);
        assert!(screen.contains("saved connections"), "{screen}");
        assert!(screen.contains("demo"), "{screen}");
        // The sidebar is narrow, so the full address appears in the details
        // panel rather than the list row.
        assert!(screen.contains("example.invalid:2222"), "{screen}");
        assert!(screen.contains("alice"), "{screen}");
        assert!(screen.contains("tabssh"), "{screen}");
    }

    #[test]
    fn help_view_lists_the_key_map() {
        let mut app = test_app();
        app.view = View::Help;
        let mut ui = Ui::default();
        // Tall enough for the whole list at once.
        let screen = render_sized(&app, &mut ui, 110, 46);
        for needle in [
            "F2",
            "F10",
            "command bar",
            "connections",
            "opening a connection",
            "files",
            "sessions",
            "this program",
            "quit",
            "setlang",
            "switch language",
        ] {
            assert!(screen.contains(needle), "missing {needle} in:\n{screen}");
        }
        // The removed verbs are gone from the list.
        for gone in [
            "monitor",
            "keep / normal",
            "help / quit",
            "clean",
            // The `windows`, `close` and `detach` command-bar verbs.
            "same as F9",
            "close the current window",
            "leave the task running",
        ] {
            assert!(!screen.contains(gone), "stale help line {gone} in:\n{screen}");
        }
    }

    #[test]
    fn the_help_closes_with_an_about_note() {
        let mut app = test_app();
        app.view = View::Help;
        let mut ui = Ui::default();
        // Tall enough to see the tail of the page without scrolling.
        let screen = render_sized(&app, &mut ui, 110, 60);
        for needle in [
            "about",
            "extremely lightweight",
            "F2 command bar",
            "GNU screen",
            "more efficient",
        ] {
            assert!(screen.contains(needle), "missing {needle} in:\n{screen}");
        }
    }

    #[test]
    fn the_help_scrolls_on_a_small_screen() {
        let mut app = test_app();
        app.view = View::Help;
        let mut ui = Ui::default();

        let top = render_sized(&app, &mut ui, 100, 20);
        assert!(top.contains("quick actions"), "{top}");
        assert!(
            !top.contains("switch language"),
            "the tail should be off screen"
        );

        ui.help_scroll = 40;
        let bottom = render_sized(&app, &mut ui, 100, 20);
        assert!(
            bottom.contains("switch language") || bottom.contains("quit"),
            "{bottom}"
        );
    }

    #[test]
    fn terminal_view_renders_remote_output() {
        let mut app = test_app();
        let p = app.store.profiles()[0].clone();
        let id = app.new_session(&p);
        app.view = View::Terminal;
        if let Some(i) = app.session_index(id) {
            app.sessions[i].process(b"hello from the remote shell\r\n$ ");
        }
        let mut ui = Ui::default();
        let screen = render(&app, &mut ui);
        assert!(screen.contains("hello from the remote shell"), "{screen}");
    }

    #[test]
    fn the_help_page_shows_the_version_and_build_time() {
        let mut app = test_app();
        app.view = View::Help;
        let mut ui = Ui::default();
        let screen = render_sized(&app, &mut ui, 120, 30);
        assert!(
            screen.contains(crate::VERSION),
            "the version must be on the F1 page:\n{screen}"
        );
        assert!(
            screen.contains(crate::BUILD_TIME),
            "the build time must be on the F1 page:\n{screen}"
        );
    }

    #[test]
    fn page_keys_scroll_the_terminal_scrollback() {
        let (mut app, mut rx) = app_with_sink();
        let p = app.store.profiles()[0].clone();
        let id = app.new_session(&p);
        app.view = View::Terminal;
        if let Some(i) = app.session_index(id) {
            let mut out = Vec::new();
            for n in 0..100 {
                out.extend_from_slice(format!("line {n}\r\n").as_bytes());
            }
            app.sessions[i].process(&out);
        }
        let mut ui = Ui::default();
        let scroll = |app: &App| app.active_session().map(|s| s.scroll).unwrap_or(0);

        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
        );
        assert!(scroll(&app) > 0, "PageUp must look back into the scrollback");
        assert!(
            forwarded(&mut rx).is_empty(),
            "PageUp must be captured locally, not sent to the shell"
        );

        // PageDown comes back towards the live view and stops at the bottom.
        for _ in 0..3 {
            handle_key(
                &mut app,
                &mut ui,
                KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
            );
        }
        assert_eq!(scroll(&app), 0);

        // An ordinary key snaps back to the live view and still reaches the shell.
        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
        );
        assert!(scroll(&app) > 0);
        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        );
        assert_eq!(scroll(&app), 0, "typing returns to the live view");
        assert_eq!(forwarded(&mut rx), b"x");
    }

    #[test]
    fn scrolling_back_past_a_screen_shows_older_lines() {
        let mut app = test_app();
        let p = app.store.profiles()[0].clone();
        let id = app.new_session(&p);
        app.view = View::Terminal;
        if let Some(i) = app.session_index(id) {
            let mut out = Vec::new();
            for n in 0..100 {
                out.extend_from_slice(format!("line {n}\r\n").as_bytes());
            }
            app.sessions[i].process(&out);
        }
        let mut ui = Ui::default();
        // Several pages back: the offset goes past one screen, which is where a
        // naive scrollback implementation falls over.
        for _ in 0..6 {
            handle_key(
                &mut app,
                &mut ui,
                KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
            );
        }
        let screen = render(&app, &mut ui);
        assert!(screen.contains("line 1"), "older lines should be in view:\n{screen}");
        assert!(
            !screen.contains("line 99"),
            "the newest line should have scrolled out of view:\n{screen}"
        );
    }

    /// Opens a window that the controller would report as a screen task.
    fn screen_task(app: &mut App, p: &crate::config::Profile) -> SessionId {
        let id = app.new_session(p);
        app.apply(crate::app::Event::Opened {
            id,
            title: p.name.clone(),
            agent_ready: true,
            remote_id: Some("qf001".into()),
            fallback: None,
        });
        id
    }

    #[test]
    fn a_screen_task_keeps_its_output_on_the_main_scrollback() {
        let mut app = test_app();
        let p = app.store.profiles()[0].clone();
        let id = screen_task(&mut app, &p);
        app.view = View::Terminal;
        if let Some(i) = app.session_index(id) {
            // Screen enters the alternate screen, paints, then leaves it.  None
            // of that may send the session off the grid that has a scrollback.
            let mut out = Vec::new();
            out.extend_from_slice(b"\x1b[?1049h");
            for n in 0..100 {
                out.extend_from_slice(format!("hist {n}\r\n").as_bytes());
            }
            out.extend_from_slice(b"\x1b[?1049l");
            app.sessions[i].process(&out);
            assert!(
                !app.sessions[i].parser.screen().alternate_screen(),
                "a screen task must be kept off the alternate screen"
            );
        }
        let mut ui = Ui::default();
        for _ in 0..6 {
            handle_key(
                &mut app,
                &mut ui,
                KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
            );
        }
        let screen = render(&app, &mut ui);
        assert!(screen.contains("hist 1"), "past lines reachable:\n{screen}");
        assert!(!screen.contains("hist 99"), "newest line scrolled out:\n{screen}");
    }

    #[test]
    fn a_split_alternate_screen_sequence_is_still_dropped() {
        let mut app = test_app();
        let p = app.store.profiles()[0].clone();
        let id = screen_task(&mut app, &p);
        if let Some(i) = app.session_index(id) {
            // The enter sequence straddles two reads.
            app.sessions[i].process(b"\x1b[?10");
            app.sessions[i].process(b"49h");
            app.sessions[i].process(b"ok\r\n");
            assert!(!app.sessions[i].parser.screen().alternate_screen());
            assert!(app.sessions[i].parser.screen().contents().contains("ok"));
        }
    }

    #[test]
    fn a_screen_tasks_past_text_is_replayed_and_scrollable() {
        let mut app = test_app();
        let p = app.store.profiles()[0].clone();
        let id = screen_task(&mut app, &p);
        app.view = View::Terminal;
        // The task produced this long before this attach — on another machine,
        // or while nothing was watching.
        let mut past = String::new();
        for n in 0..80 {
            past.push_str(&format!("old {n}\n"));
        }
        app.apply(crate::app::Event::History {
            id,
            data: past.into_bytes(),
        });
        let mut ui = Ui::default();
        for _ in 0..6 {
            handle_key(
                &mut app,
                &mut ui,
                KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
            );
        }
        let screen = render(&app, &mut ui);
        assert!(screen.contains("old 1"), "past text reachable:\n{screen}");
        assert!(!screen.contains("old 79"), "newest past line scrolled out:\n{screen}");
    }

    #[test]
    fn function_keys_drive_the_view_state() {
        let mut app = test_app();
        let mut ui = Ui::default();
        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE),
        );
        assert_eq!(app.view, View::Help);
        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::F(9), KeyModifiers::NONE),
        );
        assert_eq!(app.view, View::Manager);
        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE),
        );
        assert!(app.cmd_focus);
        for c in "hel".chars() {
            handle_key(
                &mut app,
                &mut ui,
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            );
        }
        // Characters are held for an instant — that is what lets a dropped
        // file be told from typing — then they land in the bar.
        ui.pending_at = Some(Instant::now() - Duration::from_millis(100));
        flush_pending(&mut app, &mut ui, false);
        assert_eq!(app.cmd_input, "hel");
        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        );
        assert_eq!(app.cmd_input, "he");
        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        );
        assert!(!app.cmd_focus);
        assert!(app.cmd_input.is_empty());
        app.view = View::Terminal;
        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::F(10), KeyModifiers::NONE),
        );
        assert!(app.should_quit);
    }

    #[test]
    fn saving_the_form_leaves_the_cursor_on_what_was_saved() {
        let mut app = test_app();
        let mut ui = Ui::default();
        // Several profiles exist and the cursor is somewhere else entirely.
        app.store
            .upsert(crate::config::Profile::new("z99", "h", "u"));
        app.selected = 0;

        run_command(&mut app, "new 10.0.0.5");
        assert!(app.editor.is_some(), "the form should be open");
        app.editor.as_mut().unwrap().profile.user = "me".into();
        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
        );

        let saved = app
            .store
            .profiles()
            .last()
            .expect("a profile was saved")
            .name
            .clone();
        let row = app
            .manager_rows()
            .iter()
            .position(|r| matches!(r, Row::Profile(p) if p.name == saved));
        assert_eq!(
            Some(app.selected),
            row,
            "Enter must connect to {saved}, not to whatever was selected before"
        );
    }

    #[test]
    fn opening_a_saved_connection_always_makes_a_new_window() {
        let mut app = test_app();
        let p = app.store.profiles()[0].clone();
        open_profile(&mut app, &p, false);
        open_profile(&mut app, &p, false);
        assert_eq!(
            app.sessions.len(),
            2,
            "Enter never reuses or rebuilds — it opens another window"
        );
        assert_ne!(app.sessions[0].number, app.sessions[1].number);
    }

    #[test]
    fn enter_in_the_manager_opens_another_window_for_the_same_connection() {
        // Pressing Enter on a saved connection opens a window; going back to the
        // manager and pressing Enter again must open a *second* one, not jump to
        // the first.
        let mut app = test_app();
        let mut ui = Ui::default();
        app.view = View::Manager;
        app.selected = 0;
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let f9 = KeyEvent::new(KeyCode::F(9), KeyModifiers::NONE);

        handle_key(&mut app, &mut ui, enter);
        assert_eq!(app.sessions.len(), 1);
        handle_key(&mut app, &mut ui, f9); // back to the manager
        app.selected = 0;
        handle_key(&mut app, &mut ui, enter);
        assert_eq!(
            app.sessions.len(),
            2,
            "the same connection opens a second window"
        );
        assert_ne!(app.sessions[0].number, app.sessions[1].number);
    }

    #[test]
    fn the_manager_draws_a_divider_between_windows_and_tasks() {
        // A window (its own task) and a host task must be separated by a ruled
        // line, so the two groups never read as one list.
        let mut app = test_app();
        let p = app.store.profiles()[0].clone();
        let id = app.new_session(&p);
        app.apply(crate::app::Event::Opened {
            id,
            title: p.name.clone(),
            agent_ready: true,
            remote_id: Some("qf001".into()),
            fallback: None,
        });
        app.remote_sessions = vec![crate::app::RemoteSession {
            id: "qf001".into(),
            ..Default::default()
        }];
        app.view = View::Manager;
        let mut ui = Ui::default();
        let screen = render_sized(&app, &mut ui, 120, 50);
        assert!(
            screen.contains('─'),
            "a divider should separate windows from tasks:\n{screen}"
        );
    }

    #[test]
    fn q_in_the_manager_closes_a_window_or_ends_a_task() {
        let (mut app, mut rx) = app_with_sink();
        let p = app.store.profiles()[0].clone();
        let id = app.new_session(&p);
        app.remote_sessions = vec![crate::app::RemoteSession {
            id: "qf001".into(),
            ..Default::default()
        }];
        app.view = View::Manager;
        let mut ui = Ui::default();

        // `q` on the window row closes that window.
        app.selected = app
            .manager_rows()
            .iter()
            .position(|r| matches!(r, crate::app::Row::Window { .. }))
            .expect("a window row");
        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        );
        let mut closed = false;
        while let Ok(cmd) = rx.try_recv() {
            if matches!(cmd, Cmd::Close { id: cid } if cid == id) {
                closed = true;
            }
        }
        assert!(closed, "q on a window closes it: {}", app.notice);

        // `q` on the task row ends that task (no window holds it → KillRemote).
        app.selected = app
            .manager_rows()
            .iter()
            .position(|r| matches!(r, crate::app::Row::Remote { .. }))
            .expect("a task row");
        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        );
        let mut killed = false;
        while let Ok(cmd) = rx.try_recv() {
            if matches!(cmd, Cmd::KillRemote { .. }) {
                killed = true;
            }
        }
        assert!(killed, "q on a task ends it: {}", app.notice);
    }

    #[test]
    fn esc_and_the_view_keys_dismiss_the_ls_listing() {
        let mut app = test_app();
        let mut ui = Ui::default();

        run_command(&mut app, "ls");
        assert!(app.overlay.is_some(), "ls shows a listing");
        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        );
        assert!(app.overlay.is_none(), "Esc dismisses the listing");

        run_command(&mut app, "ls");
        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::F(9), KeyModifiers::NONE),
        );
        assert!(app.overlay.is_none(), "F9 dismisses the listing");
        assert_eq!(app.view, View::Manager, "and still switches to the manager");

        run_command(&mut app, "ls");
        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::F(3), KeyModifiers::NONE),
        );
        assert!(app.overlay.is_none(), "F3 dismisses the listing");
    }

    #[test]
    fn sessions_are_numbered_with_three_digits() {
        let mut app = test_app();
        let p = app.store.profiles()[0].clone();
        app.new_session(&p);
        app.new_session(&p);
        let numbers: Vec<String> = app
            .manager_rows()
            .iter()
            .filter_map(|r| match r {
                Row::Window { title, .. } => Some(title.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(numbers.len(), 2, "{numbers:?}");
        assert!(
            numbers
                .iter()
                .all(|n| n.len() == 3 && n.chars().all(|c| c.is_ascii_digit())),
            "{numbers:?}"
        );
        assert_ne!(numbers[0], numbers[1], "numbers must be distinct");
    }

    #[test]
    fn a_window_is_named_by_its_number_not_by_its_connection() {
        let mut app = test_app();
        let p = app.store.profiles()[0].clone();
        // Two windows from one saved connection: same description, different
        // names.  Nothing may show the connection name as the window's name.
        app.new_session(&p);
        app.new_session(&p);
        app.view = View::Terminal;
        app.active = Some(1);
        let mut ui = Ui::default();
        let screen = render_sized(&app, &mut ui, 100, 24);

        let name = app.sessions[1].name();
        let other = app.sessions[0].name();
        assert!(
            screen.contains(&name),
            "the window's number is its name:\n{screen}"
        );
        assert!(
            screen.contains(&other),
            "the other window keeps its name too:\n{screen}"
        );
        assert_ne!(name, other);
        // The connection is the description, beside the number in the tab and
        // the pane title — not the name of either window.
        assert!(screen.contains(&p.name), "{screen}");
        assert!(app.sessions[1].tab().contains(&name));
        assert!(app.sessions[1].tab().contains(&p.name));
    }

    #[test]
    fn a_three_digit_number_names_a_window_and_a_name_names_a_task() {
        let mut app = test_app();
        app.remote_sessions = vec![
            crate::app::RemoteSession {
                id: "qf001".into(),
                ..Default::default()
            },
            crate::app::RemoteSession {
                id: "qf002".into(),
                ..Default::default()
            },
        ];
        // A three digit number is always a window, never a task.
        assert_eq!(session_number("001"), Some(1));
        assert_eq!(session_number("002"), Some(2));
        assert_eq!(session_number("12"), None);
        assert_eq!(session_number("qf001"), None);
        // A task is addressed by its name (which is also its id).
        assert_eq!(named_task(&app, "qf002").as_deref(), Some("qf002"));
        assert_eq!(named_task(&app, "nope"), None);
    }

    #[test]
    fn tab_completes_saved_connection_names() {
        let mut app = test_app();
        app.store
            .upsert(crate::config::Profile::new("k07", "h", "u"));
        let _ui = Ui::default();
        app.cmd_input = "edit k0".into();
        request_completion(&mut app);
        assert_eq!(app.cmd_input, "edit k07");
    }

    #[test]
    fn tab_completes_session_ids() {
        let mut app = test_app();
        app.remote_sessions = vec![crate::app::RemoteSession {
            id: "web-77".into(),
            ..Default::default()
        }];
        let _ui = Ui::default();
        app.cmd_input = "quit web".into();
        request_completion(&mut app);
        assert_eq!(app.cmd_input, "quit web-77");
    }

    #[test]
    fn tab_completes_the_command_name() {
        let mut app = test_app();
        let _ui = Ui::default();

        // A half-typed verb completes against the command names, and a space is
        // left behind so the argument can follow right away.
        app.cmd_input = "se".into();
        request_completion(&mut app);
        assert_eq!(app.cmd_input, "setlang ");

        // A single match is completed in place.
        app.cmd_input = "c".into();
        request_completion(&mut app);
        assert_eq!(app.cmd_input, "cd ");

        // A whole command name is not completed again: Tab is now about its
        // argument, exactly as before.
        app.cmd_input = "setlang".into();
        request_completion(&mut app);
        assert_eq!(app.cmd_input, "setlang", "the verb itself is left alone");
    }

    #[test]
    fn tab_completes_windows_and_persistent_tasks() {
        let mut app = test_app();
        let p = app.store.profiles()[0].clone();
        app.new_session(&p);
        app.remote_sessions = vec![crate::app::RemoteSession {
            id: "qf001".into(),
            ..Default::default()
        }];
        let _ui = Ui::default();

        // A window is completed by its three digit number.
        let n = app.sessions[0].name();
        let prefix: String = n.chars().take(1).collect();
        app.cmd_input = format!("quit {prefix}");
        request_completion(&mut app);
        assert_eq!(app.cmd_input, format!("quit {n}"));

        // A persistent task is completed by its name.
        app.cmd_input = "quit q".into();
        request_completion(&mut app);
        assert_eq!(app.cmd_input, "quit qf001");

        // Bare `end` + Tab must offer the candidates, not match the verb.
        app.cmd_input = "quit".into();
        request_completion(&mut app);
        assert!(
            app.overlay.is_some(),
            "quit + Tab should list window/task candidates"
        );
        let listed = app.overlay.as_ref().map(|o| o.values.clone()).unwrap_or_default();
        assert!(listed.iter().any(|v| v == "qf001"), "{listed:?}");
    }

    #[test]
    fn only_anchored_arguments_are_taken_as_written() {
        let mut app = test_app();
        let tmp = std::env::temp_dir();
        app.cwd = tmp.clone();

        // A bare name — with or without spaces — comes from where we are.
        assert_eq!(
            resolve_local(&app, "amorphous.pdf"),
            tmp.join("amorphous.pdf")
        );
        assert_eq!(
            resolve_local(&app, "amorphous report.pdf"),
            tmp.join("amorphous report.pdf")
        );
        assert_eq!(resolve_local(&app, "./x"), tmp.join("x"));
        assert_eq!(resolve_local(&app, "../x"), tmp.parent().unwrap().join("x"));

        // Anchored forms are not taken relative to the current directory.
        let rooted = resolve_local(&app, "/etc/hosts");
        assert!(
            !rooted.starts_with(&tmp),
            "a leading / must not resolve against the current directory: {rooted:?}"
        );
        assert!(
            rooted
                .to_string_lossy()
                .replace('\\', "/")
                .ends_with("etc/hosts"),
            "{rooted:?}"
        );
        assert_eq!(resolve_local(&app, "~"), crate::config::desktop_dir());
        assert_eq!(
            resolve_local(&app, "~/x.pdf"),
            crate::config::desktop_dir().join("x.pdf")
        );
        assert!(has_drive_prefix(r"C:\x") && has_drive_prefix("c:/x"));
        assert!(is_unc(r"\\server\share"));
        assert!(!has_drive_prefix("amorphous.pdf"));
    }

    #[test]
    fn cd_moves_the_local_working_directory_and_tilde_resets_it() {
        let mut app = test_app();
        let dir = std::env::temp_dir();
        run_command(&mut app, &format!("cd {}", dir.display()));
        assert_eq!(app.cwd, dir, "cd must move the temporary working directory");

        // `~` (and a bare `cd`) is the desktop — the root the browser starts at,
        // not `%USERPROFILE%`.
        run_command(&mut app, "cd ~");
        assert_eq!(app.cwd, crate::config::desktop_dir());
        run_command(&mut app, &format!("cd {}", dir.display()));
        run_command(&mut app, "cd");
        assert_eq!(app.cwd, crate::config::desktop_dir());
    }

    #[test]
    fn downloads_land_in_the_working_directory_unless_the_connection_names_one() {
        let mut app = test_app();
        let dir = std::env::temp_dir().join("tabssh-get-dest");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let profile = app.store.profiles()[0].clone();
        app.new_session(&profile);

        // Blank connection setting: the command bar's directory wins, so `get`
        // follows `cd` just as `put` does.
        app.cwd = dir.clone();
        assert_eq!(download_target(&app), dir);

        // A directory set on the connection still takes precedence.
        app.sessions[0].profile.download_dir = Some(dir.join("explicit").display().to_string());
        assert_eq!(download_target(&app), dir.join("explicit"));
    }

    #[test]
    fn a_name_with_spaces_is_taken_as_one_file() {
        let mut app = test_app();
        let dir = std::env::temp_dir().join("tabssh-put-space");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("amorphous report.pdf"), b"x").unwrap();
        app.cwd = dir.clone();
        let profile = app.store.profiles()[0].clone();
        app.new_session(&profile);

        // No quotes anywhere — the client works it out.
        run_command(&mut app, "put amorphous report.pdf");
        assert!(
            !app.notice.contains("no such local path"),
            "it should have found the file: {}",
            app.notice
        );
        assert!(app.notice.contains("uploading"), "{}", app.notice);

        // Two real files still work as two.
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        std::fs::write(dir.join("b.txt"), b"x").unwrap();
        run_command(&mut app, "put a.txt b.txt");
        assert!(app.notice.contains("2 item"), "{}", app.notice);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dropped_path_is_recognised_instead_of_being_typed() {
        let mut app = test_app();
        let p = app.store.profiles()[0].clone();
        app.new_session(&p);
        app.view = View::Terminal;
        // Path capture belongs to the command bar, so open it first.
        app.cmd_focus = true;

        let dir = std::env::temp_dir().join("tabssh-drop-burst");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("dropped file.txt");
        std::fs::write(&file, b"x").unwrap();

        let mut ui = Ui::default();
        // A dragged file arrives as a burst of characters, not as a paste.
        for c in file.to_string_lossy().chars() {
            handle_key(
                &mut app,
                &mut ui,
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            );
        }
        assert!(!ui.pending.is_empty(), "the burst should be held");
        ui.pending_at = Some(Instant::now() - Duration::from_millis(100));
        flush_pending(&mut app, &mut ui, false);

        assert!(
            app.cmd_input.starts_with("put ") && app.cmd_input.contains("dropped file.txt"),
            "the burst becomes a command to run: {}",
            app.cmd_input
        );
        assert!(app.cmd_focus, "with the bar focused");
        assert!(ui.pending.is_empty(), "the burst was consumed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ordinary_typing_is_forwarded_rather_than_eaten() {
        let (mut app, mut rx) = app_with_sink();
        let p = app.store.profiles()[0].clone();
        app.new_session(&p);
        app.view = View::Terminal;

        let mut ui = Ui::default();
        type_keys(&mut app, &mut ui, "ls -la");

        // A terminal is a terminal: the characters go out at once, with nothing
        // held back and no path inspection.
        assert!(ui.pending.is_empty(), "typing is not held in a terminal");
        assert!(
            !app.notice.contains("uploading"),
            "typing must not be mistaken for a drop: {}",
            app.notice
        );
        assert_eq!(forwarded(&mut rx), b"ls -la");
    }

    #[test]
    fn typing_a_slash_in_a_terminal_does_not_open_the_command_bar() {
        let (mut app, mut rx) = app_with_sink();
        let p = app.store.profiles()[0].clone();
        app.new_session(&p);
        app.view = View::Terminal;
        let mut ui = Ui::default();

        // `/` and `.` both name real local paths, but only the command bar may
        // read a burst as a drop.  In a terminal they are ordinary typing and
        // must reach the remote shell, exactly as a plain terminal would.
        type_keys(&mut app, &mut ui, "/");
        type_keys(&mut app, &mut ui, ".");
        type_keys(&mut app, &mut ui, "cd /tmp");

        assert_eq!(forwarded(&mut rx), b"/.cd /tmp");
        assert!(!app.cmd_focus, "the command bar must stay closed");
        assert!(app.overlay.is_none(), "no overlay should have opened");
        assert!(app.cmd_input.is_empty(), "the bar should be untouched");
        assert!(ui.pending.is_empty(), "nothing is held in a terminal");
    }

    #[test]
    fn the_status_line_shows_where_you_are_and_the_last_rate() {
        let mut app = test_app();
        app.cwd = std::path::PathBuf::from("/somewhere/on/disk");
        app.last_transfer = Some("2.0 MiB/s · 1.0s".into());
        app.notice = "done".into();
        let mut ui = Ui::default();
        let screen = render_sized(&app, &mut ui, 100, 10);
        assert!(
            screen.contains("somewhere") && screen.contains("disk"),
            "the local directory should be shown: {screen}"
        );
        assert!(screen.contains("2.0 MiB/s"), "{screen}");
        assert!(screen.contains("done"), "{screen}");
    }

    /// A window only counts as live once the controller says so; this is what
    /// it says when a session really is up.
    fn mark_live(app: &mut App, id: SessionId, agent: bool) {
        let title = app
            .session_index(id)
            .map(|i| app.sessions[i].title.clone())
            .unwrap_or_default();
        app.apply(crate::app::Event::Opened {
            id,
            title,
            agent_ready: agent,
            remote_id: Some("host-1".into()),
            fallback: None,
        });
    }

    #[test]
    fn bare_end_closes_the_current_window() {
        // `end` with nothing named acts on the window you are looking at.
        let (mut app, mut rx) = app_with_sink();
        let p = app.store.profiles()[0].clone();
        let id = app.new_session(&p);
        app.active = Some(0);
        run_command(&mut app, "quit");
        let mut closed = false;
        while let Ok(cmd) = rx.try_recv() {
            if let Cmd::Close { id: cid } = cmd {
                assert_eq!(cid, id);
                closed = true;
            }
        }
        assert!(
            closed,
            "quit with no argument closes the current window: {}",
            app.notice
        );
    }

    #[test]
    fn tab_completion_asks_the_host_once_the_session_is_live() {
        let mut app = test_app();
        let p = app.store.profiles()[0].clone();
        let id = app.new_session(&p);
        let _ui = Ui::default();

        // Before it is live there is nothing to ask.
        app.cmd_input = "get /e".into();
        request_completion(&mut app);
        assert!(!app.completing, "no live session, no request");

        mark_live(&mut app, id, true);
        app.cmd_input = "get /e".into();
        request_completion(&mut app);
        assert!(app.completing, "the host should have been asked");
    }

    #[test]
    fn a_late_completion_for_a_line_that_moved_on_is_dropped() {
        use crate::app::Event;
        let mut app = test_app();
        let p = app.store.profiles()[0].clone();
        let id = app.new_session(&p);
        mark_live(&mut app, id, true);
        app.cmd_focus = true;
        app.cmd_input = "get /e".into();
        request_completion(&mut app);
        assert!(app.completing);

        // The user submitted the line before the answer came back.
        app.cmd_input.clear();
        app.apply(Event::Completions(Completion {
            kind: CompletionKind::Remote,
            word: "/e".into(),
            candidates: vec!["/etc/hosts".into()],
        }));
        assert!(!app.completing);
        assert_eq!(app.cmd_input, "", "nothing may be typed into the new line");
    }

    #[test]
    fn ctrl_c_clears_the_command_bar_then_closes_it() {
        let mut app = test_app();
        let mut ui = Ui::default();
        app.cmd_focus = true;
        app.cmd_input = "get /etc/hosts".into();

        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        handle_key(&mut app, &mut ui, ctrl_c);
        assert!(app.cmd_input.is_empty(), "the line is abandoned");
        assert!(app.cmd_focus, "still open so you can type another");

        // A second press, with nothing to abandon, closes the bar.
        handle_key(&mut app, &mut ui, ctrl_c);
        assert!(!app.cmd_focus, "the second press closes it");
    }

    #[test]
    fn a_file_dropped_on_the_bar_is_uploaded_not_typed() {
        let mut app = test_app();
        let p = app.store.profiles()[0].clone();
        app.new_session(&p);
        let _ui = Ui::default();

        let dir = std::env::temp_dir().join("tabssh-drop-bar");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("dropped.pdf");
        std::fs::write(&file, b"x").unwrap();

        app.cmd_focus = true;
        let consumed = handle_paste(&mut app, &file.to_string_lossy());
        assert!(consumed);
        assert!(
            app.cmd_input.starts_with("put "),
            "a drop fills in the upload command: {}",
            app.cmd_input
        );
        assert!(app.cmd_input.contains("dropped.pdf"), "{}", app.cmd_input);
        assert!(app.cmd_focus, "and leaves the bar focused to run it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_dropped_on_the_focused_bar_uploads_instead() {
        let mut app = test_app();
        let p = app.store.profiles()[0].clone();
        app.new_session(&p);
        let mut ui = Ui::default();
        app.cmd_focus = true;

        let dir = std::env::temp_dir().join("tabssh-drop-focus");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("dropped on bar.pdf");
        std::fs::write(&file, b"x").unwrap();

        // A drop arrives as a burst of keystrokes, even with the bar focused.
        for c in file.to_string_lossy().chars() {
            handle_key(
                &mut app,
                &mut ui,
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            );
        }
        ui.pending_at = Some(Instant::now() - Duration::from_millis(100));
        flush_pending(&mut app, &mut ui, false);

        assert!(
            app.cmd_input.starts_with("put "),
            "a burst that names a file fills in the command: {}",
            app.cmd_input
        );
        assert!(
            app.cmd_input.contains("dropped on bar.pdf"),
            "{}",
            app.cmd_input
        );
        assert!(app.cmd_focus, "and leaves the bar focused to run it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn enter_runs_the_command_even_though_the_typing_was_held() {
        let mut app = test_app();
        let mut ui = Ui::default();
        app.cmd_focus = true;
        // Start somewhere else so the command's effect is visible.
        app.view = View::Terminal;

        for c in "ls".chars() {
            handle_key(
                &mut app,
                &mut ui,
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            );
        }
        assert!(!ui.pending.is_empty(), "the typing is still in hand");

        // Enter must submit what is held, not race past it into an empty bar.
        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );

        assert!(app.overlay.is_some(), "`ls` should have run");
        assert!(!app.cmd_focus, "and the bar should have closed");
        assert!(app.cmd_input.is_empty());
    }

    #[test]
    fn terminal_typing_reaches_the_shell_before_enter() {
        let (mut app, mut rx) = app_with_sink();
        let p = app.store.profiles()[0].clone();
        app.new_session(&p);
        app.view = View::Terminal;
        let mut ui = Ui::default();

        type_keys(&mut app, &mut ui, "echo hi");
        // Nothing is held in a terminal, so Enter cannot overtake the command:
        // the bytes arrive at the shell in the order they were typed.
        assert!(ui.pending.is_empty());

        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert_eq!(forwarded(&mut rx), b"echo hi\r");
    }

    #[test]
    fn completion_helpers_behave() {
        let items = vec![
            "src/a.rs".to_string(),
            "src/b.rs".to_string(),
            "src/ab.rs".to_string(),
        ];
        assert_eq!(common_prefix(&items), "src/");
        assert_eq!(common_prefix(&["/x".to_string()]), "/x");
        assert_eq!(common_prefix(&[]), "");

        assert_eq!(replace_last_word("get src/a", "src/ab.rs"), "get src/ab.rs");
        assert_eq!(replace_last_word("get ", "/etc/passwd"), "get /etc/passwd");
        assert_eq!(replace_last_word("put", "x"), "x");
        // A full-width space is whitespace too, and three bytes wide: the cut
        // must land on its boundary, not in its middle.
        assert_eq!(replace_last_word("put　文件", "文件.txt"), "put　文件.txt");
    }

    #[test]
    fn a_single_match_is_inserted() {
        let mut app = test_app();
        let _ui = Ui::default();
        app.cmd_input = "get /etc/hos".into();
        apply_completion(
            &mut app,
            Completion {
                kind: CompletionKind::Remote,
                word: "/etc/hos".into(),
                candidates: vec!["/etc/hostname".into()],
            },
        );
        assert_eq!(app.cmd_input, "get /etc/hostname");
    }

    #[test]
    fn several_matches_are_listed_and_extend_the_prefix() {
        let mut app = test_app();
        let _ui = Ui::default();
        app.cmd_input = "get /var/l".into();
        apply_completion(
            &mut app,
            Completion {
                kind: CompletionKind::Remote,
                word: "/var/l".into(),
                candidates: vec!["/var/lib/".into(), "/var/local/".into(), "/var/log/".into()],
            },
        );
        // "/var/l" already covers the common prefix, so the word is unchanged…
        assert!(app.cmd_input.starts_with("get /var/l"), "{}", app.cmd_input);
        // …and every candidate is listed by name, the way a shell does.
        let o = app
            .overlay
            .as_ref()
            .expect("a completion list should be up");
        assert_eq!(o.items.len(), 3, "{:?}", o.items);
        // Names only — a trailing slash marks a directory, nothing more.
        assert!(
            o.items
                .iter()
                .all(|i| !i.trim_end_matches('/').contains('/')),
            "names only: {:?}",
            o.items
        );
        assert_eq!(o.values.len(), 3, "the values keep the full paths");
    }

    #[test]
    fn an_empty_result_is_reported_not_silent() {
        let mut app = test_app();
        let _ui = Ui::default();
        app.cmd_input = "put /nope/zzz".into();
        apply_completion(
            &mut app,
            Completion {
                kind: CompletionKind::Local,
                word: "/nope/zzz".into(),
                candidates: Vec::new(),
            },
        );
        assert!(app.notice.contains("nope"), "{}", app.notice);
    }

    #[test]
    fn unknown_command_is_reported_not_fatal() {
        let mut app = test_app();
        run_command(&mut app, "definitely-not-a-command");
        assert!(app.notice.contains("unknown command"), "{}", app.notice);
    }

    #[test]
    fn closing_a_window_removes_it_instead_of_leaving_a_dead_row() {
        use crate::app::Event;
        let mut app = test_app();
        let p = app.store.profiles()[0].clone();
        let id = app.new_session(&p);
        assert_eq!(app.sessions.len(), 1);
        app.view = View::Terminal;

        app.apply(Event::Closed {
            id,
            reason: "closed and terminated".into(),
        });

        assert!(app.sessions.is_empty(), "the window must be gone");
        assert!(app.active.is_none(), "nothing is selected any more");
        assert_eq!(app.view, View::Manager, "fall back to the manager");
    }

    #[test]
    fn closing_one_of_several_windows_keeps_the_selection_sensible() {
        use crate::app::Event;
        let mut app = test_app();
        let p = app.store.profiles()[0].clone();
        let a = app.new_session(&p);
        let b = app.new_session(&p);
        let c = app.new_session(&p);

        // Remove the first one while the last is active.
        app.active = Some(2);
        app.apply(Event::Closed {
            id: a,
            reason: "closed".into(),
        });
        assert_eq!(app.sessions.len(), 2);
        assert_eq!(app.active, Some(1), "the active window shifts down");

        // Remove the active middle one: the neighbour takes over.
        app.apply(Event::Closed {
            id: b,
            reason: "closed".into(),
        });
        assert_eq!(app.sessions.len(), 1);
        assert_eq!(app.active, Some(0));
        assert_eq!(app.active_id(), Some(c));
    }

    #[test]
    fn a_window_still_connecting_can_be_removed_too() {
        use crate::app::Event;
        let mut app = test_app();
        let p = app.store.profiles()[0].clone();
        let id = app.new_session(&p);
        // This is what the controller sends when a connect fails or is
        // cancelled: the window has no backend, but it must still clean up.
        app.apply(Event::Closed {
            id,
            reason: "could not connect: connection refused".into(),
        });
        assert!(app.sessions.is_empty());
        assert!(app.notice.contains("could not connect"), "{}", app.notice);
    }

    #[test]
    fn the_command_bar_dispatches_commands_with_or_without_a_prefix() {
        let mut app = test_app();
        let p = app.store.profiles()[0].clone();
        app.new_session(&p);

        // The old `tabssh:` prefix is still accepted, so existing notes and
        // scripts keep working.
        app.view = View::Terminal;
        run_command(&mut app, "setlang");
        assert!(app.notice.contains("language"), "{}", app.notice);
        run_command(&mut app, "tabssh:setlang");
        assert!(app.notice.contains("language"), "{}", app.notice);

        // A name that is not anything is reported, not swallowed.
        run_command(&mut app, "quit no-such-thing");
        assert!(app.notice.contains("nothing called"), "{}", app.notice);

        // Naming a saved connection forgets it.
        let name = app.store.profiles().last().unwrap().name.clone();
        run_command(&mut app, &format!("quit {name}"));
        assert!(app.notice.contains("forgot"), "{}", app.notice);
        assert!(
            app.store.find(&name).is_none(),
            "the connection should be gone"
        );
    }

    #[test]
    fn the_bar_no_longer_knows_windows_close_or_detach() {
        // These three were dropped from the command bar (and the F1 page); the
        // manager keeps its own `D` (detach) shortcut.
        for verb in ["windows", "close", "detach"] {
            assert!(
                matches!(parse_command(verb), Command::Unknown(_)),
                "`{verb}` should be gone from the command bar"
            );
        }
    }

    #[test]
    fn end_closes_the_window_named_by_its_number() {
        let mut app = test_app();
        let p = app.store.profiles()[0].clone();
        let id = app.new_session(&p);
        let number = format!("{:03}", app.sessions[0].number);
        run_command(&mut app, &format!("quit {number}"));
        assert!(app.notice.contains("closing window"), "{}", app.notice);
        assert_eq!(
            app.session_index(id),
            Some(0),
            "the window is still there until it closes"
        );
    }

    #[test]
    fn end_closes_a_window_but_ending_its_task_resets_it() {
        use crate::app::Event;
        // `end <window>` closes the window and leaves its task running.
        // `end <task>`, with a window attached, ends the task and resets that
        // window in place instead of removing it.
        let (mut app, mut rx) = app_with_sink();
        let p = app.store.profiles()[0].clone();
        let id = app.new_session(&p);
        app.apply(Event::Opened {
            id,
            title: p.name.clone(),
            agent_ready: true,
            remote_id: Some("qf001".into()),
            fallback: None,
        });
        let number = format!("{:03}", app.sessions[0].number);

        run_command(&mut app, &format!("quit {number}"));
        let mut close = false;
        let mut reset = false;
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                Cmd::Close { .. } => close = true,
                Cmd::Reset { .. } => reset = true,
                _ => {}
            }
        }
        assert!(close && !reset, "quit <window> closes, it does not reset");

        run_command(&mut app, "quit qf001");
        let mut reset = false;
        while let Ok(cmd) = rx.try_recv() {
            if let Cmd::Reset { id: rid } = cmd {
                assert_eq!(rid, id);
                reset = true;
            }
        }
        assert!(
            reset,
            "quit <task> resets the window that holds it: {}",
            app.notice
        );
        assert_eq!(
            app.sessions.len(),
            1,
            "the window stays while its task is replaced"
        );
    }

    #[test]
    fn new_connection_opens_the_form_and_ctrl_s_saves_it() {
        let mut app = test_app();
        let mut ui = Ui::default();
        run_command(&mut app, "new box root@10.0.0.9:2200");
        let ed = app.editor.as_ref().expect("the settings form should open");
        assert_eq!(ed.profile.host, "10.0.0.9");
        assert_eq!(ed.profile.port, 2200);
        assert_eq!(ed.profile.user, "root");
        assert_eq!(ed.profile.name, "box");
        assert_eq!(app.view, View::Editor);
        assert!(app.store.find("box").is_none(), "not saved until Ctrl-S");

        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
        );
        assert!(app.editor.is_none(), "the form should close after saving");
        let p = app.store.find("box").expect("profile saved");
        assert_eq!(p.host, "10.0.0.9");
        assert_eq!(p.port, 2200);
    }

    #[test]
    fn the_form_edits_every_field_group() {
        let mut app = test_app();
        let p = app.store.profiles()[0].clone();
        open_editor(&mut app, p, false);
        let mut ui = Ui::default();

        // Walk down to the port field and change it.
        app.editor.as_mut().unwrap().field = 2;
        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(app.editor.as_ref().unwrap().editing);
        for c in ['2', '2', '2', '2'] {
            handle_key(
                &mut app,
                &mut ui,
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            );
        }
        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert_eq!(app.editor.as_ref().unwrap().profile.port, 2222);

        // A bad number is rejected without leaving edit mode.
        let ed = app.editor.as_mut().unwrap();
        ed.field = 2; // port
        ed.buf = "abc".into();
        ed.editing = true;
        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(
            app.editor.as_ref().unwrap().editing,
            "bad input stays editable"
        );
        assert!(app.notice.contains("port"), "{}", app.notice);

        // The note is an ordinary text field.
        let ed = app.editor.as_mut().unwrap();
        ed.editing = false;
        ed.field = 9; // note
        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        for c in ['h', 'i'] {
            handle_key(
                &mut app,
                &mut ui,
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            );
        }
        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert_eq!(app.editor.as_ref().unwrap().profile.note, "hi");

        // The screen status row is shown, never edited.
        let ed = app.editor.as_mut().unwrap();
        ed.editing = false;
        ed.field = 8; // screen status
        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(
            !app.editor.as_ref().unwrap().editing,
            "the status row is read-only"
        );
    }

    #[test]
    fn the_form_renders_every_settings_group() {
        let mut app = test_app();
        let p = app.store.profiles()[0].clone();
        open_editor(&mut app, p, false);
        let mut ui = Ui::default();
        // A tall viewport so every group is visible at once.
        let screen = render_sized(&app, &mut ui, 120, 50);
        for needle in [
            "network",
            "connection",
            "transfer",
            "behaviour",
            "upload dir",
            "download dir",
            "screen status",
        ] {
            assert!(screen.contains(needle), "missing {needle} in:\n{screen}");
        }
    }

    #[test]
    fn the_form_scrolls_to_keep_the_selection_visible() {
        let mut app = test_app();
        let p = app.store.profiles()[0].clone();
        open_editor(&mut app, p, false);
        let mut ui = Ui::default();
        // A short terminal: the last field can only be reached by scrolling.
        app.editor.as_mut().unwrap().field = fields().len() - 1;
        let screen = render_sized(&app, &mut ui, 100, 18);
        assert!(
            screen.contains(fields()[fields().len() - 1].label),
            "the selected field should be scrolled into view:\n{screen}"
        );
        // ... and the first field is then off screen.
        assert!(!screen.contains("  host "), "{screen}");
    }

    #[test]
    fn the_manager_scrolls_with_the_selection() {
        let mut app = test_app();
        for i in 0..30 {
            app.store
                .upsert(crate::config::Profile::new(&format!("p{i:02}"), "h", "u"));
        }
        app.selected = 25;
        let expected = app.store.profiles()[app.selected].name.clone();
        let mut ui = Ui::default();
        let screen = render_sized(&app, &mut ui, 100, 20);
        assert!(
            screen.contains(&expected),
            "selection {expected} should be visible:\n{screen}"
        );
    }

    #[test]
    fn esc_discards_the_form() {
        let mut app = test_app();
        let p = app.store.profiles()[0].clone();
        open_editor(&mut app, p, false);
        let mut ui = Ui::default();
        app.editor.as_mut().unwrap().profile.host = "changed.invalid".into();
        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        );
        assert!(app.editor.is_none());
        assert_eq!(app.store.profiles()[0].host, "example.invalid");
    }

    #[test]
    fn the_form_refuses_an_incomplete_profile() {
        let mut app = test_app();
        let mut ui = Ui::default();
        run_command(&mut app, "new");
        assert!(app.editor.is_some());
        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
        );
        assert!(app.editor.is_some(), "an empty profile must not be saved");
        assert!(app.notice.contains("cannot save"), "{}", app.notice);
    }

    #[test]
    fn dropping_a_local_path_triggers_an_upload() {
        let mut app = test_app();
        let p = app.store.profiles()[0].clone();
        app.new_session(&p);
        let dir = std::env::temp_dir().join("tabssh-drop-test");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("dropped.txt");
        std::fs::write(&f, b"x").unwrap();
        // Only the bar interprets a paste; with it closed a paste is passed
        // straight through to the caller.
        assert!(!handle_paste(&mut app, &f.to_string_lossy()));

        app.cmd_focus = true;
        let consumed = handle_paste(&mut app, &f.to_string_lossy());
        assert!(
            consumed,
            "a dropped path must be consumed as an upload offer"
        );
        assert!(
            app.cmd_input.starts_with("put ") && app.cmd_input.contains("dropped.txt"),
            "the drop is turned into a command for you to run: {}",
            app.cmd_input
        );
        assert!(app.cmd_focus, "with the bar focused so Enter runs it");
        // Once the bar is closed, ordinary text is forwarded, not swallowed.
        app.cmd_focus = false;
        assert!(!handle_paste(&mut app, "ls -la\n"));
    }

    #[test]
    fn paste_paths_ignores_non_paths_and_quotes() {
        let dir = std::env::temp_dir().join("tabssh-drop-test2");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a b.txt");
        std::fs::write(&f, b"x").unwrap();
        let text = format!("\"{}\"\nnot-a-real-path-xyz\n", f.display());
        let got = paste_paths(&text);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0], f);
    }

    #[test]
    fn pasting_into_an_open_field_lands_in_the_field() {
        let mut app = test_app();
        let p = app.store.profiles()[0].clone();
        open_editor(&mut app, p, false);
        // Put the cursor on the key field and start editing it.
        app.editor.as_mut().unwrap().field = 4;
        app.editor.as_mut().unwrap().editing = true;
        app.editor.as_mut().unwrap().buf.clear();

        let key = "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----";
        assert!(handle_paste(&mut app, key), "the field must take the paste");
        assert!(app.editor.as_ref().unwrap().buf.contains("BEGIN OPENSSH"));
    }

    #[test]
    fn a_drop_with_spaces_or_several_files_uses_the_exact_paths() {
        let (mut app, mut rx) = app_with_sink();
        let p = app.store.profiles()[0].clone();
        app.new_session(&p);
        let dir = std::env::temp_dir().join("tabssh-drop-exact");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("first file.txt");
        let b = dir.join("second file.txt");
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&b, b"x").unwrap();

        app.cmd_focus = true;
        // A Windows drop quotes each path and separates them with a space.
        let text = format!("\"{}\" \"{}\"", a.display(), b.display());
        assert!(handle_paste(&mut app, &text));
        assert!(app.cmd_input.starts_with("put "), "{}", app.cmd_input);
        assert!(!app.cmd_input.contains('"'), "no quotes: {}", app.cmd_input);

        // Running the line the drop produced uploads exactly those two files,
        // spaces and all.
        let line = app.cmd_input.clone();
        run_command(&mut app, &line);
        let mut locals = None;
        while let Ok(cmd) = rx.try_recv() {
            if let Cmd::Upload { locals: l, .. } = cmd {
                locals = Some(l);
            }
        }
        let locals = locals.expect("an upload was sent");
        assert_eq!(locals.len(), 2, "{locals:?}");
        assert!(locals.contains(&a) && locals.contains(&b), "{locals:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn typing_dot_slash_and_tilde_are_never_captured_as_a_drop() {
        let mut app = test_app();
        let p = app.store.profiles()[0].clone();
        app.new_session(&p);
        app.view = View::Terminal;
        app.cmd_focus = true;
        app.cmd_input = "ls ".into();
        let mut ui = Ui::default();

        // `.`, `/` and `~` each name a real local path, but on their own they
        // are ordinary typing: they must be appended, never turn into a `put`
        // offer that wipes the line.
        for c in ['.', '/', '~'] {
            handle_key(
                &mut app,
                &mut ui,
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            );
            ui.pending_at = Some(Instant::now() - Duration::from_millis(100));
            flush_pending(&mut app, &mut ui, false);
        }
        assert_eq!(
            app.cmd_input, "ls ./~",
            "the line must survive: {}",
            app.cmd_input
        );
        assert!(!app.cmd_input.starts_with("put"), "{}", app.cmd_input);
        assert!(app.cmd_focus);
    }

    #[test]
    fn a_drop_is_offered_but_never_runs_itself() {
        let (mut app, mut rx) = app_with_sink();
        let p = app.store.profiles()[0].clone();
        app.new_session(&p);
        app.view = View::Terminal;
        app.cmd_focus = true;

        let dir = std::env::temp_dir().join("tabssh-drop-noauto");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("dropped.txt");
        std::fs::write(&file, b"x").unwrap();

        let mut ui = Ui::default();
        for c in file.to_string_lossy().chars() {
            handle_key(
                &mut app,
                &mut ui,
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            );
        }
        // A dragged file arrives trailed by a newline; that Enter fills the
        // command in and stops there.
        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(app.cmd_focus, "the bar is filled in, not run");
        assert!(app.cmd_input.starts_with("put "), "{}", app.cmd_input);
        assert!(
            !app.cmd_input.contains('"'),
            "no quotes are added: {}",
            app.cmd_input
        );
        assert!(rx.try_recv().is_err(), "nothing may be uploaded yet");

        // The user's own Enter is what runs it.
        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        let mut uploaded = false;
        while let Ok(cmd) = rx.try_recv() {
            if let Cmd::Upload { .. } = cmd {
                uploaded = true;
            }
        }
        assert!(uploaded, "the user's Enter should upload");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reattaching_to_an_open_session_switches_instead_of_duplicating() {
        let mut app = test_app();
        let p = app.store.profiles()[0].clone();
        let id = app.new_session(&p);
        mark_live(&mut app, id, true);
        app.view = View::Manager;

        reattach(&mut app, "host-1");
        assert_eq!(
            app.sessions.len(),
            1,
            "no second window onto the same session"
        );
        assert_eq!(app.active, Some(0));
        assert_eq!(app.view, View::Terminal);
        assert!(app.notice.contains("switched"), "{}", app.notice);
    }

    #[test]
    fn reattaching_a_listed_screen_task_opens_a_window() {
        // Every host session is a screen task now, so Enter on a listed row
        // opens a window into it (through the Attach command).
        let (mut app, mut rx) = app_with_sink();
        let p = app.store.profiles()[0].clone();
        app.remote_host = p.host.clone();
        app.remote_sessions = vec![crate::app::RemoteSession {
            id: "sc-1".into(),
            shell: "screen".into(),
            pid: 4321,
            ..Default::default()
        }];

        reattach(&mut app, "sc-1");
        assert_eq!(app.sessions.len(), 1, "a new window is created");
        assert_eq!(app.view, View::Terminal);
        let mut attached = false;
        while let Ok(cmd) = rx.try_recv() {
            if let Cmd::Attach { remote_id, pid, .. } = cmd {
                assert_eq!(remote_id, "sc-1");
                assert_eq!(pid, 4321, "the pid disambiguates a duplicated name");
                attached = true;
            }
        }
        assert!(attached, "a listed screen task is attached to");
    }

    #[test]
    fn a_connect_that_finishes_late_does_not_steal_focus() {
        use crate::app::Event;
        let mut app = test_app();
        let p = app.store.profiles()[0].clone();
        let a = app.new_session(&p);
        let b = app.new_session(&p);
        // The user has moved to the second window while the first connects.
        app.active = Some(1);
        app.view = View::Terminal;

        app.apply(Event::Opened {
            id: a,
            title: p.name.clone(),
            agent_ready: true,
            remote_id: Some("r-a".into()),
            fallback: None,
        });

        assert_eq!(
            app.active_id(),
            Some(b),
            "focus must stay where the user put it"
        );
        assert_eq!(app.view, View::Terminal);
        assert_eq!(
            app.sessions[0].remote_id.as_deref(),
            Some("r-a"),
            "but the connection is still recorded on its own window"
        );
    }

    #[test]
    fn a_connect_that_finishes_while_the_manager_is_open_keeps_the_view() {
        use crate::app::Event;
        let mut app = test_app();
        let p = app.store.profiles()[0].clone();
        let id = app.new_session(&p);
        // The user went back to the manager while the window connects.
        app.view = View::Manager;

        app.apply(Event::Opened {
            id,
            title: p.name.clone(),
            agent_ready: true,
            remote_id: Some("r-1".into()),
            fallback: None,
        });

        assert_eq!(app.view, View::Manager, "the view must not be stolen");
        assert!(app.sessions[0].live, "the window itself is still recorded");
    }

    #[test]
    fn a_late_open_for_a_closed_window_is_dropped_entirely() {
        use crate::app::Event;
        let mut app = test_app();
        let p = app.store.profiles()[0].clone();
        let id = app.new_session(&p);
        app.view = View::Terminal;
        // The window is closed while it is still connecting.
        app.apply(Event::Closed {
            id,
            reason: "cancelled".into(),
        });
        assert!(app.sessions.is_empty());

        // The background connect finishes afterwards and reports Opened.
        app.apply(Event::Opened {
            id,
            title: p.name.clone(),
            agent_ready: true,
            remote_id: Some("x".into()),
            fallback: None,
        });
        assert!(app.sessions.is_empty(), "no window may be resurrected");
        assert!(app.active.is_none());
        assert_eq!(
            app.view,
            View::Manager,
            "and it must not switch to a terminal"
        );
    }

    #[test]
    fn each_window_keeps_its_own_remote_session_association() {
        use crate::app::Event;
        let mut app = test_app();
        let p = app.store.profiles()[0].clone();
        let a = app.new_session(&p);
        let b = app.new_session(&p);
        for (id, rid) in [(a, "demo-1"), (b, "demo-2")] {
            app.apply(Event::Opened {
                id,
                title: p.name.clone(),
                agent_ready: true,
                remote_id: Some(rid.into()),
                fallback: None,
            });
        }
        assert_eq!(app.sessions[0].remote_id.as_deref(), Some("demo-1"));
        assert_eq!(app.sessions[1].remote_id.as_deref(), Some("demo-2"));

        // Reattaching to the second switches to *it*, never a third window.
        reattach(&mut app, "demo-2");
        assert_eq!(app.sessions.len(), 2);
        assert_eq!(app.active, Some(1));
        assert!(app.sessions[1].tab().contains(&app.sessions[1].name()));
    }

    /// Every `Close` id the app has sent so far.
    fn closed_ids(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Cmd>) -> Vec<SessionId> {
        let mut out = Vec::new();
        while let Ok(cmd) = rx.try_recv() {
            if let Cmd::Close { id } = cmd {
                out.push(id);
            }
        }
        out
    }

    #[test]
    fn two_windows_on_one_host_are_distinct_and_end_targets_one() {
        use crate::app::Event;
        let (mut app, mut rx) = app_with_sink();
        let p = app.store.profiles()[0].clone();
        let a = app.new_session(&p);
        let b = app.new_session(&p);
        app.apply(Event::Opened {
            id: a,
            title: p.name.clone(),
            agent_ready: true,
            remote_id: Some("demo-111".into()),
            fallback: None,
        });
        app.apply(Event::Opened {
            id: b,
            title: p.name.clone(),
            agent_ready: true,
            remote_id: Some("demo-222".into()),
            fallback: None,
        });
        let (n1, n2) = (app.sessions[0].number, app.sessions[1].number);
        assert_ne!(n1, n2, "each window keeps its own number");
        assert_ne!(
            app.sessions[0].remote_id, app.sessions[1].remote_id,
            "and its own host session"
        );

        run_command(&mut app, &format!("quit {n1:03}"));
        assert_eq!(
            closed_ids(&mut rx),
            vec![a],
            "only window {n1:03} is closed"
        );
        assert_eq!(
            app.sessions.len(),
            2,
            "the window leaves only when confirmed"
        );
    }

    #[test]
    fn ending_a_saved_connection_closes_its_windows_and_forgets_it() {
        let (mut app, mut rx) = app_with_sink();
        let p = app.store.profiles()[0].clone();
        let name = p.name.clone();
        let a = app.new_session(&p);
        let b = app.new_session(&p);

        run_command(&mut app, &format!("quit {name}"));
        let mut ids = closed_ids(&mut rx);
        ids.sort();
        let mut want = vec![a, b];
        want.sort();
        assert_eq!(ids, want, "both windows are closed");
        assert!(
            app.store.find(&name).is_none(),
            "the connection is forgotten"
        );
        assert!(app.notice.contains("forgot"), "{}", app.notice);
    }

    #[test]
    fn enter_on_a_window_row_switches_to_it() {
        // Jumping between windows is the manager's job — Enter on a window row
        // switches to it.  There is no `attach` command any more.
        let mut app = test_app();
        let mut ui = Ui::default();
        let p = app.store.profiles()[0].clone();
        app.new_session(&p);
        app.new_session(&p);
        let row = app
            .manager_rows()
            .iter()
            .position(|r| matches!(r, Row::Window { index, .. } if *index == 0))
            .expect("a window row");
        app.selected = row;
        app.active = Some(1);
        app.view = View::Manager;

        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert_eq!(app.active, Some(0), "it switches to the first window");
        assert_eq!(app.view, View::Terminal);
    }

    #[test]
    fn a_plain_window_still_refreshes_the_task_list() {
        // Tasks are decoupled from windows, so even a plain live window keeps
        // the manager's host listing fresh — otherwise a killed task would
        // linger as a ghost row.
        let (mut app, mut rx) = app_with_sink();
        let p = app.store.profiles()[0].clone();
        let id = app.new_session(&p);
        app.apply(crate::app::Event::Opened {
            id,
            title: p.name.clone(),
            agent_ready: false,
            remote_id: None,
            fallback: None,
        });
        app.view = View::Manager;
        let mut ui = Ui::default();
        tick(&mut app, &mut ui);
        let mut polled = false;
        while let Ok(cmd) = rx.try_recv() {
            if matches!(cmd, Cmd::Monitor { .. } | Cmd::ListRemote { .. }) {
                polled = true;
            }
        }
        assert!(polled, "a plain window still refreshes the host listing");
    }

    #[test]
    fn a_bare_number_is_a_window_and_a_name_is_a_task() {
        // A bare number is always a window, and a task's name ends it on the
        // host when no window is holding it.
        let (mut app, mut rx) = app_with_sink();
        let p = app.store.profiles()[0].clone();
        let id = app.new_session(&p);
        let n = app.sessions[0].number;
        app.remote_sessions = vec![crate::app::RemoteSession {
            id: "qf001".into(),
            ..Default::default()
        }];

        // `end <n>` closes the window and never touches the host task.
        run_command(&mut app, &format!("quit {n:03}"));
        assert_eq!(closed_ids(&mut rx), vec![id], "a bare number is a window");
        let mut killed = false;
        while let Ok(cmd) = rx.try_recv() {
            if let Cmd::KillRemote { .. } = cmd {
                killed = true;
            }
        }
        assert!(
            !killed,
            "quit must not kill the host task: {}",
            app.notice
        );

        // `end qf001`, with no window holding it, ends the task on the host.
        run_command(&mut app, "quit qf001");
        let mut killed = false;
        while let Ok(cmd) = rx.try_recv() {
            if let Cmd::KillRemote { remote_id, .. } = cmd {
                assert_eq!(remote_id, "qf001");
                killed = true;
            }
        }
        assert!(killed, "quit qf001 ends the host task: {}", app.notice);
    }

    #[test]
    fn renaming_a_connection_refreshes_its_open_windows() {
        let mut app = test_app();
        let p = app.store.profiles()[0].clone();
        let old = p.name.clone();
        app.new_session(&p);
        open_editor(&mut app, p.clone(), false);
        app.editor.as_mut().unwrap().profile.name = "renamed".into();
        save_editor(&mut app);

        assert_eq!(app.sessions[0].profile_name, "renamed");
        assert_eq!(app.sessions[0].title, "renamed");
        assert_eq!(app.sessions[0].profile.name, "renamed");
        assert!(app.store.find(&old).is_none());
        assert!(app.store.find("renamed").is_some());
    }

    #[test]
    fn the_manager_refreshes_a_screen_window_without_the_tool_quietly() {
        use crate::app::Event;
        let (mut app, mut rx) = app_with_sink();
        let p = app.store.profiles()[0].clone();
        let id = app.new_session(&p);
        // A screen window whose host has no tool: the quick screen listing.
        app.apply(Event::Opened {
            id,
            title: p.name.clone(),
            agent_ready: false,
            remote_id: Some("sc-1".into()),
            fallback: None,
        });
        app.view = View::Manager;
        let mut ui = Ui::default();

        // The first tick refreshes at once, quietly, over screen.
        tick(&mut app, &mut ui);
        let mut refreshed = false;
        while let Ok(cmd) = rx.try_recv() {
            if let Cmd::ListRemote {
                screen: true,
                quiet: true,
                ..
            } = cmd
            {
                refreshed = true;
            }
        }
        assert!(refreshed, "a live screen window is watched");

        // A second call inside the same half second does nothing.
        tick(&mut app, &mut ui);
        assert!(rx.try_recv().is_err(), "at most twice a second");

        // Outside the F9 session view, nothing is polled.
        app.view = View::Terminal;
        ui.screen_tick = None;
        tick(&mut app, &mut ui);
        assert!(
            rx.try_recv().is_err(),
            "only the F9 session view refreshes"
        );
    }

    #[test]
    fn a_window_with_the_tool_is_sampled_quietly() {
        // A screen window whose host has the tool is kept fresh with the tool's
        // monitor sample, so its cpu/memory/state columns stay live — and the
        // refresh is quiet.
        let (mut app, mut rx) = app_with_sink();
        let p = app.store.profiles()[0].clone();
        let id = app.new_session(&p);
        mark_live(&mut app, id, true);
        app.view = View::Manager;
        let mut ui = Ui::default();
        tick(&mut app, &mut ui);
        let mut sampled = false;
        while let Ok(cmd) = rx.try_recv() {
            if let Cmd::Monitor { quiet: true, .. } = cmd {
                sampled = true;
            }
        }
        assert!(sampled, "a window with the tool is sampled quietly");
    }

    #[test]
    fn ending_a_host_task_goes_through_screen() {
        let (mut app, mut rx) = app_with_sink();
        let p = app.store.profiles()[0].clone();
        app.store.upsert(p);
        app.remote_sessions = vec![crate::app::RemoteSession {
            id: "qf009".into(),
            ..Default::default()
        }];
        app.selected = 0;

        run_command(&mut app, "quit qf009");
        let mut killed = false;
        while let Ok(cmd) = rx.try_recv() {
            if let Cmd::KillRemote { remote_id, .. } = cmd {
                assert_eq!(remote_id, "qf009");
                killed = true;
            }
        }
        assert!(killed, "quit <task> names the host task");
    }

    #[test]
    fn connecting_probes_the_host_for_screen() {
        let (mut app, mut rx) = app_with_sink();
        let p = app.store.profiles()[0].clone();
        open_profile(&mut app, &p, false);
        let mut probed = false;
        while let Ok(cmd) = rx.try_recv() {
            if let Cmd::ProbeScreen { .. } = cmd {
                probed = true;
            }
        }
        assert!(probed, "a connect tests the host for screen");
    }

    #[test]
    fn the_screen_status_field_shows_the_probe_result() {
        use crate::app::Event;
        let mut app = test_app();
        let p = app.store.profiles()[0].clone();
        open_editor(&mut app, p.clone(), false);
        assert!(
            app.screen_ok.get(&p.name).is_none(),
            "untested to begin with"
        );

        app.apply(Event::ScreenStatus {
            profile: p.name.clone(),
            ok: true,
        });
        assert_eq!(app.screen_ok.get(&p.name).copied(), Some(true));

        let mut ui = Ui::default();
        let screen = render_sized(&app, &mut ui, 120, 50);
        assert!(screen.contains("screen status"), "{screen}");
        assert!(screen.contains("installed"), "{screen}");
        assert!(screen.contains("install screen"), "{screen}");

        // A host without screen reads as "not installed".
        app.apply(Event::ScreenStatus {
            profile: p.name.clone(),
            ok: false,
        });
        let screen = render_sized(&app, &mut ui, 120, 50);
        assert!(screen.contains("not installed"), "{screen}");
    }

    #[test]
    fn the_form_shows_copyable_install_commands_per_distro() {
        let mut app = test_app();
        let p = app.store.profiles()[0].clone();
        open_editor(&mut app, p, false);
        let mut ui = Ui::default();
        let screen = render_sized(&app, &mut ui, 120, 50);
        // The copyable commands for the common distributions are on screen.
        assert!(screen.contains("apt-get install -y screen"), "{screen}");
        assert!(screen.contains("dnf install -y screen"), "{screen}");
        assert!(screen.contains("apk add screen"), "{screen}");
    }

    #[test]
    fn pressing_enter_on_an_install_row_copies_it() {
        let mut app = test_app();
        let p = app.store.profiles()[0].clone();
        open_editor(&mut app, p, false);
        let mut ui = Ui::default();
        let idx = fixed_field_count();
        assert!(
            field_command(idx).unwrap().contains("screen"),
            "the first copy row is a screen install command"
        );
        app.editor.as_mut().unwrap().field = idx;
        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        // Either it landed on the clipboard or it said why not.
        assert!(
            app.notice.contains("clipboard") || app.notice.contains("could not copy"),
            "{}",
            app.notice
        );
    }

    #[test]
    fn the_completion_list_opens_on_the_first_item_and_arrows_preview() {
        let mut app = test_app();
        let mut ui = Ui::default();
        app.cmd_focus = true;
        app.cmd_input = "get /var/l".into();
        apply_completion(
            &mut app,
            Completion {
                kind: CompletionKind::Remote,
                word: "/var/l".into(),
                candidates: vec!["/var/lib/".into(), "/var/local/".into(), "/var/log/".into()],
            },
        );
        assert_eq!(
            app.overlay.as_ref().unwrap().pick,
            0,
            "first item by default"
        );
        assert_eq!(
            app.cmd_input, "get /var/l",
            "the line is untouched until you move"
        );

        // Down previews the second candidate on the line.
        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        );
        assert_eq!(app.overlay.as_ref().unwrap().pick, 1);
        assert_eq!(app.cmd_input, "get /var/local/");

        // Typing again restores the line and resets the list, then appends.
        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        );
        assert!(app.overlay.is_none(), "typing resets the list");
        assert_eq!(
            app.cmd_input, "get /var/l",
            "the previous input is restored"
        );
        ui.pending_at = Some(Instant::now() - Duration::from_millis(100));
        flush_pending(&mut app, &mut ui, false);
        assert_eq!(app.cmd_input, "get /var/lx", "previous + this input");
    }

    #[test]
    fn enter_accepts_the_highlighted_completion_without_running_it() {
        let mut app = test_app();
        let mut ui = Ui::default();
        app.cmd_focus = true;
        app.cmd_input = "get /var/l".into();
        apply_completion(
            &mut app,
            Completion {
                kind: CompletionKind::Remote,
                word: "/var/l".into(),
                candidates: vec!["/var/lib/".into(), "/var/local/".into()],
            },
        );
        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        );
        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(app.overlay.is_none());
        assert_eq!(app.cmd_input, "get /var/local/");
        assert!(app.cmd_focus, "accepting a completion does not run it");
    }

    #[test]
    fn e_on_a_task_edits_the_screen_session_when_connected() {
        let (mut app, mut rx) = app_with_sink();
        let p = app.store.profiles()[0].clone();
        app.remote_sessions = vec![crate::app::RemoteSession {
            id: "qf001".into(),
            ..Default::default()
        }];
        app.remote_host = p.host.clone();
        app.view = View::Manager;
        let mut ui = Ui::default();
        let press_e = |app: &mut App, ui: &mut Ui| {
            app.selected = app
                .manager_rows()
                .iter()
                .position(|r| matches!(r, crate::app::Row::Remote { .. }))
                .expect("a task row");
            handle_key(app, ui, KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        };

        // Nothing is connected: `e` says so and opens no form.
        press_e(&mut app, &mut ui);
        assert!(app.session_form.is_none(), "no form without a connection");
        assert_eq!(app.view, View::Manager);

        // A live window on that host is the condition for editing the session.
        let id = app.new_session(&p);
        app.apply(crate::app::Event::Opened {
            id,
            title: p.name.clone(),
            agent_ready: false,
            remote_id: None,
            fallback: None,
        });
        app.view = View::Manager;
        press_e(&mut app, &mut ui);
        assert!(app.session_form.is_some(), "e opens the form: {}", app.notice);
        assert_eq!(app.view, View::ScreenForm);

        // Renaming and Ctrl-S sends exactly one ScreenEdit.
        app.session_form.as_mut().unwrap().name = "qf002".into();
        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
        );
        let mut sent = None;
        while let Ok(cmd) = rx.try_recv() {
            if let Cmd::ScreenEdit { remote_id, ops, .. } = cmd {
                sent = Some((remote_id, ops));
            }
        }
        let (remote_id, ops) = sent.expect("a ScreenEdit was forwarded");
        assert_eq!(remote_id, "qf001");
        assert!(
            matches!(ops.as_slice(), [crate::app::ScreenOp::Rename(n)] if n == "qf002"),
            "{ops:?}"
        );
        assert!(app.session_form.is_none(), "the form closes on save");
        assert_eq!(app.view, View::Manager);
    }

    #[test]
    fn enter_confirms_a_field_and_tab_saves_it_and_moves_on() {
        let mut app = test_app();
        let p = app.store.profiles()[0].clone();
        open_editor(&mut app, p, false);
        let mut ui = Ui::default();

        // Enter lands the value and leaves the cursor where it is.
        {
            let ed = app.editor.as_mut().unwrap();
            ed.field = 1; // host
            ed.editing = true;
            ed.buf = "new.example".into();
        }
        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        {
            let ed = app.editor.as_ref().unwrap();
            assert_eq!(ed.profile.host, "new.example");
            assert_eq!(ed.field, 1, "Enter stays on the field");
            assert!(!ed.editing, "Enter closes the field");
        }

        // Tab saves the field and opens the next one, ready to type.
        {
            let ed = app.editor.as_mut().unwrap();
            ed.field = 2; // port
            ed.editing = true;
            ed.buf = "2223".into();
        }
        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
        );
        {
            let ed = app.editor.as_ref().unwrap();
            assert_eq!(ed.profile.port, 2223, "Tab saves the field");
            assert_eq!(ed.field, 3, "Tab moves to the next field");
            assert!(ed.editing, "the next field is open to type");
            assert_eq!(ed.buf, "alice", "seeded with the next field's value");
        }
    }

    // -- the host-key ruling --------------------------------------------------

    /// Raises a prompt the way the controller would, keeping the receiving end
    /// so the test can see which way it was answered.
    fn host_key_prompt(app: &mut App, previous: Option<&str>) -> (SessionId, tokio::sync::oneshot::Receiver<bool>) {
        let p = app.store.profiles()[0].clone();
        let id = app.new_session(&p);
        let (answer, wait) = tokio::sync::oneshot::channel();
        app.apply(crate::app::Event::HostKey {
            id,
            host: p.host.clone(),
            fingerprint: "SHA256:offered".into(),
            previous: previous.map(str::to_string),
            answer: crate::app::KeyAnswer::new(answer),
        });
        (id, wait)
    }

    #[test]
    fn the_host_key_prompt_owns_the_keyboard_until_it_is_answered() {
        let mut app = test_app();
        let mut ui = Ui::default();
        let (_id, mut wait) = host_key_prompt(&mut app, None);
        assert!(!app.host_keys.is_empty());
        // Anything but y/n is swallowed while the ruling is up.
        type_keys(&mut app, &mut ui, "abc");
        assert!(!app.host_keys.is_empty(), "unrelated keys change nothing");
        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
        );
        assert!(app.host_keys.is_empty());
        assert_eq!(wait.try_recv().unwrap(), true, "y trusts the key");
        assert!(app.notice.contains("saved"), "{}", app.notice);
    }

    #[test]
    fn refusing_a_host_key_closes_the_window_waiting_on_it() {
        let mut app = test_app();
        let mut ui = Ui::default();
        let (id, mut wait) = host_key_prompt(&mut app, None);
        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
        );
        assert!(app.host_keys.is_empty());
        assert!(app.session_index(id).is_none(), "the window is gone");
        assert_eq!(app.view, View::Manager);
        assert_eq!(wait.try_recv().unwrap(), false, "n refuses the key");
        assert!(app.notice.contains("refused"), "{}", app.notice);
    }

    #[test]
    fn a_changed_key_is_shown_as_a_warning() {
        let mut app = test_app();
        let (_id, _wait) = host_key_prompt(&mut app, Some("SHA256:saved"));
        let mut ui = Ui::default();
        let screen = render(&app, &mut ui);
        assert!(screen.contains("WARNING"), "{screen}");
        assert!(screen.contains("SHA256:offered"), "{screen}");
        assert!(screen.contains("SHA256:saved"), "{screen}");
    }

    #[test]
    fn a_second_prompt_queues_behind_the_first() {
        let mut app = test_app();
        let (_first, mut wait_a) = host_key_prompt(&mut app, None);
        let (_second, mut wait_b) = host_key_prompt(&mut app, None);
        assert_eq!(app.host_keys.len(), 2, "the second ruling queues up");
        let mut ui = Ui::default();
        handle_key(
            &mut app,
            &mut ui,
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
        );
        assert_eq!(wait_a.try_recv().unwrap(), true);
        assert_eq!(app.host_keys.len(), 1, "the next ruling is now up");
        assert_eq!(app.host_keys[0].id, _second);
        // The queued one is still waiting, not dropped.
        assert!(wait_b.try_recv().is_err());
    }

    #[test]
    fn a_prompt_goes_away_with_its_window() {
        let mut app = test_app();
        let (id, mut wait) = host_key_prompt(&mut app, None);
        // The controller reports the window closed while its ruling was up:
        // the prompt must not outlive it, and the parked connect gets its no.
        app.apply(crate::app::Event::Closed {
            id,
            reason: "closed".into(),
        });
        assert!(app.host_keys.is_empty());
        // Dropping the answer unanswered reads as a no on the parked connect.
        assert!(matches!(
            wait.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed)
        ));
    }

    // -- mouse text selection -------------------------------------------------

    /// The composed frame of a manager page, drawn at 100x30.
    fn drawn_frame(app: &App, ui: &mut Ui) -> ratatui::buffer::Buffer {
        let backend = TestBackend::new(100, 30);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, app, ui)).unwrap();
        term.backend().buffer().clone()
    }

    /// The row whose composed text contains `needle`.
    fn row_with(buf: &ratatui::buffer::Buffer, needle: &str) -> u16 {
        for y in 0..buf.area.height {
            let line: String = (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol())
                .collect();
            if line.contains(needle) {
                return y;
            }
        }
        panic!("no row contains {needle:?}");
    }

    #[test]
    fn mouse_selection_reads_text_off_any_page() {
        use crate::app::Selection;
        let app = test_app();
        let mut ui = Ui::default();
        let buf = drawn_frame(&app, &mut ui);
        let y = row_with(&buf, "demo");

        // A whole row, dragged left-to-right.
        let sel = Selection {
            anchor: (y, 0),
            head: (y, 99),
        };
        assert!(selected_text(&buf, sel).contains("demo"));
        // The same stretch dragged bottom-up reads identically.
        let rev = Selection {
            anchor: (y, 99),
            head: (y, 0),
        };
        assert_eq!(selected_text(&buf, sel), selected_text(&buf, rev));

        // Several rows join with newlines, blank padding rows dropped.
        let y2 = row_with(&buf, "example.invalid:2222");
        let (top, bottom) = if y < y2 { (y, y2) } else { (y2, y) };
        let multi = selected_text(
            &buf,
            Selection {
                anchor: (top, 0),
                head: (bottom, 99),
            },
        );
        assert!(multi.contains('\n'), "multi-row selection: {multi:?}");
        assert!(multi.contains("demo") && multi.contains("example.invalid:2222"));
    }

    #[test]
    fn a_selection_is_highlighted_and_the_frame_is_kept_for_the_clipboard() {
        use crate::app::Selection;
        let mut app = test_app();
        let mut ui = Ui::default();
        let y = {
            let buf = drawn_frame(&app, &mut ui);
            row_with(&buf, "demo")
        };
        app.selection = Some(Selection {
            anchor: (y, 0),
            head: (y, 99),
        });
        let buf = drawn_frame(&app, &mut ui);
        assert_eq!(
            buf[(2, y)].style().bg,
            Some(ratatui::style::Color::Cyan),
            "selected cells are inverted"
        );
        assert!(ui.frame.is_some(), "the text is kept for the clipboard");
        // The kept frame is the *unstyled* text.
        assert!(selected_text(ui.frame.as_ref().unwrap(), app.selection.unwrap()).contains("demo"));

        // Selection over: the frame snapshot goes with it.
        app.selection = None;
        drawn_frame(&app, &mut ui);
        assert!(ui.frame.is_none());
    }

    #[test]
    fn a_plain_click_selects_and_copies_nothing() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut app = test_app();
        let mut ui = Ui::default();
        let down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 3,
            modifiers: KeyModifiers::NONE,
        };
        let up = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            ..down
        };
        handle_mouse(&mut app, &mut ui, down);
        assert!(app.selection.is_some(), "a drag starts here");
        handle_mouse(&mut app, &mut ui, up);
        assert!(app.selection.is_none(), "released without movement: nothing");
        // No copy happened: the notice is untouched.
        assert_eq!(app.notice, "");
    }
