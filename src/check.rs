//! The checker: refresh sync DBs into a throwaway copy (the checkupdates
//! trick, so a check alone can never leave partial-upgrade state), list
//! repo updates from it and AUR updates from paru, and stamp each with
//! repository provenance — installed origin vs candidate origin.

use crate::alpm;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

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
/// `pacman -Qu` and `paru -Qua`). Anything else returns None.
pub fn parse_update_line(line: &str) -> Option<(String, String, String)> {
    let mut parts = line.split_whitespace();
    let name = parts.next()?;
    let old = parts.next()?;
    if parts.next()? != "->" {
        return None;
    }
    let new = parts.next()?;
    Some((name.to_string(), old.to_string(), new.to_string()))
}

/// libalpm's version comparison, via the `vercmp` tool that ships
/// with pacman. Returns Equal if the tool cannot be run: the custom
/// cross-repository scan then simply finds nothing, rather than
/// inventing an ordering with string comparison.
fn system_vercmp(a: &str, b: &str) -> Ordering {
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

/// Merge repo updates (with sync-DB candidate mapping) and AUR updates
/// into one provenance-stamped list.
///
/// Standard updates come from `pacman -Qu`, preserving libalpm's own
/// handling of ignores, replacements, and architecture. On top of
/// that, a cross-repository scan promotes a strictly newer version
/// from a lower-ranked repository: it replaces the standard
/// candidate when there is one, and adds an update pacman would
/// never offer when the first repository's copy is not newer than
/// the installed package. A package pacman withheld despite its
/// first repository being newer (IgnorePkg and friends) is left
/// alone. Everything except the injected version comparison is pure.
pub fn merge_updates(
    repo_lines: &str,
    aur_lines: &str,
    installed: &HashMap<String, alpm::InstalledPkg>,
    sync_entries: &[alpm::SyncEntry],
) -> Vec<Update> {
    let mut cache: HashMap<(String, String), Ordering> = HashMap::new();
    merge_updates_with_cmp(repo_lines, aur_lines, installed, sync_entries, &mut |a, b| {
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
        });
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
    let updates = merge_updates(&repo_lines, &aur_lines, &installed, &sync_entries);
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
        let ups = merge_updates_with_cmp("", "", &installed_map(), &sync, &mut |a, b| a.cmp(b));
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
        let ups = merge_updates_with_cmp("", "", &installed_map(), &sync, &mut |a, b| a.cmp(b));
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
    }
}

