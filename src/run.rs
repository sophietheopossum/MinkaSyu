//! A step that changes the system (an upgrade, an install, a removal) and
//! how it ended.
//!
//! paru needs a terminal (sudo's password, its own questions), and the
//! terminal closes the moment paru exits, taking its errors with it. So the
//! terminal runs `minka-syu run …` instead (`run_verb`): that runs paru under
//! script(1), which writes everything paru shows to a log as well, then
//! records how the step ended in a result file and, when it failed or
//! printed error lines that still matter, keeps the window open until
//! Enter. Whoever opened the terminal waits for that file (`await_result`):
//! the CLI prints a failure to stderr and exits non-zero, and `serve` sends
//! it to the window as {event:"run"}, named by the step's id.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, IsTerminal};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How a step ended, as its result file and the {event:"run"} payload
/// carry it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RunResult {
    /// Which step this is: its result file's stem ("apply-<secs>-<pid>-<n>").
    /// Two steps can run at once, and their reports must not be confused.
    pub id: String,
    /// "apply", "install", "remove" or "reason" (pacman -D).
    pub step: String,
    /// What ran in the terminal: paru and its arguments.
    pub command: Vec<String>,
    /// It exited 0. False as well when no result came back at all.
    pub ok: bool,
    /// It did not run through because it was told not to: a question
    /// answered no, or Ctrl+C. Not ok, but not broken either.
    pub cancelled: bool,
    /// Its exit status. script(1) passes a child killed by signal N on
    /// as 128+N, as shells do.
    pub exit_code: Option<i32>,
    /// The signal that killed the recorder or the step itself.
    pub signal: Option<i32>,
    /// What happened, in one line.
    pub headline: String,
    /// The error lines of the whole output: pacman's and paru's `error:`
    /// lines with what pacman lists under them, and each makepkg
    /// `==> ERROR:` with the build lines before it. After an exit 0 only
    /// what still matters then (a failed hook, scriptlet or dkms build);
    /// after Ctrl+C only what came before it.
    pub excerpt: Vec<String>,
    /// The output after the last error line (all of its end, when there
    /// were none).
    pub tail: Vec<String>,
    /// The full output, when it was recorded and kept ("" otherwise).
    pub log: String,
}

/// The step's name in a headline.
pub fn step_label(step: &str) -> &str {
    match step {
        "apply" => "Upgrade",
        "install" => "Install",
        "remove" => "Removal",
        "reason" => "Install reason change",
        other => other,
    }
}

fn signal_name(signal: i32) -> &'static str {
    match signal {
        1 => " (SIGHUP)",
        2 => " (SIGINT)",
        3 => " (SIGQUIT)",
        6 => " (SIGABRT)",
        9 => " (SIGKILL)",
        11 => " (SIGSEGV)",
        13 => " (SIGPIPE)",
        15 => " (SIGTERM)",
        _ => "",
    }
}

/// Whether a result asks to be looked at: a failure, or error lines in
/// any outcome (a failed pacman hook does not fail the transaction). A
/// cancel the user made does not.
pub fn needs_attention(r: &RunResult) -> bool {
    (!r.ok && !r.cancelled) || !r.excerpt.is_empty()
}

/// A question on the last line, answered no or left at a default of no:
/// pacman's and paru's "[Y/n]" and "[y/N]".
fn declined(line: &str) -> bool {
    for (prompt, default_no) in [("[Y/n]", false), ("[y/N]", true), ("[y/n]", false)] {
        if let Some(at) = line.rfind(prompt) {
            let answer = line[at + prompt.len()..].trim_start_matches(':').trim();
            return answer.eq_ignore_ascii_case("n")
                || answer.eq_ignore_ascii_case("no")
                || (default_no && answer.is_empty());
        }
    }
    false
}

/// The last line of a recording cut off at a question, unanswered or
/// answered no: its window was closed to back out. sudo's password prompt
/// counts as one.
fn closed_at_question(line: &str) -> bool {
    let l = line.trim_end();
    for prompt in ["[Y/n]", "[y/N]", "[y/n]"] {
        if let Some(at) = l.rfind(prompt) {
            let answer = l[at + prompt.len()..].trim_start_matches(':').trim();
            return answer.is_empty() || answer.eq_ignore_ascii_case("n") || answer.eq_ignore_ascii_case("no");
        }
    }
    l.starts_with("[sudo] password for ") && l.ends_with(':')
}

/// The headline of a step that ran to an exit status or a signal.
pub fn headline(r: &RunResult) -> String {
    let label = step_label(&r.step);
    let program = r.command.first().map(String::as_str).unwrap_or("the step");
    if r.ok {
        return if r.excerpt.is_empty() {
            format!("{label} finished")
        } else {
            format!("{label} finished, but its output has error lines")
        };
    }
    if r.cancelled {
        return if r.exit_code == Some(130) || r.signal == Some(2) {
            format!("{label} cancelled with Ctrl+C")
        } else {
            format!("{label} cancelled at {program}'s question")
        };
    }
    let how = match (r.exit_code, r.signal) {
        (Some(code), _) if (129..=192).contains(&code) => {
            let signal = code - 128;
            format!("{program} ended with status {code}, killed by signal {signal}{}", signal_name(signal))
        }
        (Some(code), _) => format!("{program} exited with status {code}"),
        (None, Some(signal)) => format!("{program} was killed by signal {signal}{}", signal_name(signal)),
        (None, None) => return format!("{label} failed"),
    };
    match failed_builds(&r.excerpt).as_slice() {
        [] => format!("{label} failed: {how}"),
        names => format!("{label} failed: {} did not build ({how})", name_list(names)),
    }
}

/// The packages paru could not build, by base name, from its own lines:
/// "packages failed to build: a-1-1  b-2-1 (b b-docs)" at the end, or with
/// --failfast only the "failed to build 'a-1-1'" of the first.
fn failed_builds(excerpt: &[String]) -> Vec<String> {
    // "pkgbase-pkgver-pkgrel", with " (pkg pkg…)" after it for a split
    // package; neither pkgver nor pkgrel has a '-'.
    let base = |shown: &str| {
        let shown = shown.split(" (").next().unwrap_or(shown).trim();
        shown.rsplitn(3, '-').nth(2).unwrap_or(shown).to_string()
    };
    if let Some(list) = excerpt.iter().rev().find_map(|l| l.strip_prefix("error: packages failed to build: ")) {
        return list.split("  ").filter(|s| !s.trim().is_empty()).map(base).collect();
    }
    let mut names: Vec<String> = Vec::new();
    for line in excerpt {
        let Some(rest) = line.strip_prefix("error: ") else { continue };
        let shown = ["failed to build '", "failed to download sources for '"]
            .iter()
            .find_map(|p| rest.strip_prefix(p).and_then(|r| r.split('\'').next()))
            .or_else(|| rest.strip_prefix("can't build ").and_then(|r| r.split(", deps not satisfied").next()));
        if let Some(name) = shown.map(base).filter(|n| !names.contains(n)) {
            names.push(name);
        }
    }
    names
}

