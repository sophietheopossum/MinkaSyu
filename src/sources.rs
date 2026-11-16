//! Sources: the packages Sophie builds herself (MinkaPKG), read through
//! `minkarepo`. Checking upstream, bumping and publishing are minkarepo's
//! job; this module only runs it and hands its answer to the UI.
//!
//! - `minkarepo check --tsv` gives one row per package (PKGBUILD version,
//!   the version in the [minka] repo, upstream's newest, a status).
//! - `minkarepo track <pkg> yes|no` marks a package as one she does not
//!   care to follow; check then skips its network round trip.
//! - `minkarepo update <pkg>...` runs in a terminal like paru does: builds
//!   can take hours, and makepkg -s may ask for a sudo password.
//!
//! The last answer is cached in the state dir, so the tab opens on it
//! instantly and says how old it is.

use crate::check::state_dir;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SourceRow {
    /// The package directory in MinkaPKG (what minkarepo takes).
    pub package: String,
    /// github-release | github-tag | git-commit | local | follows |
    /// watch-package, or "" without an upstream file.
    pub kind: String,
    /// `update --all` may bump it unasked.
    pub auto: bool,
    pub tracked: bool,
    /// [epoch:]pkgver-pkgrel as the PKGBUILD says.
    pub pkgbuild: String,
    /// The same in the repo database, "" when not published.
    pub published: String,
    /// Upstream's newest, in pkgver form ("" when unknown).
    pub upstream: String,
    /// git-commit packages: upstream's branch head.
    pub commit: String,
    /// current | update | update-by-hand | watch | publish | older |
    /// untracked | error
    pub status: String,
    pub note: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Sources {
    /// A minkarepo was found; false hides the tab.
    pub available: bool,
    pub minkarepo: String,
    /// Unix seconds of the check these rows came from (0: none yet).
    pub checked_at: i64,
    pub rows: Vec<SourceRow>,
}

/// $MINKA_SYU_MINKAREPO, `minkarepo` on PATH, then ~/.local/bin (a
/// session started by the compositor may not have it on PATH).
pub fn find_minkarepo() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("MINKA_SYU_MINKAREPO").filter(|v| !v.is_empty()) {
        let p = PathBuf::from(p);
        return is_executable(&p).then_some(p);
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let p = dir.join("minkarepo");
            if is_executable(&p) {
                return Some(p);
            }
        }
    }
    let home = std::env::var_os("HOME")?;
    let p = PathBuf::from(home).join(".local/bin/minkarepo");
    is_executable(&p).then_some(p)
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn cache_path() -> PathBuf {
    state_dir().join("sources.json")
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// One `check --tsv` line: dir, kind, auto, track, PKGBUILD version,
/// published, upstream, commit, status, note, error.
pub fn parse_tsv_line(line: &str) -> Option<SourceRow> {
    let f: Vec<&str> = line.split('\t').collect();
    if f.len() < 11 || f[0].is_empty() {
        return None;
    }
    Some(SourceRow {
        package: f[0].to_string(),
        kind: f[1].to_string(),
        auto: f[2] == "yes",
        tracked: f[3] != "no",
        pkgbuild: f[4].to_string(),
        published: f[5].to_string(),
        upstream: f[6].to_string(),
        commit: f[7].to_string(),
        status: f[8].to_string(),
        note: f[9].to_string(),
        // A note or error may itself hold a tab; keep the rest whole.
        error: f[10..].join("\t"),
    })
}

fn run_check_tsv(minkarepo: &Path, packages: &[String]) -> Result<Vec<SourceRow>, String> {
    let out = Command::new(minkarepo)
        .arg("check")
        .arg("--tsv")
        .args(packages)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("running {}: {e}", minkarepo.display()))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "minkarepo check failed: {}",
            err.lines().last().unwrap_or("no message")
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(parse_tsv_line)
        .collect())
}

pub fn load_cache() -> Option<Sources> {
    let text = std::fs::read_to_string(cache_path()).ok()?;
    serde_json::from_str(&text).ok()
}

fn save_cache(s: &Sources) {
    let dir = state_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let tmp = dir.join("sources.json.tmp");
    if let Ok(json) = serde_json::to_string_pretty(s) {
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, cache_path());
        }
    }
}

