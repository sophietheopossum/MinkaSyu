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
    /// True when a plain `-Syu` would not install this candidate at all, so
    /// applying it requires an explicit `repo/name` target: a strictly
    /// newer version in a lower-ranked repository for a package whose
    /// first repository's copy is not newer than the installed one (a
    /// later-repo row), a switch, or the origin group.
    #[serde(default)]
    pub explicit_target: bool,
    /// A later-repo row's first repository and its copy's version (not
    /// newer than the installed package, or -Syu would upgrade it).
    #[serde(default)]
    pub first_repo: Option<String>,
    #[serde(default)]
    pub first_version: Option<String>,
    /// On a row that is pacman's own upgrade: a strictly newer build in a
    /// lower-ranked repository, noted and not taken. The row stays that
    /// upgrade, ticked by default, since an unticked row is `--ignore`,
    /// which skips pacman's upgrade as well (on 23/11/2026 nvidia-settings
    /// stayed at 615.78.08-1 that way: ALHP's 620.73.04-1.1 is newer than
    /// cachyos-v4's 620.73.04-1 only by its rebuild suffix, and the row
    /// offered it unticked). The window's detail view still offers this
    /// build, as a `repo/name` target. Left out of the JSON when None, so
    /// state.json reads as before in front ends that do not know it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newer_repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newer_version: Option<String>,
    /// What that newer build would replace, attributed as an explicit
    /// row's `replaces` are (annotate_explicit): chosen in the window, it
    /// is a `repo/name` target like one, and pacman removes these. Kept
    /// apart from `replaces`, which is what the row's own build does. Left
    /// out of the JSON when empty, like `newer_repo`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub newer_replaces: Vec<Replaced>,
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
    /// A switch: nothing newer anywhere, but no repository carries the
    /// installed build, while `candidate_repo` carries the same version
    /// as a build of its own (the installed one came from the AUR or a
    /// file, or from a repository that dropped or rebuilt it). Also an
    /// `explicit_target`, with no `first_repo`: -Syu never touches the
    /// package, and the `repo/name` target makes pacman reinstall it
    /// from there. Unticked, it is simply not targeted: never --ignore.
    #[serde(default)]
    pub switch_build: bool,
    /// Not empty only on the origin group's row (named ORIGIN_GROUP): the
    /// installed packages with no %INSTALLED_DB% whose very build a
    /// repository carries, each reinstalled from that repository as a
    /// `repo/name` target, which records it. One row, so they count as one
    /// update (State::total, the notification, MinkaShell's pill), however
    /// many there are. Its `candidate_repo` lists their repositories and is
    /// no target itself: explicit_targets expands the row.
    #[serde(default)]
    pub origin_group: Vec<OriginMember>,
}

/// The origin group's row name. Not a package name (they have no spaces),
/// so it never meets a real package; never passed to paru either.
pub const ORIGIN_GROUP: &str = "(no recorded repository)";

/// One package of the origin group.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OriginMember {
    pub name: String,
    pub version: String,
    /// The repository whose copy is the installed build (the first in
    /// pacman.conf order, when several are): its reinstall target.
    pub repo: String,
    pub explicit: bool,
}

impl OriginMember {
    pub fn target(&self) -> String {
        format!("{}/{}", self.repo, self.name)
    }
}

impl Update {
    /// Whether a plain run applies this row as listed: not held, and not
    /// standing for a package that a replacement removes.
    pub fn pending(&self) -> bool {
        !self.held && self.superseded_by.is_empty()
    }

    /// Whether this is the origin group's row.
    pub fn is_origin_group(&self) -> bool {
        !self.origin_group.is_empty()
    }

    /// What a switch row does, in words.
    pub fn switch_note(&self) -> String {
        if self.installed_repo.as_deref() == Some(self.candidate_repo.as_str()) {
            format!("reinstall {}'s current build", self.candidate_repo)
        } else {
            format!("switch to {}'s build", self.candidate_repo)
        }
    }

    /// The newer build a row notes and does not take, in words, with what
    /// it would replace: choosing it removes those.
    pub fn newer_note(&self) -> Option<String> {
        let note = format!("newer build in {}: {}", self.newer_repo.as_ref()?, self.newer_version.as_ref()?);
        if self.newer_replaces.is_empty() {
            return Some(note);
        }
        let replaced: Vec<String> = self.newer_replaces.iter().map(|r| format!("{} {}", r.name, r.version)).collect();
        Some(format!("{note}, which replaces {}", replaced.join(", ")))
    }
}

/// One installed package that a pending update replaces.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Replaced {
    pub name: String,
    pub version: String,
    pub installed_repo: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub schema: u32,
    /// Unix seconds of the last successful check.
    pub checked_at: i64,
    pub updates: Vec<Update>,
    pub repo_change_count: usize,
    /// Total update count at last notification; notification fires only
    /// when the count grows past this.
    pub last_notified_count: usize,
    /// Installed packages nothing tracks: no repository has their build,
    /// and neither does the AUR, or paru never asks it (see
    /// untracked_candidates). An error to fix, not an update: never counted.
    #[serde(default)]
    pub untracked: Vec<Untracked>,
    /// Set when the AUR could not be asked which candidates it has: then no
    /// package in no repository is flagged (the AUR's would be), and this
    /// says so.
    #[serde(default)]
    pub aur_unreachable: Option<String>,
}

/// An installed package nothing tracks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Untracked {
    pub name: String,
    pub version: String,
    pub explicit: bool,
    /// The installed packages that depend on it (alpm::required_by).
    pub required_by: Vec<String>,
    /// The installed packages that optionally depend on it
    /// (alpm::optional_for).
    #[serde(default)]
    pub optional_for: Vec<String>,
    /// %INSTALLED_DB%, when one was recorded (that repository dropped it).
    pub installed_repo: Option<String>,
    /// "no_source": in no repository and not in the AUR. "other_build":
    /// repositories have the name, but none this version, no repository
    /// was recorded, and it was installed from a file with no signature
    /// (%VALIDATION% none).
    pub kind: String,
    /// The repositories' versions of that name, "repo version", for
    /// "other_build".
    pub repo_versions: Vec<String>,
    /// The AUR's version of that name, for "other_build": paru never asks
    /// the AUR about a name a repository has, so it tracks nothing.
    #[serde(default)]
    pub aur_version: Option<String>,
    /// What to do about it, in one line.
    pub hint: String,
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

/// Whether a package (its name, version and provides) satisfies a
/// depstring, as libalpm's _alpm_depcmp decides it: by name, its version
/// checked against the constraint, or by a provide of that name, whose
/// version must then be an `=` one that satisfies it (an unversioned
/// provide only satisfies an unversioned depstring). The same rule serves
/// dependencies, optional ones and conflicts.
pub(crate) fn dep_matches(
    dep: &DepSpec,
    name: &str,
    version: &str,
    provides: &[DepSpec],
    cmp: &mut dyn FnMut(&str, &str) -> Ordering,
) -> bool {
    (dep.name == name && satisfies(version, &dep.constraint, cmp))
        || provides.iter().any(|p| {
            p.name == dep.name
                && match (&dep.constraint, &p.constraint) {
                    (None, _) => true,
                    (Some(_), Some((op, v))) if op == "=" => satisfies(v, &dep.constraint, cmp),
                    (Some(_), _) => false,
                }
        })
}

/// system_vercmp, each pair of versions asked only once.
pub(crate) fn memo_vercmp() -> impl FnMut(&str, &str) -> Ordering {
    let mut cache: HashMap<(String, String), Ordering> = HashMap::new();
    move |a, b| *cache.entry((a.to_string(), b.to_string())).or_insert_with(|| system_vercmp(a, b))
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
    /// Repos pacman.conf lets plain `-S` installs use (`Usage` includes
    /// Install, or All). None as above. An explicit `repo/name` target
    /// can use any repository whatever its Usage (pacman adds Install for
    /// it), so this is the user's choice of repos to offer, not a limit.
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
        self.conflicts.iter().any(|c| dep_matches(c, &other.name, &other.version, &other.provides, cmp))
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
/// looks for a strictly newer version in a lower-ranked repository it can
/// install from. Where -Su upgrades the package, that build is only noted
/// on pacman's own row (`newer_repo`); it never takes the row over, whose
/// untick would then hold back pacman's upgrade too. Where the first
/// repository's copy is not newer than the installed package, it is an
/// update pacman would never offer: a later-repo row. Held rows get
/// neither. Everything except the injected version comparison is pure.
pub fn merge_updates(inputs: &Inputs) -> Vec<Update> {
    merge_updates_with_cmp(inputs, &mut memo_vercmp())
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
    // Explicit candidates: only repos pacman.conf lets installs use.
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
        let mut newer_repo = None;
        let mut newer_version = None;

        // Note a strictly newer version in a lower-ranked repo; the row
        // stays pacman's own candidate (see Update::newer_repo). Not for a
        // held package: it is shown and left alone.
        if let Some(entries) = sync_by_name.get(name.as_str()).filter(|_| !held) {
            let mut best: Option<&alpm::SyncEntry> = None;
            for e in entries {
                let current = best.map_or(new_version.as_str(), |b| b.version.as_str());
                if cmp(&e.version, current) == Ordering::Greater {
                    best = Some(e);
                }
            }
            if let Some(best) = best.filter(|b| b.repo != candidate_repo) {
                newer_repo = Some(best.repo.clone());
                newer_version = Some(best.version.clone());
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
            explicit_target: false,
            first_repo: None,
            first_version: None,
            newer_repo,
            newer_version,
            newer_replaces: Vec::new(),
            replaces: Vec::new(),
            held,
            held_by: held_by.map(String::from),
            superseded_by: Vec::new(),
            new_install: false,
            reinstalls: Vec::new(),
            switch_build: false,
            origin_group: Vec::new(),
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
                newer_repo: None,
                newer_version: None,
                newer_replaces: Vec::new(),
                replaces: Vec::new(),
                held,
                held_by: held.then(|| "ignore".to_string()),
                superseded_by: Vec::new(),
                new_install: false,
                reinstalls: Vec::new(),
                switch_build: false,
                origin_group: Vec::new(),
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
            newer_repo: None,
            newer_version: None,
            newer_replaces: Vec::new(),
            replaces: Vec::new(),
            held,
            held_by: held.then(|| "ignore".to_string()),
            superseded_by: Vec::new(),
            new_install: false,
            reinstalls: Vec::new(),
            switch_build: false,
            origin_group: Vec::new(),
        });
    }

    // Replacements take their repo and version from -Su's own target
    // set, never from the by-name ranking above: a same-named package in
    // a higher repo without the replaces entry is not what -Syu installs.
    // Nor is a newer build noted: only the replacing build replaces.
    for r in &replacements {
        if let Some(row) = updates.iter_mut().find(|u| u.name == r.name) {
            row.new_version = r.version.clone();
            row.candidate_repo = r.repo.clone();
            row.transition = transition(&row.installed_repo, &r.repo);
            row.explicit_target = false;
            row.first_repo = None;
            row.first_version = None;
            row.newer_repo = None;
            row.newer_version = None;
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
                newer_repo: None,
                newer_version: None,
                newer_replaces: Vec::new(),
                replaces: r.replaced.clone(),
                held: false,
                held_by: None,
                superseded_by: Vec::new(),
                new_install,
                reinstalls: reinstalls.get(&r.name).cloned().unwrap_or_default(),
                switch_build: false,
                origin_group: Vec::new(),
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

/// Groups, replaces and conflicts of an explicit candidate's build, or of
/// a newer build a row notes: -Su's target set cannot speak for builds a
/// plain -Syu never selects.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CandidateInfo {
    pub groups: Vec<String>,
    pub replaces: Vec<DepSpec>,
    pub conflicts: Vec<DepSpec>,
}

/// (repo, name) -> that build (see build_repo).
pub type CandidateInfos = HashMap<(String, String), CandidateInfo>;

/// The print format of explicit-build queries: name, repo, groups,
/// replaces, conflicts. Print mode never wraps lines or translates field
/// names, which `pacman -Si` does.
const BUILD_FORMAT: &str = "%n|%r|%G|%R|%H";

/// Parse `pacman -Sp ... --print-format BUILD_FORMAT` lines.
pub fn parse_candidate_info(text: &str) -> HashMap<(String, String), CandidateInfo> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.splitn(5, '|');
            let (name, repo) = (f.next()?, f.next()?);
            if name.is_empty() || repo.is_empty() {
                return None;
            }
            let groups = f.next().unwrap_or("").split_whitespace().map(String::from).collect();
            let replaces = f.next().unwrap_or("").split_whitespace().filter_map(parse_depstring).collect();
            let conflicts = f.next().unwrap_or("").split_whitespace().filter_map(parse_depstring).collect();
            Some(((repo.to_string(), name.to_string()), CandidateInfo { groups, replaces, conflicts }))
        })
        .collect()
}

