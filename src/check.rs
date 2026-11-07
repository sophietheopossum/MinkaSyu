//! The checker: refresh sync DBs into a throwaway copy (the checkupdates
//! trick, so a check alone can never leave partial-upgrade state), list
//! repo updates from it and AUR updates from paru, and stamp each with
//! repository provenance — installed origin vs candidate origin.

use crate::alpm;
use serde::{Deserialize, Serialize};
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

/// Merge repo updates (with sync-DB candidate mapping) and AUR updates
/// into one provenance-stamped list. Pure: all IO happens in run_check.
pub fn merge_updates(
    repo_lines: &str,
    aur_lines: &str,
    installed: &HashMap<String, alpm::InstalledPkg>,
    sync_entries: &[alpm::SyncEntry],
) -> Vec<Update> {
    let in_sync: HashSet<&str> = sync_entries.iter().map(|e| e.name.as_str()).collect();
    let mut updates = Vec::new();

    for line in repo_lines.lines() {
        let Some((name, old, new)) = parse_update_line(line) else {
            continue;
        };
        let candidate_repo =
            alpm::candidate_repo(sync_entries, &name, &new).unwrap_or("unknown").to_string();
        let installed_repo = installed.get(&name).and_then(|p| p.repo.clone());
        updates.push(Update {
            transition: transition(&installed_repo, &candidate_repo),
            name,
            old_version: old,
            new_version: new,
            installed_repo,
            candidate_repo,
        });
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
    let db = cache_dir().join("checkdb");
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
                },
            ),
            (
                "gitkraken".to_string(),
                alpm::InstalledPkg { version: "12.6.1-1".into(), repo: None },
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
    fn update_line_parsing() {
        assert_eq!(
            parse_update_line("linux 6.1-1 -> 6.2-1"),
            Some(("linux".into(), "6.1-1".into(), "6.2-1".into()))
        );
        assert_eq!(parse_update_line(":: some notice"), None);
        assert_eq!(parse_update_line(""), None);
    }
}

