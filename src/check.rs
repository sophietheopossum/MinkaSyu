//! The checker: refresh sync DBs into a throwaway copy (the checkupdates
//! trick, so a check alone can never leave partial-upgrade state), list
//! repo updates from it and AUR updates from paru, and stamp each with
//! repository provenance — installed origin vs candidate origin.

use crate::alpm;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transition {
    Same,
    Changed,
    UnknownSource,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Update {
    pub name: String,
    pub old_version: String,
    pub new_version: String,
    /// Repo the installed package came from (%INSTALLED_DB%), or "aur"
    /// for paru-identified AUR packages. None when not establishable.
    pub installed_repo: Option<String>,
    /// Repo the candidate comes from ("aur" for AUR updates).
    pub candidate_repo: String,
    pub transition: Transition,
    /// True when this candidate is a strictly newer version in a
    /// lower-ranked repository than the one pacman would normally
    /// choose. Applying it requires an explicit `repo/name` target;
    /// a plain `-Syu` would never select it.
    #[serde(default)]
    pub explicit_target: bool,
    /// The higher-ranked repo/version pacman would have used instead,
    /// present only for explicit-target candidates.
    #[serde(default)]
    pub first_repo: Option<String>,
    #[serde(default)]
    pub first_version: Option<String>,
    /// Installed packages this candidate replaces (its %REPLACES%):
    /// libalpm removes them when `-Syu` installs it, so the row is the
    /// replacer, named after it, and `old_version` / `installed_repo`
    /// describe the first package it replaces unless the replacer is
    /// itself installed. Empty for an ordinary update.
    #[serde(default)]
    pub replaces: Vec<Replaced>,
}

/// One installed package that a pending update replaces.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Replaced {
    pub name: String,
    pub version: String,
    pub installed_repo: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub schema: u32,
    /// Unix seconds of the last successful check.
    pub checked_at: i64,
    pub updates: Vec<Update>,
    pub repo_change_count: usize,
    /// Total update count at last notification; notification fires only
    /// when the count grows past this.
    pub last_notified_count: usize,
}

impl State {
    pub fn total(&self) -> usize {
        self.updates.len()
    }
}

fn transition(installed_repo: &Option<String>, candidate_repo: &str) -> Transition {
    match installed_repo {
        Some(repo) if repo == candidate_repo => Transition::Same,
        Some(_) => Transition::Changed,
        None => Transition::UnknownSource,
    }
}

/// Parse one `name old -> new` line (the shared output shape of
/// `pacman -Qu` and `paru -Qua`). Anything else returns None, and so
/// does a line pacman marks ` [ignored]`: -Qu still lists packages
/// IgnorePkg/IgnoreGroup hold back (and those in a repo without
/// `Usage = Upgrade`), which -Syu will not touch.
pub fn parse_update_line(line: &str) -> Option<(String, String, String)> {
    let mut parts = line.split_whitespace();
    let name = parts.next()?;
    let old = parts.next()?;
    if parts.next()? != "->" {
        return None;
    }
    let new = parts.next()?;
    if parts.next() == Some("[ignored]") {
        return None;
    }
    Some((name.to_string(), old.to_string(), new.to_string()))
}

/// A `%REPLACES%` entry: a package name with an optional version
/// constraint (`<`, `<=`, `=`, `>=`, `>`), as libalpm writes depstrings.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DepSpec {
    pub name: String,
    pub constraint: Option<(String, String)>,
}

pub(crate) fn parse_depstring(s: &str) -> Option<DepSpec> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let Some(i) = s.find(['<', '>', '=']) else {
        return Some(DepSpec { name: s.to_string(), constraint: None });
    };
    let (name, rest) = s.split_at(i);
    let op_len = if !rest.starts_with('=') && rest[1..].starts_with('=') { 2 } else { 1 };
    let (op, version) = rest.split_at(op_len);
    if name.is_empty() || version.is_empty() {
        return None;
    }
    Some(DepSpec {
        name: name.to_string(),
        constraint: Some((op.to_string(), version.to_string())),
    })
}

/// Whether `version` satisfies a depstring constraint, libalpm-style
/// (the comparison is `vercmp`, injected so tests stay pure).
fn satisfies(
    version: &str,
    constraint: &Option<(String, String)>,
    cmp: &mut dyn FnMut(&str, &str) -> Ordering,
) -> bool {
    let Some((op, wanted)) = constraint else {
        return true;
    };
    let ord = cmp(version, wanted);
    match op.as_str() {
        "<" => ord == Ordering::Less,
        "<=" => ord != Ordering::Greater,
        "=" => ord == Ordering::Equal,
        ">=" => ord != Ordering::Less,
        ">" => ord == Ordering::Greater,
        _ => false,
    }
}

