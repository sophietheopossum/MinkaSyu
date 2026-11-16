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
    /// Installed packages this candidate replaces (its %REPLACES%, as
    /// libalpm attributes them): installing it removes them. The row is
    /// the replacer, named after it; `old_version` / `installed_repo`
    /// describe the first package it replaces unless the replacer is
    /// itself installed. Empty for an ordinary update.
    #[serde(default)]
    pub replaces: Vec<Replaced>,
    /// pacman.conf holds this back, so -Syu skips it: shown so it is not
    /// forgotten, never ticked, counted or targeted. See `held_by`.
    #[serde(default)]
    pub held: bool,
    /// Why it is held: "ignore" (IgnorePkg / IgnoreGroup, on the
    /// installed package or on the new build) or "usage" (no repository
    /// with `Usage = Upgrade` carries the newer build).
    #[serde(default)]
    pub held_by: Option<String>,
    /// Replacements (by name) that remove this package. The row is shown
    /// for information and never applied as listed: unticking the
    /// replacement (--ignore) skips that replacement, and pacman then
    /// decides what happens to this package (it may upgrade it, hold it,
    /// or hand it to another replacer); the next check shows the result.
    #[serde(default)]
    pub superseded_by: Vec<String>,
    /// Not installed before: a package the run adds because it replaces
    /// one that the same run installs again (an Arch split, foo-gtk
    /// replacing foo while depending on it). `old_version` is empty.
    #[serde(default)]
    pub new_install: bool,
    /// Installed packages this replacer is credited with that the same
    /// run installs again (so they are not in `replaces`). Ignoring one
    /// of them makes libalpm skip this replacement too, so the window
    /// keeps them ticked while this row is.
    #[serde(default)]
    pub reinstalls: Vec<String>,
}

impl Update {
    /// Whether a plain run applies this row as listed: not held, and not
    /// standing for a package that a replacement removes.
    pub fn pending(&self) -> bool {
        !self.held && self.superseded_by.is_empty()
    }
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
    /// Updates a plain `-Syu` applies (see Update::pending).
    pub fn total(&self) -> usize {
        self.updates.iter().filter(|u| u.pending()).count()
    }

    pub fn held(&self) -> usize {
        self.updates.iter().filter(|u| u.held).count()
    }
}

fn transition(installed_repo: &Option<String>, candidate_repo: &str) -> Transition {
    match installed_repo {
        Some(repo) if repo == candidate_repo => Transition::Same,
        Some(_) => Transition::Changed,
        None => Transition::UnknownSource,
    }
}

/// One parsed `name old -> new` line (the shared output shape of
/// `pacman -Qu` and `paru -Qua`).
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateLine {
    pub name: String,
    pub old: String,
    pub new: String,
    /// The line ends in ` [ignored]`: -Qu lists the first repository's
    /// copy even when IgnorePkg/IgnoreGroup hold the installed package
    /// back or that repository lacks `Usage = Upgrade`.
    pub held: bool,
}

/// Parse one `name old -> new [ignored]` line; anything else is None.
pub fn parse_update_line(line: &str) -> Option<UpdateLine> {
    let mut parts = line.split_whitespace();
    let name = parts.next()?;
    let old = parts.next()?;
    if parts.next()? != "->" {
        return None;
    }
    let new = parts.next()?;
    Some(UpdateLine {
        name: name.to_string(),
        old: old.to_string(),
        new: new.to_string(),
        held: parts.next() == Some("[ignored]"),
    })
}

/// A `%REPLACES%` entry: a package name with an optional version
/// constraint (`<`, `<=`, `=`, `>=`, `>`), as libalpm writes depstrings.
#[derive(Debug, Clone, PartialEq)]
pub struct DepSpec {
    pub name: String,
    pub constraint: Option<(String, String)>,
}

pub fn parse_depstring(s: &str) -> Option<DepSpec> {
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

/// IgnorePkg / IgnoreGroup matching exactly as libalpm does it
/// (_alpm_fnmatch: glibc's fnmatch(3) with no flags), so character
/// classes, ranges, escapes and malformed patterns all behave the same.
pub(crate) fn glob_match(pattern: &str, text: &str) -> bool {
    use std::ffi::{c_char, c_int, CString};
    extern "C" {
        fn fnmatch(pattern: *const c_char, string: *const c_char, flags: c_int) -> c_int;
    }
    let (Ok(p), Ok(t)) = (CString::new(pattern), CString::new(text)) else {
        return false;
    };
    // SAFETY: both are valid NUL-terminated C strings that outlive the call.
    unsafe { fnmatch(p.as_ptr(), t.as_ptr(), 0) == 0 }
}

/// What the check needs from pacman.conf, via pacman-conf.
#[derive(Debug, Clone, Default)]
pub struct Conf {
    /// Repos `-Su` upgrades from (`Usage` includes Upgrade, or All).
    /// None when pacman-conf could not say: then every repo counts.
    pub upgrade_repos: Option<HashSet<String>>,
    /// Repos an explicit `repo/name` target can install from (`Usage`
    /// includes Install, or All). None as above.
    pub install_repos: Option<HashSet<String>>,
    pub pkg_patterns: Vec<String>,
    pub group_patterns: Vec<String>,
    /// Installed packages IgnorePkg / IgnoreGroup hold back (by name,
    /// and by the groups in the local DB).
    pub ignored: HashSet<String>,
}

impl Conf {
    fn upgrades_from(&self, repo: &str) -> bool {
        self.upgrade_repos.as_ref().is_none_or(|r| r.contains(repo))
    }

    fn installs_from(&self, repo: &str) -> bool {
        self.install_repos.as_ref().is_none_or(|r| r.contains(repo))
    }

    /// Whether IgnorePkg / IgnoreGroup hold back a sync build.
    fn ignores_build(&self, name: &str, groups: &[String]) -> bool {
        self.pkg_patterns.iter().any(|p| glob_match(p, name))
            || groups.iter().any(|g| self.group_patterns.iter().any(|p| glob_match(p, g)))
    }
}

/// The print format of every target query: name, version, repo, then
/// replaces, conflicts and provides (space-separated depstrings).
const TARGET_FORMAT: &str = "%n|%v|%r|%R|%H|%P";

/// One line of a print-mode target query (`pacman -Sup ...
/// --print-format TARGET_FORMAT`): a target of libalpm's own
/// transaction, with its repo and relations.
#[derive(Debug, Clone, PartialEq)]
pub struct SupTarget {
    pub name: String,
    pub version: String,
    pub repo: String,
    pub replaces: Vec<DepSpec>,
    pub conflicts: Vec<DepSpec>,
    pub provides: Vec<DepSpec>,
}

impl SupTarget {
    /// Whether this package conflicts with `other` the way libalpm's
    /// inner-conflict check decides it: a conflicts entry names the
    /// other package (constraint against its version) or something it
    /// provides (constraint against the provided version, if any).
    fn conflicts_with(&self, other: &SupTarget, cmp: &mut dyn FnMut(&str, &str) -> Ordering) -> bool {
        self.conflicts.iter().any(|c| {
            (c.name == other.name && satisfies(&other.version, &c.constraint, cmp))
                || other.provides.iter().any(|p| {
                    p.name == c.name
                        && match (&c.constraint, &p.constraint) {
                            (None, _) => true,
                            (Some(_), Some((op, v))) if op == "=" => satisfies(v, &c.constraint, cmp),
                            (Some(_), _) => false,
                        }
                })
        })
    }
}

pub fn parse_sysupgrade(text: &str) -> Vec<SupTarget> {
    let deps = |f: Option<&str>| -> Vec<DepSpec> {
        f.unwrap_or("").split_whitespace().filter_map(parse_depstring).collect()
    };
    text.lines()
        .filter_map(|line| {
            let mut f = line.splitn(6, '|');
            let (name, version, repo) = (f.next()?, f.next()?, f.next()?);
            if name.is_empty() || version.is_empty() || repo.is_empty() {
                return None;
            }
            Some(SupTarget {
                name: name.to_string(),
                version: version.to_string(),
                repo: repo.to_string(),
                replaces: deps(f.next()),
                conflicts: deps(f.next()),
                provides: deps(f.next()),
            })
        })
        .collect()
}

/// A sysupgrade target that replaces installed packages.
#[derive(Debug, Clone, PartialEq)]
pub struct Replacement {
    pub name: String,
    pub version: String,
    pub repo: String,
    pub replaced: Vec<Replaced>,
}

/// Repos in pacman.conf order (as `pacman -Sl` lists them) that `-Su`
/// upgrades from.
fn upgrade_order<'a>(sync_entries: &'a [alpm::SyncEntry], conf: &Conf) -> Vec<&'a str> {
    let mut out: Vec<&str> = Vec::new();
    for e in sync_entries {
        if !out.contains(&e.repo.as_str()) && conf.upgrades_from(&e.repo) {
            out.push(&e.repo);
        }
    }
    out
}

