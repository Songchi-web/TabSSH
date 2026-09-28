use super::controller::{
    complete_local, expand_tilde, is_rooted, parse_monitor, parse_remote_list, plain_pid_file,
    plain_shell_command, resolve_remote, split_word,
};
use super::*;
use crate::config::Profile;
use crate::ssh::Auth;

const SAMPLE: &str = r#"{"id":"web-1","shell":"/bin/bash","pid":1234,"sup_pid":1200,"created":1737000000,"age_secs":900,"last_activity_secs":3,"samples":6,"alive":true,"socket":true,"responsive":true,"probe":"ok","probe_ms":4,"cpu_pct":3.4,"cpu_pct_max":12.0,"rss_kb":5120,"procs":3,"agent_version":"0.1.0","verdict":"running"}"#;

#[test]
fn parses_the_monitor_report() {
    let rows = parse_monitor(SAMPLE);
    assert_eq!(rows.len(), 1);
    let r = &rows[0];
    assert_eq!(r.id, "web-1");
    assert_eq!(r.pid, 1234);
    assert_eq!(r.sup_pid, 1200);
    assert_eq!(r.created, 1_737_000_000);
    assert_eq!(r.age_secs, 900);
    assert_eq!(r.last_activity_secs, 3);
    assert!(r.alive && r.socket && r.responsive);
    assert_eq!(r.probe, "ok");
    assert!((r.cpu_pct - 3.4).abs() < 0.01, "{}", r.cpu_pct);
    assert!((r.cpu_pct_max - 12.0).abs() < 0.01, "{}", r.cpu_pct_max);
    assert_eq!(r.rss_kb, 5120);
    assert_eq!(r.procs, 3);
    assert_eq!(r.verdict, "running");
    assert!(r.measured());
}

#[test]
fn monitor_parser_tolerates_junk_and_dead_sessions() {
    let dead = r#"{"id":"d1","shell":"/bin/bash","pid":9,"sup_pid":8,"created":1,"age_secs":2,"last_activity_secs":2,"alive":false,"socket":false,"responsive":false,"probe":"timeout","probe_ms":0,"cpu_pct":0.0,"cpu_pct_max":0.0,"rss_kb":0,"procs":0,"verdict":"dead"}"#;
    let text = format!("{SAMPLE}\nnot json at all\n\n{dead}\n");
    let rows = parse_monitor(&text);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[1].verdict, "dead");
    assert!(!rows[1].alive);
    assert!(!rows[1].socket);
    assert_eq!(rows[1].probe, "timeout");
}