/// Finish explicit (later-repo) rows with their build's own data, and the
/// newer builds plain rows note.
///
/// IgnoreGroup on the later build: libalpm checks the build as well as
/// the installed package. An explicit row whose build it names is held:
/// -Syu does nothing else for that package (see Update::explicit_target).
/// A newer build it names is dropped from the note of its row, which is
/// still pacman's own upgrade: no row points at a build pacman.conf holds
/// back.
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
///
/// A newer build a row notes is such a target once the window chooses
/// it, so the same rules list what it replaces, in `newer_replaces`: the
/// row's own build is pacman's, whose replacements -Su's target set gives.
pub fn annotate_explicit(
    updates: &mut [Update],
    info: &HashMap<(String, String), CandidateInfo>,
    inputs: &Inputs,
    cmp: &mut dyn FnMut(&str, &str) -> Ordering,
) {
    let Inputs { installed, sync_entries, sysupgrade, final_targets, conf, .. } = *inputs;
    let ignored_build = |repo: &str, name: &str| {
        info.get(&(repo.to_string(), name.to_string())).is_some_and(|b| conf.ignores_build(name, &b.groups))
    };
    for u in updates.iter_mut().filter(|u| !u.held) {
        if u.explicit_target {
            if ignored_build(&u.candidate_repo, &u.name) {
                u.held = true;
                u.held_by = Some("ignore".to_string());
            }
        } else if u.newer_repo.as_deref().is_some_and(|repo| ignored_build(repo, &u.name)) {
            u.newer_repo = None;
            u.newer_version = None;
            u.newer_replaces.clear();
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
    for u in updates.iter_mut().filter(|u| u.pending()) {
        let Some(repo) = build_repo(u).map(str::to_string) else {
            continue;
        };
        let Some(build) = info.get(&(repo.clone(), u.name.clone())) else {
            continue;
        };
        if !conf.upgrades_from(&repo) {
            continue;
        }
        let own_rank = rank(&repo);
        let listed = if u.explicit_target { &mut u.replaces } else { &mut u.newer_replaces };
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
            if literal_above || replacer_above || listed.iter().any(|r| r.name == dep.name) {
                continue;
            }
            listed.push(Replaced {
                name: dep.name.clone(),
                version: pkg.version.clone(),
                installed_repo: pkg.repo.clone(),
            });
        }
    }
}

/// One sync repository's build of a package, from a build lookup: with
/// the version, what tells two builds of one version apart.
#[derive(Debug, Clone, PartialEq)]
pub struct SyncBuild {
    pub version: String,
    pub build_date: Option<i64>,
    pub packager: String,
}

/// (repo, name) -> that repository's build.
pub type SyncBuilds = HashMap<(String, String), SyncBuild>;

/// The print format of build lookups: name, repo, version, build date,
/// packager. pacman prints the date with strftime's %c, so the lookup
/// runs with LC_ALL=C and TZ=UTC0 for one fixed form (parse_c_date).
const SWITCH_FORMAT: &str = "%n|%r|%v|%b|%p";

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// days_from_civil).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// strftime's %c in the C locale, read as UTC ("Thu Aug 27 12:25:22
/// 2026", the day space-padded): Unix seconds, or None for anything else.
/// pacman prints the epoch ("Thu Jan  1 00:00:00 1970") for a build
/// without a date, which this reads as 0 (parse_build_date drops it), and
/// leaves a literal "%b" for a date of -1, which this refuses.
pub fn parse_c_date(text: &str) -> Option<i64> {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let f: Vec<&str> = text.split_whitespace().collect();
    let [_weekday, month, day, time, year] = f.as_slice() else {
        return None;
    };
    let month = MONTHS.iter().position(|m| m == month)? as i64 + 1;
    let day: i64 = day.parse().ok()?;
    let year: i64 = year.parse().ok()?;
    let hms: Vec<i64> = time.split(':').map(|v| v.parse().ok()).collect::<Option<_>>()?;
    let [h, m, s] = hms.as_slice() else {
        return None;
    };
    if !(1..=31).contains(&day) || !(0..24).contains(h) || !(0..60).contains(m) || !(0..=60).contains(s) {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86400 + h * 3600 + m * 60 + s)
}

/// A sync build's date as pacman prints it (%b, or -Si's "Build Date"),
/// None when unknown. The epoch is pacman's form of "no date": libalpm
/// never writes a local %BUILDDATE% of 0, so a 0 here could never match
/// an installed build anyway.
pub(crate) fn parse_build_date(text: &str) -> Option<i64> {
    parse_c_date(text).filter(|d| *d != 0)
}

/// Parse `pacman -Sp ... --print-format SWITCH_FORMAT` lines.
pub fn parse_sync_builds(text: &str) -> SyncBuilds {
    text.lines()
        .filter_map(|line| {
            let mut f = line.splitn(5, '|');
            let (name, repo, version, date, packager) = (f.next()?, f.next()?, f.next()?, f.next()?, f.next()?);
            if name.is_empty() || repo.is_empty() || version.is_empty() {
                return None;
            }
            let build = SyncBuild {
                version: version.to_string(),
                build_date: parse_build_date(date),
                packager: packager.to_string(),
            };
            Some(((repo.to_string(), name.to_string()), build))
        })
        .collect()
}

/// Whether a repository's build is the installed one: same version,
/// build date and packager (what the local DB keeps of a build).
fn is_installed_build(pkg: &alpm::InstalledPkg, build: &SyncBuild) -> bool {
    build.version == pkg.version
        && build.build_date.is_some()
        && build.build_date == pkg.build_date
        && build.packager == pkg.packager
}

/// An installed package that may be a switch: no row touches it, and some
/// repository carries its exact version.
struct SwitchSuspect<'a> {
    name: &'a str,
    pkg: &'a alpm::InstalledPkg,
    /// The repositories carrying that version, in pacman.conf order.
    copies: Vec<&'a str>,
}

fn switch_suspects<'a>(
    updates: &[Update],
    installed: &'a HashMap<String, alpm::InstalledPkg>,
    sync_entries: &'a [alpm::SyncEntry],
) -> Vec<SwitchSuspect<'a>> {
    // A package with a row of its own (an upgrade, held or not, a later
    // repo, the AUR, a replacement) or one a replacement removes or
    // installs again is the list's already, and so is one a noted newer
    // build would remove once chosen (the row says so).
    let mut touched: HashSet<&str> = HashSet::new();
    for u in updates {
        touched.insert(u.name.as_str());
        touched.extend(u.replaces.iter().map(|r| r.name.as_str()));
        touched.extend(u.newer_replaces.iter().map(|r| r.name.as_str()));
        touched.extend(u.reinstalls.iter().map(String::as_str));
    }
    let mut by_name: HashMap<&str, Vec<&alpm::SyncEntry>> = HashMap::new();
    for e in sync_entries {
        by_name.entry(e.name.as_str()).or_default().push(e);
    }
    let mut out: Vec<SwitchSuspect> = installed
        .iter()
        .filter(|(name, pkg)| !touched.contains(name.as_str()) && pkg.build_date.is_some())
        .filter_map(|(name, pkg)| {
            let copies: Vec<&str> = by_name
                .get(name.as_str())?
                .iter()
                .filter(|e| e.version == pkg.version)
                .map(|e| e.repo.as_str())
                .collect();
            (!copies.is_empty()).then_some(SwitchSuspect { name, pkg, copies })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(b.name));
    out
}

/// The next builds to look up for switch_rows, at most one per package
/// (pacman refuses a name twice in one transaction): for each suspect no
/// looked-up copy has matched yet, its next copy not yet `tried`. Every
/// copy is looked up, whatever its repository's Usage: a `repo/name`
/// lookup works in any repository, and switch_rows needs every copy's
/// build. Suspects with no copy in a repository pacman.conf lets installs
/// use are skipped, since switch_rows has no target for them. The
/// installed origin's copy goes first, as the one most likely to be the
/// installed build, so one round mostly settles everything; then
/// pacman.conf order. Empty once nothing is left to learn.
pub fn switch_lookups(
    updates: &[Update],
    inputs: &Inputs,
    builds: &SyncBuilds,
    tried: &HashSet<(String, String)>,
) -> Vec<(String, String)> {
    let Inputs { installed, sync_entries, conf, .. } = *inputs;
    let key = |repo: &str, name: &str| (repo.to_string(), name.to_string());
    switch_suspects(updates, installed, sync_entries)
        .into_iter()
        .filter(|s| s.copies.iter().any(|r| conf.installs_from(r)))
        .filter_map(|s| {
            let matched = s
                .copies
                .iter()
                .any(|r| builds.get(&key(r, s.name)).is_some_and(|b| is_installed_build(s.pkg, b)));
            if matched {
                return None;
            }
            let origin = s.pkg.repo.as_deref().filter(|o| s.copies.contains(o));
            origin
                .into_iter()
                .chain(s.copies.iter().copied().filter(|r| Some(*r) != origin))
                .map(|r| key(r, s.name))
                .find(|k| !tried.contains(k))
        })
        .collect()
}

/// Same-version switches. A row for each installed package that no row
/// touches, that some repository carries at exactly the installed
/// version, and whose build (date and packager) is none of those copies:
/// it came from the AUR or a file, or from a repository that dropped the
/// package or rebuilt it. -Syu never touches it, since nothing is newer.
/// The row offers the first repository in pacman.conf order (of those
/// pacman.conf lets installs use) with that version, as an explicit
/// `repo/name` target; pacman reinstalls the package from there, since
/// MinkaSyu never passes --needed. Every copy's build must be known: one
/// not looked up settles nothing, so no row. IgnorePkg/IgnoreGroup on the
/// installed package hold it, like every other row (IgnoreGroup on the
/// new build: annotate_explicit). A switch that something the run may
/// install conflicts with is dropped afterwards (add_build_rows).
pub fn switch_rows(updates: &[Update], inputs: &Inputs, builds: &SyncBuilds) -> Vec<Update> {
    let Inputs { installed, sync_entries, conf, .. } = *inputs;
    switch_suspects(updates, installed, sync_entries)
        .into_iter()
        .filter_map(|s| {
            for repo in &s.copies {
                let build = builds.get(&(repo.to_string(), s.name.to_string()))?;
                if build.version != s.pkg.version || build.build_date.is_none() || is_installed_build(s.pkg, build) {
                    return None;
                }
            }
            let target = *s.copies.iter().find(|r| conf.installs_from(r))?;
            let held = conf.ignored.contains(s.name);
            let installed_repo = s.pkg.repo.clone();
            Some(Update {
                transition: transition(&installed_repo, target),
                name: s.name.to_string(),
                old_version: s.pkg.version.clone(),
                new_version: s.pkg.version.clone(),
                installed_repo,
                candidate_repo: target.to_string(),
                explicit_target: true,
                first_repo: None,
                first_version: None,
                newer_repo: None,
                newer_version: None,
                newer_replaces: Vec::new(),
                replaces: Vec::new(),
                held,
                held_by: held.then(|| "ignore".to_string()),
                superseded_by: Vec::new(),
                new_install: false,
                reinstalls: Vec::new(),
                switch_build: true,
                origin_group: Vec::new(),
            })
        })
        .collect()
}

/// The repository of a row's build that -Su's target set does not speak
/// for, which CandidateInfos holds: an explicit row's own, or the newer
/// build a plain row notes.
fn build_repo(u: &Update) -> Option<&str> {
    if u.explicit_target { Some(&u.candidate_repo) } else { u.newer_repo.as_deref() }
}

/// What a run may install, each with what it conflicts with: -Su's
/// target set (the run without -dd when there was one), the builds of
/// every explicit row, ticked or not (the check cannot know which will
/// be), and the newer builds plain rows note, which the window can choose
/// for them.
pub type RunInstalls = Vec<(String, Vec<DepSpec>)>;

pub fn run_installs(inputs: &Inputs, updates: &[Update], info: &CandidateInfos) -> RunInstalls {
    let targets = inputs.final_targets.or(inputs.sysupgrade).unwrap_or_default();
    let mut out: RunInstalls = targets.iter().map(|t| (t.name.clone(), t.conflicts.clone())).collect();
    for u in updates.iter().filter(|u| u.pending()) {
        if let Some(build) = build_repo(u).and_then(|repo| info.get(&(repo.to_string(), u.name.clone()))) {
            out.push((u.name.clone(), build.conflicts.clone()));
        }
    }
    out
}