/// Which installed packages each of `-Su`'s targets removes.
///
/// pacman prints what a sysupgrade installs, not what it removes, so
/// the removals are attributed by libalpm's own walk
/// (lib/libalpm/sync.c: alpm_sync_sysupgrade, check_replacers,
/// check_literal), run over the target set pacman already chose:
/// - installed packages in name order (strcmp), skipping any already
///   added as a target;
/// - for each one, L, the repos it upgrades from in pacman.conf order;
/// - in each repo, first the targets from it whose replaces names L
///   (by name only, constraint checked against L's installed version;
///   none at all when IgnorePkg/IgnoreGroup hold L back). A new one is
///   added and credited with L; one added earlier from the same repo is
///   credited too; one added from another repo is skipped ("cannot
///   replace"). A new one ends the walk;
/// - otherwise a package named L in that repo ends it, and is added if
///   -Su lists it from there.
pub fn attribute_replacements(
    targets: &[SupTarget],
    installed: &HashMap<String, alpm::InstalledPkg>,
    sync_entries: &[alpm::SyncEntry],
    conf: &Conf,
    cmp: &mut dyn FnMut(&str, &str) -> Ordering,
) -> Vec<Replacement> {
    let repos = upgrade_order(sync_entries, conf);
    let literal: HashSet<(&str, &str)> =
        sync_entries.iter().map(|e| (e.repo.as_str(), e.name.as_str())).collect();
    let target_at: HashSet<(&str, &str)> =
        targets.iter().map(|t| (t.repo.as_str(), t.name.as_str())).collect();
    let mut by_repo: HashMap<&str, Vec<&SupTarget>> = HashMap::new();
    for t in targets {
        by_repo.entry(t.repo.as_str()).or_default().push(t);
    }
    for list in by_repo.values_mut() {
        list.sort_by(|a, b| a.name.cmp(&b.name));
    }

    // target name -> (repo it was added from, installed packages it removes)
    let mut added: HashMap<String, (String, Vec<Replaced>)> = HashMap::new();
    let mut names: Vec<&String> = installed.keys().collect();
    names.sort();
    for local in names {
        if added.contains_key(local.as_str()) {
            continue;
        }
        let pkg = &installed[local];
        let held = conf.ignored.contains(local);
        for repo in &repos {
            let mut new_replacer = false;
            if !held {
                for t in by_repo.get(repo).into_iter().flatten() {
                    if t.name == *local {
                        continue;
                    }
                    let replaces_local = t
                        .replaces
                        .iter()
                        .any(|d| d.name == *local && satisfies(&pkg.version, &d.constraint, cmp));
                    if !replaces_local {
                        continue;
                    }
                    let replaced = Replaced {
                        name: local.clone(),
                        version: pkg.version.clone(),
                        installed_repo: pkg.repo.clone(),
                    };
                    match added.get_mut(&t.name) {
                        Some((from, list)) if from == repo => list.push(replaced),
                        Some(_) => {}
                        None => {
                            added.insert(t.name.clone(), (repo.to_string(), vec![replaced]));
                            new_replacer = true;
                        }
                    }
                }
            }
            if new_replacer {
                break;
            }
            if literal.contains(&(*repo, local.as_str())) {
                if target_at.contains(&(*repo, local.as_str())) {
                    added.entry(local.clone()).or_insert_with(|| (repo.to_string(), Vec::new()));
                }
                break;
            }
        }
    }

    let mut out: Vec<Replacement> = added
        .into_iter()
        .filter(|(_, (_, list))| !list.is_empty())
        .filter_map(|(name, (repo, replaced))| {
            let t = targets.iter().find(|t| t.name == name && t.repo == repo)?;
            Some(Replacement { name, version: t.version.clone(), repo, replaced })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
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

/// Everything one check gathered, for the pure merge.
#[derive(Debug, Clone, Copy)]
pub struct Inputs<'a> {
    /// `pacman -Qu` on the private DB.
    pub repo_lines: &'a str,
    /// `paru -Qua`.
    pub aur_lines: &'a str,
    pub installed: &'a HashMap<String, alpm::InstalledPkg>,
    /// `pacman -Sl` on the private DB, in pacman.conf order.
    pub sync_entries: &'a [alpm::SyncEntry],
    /// `-Su`'s own target set (`-Sup -dd`); None when that failed, and
    /// the merge then falls back to what `-Qu` and pacman.conf say.
    pub sysupgrade: Option<&'a [SupTarget]>,
    /// The same query without `-dd`, made only when there are
    /// replacements: everything the transaction installs, including what
    /// dependencies pull back in. None: not made, or it failed.
    pub final_targets: Option<&'a [SupTarget]>,
    pub conf: &'a Conf,
}

/// Merge repo updates, replacements and AUR updates into one
/// provenance-stamped list.
///
/// The baseline is `pacman -Qu`, which matches packages by name only:
/// it never reports a replacement, it lists the first repository's copy
/// even where -Su takes another (a first repository without
/// `Usage = Upgrade`), and it lists held packages with ` [ignored]` but
/// not those held back by IgnoreGroup on the new build. Where `-Sup`
/// answered, its target set settles all three: a -Qu package it does
/// not upgrade (and no replacement removes) is held, and one it does
/// takes its repo and version from there. Replacements are attributed
/// by attribute_replacements. On top of that, a cross-repository scan
/// promotes a strictly newer version from a lower-ranked repository it
/// can install from: it replaces the standard candidate when there is
/// one, and adds an update pacman would never offer when the first
/// repository's copy is not newer than the installed package. Held
/// rows are never promoted. Everything except the injected version
/// comparison is pure.
pub fn merge_updates(inputs: &Inputs) -> Vec<Update> {
    let mut cache: HashMap<(String, String), Ordering> = HashMap::new();
    merge_updates_with_cmp(inputs, &mut |a, b| {
        *cache
            .entry((a.to_string(), b.to_string()))
            .or_insert_with(|| system_vercmp(a, b))
    })
}

fn merge_updates_with_cmp(inputs: &Inputs, cmp: &mut dyn FnMut(&str, &str) -> Ordering) -> Vec<Update> {
    let Inputs { repo_lines, aur_lines, installed, sync_entries, sysupgrade, final_targets, conf } = *inputs;
    let replacements = sysupgrade
        .map(|targets| attribute_replacements(targets, installed, sync_entries, conf, cmp))
        .unwrap_or_default();
    let replacer_names: HashSet<String> = replacements.iter().map(|r| r.name.clone()).collect();
    // What the transaction ends up installing: the run without -dd when
    // there was one, else -Su's own set.
    let final_set = final_targets.or(sysupgrade);
    let sup_by_name: Option<HashMap<&str, &SupTarget>> =
        final_set.map(|targets| targets.iter().map(|t| (t.name.as_str(), t)).collect());
    // A replaced package the same transaction installs again (an Arch
    // split, where the replacer depends on it; or libalpm's append path,
    // which credits it to a replacer and still adds its newer build) is
    // upgraded, not gone: superseded only when its new build conflicts
    // with the replacer either way. libalpm's inner-conflict step then
    // drops it if the replacer provides its name, and fails otherwise.
    // Rows say only what is really gone, so such a package is dropped
    // from its replacer's list too.
    let mut superseded: HashMap<String, Vec<String>> = HashMap::new();
    let mut reinstalls: HashMap<String, Vec<String>> = HashMap::new();
    let mut replacements = replacements;
    for r in &mut replacements {
        let mut again_names: Vec<String> = Vec::new();
        let replacer = final_set
            .into_iter()
            .flatten()
            .chain(sysupgrade.into_iter().flatten())
            .find(|t| t.name == r.name && t.repo == r.repo);
        r.replaced.retain(|p| {
            let again = sup_by_name.as_ref().and_then(|m| m.get(p.name.as_str()).copied());
            let gone = match (again, replacer) {
                (Some(again), Some(replacer)) => {
                    replacer.conflicts_with(again, cmp) || again.conflicts_with(replacer, cmp)
                }
                _ => true,
            };
            if !gone {
                again_names.push(p.name.clone());
            }
            gone
        });
        reinstalls.insert(r.name.clone(), again_names);
        for p in &r.replaced {
            superseded.entry(p.name.clone()).or_default().push(r.name.clone());
        }
    }

    let in_sync: HashSet<&str> = sync_entries.iter().map(|e| e.name.as_str()).collect();
    // Explicit candidates: only repos an explicit target can install from.
    let mut sync_by_name: HashMap<&str, Vec<&alpm::SyncEntry>> = HashMap::new();
    for entry in sync_entries.iter().filter(|e| conf.installs_from(&e.repo)) {
        sync_by_name.entry(entry.name.as_str()).or_default().push(entry);
    }
    let aur_names: HashSet<String> = aur_lines
        .lines()
        .filter_map(parse_update_line)
        .map(|l| l.name)
        .collect();

    // What -Su does to installed packages by name: -Qu's lines, plus any
    // -Sup target -Qu missed (its first repository is not newer, while
    // a later one -Su upgrades from is).
    let mut baseline: Vec<UpdateLine> = repo_lines.lines().filter_map(parse_update_line).collect();
    if let Some(targets) = final_set {
        for t in targets {
            let Some(pkg) = installed.get(&t.name) else { continue };
            if replacer_names.contains(t.name.as_str()) || baseline.iter().any(|l| l.name == t.name) {
                continue;
            }
            // Same version, nothing else removed: a package that lists
            // itself in %REPLACES% (check_replacers has no self-check),
            // which every -Syu reinstalls unchanged. Not an update.
            if cmp(&t.version, &pkg.version) == Ordering::Equal {
                continue;
            }
            baseline.push(UpdateLine {
                name: t.name.clone(),
                old: pkg.version.clone(),
                new: t.version.clone(),
                held: false,
            });
        }
    }

    let mut updates = Vec::new();
    let mut baseline_names: HashSet<String> = HashSet::new();

    for UpdateLine { name, old, new, held: qu_held } in baseline {
        baseline_names.insert(name.clone());
        let mut candidate_repo =
            alpm::candidate_repo(sync_entries, &name, &new).unwrap_or("unknown").to_string();
        let mut new_version = new;
        let locally_ignored = conf.ignored.contains(&name);
        // " [ignored]" without IgnorePkg/IgnoreGroup on the installed
        // package means the first repository lacks Usage = Upgrade.
        let reason = if qu_held && !locally_ignored { "usage" } else { "ignore" };
        let held_by = match &sup_by_name {
            Some(sup) => match sup.get(name.as_str()) {
                Some(t) => {
                    candidate_repo = t.repo.clone();
                    new_version = t.version.clone();
                    None
                }
                None if superseded.contains_key(&name) || replacer_names.contains(name.as_str()) => None,
                None => Some(reason),
            },
            None => (qu_held || locally_ignored).then_some(reason),
        };
        let held = held_by.is_some();
        let mut explicit_target = false;
        let mut first_repo = None;
        let mut first_version = None;

        // Promote a strictly newer version from a lower-ranked repo
        // over pacman's own candidate. Not for a held package: it stays
        // pacman's candidate, shown and left alone.
        if let Some(entries) = sync_by_name.get(name.as_str()).filter(|_| !held) {
            let mut best: Option<&alpm::SyncEntry> = None;
            for e in entries {
                let current = best.map_or(new_version.as_str(), |b| b.version.as_str());
                if cmp(&e.version, current) == Ordering::Greater {
                    best = Some(e);
                }
            }
            if let Some(best) = best.filter(|b| b.repo != candidate_repo) {
                explicit_target = true;
                first_repo = Some(std::mem::replace(&mut candidate_repo, best.repo.clone()));
                first_version = Some(std::mem::replace(&mut new_version, best.version.clone()));
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
            held,
            held_by: held_by.map(String::from),
            superseded_by: Vec::new(),
            new_install: false,
            reinstalls: Vec::new(),
        });
    }

    // Updates pacman cannot see at all: the highest-ranked copy is
    // not newer than what is installed, but a lower-ranked repo has
    // a strictly newer version still.
    for (name, pkg) in installed {
        if baseline_names.contains(name) || aur_names.contains(name) || replacer_names.contains(name.as_str()) {
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
            // Shown, but never offered as a target when held: an explicit
            // repo/name of an ignored package makes pacman ask to
            // install it anyway. (IgnoreGroup on the new build is checked
            // later, by annotate_explicit.)
            let held = conf.ignored.contains(name);
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
                held,
                held_by: held.then(|| "ignore".to_string()),
                superseded_by: Vec::new(),
                new_install: false,
                reinstalls: Vec::new(),
            });
        }
    }

    for line in aur_lines.lines() {
        let Some(UpdateLine { name, old, new, held }) = parse_update_line(line) else {
            continue;
        };
        let held = held || conf.ignored.contains(&name);
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
            held,
            held_by: held.then(|| "ignore".to_string()),
            superseded_by: Vec::new(),
            new_install: false,
            reinstalls: Vec::new(),
        });
    }

    // Replacements take their repo and version from -Su's own target
    // set, never from the by-name ranking above: a same-named package in
    // a higher repo without the replaces entry is not what -Syu installs.
    for r in &replacements {
        if let Some(row) = updates.iter_mut().find(|u| u.name == r.name) {
            row.new_version = r.version.clone();
            row.candidate_repo = r.repo.clone();
            row.transition = transition(&row.installed_repo, &r.repo);
            row.explicit_target = false;
            row.first_repo = None;
            row.first_version = None;
            row.replaces = r.replaced.clone();
            row.reinstalls = reinstalls.get(&r.name).cloned().unwrap_or_default();
            row.held = false;
            row.held_by = None;
        } else {
            let (old_version, installed_repo) = match (installed.get(&r.name), r.replaced.first()) {
                // The replacer is installed already: -Syu reinstalls it
                // and removes the replaced packages.
                (Some(own), _) => (own.version.clone(), own.repo.clone()),
                (None, Some(first)) => (first.version.clone(), first.installed_repo.clone()),
                // Everything it replaces is installed again: a new package.
                (None, None) => (String::new(), None),
            };
            let new_install = !installed.contains_key(&r.name) && r.replaced.is_empty();
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
                held: false,
                held_by: None,
                superseded_by: Vec::new(),
                new_install,
                reinstalls: reinstalls.get(&r.name).cloned().unwrap_or_default(),
            });
        }
    }
    // A replaced package keeps its own row, marked, for information: what
    // -Syu does to it once the replacement is unticked depends on the
    // whole selection, so the row never promises an outcome.
    for u in &mut updates {
        if replacer_names.contains(u.name.as_str()) {
            continue;
        }
        if let Some(by) = superseded.get(&u.name) {
            u.superseded_by = by.clone();
        }
    }

    updates.sort_by(|a, b| a.name.cmp(&b.name));
    updates
}

