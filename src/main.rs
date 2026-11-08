//! minka-syu — update checker/launcher core for MinkaSyu.
//!
//! Verbs:
//!   check [--notify]   refresh throwaway sync DBs, list repo + AUR updates
//!                      with repository provenance, write the state file
//!   apply [--dry-run] [--ignore a,b]
//!                      open the terminal on `paru -Syu --sudoloop`,
//!                      optionally ignoring named updates this once
//!   search <terms...>  search sync repos + AUR by name/description
//!   installed          list installed packages with origin + install reason
//!   install [--dry-run] <pkg...>   open the terminal on `paru -S <pkg...>`
//!   remove [--dry-run] <pkg...>    open the terminal on `paru -Rns <pkg...>`
//!   serve              NDJSON over stdio for the Quickshell UI
//!                      (MinkaLink's protocol, verbatim)
//!
//! Applying updates is paru's job, never this binary's: no libalpm
//! transactions happen here, so a check can never leave partial-upgrade
//! state behind.

mod alpm;
mod appstream;
mod catalog;
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
        "apply" => cmd_apply(rest.contains(&"--dry-run"), &parse_ignore(&rest)),
        "search" => cmd_search(&rest.join(" ")),
        "installed" => cmd_installed(),
        "appstream" => cmd_appstream(rest.first().copied().unwrap_or("")),
        "install" => cmd_packages(true, rest.contains(&"--dry-run"), &positional(&rest)),
        "remove" => cmd_packages(false, rest.contains(&"--dry-run"), &positional(&rest)),
        "serve" => {
            serve();
            Ok(())
        }
        other => {
            eprintln!(
                "minka-syu: unknown verb '{other}' (expected check|apply|search|installed|install|remove|serve)"
            );
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
        if u.explicit_target {
            println!(
                "{} {} -> {}  [repo: {} -> {}; newer than {} {}]",
                u.name,
                u.old_version,
                u.new_version,
                from,
                u.candidate_repo,
                u.first_repo.as_deref().unwrap_or("?"),
                u.first_version.as_deref().unwrap_or("?")
            );
        } else if u.transition == check::Transition::Changed {
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

/// Package names accepted into a terminal argv. They come from the
/// UI's own lists, but they still cross into a sudo-running command,
/// so anything that is not a plausible pkgname is refused outright
/// rather than quoted-and-hoped.
fn valid_package_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '@' | '.' | '_' | '+' | '-'))
}

fn validate_names(names: &[String], what: &str) -> Result<(), String> {
    if names.is_empty() {
        return Err(format!("no packages given for {what}"));
    }
    for n in names {
        if !valid_package_name(n) {
            return Err(format!("invalid package name for {what}: {n:?}"));
        }
    }
    Ok(())
}

fn terminal() -> String {
    std::env::var("MINKA_SYU_TERMINAL").unwrap_or_else(|_| "rio".to_string())
}

/// `repo/package` targets selected for a system upgrade: the ticked
/// updates whose chosen candidate sits in a lower-ranked repository
/// than pacman's own, so a plain `-Syu` would never select them.
fn explicit_targets(state: &State, ignore: &[String]) -> Vec<String> {
    state
        .updates
        .iter()
        .filter(|u| u.explicit_target && !ignore.contains(&u.name))
        .map(|u| format!("{}/{}", u.candidate_repo, u.name))
        .collect()
}

fn validate_targets(targets: &[String]) -> Result<(), String> {
    for t in targets {
        let Some((repo, name)) = t.split_once('/') else {
            return Err(format!("invalid upgrade target: {t:?}"));
        };
        if !valid_package_name(repo) || !valid_package_name(name) {
            return Err(format!("invalid upgrade target: {t:?}"));
        }
    }
    Ok(())
}

/// The terminal argv for a system upgrade: $MINKA_SYU_TERMINAL or rio,
/// running paru. Unticked updates become one --ignore list, this run
/// only; nothing about them is remembered. Explicit `repo/name`
/// targets ride along in the same transaction, so libalpm still
/// resolves the whole upgrade at once. Rio's -e takes the rest of
/// argv as the command.
pub fn upgrade_argv_with_targets(ignore: &[String], targets: &[String]) -> Vec<String> {
    let mut argv = vec![
        terminal(),
        "-e".into(),
        "paru".into(),
        "-Syu".into(),
        "--sudoloop".into(),
    ];
    if !ignore.is_empty() {
        argv.push(format!("--ignore={}", ignore.join(",")));
    }
    argv.extend(targets.iter().cloned());
    argv
}

pub fn upgrade_argv(ignore: &[String]) -> Vec<String> {
    upgrade_argv_with_targets(ignore, &[])
}

