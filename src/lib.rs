//! minka-syu — update checker/launcher core for MinkaSyu.
//!
//! Verbs:
//!   check [--notify]   refresh throwaway sync DBs, list repo + AUR updates
//!                      with repository provenance, write the state file
//!   apply [--dry-run] [--ignore a,b]
//!                      open the terminal on `paru -Syu --sudoloop`,
//!                      optionally ignoring named updates this once
//!   search <terms...>  search sync repos + AUR by name/description
//!   repos <pkg>        list every repo build of one package, in rank order
//!   installed          list installed packages with origin + install reason
//!   install [--dry-run] <pkg...>   open the terminal on `paru -S <pkg...>`
//!   remove [--dry-run] <pkg...>    open the terminal on `paru -Rns <pkg...>`
//!   reason [--json] <pkg>          an installed package's install reason
//!                      and what requires it
//!   reason [--dry-run] --asdeps|--asexplicit <pkg>
//!                      open the terminal on `sudo pacman -D --asdeps|--asexplicit`
//!   sources [--refresh]
//!                      MinkaPKG packages vs upstream, through
//!                      `minkarepo check` (JSON; cached unless --refresh)
//!   compare <pkg> <repo-a> <repo-b> [--print]
//!                      the source trees behind two repositories' builds
//!                      of a package ("aur" for the AUR's), written to the
//!                      cache dir and opened in meld (--print: only say
//!                      where they are and where they came from)
//!   ui                 open the window (minka-syu-gui)
//!   serve              NDJSON over stdio for the UI
//!                      (MinkaLink's protocol, verbatim)
//!   run --step <s> --result <f> -- <command...>
//!                      what the terminal of apply/install/remove runs:
//!                      the command, recorded, and how it ended (run.rs)
//!
//! Applying updates is paru's job, never this binary's: no libalpm
//! transactions happen here, so a check can never leave partial-upgrade
//! state behind. What this binary does see is how paru ended: apply,
//! install and remove wait for it and report a failure (its status and
//! its error lines) on stderr with a non-zero exit, and `serve` sends it
//! to the window as {event:"run"}.

mod alpm;
mod appstream;
mod catalog;
mod check;
mod compare;
mod run;
mod sources;

use check::{load_state, maybe_notify, run_check, State};
use std::io::{BufRead, Write};
use std::process::Command;

pub fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let verb = args.first().map(String::as_str).unwrap_or("serve");
    let rest: Vec<&str> = args.iter().skip(1).map(String::as_str).collect();

    let result = match verb {
        "check" => cmd_check(rest.contains(&"--notify")),
        "apply" => cmd_apply(rest.contains(&"--dry-run"), &parse_ignore(&rest)),
        "search" => cmd_search(&rest.join(" ")),
        "repos" => cmd_repos(rest.first().copied().unwrap_or("")),
        "installed" => cmd_installed(),
        "appstream" => cmd_appstream(rest.first().copied().unwrap_or("")),
        "install" => cmd_packages(true, rest.contains(&"--dry-run"), &positional(&rest), Report::Wait),
        "remove" => cmd_packages(false, rest.contains(&"--dry-run"), &positional(&rest), Report::Wait),
        "reason" => cmd_reason(&rest),
        "sources" => cmd_sources(rest.contains(&"--refresh")),
        "compare" => cmd_compare(&positional(&rest), rest.contains(&"--print")),
        "ui" => cmd_ui(),
        "run" => match run::run_verb(&rest) {
            Ok(code) => std::process::exit(code),
            Err(e) => Err(e),
        },
        "serve" => {
            serve();
            Ok(())
        }
        other => {
            minka_note::note!(
                "minka-syu: unknown verb '{other}' (expected check|apply|search|repos|installed|install|remove|reason|sources|compare|ui|serve)"
            );
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        minka_note::note!("minka-syu: {e}");
        std::process::exit(1);
    }
}

/// Why pacman.conf holds an update back, in words.
fn held_reason(u: &check::Update) -> &'static str {
    match u.held_by.as_deref() {
        Some("usage") => "no repository with Usage = Upgrade has it",
        _ => "IgnorePkg/IgnoreGroup",
    }
}

/// One update as the CLI lists it.
fn update_line(u: &check::Update) -> String {
    let from = u.installed_repo.as_deref().unwrap_or("unknown source");
    if u.held {
        let switch = if u.switch_build { format!("; {}", u.switch_note()) } else { String::new() };
        format!(
            "{} {} -> {}  [{}] (held: {}{switch})",
            u.name,
            u.old_version,
            u.new_version,
            u.candidate_repo,
            held_reason(u)
        )
    } else if !u.superseded_by.is_empty() {
        format!(
            "{} {} -> {}  [{}] (replaced by {})",
            u.name,
            u.old_version,
            u.new_version,
            u.candidate_repo,
            u.superseded_by.join(", ")
        )
    } else if u.new_install {
        format!("{} {}  [{}] (new package)", u.name, u.new_version, u.candidate_repo)
    } else if !u.replaces.is_empty() {
        let replaced: Vec<String> = u.replaces.iter().map(|r| format!("{} {}", r.name, r.version)).collect();
        format!("{} {}  [{}] replaces {}", u.name, u.new_version, u.candidate_repo, replaced.join(", "))
    } else if u.switch_build {
        format!(
            "{} {} -> {}  [repo: {} -> {}; {}]",
            u.name,
            u.old_version,
            u.new_version,
            from,
            u.candidate_repo,
            u.switch_note()
        )
    } else if u.explicit_target {
        format!(
            "{} {} -> {}  [repo: {} -> {}; newer than {} {}]",
            u.name,
            u.old_version,
            u.new_version,
            from,
            u.candidate_repo,
            u.first_repo.as_deref().unwrap_or("?"),
            u.first_version.as_deref().unwrap_or("?")
        )
    } else {
        // pacman's own upgrade, and the newer build a later repository
        // has, when there is one: the window can choose that instead.
        let newer = u.newer_note().map(|n| format!("; {n}")).unwrap_or_default();
        if u.transition == check::Transition::Changed {
            format!("{} {} -> {}  [repo: {} -> {}{newer}]", u.name, u.old_version, u.new_version, from, u.candidate_repo)
        } else {
            format!("{} {} -> {}  [{}{newer}]", u.name, u.old_version, u.new_version, u.candidate_repo)
        }
    }
}

fn print_summary(state: &State) {
    // The origin group is bookkeeping: after the updates, next to the
    // total that counts it.
    for u in state.updates.iter().filter(|u| !u.is_origin_group()) {
        println!("{}", update_line(u));
    }
    if let Some(group) = origin_row(state) {
        print_origin_group(group);
    }
    let held = state.held();
    println!(
        "{} update{} ({} change{} repository){}",
        state.total(),
        if state.total() == 1 { "" } else { "s" },
        state.repo_change_count,
        if state.repo_change_count == 1 { "s" } else { "" },
        if held > 0 { format!(", {held} held") } else { String::new() }
    );
    print_untracked(state);
}

/// The origin group: one line, then its names by repository.
fn print_origin_group(u: &check::Update) {
    let n = u.origin_group.len();
    println!(
        "{n} package{} with no recorded repository: reinstall {} from {} to record where {} came from  [{}]",
        if n == 1 { "" } else { "s" },
        if n == 1 { "it" } else { "them" },
        if n == 1 { "its repository" } else { "their repositories" },
        if n == 1 { "it" } else { "they" },
        u.candidate_repo
    );
    for repo in u.candidate_repo.split(", ") {
        let names: Vec<&str> = u.origin_group.iter().filter(|m| m.repo == repo).map(|m| m.name.as_str()).collect();
        if !names.is_empty() {
            for line in wrap_words(&format!("    {repo}: "), &names, NAMES_WIDTH) {
                println!("{line}");
            }
        }
    }
}

