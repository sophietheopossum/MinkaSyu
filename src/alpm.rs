//! Readers for ALPM's on-disk databases, narrow enough to stay honest:
//! the local DB for what is installed (and which repo it came from), and
//! `pacman -Sl` output for what the sync DBs offer, in pacman.conf order.

use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub struct InstalledPkg {
    pub version: String,
    /// The sync repo the package was installed from (`%INSTALLED_DB%`).
    /// Absent for packages installed from files (most AUR/manual builds)
    /// and for anything installed before libalpm recorded it.
    pub repo: Option<String>,
}

/// Parse one local-DB `desc` file into (name, version, installed_db).
/// Format: `%FIELD%` line, value line(s), blank line, repeated.
pub fn parse_desc(text: &str) -> (Option<String>, Option<String>, Option<String>) {
    // desc files are small; collect field -> first value line directly.
    let mut name = None;
    let mut version = None;
    let mut repo = None;
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let target = match line.trim() {
            "%NAME%" => &mut name,
            "%VERSION%" => &mut version,
            "%INSTALLED_DB%" => &mut repo,
            _ => continue,
        };
        if let Some(value) = lines.next() {
            let value = value.trim();
            if !value.is_empty() {
                *target = Some(value.to_string());
            }
        }
    }
    (name, version, repo)
}

/// Read every installed package's version + origin repo from the local DB.
pub fn read_installed(local_db: &Path) -> HashMap<String, InstalledPkg> {
    let mut out = HashMap::new();
    let Ok(entries) = std::fs::read_dir(local_db) else {
        return out;
    };
    for entry in entries.flatten() {
        let desc_path = entry.path().join("desc");
        let Ok(text) = std::fs::read_to_string(&desc_path) else {
            continue;
        };
        let (name, version, repo) = parse_desc(&text);
        if let (Some(name), Some(version)) = (name, version) {
            out.insert(name, InstalledPkg { version, repo });
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq)]
pub struct SyncEntry {
    pub repo: String,
    pub name: String,
    pub version: String,
}

/// Parse `pacman -Sl` output: `repo name version [installed...]`,
/// in pacman.conf repository order.
pub fn parse_sync_list(text: &str) -> Vec<SyncEntry> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        if let (Some(repo), Some(name), Some(version)) = (parts.next(), parts.next(), parts.next())
        {
            out.push(SyncEntry {
                repo: repo.to_string(),
                name: name.to_string(),
                version: version.to_string(),
            });
        }
    }
    out
}

/// The repo libalpm would take `name` at exactly `version` from: the first
/// entry in pacman.conf order carrying that version. Falls back to the
/// first entry carrying the name at all, then None.
pub fn candidate_repo<'a>(entries: &'a [SyncEntry], name: &str, version: &str) -> Option<&'a str> {
    entries
        .iter()
        .find(|e| e.name == name && e.version == version)
        .or_else(|| entries.iter().find(|e| e.name == name))
        .map(|e| e.repo.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desc_with_installed_db() {
        let text = "%NAME%\nbcachefs-dkms\n\n%VERSION%\n3:1.39.7-1.1\n\n%INSTALLED_DB%\ncachyos-extra-v4\n\n%DESC%\nx\n";
        let (name, version, repo) = parse_desc(text);
        assert_eq!(name.as_deref(), Some("bcachefs-dkms"));
        assert_eq!(version.as_deref(), Some("3:1.39.7-1.1"));
        assert_eq!(repo.as_deref(), Some("cachyos-extra-v4"));
    }

    #[test]
    fn desc_without_installed_db() {
        let text = "%NAME%\ngitkraken\n\n%VERSION%\n12.6.1-1\n";
        let (name, version, repo) = parse_desc(text);
        assert_eq!(name.as_deref(), Some("gitkraken"));
        assert_eq!(version.as_deref(), Some("12.6.1-1"));
        assert_eq!(repo, None);
    }

    #[test]
    fn candidate_prefers_first_exact_in_order() {
        let entries = parse_sync_list(
            "cachyos-extra-v4 bcachefs-dkms 3:1.39.7-1.1 [installed]\n\
             extra bcachefs-dkms 3:1.39.7-1\n\
             minka bcachefs-dkms 3:1.39.7-2\n",
        );
        assert_eq!(
            candidate_repo(&entries, "bcachefs-dkms", "3:1.39.7-2"),
            Some("minka")
        );
        assert_eq!(
            candidate_repo(&entries, "bcachefs-dkms", "3:1.39.7-1.1"),
            Some("cachyos-extra-v4")
        );
        assert_eq!(candidate_repo(&entries, "nope", "1"), None);
    }
}