/// For continuity with anything that printed the plain argv.
pub fn apply_argv() -> Vec<String> {
    upgrade_argv(&[])
}

pub fn install_argv(packages: &[String]) -> Vec<String> {
    let mut argv = vec![terminal(), "-e".into(), "paru".into(), "-S".into(), "--sudoloop".into(), "--".into()];
    argv.extend(packages.iter().cloned());
    argv
}

pub fn remove_argv(packages: &[String]) -> Vec<String> {
    // -Rns: remove, drop now-unneeded dependencies, and don't keep
    // .pacsave backups. The terminal still shows paru/pacman's own
    // confirmation with the full dependency picture before anything
    // happens.
    let mut argv = vec![terminal(), "-e".into(), "paru".into(), "-Rns".into(), "--".into()];
    argv.extend(packages.iter().cloned());
    argv
}

fn launch(argv: &[String], dry_run: bool) -> Result<(), String> {
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

fn cmd_apply_state(dry_run: bool, ignore: &[String], state: &State) -> Result<(), String> {
    if !ignore.is_empty() {
        validate_names(ignore, "--ignore")?;
    }
    let targets = explicit_targets(state, ignore);
    validate_targets(&targets)?;
    launch(&upgrade_argv_with_targets(ignore, &targets), dry_run)
}

fn cmd_apply(dry_run: bool, ignore: &[String]) -> Result<(), String> {
    cmd_apply_state(dry_run, ignore, &state_or_empty())
}

fn cmd_packages(install: bool, dry_run: bool, packages: &[String]) -> Result<(), String> {
    let what = if install { "install" } else { "remove" };
    validate_names(packages, what)?;
    let argv = if install { install_argv(packages) } else { remove_argv(packages) };
    launch(&argv, dry_run)
}

fn cmd_search(query: &str) -> Result<(), String> {
    let found = catalog::run_search(query)?;
    for r in &found.results {
        match &r.installed_version {
            Some(v) => println!("{}/{} {} [installed: {}]", r.repo, r.name, r.version, v),
            None => println!("{}/{} {}", r.repo, r.name, r.version),
        }
        if !r.description.is_empty() {
            println!("    {}", r.description);
        }
    }
    if found.truncated {
        println!("(results truncated; refine the query)");
    }
    Ok(())
}

fn cmd_appstream(package: &str) -> Result<(), String> {
    if !valid_package_name(package) {
        return Err(format!("invalid package name: {package:?}"));
    }
    match appstream::run_appstream(package) {
        Some(info) => {
            println!("{} ({})", info.name, info.id);
            println!("{}", info.summary);
            if !info.description.is_empty() {
                println!("\n{}", info.description);
            }
            if !info.homepage.is_empty() {
                println!("\nHomepage: {}", info.homepage);
            }
            if !info.screenshots.is_empty() {
                println!("Screenshot: {}", info.screenshots[0]);
            }
            Ok(())
        }
        None => Err(format!("no AppStream data for {package}")),
    }
}

fn cmd_installed() -> Result<(), String> {
    for e in catalog::run_installed() {
        println!(
            "{} {} [{}]{}",
            e.name,
            e.version,
            e.repo,
            if e.explicit { "" } else { " (dependency)" }
        );
    }
    Ok(())
}

/// `--ignore a,b` or `--ignore=a,b` from CLI args.
fn parse_ignore(args: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--ignore" {
            if let Some(v) = args.get(i + 1) {
                out.extend(v.split(',').map(|s| s.to_string()));
                i += 1;
            }
        } else if let Some(v) = args[i].strip_prefix("--ignore=") {
            out.extend(v.split(',').map(|s| s.to_string()));
        }
        i += 1;
    }
    out
}