/// "a", "a and b", "a, b and c", "a, b, c and 2 more".
fn name_list(names: &[String]) -> String {
    const SHOWN: usize = 3;
    match names.split_last() {
        None => String::new(),
        Some((last, [])) => last.clone(),
        Some((last, rest)) if names.len() <= SHOWN => format!("{} and {last}", rest.join(", ")),
        Some(_) => format!("{} and {} more", names[..SHOWN].join(", "), names.len() - SHOWN),
    }
}

// ---- reading a terminal recording ----------------------------------------

/// One line of a terminal recording as it looked: what a carriage return
/// went back over is gone, and colour and cursor sequences are dropped.
pub fn clean_line(raw: &str) -> String {
    let raw = raw.trim_end_matches(['\r', '\n']);
    let visible = raw.rsplit('\r').next().unwrap_or(raw);
    let mut out = String::with_capacity(visible.len());
    let mut chars = visible.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                // CSI: parameters, then one final byte in @..~.
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC, DCS and the other strings: to BEL or ESC \.
                Some(']' | 'P' | 'X' | '^' | '_') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                // Character set selection takes one more character.
                Some('(' | ')' | '*' | '+') => {
                    chars.next();
                }
                _ => {}
            },
            '\u{8}' => {
                out.pop();
            }
            '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out.truncate(out.trim_end().len());
    out
}

fn is_make_error(l: &str) -> bool {
    l.starts_with("make") && l.contains(": *** ")
}

/// A line that says something failed, from pacman, paru, makepkg, git,
/// sudo, make, rustc or dkms.
fn is_error_line(line: &str) -> bool {
    let l = line.trim_start();
    l.starts_with("error:")
        || l.starts_with("error[")
        || l.starts_with("==> ERROR:")
        || l.starts_with("Errors occurred")
        || l.starts_with("fatal:")
        || l.starts_with("sudo:")
        || l.starts_with("Error!")
        || is_make_error(l)
}

/// The error lines that still matter when the step exited 0: a pacman hook
/// or install scriptlet that failed (pacman goes on, and its exit status
/// says nothing of it), dkms failing to build a module, mkinitcpio's
/// errors. Not a mirror pacman gave up on for the next one ("failed
/// retrieving file"), nor git's or make's noise from a build that worked.
fn matters_despite_success(line: &str) -> bool {
    let l = line.trim_start();
    l.starts_with("error: command failed to execute correctly") || l.starts_with("Error!") || l.starts_with("==> ERROR:")
}

/// paru's and pacman's own lines that say what failed: the packages that
/// did not build, the transaction that did not happen. However much build
/// output comes before them, they go in the excerpt.
fn is_summary_line(line: &str) -> bool {
    if line.starts_with("Errors occurred") {
        return true;
    }
    let Some(rest) = line.strip_prefix("error: ") else {
        return false;
    };
    [
        "packages failed to build:",
        "failed to build '",
        "failed to download sources for '",
        "can't build ",
        "could not find all required packages",
        "target not found:",
        "failed to init transaction",
        "failed to prepare transaction",
        "failed to commit transaction",
        "failed to synchronize all databases",
        "failed to retrieve some files",
        "unresolvable package conflicts",
    ]
    .iter()
    .any(|p| rest.starts_with(p))
        || rest.contains("could not lock database")
}

/// The first sign of a Ctrl+C in a recording: the terminal's echo of it,
/// makepkg's trap, pacman's handler, make's "Interrupt". What comes after
/// it is what the interrupt made everything print.
fn is_interrupt_line(line: &str) -> bool {
    let l = line.trim_start();
    l.contains("^C")
        || l.contains("Aborted by user")
        || l.starts_with("Interrupt signal received")
        || (is_make_error(l) && (l.ends_with("Interrupt") || l.contains("*** Deleting file")))
}

/// makepkg's "A failure occurred in build()." and make's "*** Error 1" say
/// nothing on their own: the build's own error is in the lines before. So
/// does pacman's line for a hook or scriptlet that failed: the hook's own
/// complaint comes just before it.
fn wants_lines_before(line: &str) -> bool {
    let l = line.trim_start();
    l.starts_with("==> ERROR:") || is_make_error(l) || l.starts_with("error: command failed to execute correctly")
}

/// pacman lists what went wrong under a failed transaction, unprefixed
/// (the conflicting files, the broken dependencies), and the lock error
/// explains itself on the lines after it.
fn wants_lines_after(line: &str) -> bool {
    let l = line.trim_start();
    ["error: failed to init transaction", "error: failed to prepare transaction", "error: failed to commit transaction"]
        .iter()
        .any(|p| l.starts_with(p))
        || (l.starts_with("error:") && l.contains("could not lock database"))
}

/// The script(1) banners around a recording.
fn is_recorder_banner(line: &str) -> bool {
    line.starts_with("Script started on ") || line.starts_with("Script done on ")
}

const LINES_BEFORE: usize = 15;
const LINES_AFTER: usize = 30;
const TAIL_LINES: usize = 40;
const EXCERPT_LINES: usize = 150;
/// paru's and pacman's summary lines kept beyond EXCERPT_LINES.
const SUMMARY_LINES: usize = 40;

/// How the recorded step ended, which decides what counts as an error line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    /// Exit 0: only what still matters then (`matters_despite_success`).
    Succeeded,
    /// Every error line counts.
    Failed,
    /// Ctrl+C: the error lines from before it count, none after it.
    Interrupted,
}