/// Groups and replaces of an explicit candidate's build: -Su's target
/// set cannot speak for builds a plain -Syu never selects.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CandidateInfo {
    pub groups: Vec<String>,
    pub replaces: Vec<DepSpec>,
}

/// The print format of explicit-build queries: name, repo, groups,
/// replaces. Print mode never wraps lines or translates field names,
/// which `pacman -Si` does.
const BUILD_FORMAT: &str = "%n|%r|%G|%R";

/// Parse `pacman -Sp ... --print-format BUILD_FORMAT` lines.
pub fn parse_candidate_info(text: &str) -> HashMap<(String, String), CandidateInfo> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.splitn(4, '|');
            let (name, repo) = (f.next()?, f.next()?);
            if name.is_empty() || repo.is_empty() {
                return None;
            }
            let groups = f.next().unwrap_or("").split_whitespace().map(String::from).collect();
            let replaces = f.next().unwrap_or("").split_whitespace().filter_map(parse_depstring).collect();
            Some(((repo.to_string(), name.to_string()), CandidateInfo { groups, replaces }))
        })
        .collect()
}

/// Finish explicit (later-repo) rows with their build's own data.
///
/// IgnoreGroup on the later build: libalpm checks the build as well as
/// the installed package. A row that also has a standard upgrade (the
/// first repo's build is newer than the installed one) falls back to
/// it, which -Syu still performs; a row with nothing else is held.
///
/// Replaces: ticking the row adds the build before the sysupgrade walk,
/// which then removes the packages it replaces, if:
/// - its repo has Usage = Upgrade (the walk runs check_replacers on
///   upgrade repos only);
/// - the package is installed, satisfies the constraint, is not held
///   back, and is not itself an upgrade target or another explicit row
///   (then it is already in the target list, or ignored);
/// - no upgrade repo ranked above the build's offers a package of that
///   name or another replacer whose constraint it satisfies.
pub fn annotate_explicit(
    updates: &mut [Update],
    info: &HashMap<(String, String), CandidateInfo>,
    inputs: &Inputs,
    cmp: &mut dyn FnMut(&str, &str) -> Ordering,
) {
    let Inputs { installed, sync_entries, sysupgrade, final_targets, conf, .. } = *inputs;
    for u in updates.iter_mut().filter(|u| u.explicit_target && !u.held) {
        let Some(build) = info.get(&(u.candidate_repo.clone(), u.name.clone())) else {
            continue;
        };
        if !conf.ignores_build(&u.name, &build.groups) {
            continue;
        }
        let standard = u.first_version.as_deref().is_some_and(|v| cmp(v, &u.old_version) == Ordering::Greater);
        if standard {
            u.candidate_repo = u.first_repo.take().unwrap_or_default();
            u.new_version = u.first_version.take().unwrap_or_default();
            u.explicit_target = false;
            u.transition = transition(&u.installed_repo, &u.candidate_repo);
        } else {
            u.held = true;
            u.held_by = Some("ignore".to_string());
        }
    }

    let repos = upgrade_order(sync_entries, conf);
    let rank = |repo: &str| repos.iter().position(|r| *r == repo).unwrap_or(usize::MAX);
    let targets = final_targets.or(sysupgrade);
    let target_names: HashSet<&str> = targets.into_iter().flatten().map(|t| t.name.as_str()).collect();
    let own_rows: HashSet<String> = updates
        .iter()
        .filter(|u| u.explicit_target && u.pending())
        .map(|u| u.name.clone())
        .collect();
    for u in updates.iter_mut().filter(|u| u.explicit_target && u.pending()) {
        let Some(build) = info.get(&(u.candidate_repo.clone(), u.name.clone())) else {
            continue;
        };
        if !conf.upgrades_from(&u.candidate_repo) {
            continue;
        }
        let own_rank = rank(&u.candidate_repo);
        for dep in &build.replaces {
            if dep.name == u.name
                || target_names.contains(dep.name.as_str())
                || own_rows.contains(&dep.name)
                || conf.ignored.contains(&dep.name)
            {
                continue;
            }
            let Some(pkg) = installed.get(&dep.name) else { continue };
            if !satisfies(&pkg.version, &dep.constraint, cmp) {
                continue;
            }
            let literal_above = sync_entries.iter().any(|e| e.name == dep.name && rank(&e.repo) < own_rank);
            let replacer_above = sysupgrade.into_iter().flatten().any(|t| {
                rank(&t.repo) < own_rank
                    && t.replaces.iter().any(|d| d.name == dep.name && satisfies(&pkg.version, &d.constraint, cmp))
            });
            if literal_above || replacer_above || u.replaces.iter().any(|r| r.name == dep.name) {
                continue;
            }
            u.replaces.push(Replaced {
                name: dep.name.clone(),
                version: pkg.version.clone(),
                installed_repo: pkg.repo.clone(),
            });
        }
    }
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
/// lock and answers its own questions. With `skip_deps` (`-dd`) one
/// unsatisfiable dependency elsewhere in the upgrade cannot fail the
/// whole answer, and pulled-in dependencies are left out; without it,
/// the answer is everything the transaction installs. pacman prints
/// such failures on stdout, so only a zero exit status counts.
fn sysupgrade_targets(db: &str, skip_deps: bool) -> Result<Vec<SupTarget>, String> {
    let mut cmd = Command::new("pacman");
    cmd.args(["-Sup", "--dbpath", db, "--logfile", "/dev/null", "--print-format", TARGET_FORMAT]);
    if skip_deps {
        cmd.arg("-dd");
    }
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("running pacman -Sup: {e}"))?;
    if !out.status.success() {
        let text = String::from_utf8_lossy(&out.stdout);
        let err = String::from_utf8_lossy(&out.stderr);
        let last = err.lines().chain(text.lines()).rfind(|l| !l.trim().is_empty()).unwrap_or("");
        return Err(format!("pacman -Sup failed ({}): {last}", out.status));
    }
    Ok(parse_sysupgrade(&String::from_utf8_lossy(&out.stdout)))
}

