use crate::app::{App, Cmd, Completion, CompletionKind, Overlay};
use crate::{t, tf};

use super::*;

// ---------------------------------------------------------------------------
// tab completion
// ---------------------------------------------------------------------------

/// Every command the bar understands, by its one and only name.
///
/// Verb completion offers these while the verb is still being typed.  There are
/// no aliases any more, so this list is exactly the command set.
const VERBS: &[&str] = &["new", "edit", "quit", "setlang", "cd", "ls", "get", "put"];

/// Asks the controller to complete the word the cursor is on.
pub(super) fn request_completion(app: &mut App) {
    let line = app.cmd_input.clone();
    let trimmed = line.trim_end();
    if trimmed.is_empty() {
        return;
    }
    let parts = split_args(trimmed);
    let raw = parts.first().map(String::as_str).unwrap_or("");
    let verb = raw.strip_prefix("tabssh:").unwrap_or(raw);

    // No whitespace yet: the verb is still being typed, so complete it against
    // the command names.  Once the token is a whole command, Tab is about its
    // argument instead, and falls through to the logic below.
    if !line.contains(char::is_whitespace) && matches!(parse_command(verb), Command::Unknown(_)) {
        let candidates: Vec<String> = VERBS
            .iter()
            .filter(|v| v.starts_with(verb))
            .map(|v| (*v).to_string())
            .collect();
        apply_completion(
            app,
            Completion {
                kind: CompletionKind::Verb,
                word: verb.to_string(),
                candidates,
            },
        );
        return;
    }

    // The trailing token, or empty when the cursor is on the verb itself (the
    // user typed just `quit` and pressed Tab) or right after a space — either
    // way every candidate should be offered.
    let word = if line.ends_with(char::is_whitespace) || parts.len() < 2 {
        String::new()
    } else {
        trimmed
            .split_whitespace()
            .next_back()
            .unwrap_or("")
            .trim_start_matches('"')
            .to_string()
    };

    let kind = match verb {
        "get" => CompletionKind::Remote,
        "put" | "cd" | "ls" => CompletionKind::Local,
        "edit" => CompletionKind::Profile,
        "quit" => CompletionKind::Any,
        // Nothing else takes an argument, so leave the key alone.
        _ => return,
    };

    // Everything local is right here, so it needs no connection at all.
    match kind {
        CompletionKind::Local => {
            let candidates = crate::app::complete_local_in(&app.cwd, &word);
            apply_completion(
                app,
                Completion {
                    kind,
                    word,
                    candidates,
                },
            );
        }
        CompletionKind::Profile | CompletionKind::Any => {
            // How you name a thing: a window by its three digit number, a host
            // task by its name or id, and — for `quit` — also a saved
            // connection's name.
            let windows = || app.sessions.iter().map(|s| s.name());
            let tasks = || {
                app.remote_sessions
                    .iter()
                    .map(|r| r.id.clone())
                    // Windows that are already up name their own host task even
                    // before a fresh listing arrives.
                    .chain(app.sessions.iter().filter_map(|s| s.remote_id.clone()))
            };
            let profiles = || app.store.profiles().into_iter().map(|p| p.name.clone());
            let candidates: Vec<String> = match kind {
                CompletionKind::Profile => profiles().filter(|n| n.starts_with(&word)).collect(),
                _ => windows()
                    .chain(tasks())
                    .chain(profiles())
                    .filter(|n| n.starts_with(&word))
                    .collect(),
            };
            apply_completion(
                app,
                Completion {
                    kind,
                    word,
                    candidates,
                },
            );
        }
        CompletionKind::Remote => {
            // Only the host's filesystem needs to be asked.
            let Some(id) = app.active_id() else {
                app.notice(t!("open a window first"));
                return;
            };
            if !app.active_session().map(|s| s.is_live()).unwrap_or(false) {
                app.notice(t!("that needs a connected window"));
                return;
            }
            if app.completing {
                return;
            }
            app.completing = true;
            app.completion_for = Some((id, line.clone()));
            app.send(Cmd::Complete { id, kind, word });
        }
        // The verb list is answered right here, so the controller never sees
        // this kind.
        CompletionKind::Verb => {}
    }
}

/// Applies a completion result: a single match is inserted, several matches
/// extend to their common prefix and get listed.
pub fn apply_completion(app: &mut App, c: Completion) {
    if c.candidates.is_empty() {
        app.overlay = None;
        app.notice(match c.kind {
            CompletionKind::Verb => tf!("no command matches {}", c.word),
            CompletionKind::Remote => tf!("nothing on the host matches {}", c.word),
            CompletionKind::Local => tf!("nothing local matches {}", c.word),
            CompletionKind::Profile => tf!("no saved connection matches {}", c.word),
            CompletionKind::Any => tf!("no session matches {}", c.word),
        });
        return;
    }

    if c.candidates.len() == 1 {
        let chosen = c.candidates[0].clone();
        // A completed command name gets a trailing space, so its argument can
        // follow right away; an argument completion is inserted as it is.
        let chosen = if c.kind == CompletionKind::Verb {
            format!("{chosen} ")
        } else {
            chosen
        };
        app.cmd_input = replace_last_word(&app.cmd_input, &chosen);
        app.overlay = None;
        app.notice("");
        return;
    }

    // Extend as far as the candidates agree, then list every possibility by
    // name — the way a shell does, so an inexact match is never a dead end.
    let shared = common_prefix(&c.candidates);
    if shared.chars().count() > c.word.chars().count() {
        app.cmd_input = replace_last_word(&app.cmd_input, &shared);
    }
    let shown: Vec<String> = c.candidates.iter().map(|v| short_name(v)).collect();
    // The values the list inserts carry the same trailing space for a command.
    let values: Vec<String> = if c.kind == CompletionKind::Verb {
        c.candidates.iter().map(|v| format!("{v} ")).collect()
    } else {
        c.candidates.clone()
    };
    // Remember the line as it stands now; moving the highlight previews a
    // candidate, and typing again falls back to this.
    let base = app.cmd_input.clone();
    app.overlay = Some(Overlay::completions(shown, values, base));
}

/// The last path component, which is all a completion list needs to show.
pub(super) fn short_name(value: &str) -> String {
    let trimmed = value.trim_end_matches(['/', '\\']);
    match trimmed.rsplit(['/', '\\']).next() {
        Some(name) if name != trimmed => {
            if value.ends_with(['/', '\\']) {
                format!("{name}/")
            } else {
                name.to_string()
            }
        }
        _ => trimmed.to_string(),
    }
}

/// Replaces the trailing whitespace-delimited word.  Byte indices come from
/// `char_indices`, so a full-width space is cut on its own boundary.
pub(super) fn replace_last_word(input: &str, new: &str) -> String {
    match input.char_indices().rev().find(|(_, c)| c.is_whitespace()) {
        Some((i, c)) => format!("{}{new}", &input[..i + c.len_utf8()]),
        None => new.to_string(),
    }
}

pub(super) fn common_prefix(items: &[String]) -> String {
    let mut it = items.iter();
    let Some(first) = it.next() else {
        return String::new();
    };
    let mut out = first.clone();
    for item in it {
        let n = out
            .chars()
            .zip(item.chars())
            .take_while(|(a, b)| a == b)
            .count();
        out = out.chars().take(n).collect();
        if out.is_empty() {
            break;
        }
    }
    out
}