/// The excerpt and the tail of a recording, read line by line (a build's
/// log can be large).
pub fn summarize(lines: impl Iterator<Item = String>, ended: Ended) -> (Vec<String>, Vec<String>) {
    // By line number: the lines before a marker can come in after an
    // earlier marker was taken, and still go in their place.
    let mut taken: BTreeMap<usize, String> = BTreeMap::new();
    // The lines held against EXCERPT_LINES, and the summary lines beside them.
    let mut counted = 0usize;
    let mut pinned = 0usize;
    // The error lines that did not fit (context lines that did not are not
    // counted: they are no error lines of their own).
    let mut omitted = 0usize;
    let mut recent: VecDeque<(usize, String)> = VecDeque::with_capacity(LINES_BEFORE + 1);
    let mut tail: VecDeque<String> = VecDeque::with_capacity(TAIL_LINES + 1);
    let mut after = 0usize;
    let mut interrupted = false;
    // n counts the lines kept, so a blank line is no gap in the excerpt.
    let mut n = 0usize;
    for raw in lines {
        let line = clean_line(&raw);
        if line.is_empty() {
            // pacman's list under an error ends at a blank line.
            after = 0;
            continue;
        }
        if is_recorder_banner(&line) {
            continue;
        }
        if ended == Ended::Interrupted && !interrupted && is_interrupt_line(&line) {
            interrupted = true;
            after = 0;
        }
        let marker = !interrupted
            && match ended {
                Ended::Succeeded => matters_despite_success(&line),
                Ended::Failed | Ended::Interrupted => is_error_line(&line),
            };
        if marker && wants_lines_before(&line) {
            for (i, l) in &recent {
                if counted < EXCERPT_LINES && !taken.contains_key(i) {
                    taken.insert(*i, l.clone());
                    counted += 1;
                }
            }
        }
        if marker || after > 0 {
            if marker && pinned < SUMMARY_LINES && is_summary_line(&line) {
                taken.insert(n, line.clone());
                pinned += 1;
            } else if counted < EXCERPT_LINES {
                taken.insert(n, line.clone());
                counted += 1;
            } else if marker {
                omitted += 1;
            }
            // The tail is what came after the last error line.
            tail.clear();
            after = after.saturating_sub(1);
            if marker && wants_lines_after(&line) {
                after = LINES_AFTER;
            }
        } else {
            if tail.len() == TAIL_LINES {
                tail.pop_front();
            }
            tail.push_back(line.clone());
        }
        if recent.len() == LINES_BEFORE {
            recent.pop_front();
        }
        recent.push_back((n, line));
        n += 1;
    }
    let mut excerpt = Vec::with_capacity(taken.len() + 4);
    let mut previous: Option<usize> = None;
    for (n, line) in taken {
        // Separate pieces of the output are marked as such.
        if previous.is_some_and(|p| n > p + 1) {
            excerpt.push("…".to_string());
        }
        previous = Some(n);
        excerpt.push(line);
    }
    if omitted > 0 {
        excerpt.push(format!("… {omitted} more error lines (see the full log)"));
    }
    (excerpt, tail.into_iter().collect())
}

/// The lines of a file, decoded leniently (a build may print anything).
fn file_lines(path: &Path) -> Option<impl Iterator<Item = String>> {
    let mut reader = BufReader::new(std::fs::File::open(path).ok()?);
    Some(std::iter::from_fn(move || {
        let mut buf = Vec::new();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(String::from_utf8_lossy(&buf).into_owned()),
        }
    }))
}

// ---- running the step (inside the terminal) ------------------------------

/// `command` as one sh(1) command line, every argument quoted. It ends in
/// exec, so the step is script's own child and its status is script's.
/// script runs it with $SHELL, which is set to /bin/sh for that; the
/// user's own SHELL is put back first for the step and what it starts.
pub fn sh_command(command: &[String], user_shell: Option<&str>) -> String {
    let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    let mut line = match user_shell {
        Some(shell) => format!("SHELL={}; export SHELL; exec", quote(shell)),
        None => "unset SHELL; exec".to_string(),
    };
    for arg in command {
        line.push(' ');
        line.push_str(&quote(arg));
    }
    line
}

/// Where a step's input and output go.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Console {
    /// This process's own: the terminal the step is run in.
    Attached,
    /// None (the log still gets the output): for the tests, which must not
    /// put a developer's terminal into raw mode.
    Detached,
}

fn with_console(mut cmd: Command, console: Console) -> Command {
    if console == Console::Detached {
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    }
    cmd
}

/// Runs `command` with its output also recorded to `log` by `recorder`
/// (script(1); its name is a parameter for the tests). When the recorder is
/// missing, the step still runs, unrecorded.
pub fn run_recorded(step: &str, command: &[String], log: &Path, recorder: &str, console: Console) -> RunResult {
    let mut result = RunResult {
        step: step.to_string(),
        command: command.to_vec(),
        ..RunResult::default()
    };
    if command.is_empty() {
        result.headline = format!("{} failed: nothing to run", step_label(step));
        return result;
    }
    let user_shell = std::env::var("SHELL").ok();
    let mut script = Command::new(recorder);
    script
        .args(["-q", "-e", "-f", "-c", &sh_command(command, user_shell.as_deref())])
        .arg(log)
        .env("SHELL", "/bin/sh");
    let recorded = with_console(script, console).status();
    let mut note = None;
    let status: Result<ExitStatus, String> = match recorded {
        Ok(status) => Ok(status),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            note = Some(format!("(nothing was recorded: {recorder} was not found)"));
            let mut direct = Command::new(&command[0]);
            direct.args(&command[1..]);
            with_console(direct, console)
                .status()
                .map_err(|e| format!("cannot start {}: {e}", command[0]))
        }
        Err(e) => Err(format!("cannot start {recorder}: {e}")),
    };
    match status {
        Ok(status) => {
            result.exit_code = status.code();
            result.signal = status.signal();
            result.ok = status.success();
        }
        Err(e) => {
            result.headline = format!("{} failed: {e}", step_label(step));
            result.tail = vec![e];
            return result;
        }
    }
    let interrupted = result.exit_code == Some(130) || result.signal == Some(2);
    let ended = if result.ok {
        Ended::Succeeded
    } else if interrupted {
        Ended::Interrupted
    } else {
        Ended::Failed
    };
    match file_lines(log) {
        Some(lines) if note.is_none() => {
            let (excerpt, tail) = summarize(lines, ended);
            result.excerpt = excerpt;
            result.tail = tail;
            result.log = log.display().to_string();
        }
        _ => result.tail = vec![note.unwrap_or_else(|| "(nothing was recorded)".to_string())],
    }
    result.cancelled = !result.ok && (interrupted || result.tail.last().is_some_and(|l| declined(l)));
    result.headline = headline(&result);
    result
}

/// Writes the result whole or not at all: the watcher may read at any time.
pub fn write_result(path: &Path, result: &RunResult) -> std::io::Result<()> {
    let tmp = path.with_extension("json.part");
    let json = serde_json::to_string(result).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)
}