/// Whether something the run may install conflicts with the installed
/// package `name`: a conflicts entry it satisfies (dep_matches, against
/// its version and provides). A `repo/name` target of that package would
/// then make pacman abort the whole transaction ("unresolvable package
/// conflicts"), where without the target it asks to remove the package.
fn conflicted(
    installs: &RunInstalls,
    name: &str,
    pkg: &alpm::InstalledPkg,
    cmp: &mut dyn FnMut(&str, &str) -> Ordering,
) -> bool {
    installs
        .iter()
        .any(|(by, conflicts)| by != name && conflicts.iter().any(|c| dep_matches(c, name, &pkg.version, &pkg.provides, cmp)))
}

/// The rows the build lookup adds to the merge's, in the order they need:
/// - the switches first: they are explicit targets, so annotate_explicit
///   reads their builds (`info_for`) for IgnoreGroup and replaces too;
/// - then any switch that something the run may install conflicts with
///   goes (`conflicted`), and another build may then list its package as
///   replaced;
/// - the origin group last, so the replaces annotate_explicit adds keep
///   their packages out of it, and so does a conflict.
pub fn add_build_rows(
    updates: &mut Vec<Update>,
    inputs: &Inputs,
    builds: &SyncBuilds,
    info_for: &mut dyn FnMut(&[Update]) -> CandidateInfos,
    cmp: &mut dyn FnMut(&str, &str) -> Ordering,
) {
    let switches = switch_rows(updates, inputs, builds);
    updates.extend(switches);
    updates.sort_by(|a, b| a.name.cmp(&b.name));
    let info = info_for(updates);
    annotate_explicit(updates, &info, inputs, cmp);
    let installs = run_installs(inputs, updates, &info);
    let before = updates.len();
    updates.retain(|u| {
        !u.switch_build || !inputs.installed.get(&u.name).is_some_and(|pkg| conflicted(&installs, &u.name, pkg, cmp))
    });
    if updates.len() != before {
        // A dropped switch's package is no explicit row now, which kept
        // another build from listing it as replaced. A second pass only
        // adds what the first skipped for that.
        annotate_explicit(updates, &info, inputs, cmp);
    }
    let members = origin_members(updates, inputs, builds, &installs, cmp);
    updates.extend(origin_group_row(members, inputs.sync_entries));
    updates.sort_by(|a, b| a.name.cmp(&b.name));
}

/// The origin group's packages: installed with no %INSTALLED_DB%, no row
/// touching them, and a repository carrying their very build (version,
/// build date and packager), found by the same lookup as the switches.
/// libalpm records the repository only for a named target or a plain
/// sysupgrade candidate, so a package pulled in as a dependency, or as a
/// replacement, has none until its next upgrade; nothing is wrong with
/// the package itself. Reinstalling it from that repository, as a
/// `repo/name` target, records it. When several copies are the build, the
/// first in pacman.conf order (the lookup's order) is the target. Left
/// out:
/// - a package IgnorePkg/IgnoreGroup holds back: an explicit target of it
///   makes pacman ask to install it anyway;
/// - a package something the run may install conflicts with (`installs`,
///   see conflicted): its target would abort the whole -Syu.
///
/// Not seen: conflicts of dependencies new to the run (the -dd query
/// leaves them out), and a member's own %CONFLICTS%.
pub fn origin_members(
    updates: &[Update],
    inputs: &Inputs,
    builds: &SyncBuilds,
    installs: &RunInstalls,
    cmp: &mut dyn FnMut(&str, &str) -> Ordering,
) -> Vec<OriginMember> {
    let Inputs { installed, sync_entries, conf, .. } = *inputs;
    switch_suspects(updates, installed, sync_entries)
        .into_iter()
        .filter(|s| s.pkg.repo.is_none() && !conf.ignored.contains(s.name))
        .filter(|s| !conflicted(installs, s.name, s.pkg, cmp))
        .filter_map(|s| {
            let repo = s
                .copies
                .iter()
                .find(|r| builds.get(&(r.to_string(), s.name.to_string())).is_some_and(|b| is_installed_build(s.pkg, b)))?;
            Some(OriginMember {
                name: s.name.to_string(),
                version: s.pkg.version.clone(),
                repo: repo.to_string(),
                explicit: s.pkg.explicit,
            })
        })
        .collect()
}

/// The one row standing for the origin group, or None without members.
/// Its `candidate_repo` names the members' repositories in pacman.conf
/// order, for reading only; `old_version` and `new_version` say what the
/// row does, for front ends that show any row as "old → new" (the QML
/// one). A plain explicit target otherwise: unticked it is only left out,
/// never --ignore (main.rs paru_ignore). A sync reinstall keeps each
/// package's install reason (libalpm copies %REASON% from the installed
/// package), so dependencies stay dependencies with no -D afterwards.
pub fn origin_group_row(members: Vec<OriginMember>, sync_entries: &[alpm::SyncEntry]) -> Option<Update> {
    if members.is_empty() {
        return None;
    }
    let mut repos: Vec<&str> = Vec::new();
    for e in sync_entries {
        if !repos.contains(&e.repo.as_str()) && members.iter().any(|m| m.repo == e.repo) {
            repos.push(&e.repo);
        }
    }
    let n = members.len();
    Some(Update {
        name: ORIGIN_GROUP.to_string(),
        old_version: format!("{n} package{}", if n == 1 { "" } else { "s" }),
        new_version: "same builds".to_string(),
        installed_repo: None,
        candidate_repo: repos.join(", "),
        transition: Transition::UnknownSource,
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
        origin_group: members,
    })
}

