//! Browsing: searching the sync repos + AUR, and listing what is
//! installed. Read-only, like the checker — installing and removing
//! stay paru's job in a terminal; this module only produces the lists
//! the UI selects from.

use crate::alpm;
use crate::check;
use serde::Serialize;
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
    fn escapes_regex_terms() {
        assert_eq!(escape_regex("libsigc++"), "libsigc\\+\\+");
        assert_eq!(escape_regex("gtk4"), "gtk4");
        assert_eq!(escape_regex("a.b[c]"), "a\\.b\\[c\\]");
    }
}
