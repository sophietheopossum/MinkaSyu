//! Browsing: searching the sync repos + AUR, and listing what is
//! installed. Read-only, like the checker — installing and removing
//! stay paru's job in a terminal; this module only produces the lists
//! the UI selects from.

use crate::alpm;
use crate::check;
use serde::Serialize;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;

/// One search hit. `installed_version` comes from the local DB, so a
/// hit in another repo at another version still reports what is
/// actually installed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SearchResult {
    pub name: String,
    pub repo: String,
    pub version: String,
    pub description: String,
    pub installed_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SearchResponse {
    pub results: Vec<SearchResult>,
    /// True when the AUR or total cap cut results off; refine the query.
    pub truncated: bool,
}

/// One concrete build of a package offered by a sync repo (or the
/// AUR), for the repository picker. Options are in pacman.conf order
/// with the AUR last; `target` is the argv form (`repo/name`, AUR
/// included) that pins exactly this build.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RepoOption {
    pub repo: String,
    pub name: String,
    pub version: String,
    pub architecture: String,
    pub description: String,
    pub target: String,
    /// The first sync option: the one plain pacman would choose.
    pub is_default: bool,
    /// This exact repo+version is what is installed.
    pub is_installed: bool,
    /// Highest version among the options (libalpm ordering).
    pub is_newest: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InstalledEntry {
    pub name: String,
    pub version: String,
    pub description: String,
    /// Origin repo from the local DB. "foreign" when the package is in
    /// no sync repo (AUR or hand-built), "unknown" when it is in a sync
    /// repo but its origin was never recorded.
    pub repo: String,
    pub explicit: bool,
    pub size_bytes: u64,
}

const AUR_CAP: usize = 100;
const TOTAL_CAP: usize = 250;

/// Escape a literal search term for pacman/paru, which treat search
/// arguments as regexes. Users type substrings, not patterns.
pub fn escape_regex(term: &str) -> String {
    let mut out = String::with_capacity(term.len());
    for c in term.chars() {
        if matches!(c, '.' | '+' | '*' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Parse `pacman -Ss` / `paru -Ss` output: a header line
/// `repo/name version [annotations]` followed by indented description
/// lines. Annotations (sizes, votes, installed markers) are ignored;
/// installed versions are taken from the local DB map instead, which
/// is exact even when the hit is from a different repo.
pub fn parse_search_output(
    text: &str,
    installed: &HashMap<String, alpm::InstalledPkg>,
) -> Vec<SearchResult> {
    let mut out: Vec<SearchResult> = Vec::new();
    for line in text.lines() {
        if line.starts_with(|c: char| c.is_whitespace()) {
            let desc = line.trim();
            if !desc.is_empty() {
                if let Some(last) = out.last_mut() {
                    if !last.description.is_empty() {
                        last.description.push(' ');
                    }
                    last.description.push_str(desc);
                }
            }
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(first), Some(version)) = (parts.next(), parts.next()) else {
            continue;
        };
        let Some((repo, name)) = first.split_once('/') else {
            continue;
        };
        out.push(SearchResult {
            installed_version: installed.get(name).map(|p| p.version.clone()),
            name: name.to_string(),
            repo: repo.to_string(),
            version: version.to_string(),
            description: String::new(),
        });
    }
    out
}

/// Capture stdout, treating exit 1 as "no matches" (pacman's search
/// convention). Any other failure is an error.
fn capture_search(program: &str, args: &[String]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("running {program}: {e}"))?;
    if output.status.success() || output.status.code() == Some(1) {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "{program} search failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).lines().last().unwrap_or("")
        ))
    }
}