/// fnmatch(3) without flags, which is how libalpm matches IgnorePkg
/// and IgnoreGroup patterns: `*`, `?`, `[...]` (ranges, `!` or `^`
/// to negate) and `\` escapes.
pub(crate) fn glob_match(pattern: &str, text: &str) -> bool {
    // Some((matched, chars consumed)) for a bracket expression at p[0];
    // None when it is unterminated, in which case '[' is literal.
    fn class(p: &[char], c: char) -> Option<(bool, usize)> {
        let mut i = 1;
        let negate = matches!(p.get(i), Some('!') | Some('^'));
        if negate {
            i += 1;
        }
        let start = i;
        let mut matched = false;
        while i < p.len() {
            if p[i] == ']' && i > start {
                return Some((matched != negate, i + 1));
            }
            let mut lo = p[i];
            if lo == '\\' && i + 1 < p.len() {
                i += 1;
                lo = p[i];
            }
            if i + 2 < p.len() && p[i + 1] == '-' && p[i + 2] != ']' {
                if lo <= c && c <= p[i + 2] {
                    matched = true;
                }
                i += 3;
            } else {
                if lo == c {
                    matched = true;
                }
                i += 1;
            }
        }
        None
    }
    fn go(p: &[char], t: &[char]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some('*') => (0..=t.len()).any(|k| go(&p[1..], &t[k..])),
            Some('?') => !t.is_empty() && go(&p[1..], &t[1..]),
            Some('[') if !t.is_empty() => match class(p, t[0]) {
                Some((true, used)) => go(&p[used..], &t[1..]),
                Some((false, _)) => false,
                None => t[0] == '[' && go(&p[1..], &t[1..]),
            },
            Some('\\') if p.len() > 1 => !t.is_empty() && t[0] == p[1] && go(&p[2..], &t[1..]),
            Some(c) => !t.is_empty() && t[0] == *c && go(&p[1..], &t[1..]),
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    go(&p, &t)
}

/// A sysupgrade target that replaces installed packages.
#[derive(Debug, Clone, PartialEq)]
pub struct Replacement {
    pub name: String,
    pub version: String,
    pub repo: String,
    pub replaced: Vec<Replaced>,
}

/// The replacements in `-Syu`'s target set.
///
/// `sup_text` is `pacman -Sup -dd --print-format '%n|%v|%r|%R'`:
/// exactly libalpm's sysupgrade targets with their repo and replaces
/// list. pacman prints what it would install, not what it would
/// remove, so a target's %R entry counts as a replacement only when
/// libalpm's own rules (lib/libalpm/sync.c, check_replacers) agree:
/// - it names an installed package by NAME (provides never count)
///   whose installed version satisfies the entry's constraint;
/// - that package is not itself an upgrade target (then it is being
///   upgraded, not replaced);
/// - IgnorePkg/IgnoreGroup do not hold it back (`ignored`);
/// - no repository ranked above the target's carries a package of
///   that name: libalpm stops at the first repository holding either
///   a replacer or the literal name, even an older one.
///
/// Ranks come from `sync_entries` (pacman -Sl order).
pub fn find_replacements(
    sup_text: &str,
    installed: &HashMap<String, alpm::InstalledPkg>,
    sync_entries: &[alpm::SyncEntry],
    ignored: &HashSet<String>,
    cmp: &mut dyn FnMut(&str, &str) -> Ordering,
) -> Vec<Replacement> {
    let mut rank: HashMap<&str, usize> = HashMap::new();
    for e in sync_entries {
        let next = rank.len();
        rank.entry(e.repo.as_str()).or_insert(next);
    }
    let targets: Vec<(&str, &str, &str, &str)> = sup_text
        .lines()
        .filter_map(|line| {
            let mut f = line.splitn(4, '|');
            let (name, version, repo) = (f.next()?, f.next()?, f.next()?);
            let replaces = f.next().unwrap_or("");
            (!name.is_empty() && !version.is_empty() && !repo.is_empty())
                .then_some((name, version, repo, replaces))
        })
        .collect();
    let target_names: HashSet<&str> = targets.iter().map(|t| t.0).collect();

    let mut out = Vec::new();
    for (name, version, repo, replaces) in targets {
        let own_rank = rank.get(repo).copied().unwrap_or(usize::MAX);
        let mut replaced = Vec::new();
        for dep in replaces.split_whitespace().filter_map(parse_depstring) {
            if dep.name == name || target_names.contains(dep.name.as_str()) || ignored.contains(&dep.name) {
                continue;
            }
            let Some(pkg) = installed.get(&dep.name) else {
                continue;
            };
            if !satisfies(&pkg.version, &dep.constraint, cmp) {
                continue;
            }
            let shadowed = sync_entries.iter().any(|e| {
                e.name == dep.name && rank.get(e.repo.as_str()).copied().unwrap_or(usize::MAX) < own_rank
            });
            if shadowed || replaced.iter().any(|r: &Replaced| r.name == dep.name) {
                continue;
            }
            replaced.push(Replaced {
                name: dep.name.clone(),
                version: pkg.version.clone(),
                installed_repo: pkg.repo.clone(),
            });
        }
        if !replaced.is_empty() {
            out.push(Replacement {
                name: name.to_string(),
                version: version.to_string(),
                repo: repo.to_string(),
                replaced,
            });
        }
    }
    out
}

/// libalpm's version comparison, via the `vercmp` tool that ships
/// with pacman. Returns Equal if the tool cannot be run: the custom
/// cross-repository scan then simply finds nothing, rather than
/// inventing an ordering with string comparison.
pub(crate) fn system_vercmp(a: &str, b: &str) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    let Ok(out) = Command::new("vercmp").args([a, b]).output() else {
        return Ordering::Equal;
    };
    if !out.status.success() {
        return Ordering::Equal;
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<i32>()
        .map(|n| n.cmp(&0))
        .unwrap_or(Ordering::Equal)
}

/// The candidate pacman would use (first entry, in pacman.conf
/// order) and the best candidate under MinkaSyu's policy: walk the
/// repos in rank order, and promote a lower-ranked entry only when
/// its version is strictly newer than the best so far. Equal
/// versions keep the higher-ranked repository.
fn ranked_candidates<'a>(
    entries: &[&'a alpm::SyncEntry],
    cmp: &mut dyn FnMut(&str, &str) -> Ordering,
) -> Option<(&'a alpm::SyncEntry, &'a alpm::SyncEntry)> {
    let first = *entries.first()?;
    let mut best = first;
    for entry in &entries[1..] {
        if cmp(&entry.version, &best.version) == Ordering::Greater {
            best = entry;
        }
    }
    Some((first, best))
}

/// Merge repo updates (with sync-DB candidate mapping), replacements
/// and AUR updates into one provenance-stamped list.
///
/// Standard updates come from `pacman -Qu`, which matches by package
/// name only: it never reports a package that replaces an installed
/// one under another name, and it lists ignored packages with a
/// trailing ` [ignored]` (dropped by parse_update_line). Replacements
/// come from find_replacements instead. On top of that, a
/// cross-repository scan promotes a strictly newer version from a
/// lower-ranked repository: it replaces the standard candidate when
/// there is one, and adds an update pacman would never offer when the
/// first repository's copy is not newer than the installed package.
/// A package pacman withheld despite its first repository being newer
/// (IgnorePkg and friends) is left alone. Everything except the
/// injected version comparison is pure.
pub fn merge_updates(
    repo_lines: &str,
    aur_lines: &str,
    installed: &HashMap<String, alpm::InstalledPkg>,
    sync_entries: &[alpm::SyncEntry],
    replacements: &[Replacement],
) -> Vec<Update> {
    let mut cache: HashMap<(String, String), Ordering> = HashMap::new();
    merge_updates_with_cmp(repo_lines, aur_lines, installed, sync_entries, replacements, &mut |a, b| {
        *cache
            .entry((a.to_string(), b.to_string()))
            .or_insert_with(|| system_vercmp(a, b))
    })
}

fn merge_updates_with_cmp(
    repo_lines: &str,
    aur_lines: &str,
    installed: &HashMap<String, alpm::InstalledPkg>,
    sync_entries: &[alpm::SyncEntry],
    replacements: &[Replacement],
    cmp: &mut dyn FnMut(&str, &str) -> Ordering,
) -> Vec<Update> {
    let in_sync: HashSet<&str> = sync_entries.iter().map(|e| e.name.as_str()).collect();
    let mut sync_by_name: HashMap<&str, Vec<&alpm::SyncEntry>> = HashMap::new();
    for entry in sync_entries {
        sync_by_name.entry(entry.name.as_str()).or_default().push(entry);
    }
    let aur_names: HashSet<String> = aur_lines
        .lines()
        .filter_map(parse_update_line)
        .map(|(name, _, _)| name)
        .collect();

    let mut updates = Vec::new();
    let mut baseline_names: HashSet<String> = HashSet::new();

    for line in repo_lines.lines() {
        let Some((name, old, new)) = parse_update_line(line) else {
            continue;
        };
        baseline_names.insert(name.clone());
        let mut candidate_repo =
            alpm::candidate_repo(sync_entries, &name, &new).unwrap_or("unknown").to_string();
        let mut new_version = new;
        let mut explicit_target = false;
        let mut first_repo = None;
        let mut first_version = None;

        // Promote a strictly newer version from a lower-ranked repo
        // over pacman's own candidate for this update.
        if let Some(entries) = sync_by_name.get(name.as_str()) {
            if let Some((first, best)) = ranked_candidates(entries, cmp) {
                if best.repo != first.repo
                    && cmp(&best.version, &new_version) == Ordering::Greater
                {
                    explicit_target = true;
                    first_repo = Some(first.repo.clone());
                    first_version = Some(first.version.clone());
                    candidate_repo = best.repo.clone();
                    new_version = best.version.clone();
                }
            }
        }

        let installed_repo = installed.get(&name).and_then(|p| p.repo.clone());
        updates.push(Update {
            transition: transition(&installed_repo, &candidate_repo),
            name,
            old_version: old,
            new_version,
            installed_repo,
            candidate_repo,
            explicit_target,
            first_repo,
            first_version,
            replaces: Vec::new(),
        });
    }

    // Updates pacman cannot see at all: the highest-ranked copy is
    // not newer than what is installed, but a lower-ranked repo has
    // a strictly newer version still.
    for (name, pkg) in installed {
        if baseline_names.contains(name) || aur_names.contains(name) {
            continue;
        }
        let Some(entries) = sync_by_name.get(name.as_str()) else {
            continue;
        };
        let Some((first, best)) = ranked_candidates(entries, cmp) else {
            continue;
        };
        if best.repo == first.repo {
            continue;
        }
        // Pacman withheld a first-repository upgrade (IgnorePkg et
        // al.): respect that rather than routing around it.
        if cmp(&first.version, &pkg.version) == Ordering::Greater {
            continue;
        }
        if cmp(&best.version, &pkg.version) == Ordering::Greater
            && cmp(&best.version, &first.version) == Ordering::Greater
        {
            let installed_repo = pkg.repo.clone();
            updates.push(Update {
                transition: transition(&installed_repo, &best.repo),
                name: name.clone(),
                old_version: pkg.version.clone(),
                new_version: best.version.clone(),
                installed_repo,
                candidate_repo: best.repo.clone(),
                explicit_target: true,
                first_repo: Some(first.repo.clone()),
                first_version: Some(first.version.clone()),
                replaces: Vec::new(),
            });
        }
    }

    for line in aur_lines.lines() {
        let Some((name, old, new)) = parse_update_line(line) else {
            continue;
        };
        // paru -Qua only reports AUR packages, so an AUR candidate's
        // origin is the AUR unless the local DB recorded a sync repo
        // (possible when a package moved from a repo into the AUR).
        let installed_repo = installed
            .get(&name)
            .and_then(|p| p.repo.clone())
            .or_else(|| (!in_sync.contains(name.as_str())).then(|| "aur".to_string()));
        updates.push(Update {
            transition: transition(&installed_repo, "aur"),
            name,
            old_version: old,
            new_version: new,
            installed_repo,
            candidate_repo: "aur".to_string(),
            explicit_target: false,
            first_repo: None,
            first_version: None,
            replaces: Vec::new(),
        });
    }

    // Replacements: libalpm removes the replaced packages, so any row of
    // their own is superseded (and unticking one would pass it to
    // --ignore, which cancels the replacement too). The replacer takes
    // its repo and version from -Syu's own target set, never from the
    // by-name ranking above: a same-named package in a higher repo
    // without the replaces entry is not what -Syu installs.
    let superseded: HashSet<&str> = replacements
        .iter()
        .flat_map(|r| r.replaced.iter().map(|p| p.name.as_str()))
        .collect();
    updates.retain(|u| !superseded.contains(u.name.as_str()));
    for r in replacements {
        if let Some(row) = updates.iter_mut().find(|u| u.name == r.name) {
            row.new_version = r.version.clone();
            row.candidate_repo = r.repo.clone();
            row.transition = transition(&row.installed_repo, &r.repo);
            row.explicit_target = false;
            row.first_repo = None;
            row.first_version = None;
            row.replaces = r.replaced.clone();
        } else if let Some(first) = r.replaced.first() {
            let (old_version, installed_repo) = match installed.get(&r.name) {
                // The replacer is installed already: -Syu reinstalls it
                // and removes the replaced packages.
                Some(own) => (own.version.clone(), own.repo.clone()),
                None => (first.version.clone(), first.installed_repo.clone()),
            };
            updates.push(Update {
                transition: transition(&installed_repo, &r.repo),
                name: r.name.clone(),
                old_version,
                new_version: r.version.clone(),
                installed_repo,
                candidate_repo: r.repo.clone(),
                explicit_target: false,
                first_repo: None,
                first_version: None,
                replaces: r.replaced.clone(),
            });
        }
    }

    updates.sort_by(|a, b| a.name.cmp(&b.name));
    updates
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

pub fn state_dir() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".local/state"));
    base.join("minka-syu")
}