/// Installed packages that may be untracked, before the AUR is asked
/// about them (flag_untracked). No row may touch them (an update, a
/// replacement, an AUR update: those are tracked, or about to change;
/// nor a newer build a row notes, which would replace them).
/// Two kinds:
/// - "no_source": no repository has a package of that name, whatever was
///   recorded (a recorded repository that dropped it tracks nothing now).
///   paru checks such "foreign" packages against the AUR, so an AUR
///   package of that name tracks it;
/// - "other_build": repositories have the name, but not this version, no
///   repository was recorded, and libalpm validated the file with nothing
///   (%VALIDATION% none): installed with -U from a file with no signature,
///   often an AUR build. paru splits installed packages by whether a sync
///   repository has the name (util.rs repo_aur_pkgs), so it never asks the
///   AUR about these, and the AUR's package tracks nothing.
///
/// A missing %INSTALLED_DB% alone says nothing: libalpm records none for
/// a package pulled in as a dependency or a replacement. One with a
/// checksum or a signature came from a repository (or a signed file), and
/// pacman tracks it by name, as it does one whose recorded repository
/// moved past it or back (-Syu warns about a newer local version): a
/// v4 repository's "1.68-1.1" that it dropped, while [extra] has
/// "1.68-1", is no error, and installing the lower build no advice.
///
/// The same version with another build is a switch or the origin group,
/// never this.
pub fn untracked_candidates(updates: &[Update], inputs: &Inputs) -> Vec<Untracked> {
    let Inputs { installed, sync_entries, .. } = *inputs;
    let mut cmp = memo_vercmp();
    let mut touched: HashSet<&str> = HashSet::new();
    for u in updates {
        touched.insert(u.name.as_str());
        touched.extend(u.replaces.iter().map(|r| r.name.as_str()));
        touched.extend(u.newer_replaces.iter().map(|r| r.name.as_str()));
        touched.extend(u.reinstalls.iter().map(String::as_str));
        touched.extend(u.origin_group.iter().map(|m| m.name.as_str()));
    }
    let mut copies: HashMap<&str, Vec<&alpm::SyncEntry>> = HashMap::new();
    for e in sync_entries {
        copies.entry(e.name.as_str()).or_default().push(e);
    }
    let mut out: Vec<Untracked> = installed
        .iter()
        .filter(|(name, _)| !touched.contains(name.as_str()))
        .filter_map(|(name, pkg)| {
            let (kind, repo_versions, hint) = match copies.get(name.as_str()) {
                None => {
                    let hint = match &pkg.repo {
                        Some(repo) => format!(
                            "{repo} no longer has it, and no other repository or the AUR does: remove it if nothing needs it, or find its source"
                        ),
                        None => "no repository or AUR package provides it: remove it if nothing needs it, or find its source"
                            .to_string(),
                    };
                    ("no_source", Vec::new(), hint)
                }
                Some(list)
                    if pkg.repo.is_none() && pkg.unsigned_file() && list.iter().all(|e| e.version != pkg.version) =>
                {
                    let first = list[0];
                    let versions = list.iter().map(|e| format!("{} {}", e.repo, e.version)).collect();
                    let hint = format!(
                        "{} has {}, not this build, which was installed from a file with no signature: install {}'s to track it, or find where this one came from",
                        first.repo, first.version, first.repo
                    );
                    ("other_build", versions, hint)
                }
                Some(_) => return None,
            };
            Some(Untracked {
                name: name.clone(),
                version: pkg.version.clone(),
                explicit: pkg.explicit,
                required_by: alpm::required_by(name, installed, &mut cmp),
                optional_for: alpm::optional_for(name, installed, &mut cmp),
                installed_repo: pkg.repo.clone(),
                kind: kind.to_string(),
                repo_versions,
                aur_version: None,
                hint,
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// The untracked packages among the candidates, given the AUR's answer
/// (name -> version), or why it could not be asked:
/// - "no_source": flagged when the AUR has no package of that name.
///   Without an answer none is flagged (most are AUR packages, which paru
///   tracks), and the second value says the AUR was unreachable;
/// - "other_build": always flagged, since paru never asks the AUR about
///   them (untracked_candidates); the answer only names the AUR's version.
pub fn flag_untracked(
    candidates: Vec<Untracked>,
    aur: Result<&HashMap<String, String>, &str>,
) -> (Vec<Untracked>, Option<String>) {
    let unchecked = candidates.iter().filter(|c| c.kind == "no_source").count();
    let mut out = Vec::new();
    for mut c in candidates {
        let aur_version = aur.ok().and_then(|has| has.get(&c.name));
        if c.kind == "no_source" {
            if aur.is_ok() && aur_version.is_none() {
                out.push(c);
            }
            continue;
        }
        if let (Some(v), Some((repo, version))) = (aur_version, c.repo_versions.first().and_then(|r| r.split_once(' '))) {
            c.hint = format!(
                "{repo} has {version}, not this build; the AUR has {v}, but paru never checks the AUR for a name a repository has: install {repo}'s to track it, or remove it if nothing needs it"
            );
            c.aur_version = Some(v.clone());
        }
        out.push(c);
    }
    let note = match aur {
        Err(e) if unchecked > 0 => Some(format!(
            "AUR unreachable ({e}): {unchecked} package{} in no repository {} not checked",
            if unchecked == 1 { "" } else { "s" },
            if unchecked == 1 { "was" } else { "were" }
        )),
        _ => None,
    };
    (out, note)
}

/// The AUR's RPC (v5) info endpoint; $MINKA_SYU_AUR_RPC overrides it.
const AUR_INFO_URL: &str = "https://aur.archlinux.org/rpc/v5/info";

/// The packages in an AUR RPC info answer (name -> version), or what went
/// wrong.
pub fn parse_aur_info(text: &str) -> Result<HashMap<String, String>, String> {
    let v: serde_json::Value = serde_json::from_str(text).map_err(|e| format!("unreadable answer: {e}"))?;
    if v.get("type").and_then(|t| t.as_str()) == Some("error") {
        let what = v.get("error").and_then(|e| e.as_str()).unwrap_or("an error");
        return Err(format!("the AUR answered: {what}"));
    }
    let results = v.get("results").and_then(|r| r.as_array()).ok_or("an answer without results")?;
    Ok(results
        .iter()
        .filter_map(|r| {
            let name = r.get("Name").and_then(|n| n.as_str())?;
            let version = r.get("Version").and_then(|v| v.as_str()).unwrap_or_default();
            Some((name.to_string(), version.to_string()))
        })
        .collect())
}

/// `arg[]=a&arg[]=b`, form-encoded: the names an info request asks about.
pub fn aur_info_form(names: &[String]) -> String {
    let encode = |name: &str| {
        name.bytes()
            .map(|b| match b {
                b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'.' | b'_' => (b as char).to_string(),
                other => format!("%{other:02X}"),
            })
            .collect::<String>()
    };
    names.iter().map(|n| format!("arg%5B%5D={}", encode(n))).collect::<Vec<_>>().join("&")
}

/// The User-Agent of every request MinkaSyu makes itself: generic, the
/// program and its version only.
pub const USER_AGENT: &str = concat!("minka-syu/", env!("CARGO_PKG_VERSION"));

/// Which of `names` the AUR has, with its versions (what paru asks for its
/// foreign packages anyway).
fn aur_versions(names: &[String]) -> Result<HashMap<String, String>, String> {
    if names.is_empty() {
        return Ok(HashMap::new());
    }
    parse_aur_info(&aur_info(names)?)
}

/// The AUR's answer to one RPC info request about `names`, POSTed so any
/// number of names fits, through curl, as the window fetches screenshots.
pub fn aur_info(names: &[String]) -> Result<String, String> {
    use std::io::Write;
    let url = std::env::var("MINKA_SYU_AUR_RPC").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| AUR_INFO_URL.to_string());
    let mut child = Command::new("curl")
        // It answers in well under a second; a host that never answers
        // costs 5 s, not the 30 of a plain --max-time.
        .args(["-sS", "--fail", "--connect-timeout", "5", "--max-time", "15", "-A", USER_AGENT, "--data-binary", "@-", "--"])
        .arg(&url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("running curl: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(aur_info_form(names).as_bytes()).map_err(|e| format!("writing to curl: {e}"))?;
    }
    let out = child.wait_with_output().map_err(|e| format!("running curl: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(err.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("curl failed").trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
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

/// $XDG_CACHE_HOME, or ~/.cache: where MinkaSyu's cache is, and paru's.
pub fn xdg_cache_home() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from).unwrap_or_else(|| home_dir().join(".cache"))
}

pub fn cache_dir() -> PathBuf {
    xdg_cache_home().join("minka-syu")
}

/// The throwaway pacman DB root a check refreshes. Browsing reads it
/// (when present) so search results reflect the last check's view of
/// the repos rather than the system DB's.
pub fn checkdb_path() -> PathBuf {
    cache_dir().join("checkdb")
}

/// Whether the system's sync DBs are the lists the window shows: every
/// repository in pacman.conf has the same database in the checker's copy
/// as in /var/lib/pacman. With no copy yet, browsing read the system's
/// own lists, so they match by definition. When pacman-conf cannot list
/// the repos, they count as different (an install then refreshes them).
pub fn system_lists_match_check() -> bool {
    let checker = checkdb_path().join("sync");
    if !checker.exists() {
        return true;
    }
    let Ok(repos) = run_capture("pacman-conf", &["--repo-list"]) else {
        return false;
    };
    let repos: Vec<&str> = repos.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    sync_dbs_match(&repos, &checker, Path::new("/var/lib/pacman/sync"))
}

/// Byte for byte, `<repo>.db` in one sync directory against the other. A
/// database missing from either side is a difference.
fn sync_dbs_match(repos: &[&str], a: &Path, b: &Path) -> bool {
    repos.iter().all(|repo| {
        let file = format!("{repo}.db");
        match (std::fs::read(a.join(&file)), std::fs::read(b.join(&file))) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    })
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

/// Groups and replaces of the explicit rows' builds, and of the newer
/// builds plain rows note, in one print-mode `pacman -Sp -dd` of their
/// repo/name targets on the private DB (one per row, so no name twice).
/// Failure means no information (no holds, no disclosures).
fn explicit_candidate_info(db: &str, updates: &[Update]) -> HashMap<(String, String), CandidateInfo> {
    let targets: Vec<String> = updates
        .iter()
        .filter(|u| !u.held && !u.is_origin_group())
        .filter_map(|u| build_repo(u).map(|repo| format!("{repo}/{}", u.name)))
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

/// The builds switch_rows needs, by print-mode `pacman -Sp -dd` of
/// repo/name targets on the private DB: one call per round of
/// switch_lookups (mostly a single one) until nothing is left to learn. A
/// failed call ends the lookup; what stays unknown makes no rows.
fn switch_builds(db: &str, updates: &[Update], inputs: &Inputs) -> SyncBuilds {
    let mut builds = SyncBuilds::new();
    let mut tried: HashSet<(String, String)> = HashSet::new();
    loop {
        let round = switch_lookups(updates, inputs, &builds, &tried);
        if round.is_empty() {
            break;
        }
        let targets: Vec<String> = round.iter().map(|(repo, name)| format!("{repo}/{name}")).collect();
        tried.extend(round);
        match Command::new("pacman")
            .args(["-Sp", "-dd", "--dbpath", db, "--logfile", "/dev/null", "--print-format", SWITCH_FORMAT])
            .args(&targets)
            .env("LC_ALL", "C")
            .env("TZ", "UTC0")
            .stdin(Stdio::null())
            .output()
        {
            Ok(out) if out.status.success() => builds.extend(parse_sync_builds(&String::from_utf8_lossy(&out.stdout))),
            Ok(out) => {
                eprintln!(
                    "minka-syu: pacman -Sp for same-version builds failed ({}): {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("")
                );
                break;
            }
            Err(e) => {
                eprintln!("minka-syu: pacman -Sp for same-version builds failed: {e}");
                break;
            }
        }
    }
    builds
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
    // Same-version switches, and the origin group: a package with no
    // recorded repository whose build a repository has. One build lookup
    // finds both (add_build_rows says in which order they join).
    let builds = switch_builds(db_str, &updates, &inputs);
    add_build_rows(
        &mut updates,
        &inputs,
        &builds,
        &mut |rows| explicit_candidate_info(db_str, rows),
        &mut memo_vercmp(),
    );
    let repo_change_count = updates.iter().filter(|u| u.pending() && u.transition == Transition::Changed).count();

    // Packages nothing tracks. Most candidates are AUR packages, so the
    // AUR is asked once about all of them.
    let candidates = untracked_candidates(&updates, &inputs);
    let names: Vec<String> = candidates.iter().map(|c| c.name.clone()).collect();
    let aur = aur_versions(&names);
    if let Err(e) = &aur {
        eprintln!("minka-syu: could not ask the AUR which packages it has: {e}");
    }
    let (untracked, aur_unreachable) = flag_untracked(candidates, aur.as_ref().map_err(String::as_str));

    let previous = load_state();
    let state = State {
        schema: 1,
        checked_at: now_unix(),
        updates,
        repo_change_count,
        last_notified_count: previous.map(|p| p.last_notified_count).unwrap_or(0),
        untracked,
        aur_unreachable,
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

    #[test]
    fn sync_dbs_match_byte_for_byte_and_every_repo() {
        let root = std::env::temp_dir().join(format!("minka-syu-check-test-{}-syncdbs", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (checker, system) = (root.join("checker"), root.join("system"));
        std::fs::create_dir_all(&checker).unwrap();
        std::fs::create_dir_all(&system).unwrap();
        for dir in [&checker, &system] {
            std::fs::write(dir.join("core.db"), b"core 1").unwrap();
            std::fs::write(dir.join("minka.db"), b"minka 1").unwrap();
        }
        assert!(sync_dbs_match(&["core", "minka"], &checker, &system));
        // 24/11/2026: the check saw minkade, the system's lists did not.
        std::fs::write(checker.join("minka.db"), b"minka 2").unwrap();
        assert!(!sync_dbs_match(&["core", "minka"], &checker, &system));
        assert!(sync_dbs_match(&["core"], &checker, &system));
        // A repository new to pacman.conf, not yet synced by the system.
        std::fs::write(checker.join("new.db"), b"new").unwrap();
        assert!(!sync_dbs_match(&["core", "new"], &checker, &system));
        let _ = std::fs::remove_dir_all(&root);
    }

    fn pkg(version: &str, repo: Option<&str>) -> alpm::InstalledPkg {
        alpm::InstalledPkg {
            version: version.into(),
            repo: repo.map(String::from),
            explicit: true,
            ..Default::default()
        }
    }

    /// Depstrings, parsed.
    fn deps(list: &[&str]) -> Vec<DepSpec> {
        list.iter().filter_map(|d| parse_depstring(d)).collect()
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
            assert_eq!((u.newer_repo.as_ref(), u.newer_version.as_ref()), (None, None));
        }
    }

    #[test]
    fn a_newer_lower_repo_build_is_noted_on_pacman_s_own_upgrade() {
        let sync = alpm::parse_sync_list(
            "cachyos-extra-v4 bcachefs-dkms 3:1.39.7-1.2\n\
             minka bcachefs-dkms 3:1.39.7-2\n",
        );
        let ups = merge("bcachefs-dkms 3:1.39.7-1.1 -> 3:1.39.7-1.2\n", "", &installed_map(), &sync, None, &Conf::default());
        assert_eq!(ups.len(), 1);
        let u = &ups[0];
        // The row is -Syu's upgrade, which an untick (--ignore) would hold.
        assert_eq!((u.candidate_repo.as_str(), u.new_version.as_str()), ("cachyos-extra-v4", "3:1.39.7-1.2"));
        assert!(!u.explicit_target && u.pending());
        assert_eq!(u.transition, Transition::Same);
        assert_eq!((u.first_repo.as_ref(), u.first_version.as_ref()), (None, None));
        assert_eq!(u.newer_repo.as_deref(), Some("minka"));
        assert_eq!(u.newer_version.as_deref(), Some("3:1.39.7-2"));
        assert_eq!(u.newer_note().as_deref(), Some("newer build in minka: 3:1.39.7-2"));

        // Equal versions: nothing newer.
        let tie = alpm::parse_sync_list(
            "cachyos-extra-v4 bcachefs-dkms 3:1.39.7-2\n\
             minka bcachefs-dkms 3:1.39.7-2\n",
        );
        let ups = merge("bcachefs-dkms 3:1.39.7-1.1 -> 3:1.39.7-2\n", "", &installed_map(), &tie, None, &Conf::default());
        assert_eq!(ups.len(), 1);
        assert_eq!(ups[0].candidate_repo, "cachyos-extra-v4");
        assert!(!ups[0].explicit_target);
        assert_eq!((ups[0].newer_repo.as_ref(), ups[0].newer_note()), (None, None));
    }

    #[test]
    fn a_rebuild_suffix_never_holds_back_pacman_s_upgrade() {
        // 23/11/2026: nvidia-settings 615.78.08-1 from cachyos-v4, which
        // has 620.73.04-1; ALHP's extra-x86-64-v4 (ranked below) has
        // 620.73.04-1.1, newer only by its rebuild suffix. The row was
        // that build, unticked, so paru got --ignore=nvidia-settings and
        // skipped cachyos-v4's upgrade as well.
        let installed = HashMap::from([("nvidia-settings".to_string(), pkg("615.78.08-1", Some("cachyos-v4")))]);
        let sync = alpm::parse_sync_list(
            "cachyos-v4 nvidia-settings 620.73.04-1\n\
             extra-x86-64-v4 nvidia-settings 620.73.04-1.1\n\
             extra nvidia-settings 620.73.04-1\n",
        );
        let ups = merge(
            "nvidia-settings 615.78.08-1 -> 620.73.04-1\n",
            "",
            &installed,
            &sync,
            Some("nvidia-settings|620.73.04-1|cachyos-v4|||\n"),
            &Conf::default(),
        );
        assert_eq!(ups.len(), 1, "{ups:#?}");
        let u = &ups[0];
        assert!(!u.explicit_target && !u.held && u.pending(), "{u:#?}");
        assert_eq!((u.candidate_repo.as_str(), u.new_version.as_str()), ("cachyos-v4", "620.73.04-1"));
        assert_eq!(u.transition, Transition::Same);
        assert_eq!(u.newer_note().as_deref(), Some("newer build in extra-x86-64-v4: 620.73.04-1.1"));
        // One update, and no change of repository: the noted build is not
        // what the run installs.
        let state = State { schema: 1, updates: ups.clone(), repo_change_count: 0, ..Default::default() };
        assert_eq!(state.total(), 1);
        assert_eq!(ups.iter().filter(|u| u.pending() && u.transition == Transition::Changed).count(), 0);
        // The CLI's apply has no target for it: a plain -Syu upgrades it.
        assert!(ups.iter().all(|u| !u.explicit_target));

        // In state.json only when there is one, and read back.
        let json = serde_json::to_value(u).unwrap();
        assert_eq!(json["newer_repo"], "extra-x86-64-v4");
        let back: Update = serde_json::from_value(json).unwrap();
        assert_eq!(&back, u);
        let plain = Update { newer_repo: None, newer_version: None, ..u.clone() };
        let json = serde_json::to_value(&plain).unwrap();
        assert!(json.get("newer_repo").is_none() && json.get("newer_version").is_none(), "{json}");
        assert!(json.get("newer_replaces").is_none(), "{json}");
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
        let state = State { schema: 1, checked_at: 0, updates: ups, repo_change_count: 0, last_notified_count: 0, ..Default::default() };
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
        let state = State { schema: 1, checked_at: 0, updates: ups, repo_change_count: 0, last_notified_count: 0, ..Default::default() };
        assert_eq!((state.total(), state.held()), (0, 1));
    }

    #[test]
    fn held_updates_are_listed_and_left_alone() {
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
        assert_eq!(ups[0].newer_repo, None, "a held row points at no other build");
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

        // The same rules for a newer build a row notes (hi's aa 2-1 is
        // -Su's upgrade, lo's 3-1 replaces zz), listed apart from what the
        // row's own build replaces; n7 holds for it too.
        let installed = HashMap::from([("aa".to_string(), pkg("1-1", Some("hi"))), ("zz".to_string(), pkg("2-1", None))]);
        let sync = alpm::parse_sync_list("hi aa 2-1\nlo aa 3-1\n");
        let sup = parse_sysupgrade("aa|2-1|hi|\n");
        let ups = explicit_inputs_case(&installed, &sync, &sup, &Conf::default(), "aa|lo||zz\n");
        assert!(!ups[0].explicit_target && ups[0].replaces.is_empty(), "{ups:#?}");
        assert_eq!(ups[0].newer_replaces.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["zz"]);
        let ups = explicit_inputs_case(&installed, &sync, &sup, &usage(&["hi"], &["hi", "lo"]), "aa|lo||zz\n");
        assert_eq!(ups[0].newer_repo.as_deref(), Some("lo"), "{ups:#?}");
        assert!(ups[0].newer_replaces.is_empty(), "{ups:#?}");
    }

    #[test]
    fn a_newer_build_ignoregroup_holds_is_not_noted() {
        // hi/foo 2-1 has no group, lo/foo 3-1 is in plasma; IgnoreGroup =
        // plasma*. The row is -Syu's upgrade to hi's 2-1 either way, and
        // points at lo's build only while pacman.conf lets it in.
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
        let merged = merge_updates_with_cmp(&inputs, &mut system_vercmp);
        assert!(!merged[0].explicit_target);
        assert_eq!(merged[0].newer_note().as_deref(), Some("newer build in lo: 3-1"));
        let mut ups = merged.clone();
        annotate_explicit(&mut ups, &parse_candidate_info("foo|lo|plasma|\n"), &inputs, &mut system_vercmp);
        let u = &ups[0];
        assert!(!u.held && !u.explicit_target, "{u:#?}");
        assert_eq!((u.candidate_repo.as_str(), u.new_version.as_str()), ("hi", "2-1"));
        assert_eq!((u.first_repo.as_ref(), u.newer_repo.as_ref(), u.newer_version.as_ref()), (None, None, None));
        // Outside the group, the note stays.
        let mut ups = merged;
        annotate_explicit(&mut ups, &parse_candidate_info("foo|lo|kde|\n"), &inputs, &mut system_vercmp);
        assert_eq!(ups[0].newer_repo.as_deref(), Some("lo"));
        assert!(!ups[0].held);
    }

    #[test]
    fn sysupgrade_parsing_skips_junk() {
        let t = parse_sysupgrade("nautilus|51.1-1.1|cachyos-extra-v4|\nbroken line\n|1|x|y\na|1|r|b c<2\n");
        assert_eq!(t.len(), 2);
        assert!(t[0].replaces.is_empty());
        assert_eq!(t[1].replaces.len(), 2);
    }

    // ---- same-version switches ---------------------------------------------

    /// An installed package with a recorded build.
    fn built(version: &str, repo: Option<&str>, date: i64, packager: &str) -> alpm::InstalledPkg {
        alpm::InstalledPkg { build_date: Some(date), packager: packager.into(), ..pkg(version, repo) }
    }

    /// What pacman -Sp prints for one repo's build, in SWITCH_FORMAT, the
    /// date as %c in the C locale (UTC).
    fn sp_line(repo: &str, name: &str, version: &str, date: &str, packager: &str) -> String {
        format!("{name}|{repo}|{version}|{date}|{packager}\n")
    }

    /// lib32-libcaca on 22/11/2026: Arch's [multilib] build, installed;
    /// multilib no longer has it, [minka] has a rebuild of that version.
    const ARCH_LIBCACA: (&str, i64, &str) = ("0.99.beta20-2", 1725703316, "Antonio Rojas <arojas@archlinux.org>");
    const MINKA_LIBCACA: &str = "Sat Nov 22 01:48:00 2026";

    /// The check's switch pipeline: merge, lookup rounds answered from
    /// `printed` (all the builds pacman -Sp could print), switch rows,
    /// then annotate_explicit with `info`. Also how many rounds it took.
    fn switch_case(
        repo_lines: &str,
        installed: &HashMap<String, alpm::InstalledPkg>,
        sync: &[alpm::SyncEntry],
        conf: &Conf,
        printed: &str,
        info: &str,
    ) -> (Vec<Update>, usize) {
        let inputs = Inputs {
            repo_lines,
            aur_lines: "",
            installed,
            sync_entries: sync,
            sysupgrade: Some(&[]),
            final_targets: None,
            conf,
        };
        let mut ups = merge_updates_with_cmp(&inputs, &mut system_vercmp);
        let all = parse_sync_builds(printed);
        let (mut builds, mut tried, mut rounds) = (SyncBuilds::new(), HashSet::new(), 0);
        loop {
            let round = switch_lookups(&ups, &inputs, &builds, &tried);
            if round.is_empty() {
                break;
            }
            rounds += 1;
            let names: HashSet<&str> = round.iter().map(|(_, n)| n.as_str()).collect();
            assert_eq!(names.len(), round.len(), "a name twice in one call: {round:?}");
            for k in round {
                if let Some(b) = all.get(&k) {
                    builds.insert(k.clone(), b.clone());
                }
                tried.insert(k);
            }
        }
        add_build_rows(&mut ups, &inputs, &builds, &mut |_| parse_candidate_info(info), &mut system_vercmp);
        // The origin group has tests of its own.
        ups.retain(|u| !u.is_origin_group());
        (ups, rounds)
    }

    fn libcaca_installed() -> HashMap<String, alpm::InstalledPkg> {
        let (v, date, by) = ARCH_LIBCACA;
        HashMap::from([("lib32-libcaca".to_string(), built(v, Some("multilib"), date, by))])
    }

    #[test]
    fn a_dropped_origin_with_another_build_is_a_switch() {
        let sync = alpm::parse_sync_list("extra libcaca 0.99.beta20-6\nminka lib32-libcaca 0.99.beta20-2\n");
        let printed = sp_line("minka", "lib32-libcaca", "0.99.beta20-2", MINKA_LIBCACA, "Sophie <s@example.org>");
        let (ups, rounds) = switch_case("", &libcaca_installed(), &sync, &Conf::default(), &printed, "");
        assert_eq!(rounds, 1);
        assert_eq!(ups.len(), 1, "{ups:#?}");
        let u = &ups[0];
        assert!(u.switch_build && u.explicit_target && u.pending(), "{u:#?}");
        assert_eq!((u.old_version.as_str(), u.new_version.as_str()), ("0.99.beta20-2", "0.99.beta20-2"));
        assert_eq!(u.installed_repo.as_deref(), Some("multilib"));
        assert_eq!(u.candidate_repo, "minka");
        assert_eq!(u.transition, Transition::Changed);
        assert_eq!((u.first_repo.as_ref(), u.first_version.as_ref()), (None, None));
        let state = State { schema: 1, checked_at: 0, updates: ups, repo_change_count: 0, last_notified_count: 0, ..Default::default() };
        assert_eq!((state.total(), state.held()), (1, 0));

        // Several other builds of that version: the first in pacman.conf
        // order is the target, once every copy is known to differ.
        let sync = alpm::parse_sync_list("cachyos lib32-libcaca 0.99.beta20-2\nminka lib32-libcaca 0.99.beta20-2\n");
        let printed = format!(
            "{}{}",
            sp_line("cachyos", "lib32-libcaca", "0.99.beta20-2", "Sun Nov  1 10:00:00 2026", "CachyOS <admin@cachyos.org>"),
            sp_line("minka", "lib32-libcaca", "0.99.beta20-2", MINKA_LIBCACA, "Sophie <s@example.org>")
        );
        let (ups, rounds) = switch_case("", &libcaca_installed(), &sync, &Conf::default(), &printed, "");
        assert_eq!(rounds, 2, "one copy per call");
        assert_eq!(ups[0].candidate_repo, "cachyos");
        // A foreign package (no %INSTALLED_DB%) with another build: a switch
        // from an unknown source.
        let foreign = HashMap::from([("lib32-libcaca".to_string(), built("0.99.beta20-2", None, 1, "Sophie <s@example.org>"))]);
        let (ups, _) = switch_case("", &foreign, &sync, &Conf::default(), &printed, "");
        assert!(ups[0].switch_build);
        assert_eq!(ups[0].transition, Transition::UnknownSource);
    }

    #[test]
    fn a_repo_build_without_installed_db_is_no_switch() {
        // Installed before libalpm recorded %INSTALLED_DB%, or with -U of
        // the repo's own file: build date and packager equal [extra]'s.
        let installed =
            HashMap::from([("zstd".to_string(), built("1.5.7-2", None, 1755000000, "Levente Polyák <anthraxx@archlinux.org>"))]);
        let sync = alpm::parse_sync_list("cachyos zstd 1.5.7-2\nextra zstd 1.5.7-2\n");
        let printed = format!(
            "{}{}",
            sp_line("cachyos", "zstd", "1.5.7-2", "Sun Nov  1 10:00:00 2026", "CachyOS <admin@cachyos.org>"),
            sp_line("extra", "zstd", "1.5.7-2", "Tue Aug 12 12:00:00 2025", "Levente Polyák <anthraxx@archlinux.org>")
        );
        let (ups, rounds) = switch_case("", &installed, &sync, &Conf::default(), &printed, "");
        assert!(ups.is_empty(), "{ups:#?}");
        assert_eq!(rounds, 2, "cachyos's copy differs, so extra's is looked up too");
    }

    #[test]
    fn an_origin_still_carrying_the_build_is_no_switch() {
        let installed =
            HashMap::from([("ripgrep".to_string(), built("15.1.0-1", Some("extra"), 1755000000, "Arch <a@archlinux.org>"))]);
        let sync = alpm::parse_sync_list("cachyos ripgrep 15.1.0-1\nextra ripgrep 15.1.0-1\n");
        let printed = format!(
            "{}{}",
            sp_line("cachyos", "ripgrep", "15.1.0-1", "Sun Nov  1 10:00:00 2026", "CachyOS <admin@cachyos.org>"),
            sp_line("extra", "ripgrep", "15.1.0-1", "Tue Aug 12 12:00:00 2025", "Arch <a@archlinux.org>")
        );
        let (ups, rounds) = switch_case("", &installed, &sync, &Conf::default(), &printed, "");
        assert!(ups.is_empty(), "{ups:#?}");
        assert_eq!(rounds, 1, "the origin's copy is looked up first and settles it");
    }

    #[test]
    fn a_newer_version_is_an_update_not_a_switch() {
        // [cachyos] (first) has a newer build: -Qu's ordinary update.
        let sync = alpm::parse_sync_list("cachyos lib32-libcaca 0.99.beta20-3\nminka lib32-libcaca 0.99.beta20-2\n");
        let printed = sp_line("minka", "lib32-libcaca", "0.99.beta20-2", MINKA_LIBCACA, "Sophie <s@example.org>");
        let (ups, rounds) = switch_case(
            "lib32-libcaca 0.99.beta20-2 -> 0.99.beta20-3\n",
            &libcaca_installed(),
            &sync,
            &Conf::default(),
            &printed,
            "",
        );
        assert_eq!(rounds, 0);
        assert_eq!(ups.len(), 1, "{ups:#?}");
        assert!(!ups[0].switch_build && !ups[0].explicit_target);
        assert_eq!(ups[0].new_version, "0.99.beta20-3");
        // A lower repo has the newer one: a later-repo row, not a switch.
        let sync = alpm::parse_sync_list("cachyos lib32-libcaca 0.99.beta20-2\nminka lib32-libcaca 0.99.beta20-3\n");
        let (ups, _) = switch_case("", &libcaca_installed(), &sync, &Conf::default(), &printed, "");
        assert_eq!(ups.len(), 1, "{ups:#?}");
        assert!(ups[0].explicit_target && !ups[0].switch_build);
        assert_eq!(ups[0].first_repo.as_deref(), Some("cachyos"));
    }

    #[test]
    fn a_copy_of_the_installed_build_is_no_switch() {
        // Only [minka] has that version, and it is the very build installed
        // (the same file, republished): nothing to switch to.
        let sync = alpm::parse_sync_list("minka lib32-libcaca 0.99.beta20-2\n");
        let (v, _, by) = ARCH_LIBCACA;
        let printed = sp_line("minka", "lib32-libcaca", v, "Sat Sep  7 10:01:56 2024", by);
        let (ups, _) = switch_case("", &libcaca_installed(), &sync, &Conf::default(), &printed, "");
        assert!(ups.is_empty(), "{ups:#?}");
        // The same date by another packager is another build.
        let printed = sp_line("minka", "lib32-libcaca", v, "Sat Sep  7 10:01:56 2024", "Sophie <s@example.org>");
        let (ups, _) = switch_case("", &libcaca_installed(), &sync, &Conf::default(), &printed, "");
        assert_eq!(ups.len(), 1);
    }

    #[test]
    fn unknown_builds_settle_nothing() {
        let sync = alpm::parse_sync_list("cachyos lib32-libcaca 0.99.beta20-2\nminka lib32-libcaca 0.99.beta20-2\n");
        // pacman printed only one copy (the lookup failed for the other).
        let printed = sp_line("minka", "lib32-libcaca", "0.99.beta20-2", MINKA_LIBCACA, "Sophie <s@example.org>");
        let (ups, _) = switch_case("", &libcaca_installed(), &sync, &Conf::default(), &printed, "");
        assert!(ups.is_empty(), "{ups:#?}");
        // A build without a date is no build to compare: pacman prints the
        // epoch for it.
        let printed = sp_line("minka", "lib32-libcaca", "0.99.beta20-2", "Thu Jan  1 00:00:00 1970", "Sophie <s@example.org>");
        let sync = alpm::parse_sync_list("minka lib32-libcaca 0.99.beta20-2\n");
        let (ups, rounds) = switch_case("", &libcaca_installed(), &sync, &Conf::default(), &printed, "");
        assert_eq!(rounds, 1);
        assert!(ups.is_empty(), "{ups:#?}");
    }

    #[test]
    fn copies_without_install_usage_are_looked_up_but_never_targets() {
        // Built from the AUR (no %INSTALLED_DB%); [hi] has Install, [lo]
        // (later in pacman.conf) has not. A repo/name lookup works in any
        // repository, so lo's copy is learnt too, in a later round.
        let installed = HashMap::from([("xx".to_string(), built("1-1", None, 5, "p"))]);
        let sync = alpm::parse_sync_list("hi xx 1-1\nlo xx 1-1\n");
        let conf = Conf { install_repos: Some(HashSet::from(["hi".to_string()])), ..Conf::default() };
        let inputs = Inputs {
            repo_lines: "",
            aur_lines: "",
            installed: &installed,
            sync_entries: &sync,
            sysupgrade: Some(&[]),
            final_targets: None,
            conf: &conf,
        };
        let k = |r: &str, n: &str| (r.to_string(), n.to_string());
        let b = |date: i64| SyncBuild { version: "1-1".into(), build_date: Some(date), packager: "p".into() };
        assert_eq!(switch_lookups(&[], &inputs, &SyncBuilds::new(), &HashSet::new()), [k("hi", "xx")]);
        let builds = SyncBuilds::from([(k("hi", "xx"), b(6))]);
        let tried = HashSet::from([k("hi", "xx")]);
        assert_eq!(switch_lookups(&[], &inputs, &builds, &tried), [k("lo", "xx")]);

        // Both copies are other builds: a row, targeting the Install repo.
        let sp = |repo: &str, date: &str| sp_line(repo, "xx", "1-1", date, "p");
        let printed = format!("{}{}", sp("hi", "Thu Jan  1 00:00:06 1970"), sp("lo", "Thu Jan  1 00:00:07 1970"));
        let (ups, rounds) = switch_case("", &installed, &sync, &conf, &printed, "");
        assert_eq!(rounds, 2);
        assert_eq!(ups.len(), 1, "{ups:#?}");
        assert!(ups[0].switch_build);
        assert_eq!(ups[0].candidate_repo, "hi");
        // lo's copy is the installed build (an -U of its file): no row.
        let printed = format!("{}{}", sp("hi", "Thu Jan  1 00:00:06 1970"), sp("lo", "Thu Jan  1 00:00:05 1970"));
        let (ups, _) = switch_case("", &installed, &sync, &conf, &printed, "");
        assert!(ups.is_empty(), "{ups:#?}");

        // A package whose only copy is in [lo] has no target: not looked up.
        let sync = alpm::parse_sync_list("lo xx 1-1\n");
        let inputs = Inputs { sync_entries: &sync, ..inputs };
        assert!(switch_lookups(&[], &inputs, &SyncBuilds::new(), &HashSet::new()).is_empty());
    }

    #[test]
    fn ignored_switches_are_held() {
        let sync = alpm::parse_sync_list("minka lib32-libcaca 0.99.beta20-2\n");
        let printed = sp_line("minka", "lib32-libcaca", "0.99.beta20-2", MINKA_LIBCACA, "Sophie <s@example.org>");
        // IgnorePkg on the installed package: shown held, like any other row.
        let conf = Conf { ignored: HashSet::from(["lib32-libcaca".to_string()]), ..Conf::default() };
        let (ups, _) = switch_case("", &libcaca_installed(), &sync, &conf, &printed, "");
        assert_eq!(ups.len(), 1);
        assert!(ups[0].switch_build && ups[0].held, "{ups:#?}");
        assert_eq!(ups[0].held_by.as_deref(), Some("ignore"));
        let state = State { schema: 1, checked_at: 0, updates: ups, repo_change_count: 0, last_notified_count: 0, ..Default::default() };
        assert_eq!((state.total(), state.held()), (0, 1));
        // IgnoreGroup on the switch's own build: held as well.
        let conf = Conf { group_patterns: vec!["lib32*".into()], ..Conf::default() };
        let (ups, _) = switch_case("", &libcaca_installed(), &sync, &conf, &printed, "lib32-libcaca|minka|lib32-extra|\n");
        assert!(ups[0].switch_build && ups[0].held, "{ups:#?}");
    }

    #[test]
    fn switch_lookups_ask_each_name_once_origin_first() {
        let installed = HashMap::from([
            ("aa".to_string(), built("1-1", Some("lo"), 5, "p")),
            ("bb".to_string(), built("1-1", None, 5, "p")),
            ("cc".to_string(), pkg("1-1", None)),
        ]);
        let sync = alpm::parse_sync_list("hi aa 1-1\nhi bb 1-1\nhi cc 1-1\nlo aa 1-1\nlo bb 1-1\n");
        let inputs = Inputs {
            repo_lines: "",
            aur_lines: "",
            installed: &installed,
            sync_entries: &sync,
            sysupgrade: Some(&[]),
            final_targets: None,
            conf: &Conf::default(),
        };
        let k = |r: &str, n: &str| (r.to_string(), n.to_string());
        let (builds, tried) = (SyncBuilds::new(), HashSet::new());
        // cc has no build date to compare: never looked up.
        assert_eq!(switch_lookups(&[], &inputs, &builds, &tried), [k("lo", "aa"), k("hi", "bb")]);
        let tried = HashSet::from([k("lo", "aa"), k("hi", "bb")]);
        let mut builds = SyncBuilds::new();
        let b = |date: i64| SyncBuild { version: "1-1".into(), build_date: Some(date), packager: "p".into() };
        builds.insert(k("lo", "aa"), b(5));
        builds.insert(k("hi", "bb"), b(6));
        // aa matched its origin; bb's next copy is asked.
        assert_eq!(switch_lookups(&[], &inputs, &builds, &tried), [k("lo", "bb")]);
        // A package with a row of its own, or one a row replaces, is never
        // asked about.
        let row = |name: &str, replaces: &[&str]| Update {
            name: name.into(),
            old_version: "1-1".into(),
            new_version: "2-1".into(),
            installed_repo: None,
            candidate_repo: "hi".into(),
            transition: Transition::UnknownSource,
            explicit_target: false,
            first_repo: None,
            first_version: None,
            newer_repo: None,
            newer_version: None,
            newer_replaces: Vec::new(),
            replaces: replaces
                .iter()
                .map(|n| Replaced { name: n.to_string(), version: "1-1".into(), installed_repo: None })
                .collect(),
            held: false,
            held_by: None,
            superseded_by: Vec::new(),
            new_install: false,
            reinstalls: Vec::new(),
            switch_build: false,
            origin_group: Vec::new(),
        };
        assert!(switch_lookups(&[row("bb", &[])], &inputs, &builds, &tried).is_empty());
        assert!(switch_lookups(&[row("zz", &["bb"])], &inputs, &builds, &tried).is_empty());
    }

    #[test]
    fn c_locale_dates_and_build_lines_parse() {
        assert_eq!(parse_c_date("Thu Aug 27 12:25:22 2026"), Some(1787833522));
        assert_eq!(parse_c_date("Sat Sep  7 10:01:56 2024"), Some(1725703316));
        assert_eq!(parse_c_date("Thu Jan  1 00:00:00 1970"), Some(0));
        assert_eq!(parse_c_date("Tue Feb 29 23:59:59 2028"), Some(1835481599));
        assert_eq!(parse_c_date("Wed Mar  1 00:00:00 2000"), Some(951868800));
        assert_eq!(parse_c_date(""), None);
        assert_eq!(parse_c_date("Sat 07 Sep 2024 11:01:56 BST"), None, "a localised %c is refused");
        assert_eq!(parse_c_date("Sat Sep  7 25:01:56 2024"), None);
        let b = parse_sync_builds(
            "python-pkg_resources|cachyos|81.0.0-1|Thu Aug 27 12:25:22 2026|CachyOS <admin@cachyos.org>\n\
             warning: not a build line\n\
             odd|minka|1-1||A | B <ab@example.org>\n\
             nodate|minka|1-1|Thu Jan  1 00:00:00 1970|T <t@example.org>\n",
        );
        assert_eq!(b.len(), 3);
        let nodate = &b[&("minka".to_string(), "nodate".to_string())];
        assert_eq!(nodate.build_date, None, "pacman's epoch for no date is unknown");
        let p = &b[&("cachyos".to_string(), "python-pkg_resources".to_string())];
        assert_eq!((p.version.as_str(), p.build_date), ("81.0.0-1", Some(1787833522)));
        assert_eq!(p.packager, "CachyOS <admin@cachyos.org>");
        let odd = &b[&("minka".to_string(), "odd".to_string())];
        assert_eq!((odd.build_date, odd.packager.as_str()), (None, "A | B <ab@example.org>"));
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
        assert!(!u.switch_build && !u.is_origin_group());
        assert!(state.untracked.is_empty() && state.aur_unreachable.is_none());
        assert_eq!(state.total(), 1);
    }

    // ---- the origin group --------------------------------------------------

    /// The check's pipeline as run_check runs it: merge, lookup rounds
    /// answered from `printed`, then add_build_rows.
    fn origin_case(
        repo_lines: &str,
        installed: &HashMap<String, alpm::InstalledPkg>,
        sync: &[alpm::SyncEntry],
        conf: &Conf,
        printed: &str,
    ) -> Vec<Update> {
        origin_case_with(repo_lines, "", installed, sync, conf, printed, "")
    }

    /// origin_case with `sup` as `-Sup -dd` output (TARGET_FORMAT) and
    /// `info` as the explicit rows' builds (BUILD_FORMAT).
    fn origin_case_with(
        repo_lines: &str,
        sup: &str,
        installed: &HashMap<String, alpm::InstalledPkg>,
        sync: &[alpm::SyncEntry],
        conf: &Conf,
        printed: &str,
        info: &str,
    ) -> Vec<Update> {
        let targets = parse_sysupgrade(sup);
        let inputs = Inputs {
            repo_lines,
            aur_lines: "",
            installed,
            sync_entries: sync,
            sysupgrade: Some(&targets),
            final_targets: None,
            conf,
        };
        let mut ups = merge_updates_with_cmp(&inputs, &mut system_vercmp);
        let all = parse_sync_builds(printed);
        let (mut builds, mut tried) = (SyncBuilds::new(), HashSet::new());
        loop {
            let round = switch_lookups(&ups, &inputs, &builds, &tried);
            if round.is_empty() {
                break;
            }
            for k in round {
                if let Some(b) = all.get(&k) {
                    builds.insert(k.clone(), b.clone());
                }
                tried.insert(k);
            }
        }
        add_build_rows(&mut ups, &inputs, &builds, &mut |_| parse_candidate_info(info), &mut system_vercmp);
        ups
    }

    const EXTRA_DATE: &str = "Tue Aug 12 12:00:00 2025";
    const EXTRA_SECS: i64 = 1755000000;
    const ARCH: &str = "Arch <a@archlinux.org>";

    /// A dependency installed with no %INSTALLED_DB%, built at EXTRA_SECS by ARCH.
    fn pulled_in(version: &str) -> alpm::InstalledPkg {
        alpm::InstalledPkg { explicit: false, ..built(version, None, EXTRA_SECS, ARCH) }
    }

    #[test]
    fn a_repo_build_without_recorded_repo_joins_the_origin_group() {
        let installed = HashMap::from([
            ("zstd".to_string(), pulled_in("1.5.7-2")),
            // Its repository recorded: tracked, never in the group.
            ("ripgrep".to_string(), built("15.1.0-1", Some("extra"), EXTRA_SECS, ARCH)),
        ]);
        let sync = alpm::parse_sync_list("extra zstd 1.5.7-2\nextra ripgrep 15.1.0-1\n");
        let printed = format!(
            "{}{}",
            sp_line("extra", "zstd", "1.5.7-2", EXTRA_DATE, ARCH),
            sp_line("extra", "ripgrep", "15.1.0-1", EXTRA_DATE, ARCH)
        );
        let ups = origin_case("", &installed, &sync, &Conf::default(), &printed);
        assert_eq!(ups.len(), 1, "{ups:#?}");
        let g = &ups[0];
        assert!(g.is_origin_group() && g.explicit_target && g.pending() && !g.switch_build, "{g:#?}");
        assert_eq!(g.name, ORIGIN_GROUP);
        assert_eq!(g.transition, Transition::UnknownSource);
        assert_eq!(
            g.origin_group,
            [OriginMember { name: "zstd".into(), version: "1.5.7-2".into(), repo: "extra".into(), explicit: false }]
        );
        assert_eq!(g.origin_group[0].target(), "extra/zstd");

        // The same version as another build: a switch, not the group.
        let printed = sp_line("extra", "zstd", "1.5.7-2", "Sun Nov  1 10:00:00 2026", ARCH);
        let ups = origin_case("", &installed, &sync, &Conf::default(), &printed);
        assert!(ups.iter().all(|u| !u.is_origin_group()), "{ups:#?}");
        assert!(ups.iter().any(|u| u.name == "zstd" && u.switch_build), "{ups:#?}");

        // A build lookup that found nothing settles nothing.
        assert!(origin_case("", &installed, &sync, &Conf::default(), "").is_empty());

        // A pending upgrade is an update row, and the upgrade records it.
        let sync = alpm::parse_sync_list("extra zstd 1.5.7-3\nextra ripgrep 15.1.0-1\n");
        let ups = origin_case("zstd 1.5.7-2 -> 1.5.7-3\n", &installed, &sync, &Conf::default(), &printed);
        assert_eq!(ups.len(), 1, "{ups:#?}");
        assert!(!ups[0].is_origin_group() && ups[0].name == "zstd" && ups[0].new_version == "1.5.7-3");

        // IgnorePkg holds it: an explicit target would install it anyway.
        let sync = alpm::parse_sync_list("extra zstd 1.5.7-2\n");
        let printed = sp_line("extra", "zstd", "1.5.7-2", EXTRA_DATE, ARCH);
        let conf = Conf { ignored: HashSet::from(["zstd".to_string()]), ..Conf::default() };
        assert!(origin_case("", &installed, &sync, &conf, &printed).is_empty());
    }

    #[test]
    fn the_origin_group_is_one_update_whatever_its_size() {
        let names = ["aa", "bb", "cc", "dd"];
        let installed: HashMap<String, alpm::InstalledPkg> =
            names.iter().map(|n| (n.to_string(), pulled_in("1-1"))).collect();
        let sync = alpm::parse_sync_list(
            &names.iter().map(|n| format!("extra {n} 1-1\n")).collect::<String>(),
        );
        let printed: String = names.iter().map(|n| sp_line("extra", n, "1-1", EXTRA_DATE, ARCH)).collect();
        let mut ups = origin_case("", &installed, &sync, &Conf::default(), &printed);
        assert_eq!(ups.len(), 1, "{ups:#?}");
        let group = ups[0].clone();
        assert_eq!(group.origin_group.len(), 4);
        assert_eq!((group.old_version.as_str(), group.candidate_repo.as_str()), ("4 packages", "extra"));
        ups.push(Update {
            name: "mesa".into(),
            old_version: "1-1".into(),
            new_version: "2-1".into(),
            installed_repo: Some("extra".into()),
            candidate_repo: "extra".into(),
            transition: Transition::Same,
            explicit_target: false,
            origin_group: Vec::new(),
            ..group
        });
        let state = State { schema: 1, updates: ups, ..Default::default() };
        assert_eq!(state.total(), 2, "the group and mesa");
        assert_eq!(state.repo_change_count, 0);
    }

    #[test]
    fn the_origin_group_reinstalls_from_the_repo_whose_copy_matches() {
        // [cachyos] (first) has another build of that version, [extra] and
        // [minka] both the installed one: extra is the first that matches.
        let installed = HashMap::from([("zstd".to_string(), pulled_in("1.5.7-2"))]);
        let sync = alpm::parse_sync_list("cachyos zstd 1.5.7-2\nextra zstd 1.5.7-2\nminka zstd 1.5.7-2\n");
        let printed = format!(
            "{}{}{}",
            sp_line("cachyos", "zstd", "1.5.7-2", "Sun Nov  1 10:00:00 2026", "CachyOS <admin@cachyos.org>"),
            sp_line("extra", "zstd", "1.5.7-2", EXTRA_DATE, ARCH),
            sp_line("minka", "zstd", "1.5.7-2", EXTRA_DATE, ARCH)
        );
        let ups = origin_case("", &installed, &sync, &Conf::default(), &printed);
        assert_eq!(ups.len(), 1, "{ups:#?}");
        assert_eq!(ups[0].origin_group[0].repo, "extra");
        assert_eq!(ups[0].candidate_repo, "extra");
        // Members from several repositories: listed in pacman.conf order.
        let installed = HashMap::from([
            ("zstd".to_string(), pulled_in("1.5.7-2")),
            ("lib32-zstd".to_string(), pulled_in("1.5.7-2")),
        ]);
        let sync = alpm::parse_sync_list("extra zstd 1.5.7-2\nmultilib lib32-zstd 1.5.7-2\n");
        let printed = format!(
            "{}{}",
            sp_line("extra", "zstd", "1.5.7-2", EXTRA_DATE, ARCH),
            sp_line("multilib", "lib32-zstd", "1.5.7-2", EXTRA_DATE, ARCH)
        );
        let ups = origin_case("", &installed, &sync, &Conf::default(), &printed);
        assert_eq!(ups[0].candidate_repo, "extra, multilib");
        let targets: Vec<String> = ups[0].origin_group.iter().map(OriginMember::target).collect();
        assert_eq!(targets, ["multilib/lib32-zstd", "extra/zstd"]);
    }

    #[test]
    fn a_member_an_upgrade_conflicts_with_stays_out_of_the_group() {
        // Reproduced in a scratch root on 22/11/2026: app 2.0-1 conflicts
        // with mem, which app 1.0-1 pulled in. `-Su scratch/mem` aborts
        // ("unresolvable package conflicts"), while plain -Su asks to
        // remove mem.
        let installed = HashMap::from([
            ("app".to_string(), built("1.0-1", Some("scratch"), EXTRA_SECS, ARCH)),
            ("mem".to_string(), pulled_in("1.0-1")),
        ]);
        let sync = alpm::parse_sync_list("scratch app 2.0-1\nscratch mem 1.0-1\n");
        let printed = sp_line("scratch", "mem", "1.0-1", EXTRA_DATE, ARCH);
        let ups = origin_case_with(
            "app 1.0-1 -> 2.0-1\n",
            "app|2.0-1|scratch||mem|\n",
            &installed,
            &sync,
            &Conf::default(),
            &printed,
            "",
        );
        assert_eq!(names(&ups), ["app"], "{ups:#?}");
        // Without the conflict, mem is a member.
        let ups = origin_case_with(
            "app 1.0-1 -> 2.0-1\n",
            "app|2.0-1|scratch|||\n",
            &installed,
            &sync,
            &Conf::default(),
            &printed,
            "",
        );
        assert_eq!(names(&ups), [ORIGIN_GROUP, "app"], "{ups:#?}");
        // A conflict on something mem provides, with its version.
        let provider = HashMap::from([
            ("app".to_string(), built("1.0-1", Some("scratch"), EXTRA_SECS, ARCH)),
            ("mem".to_string(), alpm::InstalledPkg { provides: deps(&["libmem.so=1-64"]), ..pulled_in("1.0-1") }),
        ]);
        let conflicts = |c: &str| {
            let sup = format!("app|2.0-1|scratch||{c}|\n");
            let ups = origin_case_with("app 1.0-1 -> 2.0-1\n", &sup, &provider, &sync, &Conf::default(), &printed, "");
            ups.iter().any(Update::is_origin_group)
        };
        assert!(!conflicts("libmem.so"), "an unversioned conflict");
        assert!(!conflicts("libmem.so=1-64"));
        assert!(conflicts("libmem.so=2-64"), "another soname is no conflict");
        assert!(conflicts("mem<1.0"), "a constraint mem's version fails is none");
    }

    #[test]
    fn a_member_a_later_build_replaces_or_conflicts_with_stays_out() {
        // Reproduced on 22/11/2026: ax 1.0-1 from repoA, which has 1.1-1;
        // repoB has ax 2.0-1 with replaces=(zm) conflicts=(zm); zm came
        // from repoB unrecorded. ax sorts before zm, so libalpm's walk
        // takes repoA/ax first and -Sup -dd shows no replacement: only
        // repoB/ax as a target replaces zm, and with zm a target as well
        // pacman aborts.
        let installed = HashMap::from([
            ("ax".to_string(), built("1.0-1", Some("repoA"), EXTRA_SECS, ARCH)),
            ("zm".to_string(), pulled_in("1.0-1")),
        ]);
        let sync = alpm::parse_sync_list("repoA ax 1.1-1\nrepoB ax 2.0-1\nrepoB zm 1.0-1\n");
        let printed = sp_line("repoB", "zm", "1.0-1", EXTRA_DATE, ARCH);
        let case = |info: &str| {
            origin_case_with("ax 1.0-1 -> 1.1-1\n", "ax|1.1-1|repoA|||\n", &installed, &sync, &Conf::default(), &printed, info)
        };
        // -Syu upgrades ax from repoA, so the row is that upgrade, noting
        // repoB's build, which the window can still choose for it. That
        // build would remove zm, which the note says (pacman asks "Replace
        // zm with repoB/ax?", default yes) ...
        let ups = case("ax|repoB||zm|zm\n");
        assert_eq!(names(&ups), ["ax"], "{ups:#?}");
        let ax = &ups[0];
        assert!(!ax.explicit_target && ax.candidate_repo == "repoA", "{ax:#?}");
        assert_eq!(ax.newer_repo.as_deref(), Some("repoB"));
        assert!(ax.replaces.is_empty(), "the row's own build replaces nothing");
        assert_eq!(ax.newer_replaces.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["zm"]);
        assert_eq!(ax.newer_note().as_deref(), Some("newer build in repoB: 2.0-1, which replaces zm 1.0-1"));
        // ... so zm stays out of the group, as with an explicit row: by
        // that replacement alone, or by the conflict alone.
        let ups = case("ax|repoB||zm|\n");
        assert_eq!(names(&ups), ["ax"], "{ups:#?}");
        assert_eq!(ups[0].newer_replaces.len(), 1);
        let ups = case("ax|repoB|||zm\n");
        assert_eq!(names(&ups), ["ax"]);
        assert!(ups[0].newer_replaces.is_empty() && ups[0].newer_note().as_deref() == Some("newer build in repoB: 2.0-1"));
        // Neither: zm is a member.
        assert_eq!(names(&case("ax|repoB|||\n")), [ORIGIN_GROUP, "ax"]);

        // With nothing in repoA newer than ax 1.0-1, repoB/ax is a later-repo
        // row, whose replacement alone keeps zm out too: the row lists zm,
        // and the group is made after it.
        let sync = alpm::parse_sync_list("repoA ax 1.0-1\nrepoB ax 2.0-1\nrepoB zm 1.0-1\n");
        let case = |info: &str| origin_case_with("", "", &installed, &sync, &Conf::default(), &printed, info);
        let ups = case("ax|repoB||zm|zm\n");
        assert_eq!(names(&ups), ["ax"], "{ups:#?}");
        let ax = &ups[0];
        assert!(ax.explicit_target && ax.candidate_repo == "repoB", "{ax:#?}");
        assert_eq!(ax.replaces.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["zm"]);
        assert_eq!(names(&case("ax|repoB||zm|\n")), ["ax"]);
        assert_eq!(names(&case("ax|repoB|||zm\n")), ["ax"]);
        assert_eq!(names(&case("ax|repoB|||\n")), [ORIGIN_GROUP, "ax"]);
    }

    #[test]
    fn a_switch_an_upgrade_conflicts_with_is_dropped() {
        // lib32-libcaca as a switch; an upgrade that conflicts with it would
        // make its minka/lib32-libcaca target abort the whole -Syu.
        let mut installed = libcaca_installed();
        installed.insert("app".to_string(), built("1-1", Some("extra"), EXTRA_SECS, ARCH));
        let sync = alpm::parse_sync_list("extra app 2-1\nminka lib32-libcaca 0.99.beta20-2\n");
        let printed = sp_line("minka", "lib32-libcaca", "0.99.beta20-2", MINKA_LIBCACA, "Sophie <s@example.org>");
        let case = |sup: &str| {
            let ups = origin_case_with("app 1-1 -> 2-1\n", sup, &installed, &sync, &Conf::default(), &printed, "");
            ups.iter().any(|u| u.switch_build)
        };
        assert!(case("app|2-1|extra|||\n"));
        assert!(!case("app|2-1|extra||lib32-libcaca|\n"));

        // A later-repo build that replaces it and conflicts with it: no
        // switch, and that row lists it as replaced.
        let mut installed = libcaca_installed();
        installed.insert("xx".to_string(), built("1-1", Some("repoA"), EXTRA_SECS, ARCH));
        let info = "xx|repoB||lib32-libcaca|lib32-libcaca\n";
        let sync = alpm::parse_sync_list("repoA xx 1-1\nrepoB xx 2-1\nminka lib32-libcaca 0.99.beta20-2\n");
        let ups = origin_case_with("", "", &installed, &sync, &Conf::default(), &printed, info);
        assert_eq!(names(&ups), ["xx"], "{ups:#?}");
        assert!(ups[0].explicit_target);
        assert_eq!(ups[0].replaces.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["lib32-libcaca"]);
        // The same build noted on -Syu's own upgrade of xx (repoA has 1.1-1)
        // can still be chosen for it in the window: no switch either. The
        // row's own build replaces nothing; the noted one lists it.
        let sync = alpm::parse_sync_list("repoA xx 1.1-1\nrepoB xx 2-1\nminka lib32-libcaca 0.99.beta20-2\n");
        let ups =
            origin_case_with("xx 1-1 -> 1.1-1\n", "xx|1.1-1|repoA|||\n", &installed, &sync, &Conf::default(), &printed, info);
        assert_eq!(names(&ups), ["xx"], "{ups:#?}");
        assert!(!ups[0].explicit_target && ups[0].replaces.is_empty(), "{ups:#?}");
        assert_eq!(ups[0].newer_replaces.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["lib32-libcaca"]);
        assert_eq!(ups[0].newer_repo.as_deref(), Some("repoB"));
    }

    // ---- untracked packages ------------------------------------------------

    fn untracked_inputs<'a>(
        installed: &'a HashMap<String, alpm::InstalledPkg>,
        sync: &'a [alpm::SyncEntry],
        conf: &'a Conf,
    ) -> Inputs<'a> {
        Inputs {
            repo_lines: "",
            aur_lines: "",
            installed,
            sync_entries: sync,
            sysupgrade: Some(&[]),
            final_targets: None,
            conf,
        }
    }

    #[test]
    fn untracked_packages_are_in_no_repo_and_not_in_the_aur() {
        // 22/11/2026: gitcomet in no repository and not in the AUR; paru in
        // the AUR; python-async-timeout the AUR's 5.0.1-2, newer than
        // [extra]'s (paru never asks the AUR about it); libwpe needed by
        // wpebackend-fdo through its soname.
        let mut installed = HashMap::from([
            ("gitcomet".to_string(), pkg("0.2.5-1", None)),
            ("paru".to_string(), pkg("2.2.0-2", None)),
            (
                "python-async-timeout".to_string(),
                alpm::InstalledPkg { explicit: false, validation: vec!["none".into()], ..pkg("5.0.1-2", None) },
            ),
            // 22/11/2026: cachyos-extra-v4's signed 1.68-1.1, if that
            // repository dropped it while [extra] has 1.68-1: pacman tracks
            // it by name, so no error (and no advice to downgrade).
            (
                "perl-datetime".to_string(),
                alpm::InstalledPkg { explicit: false, validation: vec!["pgp".into()], ..pkg("1.68-1.1", None) },
            ),
            // [minka]'s checksummed build, pulled in with no record.
            (
                "coreutils-uutils".to_string(),
                alpm::InstalledPkg { validation: vec!["sha256".into()], ..pkg("0.4.0-2", None) },
            ),
            ("ripgrep".to_string(), pkg("15.1.0-1", Some("extra"))),
            // Recorded from [extra], which is past it: pacman tracks that.
            ("moved".to_string(), pkg("3-1", Some("extra"))),
            // A repository that dropped it, and nothing else has it.
            ("dropped".to_string(), pkg("1-1", Some("multilib"))),
            (
                "libwpe".to_string(),
                alpm::InstalledPkg {
                    explicit: false,
                    provides: deps(&["libwpe-1.0.so=1-64"]),
                    ..pkg("1.16.3-1.1", None)
                },
            ),
            (
                "wpebackend-fdo".to_string(),
                alpm::InstalledPkg {
                    explicit: false,
                    depends: deps(&["libwpe-1.0.so=1-64"]),
                    optdepends: deps(&["gitcomet"]),
                    ..pkg("1.16.1-1.1", None)
                },
            ),
        ]);
        let sync = alpm::parse_sync_list(
            "extra python-async-timeout 5.0.1-1\nextra ripgrep 15.1.0-1\nextra moved 2-1\nextra perl-datetime 1.68-1\n\
             minka coreutils-uutils 0.4.0-1\n",
        );
        let conf = Conf::default();
        let inputs = untracked_inputs(&installed, &sync, &conf);
        let candidates = untracked_candidates(&[], &inputs);
        let names: Vec<&str> = candidates.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["dropped", "gitcomet", "libwpe", "paru", "python-async-timeout", "wpebackend-fdo"]);

        let aur = HashMap::from([
            ("paru".to_string(), "2.2.0-2".to_string()),
            ("python-async-timeout".to_string(), "5.0.1-2".to_string()),
        ]);
        let (flagged, note) = flag_untracked(candidates.clone(), Ok(&aur));
        assert_eq!(note, None);
        let names: Vec<&str> = flagged.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["dropped", "gitcomet", "libwpe", "python-async-timeout", "wpebackend-fdo"]);
        let by = |n: &str| flagged.iter().find(|t| t.name == n).unwrap_or_else(|| panic!("{n} not flagged: {flagged:#?}"));
        let g = by("gitcomet");
        assert_eq!((g.kind.as_str(), g.explicit, g.required_by.len()), ("no_source", true, 0));
        assert_eq!(g.optional_for, ["wpebackend-fdo"]);
        assert!(g.hint.starts_with("no repository or AUR package provides it"), "{}", g.hint);
        let p = by("python-async-timeout");
        assert_eq!((p.kind.as_str(), p.explicit), ("other_build", false));
        assert_eq!((p.repo_versions.as_slice(), p.aur_version.as_deref()), (&["extra 5.0.1-1".to_string()][..], Some("5.0.1-2")));
        assert!(p.hint.starts_with("extra has 5.0.1-1, not this build; the AUR has 5.0.1-2, but paru never"), "{}", p.hint);
        assert_eq!(by("libwpe").required_by, ["wpebackend-fdo"]);
        assert!(by("dropped").hint.starts_with("multilib no longer has it"), "{}", by("dropped").hint);
        // Not the AUR's either: a file of unknown origin.
        let (flagged, _) = flag_untracked(candidates.clone(), Ok(&HashMap::new()));
        let p = flagged.iter().find(|t| t.name == "python-async-timeout").expect("flagged");
        assert!(p.aur_version.is_none() && p.hint.contains("installed from a file with no signature"), "{}", p.hint);
        // Without its %VALIDATION% (or with a signature) the same package
        // is pacman's to track: no candidate.
        let mut signed = installed.clone();
        signed.insert("python-async-timeout".into(), alpm::InstalledPkg { explicit: false, ..pkg("5.0.1-2", None) });
        let inputs_signed = untracked_inputs(&signed, &sync, &conf);
        assert!(untracked_candidates(&[], &inputs_signed).iter().all(|c| c.name != "python-async-timeout"));

        // The AUR could not be asked: nothing in no repository is flagged
        // (paru may well track it), and the note says why. What a
        // repository has the name of is flagged still: the AUR's answer
        // could not have cleared it.
        let (flagged, note) = flag_untracked(candidates, Err("curl: (6) Could not resolve host"));
        let names: Vec<&str> = flagged.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["python-async-timeout"]);
        let note = note.expect("a note");
        assert!(note.starts_with("AUR unreachable") && note.contains("5 packages in no repository were not checked"), "{note}");
        // With no candidates there is nothing to ask about.
        assert_eq!(flag_untracked(Vec::new(), Err("down")), (Vec::new(), None));

        // A package a row touches is the list's already: an AUR update, a
        // replaced package.
        installed.insert("old".to_string(), pkg("1-1", None));
        let inputs = untracked_inputs(&installed, &sync, &conf);
        let mut row = Update {
            name: "gitcomet".into(),
            old_version: "0.2.5-1".into(),
            new_version: "0.2.6-1".into(),
            installed_repo: Some("aur".into()),
            candidate_repo: "aur".into(),
            transition: Transition::Same,
            explicit_target: false,
            first_repo: None,
            first_version: None,
            newer_repo: None,
            newer_version: None,
            newer_replaces: Vec::new(),
            replaces: vec![Replaced { name: "old".into(), version: "1-1".into(), installed_repo: None }],
            held: false,
            held_by: None,
            superseded_by: Vec::new(),
            new_install: false,
            reinstalls: Vec::new(),
            switch_build: false,
            origin_group: Vec::new(),
        };
        let names = |rows: &[Update]| -> Vec<String> {
            untracked_candidates(rows, &inputs).into_iter().map(|c| c.name).collect()
        };
        assert!(!names(std::slice::from_ref(&row)).iter().any(|n| n == "gitcomet" || n == "old"));
        row.replaces.clear();
        assert!(names(&[row]).iter().any(|n| n == "old"));
    }

    #[test]
    fn aur_answers_parse() {
        let ok = r#"{"resultcount":1,"results":[{"Name":"paru","Version":"2.2.0-1"}],"type":"multiinfo","version":5}"#;
        assert_eq!(parse_aur_info(ok), Ok(HashMap::from([("paru".to_string(), "2.2.0-1".to_string())])));
        let none = r#"{"resultcount":0,"results":[],"type":"multiinfo","version":5}"#;
        assert_eq!(parse_aur_info(none), Ok(HashMap::new()));
        let err = r#"{"error":"Too many package results.","resultcount":0,"results":[],"type":"error","version":5}"#;
        assert_eq!(parse_aur_info(err), Err("the AUR answered: Too many package results.".to_string()));
        assert!(parse_aur_info("<html>502</html>").is_err());
        assert!(parse_aur_info("{}").is_err());
        assert_eq!(
            aur_info_form(&["libsigc++".into(), "python-async-timeout".into(), "a@b".into()]),
            "arg%5B%5D=libsigc%2B%2B&arg%5B%5D=python-async-timeout&arg%5B%5D=a%40b"
        );
    }
}