/// How wide a list of names may run before it wraps.
const NAMES_WIDTH: usize = 100;

/// `words` after `lead`, wrapped at `width` columns, the lines after the
/// first indented under the first word. Every word is kept: these are what
/// an apply reinstalls, and the CLI cannot expand an "and N more".
fn wrap_words(lead: &str, words: &[&str], width: usize) -> Vec<String> {
    let indent = " ".repeat(lead.chars().count());
    let mut lines = Vec::new();
    let mut line = lead.to_string();
    let mut empty = true;
    for w in words {
        if !empty && line.chars().count() + 1 + w.chars().count() > width {
            lines.push(std::mem::replace(&mut line, indent.clone()));
            empty = true;
        }
        if !empty {
            line.push(' ');
        }
        line.push_str(w);
        empty = false;
    }
    lines.push(line);
    lines
}

/// "required by a, b and 3 more" with at most `max` names, as the window
/// caps them, then "; optional for ..." when optional dependencies name
/// it, then, given `needs` (all_required_by, needed_by_explicit and
/// covered_by of a LocalPackage), whether anything installed explicitly
/// needs it and what satisfies the packages that require it without
/// needing it, then which other installed packages offer the names it
/// offers (`shared`: at most `max` of those names), and `full` (where to
/// see every name) when a list was cut. `name` itself is
/// left out of `required_by`: pacman lists a package that depends on itself
/// there, which says nothing about what else needs it.
fn needed_text(
    name: &str,
    required_by: &[String],
    optional_for: &[String],
    needs: Option<(&[String], &[String], &[String])>,
    shared: &[alpm::SharedProvide],
    max: usize,
    full: &str,
) -> String {
    let list = |names: &[String]| {
        let shown: Vec<&str> = names.iter().take(max).map(String::as_str).collect();
        let more = names.len().saturating_sub(max);
        if more > 0 { format!("{} and {more} more", shown.join(", ")) } else { shown.join(", ") }
    };
    let others: Vec<String> = required_by.iter().filter(|n| *n != name).cloned().collect();
    let mut out = match (others.is_empty(), required_by.is_empty()) {
        (true, true) => "nothing requires it".to_string(),
        (true, false) => "nothing else requires it".to_string(),
        (false, _) => format!("required by {}", list(&others)),
    };
    if !optional_for.is_empty() {
        out += &format!("; optional for {}", list(optional_for));
    }
    let mut cut = others.len() > max || optional_for.len() > max;
    // The covered_by this line names, if any.
    let mut said_covers: &[String] = &[];
    // Nothing (else) requires it: that has said all of it.
    if let Some((all, explicit, covered_by)) = needs.filter(|_| !others.is_empty()) {
        said_covers = covered_by;
        let (clause, shown) = match explicit.len() {
            // Not expected: what requires it and stays has a provider that stays.
            0 if all.is_empty() && covered_by.is_empty() => {
                ("; nothing needs it: another installed package satisfies what requires it".to_string(), all)
            }
            0 if all.is_empty() => {
                let verb = if covered_by.len() == 1 { "provides" } else { "provide" };
                (format!("; nothing needs it: {} also {verb} what requires it", list(covered_by)), covered_by)
            }
            0 => {
                let what = if all.len() == 1 { "dependency" } else { "dependencies" };
                (format!("; nothing you installed explicitly needs it, only the {what} {}", list(all)), all)
            }
            n => (format!("; needed by {n} explicitly installed package{}: {}", if n == 1 { "" } else { "s" }, list(explicit)), explicit),
        };
        out += &clause;
        cut |= shown.len() > max;
        // Some need it, and the rest that require it have another provider.
        if !all.is_empty() && !covered_by.is_empty() {
            out += &format!("; others are covered by {}", list(covered_by));
            cut |= covered_by.len() > max;
        }
    }
    // Its own name, when what covers its requirers (named above) are all
    // that offer it: ca-certificates and ca-certificates-utils, said once.
    let shared: Vec<&alpm::SharedProvide> =
        shared.iter().filter(|s| !(s.name == name && s.also.iter().all(|n| said_covers.contains(n)))).collect();
    for s in shared.iter().take(max) {
        // A name that is another package's own (ca-certificates-utils
        // provides ca-certificates): "provided by ca-certificates" would read
        // as a loop.
        let providers: Vec<String> = s.also.iter().filter(|n| **n != s.name).cloned().collect();
        out += &match (providers.len() < s.also.len(), providers.is_empty()) {
            (true, true) => format!("; {} is also an installed package", s.name),
            (true, false) => format!("; {} is also an installed package, and provided by {}", s.name, list(&providers)),
            (false, _) => format!("; {} is also provided by {}", s.name, list(&providers)),
        };
        cut |= providers.len() > max;
    }
    if shared.len() > max {
        out += &format!("; and {} more names other packages offer too", shared.len() - max);
        cut = true;
    }
    if cut {
        out += &format!("; {full}");
    }
    out
}