#[test]
fn the_quick_list_marks_sessions_as_unmeasured() {
    let rows =
        parse_remote_list(r#"{"id":"a","shell":"/bin/bash","pid":5,"sup_pid":4,"created":10}"#);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].verdict, "unmeasured");
    assert!(!rows[0].measured());
    assert_eq!(rows[0].sup_pid, 4);
}

#[test]
fn monitor_labels_read_naturally() {
    let mut r = RemoteSession {
        cpu_pct: 0.12,
        cpu_pct_max: 0.4,
        rss_kb: 5120,
        ..Default::default()
    };
    assert_eq!(r.cpu_label(), "0.1%");
    r.cpu_pct_max = 42.0;
    assert_eq!(r.cpu_label(), "42%");
    assert_eq!(r.mem_label(), "5.0 MiB");
}

#[test]
fn auth_is_tried_in_a_fixed_sensible_order() {
    let mut p = Profile::new("n", "h", "u");

    // Nothing configured: the machine's own keys, then nothing.
    let bare = auth_attempts(&p, None);
    assert!(matches!(bare[0], Auth::DefaultKeys), "{bare:?}");
    assert!(matches!(bare[1], Auth::None), "{bare:?}");
    assert_eq!(bare.len(), 2);

    // What the user gave is tried first, key before password.
    p.key = Some("/home/me/.ssh/id_ed25519".into());
    let both = auth_attempts(&p, Some("pw".into()));
    assert!(matches!(both[0], Auth::Key { .. }), "{both:?}");
    assert!(matches!(both[1], Auth::Password(_)), "{both:?}");
    assert!(matches!(both[2], Auth::DefaultKeys), "{both:?}");
    assert!(matches!(both[3], Auth::None), "{both:?}");
}

#[test]
fn a_pasted_key_is_recognised_and_used_as_data() {
    let mut p = Profile::new("n", "h", "u");
    p.key = Some(
        "-----BEGIN OPENSSH PRIVATE KEY-----
abc
-----END OPENSSH PRIVATE KEY-----"
            .into(),
    );
    assert!(p.key_is_inline());
    let attempts = auth_attempts(&p, None);
    assert!(matches!(attempts[0], Auth::KeyData { .. }), "{attempts:?}");
}

#[test]
fn an_older_config_that_only_stored_a_key_path_still_works() {
    let mut p = Profile::new("n", "h", "u");
    p.key_path = Some("/old/id_rsa".into());
    assert_eq!(p.key_source(), Some("/old/id_rsa"));
    assert!(!p.key_is_inline());
    p.tidy();
    assert_eq!(p.key.as_deref(), Some("/old/id_rsa"));
    assert!(p.key_path.is_none(), "the old field is folded away");
}

#[test]
fn splits_a_word_into_directory_and_prefix() {
    assert_eq!(split_word("/etc/hos"), ("/etc/".into(), "hos".into()));
    assert_eq!(split_word("hos"), (String::new(), "hos".into()));
    assert_eq!(split_word(""), (String::new(), String::new()));
    let win = r"C:\Users\a";
    assert_eq!(split_word(win), (r"C:\Users\".into(), "a".into()));
}

#[test]
fn local_completion_lists_matching_entries() {
    let dir = std::env::temp_dir().join("tabssh-complete-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("alpha.txt"), b"x").unwrap();
    std::fs::create_dir_all(dir.join("alpine")).unwrap();
    std::fs::write(dir.join("beta.txt"), b"x").unwrap();

    let got = complete_local(&format!("{}/al", dir.display()));
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(
        got.iter()
            .all(|s| s.ends_with(".txt") || s.ends_with(std::path::MAIN_SEPARATOR)),
        "{got:?}"
    );
    // A directory candidate keeps a trailing separator so Tab can descend.
    assert!(
        got.iter().any(|s| s.ends_with(std::path::MAIN_SEPARATOR)),
        "{got:?}"
    );
    assert!(complete_local(&format!("{}/zzz", dir.display())).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn parser_reads_the_right_fields_among_similar_keys() {
    // serde matches keys exactly, so a "proc" key can never shadow "procs".
    let line = r#"{"id":"s","procs":7,"alive":false,"cpu_pct":1.5}"#;
    let rows = parse_monitor(line);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].procs, 7);
    assert!(!rows[0].alive);
    assert!((rows[0].cpu_pct - 1.5).abs() < 1e-6, "{}", rows[0].cpu_pct);
}

#[test]
fn a_relative_remote_name_follows_the_shells_directory() {
    // With a known shell directory, a bare name is taken from there — the whole
    // point of following the ssh session.
    assert_eq!(
        resolve_remote("out.log", Some("/var/log"), "/home/me"),
        "/var/log/out.log"
    );
    // A trailing slash on the directory must not double up.
    assert_eq!(
        resolve_remote("out.log", Some("/var/log/"), "/home/me"),
        "/var/log/out.log"
    );
    // `~` and absolute paths are taken as written.
    assert_eq!(
        resolve_remote("~/x", Some("/var/log"), "/home/me"),
        "/home/me/x"
    );
    assert_eq!(resolve_remote("~", None, "/home/me"), "/home/me");
    assert_eq!(resolve_remote("/etc/hosts", Some("/var/log"), "/home/me"), "/etc/hosts");
    // Without a shell directory, a bare name is left for sftp, which starts at
    // the home directory — the old behaviour, kept as the fallback.
    assert_eq!(resolve_remote("out.log", None, "/home/me"), "/home/me/out.log");
    // An unresolved home is left alone rather than producing a literal `~/…`.
    assert_eq!(resolve_remote("out.log", None, "~"), "out.log");
}

#[test]
fn the_plain_shell_wrapper_records_its_own_pid_then_logs_in() {
    let file = "/home/me/.tabssh/plain.7";
    let cmd = plain_shell_command(file);
    // The pid is written before `exec`, which keeps the same pid, so the number
    // stays valid for the window's life.
    assert!(cmd.contains("echo $$ > /home/me/.tabssh/plain.7"), "{cmd}");
    assert!(cmd.contains("exec \"${SHELL:-/bin/sh}\" -l"), "{cmd}");
    assert!(cmd.starts_with("mkdir -p /home/me/.tabssh;"), "{cmd}");
    // Steps are `;`-separated, so a failed mkdir still leaves a shell behind.
    assert!(!cmd.contains("&&"), "{cmd}");
    // A directory with a space still comes out as one shell word.
    let spaced = plain_shell_command("/home/a b/.tabssh/plain.7");
    assert!(spaced.contains("mkdir -p '/home/a b/.tabssh';"), "{spaced}");
}

#[test]
fn a_plain_shell_pid_file_needs_a_real_home() {
    assert_eq!(
        plain_pid_file("/home/me", 3).as_deref(),
        Some("/home/me/.tabssh/plain.3")
    );
    // A trailing slash is folded away.
    assert_eq!(
        plain_pid_file("/home/me/", 3).as_deref(),
        Some("/home/me/.tabssh/plain.3")
    );
    assert!(plain_pid_file("~", 3).is_none());
    assert!(plain_pid_file("", 3).is_none());
}

#[test]
fn tilde_expands_to_the_home_before_listing() {
    assert_eq!(expand_tilde("~/", "/home/me"), "/home/me/");
    assert_eq!(expand_tilde("~", "/home/me"), "/home/me/");
    assert_eq!(expand_tilde("~/x/y", "/home/me"), "/home/me/x/y");
    // Not a tilde word: left exactly as typed.
    assert_eq!(expand_tilde("/abs", "/home/me"), "/abs");
    assert_eq!(expand_tilde("rel", "/home/me"), "rel");
    // A trailing slash on the home does not double up.
    assert_eq!(expand_tilde("~/x", "/home/me/"), "/home/me/x");
    // The Windows form keeps its backslash.
    assert_eq!(expand_tilde(r"~\x", r"C:\Users\me"), r"C:\Users\me\x");
}

#[test]
fn a_directory_part_is_rooted_or_not() {
    assert!(!is_rooted("./"));
    assert!(!is_rooted("sub/"));
    assert!(is_rooted("/var/"));
    assert!(is_rooted(r"C:\x"));
    assert!(is_rooted(r"\\server\share"));
}

#[test]
fn local_completion_reads_relative_directories_under_the_browser() {
    let base = std::env::temp_dir().join("tabssh-complete-anchor");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("sub")).unwrap();
    std::fs::write(base.join("here.txt"), b"x").unwrap();
    std::fs::write(base.join("sub").join("inside.txt"), b"x").unwrap();

    // A bare name is read from the browser's directory, not the process's.
    let bare = complete_local_in(&base, "here");
    assert!(bare.iter().any(|s| s.ends_with("here.txt")), "{bare:?}");

    // `./` reads the same place, and the candidate keeps the typed prefix.
    let dotted = complete_local_in(&base, "./");
    assert!(dotted.iter().any(|s| s.ends_with("here.txt")), "{dotted:?}");
    assert!(dotted.iter().all(|s| s.starts_with("./")), "{dotted:?}");

    // A relative subdirectory is anchored to the browser too.
    let sub = complete_local_in(&base, "sub/");
    assert!(sub.iter().any(|s| s.ends_with("inside.txt")), "{sub:?}");
    assert!(sub.iter().all(|s| s.starts_with("sub/")), "{sub:?}");

    // A name that only lives under the process's own directory must not leak in.
    assert!(complete_local_in(&base, "nope").is_empty());
    let _ = std::fs::remove_dir_all(&base);
}