pub fn read_result(path: &Path) -> Option<RunResult> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// The report of a result, as the CLI and the terminal print it.
pub fn report_text(r: &RunResult) -> String {
    let mut out = r.headline.clone();
    if !r.command.is_empty() {
        out += &format!("\n  command: {}", r.command.join(" "));
    }
    if !r.excerpt.is_empty() {
        out += "\n  error lines:";
        for l in &r.excerpt {
            out += &format!("\n    {l}");
        }
    }
    if !r.tail.is_empty() {
        out += if r.excerpt.is_empty() { "\n  last output:" } else { "\n  output after them:" };
        for l in &r.tail {
            out += &format!("\n    {l}");
        }
    }
    if !r.log.is_empty() {
        out += &format!("\n  full log: {}", r.log);
    }
    out
}

/// `minka-syu run --step <step> --result <file.json> -- <command…>`: what
/// the terminal runs. Returns the exit status to leave with (the step's).
pub fn run_verb(args: &[&str]) -> Result<i32, String> {
    run_step(args, Console::Attached)
}

fn run_step(args: &[&str], console: Console) -> Result<i32, String> {
    let split = args
        .iter()
        .position(|a| *a == "--")
        .ok_or("run: expected -- <command>")?;
    let (flags, command) = (&args[..split], &args[split + 1..]);
    let value = |flag: &str| {
        flags
            .iter()
            .position(|a| *a == flag)
            .and_then(|i| flags.get(i + 1))
            .copied()
    };
    let step = value("--step").ok_or("run: --step is required")?;
    let result_path = PathBuf::from(value("--result").ok_or("run: --result is required")?);
    let command: Vec<String> = command.iter().map(|s| s.to_string()).collect();

    // Tells the watcher the step has started, and by which process. Whole
    // or not at all, like the result: the watcher may read it any time.
    let pid_path = result_path.with_extension("pid");
    let pid_part = result_path.with_extension("pid.part");
    if std::fs::write(&pid_part, std::process::id().to_string()).is_ok() {
        let _ = std::fs::rename(&pid_part, &pid_path);
    }
    let log = result_path.with_extension("log");
    let mut result = run_recorded(step, &command, &log, "script", console);
    result.id = run_id(&result_path);
    // A clean run's log is not worth its space.
    if result.ok && result.excerpt.is_empty() && !result.log.is_empty() {
        let _ = std::fs::remove_file(&log);
        result.log.clear();
    }
    let written = write_result(&result_path, &result);
    let _ = std::fs::remove_file(&pid_path);
    if let Err(e) = written {
        minka_note::note!("minka-syu: cannot write {}: {e}", result_path.display());
    }

    if let Some(report) = held_report(&result) {
        // The window would close now and take the output with it. That
        // holds for a step that exited 0 with error lines (a failed dkms or
        // mkinitcpio hook) as much as for a failure: the window's card is
        // gone with the window, and the QML front end shows none.
        minka_note::note!("{report}");
        if console == Console::Attached && std::io::stdin().is_terminal() {
            eprint!("\n==> Press Enter to close this window. ");
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
        }
    }
    if !result.ok {
        return Ok(result.exit_code.filter(|c| *c != 0).unwrap_or(1));
    }
    Ok(0)
}

/// What the terminal prints before it holds its window open for Enter, or
/// None when it closes at once: red for a failure, amber for a step that
/// finished with error lines.
fn held_report(r: &RunResult) -> Option<String> {
    let colour = if r.ok { "\x1b[1;33m" } else { "\x1b[1;31m" };
    needs_attention(r).then(|| format!("\n{colour}==> minka-syu: {}\x1b[0m", report_text(r)))
}

/// A step's id: its result file's stem.
pub fn run_id(result: &Path) -> String {
    result
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

// ---- waiting for it (outside the terminal) -------------------------------

/// Where a step's files go: $XDG_STATE_HOME/minka-syu/runs.
pub fn runs_dir() -> PathBuf {
    crate::check::state_dir().join("runs")
}

/// How many steps' files are kept: the failed ones' logs are the point.
const RUNS_KEPT: usize = 8;

/// A fresh result path for one step (its log and pid file sit beside it),
/// after pruning the files of older steps.
pub fn new_run(dir: &Path, step: &str) -> std::io::Result<PathBuf> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    std::fs::create_dir_all(dir)?;
    prune_runs(dir, RUNS_KEPT);
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    Ok(dir.join(format!("{step}-{secs}-{}-{seq}.json", std::process::id())))
}

/// Removes all but the newest `keep` steps' files (by stem: .json, .log,
/// .pid and .json.part belong together).
fn prune_runs(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut stems: std::collections::HashMap<String, std::time::SystemTime> = Default::default();
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        let stem = name.split('.').next().unwrap_or(name).to_string();
        let modified = e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
        let newest = stems.entry(stem.clone()).or_insert(modified);
        if modified > *newest {
            *newest = modified;
        }
        files.push((stem, path));
    }
    let mut order: Vec<(std::time::SystemTime, String)> = stems.into_iter().map(|(s, t)| (t, s)).collect();
    order.sort_by(|a, b| b.cmp(a));
    let old: std::collections::HashSet<String> = order.into_iter().skip(keep).map(|(_, s)| s).collect();
    for (stem, path) in files {
        if old.contains(&stem) {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// The terminal argv that runs `terminal_argv`'s command through
/// `minka-syu run`. `terminal_argv` is `[terminal, "-e", command…]`.
pub fn wrap(terminal_argv: &[String], self_exe: &Path, step: &str, result: &Path) -> Vec<String> {
    let (terminal, command) = terminal_argv.split_at(terminal_argv.len().min(2));
    let mut argv = terminal.to_vec();
    argv.extend([
        self_exe.display().to_string(),
        // MinkaDE's one binary: a terminal cannot set argv[0], so the name
        // to run as goes first.
        "minka-syu".to_string(),
        "run".to_string(),
        "--step".to_string(),
        step.to_string(),
        "--result".to_string(),
        result.display().to_string(),
        "--".to_string(),
    ]);
    argv.extend(command.iter().cloned());
    argv
}

/// This binary (MinkaDE's one binary, which `wrap` tells to run as
/// minka-syu), for the terminal to run. After the package is upgraded under a
/// running core, /proc/self/exe reads "… (deleted)"; the path itself then
/// holds the new binary, which is the one to start.
pub fn self_exe() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("minka"));
    match exe.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
        Some(path) => PathBuf::from(path),
        None => exe,
    }
}