/// Search sync repos against the checker's refreshed temp DB when one
/// exists (so results match what a check would offer, not whatever
/// the system DB last saw), and the AUR through paru. A paru failure
/// degrades to repo-only results rather than failing the search.
pub fn run_search(query: &str) -> Result<SearchResponse, String> {
    let terms: Vec<String> = query.split_whitespace().map(escape_regex).collect();
    if terms.is_empty() {
        return Err("empty search query".to_string());
    }
    let installed = alpm::read_installed(Path::new("/var/lib/pacman/local"));

    let mut repo_args: Vec<String> = vec!["-Ss".into(), "--color".into(), "never".into()];
    let checkdb = check::checkdb_path();
    if checkdb.join("sync").exists() {
        if let Some(s) = checkdb.to_str() {
            repo_args.push("--dbpath".into());
            repo_args.push(s.to_string());
        }
    }
    repo_args.push("--".into());
    repo_args.extend(terms.iter().cloned());
    let repo_text = capture_search("pacman", &repo_args)?;
    let mut results = parse_search_output(&repo_text, &installed);

    let mut aur_args: Vec<String> = vec!["-Ssa".into(), "--color".into(), "never".into(), "--".into()];
    aur_args.extend(terms.iter().cloned());
    let mut truncated = false;
    match capture_search("paru", &aur_args) {
        Ok(aur_text) => {
            let mut aur = parse_search_output(&aur_text, &installed);
            if aur.len() > AUR_CAP {
                aur.truncate(AUR_CAP);
                truncated = true;
            }
            results.extend(aur);
        }
        Err(e) => {
            eprintln!("minka-syu: AUR search unavailable, repo results only: {e}");
        }
    }
    if results.len() > TOTAL_CAP {
        results.truncate(TOTAL_CAP);
        truncated = true;
    }
    Ok(SearchResponse { results, truncated })
}