/// Packages nothing tracks: an error to fix, after the updates.
fn print_untracked(state: &State) {
    if let Some(note) = &state.aur_unreachable {
        println!("note: {note}");
    }
    if state.untracked.is_empty() {
        return;
    }
    let n = state.untracked.len();
    println!(
        "untracked: {n} installed package{} that no repository or AUR package tracks",
        if n == 1 { "" } else { "s" }
    );
    for t in &state.untracked {
        let needed = needed_text(&t.name, &t.required_by, &t.optional_for, None, &[], 4, "minka-syu reason --json lists them all");
        println!(
            "  {} {} ({}; {needed}): {}",
            t.name,
            t.version,
            if t.explicit { "explicit" } else { "dependency" },
            t.hint
        );
    }
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
/// than pacman's own, or that switch to another repository's build of
/// the same version, so a plain `-Syu` would never select them. The
/// origin group's row stands for its members' targets, less the members
/// `ignore` names.
fn explicit_targets(state: &State, ignore: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for u in state.updates.iter().filter(|u| u.explicit_target && u.pending() && !ignore.contains(&u.name)) {
        if u.is_origin_group() {
            out.extend(u.origin_group.iter().filter(|m| !ignore.contains(&m.name)).map(check::OriginMember::target));
        } else {
            out.push(format!("{}/{}", u.candidate_repo, u.name));
        }
    }
    out
}

/// The origin group's row, if the state has one.
fn origin_row(state: &State) -> Option<&check::Update> {
    state.updates.iter().find(|u| u.is_origin_group())
}

/// A requested list with the origin group's own target (what the QML
/// window sends for its row: "<candidate_repo>/(no recorded repository)")
/// replaced by its members' targets, less the members `ignore` names.
fn expand_origin_target(state: &State, ignore: &[String], requested: &[String]) -> Vec<String> {
    let suffix = format!("/{}", check::ORIGIN_GROUP);
    let mut out = Vec::new();
    for t in requested {
        if t.ends_with(&suffix) {
            if let Some(row) = origin_row(state) {
                out.extend(row.origin_group.iter().filter(|m| !ignore.contains(&m.name)).map(check::OriginMember::target));
            }
        } else {
            out.push(t.clone());
        }
    }
    out
}

fn validate_targets(targets: &[String]) -> Result<(), String> {
    for t in targets {
        let Some((repo, name)) = t.split_once('/') else {
            return Err(format!("invalid upgrade target: {t:?}"));
        };
        if name.contains('/') || !valid_package_name(repo) || !valid_package_name(name) {
            return Err(format!("invalid upgrade target: {t:?}"));
        }
    }
    Ok(())
}

/// Install inputs may be plain names or the explicit `repo/name`
/// form the repository picker produces. Two builds of one package in
/// a single transaction is always a mistake, so reject duplicates
/// by package name whatever their repos.
fn validate_install_targets(targets: &[String]) -> Result<(), String> {
    if targets.is_empty() {
        return Err("no packages given for install".to_string());
    }
    let mut seen: Vec<&str> = Vec::new();
    for t in targets {
        let name = match t.split_once('/') {
            Some((repo, name)) => {
                if name.contains('/') || !valid_package_name(repo) || !valid_package_name(name) {
                    return Err(format!("invalid install target: {t:?}"));
                }
                name
            }
            None => {
                if !valid_package_name(t) {
                    return Err(format!("invalid package name for install: {t:?}"));
                }
                t.as_str()
            }
        };
        if seen.contains(&name) {
            return Err(format!("duplicate install target for package: {name}"));
        }
        seen.push(name);
    }
    Ok(())
}

/// Which explicit repo/name targets an apply carries. With no UI
/// override list, that is the state's ticked cross-repository
/// candidates. With one, it is authoritative -- but every target
/// must name a ticked update in the current state, at most once, so
/// a stale or forged list cannot smuggle arbitrary packages into a
/// sudo transaction.
fn resolve_apply_targets(
    state: &State,
    ignore: &[String],
    requested: Option<&[String]>,
) -> Result<Vec<String>, String> {
    let Some(requested) = requested else {
        let targets = explicit_targets(state, ignore);
        validate_targets(&targets)?;
        return Ok(targets);
    };
    let requested = &expand_origin_target(state, ignore, requested)[..];
    validate_targets(requested)?;
    let mut seen: Vec<String> = Vec::new();
    for t in requested {
        let (repo, name) = t.split_once('/').expect("validated above");
        if seen.iter().any(|n| n == name) {
            return Err(format!("duplicate upgrade target for package: {name}"));
        }
        if ignore.iter().any(|n| n == name) {
            return Err(format!("upgrade target is also ignored: {t}"));
        }
        // A member of the origin group: reinstalled only from the
        // repository whose copy is its build, and only with the group.
        if let Some(member) = origin_row(state).and_then(|r| r.origin_group.iter().find(|m| m.name == name)) {
            if ignore.iter().any(|n| n == check::ORIGIN_GROUP) {
                return Err(format!("{name} is in the unticked group of packages with no recorded repository: {t}"));
            }
            if repo != member.repo {
                return Err(format!("{name} is {}'s build, not {repo}'s: {t}", member.repo));
            }
            seen.push(name.to_string());
            continue;
        }
        let Some(update) = state.updates.iter().find(|u| u.name == name && !u.is_origin_group()) else {
            return Err(format!("upgrade target is not a pending update: {t}"));
        };
        if update.held {
            return Err(format!("{name} is held back by pacman.conf ({}): {t}", held_reason(update)));
        }
        // A replaced package's own update happens only once every
        // replacement that removes it is unticked (ignored).
        if let Some(by) = update.superseded_by.iter().find(|r| !ignore.contains(r)) {
            return Err(format!("{name} is replaced by {by} in this run: {t}"));
        }
        // A replacement happens only with the build that carries the
        // replaces entry. Any other repo's package of that name drops
        // the removal and ends in a conflict with the installed one.
        // (A later-repo row's own build is merely what ticking it would
        // replace; choosing another build for it is an ordinary choice.)
        if !update.explicit_target && !update.replaces.is_empty() && repo != update.candidate_repo {
            return Err(format!(
                "{name} replaces {} only from {}, not from {repo}",
                update.replaces.iter().map(|r| r.name.as_str()).collect::<Vec<_>>().join(", "),
                update.candidate_repo
            ));
        }
        seen.push(name.to_string());
    }
    Ok(requested.to_vec())
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

/// The terminal argv for an install. The window offers what the check's
/// copy of the sync DBs holds; when the system's own are older (no `-Sy`
/// since that check), plain `-S` reads those instead, and a package
/// published since is "target not found" (minkade, 24/11/2026) while an
/// older build's file may be gone from the mirror. Refreshing them
/// without upgrading is a partial upgrade, so the install then goes with
/// a full upgrade, the way Arch says to install against new lists:
/// `-Syu name`. paru shows the whole transaction before it starts;
/// IgnorePkg still holds packages back.
pub fn install_argv(packages: &[String], lists_current: bool) -> Vec<String> {
    let op = if lists_current { "-S" } else { "-Syu" };
    let mut argv = vec![terminal(), "-e".into(), "paru".into(), op.into(), "--sudoloop".into(), "--".into()];
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

fn spawn_terminal(argv: &[String]) -> Result<std::process::Child, String> {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    // Its own process group, so nothing that stops the window (a toolkit
    // tears down its children by process group) can take a running paru
    // or makepkg with it. Stdout is the serve pipe: never the terminal's.
    Command::new(&argv[0])
        .args(&argv[1..])
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawning terminal '{}': {e}", argv[0]))
}

/// Opens a terminal nobody waits on (the MinkaPKG bump, which holds its
/// own window open at the end).
fn launch(argv: &[String], dry_run: bool) -> Result<(), String> {
    if dry_run {
        println!("{}", argv.join(" "));
        return Ok(());
    }
    let mut child = spawn_terminal(argv)?;
    // Reaped in the background, or every launch would leave a zombie
    // behind for the life of `serve`.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Who hears how a step in the terminal ended.
#[derive(Clone, Copy)]
enum Report {
    /// The CLI: wait for it; a failure goes to stderr and the exit status.
    Wait,
    /// `serve`: the window, as {event:"run"}, from a thread of its own.
    Broadcast,
}

/// Opens the terminal on `argv` ([terminal, -e, paru…]) with paru run
/// through `minka-syu run`, and reports how it ended.
fn launch_step(argv: &[String], step: &str, dry_run: bool, report: Report) -> Result<(), String> {
    if dry_run {
        println!("{}", argv.join(" "));
        return Ok(());
    }
    let result = run::new_run(&run::runs_dir(), step)
        .map_err(|e| format!("cannot prepare {}: {e}", run::runs_dir().display()))?;
    let wrapped = run::wrap(argv, &run::self_exe(), step, &result);
    let mut child = spawn_terminal(&wrapped)?;
    let command: Vec<String> = argv.iter().skip(2).cloned().collect();
    let step = step.to_string();
    match report {
        Report::Wait => {
            let r = run::await_result(&mut child, &result, &step, &command, &run::Waiting::default());
            if !r.ok {
                return Err(run::report_text(&r));
            }
            if !r.excerpt.is_empty() {
                minka_note::note!("minka-syu: {}", run::report_text(&r));
            }
            // A failed step's window may still be open, held for Enter: it
            // is not ours to wait on, and this process ends here anyway.
            drop(child);
            Ok(())
        }
        Report::Broadcast => {
            std::thread::spawn(move || {
                let r = run::await_result(&mut child, &result, &step, &command, &run::Waiting::default());
                let event = serde_json::json!({ "event": "run", "payload": r });
                // One locked write per line: replies from other threads
                // never land in the middle of it.
                let mut out = std::io::stdout().lock();
                let _ = writeln!(out, "{event}");
                let _ = out.flush();
                drop(out);
                let _ = child.wait();
            });
            Ok(())
        }
    }
}

fn cmd_apply_state(dry_run: bool, ignore: &[String], state: &State) -> Result<(), String> {
    cmd_apply_state_targets(dry_run, ignore, None, state, Report::Wait)
}

/// The --ignore list paru gets: the unticked rows, less switch rows and
/// the origin group (its row and its members). A switch or a reinstall
/// happens only through its target, so unticking one just leaves the
/// target out; it is no upgrade to hold back, and --ignore would make
/// pacman treat the package as IgnorePkg for the whole run.
fn paru_ignore(state: &State, ignore: &[String]) -> Vec<String> {
    let origin = origin_row(state).map(|r| r.origin_group.as_slice()).unwrap_or_default();
    ignore
        .iter()
        .filter(|n| {
            n.as_str() != check::ORIGIN_GROUP
                && !origin.iter().any(|m| m.name == **n)
                && !state.updates.iter().any(|u| u.switch_build && u.name == **n)
        })
        .cloned()
        .collect()
}

/// The terminal argv of an apply: unticked rows as --ignore (see
/// paru_ignore), ticked explicit rows as repo/name targets. Neither
/// --needed (it would skip the same-version reinstalls) nor --asdeps or
/// --asexplicit (they would set every target's install reason): a sync
/// reinstall keeps each package's reason as it is.
fn apply_step_argv(ignore: &[String], requested: Option<&[String]>, state: &State) -> Result<Vec<String>, String> {
    let paru = paru_ignore(state, ignore);
    if !paru.is_empty() {
        validate_names(&paru, "--ignore")?;
    }
    let targets = resolve_apply_targets(state, ignore, requested)?;
    Ok(upgrade_argv_with_targets(&paru, &targets))
}

fn cmd_apply_state_targets(
    dry_run: bool,
    ignore: &[String],
    requested: Option<&[String]>,
    state: &State,
    report: Report,
) -> Result<(), String> {
    launch_step(&apply_step_argv(ignore, requested, state)?, "apply", dry_run, report)
}

fn cmd_apply(dry_run: bool, ignore: &[String]) -> Result<(), String> {
    cmd_apply_state(dry_run, ignore, &state_or_empty())
}

fn cmd_packages(install: bool, dry_run: bool, packages: &[String], report: Report) -> Result<(), String> {
    if install {
        validate_install_targets(packages)?;
    } else {
        validate_names(packages, "remove")?;
    }
    let argv = if install {
        install_argv(packages, check::system_lists_match_check())
    } else {
        remove_argv(packages)
    };
    launch_step(&argv, if install { "install" } else { "remove" }, dry_run, report)
}

/// The terminal argv that marks an installed package a dependency or
/// explicit: `sudo pacman -D`, which paru would only pass on to it.
pub fn reason_argv(package: &str, explicit: bool) -> Vec<String> {
    vec![
        terminal(),
        "-e".into(),
        "sudo".into(),
        "pacman".into(),
        "-D".into(),
        if explicit { "--asexplicit" } else { "--asdeps" }.into(),
        "--".into(),
        package.to_string(),
    ]
}

/// Changes one installed package's install reason, in the terminal, as the
/// "reason" step (run.rs reports how it ended).
fn cmd_set_reason(package: &str, explicit: bool, dry_run: bool, report: Report) -> Result<(), String> {
    if !valid_package_name(package) {
        return Err(format!("invalid package name: {package:?}"));
    }
    if !catalog::run_local_package(package).installed {
        return Err(format!("{package} is not installed"));
    }
    launch_step(&reason_argv(package, explicit), "reason", dry_run, report)
}

/// `reason [--json] <pkg>` shows; with --asdeps or --asexplicit, changes.
fn cmd_reason(args: &[&str]) -> Result<(), String> {
    let names = positional(args);
    let [package] = names.as_slice() else {
        return Err("reason takes one package".to_string());
    };
    let (deps, explicit) = (args.contains(&"--asdeps"), args.contains(&"--asexplicit"));
    if deps && explicit {
        return Err("reason: --asdeps or --asexplicit, not both".to_string());
    }
    if deps || explicit {
        return cmd_set_reason(package, explicit, args.contains(&"--dry-run"), Report::Wait);
    }
    if !valid_package_name(package) {
        return Err(format!("invalid package name: {package:?}"));
    }
    let local = catalog::run_local_package(package);
    if args.contains(&"--json") {
        println!("{}", serde_json::to_string_pretty(&local).map_err(|e| e.to_string())?);
        return Ok(());
    }
    if !local.installed {
        return Err(format!("{package} is not installed"));
    }
    let needs = (local.all_required_by.as_slice(), local.needed_by_explicit.as_slice(), local.covered_by.as_slice());
    let needed =
        needed_text(&local.name, &local.required_by, &local.optional_for, Some(needs), &local.shared_provides, 6, "--json lists them all");
    println!(
        "{} {}: {} ({needed})",
        local.name,
        local.version,
        if local.explicit { "explicit" } else { "dependency" }
    );
    Ok(())
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

fn cmd_repos(package: &str) -> Result<(), String> {
    if !valid_package_name(package) {
        return Err(format!("invalid package name: {package:?}"));
    }
    let options = catalog::run_package_repos(package);
    if options.is_empty() {
        return Err(format!("{package} is not offered by any repository"));
    }
    for o in options {
        println!(
            "{} {}{} {}{}{}",
            o.target,
            o.version,
            if o.architecture.is_empty() { String::new() } else { format!(" [{}]", o.architecture) },
            if o.is_default { " (pacman default)" } else { "" },
            if o.is_newest { " (newest)" } else { "" },
            if o.is_installed { " (installed)" } else { "" }
        );
    }
    Ok(())
}

fn cmd_sources(refresh: bool) -> Result<(), String> {
    let s = sources::sources(refresh)?;
    println!("{}", serde_json::to_string_pretty(&s).map_err(|e| e.to_string())?);
    Ok(())
}

/// `compare <pkg> <repo-a> <repo-b>`: each tree's directory and where it
/// came from, then meld on the two, unless --print.
fn cmd_compare(args: &[String], print: bool) -> Result<(), String> {
    let [package, left, right] = args else {
        return Err("compare takes a package and two repositories (aur for the AUR)".to_string());
    };
    let c = compare::compare(package, left, right)?;
    for side in [&c.left, &c.right] {
        println!("{}", side.dir);
        if print {
            for line in &side.provenance {
                println!("  {line}");
            }
        } else {
            println!("  {}", side.matched);
        }
    }
    if print { Ok(()) } else { compare::open_meld(&c) }
}

/// The window: $MINKA_SYU_GUI, else this binary run as `minka-syu-gui`.
/// MinkaDE's one binary holds both, so the window is always this build.
fn gui_command() -> Command {
    use std::os::unix::process::CommandExt;
    match std::env::var_os("MINKA_SYU_GUI").filter(|v| !v.is_empty()) {
        Some(p) => Command::new(p),
        None => {
            let mut gui = Command::new("/proc/self/exe");
            gui.arg0("minka-syu-gui");
            gui
        }
    }
}

/// Replaces this process with the window, so whoever launched it (the
/// shell's updates pill, the .desktop entry) owns the window process.
fn cmd_ui() -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    let err = gui_command().exec();
    Err(format!("cannot start the window: {err}"))
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
        ..Default::default()
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
            // The UI's chosen repo/name targets, when it sends the
            // key at all (even empty), replace the state-derived set.
            let requested;
            let requested_ref = if params.get("targets").is_some() {
                requested = string_array(params, "targets");
                Some(requested.as_slice())
            } else {
                None
            };
            cmd_apply_state_targets(false, &ignore, requested_ref, state, Report::Broadcast).map_err(error_code)?;
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
        "package_repos" => {
            let package = params
                .get("package")
                .and_then(|v| v.as_str())
                .ok_or_else(|| error_code("package_repos needs a package".to_string()))?;
            if !valid_package_name(package) {
                return Err(error_code(format!("invalid package name: {package:?}")));
            }
            serde_json::to_value(catalog::run_package_repos(package))
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
            cmd_packages(true, false, &packages, Report::Broadcast).map_err(error_code)?;
            Ok(serde_json::Value::Null)
        }
        "remove" => {
            let packages = string_array(params, "packages");
            cmd_packages(false, false, &packages, Report::Broadcast).map_err(error_code)?;
            Ok(serde_json::Value::Null)
        }
        "local_package" => {
            let package = params
                .get("package")
                .and_then(|v| v.as_str())
                .ok_or_else(|| error_code("local_package needs a package".to_string()))?;
            if !valid_package_name(package) {
                return Err(error_code(format!("invalid package name: {package:?}")));
            }
            serde_json::to_value(catalog::run_local_package(package)).map_err(|e| Error::internal(e.to_string()))
        }
        "set_reason" => {
            let package = params
                .get("package")
                .and_then(|v| v.as_str())
                .ok_or_else(|| error_code("set_reason needs a package".to_string()))?;
            let explicit = params
                .get("explicit")
                .and_then(|v| v.as_bool())
                .ok_or_else(|| error_code("set_reason needs explicit: true|false".to_string()))?;
            cmd_set_reason(package, explicit, false, Report::Broadcast).map_err(error_code)?;
            Ok(serde_json::Value::Null)
        }
        "sources" => {
            let refresh = params.get("refresh").and_then(|v| v.as_bool()).unwrap_or(false);
            let found = sources::sources(refresh).map_err(Error::internal)?;
            serde_json::to_value(found).map_err(|e| Error::internal(e.to_string()))
        }
        "sources_track" => {
            let package = params
                .get("package")
                .and_then(|v| v.as_str())
                .ok_or_else(|| error_code("sources_track needs a package".to_string()))?;
            if !valid_package_name(package) {
                return Err(error_code(format!("invalid package name: {package:?}")));
            }
            let tracked = params
                .get("tracked")
                .and_then(|v| v.as_bool())
                .ok_or_else(|| error_code("sources_track needs tracked: true|false".to_string()))?;
            let found = sources::set_tracked(package, tracked).map_err(error_code)?;
            serde_json::to_value(found).map_err(|e| Error::internal(e.to_string()))
        }
        "compare" => {
            let text = |key: &str| {
                params.get(key).and_then(|v| v.as_str()).ok_or_else(|| error_code(format!("compare needs {key}")))
            };
            let c = compare::compare(text("package")?, text("left")?, text("right")?).map_err(error_code)?;
            compare::open_meld(&c).map_err(error_code)?;
            serde_json::to_value(c).map_err(|e| Error::internal(e.to_string()))
        }
        "sources_update" => {
            let packages = string_array(params, "packages");
            if packages.is_empty() {
                return Err(error_code("sources_update needs packages".to_string()));
            }
            validate_names(&packages, "sources_update").map_err(error_code)?;
            let minkarepo = sources::find_minkarepo()
                .ok_or_else(|| error_code("no minkarepo found".to_string()))?;
            launch(&sources::update_argv(&terminal(), &minkarepo, &packages), false).map_err(error_code)?;
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

/// Methods that only run minkarepo or fetch sources (network, seconds to
/// tens of seconds) get a thread of their own, so they never hold up the
/// update list. Every other method stays in order on the main loop:
/// check, search and the catalog share the throwaway sync databases
/// (compare only reads them, with one `pacman -Sp` per side).
const THREADED: &[&str] = &["sources", "sources_track", "compare"];

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
        let method = serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .and_then(|m| m.get("method").and_then(|v| v.as_str()).map(str::to_string));
        if method.as_deref().is_some_and(|m| THREADED.contains(&m)) {
            // Stdout's write_fmt holds its lock for the whole reply line,
            // so replies from threads never interleave mid-line.
            let line = line.to_string();
            std::thread::spawn(move || {
                let mut scratch = state_or_empty();
                let mut out = std::io::stdout();
                handle_line(&mut scratch, &line, &mut out);
                let _ = out.flush();
            });
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
        let i = install_argv(&["ripgrep".into()], true);
        assert_eq!(&i[2..], ["paru", "-S", "--sudoloop", "--", "ripgrep"]);
        let i = install_argv(&["minka/minkade".into()], false);
        assert_eq!(&i[2..], ["paru", "-Syu", "--sudoloop", "--", "minka/minkade"]);
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
            newer_repo: None,
            newer_version: None,
            newer_replaces: Vec::new(),
            replaces: Vec::new(),
            held: false,
            held_by: None,
            superseded_by: Vec::new(),
            new_install: false,
            reinstalls: Vec::new(),
            switch_build: false,
            origin_group: Vec::new(),
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
            ..Default::default()
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
    fn requested_targets_must_be_ticked_updates() {
        let upd = |name: &str| check::Update {
            name: name.into(),
            old_version: "1-1".into(),
            new_version: "1-2".into(),
            installed_repo: None,
            candidate_repo: "extra".into(),
            transition: check::Transition::UnknownSource,
            explicit_target: false,
            first_repo: None,
            first_version: None,
            newer_repo: None,
            newer_version: None,
            newer_replaces: Vec::new(),
            replaces: Vec::new(),
            held: false,
            held_by: None,
            superseded_by: Vec::new(),
            new_install: false,
            reinstalls: Vec::new(),
            switch_build: false,
            origin_group: Vec::new(),
        };
        let state = State {
            schema: 1,
            checked_at: 0,
            updates: vec![upd("mesa"), upd("sqlite")],
            repo_change_count: 0,
            last_notified_count: 0,
            ..Default::default()
        };
        // A manual repo choice for a pending, ticked update is fine.
        assert_eq!(
            resolve_apply_targets(&state, &[], Some(&["core/sqlite".to_string()])).unwrap(),
            ["core/sqlite"]
        );
        // Ignored, unknown, and duplicated targets are not.
        assert!(resolve_apply_targets(&state, &["sqlite".into()], Some(&["core/sqlite".into()])).is_err());
        assert!(resolve_apply_targets(&state, &[], Some(&["core/notpending".into()])).is_err());
        assert!(resolve_apply_targets(
            &state,
            &[],
            Some(&["core/sqlite".into(), "extra/sqlite".into()])
        )
        .is_err());
        // Install takes repo-qualified targets but not two builds.
        assert!(validate_install_targets(&["extra/ardour".into(), "ripgrep".into()]).is_ok());
        assert!(validate_install_targets(&["extra/ardour".into(), "aur/ardour".into()]).is_err());
    }

    #[test]
    fn a_noted_newer_build_is_a_choice_not_the_default() {
        // nvidia-settings on 23/11/2026: pacman's own upgrade, from
        // cachyos-v4, and ALHP's suffix rebuild noted on the row.
        let upd = |name: &str| check::Update {
            name: name.into(),
            old_version: "615.78.08-1".into(),
            new_version: "620.73.04-1".into(),
            installed_repo: Some("cachyos-v4".into()),
            candidate_repo: "cachyos-v4".into(),
            transition: check::Transition::Same,
            explicit_target: false,
            first_repo: None,
            first_version: None,
            newer_repo: Some("extra-x86-64-v4".into()),
            newer_version: Some("620.73.04-1.1".into()),
            newer_replaces: Vec::new(),
            replaces: Vec::new(),
            held: false,
            held_by: None,
            superseded_by: Vec::new(),
            new_install: false,
            reinstalls: Vec::new(),
            switch_build: false,
            origin_group: Vec::new(),
        };
        let state = State { schema: 1, updates: vec![upd("nvidia-settings")], ..Default::default() };
        assert_eq!(
            update_line(&state.updates[0]),
            "nvidia-settings 615.78.08-1 -> 620.73.04-1  [cachyos-v4; newer build in extra-x86-64-v4: 620.73.04-1.1]"
        );
        // A plain apply upgrades it from cachyos-v4: no target, no --ignore.
        let argv = apply_step_argv(&[], None, &state).unwrap();
        assert_eq!(&argv[2..], ["paru", "-Syu", "--sudoloop"]);
        // The window's choice of the newer build is a target the core takes.
        let chosen = ["extra-x86-64-v4/nvidia-settings".to_string()];
        let argv = apply_step_argv(&[], Some(&chosen), &state).unwrap();
        assert_eq!(&argv[2..], ["paru", "-Syu", "--sudoloop", "extra-x86-64-v4/nvidia-settings"]);
        // From another repository, the line says so.
        let moved = check::Update { candidate_repo: "extra".into(), transition: check::Transition::Changed, ..upd("x") };
        assert_eq!(
            update_line(&moved),
            "x 615.78.08-1 -> 620.73.04-1  [repo: cachyos-v4 -> extra; newer build in extra-x86-64-v4: 620.73.04-1.1]"
        );
        let none = check::Update { newer_repo: None, newer_version: None, ..upd("x") };
        assert_eq!(update_line(&none), "x 615.78.08-1 -> 620.73.04-1  [cachyos-v4]");
        // What the noted build would replace: choosing it removes that.
        let replacing = check::Update {
            newer_replaces: vec![check::Replaced { name: "zm".into(), version: "1.0-1".into(), installed_repo: None }],
            ..upd("x")
        };
        assert_eq!(
            update_line(&replacing),
            "x 615.78.08-1 -> 620.73.04-1  [cachyos-v4; newer build in extra-x86-64-v4: 620.73.04-1.1, which replaces zm 1.0-1]"
        );
    }

    #[test]
    fn replacements_apply_only_from_their_own_repo() {
        let replacement = check::Update {
            name: "coreutils-uutils".into(),
            old_version: "9.4_0.0.28-1".into(),
            new_version: "0.12.0-1".into(),
            installed_repo: None,
            candidate_repo: "minka".into(),
            transition: check::Transition::UnknownSource,
            explicit_target: false,
            first_repo: None,
            first_version: None,
            newer_repo: None,
            newer_version: None,
            newer_replaces: Vec::new(),
            replaces: vec![check::Replaced {
                name: "coreutils-hybrid".into(),
                version: "9.4_0.0.28-1".into(),
                installed_repo: None,
            }],
            held: false,
            held_by: None,
            superseded_by: Vec::new(),
            new_install: false,
            reinstalls: Vec::new(),
            switch_build: false,
            origin_group: Vec::new(),
        };
        let state = State {
            schema: 1,
            checked_at: 0,
            updates: vec![replacement],
            repo_change_count: 0,
            last_notified_count: 0,
            ..Default::default()
        };
        // Plain -Syu performs it: no explicit target is derived.
        assert!(explicit_targets(&state, &[]).is_empty());
        assert!(resolve_apply_targets(&state, &[], None).unwrap().is_empty());
        // Its own repo is fine; the same-named [cachyos] build is not.
        assert!(resolve_apply_targets(&state, &[], Some(&["minka/coreutils-uutils".into()])).is_ok());
        let err = resolve_apply_targets(&state, &[], Some(&["cachyos/coreutils-uutils".into()])).unwrap_err();
        assert!(err.contains("only from minka"), "{err}");
        // A held update is shown but never targeted.
        let mut held_state = state.clone();
        held_state.updates[0].replaces.clear();
        held_state.updates[0].held = true;
        held_state.updates[0].held_by = Some("ignore".into());
        assert_eq!(held_state.total(), 0);
        assert_eq!(held_state.held(), 1);
        assert!(resolve_apply_targets(&held_state, &[], Some(&["minka/coreutils-uutils".into()])).is_err());
        // The replaced package's own row: only once the replacer is ignored.
        let mut both = state.clone();
        both.updates.push(check::Update {
            name: "coreutils-hybrid".into(),
            old_version: "9.4_0.0.28-1".into(),
            new_version: "9.5-1".into(),
            installed_repo: None,
            candidate_repo: "minka".into(),
            transition: check::Transition::UnknownSource,
            explicit_target: false,
            first_repo: None,
            first_version: None,
            newer_repo: None,
            newer_version: None,
            newer_replaces: Vec::new(),
            replaces: Vec::new(),
            held: false,
            held_by: None,
            superseded_by: vec!["coreutils-uutils".into()],
            new_install: false,
            reinstalls: Vec::new(),
            switch_build: false,
            origin_group: Vec::new(),
        });
        assert_eq!(both.total(), 1);
        let hybrid = ["minka/coreutils-hybrid".to_string()];
        assert!(resolve_apply_targets(&both, &[], Some(&hybrid)).is_err());
        assert!(resolve_apply_targets(&both, &["coreutils-uutils".into()], Some(&hybrid)).is_ok());
        // Unticking it is an ordinary --ignore, which skips the replacement.
        let argv = upgrade_argv(&["coreutils-uutils".into()]);
        assert_eq!(&argv[2..], ["paru", "-Syu", "--sudoloop", "--ignore=coreutils-uutils"]);
    }

    #[test]
    fn unticked_switches_are_left_out_and_never_ignored() {
        let switch = check::Update {
            name: "lib32-libcaca".into(),
            old_version: "0.99.beta20-2".into(),
            new_version: "0.99.beta20-2".into(),
            installed_repo: Some("multilib".into()),
            candidate_repo: "minka".into(),
            transition: check::Transition::Changed,
            explicit_target: true,
            first_repo: None,
            first_version: None,
            newer_repo: None,
            newer_version: None,
            newer_replaces: Vec::new(),
            replaces: Vec::new(),
            held: false,
            held_by: None,
            superseded_by: Vec::new(),
            new_install: false,
            reinstalls: Vec::new(),
            switch_build: true,
            origin_group: Vec::new(),
        };
        let mesa = check::Update {
            name: "mesa".into(),
            new_version: "2-1".into(),
            candidate_repo: "extra".into(),
            transition: check::Transition::Same,
            explicit_target: false,
            switch_build: false,
            ..switch.clone()
        };
        let state = State {
            schema: 1,
            checked_at: 0,
            updates: vec![switch, mesa],
            repo_change_count: 1,
            last_notified_count: 0,
            ..Default::default()
        };
        let tail = |argv: Vec<String>| argv[2..].to_vec();
        let ignore = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        assert_eq!(state.total(), 2);
        // Ticked (the CLI's default): its repo/name target rides along.
        assert_eq!(
            tail(apply_step_argv(&[], None, &state).unwrap()),
            ["paru", "-Syu", "--sudoloop", "minka/lib32-libcaca"]
        );
        // Unticked: no target, and no --ignore for it either.
        assert_eq!(
            tail(apply_step_argv(&ignore(&["lib32-libcaca", "mesa"]), None, &state).unwrap()),
            ["paru", "-Syu", "--sudoloop", "--ignore=mesa"]
        );
        // The window's own target list behaves the same.
        assert_eq!(
            tail(apply_step_argv(&ignore(&["lib32-libcaca"]), Some(&[]), &state).unwrap()),
            ["paru", "-Syu", "--sudoloop"]
        );
        assert_eq!(
            tail(apply_step_argv(&[], Some(&["minka/lib32-libcaca".into()]), &state).unwrap()),
            ["paru", "-Syu", "--sudoloop", "minka/lib32-libcaca"]
        );
        // Held by IgnorePkg: never targeted, and not counted.
        let mut held = state.clone();
        held.updates[0].held = true;
        held.updates[0].held_by = Some("ignore".into());
        assert_eq!(held.total(), 1);
        assert_eq!(tail(apply_step_argv(&[], None, &held).unwrap()), ["paru", "-Syu", "--sudoloop"]);
        assert!(apply_step_argv(&[], Some(&["minka/lib32-libcaca".into()]), &held).is_err());
    }

    fn origin_state() -> State {
        let member = |name: &str, repo: &str, explicit: bool| check::OriginMember {
            name: name.into(),
            version: "1-1".into(),
            repo: repo.into(),
            explicit,
        };
        let group = check::Update {
            name: check::ORIGIN_GROUP.into(),
            old_version: "2 packages".into(),
            new_version: "same builds".into(),
            installed_repo: None,
            candidate_repo: "extra, multilib".into(),
            transition: check::Transition::UnknownSource,
            explicit_target: true,
            first_repo: None,
            first_version: None,
            newer_repo: None,
            newer_version: None,
            newer_replaces: Vec::new(),
            replaces: Vec::new(),
            held: false,
            held_by: None,
            superseded_by: Vec::new(),
            new_install: false,
            reinstalls: Vec::new(),
            switch_build: false,
            origin_group: vec![member("zstd", "extra", false), member("lib32-zstd", "multilib", false)],
        };
        let mesa = check::Update {
            name: "mesa".into(),
            old_version: "1-1".into(),
            new_version: "2-1".into(),
            installed_repo: Some("extra".into()),
            candidate_repo: "extra".into(),
            transition: check::Transition::Same,
            explicit_target: false,
            origin_group: Vec::new(),
            ..group.clone()
        };
        State { schema: 1, updates: vec![group, mesa], ..Default::default() }
    }

    #[test]
    fn the_origin_group_rides_along_as_member_targets() {
        let state = origin_state();
        let tail = |argv: Vec<String>| argv[2..].to_vec();
        let names = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        assert_eq!(state.total(), 2, "the group is one update");
        // The CLI's default, like a switch row: every member's target.
        let all = ["paru", "-Syu", "--sudoloop", "extra/zstd", "multilib/lib32-zstd"];
        assert_eq!(tail(apply_step_argv(&[], None, &state).unwrap()), all);
        // A member named in --ignore is left out, and never passed on.
        assert_eq!(
            tail(apply_step_argv(&names(&["zstd", "mesa"]), None, &state).unwrap()),
            ["paru", "-Syu", "--sudoloop", "--ignore=mesa", "multilib/lib32-zstd"]
        );
        // The whole group unticked: no targets, no --ignore for it.
        assert_eq!(
            tail(apply_step_argv(&names(&[check::ORIGIN_GROUP]), None, &state).unwrap()),
            ["paru", "-Syu", "--sudoloop"]
        );
        // The window sends the members' targets.
        assert_eq!(
            tail(apply_step_argv(&[], Some(&names(&["multilib/lib32-zstd"])), &state).unwrap()),
            ["paru", "-Syu", "--sudoloop", "multilib/lib32-zstd"]
        );
        // The QML window sends the row's own target: its members, then.
        let qml = names(&[&format!("extra, multilib/{}", check::ORIGIN_GROUP)]);
        assert_eq!(tail(apply_step_argv(&names(&["lib32-zstd"]), Some(&qml), &state).unwrap()), all[..4]);
        assert_eq!(tail(apply_step_argv(&[], Some(&qml), &state).unwrap()), all);
        // A member from another repository, or with the group unticked, is refused.
        assert!(apply_step_argv(&[], Some(&names(&["cachyos/zstd"])), &state).is_err());
        assert!(apply_step_argv(&names(&[check::ORIGIN_GROUP]), Some(&names(&["extra/zstd"])), &state).is_err());
        // Nor is the row's name ever a target of its own.
        assert!(apply_step_argv(&[], Some(&names(&["extra/(no"])), &state).is_err());
        // Reinstalls keep the install reason only without these.
        let argv = apply_step_argv(&[], None, &state).unwrap();
        assert!(!argv.iter().any(|a| a == "--needed" || a == "--asdeps" || a == "--asexplicit"), "{argv:?}");
    }

    #[test]
    fn reason_changes_run_pacman_d_through_the_runner() {
        let deps = reason_argv("python-lockfile", false);
        assert_eq!(&deps[1..], ["-e", "sudo", "pacman", "-D", "--asdeps", "--", "python-lockfile"]);
        let explicit = reason_argv("libwpe", true);
        assert_eq!(&explicit[2..], ["sudo", "pacman", "-D", "--asexplicit", "--", "libwpe"]);
        // launch_step's wrapping: the terminal runs `minka minka-syu run
        // --step reason`, which records it and reports how it ended.
        let wrapped = run::wrap(&deps, std::path::Path::new("/usr/bin/minka"), "reason", std::path::Path::new("/s/r.json"));
        assert_eq!(
            &wrapped[1..],
            [
                "-e", "/usr/bin/minka", "minka-syu", "run", "--step", "reason", "--result", "/s/r.json", "--", "sudo",
                "pacman", "-D", "--asdeps", "--", "python-lockfile"
            ]
        );
        assert_eq!(run::step_label("reason"), "Install reason change");
        // Names are checked before anything reaches sudo.
        assert!(cmd_set_reason("-Rns", true, true, Report::Wait).is_err());
        assert!(cmd_reason(&["--asdeps", "--asexplicit", "x"]).is_err());
        assert!(cmd_reason(&["--asdeps"]).is_err());
    }

    #[test]
    fn cli_lists_wrap_and_cap() {
        let names: Vec<String> = (0..30).map(|i| format!("perl-module-{i:02}")).collect();
        let words: Vec<&str> = names.iter().map(String::as_str).collect();
        let lines = wrap_words("    extra: ", &words, NAMES_WIDTH);
        assert!(lines.len() > 1 && lines.iter().all(|l| l.chars().count() <= NAMES_WIDTH), "{lines:#?}");
        assert!(lines[0].starts_with("    extra: perl-module-00 "));
        assert!(lines[1].starts_with(&format!("{}perl-module-", " ".repeat(11))), "{lines:#?}");
        // Every name is kept.
        let joined = lines.iter().map(|l| l.trim()).collect::<Vec<_>>().join(" ");
        assert_eq!(joined.split_whitespace().filter(|w| w.starts_with("perl-")).count(), 30);
        assert_eq!(wrap_words("    a: ", &["x"], 10), ["    a: x"]);
        let many: Vec<String> = (0..9).map(|i| format!("p{i}")).collect();
        assert_eq!(needed_text("pkg", &[], &[], None, &[], 6, "x"), "nothing requires it");
        assert_eq!(
            needed_text("pkg", &many, &[], None, &[], 6, "--json lists them all"),
            "required by p0, p1, p2, p3, p4, p5 and 3 more; --json lists them all"
        );
        assert_eq!(needed_text("pkg", &[], &["yt-dlp".into()], None, &[], 6, "x"), "nothing requires it; optional for yt-dlp");
        // What `reason` adds: who needs it explicitly, said once.
        let (none, many): (&[String], &[String]) = (&[], &many);
        assert_eq!(needed_text("pkg", &[], &[], Some((none, none, none)), &[], 6, "x"), "nothing requires it");
        let one: &[String] = &["git".to_string()];
        assert_eq!(
            needed_text("pkg", one, &[], Some((one, one, none)), &[], 6, "x"),
            "required by git; needed by 1 explicitly installed package: git"
        );
        assert_eq!(
            needed_text("pkg", one, &[], Some((many, none, none)), &[], 6, "full"),
            "required by git; nothing you installed explicitly needs it, only the dependencies p0, p1, p2, p3, p4, p5 and 3 more; full"
        );
        assert_eq!(
            needed_text("pkg", one, &[], Some((one, none, none)), &[], 6, "x"),
            "required by git; nothing you installed explicitly needs it, only the dependency git"
        );
        assert_eq!(
            needed_text("pkg", one, &[], Some((none, none, none)), &[], 6, "x"),
            "required by git; nothing needs it: another installed package satisfies what requires it"
        );
        assert_eq!(
            needed_text("pkg", one, &[], Some((many, &many[..2], none)), &[], 6, "x"),
            "required by git; needed by 2 explicitly installed packages: p0, p1",
            "the uncut explicit list is all that is shown"
        );
        // It depends on itself, and nothing else requires it: no clause on
        // what satisfies its requirers, as there are none.
        assert_eq!(needed_text("git", one, &[], Some((none, none, none)), &[], 6, "x"), "nothing else requires it");
        let with_self = ["git".to_string(), "p0".to_string()];
        assert_eq!(
            needed_text("git", &with_self, &[], Some((&many[..1], none, none)), &[], 6, "x"),
            "required by p0; nothing you installed explicitly needs it, only the dependency p0"
        );
        // What satisfies the packages that require it without needing it.
        let utils: &[String] = &["ca-certificates-utils".to_string()];
        assert_eq!(
            needed_text("ca-certificates", one, &[], Some((none, none, utils)), &[], 6, "x"),
            "required by git; nothing needs it: ca-certificates-utils also provides what requires it"
        );
        assert_eq!(
            needed_text("pkg", one, &[], Some((none, none, many)), &[], 6, "full"),
            "required by git; nothing needs it: p0, p1, p2, p3, p4, p5 and 3 more also provide what requires it; full"
        );
        let jdks: &[String] = &["jdk11-openjdk".to_string(), "jdk25-graalvm-bin".to_string()];
        assert_eq!(
            needed_text("jdk21-openjdk", &with_self, &[], Some((&many[..1], none, jdks)), &[], 6, "x"),
            "required by git, p0; nothing you installed explicitly needs it, only the dependency p0; \
             others are covered by jdk11-openjdk, jdk25-graalvm-bin"
        );
        assert_eq!(
            needed_text("pkg", one, &[], Some((one, one, &many[..7])), &[], 6, "full"),
            "required by git; needed by 1 explicitly installed package: git; others are covered by p0, p1, p2, p3, p4, p5 \
             and 1 more; full"
        );
        // The names it offers that other installed packages offer too, said
        // even when nothing requires it, but not again when what covers its
        // requirers has said it.
        let shared = |name: &str, also: &[String]| alpm::SharedProvide { name: name.into(), also: also.to_vec() };
        assert_eq!(
            needed_text("ca-certificates", one, &[], Some((none, none, utils)), &[shared("ca-certificates", utils)], 6, "x"),
            "required by git; nothing needs it: ca-certificates-utils also provides what requires it"
        );
        assert_eq!(
            needed_text("ca-certificates", &[], &[], Some((none, none, none)), &[shared("ca-certificates", utils)], 6, "x"),
            "nothing requires it; ca-certificates is also provided by ca-certificates-utils"
        );
        // From the provider's side the name is a package's own.
        let certs = ["ca-certificates".to_string()];
        assert_eq!(
            needed_text("ca-certificates-utils", one, &[], Some((one, one, none)), &[shared("ca-certificates", &certs)], 6, "x"),
            "required by git; needed by 1 explicitly installed package: git; ca-certificates is also an installed package"
        );
        let mut both = certs.to_vec();
        both.extend_from_slice(&many[..7]);
        assert_eq!(
            needed_text("x", &[], &[], Some((none, none, none)), &[shared("ca-certificates", &both)], 6, "full"),
            "nothing requires it; ca-certificates is also an installed package, and provided by p0, p1, p2, p3, p4, p5 \
             and 1 more; full"
        );
        let fonts: Vec<alpm::SharedProvide> = (0..8).map(|i| shared(&format!("ttf-font-{i}"), &many[..2])).collect();
        assert_eq!(
            needed_text("ttf-dejavu", &[], &[], Some((none, none, none)), &fonts[..1], 6, "full"),
            "nothing requires it; ttf-font-0 is also provided by p0, p1"
        );
        let line = needed_text("ttf-dejavu", &[], &[], Some((none, none, none)), &fonts, 6, "full");
        assert!(line.contains("ttf-font-5 is also provided by p0, p1; and 2 more names other packages offer too; full"), "{line}");
        assert!(!line.contains("ttf-font-6"), "{line}");
    }

    #[test]
    fn compare_requests_are_checked_before_anything_runs() {
        let mut state = state_or_empty();
        let mut ask = |params: serde_json::Value| dispatch(&mut state, "compare", &params).err().map(|e| (e.code, e.message));
        let missing = ask(serde_json::json!({"package": "perl", "left": "core"}));
        assert_eq!(missing, Some(("bad_request", "compare needs right".to_string())));
        let forged = ask(serde_json::json!({"package": "perl", "left": "core", "right": "--help"}));
        assert_eq!(forged, Some(("bad_request", "invalid repository name: \"--help\"".to_string())));
        let same = ask(serde_json::json!({"package": "perl", "left": "core", "right": "core"}));
        assert!(same.is_some_and(|(_, m)| m.starts_with("core twice")));
        assert!(THREADED.contains(&"compare"), "fetching sources must never hold up the update list");
    }

    #[test]
    fn ignore_cli_parsing() {
        assert_eq!(parse_ignore(&["--ignore", "a,b"]), ["a", "b"]);
        assert_eq!(parse_ignore(&["--ignore=a,b", "--dry-run"]), ["a", "b"]);
        assert!(parse_ignore(&["--dry-run"]).is_empty());
    }
}