/// CLI positionals: everything that is not a flag we consume.
fn positional(args: &[&str]) -> Vec<String> {
    args.iter()
        .filter(|a| !a.starts_with("--"))
        .map(|s| s.to_string())
        .collect()
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

/// A string-array RPC param ("ignore", "packages"); absent = empty.
fn string_array(params: &serde_json::Value, key: &str) -> Vec<String> {
    params
        .get(key)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn error_code(m: String) -> Error {
    Error { code: "bad_request", message: m }
}

fn dispatch(
    state: &mut State,
    method: &str,
    params: &serde_json::Value,
) -> Result<serde_json::Value, Error> {
    match method {
        "get_state" => serde_json::to_value(&*state).map_err(|e| Error::internal(e.to_string())),
        "check" => {
            let fresh = run_check().map_err(Error::internal)?;
            *state = fresh;
            serde_json::to_value(&*state).map_err(|e| Error::internal(e.to_string()))
        }
        "apply" => {
            let ignore = string_array(params, "ignore");
            cmd_apply_state(false, &ignore, state).map_err(error_code)?;
            Ok(serde_json::Value::Null)
        }
        "search" => {
            let query = params
                .get("query")
                .and_then(|v| v.as_str())
                .ok_or_else(|| error_code("search needs a query".to_string()))?;
            let found = catalog::run_search(query).map_err(error_code)?;
            serde_json::to_value(found).map_err(|e| Error::internal(e.to_string()))
        }
        "installed" => {
            serde_json::to_value(catalog::run_installed())
                .map_err(|e| Error::internal(e.to_string()))
        }
        "appstream" => {
            let package = params
                .get("package")
                .and_then(|v| v.as_str())
                .ok_or_else(|| error_code("appstream needs a package".to_string()))?;
            if !valid_package_name(package) {
                return Err(error_code(format!("invalid package name: {package:?}")));
            }
            // Null (not an error) when no component claims the
            // package: absence of garnish is a normal answer.
            serde_json::to_value(appstream::run_appstream(package))
                .map_err(|e| Error::internal(e.to_string()))
        }
        "install" => {
            let packages = string_array(params, "packages");
            cmd_packages(true, false, &packages).map_err(error_code)?;
            Ok(serde_json::Value::Null)
        }
        "remove" => {
            let packages = string_array(params, "packages");
            cmd_packages(false, false, &packages).map_err(error_code)?;
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
    let params = msg.get("params").cloned().unwrap_or(serde_json::Value::Null);

    // Catch a panic so one bad request cannot end the session.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        dispatch(state, &method, &params)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgrade_argv_ignore() {
        let plain = upgrade_argv(&[]);
        assert_eq!(plain.len(), 5);
        assert_eq!(plain[3], "-Syu");
        let ignored = upgrade_argv(&["linux".into(), "mesa".into()]);
        assert_eq!(ignored.last().unwrap(), "--ignore=linux,mesa");
        // The plain form is unchanged for continuity.
        assert_eq!(apply_argv(), plain);
    }

    #[test]
    fn package_argv_forms() {
        let i = install_argv(&["ripgrep".into()]);
        assert_eq!(&i[2..], ["paru", "-S", "--sudoloop", "--", "ripgrep"]);
        let r = remove_argv(&["ripgrep".into(), "fd".into()]);
        assert_eq!(&r[2..], ["paru", "-Rns", "--", "ripgrep", "fd"]);
    }

    #[test]
    fn name_validation() {
        assert!(valid_package_name("libsigc++"));
        assert!(valid_package_name("python-3.14"));
        assert!(!valid_package_name("-Syu"));
        assert!(!valid_package_name("a b"));
        assert!(!valid_package_name("a;rm"));
        assert!(!valid_package_name(""));
        assert!(validate_names(&[], "install").is_err());
        assert!(validate_names(&["ok-pkg".into()], "install").is_ok());
    }

    #[test]
    fn explicit_targets_from_state() {
        let upd = |name: &str, repo: &str, explicit: bool| check::Update {
            name: name.into(),
            old_version: "1-1".into(),
            new_version: "1-2".into(),
            installed_repo: None,
            candidate_repo: repo.into(),
            transition: check::Transition::UnknownSource,
            explicit_target: explicit,
            first_repo: explicit.then(|| "cachyos-extra-v4".to_string()),
            first_version: explicit.then(|| "1-1".to_string()),
        };
        let state = State {
            schema: 1,
            checked_at: 0,
            updates: vec![
                upd("bcachefs-dkms", "minka", true),
                upd("mesa", "extra", false),
                upd("held-back", "minka", true),
            ],
            repo_change_count: 0,
            last_notified_count: 0,
        };
        // Unticked explicit candidates are left to --ignore, not targeted.
        let targets = explicit_targets(&state, &["held-back".to_string()]);
        assert_eq!(targets, ["minka/bcachefs-dkms"]);
        let argv = upgrade_argv_with_targets(&["held-back".into()], &targets);
        assert_eq!(
            &argv[2..],
            ["paru", "-Syu", "--sudoloop", "--ignore=held-back", "minka/bcachefs-dkms"]
        );
        assert!(validate_targets(&targets).is_ok());
        assert!(validate_targets(&["minka/-Syu".into()]).is_err());
        assert!(validate_targets(&["no-slash".into()]).is_err());
    }

    #[test]
    fn ignore_cli_parsing() {
        assert_eq!(parse_ignore(&["--ignore", "a,b"]), ["a", "b"]);
        assert_eq!(parse_ignore(&["--ignore=a,b", "--dry-run"]), ["a", "b"]);
        assert!(parse_ignore(&["--dry-run"]).is_empty());
    }
}