/// Everything installed, from the local DB, with origin labels.
pub fn run_installed() -> Vec<InstalledEntry> {
    let installed = alpm::read_installed(Path::new("/var/lib/pacman/local"));
    let sync_names: HashSet<String> = sync_entries()
        .into_iter()
        .map(|e| e.name)
        .collect();
    let mut out: Vec<InstalledEntry> = installed
        .into_iter()
        .map(|(name, pkg)| {
            let repo = pkg.repo.clone().unwrap_or_else(|| {
                if sync_names.contains(&name) {
                    "unknown".to_string()
                } else {
                    "foreign".to_string()
                }
            });
            InstalledEntry {
                name,
                version: pkg.version,
                description: pkg.description,
                repo,
                explicit: pkg.explicit,
                size_bytes: pkg.size_bytes,
            }
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Parse `pacman -Si` output into (repo, name) -> (architecture,
/// description). Blocks are `Key : value` lines separated by blanks;
/// only the fields we show are kept.
pub fn parse_sync_info(text: &str) -> HashMap<(String, String), (String, String)> {
    let mut out = HashMap::new();
    let mut repo = String::new();
    let mut name = String::new();
    let mut arch = String::new();
    let mut desc = String::new();
    let flush = |repo: &str, name: &str, arch: &str, desc: &str, out: &mut HashMap<(String, String), (String, String)>| {
        if !repo.is_empty() && !name.is_empty() {
            out.insert((repo.to_string(), name.to_string()), (arch.to_string(), desc.to_string()));
        }
    };
    for line in text.lines() {
        if line.trim().is_empty() {
            flush(&repo, &name, &arch, &desc, &mut out);
            repo.clear();
            name.clear();
            arch.clear();
            desc.clear();
            continue;
        }
        let Some((key, value)) = line.split_once(':') else { continue };
        let value = value.trim();
        match key.trim() {
            "Repository" => repo = value.to_string(),
            "Name" => name = value.to_string(),
            "Architecture" => arch = value.to_string(),
            "Description" => desc = value.to_string(),
            _ => {}
        }
    }
    flush(&repo, &name, &arch, &desc, &mut out);
    out
}

/// Mark the highest version(s) in place; first option wins ties so a
/// list has exactly one `is_newest` unless there are no options.
fn mark_newest(options: &mut [RepoOption], cmp: &dyn Fn(&str, &str) -> Ordering) {
    if options.is_empty() {
        return;
    }
    let mut best = 0;
    for i in 1..options.len() {
        if cmp(&options[i].version, &options[best].version) == Ordering::Greater {
            best = i;
        }
    }
    options[best].is_newest = true;
}

/// Assemble picker options from sync entries (already in rank order),
/// optional -Si details, exact AUR hits, and the installed map. Pure
/// apart from the injected version comparison.
pub fn build_repo_options(
    package: &str,
    sync: &[alpm::SyncEntry],
    info: &HashMap<(String, String), (String, String)>,
    aur_hits: &[SearchResult],
    installed: &HashMap<String, alpm::InstalledPkg>,
    cmp: &dyn Fn(&str, &str) -> Ordering,
) -> Vec<RepoOption> {
    let installed_pkg = installed.get(package);
    let is_installed = |repo: &str, version: &str| -> bool {
        let Some(pkg) = installed_pkg else { return false };
        if pkg.version != version {
            return false;
        }
        match &pkg.repo {
            Some(origin) => origin == repo,
            // No recorded origin and not offered by any sync repo:
            // it can only have come from the AUR / a local build.
            None => repo == "aur" && !sync.iter().any(|e| e.name == package),
        }
    };

    let mut out: Vec<RepoOption> = Vec::new();
    for (idx, entry) in sync.iter().filter(|e| e.name == package).enumerate() {
        let (arch, desc) = info
            .get(&(entry.repo.clone(), entry.name.clone()))
            .cloned()
            .unwrap_or_default();
        out.push(RepoOption {
            target: format!("{}/{}", entry.repo, entry.name),
            is_default: idx == 0,
            is_installed: is_installed(&entry.repo, &entry.version),
            is_newest: false,
            repo: entry.repo.clone(),
            name: entry.name.clone(),
            version: entry.version.clone(),
            architecture: arch,
            description: desc,
        });
    }
    for hit in aur_hits.iter().filter(|h| h.name == package && h.repo == "aur") {
        out.push(RepoOption {
            target: format!("aur/{}", hit.name),
            is_default: false,
            is_installed: is_installed("aur", &hit.version),
            is_newest: false,
            repo: "aur".to_string(),
            name: hit.name.clone(),
            version: hit.version.clone(),
            architecture: String::new(),
            description: hit.description.clone(),
        });
    }
    mark_newest(&mut out, cmp);
    out
}

/// Architecture + description for a handful of repo/name targets via
/// one `pacman -Si` call; empty on any failure (the picker still
/// works from the sync list alone).
fn sync_info(targets: &[String]) -> HashMap<(String, String), (String, String)> {
    if targets.is_empty() {
        return HashMap::new();
    }
    let mut args: Vec<String> = vec!["-Si".into(), "--color".into(), "never".into()];
    let checkdb = check::checkdb_path();
    if checkdb.join("sync").exists() {
        if let Some(s) = checkdb.to_str() {
            args.push("--dbpath".into());
            args.push(s.to_string());
        }
    }
    args.push("--".into());
    args.extend(targets.iter().cloned());
    capture_search("pacman", &args).map(|t| parse_sync_info(&t)).unwrap_or_default()
}

/// Every build of one package the repos (+ AUR) currently offer, in
/// rank order, for the repository picker.
pub fn run_package_repos(package: &str) -> Vec<RepoOption> {
    let installed = alpm::read_installed(Path::new("/var/lib/pacman/local"));
    let sync = sync_entries();
    let targets: Vec<String> = sync
        .iter()
        .filter(|e| e.name == package)
        .map(|e| format!("{}/{}", e.repo, e.name))
        .collect();
    let info = sync_info(&targets);

    let exact = format!("^{}$", escape_regex(package));
    let aur_hits = capture_search(
        "paru",
        &["-Ssa".into(), "--color".into(), "never".into(), "--".into(), exact],
    )
    .map(|t| parse_search_output(&t, &installed))
    .unwrap_or_default();

    build_repo_options(package, &sync, &info, &aur_hits, &installed, &check::system_vercmp)
}

fn sync_entries() -> Vec<alpm::SyncEntry> {
    let checkdb = check::checkdb_path();
    let args: Vec<String> = if checkdb.join("sync").exists() {
        match checkdb.to_str() {
            Some(s) => vec!["-Sl".into(), "--dbpath".into(), s.to_string()],
            None => vec!["-Sl".into()],
        }
    } else {
        vec!["-Sl".into()]
    };
    capture_search("pacman", &args)
        .map(|t| alpm::parse_sync_list(&t))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installed_map() -> HashMap<String, alpm::InstalledPkg> {
        HashMap::from([(
            "fastfetch".to_string(),
            alpm::InstalledPkg {
                version: "2.70.0-1.1".into(),
                repo: Some("cachyos-extra-v4".into()),
                description: String::new(),
                size_bytes: 0,
                explicit: true,
            },
        )])
    }

    #[test]
    fn parses_repo_and_aur_search_output() {
        let text = "\
cachyos-extra-v4/fastfetch 2.70.0-1.1 [0 B 2.01 MiB] [installed]\n    A fast system information tool\n    second description line\nextra/fastfetch 2.70.0-1 [664.76 KiB 2.01 MiB] [installed: 2.70.0-1.1]\n    The same, from extra\naur/fastfetch-git 2.66.0.r15-1 [+13 ~0.00]\n    Git build\nnot a result line\n";
        let hits = parse_search_output(text, &installed_map());
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].repo, "cachyos-extra-v4");
        assert_eq!(hits[0].name, "fastfetch");
        assert_eq!(hits[0].version, "2.70.0-1.1");
        assert_eq!(hits[0].description, "A fast system information tool second description line");
        assert_eq!(hits[0].installed_version.as_deref(), Some("2.70.0-1.1"));
        assert_eq!(hits[1].repo, "extra");
        assert_eq!(hits[1].installed_version.as_deref(), Some("2.70.0-1.1"));
        assert_eq!(hits[2].repo, "aur");
        assert_eq!(hits[2].installed_version, None);
    }

    #[test]
    fn builds_repo_options_in_rank_order() {
        let sync = alpm::parse_sync_list(
            "cachyos-extra-v4 bcachefs-dkms 3:1.39.7-1.1\n\
             extra bcachefs-dkms 3:1.39.7-1\n\
             minka bcachefs-dkms 3:1.39.7-2\n",
        );
        let info = parse_sync_info(
            "Repository      : minka\nName            : bcachefs-dkms\nArchitecture    : x86_64\nDescription     : bcachefs DKMS\n\n",
        );
        let installed = HashMap::from([(
            "bcachefs-dkms".to_string(),
            alpm::InstalledPkg {
                version: "3:1.39.7-1.1".into(),
                repo: Some("cachyos-extra-v4".into()),
                description: String::new(),
                size_bytes: 0,
                explicit: true,
            },
        )]);
        let opts = build_repo_options("bcachefs-dkms", &sync, &info, &[], &installed, &|a, b| a.cmp(b));
        assert_eq!(opts.len(), 3);
        assert!(opts[0].is_default && opts[0].is_installed && !opts[0].is_newest);
        assert_eq!(opts[0].target, "cachyos-extra-v4/bcachefs-dkms");
        assert!(opts[2].is_newest && !opts[2].is_default);
        assert_eq!(opts[2].architecture, "x86_64");
        assert_eq!(opts[2].target, "minka/bcachefs-dkms");
    }

    #[test]
    fn escapes_regex_terms() {
        assert_eq!(escape_regex("libsigc++"), "libsigc\\+\\+");
        assert_eq!(escape_regex("gtk4"), "gtk4");
        assert_eq!(escape_regex("a.b[c]"), "a\\.b\\[c\\]");
    }
}
