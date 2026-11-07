//! minka-syu — update checker/launcher core for MinkaSyu.
//!
//! Verbs:
//!   check [--notify]   refresh throwaway sync DBs, list repo + AUR updates
//!                      with repository provenance, write the state file
//!   apply [--dry-run]  open the terminal on `paru -Syu --sudoloop`
//!   serve              NDJSON over stdio for the Quickshell UI
//!                      (MinkaLink's protocol, verbatim)
//!
//! Applying updates is paru's job, never this binary's: no libalpm
//! transactions happen here, so a check can never leave partial-upgrade
//! state behind.

mod alpm;
mod check;

use check::{load_state, maybe_notify, run_check, State};
use std::io::{BufRead, Write};
use std::process::Command;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let verb = args.first().map(String::as_str).unwrap_or("serve");
    let rest: Vec<&str> = args.iter().skip(1).map(String::as_str).collect();

    let result = match verb {
        "check" => cmd_check(rest.contains(&"--notify")),
        "apply" => cmd_apply(rest.contains(&"--dry-run")),
        "serve" => {
            serve();
            Ok(())
        }
        other => {
            eprintln!("minka-syu: unknown verb '{other}' (expected check|apply|serve)");
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("minka-syu: {e}");
        std::process::exit(1);
    }
}

fn print_summary(state: &State) {
    for u in &state.updates {
        let from = u.installed_repo.as_deref().unwrap_or("unknown source");
        if u.transition == check::Transition::Changed {
            println!("{} {} -> {}  [repo: {} -> {}]", u.name, u.old_version, u.new_version, from, u.candidate_repo);
        } else {
            println!("{} {} -> {}  [{}]", u.name, u.old_version, u.new_version, u.candidate_repo);
        }
    }
    println!(
        "{} update{} ({} change{} repository)",
        state.total(),
        if state.total() == 1 { "" } else { "s" },
        state.repo_change_count,
        if state.repo_change_count == 1 { "s" } else { "" }
    );
}

fn cmd_check(notify: bool) -> Result<(), String> {
    let mut state = run_check()?;
    print_summary(&state);
    if notify {
        maybe_notify(&mut state);
    }
    Ok(())
}

/// The terminal argv for `apply`: $MINKA_SYU_TERMINAL or rio, running paru.
/// Rio's -e takes the rest of argv as the command and must come last-ish.
pub fn apply_argv() -> Vec<String> {
    let terminal = std::env::var("MINKA_SYU_TERMINAL").unwrap_or_else(|_| "rio".to_string());
    vec![terminal, "-e".into(), "paru".into(), "-Syu".into(), "--sudoloop".into()]
}

fn cmd_apply(dry_run: bool) -> Result<(), String> {
    let argv = apply_argv();
    if dry_run {
        println!("{}", argv.join(" "));
        return Ok(());
    }
    Command::new(&argv[0])
        .args(&argv[1..])
        .spawn()
        .map_err(|e| format!("spawning terminal '{}': {e}", argv[0]))?;
    Ok(())
}

// -- serve ---------------------------------------------------------------

struct Error {
    code: &'static str,
    message: String,
}
impl Error {
    fn internal(m: String) -> Self {
        Error { code: "internal", message: m }
    }
    fn unknown_method(m: &str) -> Self {
        Error { code: "unknown_method", message: format!("no such method: {m}") }
    }
}

fn state_or_empty() -> State {
    load_state().unwrap_or(State {
        schema: 1,
        checked_at: 0,
        updates: Vec::new(),
        repo_change_count: 0,
        last_notified_count: 0,
    })
}

fn dispatch(state: &mut State, method: &str) -> Result<serde_json::Value, Error> {
    match method {
        "get_state" => serde_json::to_value(&*state).map_err(|e| Error::internal(e.to_string())),
        "check" => {
            let fresh = run_check().map_err(Error::internal)?;
            *state = fresh;
            serde_json::to_value(&*state).map_err(|e| Error::internal(e.to_string()))
        }
        "apply" => {
            cmd_apply(false).map_err(Error::internal)?;
            Ok(serde_json::Value::Null)
        }
        other => Err(Error::unknown_method(other)),
    }
}

fn handle_line(state: &mut State, line: &str, out: &mut impl Write) {
    let msg: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return, // malformed lines are ignored, as ShojiClient does
    };
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("").to_string();

    // Catch a panic so one bad request cannot end the session.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        dispatch(state, &method)
    }));
    let result = match outcome {
        Ok(r) => r,
        Err(_) => Err(Error::internal("handler panicked".to_string())),
    };

    // A completed check is broadcast too, so every view refreshes even
    // if the request came from somewhere else.
    if method == "check" && result.is_ok() {
        let broadcast = serde_json::json!({ "event": "state", "payload": state });
        let _ = writeln!(out, "{broadcast}");
    }

    let Some(id) = id else { return }; // no id => fire-and-forget
    let reply = match result {
        Ok(value) => serde_json::json!({ "id": id, "result": value }),
        Err(e) => serde_json::json!({ "id": id, "error": { "code": e.code, "message": e.message } }),
    };
    let _ = writeln!(out, "{reply}");
    let _ = out.flush();
}

fn serve() {
    let mut state = state_or_empty();
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        handle_line(&mut state, line, &mut out);
        let _ = out.flush();
    }
}