/// Whether the `minka-syu run` that writes `result` is still running as
/// process `pid` (and not some later process that got its number).
fn runner_alive(pid: u32, result: &Path) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // The state follows the command name's closing parenthesis.
    if stat.rsplit_once(')').is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')) {
        return false;
    }
    let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let want = result.display().to_string();
    cmdline.split(|b| *b == 0).any(|arg| arg == want.as_bytes())
}

pub struct Waiting {
    pub poll: Duration,
    /// How long a terminal that has exited gets for the step to show up
    /// (a terminal that hands its window to a server returns at once).
    pub grace: Duration,
}

impl Default for Waiting {
    fn default() -> Self {
        Waiting {
            poll: Duration::from_millis(300),
            grace: Duration::from_secs(3),
        }
    }
}

/// Waits for the step in `terminal` to write `result`, and returns it as
/// soon as it is there: a step that needs looking at holds its window
/// open, and the report should not wait for that. When the terminal is
/// gone and the step with it, without a result, the result says so. It
/// carries the step's id either way.
pub fn await_result(
    terminal: &mut Child,
    result: &Path,
    step: &str,
    command: &[String],
    waiting: &Waiting,
) -> RunResult {
    let mut r = wait_for_result(terminal, result, step, command, waiting);
    r.id = run_id(result);
    r
}

fn wait_for_result(
    terminal: &mut Child,
    result: &Path,
    step: &str,
    command: &[String],
    waiting: &Waiting,
) -> RunResult {
    let pid_path = result.with_extension("pid");
    let mut closed: Option<(Option<ExitStatus>, Instant)> = None;
    loop {
        if let Some(r) = read_result(result) {
            return r;
        }
        if closed.is_none() {
            match terminal.try_wait() {
                Ok(Some(status)) => closed = Some((Some(status), Instant::now())),
                Ok(None) => {}
                Err(_) => closed = Some((None, Instant::now())),
            }
        }
        if let Some((status, at)) = closed {
            let started = pid_path.exists();
            let running = std::fs::read_to_string(&pid_path)
                .ok()
                .and_then(|p| p.trim().parse::<u32>().ok())
                .is_some_and(|pid| runner_alive(pid, result));
            if !running && (started || at.elapsed() >= waiting.grace) {
                // It may have finished between the two looks.
                if let Some(r) = read_result(result) {
                    return r;
                }
                return lost(step, command, result, started, status);
            }
        }
        std::thread::sleep(waiting.poll);
    }
}