/// pacman.conf as the check needs it, via pacman-conf (one value per
/// line; nothing when a list is unset; `All` when a repo has no Usage).
/// What cannot be read counts as permissive: no ignores, every repo
/// usable, which is what the check assumed before it read any of this.
fn read_conf(installed: &HashMap<String, alpm::InstalledPkg>) -> Conf {
    let lines = |args: &[&str]| -> Option<Vec<String>> {
        run_capture("pacman-conf", args)
            .ok()
            .map(|t| t.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect())
    };
    let pkg_patterns = lines(&["IgnorePkg"]).unwrap_or_default();
    let group_patterns = lines(&["IgnoreGroup"]).unwrap_or_default();

    let (mut upgrade, mut install) = (HashSet::new(), HashSet::new());
    let mut known = true;
    for repo in lines(&["--repo-list"]).unwrap_or_default() {
        let Some(usage) = lines(&[&format!("--repo={repo}"), "Usage"]) else {
            known = false;
            break;
        };
        let has = |u: &str| usage.iter().any(|v| v == u || v == "All");
        if has("Upgrade") {
            upgrade.insert(repo.clone());
        }
        if has("Install") {
            install.insert(repo);
        }
    }
    let known = known && !(upgrade.is_empty() && install.is_empty());

    let mut ignored: HashSet<String> = installed
        .keys()
        .filter(|name| pkg_patterns.iter().any(|p| glob_match(p, name)))
        .cloned()
        .collect();
    if !group_patterns.is_empty() {
        for (name, groups) in alpm::read_installed_groups(Path::new("/var/lib/pacman/local")) {
            if groups.iter().any(|g| group_patterns.iter().any(|p| glob_match(p, g))) {
                ignored.insert(name);
            }
        }
    }
    Conf {
        upgrade_repos: known.then_some(upgrade),
        install_repos: known.then_some(install),
        pkg_patterns,
        group_patterns,
        ignored,
    }
}