/// The cached answer (refresh = false, when there is one) or a fresh
/// `minkarepo check` (network: tens of seconds).
pub fn sources(refresh: bool) -> Result<Sources, String> {
    let Some(minkarepo) = find_minkarepo() else {
        return Ok(Sources::default());
    };
    let path = minkarepo.display().to_string();
    if !refresh {
        if let Some(mut cached) = load_cache() {
            cached.available = true;
            cached.minkarepo = path;
            return Ok(cached);
        }
    }
    let rows = run_check_tsv(&minkarepo, &[])?;
    let fresh = Sources {
        available: true,
        minkarepo: path,
        checked_at: now_unix(),
        rows,
    };
    save_cache(&fresh);
    Ok(fresh)
}

/// `minkarepo track <pkg> yes|no`, then that one package checked again,
/// so its row (and the cache) shows the new state without a full check.
pub fn set_tracked(package: &str, tracked: bool) -> Result<Sources, String> {
    let minkarepo = find_minkarepo().ok_or("no minkarepo found")?;
    let out = Command::new(&minkarepo)
        .args(["track", package, if tracked { "yes" } else { "no" }])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("running {}: {e}", minkarepo.display()))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(err.lines().last().unwrap_or("minkarepo track failed").to_string());
    }
    let fresh = run_check_tsv(&minkarepo, &[package.to_string()])?;
    let mut all = load_cache().unwrap_or_default();
    all.available = true;
    all.minkarepo = minkarepo.display().to_string();
    for row in fresh {
        match all.rows.iter_mut().find(|r| r.package == row.package) {
            Some(slot) => *slot = row,
            None => all.rows.push(row),
        }
    }
    save_cache(&all);
    Ok(all)
}

/// The terminal argv for `minkarepo update <pkg>...`. The shell keeps the
/// window open at the end: the run's last lines (what failed, the
/// re-check reminders) are the point of watching it.
pub fn update_argv(terminal: &str, minkarepo: &Path, packages: &[String]) -> Vec<String> {
    let script = "m=$1; shift; \"$m\" update \"$@\"; s=$?; \
                  printf '\\n==> minkarepo update exited %s. Press Enter to close. ' \"$s\"; read -r _";
    let mut argv = vec![
        terminal.to_string(),
        "-e".into(),
        "sh".into(),
        "-c".into(),
        script.into(),
        "minkarepo-update".into(),
        minkarepo.display().to_string(),
    ];
    argv.extend(packages.iter().cloned());
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_check_tsv() {
        let row = parse_tsv_line(
            "lxmf-rs\tgithub-tag\tyes\tyes\t0.9.8-3\t0.9.8-3\t0.16.1\t\tupdate\tre-check depends\t",
        )
        .unwrap();
        assert_eq!(row.package, "lxmf-rs");
        assert!(row.auto && row.tracked);
        assert_eq!(row.upstream, "0.16.1");
        assert_eq!(row.status, "update");
        assert_eq!(row.note, "re-check depends");
        assert_eq!(row.error, "");

        let off = parse_tsv_line("sheetbro-bin\tgithub-release\tno\tno\t0.1.0-1\t0.1.0-1\t\t\tuntracked\t\t").unwrap();
        assert!(!off.tracked && !off.auto);

        // An unreadable PKGBUILD: everything but the name and error empty.
        let bad = parse_tsv_line("broken\t\t\t\t\t\t\t\terror\t\tcannot read its PKGBUILD").unwrap();
        assert_eq!(bad.status, "error");
        assert_eq!(bad.error, "cannot read its PKGBUILD");

        assert!(parse_tsv_line("").is_none());
        assert!(parse_tsv_line("too\tfew").is_none());
    }

    #[test]
    fn update_argv_keeps_names_as_arguments() {
        let argv = update_argv("rio", Path::new("/home/x/.local/bin/minkarepo"), &["floorp".into(), "lxmf-rs".into()]);
        assert_eq!(&argv[..4], ["rio", "-e", "sh", "-c"]);
        assert_eq!(argv[5], "minkarepo-update");
        assert_eq!(&argv[6..], ["/home/x/.local/bin/minkarepo", "floorp", "lxmf-rs"]);
    }
}