/// The result of a step that never reported one. When it had started,
/// script(1) had been writing what it showed to its log all along (-f):
/// that is summarized as a finished step's would be, and its path given.
fn lost(step: &str, command: &[String], result: &Path, started: bool, terminal: Option<ExitStatus>) -> RunResult {
    let label = step_label(step);
    let mut r = RunResult {
        step: step.to_string(),
        command: command.to_vec(),
        ok: false,
        ..RunResult::default()
    };
    if !started {
        let how = match terminal.and_then(|s| s.code()) {
            Some(code) => format!(" (status {code})"),
            None => String::new(),
        };
        r.headline = format!("{label} did not run: the terminal exited{how} without starting it");
        return r;
    }
    let log = result.with_extension("log");
    if let Some(lines) = file_lines(&log) {
        let (excerpt, tail) = summarize(lines, Ended::Failed);
        r.excerpt = excerpt;
        r.tail = tail;
        if std::fs::metadata(&log).is_ok_and(|m| m.len() > 0) {
            r.log = log.display().to_string();
        }
    }
    let program = command.first().map(String::as_str).unwrap_or("the step");
    if r.tail.last().is_some_and(|l| closed_at_question(l)) {
        // Closing the window at a question is backing out, as a no is.
        r.cancelled = true;
        r.headline = format!("{label} cancelled: its window was closed at {program}'s question");
    } else {
        r.headline = format!(
            "{label} did not finish: its terminal closed before it reported how it ended (was the window closed while it ran?)"
        );
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> impl Iterator<Item = String> + '_ {
        text.split_inclusive('\n').map(str::to_string)
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("minka-syu-run-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sh(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
    }

    #[test]
    fn clean_line_drops_colour_and_overwritten_text() {
        assert_eq!(clean_line("\x1b[1;31merror:\x1b[0m target not found: foo\r\n"), "error: target not found: foo");
        assert_eq!(clean_line(" downloading 10%\r downloading 100%\r\n"), " downloading 100%");
        assert_eq!(clean_line("\x1b]0;title\x07==> ERROR: x"), "==> ERROR: x");
        assert_eq!(clean_line("\x1b(Bplain\x1b[K   "), "plain");
        assert_eq!(clean_line("ab\x08c"), "ac");
    }

    #[test]
    fn summarize_keeps_pacman_errors_and_what_they_list() {
        let log = "Script started on 2026-11-21 [COMMAND=\"x\"]\n\
                   :: Proceed with installation? [Y/n]\n\
                   (1/1) checking for file conflicts\n\
                   error: failed to commit transaction (conflicting files)\n\
                   foo: /usr/bin/foo exists in filesystem\n\
                   foo: /usr/lib/libfoo.so exists in filesystem\n\
                   Errors occurred, no packages were upgraded.\n\
                   \n\
                   Script done on 2026-11-21 [COMMAND_EXIT_CODE=\"1\"]\n";
        let (excerpt, tail) = summarize(lines(log), Ended::Failed);
        assert_eq!(
            excerpt,
            [
                "error: failed to commit transaction (conflicting files)",
                "foo: /usr/bin/foo exists in filesystem",
                "foo: /usr/lib/libfoo.so exists in filesystem",
                "Errors occurred, no packages were upgraded.",
            ]
        );
        // Nothing came after the error lines.
        assert!(tail.is_empty(), "{tail:?}");
    }

    #[test]
    fn summarize_gives_makepkg_failures_their_build_lines() {
        let mut log = String::from("==> Making package: lib32-libarchive 3.8.11-1\n");
        for i in 0..30 {
            log += &format!("  CC libarchive/test/test_{i}.o\n");
        }
        log += "/usr/bin/ld: cannot find -lfoo: No such file or directory\n\
                collect2: error: ld returned 1 exit status\n\
                make[1]: *** [Makefile:12: libarchive_test] Error 1\n\
                ==> ERROR: A failure occurred in check().\n    Aborting...\n\
                error: failed to build 'lib32-libarchive-3.8.11-1': \n\
                ==> Making package: varia 2026.11.16-1\n\
                ==> Finished making: varia 2026.11.16-1\n\
                error: packages failed to build: lib32-libarchive-3.8.11-1\n";
        let (excerpt, tail) = summarize(lines(&log), Ended::Failed);
        assert!(excerpt.contains(&"/usr/bin/ld: cannot find -lfoo: No such file or directory".to_string()), "{excerpt:#?}");
        assert!(excerpt.contains(&"==> ERROR: A failure occurred in check().".to_string()));
        assert!(excerpt.contains(&"error: packages failed to build: lib32-libarchive-3.8.11-1".to_string()));
        // Fifteen lines of context, not the whole build.
        assert!(!excerpt.iter().any(|l| l.contains("test_0.o")), "{excerpt:#?}");
        // Separate pieces are marked as such.
        assert!(excerpt.contains(&"…".to_string()));
        assert!(tail.is_empty(), "{tail:?}");
    }

    #[test]
    fn summarize_without_error_lines_keeps_the_end() {
        let mut log = String::new();
        for i in 0..100 {
            log += &format!("line {i}\n");
        }
        let (excerpt, tail) = summarize(lines(&log), Ended::Failed);
        assert!(excerpt.is_empty());
        assert_eq!(tail.len(), TAIL_LINES);
        assert_eq!(tail.last().unwrap(), "line 99");
    }

    #[test]
    fn sh_command_quotes_every_argument() {
        let line = sh_command(&["paru".into(), "-S".into(), "it's".into()], Some("/bin/fish"));
        assert_eq!(line, "SHELL='/bin/fish'; export SHELL; exec 'paru' '-S' 'it'\\''s'");
        assert_eq!(sh_command(&["true".into()], None), "unset SHELL; exec 'true'");
    }

    #[test]
    fn a_failing_step_reports_status_and_error_lines() {
        let dir = scratch("fail");
        let log = dir.join("r.log");
        let r = run_recorded(
            "install",
            &sh("echo 'resolving dependencies...'; echo 'error: failed to commit transaction (conflicting files)' >&2; exit 1"),
            &log,
            "script",
            Console::Detached,
        );
        assert!(!r.ok);
        assert_eq!(r.exit_code, Some(1));
        assert_eq!(r.excerpt, ["error: failed to commit transaction (conflicting files)"]);
        assert_eq!(r.headline, "Install failed: sh exited with status 1");
        assert_eq!(r.log, log.display().to_string());
        let text = report_text(&r);
        assert!(text.contains("error: failed to commit transaction"), "{text}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_killed_step_reports_its_signal() {
        let dir = scratch("signal");
        let r = run_recorded("apply", &sh("echo building; kill -TERM $$"), &dir.join("r.log"), "script", Console::Detached);
        assert!(!r.ok);
        assert_eq!(r.exit_code, Some(143));
        assert!(r.headline.contains("killed by signal 15 (SIGTERM)"), "{}", r.headline);
        assert_eq!(r.tail, ["building"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_clean_step_is_ok() {
        let dir = scratch("ok");
        let r = run_recorded("remove", &sh("echo removed"), &dir.join("r.log"), "script", Console::Detached);
        assert!(r.ok, "{r:?}");
        assert_eq!(r.exit_code, Some(0));
        assert!(r.excerpt.is_empty());
        assert_eq!(r.headline, "Removal finished");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_ok_exit_with_error_lines_is_not_silent() {
        let dir = scratch("hook");
        let r = run_recorded(
            "apply",
            &sh("echo '==> ERROR: module not found: foo' >&2; echo 'error: command failed to execute correctly' >&2; exit 0"),
            &dir.join("r.log"),
            "script",
            Console::Detached,
        );
        assert!(r.ok);
        assert_eq!(r.excerpt.len(), 2, "{:?}", r.excerpt);
        assert_eq!(r.headline, "Upgrade finished, but its output has error lines");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_question_answered_no_is_a_cancel_not_a_failure() {
        let dir = scratch("declined");
        let r = run_recorded(
            "install",
            &sh("echo 'Total Installed Size:  1.00 MiB'; printf ':: Proceed with installation? [Y/n] n\\n'; exit 1"),
            &dir.join("r.log"),
            "script",
            Console::Detached,
        );
        assert!(!r.ok);
        assert!(r.cancelled, "{r:?}");
        assert!(!needs_attention(&r));
        assert_eq!(r.headline, "Install cancelled at sh's question");
        // A default of no, left at Enter, is a no as well; a yes is not.
        assert!(declined(":: Remove these packages? [y/N] "));
        assert!(!declined(":: Proceed with installation? [Y/n] "));
        assert!(!declined(":: Proceed with installation? [Y/n] y"));
        // Errors before the question still ask to be looked at.
        let mut with_errors = r.clone();
        with_errors.excerpt = vec!["error: failed to build 'foo-1-1':".into()];
        assert!(needs_attention(&with_errors));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn without_a_recorder_the_status_still_counts() {
        let dir = scratch("norecorder");
        let r = run_recorded("install", &sh("exit 7"), &dir.join("r.log"), "minka-syu-no-such-recorder", Console::Detached);
        assert!(!r.ok);
        assert_eq!(r.exit_code, Some(7));
        assert!(r.tail[0].contains("nothing was recorded"), "{:?}", r.tail);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn wrap_runs_the_command_through_the_runner() {
        let argv = vec!["rio".to_string(), "-e".into(), "paru".into(), "-S".into(), "--".into(), "foo".into()];
        let wrapped = wrap(&argv, Path::new("/usr/bin/minka"), "install", Path::new("/s/runs/i.json"));
        assert_eq!(
            wrapped,
            [
                "rio", "-e", "/usr/bin/minka", "minka-syu", "run", "--step", "install", "--result", "/s/runs/i.json", "--",
                "paru", "-S", "--", "foo"
            ]
        );
    }

    #[test]
    fn the_result_is_picked_up_while_the_terminal_stays_open() {
        let dir = scratch("await");
        let result = dir.join("r.json");
        let failed = RunResult {
            step: "apply".into(),
            command: vec!["paru".into()],
            exit_code: Some(1),
            headline: "Upgrade failed: paru exited with status 1".into(),
            ..RunResult::default()
        };
        let json = serde_json::to_string(&failed).unwrap();
        // A "terminal" that writes the result and then stays open.
        let mut terminal = Command::new("sh")
            .args(["-c", "printf '%s' \"$1\" > \"$2.part\"; mv \"$2.part\" \"$2\"; sleep 5", "sh", &json])
            .arg(&result)
            .spawn()
            .unwrap();
        let start = Instant::now();
        let waiting = Waiting { poll: Duration::from_millis(20), grace: Duration::from_millis(200) };
        let r = await_result(&mut terminal, &result, "apply", &["paru".into()], &waiting);
        // It comes back named after its result file, so the window can tell
        // two steps' reports apart.
        assert_eq!(r, RunResult { id: "r".into(), ..failed });
        assert!(start.elapsed() < Duration::from_secs(4));
        let _ = terminal.kill();
        let _ = terminal.wait();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_terminal_that_exits_without_a_result_is_a_failure() {
        let dir = scratch("lost");
        let result = dir.join("r.json");
        let waiting = Waiting { poll: Duration::from_millis(20), grace: Duration::from_millis(100) };
        // Never started the step.
        let mut terminal = Command::new("sh").args(["-c", "exit 2"]).spawn().unwrap();
        let r = await_result(&mut terminal, &result, "install", &["paru".into()], &waiting);
        assert!(!r.ok);
        assert_eq!(r.headline, "Install did not run: the terminal exited (status 2) without starting it");
        // Started it (a pid file whose process is gone), then closed.
        std::fs::write(result.with_extension("pid"), "999999999").unwrap();
        let mut terminal = Command::new("sh").args(["-c", "exit 0"]).spawn().unwrap();
        let r = await_result(&mut terminal, &result, "apply", &["paru".into()], &waiting);
        assert!(!r.ok);
        assert!(r.headline.starts_with("Upgrade did not finish"), "{}", r.headline);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_runner_verb_writes_the_result_and_exits_with_the_step() {
        let dir = scratch("verb");
        let result = dir.join("install-1.json");
        let path = result.display().to_string();
        let code = run_step(
            &["--step", "install", "--result", &path, "--", "sh", "-c", "echo 'error: target not found: nope' >&2; exit 1"],
            Console::Detached,
        )
        .unwrap();
        assert_eq!(code, 1);
        let r = read_result(&result).unwrap();
        assert!(!r.ok);
        assert_eq!(r.id, "install-1");
        assert_eq!(r.excerpt, ["error: target not found: nope"]);
        assert!(!result.with_extension("pid").exists());
        assert!(result.with_extension("log").exists());
        // A clean run leaves no log behind.
        let ok = dir.join("install-2.json");
        let code =
            run_step(&["--step", "install", "--result", &ok.display().to_string(), "--", "true"], Console::Detached).unwrap();
        assert_eq!(code, 0);
        assert!(read_result(&ok).unwrap().ok);
        assert!(!ok.with_extension("log").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn old_runs_are_pruned_by_step() {
        let dir = scratch("prune");
        for i in 0..5 {
            std::fs::write(dir.join(format!("apply-{i}.json")), "{}").unwrap();
            std::fs::write(dir.join(format!("apply-{i}.log")), "x").unwrap();
            // Distinct modification times.
            std::thread::sleep(Duration::from_millis(15));
        }
        prune_runs(&dir, 2);
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["apply-3.json", "apply-3.log", "apply-4.json", "apply-4.log"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_step_that_finished_with_error_lines_holds_its_window_too() {
        // A dkms or mkinitcpio hook that failed exits 0: when nobody is
        // listening (the window closed, the QML front end), the terminal is
        // the only place left to see it.
        let hook = RunResult {
            step: "apply".into(),
            command: vec!["paru".into()],
            ok: true,
            exit_code: Some(0),
            headline: "Upgrade finished, but its output has error lines".into(),
            excerpt: vec!["Error! Bad return status for module build on kernel: 7.3.1-1-cachyos".into()],
            ..RunResult::default()
        };
        let held = held_report(&hook).expect("an ok step with error lines holds its window");
        assert!(held.contains("\x1b[1;33m==> minka-syu: Upgrade finished, but"), "amber, not red: {held:?}");
        assert!(held.contains("Error! Bad return status"), "{held:?}");
        let failed = RunResult { ok: false, exit_code: Some(1), ..hook.clone() };
        assert!(held_report(&failed).unwrap().contains("\x1b[1;31m==> minka-syu:"));
        let clean = RunResult { ok: true, exit_code: Some(0), ..RunResult::default() };
        assert!(held_report(&clean).is_none());
        let declined = RunResult { ok: false, cancelled: true, exit_code: Some(1), ..RunResult::default() };
        assert!(held_report(&declined).is_none());
    }

    #[test]
    fn a_closed_terminal_still_reports_what_it_recorded() {
        let dir = scratch("lost-log");
        let waiting = Waiting { poll: Duration::from_millis(20), grace: Duration::from_millis(100) };
        // The runner died with its window (its pid file stays behind), after
        // script(1) had flushed the build's output to the log.
        let result = dir.join("apply-1.json");
        std::fs::write(result.with_extension("pid"), "999999999").unwrap();
        std::fs::write(
            result.with_extension("log"),
            "==> Making package: foo 1-1\n  CC foo.o\nfoo.c:1:1: error: expected ';'\n\
             make: *** [Makefile:3: foo.o] Error 1\n==> ERROR: A failure occurred in build().\n    Aborting...\n",
        )
        .unwrap();
        let mut terminal = Command::new("sh").args(["-c", "exit 0"]).spawn().unwrap();
        let r = await_result(&mut terminal, &result, "apply", &["paru".into()], &waiting);
        assert!(!r.ok && !r.cancelled, "{r:?}");
        assert!(r.headline.starts_with("Upgrade did not finish"), "{}", r.headline);
        assert!(r.excerpt.contains(&"foo.c:1:1: error: expected ';'".to_string()), "{:?}", r.excerpt);
        assert!(r.excerpt.contains(&"==> ERROR: A failure occurred in build().".to_string()));
        assert_eq!(r.log, result.with_extension("log").display().to_string());
        assert!(report_text(&r).contains("full log: "), "{}", report_text(&r));

        // Closed at paru's question, unanswered: backing out, not a failure.
        let result = dir.join("install-2.json");
        std::fs::write(result.with_extension("pid"), "999999999").unwrap();
        std::fs::write(
            result.with_extension("log"),
            "Total Installed Size:  1.00 MiB\n\n:: Proceed with installation? [Y/n] ",
        )
        .unwrap();
        let mut terminal = Command::new("sh").args(["-c", "exit 0"]).spawn().unwrap();
        let r = await_result(&mut terminal, &result, "install", &["paru".into()], &waiting);
        assert!(r.cancelled, "{r:?}");
        assert!(!needs_attention(&r));
        assert_eq!(r.log, result.with_extension("log").display().to_string());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn paru_s_own_failure_lines_survive_a_long_excerpt() {
        let mut log = String::from("==> Making package: foo-rs 1.0-1\n");
        for i in 0..150 {
            log += &format!("error[E0433]: failed to resolve: use of undeclared crate `x{i}`\n");
        }
        log += "error: could not compile `foo-rs` (bin \"foo\") due to 150 previous errors\n\
                ==> ERROR: A failure occurred in build().\n    Aborting...\n\
                error: failed to build 'foo-rs-1.0-1': \n\
                ==> Making package: bar 1-1\n\
                bar.c:1:1: error: oops\n\
                make: *** [Makefile:1: bar] Error 1\n\
                ==> ERROR: A failure occurred in build().\n    Aborting...\n\
                error: failed to build 'bar-1-1 (bar bar-docs)': \n\
                error: packages failed to build: foo-rs-1.0-1  bar-1-1 (bar bar-docs)\n";
        let (excerpt, tail) = summarize(lines(&log), Ended::Failed);
        for wanted in [
            "error: failed to build 'foo-rs-1.0-1':",
            "error: failed to build 'bar-1-1 (bar bar-docs)':",
            "error: packages failed to build: foo-rs-1.0-1  bar-1-1 (bar bar-docs)",
        ] {
            assert!(excerpt.contains(&wanted.to_string()), "{wanted} is missing from {excerpt:#?}");
        }
        // Four error lines did not fit (could not compile, the make line and
        // the two makepkg ones), each counted once.
        assert_eq!(excerpt.last().unwrap(), "… 4 more error lines (see the full log)");
        assert!(tail.is_empty(), "{tail:?}");

        // The headline names what did not build.
        let failed = RunResult {
            step: "apply".into(),
            command: vec!["paru".into(), "-Syu".into()],
            exit_code: Some(1),
            excerpt: excerpt.clone(),
            ..RunResult::default()
        };
        assert_eq!(headline(&failed), "Upgrade failed: foo-rs and bar did not build (paru exited with status 1)");
        let one = RunResult { excerpt: vec!["error: packages failed to build: lib32-libarchive-3.8.11-1".into()], ..failed.clone() };
        assert_eq!(headline(&one), "Upgrade failed: lib32-libarchive did not build (paru exited with status 1)");
        // paru --failfast stops at the first, without the summary line.
        let failfast = RunResult { excerpt: vec!["error: failed to build 'baz-2:1.0-3':".into()], ..failed.clone() };
        assert_eq!(headline(&failfast), "Upgrade failed: baz did not build (paru exited with status 1)");
        let many = RunResult {
            excerpt: vec!["error: packages failed to build: a-1-1  b-1-1  c-1-1  d-1-1  e-1-1".into()],
            ..failed
        };
        assert_eq!(headline(&many), "Upgrade failed: a, b, c and 2 more did not build (paru exited with status 1)");
    }

    #[test]
    fn a_clean_upgrade_s_mirror_retries_and_git_noise_raise_no_card() {
        let dir = scratch("mirror");
        let r = run_recorded(
            "apply",
            &sh("echo \"error: failed retrieving file 'linux-cachyos-7.3.4-1-x86_64_v4.pkg.tar.zst' from mirror.example : \
                 The requested URL returned error: 404\" >&2; \
                 echo 'fatal: No names found, cannot describe anything.' >&2; \
                 echo 'make: *** [Makefile:9: docs] Error 1 (ignored)'; \
                 echo ' linux-cachyos-7.3.4-1-x86_64_v4 downloaded'; exit 0"),
            &dir.join("ok.log"),
            "script",
            Console::Detached,
        );
        assert!(r.ok, "{r:?}");
        assert!(r.excerpt.is_empty(), "{:?}", r.excerpt);
        assert!(!needs_attention(&r));
        assert_eq!(r.headline, "Upgrade finished");
        // What still matters after a clean exit: a dkms build that failed.
        let r = run_recorded(
            "apply",
            &sh("echo 'Building module(s)... failed!'; \
                 echo 'Error! Bad return status for module build on kernel: 7.3.1-1-cachyos (x86_64)'; exit 0"),
            &dir.join("dkms.log"),
            "script",
            Console::Detached,
        );
        assert!(r.ok);
        assert!(r.excerpt.iter().any(|l| l.starts_with("Error! Bad return status")), "{:?}", r.excerpt);
        // A failed step keeps every error line, the mirrors' included.
        let r = run_recorded(
            "apply",
            &sh("echo \"error: failed retrieving file 'foo-1-1-x86_64.pkg.tar.zst' from mirror.example : Operation too slow\" >&2; \
                 echo 'error: failed to retrieve some files' >&2; exit 1"),
            &dir.join("fail.log"),
            "script",
            Console::Detached,
        );
        assert!(!r.ok);
        assert_eq!(r.excerpt.len(), 2, "{:?}", r.excerpt);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ctrl_c_during_a_build_is_a_cancel_without_a_card() {
        let dir = scratch("ctrl-c");
        // makepkg's trap and make's lines, after the tty's echo of ^C.
        let r = run_recorded(
            "apply",
            &sh("echo '  CC libarchive/test/test_7.o'; echo '^Cmake[2]: *** Deleting file test_7.o'; \
                 echo 'make[2]: *** [Makefile:3: test_7.o] Interrupt' >&2; \
                 echo '==> ERROR: Aborted by user! Exiting...' >&2; exit 130"),
            &dir.join("make.log"),
            "script",
            Console::Detached,
        );
        assert!(r.cancelled, "{r:?}");
        assert!(r.excerpt.is_empty(), "{:?}", r.excerpt);
        assert!(!needs_attention(&r));
        assert!(held_report(&r).is_none());
        assert_eq!(r.headline, "Upgrade cancelled with Ctrl+C");
        // makepkg's message alone is enough (no echo of ^C).
        let r = run_recorded(
            "install",
            &sh("echo '==> ERROR: Aborted by user! Exiting...' >&2; exit 130"),
            &dir.join("makepkg.log"),
            "script",
            Console::Detached,
        );
        assert!(r.excerpt.is_empty(), "{:?}", r.excerpt);
        // Errors from before the interrupt still count; what it caused does not.
        let (excerpt, _) = summarize(
            lines("error: failed to build 'foo-1-1':\n  CC x.o\n^C\nInterrupt signal received\nerror: failed to commit transaction (transaction aborted)\n"),
            Ended::Interrupted,
        );
        assert_eq!(excerpt, ["error: failed to build 'foo-1-1':"]);
        let _ = std::fs::remove_dir_all(dir);
    }
}