/// Groups and replaces of the explicit rows' builds, in one print-mode
/// `pacman -Sp -dd` of their repo/name targets on the private DB.
/// Failure means no information (no holds, no disclosures).
fn explicit_candidate_info(db: &str, updates: &[Update]) -> HashMap<(String, String), CandidateInfo> {
    let targets: Vec<String> = updates
        .iter()
        .filter(|u| u.explicit_target && !u.held)
        .map(|u| format!("{}/{}", u.candidate_repo, u.name))
        .collect();
    if targets.is_empty() {
        return HashMap::new();
    }
    match Command::new("pacman")
        .args(["-Sp", "-dd", "--dbpath", db, "--logfile", "/dev/null", "--print-format", BUILD_FORMAT])
        .args(&targets)
        .stdin(Stdio::null())
        .output()
    {
        Ok(out) if out.status.success() => parse_candidate_info(&String::from_utf8_lossy(&out.stdout)),
        Ok(out) => {
            eprintln!(
                "minka-syu: pacman -Sp for later-repo builds failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("")
            );
            HashMap::new()
        }
        Err(e) => {
            eprintln!("minka-syu: pacman -Sp for later-repo builds failed: {e}");
            HashMap::new()
        }
    }
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
    let conf = read_conf(&installed);
    let sysupgrade = sysupgrade_targets(db_str, true)
        .map_err(|e| eprintln!("minka-syu: sysupgrade check failed, falling back to -Qu alone: {e}"))
        .ok();
    // Only with replacements does it matter what dependencies pull back
    // in (a replaced package installed again), so only then pay for it.
    let has_replacements = sysupgrade.as_deref().is_some_and(|t| {
        !attribute_replacements(t, &installed, &sync_entries, &conf, &mut system_vercmp).is_empty()
    });
    let final_targets = if has_replacements {
        sysupgrade_targets(db_str, false)
            .map_err(|e| eprintln!("minka-syu: full sysupgrade check failed, using the -dd one: {e}"))
            .ok()
    } else {
        None
    };
    let inputs = Inputs {
        repo_lines: &repo_lines,
        aur_lines: &aur_lines,
        installed: &installed,
        sync_entries: &sync_entries,
        sysupgrade: sysupgrade.as_deref(),
        final_targets: final_targets.as_deref(),
        conf: &conf,
    };
    let mut updates = merge_updates(&inputs);
    let info = explicit_candidate_info(db_str, &updates);
    annotate_explicit(&mut updates, &info, &inputs, &mut system_vercmp);
    let repo_change_count = updates.iter().filter(|u| u.pending() && u.transition == Transition::Changed).count();

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

    fn pkg(version: &str, repo: Option<&str>) -> alpm::InstalledPkg {
        alpm::InstalledPkg {
            version: version.into(),
            repo: repo.map(String::from),
            description: String::new(),
            size_bytes: 0,
            explicit: true,
        }
    }

    fn installed_map() -> HashMap<String, alpm::InstalledPkg> {
        HashMap::from([
            ("bcachefs-dkms".to_string(), pkg("3:1.39.7-1.1", Some("cachyos-extra-v4"))),
            ("gitkraken".to_string(), pkg("12.6.1-1", None)),
        ])
    }

    /// The merge, with `sup` as `pacman -Sup -dd` output (None: it failed).
    fn merge(
        repo: &str,
        aur: &str,
        installed: &HashMap<String, alpm::InstalledPkg>,
        sync: &[alpm::SyncEntry],
        sup: Option<&str>,
        conf: &Conf,
    ) -> Vec<Update> {
        merge_final(repo, aur, installed, sync, sup, None, conf)
    }

    /// The same with `final_sup` as the run without -dd.
    fn merge_final(
        repo: &str,
        aur: &str,
        installed: &HashMap<String, alpm::InstalledPkg>,
        sync: &[alpm::SyncEntry],
        sup: Option<&str>,
        final_sup: Option<&str>,
        conf: &Conf,
    ) -> Vec<Update> {
        let targets = sup.map(parse_sysupgrade);
        let finals = final_sup.map(parse_sysupgrade);
        let inputs = Inputs {
            repo_lines: repo,
            aur_lines: aur,
            installed,
            sync_entries: sync,
            sysupgrade: targets.as_deref(),
            final_targets: finals.as_deref(),
            conf,
        };
        merge_updates_with_cmp(&inputs, &mut system_vercmp)
    }

    fn usage(upgrade: &[&str], install: &[&str]) -> Conf {
        Conf {
            upgrade_repos: Some(upgrade.iter().map(|s| s.to_string()).collect()),
            install_repos: Some(install.iter().map(|s| s.to_string()).collect()),
            ..Conf::default()
        }
    }

    fn names(ups: &[Update]) -> Vec<&str> {
        ups.iter().map(|u| u.name.as_str()).collect()
    }

    #[test]
    fn classifies_transitions() {
        let sync = alpm::parse_sync_list(
            "cachyos-extra-v4 bcachefs-dkms 3:1.39.7-1.1\nminka bcachefs-dkms 3:1.39.7-2\n",
        );
        let ups = merge(
            "bcachefs-dkms 3:1.39.7-1.1 -> 3:1.39.7-2\n",
            "gitkraken 12.6.1-1 -> 12.6.2-1\n",
            &installed_map(),
            &sync,
            None,
            &Conf::default(),
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
        for sup in [None, Some("")] {
            let ups = merge("", "", &installed_map(), &sync, sup, &Conf::default());
            assert_eq!(ups.len(), 1);
            let u = &ups[0];
            assert_eq!(u.name, "bcachefs-dkms");
            assert_eq!(u.old_version, "3:1.39.7-1.1");
            assert_eq!(u.new_version, "3:1.39.7-2");
            assert_eq!(u.candidate_repo, "minka");
            assert_eq!(u.transition, Transition::Changed);
            assert!(u.explicit_target);
            assert!(!u.held);
            assert_eq!(u.first_repo.as_deref(), Some("cachyos-extra-v4"));
            assert_eq!(u.first_version.as_deref(), Some("3:1.39.7-1.1"));
        }
    }

    #[test]
    fn promotes_lower_repo_over_standard_candidate_only_when_strictly_newer() {
        let sync = alpm::parse_sync_list(
            "cachyos-extra-v4 bcachefs-dkms 3:1.39.7-1.2\n\
             minka bcachefs-dkms 3:1.39.7-2\n",
        );
        let ups = merge("bcachefs-dkms 3:1.39.7-1.1 -> 3:1.39.7-1.2\n", "", &installed_map(), &sync, None, &Conf::default());
        assert_eq!(ups.len(), 1);
        assert_eq!(ups[0].new_version, "3:1.39.7-2");
        assert_eq!(ups[0].candidate_repo, "minka");
        assert!(ups[0].explicit_target);
        assert_eq!(ups[0].first_repo.as_deref(), Some("cachyos-extra-v4"));
        assert_eq!(ups[0].first_version.as_deref(), Some("3:1.39.7-1.2"));

        // Equal versions: the higher-ranked repository keeps the win.
        let tie = alpm::parse_sync_list(
            "cachyos-extra-v4 bcachefs-dkms 3:1.39.7-2\n\
             minka bcachefs-dkms 3:1.39.7-2\n",
        );
        let ups = merge("bcachefs-dkms 3:1.39.7-1.1 -> 3:1.39.7-2\n", "", &installed_map(), &tie, None, &Conf::default());
        assert_eq!(ups.len(), 1);
        assert_eq!(ups[0].candidate_repo, "cachyos-extra-v4");
        assert!(!ups[0].explicit_target);
    }

    #[test]
    fn withheld_first_repo_upgrade_is_not_routed_around() {
        // No baseline line despite the first repo being newer than
        // installed: pacman withheld it, so the even-newer lower copy
        // must not surface either.
        let sync = alpm::parse_sync_list(
            "cachyos-extra-v4 bcachefs-dkms 3:1.39.7-1.2\n\
             minka bcachefs-dkms 3:1.39.7-2\n",
        );
        let ups = merge("", "", &installed_map(), &sync, None, &Conf::default());
        assert!(ups.is_empty());
    }

    #[test]
    fn update_line_parsing() {
        assert_eq!(
            parse_update_line("linux 6.1-1 -> 6.2-1"),
            Some(UpdateLine { name: "linux".into(), old: "6.1-1".into(), new: "6.2-1".into(), held: false })
        );
        assert_eq!(parse_update_line(":: some notice"), None);
        assert_eq!(parse_update_line(""), None);
        // -Qu lists IgnorePkg'd packages too, marked; -Syu skips them.
        assert!(parse_update_line("linux 6.1-1 -> 6.2-1 [ignored]").unwrap().held);
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
    fn glob_matching_is_fnmatch() {
        assert!(glob_match("linux-cachyos*", "linux-cachyos"));
        assert!(glob_match("linux-cachyos*", "linux-cachyos-headers"));
        assert!(!glob_match("linux-cachyos*", "linux"));
        assert!(!glob_match("nvidia-utils", "lib32-nvidia-utils"));
        assert!(glob_match("lib?2-*", "lib32-glibc"));
        assert!(glob_match("[!a]x", "bx"));
        assert!(glob_match("[a-c]1", "b1"));
        assert!(glob_match("a\\*", "a*"));
        assert!(!glob_match("a\\*", "ab"));
        // POSIX classes, which a hand-written matcher got wrong.
        assert!(glob_match("ol[[:digit:]]", "ol5"));
        assert!(glob_match("qt[[:digit:]]-*", "qt6-base"));
        assert!(!glob_match("a\0", "a"));
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
    fn coreutils_replacement_is_surfaced_from_the_lower_repo() {
        let ups = merge(
            "",
            "",
            &coreutils_installed(),
            &coreutils_sync(),
            Some("coreutils-uutils|0.12.0-1|minka|coreutils-hybrid\n"),
            &Conf::default(),
        );
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
        assert!(u.pending());
        assert_eq!(
            u.replaces,
            vec![Replaced { name: "coreutils-hybrid".into(), version: "9.4_0.0.28-1".into(), installed_repo: None }]
        );
    }

    #[test]
    fn installed_replacer_folds_and_replaced_rows_stay_marked() {
        // The replacer already installed from [cachyos] with a -Qu literal
        // update there; the replaced package has a row of its own (AUR).
        let sync = coreutils_sync();
        let mut installed = coreutils_installed();
        installed.insert("coreutils-uutils".into(), pkg("0.2.1-1", Some("cachyos")));
        let ups = merge(
            "coreutils-uutils 0.2.1-1 -> 0.2.2-3\n",
            "coreutils-hybrid 9.4_0.0.28-1 -> 9.5-1\n",
            &installed,
            &sync,
            Some("coreutils-uutils|0.12.0-1|minka|coreutils-hybrid\n"),
            &Conf::default(),
        );
        assert_eq!(names(&ups), ["coreutils-hybrid", "coreutils-uutils"]);
        let hybrid = &ups[0];
        assert_eq!(hybrid.superseded_by, ["coreutils-uutils"]);
        assert!(!hybrid.pending());
        let u = &ups[1];
        assert_eq!(u.old_version, "0.2.1-1");
        assert_eq!(u.new_version, "0.12.0-1");
        assert_eq!(u.candidate_repo, "minka");
        assert_eq!(u.transition, Transition::Changed);
        assert!(!u.explicit_target);
        assert_eq!(u.replaces.len(), 1);
        let state = State { schema: 1, checked_at: 0, updates: ups, repo_change_count: 0, last_notified_count: 0 };
        assert_eq!(state.total(), 1);
    }

    #[test]
    fn versioned_replaces_must_hold_for_the_installed_version() {
        // KDE5-era entries like kwallet5 'kwallet<5.111' name installed
        // KF6 packages they no longer apply to.
        let sup = parse_sysupgrade("kwallet5|5.116.0-6|extra|kwallet<5.111 kwallet-secrets\n");
        let kf6 = HashMap::from([("kwallet".to_string(), pkg("6.31.0-1.1", Some("extra")))]);
        let sync = alpm::parse_sync_list("extra kwallet5 5.116.0-6\nextra kwallet 6.31.0-1.1\n");
        assert!(attribute_replacements(&sup, &kf6, &sync, &Conf::default(), &mut system_vercmp).is_empty());
        let kf5 = HashMap::from([("kwallet".to_string(), pkg("5.110.0-1", Some("extra")))]);
        let sync = alpm::parse_sync_list("extra kwallet5 5.116.0-6\n");
        let reps = attribute_replacements(&sup, &kf5, &sync, &Conf::default(), &mut system_vercmp);
        assert_eq!(reps.len(), 1);
        assert_eq!(reps[0].replaced[0].name, "kwallet");
    }

    #[test]
    fn upgraded_ignored_or_shadowed_packages_are_not_replaced() {
        let installed = HashMap::from([
            ("xx".to_string(), pkg("1-1", Some("lo"))),
            ("yy".to_string(), pkg("1-1", Some("hi"))),
        ]);
        let none = |sync: &str, sup: &str, conf: &Conf| {
            let sync = alpm::parse_sync_list(sync);
            attribute_replacements(&parse_sysupgrade(sup), &installed, &sync, conf, &mut system_vercmp).is_empty()
        };
        // yy has a literal upgrade in the higher repo: upgraded, not replaced.
        assert!(none("hi yy 2-1\nlo xx 2-1\n", "xx|2-1|lo|yy\nyy|2-1|hi|\n", &Conf::default()));
        // yy held back by IgnorePkg: libalpm skips the replacement.
        let held = Conf { ignored: HashSet::from(["yy".to_string()]), ..Conf::default() };
        assert!(none("hi yy 2-1\nlo xx 2-1\n", "xx|2-1|lo|yy\n", &held));
        // A higher repo carries yy by name, even at the installed
        // version: libalpm stops there and never sees lo's replacer.
        assert!(none("hi yy 1-1\nlo xx 2-1\n", "xx|2-1|lo|yy\n", &Conf::default()));
        // In the replacer's own repo the replacer comes first.
        assert!(!none("lo xx 2-1\nlo yy 1-1\n", "xx|2-1|lo|yy\n", &Conf::default()));
    }

    #[test]
    fn repos_without_upgrade_usage_do_not_shadow() {
        // [hi] is Usage = Sync Search: -Su never looks at it.
        let installed = HashMap::from([("zz".to_string(), pkg("1-1", Some("hi")))]);
        let sync = alpm::parse_sync_list("hi zz 2-1\nlo aa 1-1\n");
        let ups = merge("zz 1-1 -> 2-1 [ignored]\n", "", &installed, &sync, Some("aa|1-1|lo|zz\n"), &usage(&["lo"], &["lo"]));
        assert_eq!(names(&ups), ["aa", "zz"], "{ups:#?}");
        assert_eq!(ups[0].replaces[0].name, "zz");
        assert_eq!(ups[1].superseded_by, ["aa"]);
        assert!(!ups[1].held);
    }

    #[test]
    fn ignored_lines_follow_what_su_actually_does() {
        let installed = HashMap::from([("zz".to_string(), pkg("1-1", Some("hi")))]);
        let sync = alpm::parse_sync_list("hi zz 2-1\nlo zz 3-1\n");
        let conf = usage(&["lo"], &["lo"]);
        // -Qu shows hi's 2-1 as [ignored]; -Su upgrades to lo's 3-1.
        let ups = merge("zz 1-1 -> 2-1 [ignored]\n", "", &installed, &sync, Some("zz|3-1|lo|\n"), &conf);
        assert_eq!(ups.len(), 1);
        assert!(!ups[0].held);
        assert_eq!(ups[0].new_version, "3-1");
        assert_eq!(ups[0].candidate_repo, "lo");
        assert!(!ups[0].explicit_target);
        // Without -Sup to ask, the line is held, for the usage reason.
        let ups = merge("zz 1-1 -> 2-1 [ignored]\n", "", &installed, &sync, None, &conf);
        assert!(ups[0].held);
        assert_eq!(ups[0].held_by.as_deref(), Some("usage"));
        // -Qu missed it entirely (first repo not newer), -Su upgrades it.
        let sync = alpm::parse_sync_list("hi zz 1-1\nlo zz 3-1\n");
        let ups = merge("", "", &installed, &sync, Some("zz|3-1|lo|\n"), &conf);
        assert_eq!(ups.len(), 1);
        assert!(ups[0].pending());
        assert_eq!(ups[0].candidate_repo, "lo");
    }

    #[test]
    fn packages_su_skips_are_held() {
        // IgnoreGroup on the new build: -Qu does not mark it, -Su skips it.
        let installed = HashMap::from([("foo".to_string(), pkg("1-1", Some("extra")))]);
        let sync = alpm::parse_sync_list("extra foo 2-1\n");
        let ups = merge("foo 1-1 -> 2-1\n", "", &installed, &sync, Some(""), &Conf::default());
        assert!(ups[0].held);
        assert_eq!(ups[0].held_by.as_deref(), Some("ignore"));
        // IgnorePkg: -Qu marks it [ignored] and IgnorePkg names it.
        let conf = Conf { ignored: HashSet::from(["foo".to_string()]), ..Conf::default() };
        let ups = merge("foo 1-1 -> 2-1 [ignored]\n", "", &installed, &sync, Some(""), &conf);
        assert_eq!(ups[0].held_by.as_deref(), Some("ignore"));
        let state = State { schema: 1, checked_at: 0, updates: ups, repo_change_count: 0, last_notified_count: 0 };
        assert_eq!((state.total(), state.held()), (0, 1));
    }

    #[test]
    fn held_updates_are_listed_but_never_promoted() {
        let sync = alpm::parse_sync_list(
            "cachyos-extra-v4 bcachefs-dkms 3:1.39.7-1.2\n\
             minka bcachefs-dkms 3:1.39.7-2\n",
        );
        let conf = Conf { ignored: HashSet::from(["bcachefs-dkms".to_string()]), ..Conf::default() };
        let ups = merge("bcachefs-dkms 3:1.39.7-1.1 -> 3:1.39.7-1.2 [ignored]\n", "", &installed_map(), &sync, Some(""), &conf);
        assert_eq!(ups.len(), 1);
        assert!(ups[0].held);
        assert_eq!(ups[0].candidate_repo, "cachyos-extra-v4");
        assert!(!ups[0].explicit_target);
        // The sqlite case: the first repo matches the installed version,
        // a lower repo is newer, IgnorePkg holds it: shown as held.
        let sync = alpm::parse_sync_list(
            "cachyos-extra-v4 bcachefs-dkms 3:1.39.7-1.1\n\
             minka bcachefs-dkms 3:1.39.7-2\n",
        );
        let ups = merge("", "", &installed_map(), &sync, Some(""), &conf);
        assert_eq!(ups.len(), 1);
        assert!(ups[0].held && ups[0].explicit_target);
    }

    #[test]
    fn replacement_chains_and_several_replacers() {
        // aa replaces bb, and bb's new build replaces cc (x3).
        let installed = HashMap::from([
            ("bb".to_string(), pkg("1-1", Some("x"))),
            ("cc".to_string(), pkg("1-1", Some("x"))),
        ]);
        let sync = alpm::parse_sync_list("x aa 1-1\nx bb 2-1\n");
        let ups = merge("bb 1-1 -> 2-1\n", "", &installed, &sync, Some("aa|1-1|x|bb\nbb|2-1|x|cc\n"), &Conf::default());
        assert_eq!(names(&ups), ["aa", "bb"], "{ups:#?}");
        // bb is installed again (2-1), so aa is a new package, not bb's end.
        assert!(ups[0].new_install && ups[0].replaces.is_empty());
        // ...whose replacement still hangs on bb: ignoring bb cancels it.
        assert_eq!(ups[0].reinstalls, ["bb"]);
        assert_eq!(ups[1].replaces[0].name, "cc");
        assert!(ups[1].superseded_by.is_empty());
        // A higher repo's replacer of zz2 is found first; the lower one
        // removes only what it is first for (m3).
        let installed = HashMap::from([
            ("aa1".to_string(), pkg("1-1", None)),
            ("zz2".to_string(), pkg("1-1", None)),
        ]);
        let sync = alpm::parse_sync_list("hi uu 1-1\nlo rr 1-1\n");
        let reps = attribute_replacements(
            &parse_sysupgrade("uu|1-1|hi|zz2\nrr|1-1|lo|aa1 zz2\n"),
            &installed,
            &sync,
            &Conf::default(),
            &mut system_vercmp,
        );
        let by: Vec<(&str, Vec<&str>)> =
            reps.iter().map(|r| (r.name.as_str(), r.replaced.iter().map(|p| p.name.as_str()).collect())).collect();
        assert_eq!(by, [("rr", vec!["aa1"]), ("uu", vec!["zz2"])]);
    }

    #[test]
    fn explicit_rows_learn_from_their_build() {
        let installed = HashMap::from([
            ("aa-new".to_string(), pkg("0.2.1-1", Some("hi"))),
            ("zz-old".to_string(), pkg("1-1", None)),
            ("plasmoid".to_string(), pkg("1-1", Some("hi"))),
        ]);
        let sync = alpm::parse_sync_list(
            "hi aa-new 0.2.1-1\nhi plasmoid 1-1\nlo aa-new 0.12.0-1\nlo plasmoid 2-1\n",
        );
        let conf = Conf { group_patterns: vec!["plasma*".into()], ..Conf::default() };
        let inputs = Inputs {
            repo_lines: "",
            aur_lines: "",
            installed: &installed,
            sync_entries: &sync,
            sysupgrade: Some(&[]),
            final_targets: None,
            conf: &conf,
        };
        let mut ups = merge_updates_with_cmp(&inputs, &mut system_vercmp);
        assert_eq!(names(&ups), ["aa-new", "plasmoid"]);
        assert!(ups.iter().all(|u| u.explicit_target && !u.held));
        let info = parse_candidate_info("aa-new|lo||zz-old\nplasmoid|lo|plasma kde|\n");
        annotate_explicit(&mut ups, &info, &inputs, &mut system_vercmp);
        assert_eq!(ups[0].replaces[0].name, "zz-old");
        assert!(ups[1].held);
        assert_eq!(ups[1].held_by.as_deref(), Some("ignore"));
    }

    #[test]
    fn candidate_info_parsing() {
        let info = parse_candidate_info("coreutils-uutils|minka||coreutils-hybrid old<2\nplasmoid|lo|plasma kde|\n");
        let ci = &info[&("minka".to_string(), "coreutils-uutils".to_string())];
        assert!(ci.groups.is_empty());
        assert_eq!(ci.replaces.len(), 2);
        assert_eq!(ci.replaces[1].constraint, Some(("<".into(), "2".into())));
        assert_eq!(info[&("lo".to_string(), "plasmoid".to_string())].groups, ["plasma", "kde"]);
    }

    #[test]
    fn a_package_replacing_itself_is_not_an_update() {
        // python-pylint-plugin-utils lists itself in %REPLACES%; libalpm
        // has no self-check, so -Sup lists it at the installed version.
        let installed = HashMap::from([("foo".to_string(), pkg("1-1", Some("extra")))]);
        let sync = alpm::parse_sync_list("extra foo 1-1\n");
        let ups = merge("", "", &installed, &sync, Some("foo|1-1|extra|foo foo-git\n"), &Conf::default());
        assert!(ups.is_empty(), "{ups:#?}");
    }

    #[test]
    fn a_replaced_package_installed_again_is_an_upgrade() {
        // libalpm's append path (n1b): rr is added for aa, yy is appended
        // to rr's removes, and yy's own newer build is still added.
        let installed = HashMap::from([
            ("aa".to_string(), pkg("1-1", Some("hi"))),
            ("yy".to_string(), pkg("1-1", Some("hi"))),
        ]);
        let sync = alpm::parse_sync_list("hi rr 1-1\nhi yy 2-1\n");
        let ups = merge("yy 1-1 -> 2-1\n", "", &installed, &sync, Some("rr|1-1|hi|aa yy\nyy|2-1|hi|\n"), &Conf::default());
        assert_eq!(names(&ups), ["rr", "yy"], "{ups:#?}");
        // yy is installed again, so rr's row lists only what is gone.
        assert_eq!(ups[0].replaces.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["aa"]);
        assert!(ups[1].pending());
        assert_eq!(ups[1].new_version, "2-1");

        // An Arch split (r2): foo-gtk replaces foo<2 and depends on foo,
        // so the run without -dd pulls foo 2-1 back in. foo-gtk's
        // conflicts=('foo<2') does not hit foo 2-1: foo is upgraded.
        let installed = HashMap::from([("foo".to_string(), pkg("1-1", Some("hi")))]);
        let sync = alpm::parse_sync_list("hi foo 2-1\nhi foo-gtk 2-1\n");
        let split = |gtk: &str| {
            merge_final(
                "foo 1-1 -> 2-1\n",
                "",
                &installed,
                &sync,
                Some(&format!("{gtk}\n")),
                Some(&format!("{gtk}\nfoo|2-1|hi|||\n")),
                &Conf::default(),
            )
        };
        let ups = split("foo-gtk|2-1|hi|foo<2|foo<2|");
        assert_eq!(names(&ups), ["foo", "foo-gtk"], "{ups:#?}");
        assert!(ups[0].pending() && !ups[0].held);
        // foo-gtk removes nothing for good: a new package next to foo.
        assert!(ups[1].new_install && ups[1].replaces.is_empty() && ups[1].old_version.is_empty());
        // A rename (n1c): the replacer conflicts with and provides the old
        // name, so libalpm's inner-conflict step drops foo's new build.
        let ups = split("foo-gtk|2-1|hi|foo<2|foo|foo");
        assert_eq!(ups[0].superseded_by, ["foo-gtk"]);
        assert!(!ups[0].pending());
        assert_eq!(ups[1].replaces[0].name, "foo");
        assert!(!ups[1].new_install);
    }

    fn explicit_inputs_case(
        installed: &HashMap<String, alpm::InstalledPkg>,
        sync: &[alpm::SyncEntry],
        sup: &[SupTarget],
        conf: &Conf,
        info: &str,
    ) -> Vec<Update> {
        let inputs = Inputs {
            repo_lines: "",
            aur_lines: "",
            installed,
            sync_entries: sync,
            sysupgrade: Some(sup),
            final_targets: None,
            conf,
        };
        let mut ups = merge_updates_with_cmp(&inputs, &mut system_vercmp);
        annotate_explicit(&mut ups, &parse_candidate_info(info), &inputs, &mut system_vercmp);
        ups
    }

    #[test]
    fn explicit_replaces_follow_the_walk() {
        // n6: hi/xx's 'zz<1' does not match zz 2-1, so it is not first.
        let installed = HashMap::from([
            ("aa".to_string(), pkg("1-1", Some("hi"))),
            ("xx".to_string(), pkg("1-1", Some("hi"))),
            ("zz".to_string(), pkg("2-1", None)),
        ]);
        let sync = alpm::parse_sync_list("hi aa 1-1\nhi xx 2-1\nlo aa 3-1\n");
        let sup = parse_sysupgrade("xx|2-1|hi|zz<1\n");
        let ups = explicit_inputs_case(&installed, &sync, &sup, &Conf::default(), "aa|lo||zz\n");
        let aa = ups.iter().find(|u| u.name == "aa").unwrap();
        assert!(aa.explicit_target);
        assert_eq!(aa.replaces.len(), 1, "{ups:#?}");

        // n7: a build from a repo without Usage = Upgrade replaces nothing.
        let conf = usage(&["hi"], &["hi", "lo"]);
        let ups = explicit_inputs_case(&installed, &sync, &sup, &conf, "aa|lo||zz\n");
        assert!(ups.iter().find(|u| u.name == "aa").unwrap().replaces.is_empty());

        // n8: zz has an explicit row of its own, so it is never removed.
        let installed = HashMap::from([
            ("aa".to_string(), pkg("1-1", Some("hi"))),
            ("zz".to_string(), pkg("1-1", Some("lo"))),
        ]);
        let sync = alpm::parse_sync_list("hi aa 1-1\nhi zz 1-1\nlo aa 3-1\nlo2 zz 2-1\n");
        let ups = explicit_inputs_case(&installed, &sync, &[], &Conf::default(), "aa|lo||zz\nzz|lo2||\n");
        assert_eq!(names(&ups), ["aa", "zz"]);
        assert!(ups[0].replaces.is_empty(), "{ups:#?}");
    }

    #[test]
    fn ignoregroup_on_a_later_build_falls_back_to_the_standard_upgrade() {
        // hi/foo 2-1 has no group, lo/foo 3-1 is in plasma; IgnoreGroup =
        // plasma*. -Syu still upgrades to hi's 2-1, so the row offers that.
        let installed = HashMap::from([("foo".to_string(), pkg("1-1", Some("hi")))]);
        let sync = alpm::parse_sync_list("hi foo 2-1\nlo foo 3-1\n");
        let conf = Conf { group_patterns: vec!["plasma*".into()], ..Conf::default() };
        let inputs = Inputs {
            repo_lines: "foo 1-1 -> 2-1\n",
            aur_lines: "",
            installed: &installed,
            sync_entries: &sync,
            sysupgrade: Some(&parse_sysupgrade("foo|2-1|hi|\n")),
            final_targets: None,
            conf: &conf,
        };
        let mut ups = merge_updates_with_cmp(&inputs, &mut system_vercmp);
        assert!(ups[0].explicit_target);
        annotate_explicit(&mut ups, &parse_candidate_info("foo|lo|plasma|\n"), &inputs, &mut system_vercmp);
        let u = &ups[0];
        assert!(!u.held && !u.explicit_target, "{u:#?}");
        assert_eq!((u.candidate_repo.as_str(), u.new_version.as_str()), ("hi", "2-1"));
        assert_eq!(u.first_repo, None);
    }

    #[test]
    fn sysupgrade_parsing_skips_junk() {
        let t = parse_sysupgrade("nautilus|51.1-1.1|cachyos-extra-v4|\nbroken line\n|1|x|y\na|1|r|b c<2\n");
        assert_eq!(t.len(), 2);
        assert!(t[0].replaces.is_empty());
        assert_eq!(t[1].replaces.len(), 2);
    }

    #[test]
    fn state_from_an_older_core_still_loads() {
        // A state.json written by MinkaSyu 0.5.0 has none of the new keys.
        let json = r#"{"schema":1,"checked_at":0,"repo_change_count":0,"last_notified_count":0,
            "updates":[{"name":"a","old_version":"1","new_version":"2","installed_repo":null,
            "candidate_repo":"extra","transition":"unknown_source"}]}"#;
        let state: State = serde_json::from_str(json).unwrap();
        let u = &state.updates[0];
        assert!(u.replaces.is_empty() && !u.held && u.held_by.is_none() && u.superseded_by.is_empty());
        assert_eq!(state.total(), 1);
    }
}