pub fn state_path() -> PathBuf {
    state_dir().join("state.json")
}

fn cache_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".cache"));
    base.join("minka-syu")
}

/// The throwaway pacman DB root a check refreshes. Browsing reads it
/// (when present) so search results reflect the last check's view of
/// the repos rather than the system DB's.
pub fn checkdb_path() -> PathBuf {
    cache_dir().join("checkdb")
}

pub fn load_state() -> Option<State> {
    let text = std::fs::read_to_string(state_path()).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn save_state(state: &State) -> Result<(), String> {
    let dir = state_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let tmp = dir.join("state.json.tmp");
    let json = serde_json::to_string_pretty(state).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, json).map_err(|e| format!("writing state: {e}"))?;
    std::fs::rename(&tmp, state_path()).map_err(|e| format!("publishing state: {e}"))?;
    Ok(())
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn run_capture(program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("running {program}: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(3).collect();
        return Err(format!(
            "{program} failed ({}){}",
            output.status,
            if tail.is_empty() {
                String::new()
            } else {
                format!(": {}", tail.into_iter().rev().collect::<Vec<_>>().join(" "))
            }
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn is_root() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(1).map(|u| u == "0"))
        })
        .unwrap_or(false)
}

/// The checkupdates trick: a private pacman DB root whose `local` is a
/// symlink to the real one, so `-Sy` refreshes only the copy and -Qu
/// against it can never leave the system in partial-upgrade state.
fn refresh_temp_db() -> Result<PathBuf, String> {
    let db = checkdb_path();
    std::fs::create_dir_all(db.join("sync"))
        .map_err(|e| format!("creating temp db: {e}"))?;
    let local_link = db.join("local");
    if !local_link.exists() {
        std::os::unix::fs::symlink("/var/lib/pacman/local", &local_link)
            .map_err(|e| format!("linking local db: {e}"))?;
    }
    let db_str = db.to_str().ok_or("non-UTF8 cache path")?.to_string();
    let mut argv: Vec<&str> = Vec::new();
    if !is_root() {
        argv.push("fakeroot");
    }
    argv.extend(["pacman", "-Sy", "--noconfirm", "--dbpath"]);
    let mut cmd = Command::new(argv[0]);
    cmd.args(&argv[1..]).arg(&db_str).args(["--logfile", "/dev/null"]);
    let output = cmd.output().map_err(|e| format!("running pacman -Sy: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "pacman -Sy (temp db) failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).lines().last().unwrap_or("")
        ));
    }
    Ok(db)
}

/// libalpm's own sysupgrade target set, from the private DB the check
/// just refreshed: what `-Syu` would install, replacements included,
/// which `-Qu` never reports. Print mode needs neither root nor the DB
/// lock and answers its own questions; `-dd` keeps one unsatisfiable
/// dependency elsewhere in the upgrade from failing the whole answer
/// (and drops pulled-in dependencies from the list). pacman prints
/// such failures on stdout, so only a zero exit status counts.
fn sysupgrade_targets(db: &str) -> Result<String, String> {
    let out = Command::new("pacman")
        .args(["-Sup", "-dd", "--dbpath", db, "--logfile", "/dev/null"])
        .args(["--print-format", "%n|%v|%r|%R"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("running pacman -Sup: {e}"))?;
    if !out.status.success() {
        let text = String::from_utf8_lossy(&out.stdout);
        let err = String::from_utf8_lossy(&out.stderr);
        let last = err.lines().chain(text.lines()).rfind(|l| !l.trim().is_empty()).unwrap_or("");
        return Err(format!("pacman -Sup failed ({}): {last}", out.status));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Installed packages IgnorePkg / IgnoreGroup hold back, from
/// pacman-conf (one pattern per line, nothing when unset), matched the
/// way libalpm does: fnmatch on the package name, and on each of its
/// groups (%GROUPS% in the local DB). An unreadable configuration
/// counts as no ignores.
fn ignored_installed(installed: &HashMap<String, alpm::InstalledPkg>) -> HashSet<String> {
    let conf = |key: &str| -> Vec<String> {
        run_capture("pacman-conf", &[key])
            .map(|t| t.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect())
            .unwrap_or_default()
    };
    let pkg_patterns = conf("IgnorePkg");
    let group_patterns = conf("IgnoreGroup");
    let mut out: HashSet<String> = installed
        .keys()
        .filter(|name| pkg_patterns.iter().any(|p| glob_match(p, name)))
        .cloned()
        .collect();
    if !group_patterns.is_empty() {
        for (name, groups) in alpm::read_installed_groups(Path::new("/var/lib/pacman/local")) {
            if groups.iter().any(|g| group_patterns.iter().any(|p| glob_match(p, g))) {
                out.insert(name);
            }
        }
    }
    out
}

/// Run a full check. On success returns the new state (also saved).
/// On failure the previous state file is left untouched: state.json
/// only ever reflects the last *successful* check.
pub fn run_check() -> Result<State, String> {
    let db = refresh_temp_db()?;
    let db_str = db.to_str().ok_or("non-UTF8 cache path")?;

    // pacman -Qu exits 1 when there is simply nothing to do; that is a
    // successful empty answer, not a failure.
    let repo_lines = {
        let out = Command::new("pacman")
            .args(["-Qu", "--dbpath", db_str])
            .output()
            .map_err(|e| format!("running pacman -Qu: {e}"))?;
        if out.status.success() || out.status.code() == Some(1) {
            String::from_utf8_lossy(&out.stdout).into_owned()
        } else {
            return Err(format!(
                "pacman -Qu failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("")
            ));
        }
    };
    let sync_text = run_capture("pacman", &["-Sl", "--dbpath", db_str])?;
    let sync_entries = alpm::parse_sync_list(&sync_text);

    let aur_lines = run_capture("paru", &["-Qua"]).unwrap_or_else(|e| {
        eprintln!("minka-syu: paru -Qua failed, continuing without AUR: {e}");
        String::new()
    });

    let installed = alpm::read_installed(Path::new("/var/lib/pacman/local"));
    let replacements = match sysupgrade_targets(db_str) {
        Ok(text) => {
            let ignored = ignored_installed(&installed);
            find_replacements(&text, &installed, &sync_entries, &ignored, &mut system_vercmp)
        }
        Err(e) => {
            eprintln!("minka-syu: replacement check failed, continuing without it: {e}");
            Vec::new()
        }
    };
    let updates = merge_updates(&repo_lines, &aur_lines, &installed, &sync_entries, &replacements);
    let repo_change_count = updates.iter().filter(|u| u.transition == Transition::Changed).count();

    let previous = load_state();
    let state = State {
        schema: 1,
        checked_at: now_unix(),
        updates,
        repo_change_count,
        last_notified_count: previous.map(|p| p.last_notified_count).unwrap_or(0),
    };
    save_state(&state)?;
    Ok(state)
}

/// Send a desktop notification if the update count grew past the last
/// notified count; persisting the new watermark either way so applying
/// updates re-arms notification for the next batch.
pub fn maybe_notify(state: &mut State) {
    let total = state.total();
    if total > state.last_notified_count {
        let mut body = format!("{total} update{} available", if total == 1 { "" } else { "s" });
        if state.repo_change_count > 0 {
            body.push_str(&format!(
                " ({} change{} repository)",
                state.repo_change_count,
                if state.repo_change_count == 1 { "s" } else { "" }
            ));
        }
        let _ = Command::new("notify-send")
            .args(["-a", "MinkaSyu", "Updates available", &body])
            .status();
    }
    if state.last_notified_count != total {
        state.last_notified_count = total;
        let _ = save_state(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installed_map() -> HashMap<String, alpm::InstalledPkg> {
        HashMap::from([
            (
                "bcachefs-dkms".to_string(),
                alpm::InstalledPkg {
                    version: "3:1.39.7-1.1".into(),
                    repo: Some("cachyos-extra-v4".into()),
                    description: String::new(),
                    size_bytes: 0,
                    explicit: true,
                },
            ),
            (
                "gitkraken".to_string(),
                alpm::InstalledPkg {
                    version: "12.6.1-1".into(),
                    repo: None,
                    description: String::new(),
                    size_bytes: 0,
                    explicit: true,
                },
            ),
        ])
    }

    #[test]
    fn classifies_transitions() {
        let sync = alpm::parse_sync_list(
            "cachyos-extra-v4 bcachefs-dkms 3:1.39.7-1.1\nminka bcachefs-dkms 3:1.39.7-2\n",
        );
        let ups = merge_updates(
            "bcachefs-dkms 3:1.39.7-1.1 -> 3:1.39.7-2\n",
            "gitkraken 12.6.1-1 -> 12.6.2-1\n",
            &installed_map(),
            &sync,
            &[],
        );
        assert_eq!(ups.len(), 2);
        assert_eq!(ups[0].candidate_repo, "minka");
        assert_eq!(ups[0].transition, Transition::Changed);
        assert_eq!(ups[1].candidate_repo, "aur");
        assert_eq!(ups[1].installed_repo.as_deref(), Some("aur"));
        assert_eq!(ups[1].transition, Transition::Same);
    }

    #[test]
    fn finds_newer_version_shadowed_by_first_repo() {
        // The bcachefs-dkms case: pacman -Qu is silent because the
        // highest-ranked repo matches the installed version, while a
        // lower-ranked repo carries a newer one.
        let sync = alpm::parse_sync_list(
            "cachyos-extra-v4 bcachefs-dkms 3:1.39.7-1.1\n\
             extra bcachefs-dkms 3:1.39.7-1\n\
             minka bcachefs-dkms 3:1.39.7-2\n",
        );
        let ups = merge_updates_with_cmp("", "", &installed_map(), &sync, &[], &mut |a, b| a.cmp(b));
        assert_eq!(ups.len(), 1);
        let u = &ups[0];
        assert_eq!(u.name, "bcachefs-dkms");
        assert_eq!(u.old_version, "3:1.39.7-1.1");
        assert_eq!(u.new_version, "3:1.39.7-2");
        assert_eq!(u.candidate_repo, "minka");
        assert_eq!(u.transition, Transition::Changed);
        assert!(u.explicit_target);
        assert_eq!(u.first_repo.as_deref(), Some("cachyos-extra-v4"));
        assert_eq!(u.first_version.as_deref(), Some("3:1.39.7-1.1"));
    }

    #[test]
    fn promotes_lower_repo_over_standard_candidate_only_when_strictly_newer() {
        let sync = alpm::parse_sync_list(
            "cachyos-extra-v4 bcachefs-dkms 3:1.39.7-1.2\n\
             minka bcachefs-dkms 3:1.39.7-2\n",
        );
        let ups = merge_updates_with_cmp(
            "bcachefs-dkms 3:1.39.7-1.1 -> 3:1.39.7-1.2\n",
            "",
            &installed_map(),
            &sync,
            &[],
            &mut |a, b| a.cmp(b),
        );
        assert_eq!(ups.len(), 1);
        assert_eq!(ups[0].new_version, "3:1.39.7-2");
        assert_eq!(ups[0].candidate_repo, "minka");
        assert!(ups[0].explicit_target);

        // Equal versions: the higher-ranked repository keeps the win.
        let tie = alpm::parse_sync_list(
            "cachyos-extra-v4 bcachefs-dkms 3:1.39.7-2\n\
             minka bcachefs-dkms 3:1.39.7-2\n",
        );
        let ups = merge_updates_with_cmp(
            "bcachefs-dkms 3:1.39.7-1.1 -> 3:1.39.7-2\n",
            "",
            &installed_map(),
            &tie,
            &[],
            &mut |a, b| a.cmp(b),
        );
        assert_eq!(ups.len(), 1);
        assert_eq!(ups[0].candidate_repo, "cachyos-extra-v4");
        assert!(!ups[0].explicit_target);
    }

    #[test]
    fn withheld_first_repo_upgrade_is_not_routed_around() {
        // No baseline line despite the first repo being newer than
        // installed: pacman withheld it (IgnorePkg etc.), so the
        // even-newer lower copy must not surface either.
        let sync = alpm::parse_sync_list(
            "cachyos-extra-v4 bcachefs-dkms 3:1.39.7-1.2\n\
             minka bcachefs-dkms 3:1.39.7-2\n",
        );
        let ups = merge_updates_with_cmp("", "", &installed_map(), &sync, &[], &mut |a, b| a.cmp(b));
        assert!(ups.is_empty());
    }

    #[test]
    fn update_line_parsing() {
        assert_eq!(
            parse_update_line("linux 6.1-1 -> 6.2-1"),
            Some(("linux".into(), "6.1-1".into(), "6.2-1".into()))
        );
        assert_eq!(parse_update_line(":: some notice"), None);
        assert_eq!(parse_update_line(""), None);
        // -Qu lists IgnorePkg'd packages too; -Syu will not touch them.
        assert_eq!(parse_update_line("linux 6.1-1 -> 6.2-1 [ignored]"), None);
    }

    fn pkg(version: &str, repo: Option<&str>) -> alpm::InstalledPkg {
        alpm::InstalledPkg {
            version: version.into(),
            repo: repo.map(String::from),
            description: String::new(),
            size_bytes: 0,
            explicit: true,
        }
    }

    // The live situation on 15/11/2026: [cachyos] (ranked first) has a
    // same-named coreutils-uutils without a replaces entry, [minka] has
    // the one that replaces the orphaned, foreign coreutils-hybrid.
    fn coreutils_sync() -> Vec<alpm::SyncEntry> {
        alpm::parse_sync_list(
            "cachyos coreutils-uutils 0.2.2-3\n\
             extra uutils-coreutils 0.12.0-3\n\
             minka coreutils-uutils 0.12.0-1\n",
        )
    }

    fn coreutils_installed() -> HashMap<String, alpm::InstalledPkg> {
        HashMap::from([
            ("coreutils-hybrid".to_string(), pkg("9.4_0.0.28-1", None)),
            ("uutils-coreutils".to_string(), pkg("0.12.0-3", Some("extra"))),
        ])
    }

    #[test]
    fn depstring_parsing() {
        let d = |s| parse_depstring(s);
        assert_eq!(d("old").unwrap(), DepSpec { name: "old".into(), constraint: None });
        assert_eq!(d("kconfig<5.111").unwrap().constraint, Some(("<".into(), "5.111".into())));
        assert_eq!(d("a<=2").unwrap().constraint, Some(("<=".into(), "2".into())));
        assert_eq!(d("a>=2").unwrap().constraint, Some((">=".into(), "2".into())));
        assert_eq!(d("a=1-1").unwrap().constraint, Some(("=".into(), "1-1".into())));
        assert_eq!(d("a>2").unwrap().constraint, Some((">".into(), "2".into())));
        assert_eq!(d(""), None);
        assert_eq!(d("<2"), None);
        assert_eq!(d("a<"), None);
    }

    #[test]
    fn glob_matching_like_fnmatch() {
        assert!(glob_match("linux-cachyos*", "linux-cachyos"));
        assert!(glob_match("linux-cachyos*", "linux-cachyos-headers"));
        assert!(!glob_match("linux-cachyos*", "linux"));
        assert!(glob_match("nvidia-utils", "nvidia-utils"));
        assert!(!glob_match("nvidia-utils", "lib32-nvidia-utils"));
        assert!(glob_match("*nvidia*", "lib32-nvidia-utils"));
        assert!(glob_match("lib?2-*", "lib32-glibc"));
        assert!(glob_match("[ab]x", "bx"));
        assert!(!glob_match("[ab]x", "cx"));
        assert!(glob_match("[!a]x", "bx"));
        assert!(!glob_match("[^a]x", "ax"));
        assert!(glob_match("[a-c]1", "b1"));
        assert!(glob_match("a\\*", "a*"));
        assert!(!glob_match("a\\*", "ab"));
        assert!(glob_match("[x", "[x"));
    }

    #[test]
    fn coreutils_replacement_is_surfaced_from_the_lower_repo() {
        let sync = coreutils_sync();
        let installed = coreutils_installed();
        let reps = find_replacements(
            "coreutils-uutils|0.12.0-1|minka|coreutils-hybrid\n",
            &installed,
            &sync,
            &HashSet::new(),
            &mut system_vercmp,
        );
        assert_eq!(reps.len(), 1);
        let ups = merge_updates_with_cmp("", "", &installed, &sync, &reps, &mut system_vercmp);
        assert_eq!(ups.len(), 1, "{ups:#?}");
        let u = &ups[0];
        assert_eq!(u.name, "coreutils-uutils");
        assert_eq!(u.old_version, "9.4_0.0.28-1");
        assert_eq!(u.new_version, "0.12.0-1");
        assert_eq!(u.candidate_repo, "minka");
        assert_eq!(u.installed_repo, None);
        assert_eq!(u.transition, Transition::UnknownSource);
        // Plain -Syu performs it; no explicit target, no "later repo".
        assert!(!u.explicit_target);
        assert_eq!(u.first_repo, None);
        assert_eq!(
            u.replaces,
            vec![Replaced { name: "coreutils-hybrid".into(), version: "9.4_0.0.28-1".into(), installed_repo: None }]
        );
    }

    #[test]
    fn versioned_replaces_must_hold_for_the_installed_version() {
        // KDE5-era entries like kwallet5 'kwallet<5.111' name installed
        // KF6 packages they no longer apply to.
        let installed = HashMap::from([("kwallet".to_string(), pkg("6.31.0-1.1", Some("extra")))]);
        let sync = alpm::parse_sync_list("extra kwallet5 5.116.0-6\nextra kwallet 6.31.0-1.1\n");
        let none = find_replacements(
            "kwallet5|5.116.0-6|extra|kwallet<5.111 kwallet-secrets\n",
            &installed,
            &sync,
            &HashSet::new(),
            &mut system_vercmp,
        );
        assert!(none.is_empty());
        let old = HashMap::from([("kwallet".to_string(), pkg("5.110.0-1", Some("extra")))]);
        let some = find_replacements(
            "kwallet5|5.116.0-6|extra|kwallet<5.111\n",
            &old,
            &alpm::parse_sync_list("extra kwallet5 5.116.0-6\n"),
            &HashSet::new(),
            &mut system_vercmp,
        );
        assert_eq!(some.len(), 1);
        assert_eq!(some[0].replaced[0].name, "kwallet");
    }

    #[test]
    fn not_a_replacement_when_the_named_package_is_upgraded_ignored_or_shadowed() {
        let installed = HashMap::from([
            ("xx".to_string(), pkg("1-1", Some("lo"))),
            ("yy".to_string(), pkg("1-1", Some("hi"))),
        ]);
        let sync = alpm::parse_sync_list("hi yy 2-1\nlo xx 2-1\n");
        // yy is a target of its own: it is upgraded, not replaced.
        let upgraded =
            find_replacements("xx|2-1|lo|yy\nyy|2-1|hi|\n", &installed, &sync, &HashSet::new(), &mut system_vercmp);
        assert!(upgraded.is_empty());
        // yy held back by IgnorePkg: libalpm skips the replacement.
        let ignored = HashSet::from(["yy".to_string()]);
        let held = find_replacements("xx|2-1|lo|yy\n", &installed, &sync, &ignored, &mut system_vercmp);
        assert!(held.is_empty());
        // A higher repo carries yy by name (even at the installed
        // version): libalpm stops there and never sees lo's replacer.
        let same = alpm::parse_sync_list("hi yy 1-1\nlo xx 2-1\n");
        let shadowed = find_replacements("xx|2-1|lo|yy\n", &installed, &same, &HashSet::new(), &mut system_vercmp);
        assert!(shadowed.is_empty());
        // In the replacer's own repo the replacer wins over the literal.
        let own = alpm::parse_sync_list("lo xx 2-1\nlo yy 1-1\n");
        let wins = find_replacements("xx|2-1|lo|yy\n", &installed, &own, &HashSet::new(), &mut system_vercmp);
        assert_eq!(wins.len(), 1);
    }

    #[test]
    fn replacer_rows_fold_and_replaced_rows_go() {
        let sync = coreutils_sync();
        let mut installed = coreutils_installed();
        // The replacer already installed from [cachyos], with a -Qu
        // literal update there that the cross-repo scan would promote.
        installed.insert("coreutils-uutils".into(), pkg("0.2.1-1", Some("cachyos")));
        let reps = vec![Replacement {
            name: "coreutils-uutils".into(),
            version: "0.12.0-1".into(),
            repo: "minka".into(),
            replaced: vec![Replaced { name: "coreutils-hybrid".into(), version: "9.4_0.0.28-1".into(), installed_repo: None }],
        }];
        // And the replaced package somehow has rows of its own (AUR).
        let ups = merge_updates_with_cmp(
            "coreutils-uutils 0.2.1-1 -> 0.2.2-3\n",
            "coreutils-hybrid 9.4_0.0.28-1 -> 9.5-1\n",
            &installed,
            &sync,
            &reps,
            &mut system_vercmp,
        );
        assert_eq!(ups.len(), 1, "{ups:#?}");
        let u = &ups[0];
        assert_eq!(u.name, "coreutils-uutils");
        assert_eq!(u.old_version, "0.2.1-1");
        assert_eq!(u.new_version, "0.12.0-1");
        assert_eq!(u.candidate_repo, "minka");
        assert_eq!(u.installed_repo.as_deref(), Some("cachyos"));
        assert_eq!(u.transition, Transition::Changed);
        assert!(!u.explicit_target);
        assert_eq!(u.first_repo, None);
        assert_eq!(u.replaces.len(), 1);
    }

    #[test]
    fn sysupgrade_lines_without_replaces_are_ignored() {
        let installed = coreutils_installed();
        let reps = find_replacements(
            "nautilus|51.1-1.1|cachyos-extra-v4|\nbroken line\n|1|x|y\n",
            &installed,
            &coreutils_sync(),
            &HashSet::new(),
            &mut system_vercmp,
        );
        assert!(reps.is_empty());
    }

    #[test]
    fn state_without_replaces_still_loads() {
        // A state.json written by MinkaSyu 0.5.0 has no `replaces` key.
        let json = r#"{"schema":1,"checked_at":0,"repo_change_count":0,"last_notified_count":0,
            "updates":[{"name":"a","old_version":"1","new_version":"2","installed_repo":null,
            "candidate_repo":"extra","transition":"unknown_source"}]}"#;
        let state: State = serde_json::from_str(json).unwrap();
        assert!(state.updates[0].replaces.is_empty());
    }
}

