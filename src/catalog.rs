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

/// What the local DB says of one package: what the detail view's install
/// reason shows, and what marking it a dependency would leave behind.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct LocalPackage {
    pub name: String,
    pub installed: bool,
    pub version: String,
    /// %REASON%: explicitly installed, or a dependency.
    pub explicit: bool,
    /// The installed packages that depend on it (pacman's "Required By").
    pub required_by: Vec<String>,
    /// The installed packages that optionally depend on it ("Optional
    /// For"). Only with both lists empty does `pacman -Qdt` list it as an
    /// orphan (when it is a dependency); with only this one, `-Qdtt` does.
    pub optional_for: Vec<String>,
    /// Every installed package that needs it, directly or through others:
    /// what `pacman -Rc` would remove with it (alpm::needed_by). Unlike
    /// required_by, it leaves out a package that another installed one
    /// satisfies just as well.
    pub all_required_by: Vec<String>,
    /// The explicitly installed packages in all_required_by. Empty: nothing
    /// installed on purpose needs it, even when a cycle of dependencies
    /// keeps it off `pacman -Qdt`.
    pub needed_by_explicit: Vec<String>,
    /// Why the packages in required_by but not in all_required_by do not
    /// need it: the installed packages that satisfy those dependencies as
    /// well and would stay (alpm::Needed::covered_by).
    pub covered_by: Vec<String>,
    /// The explicitly installed packages among required_by and
    /// optional_for, sorted: the window marks them in those lists.
    pub explicit_dependents: Vec<String>,
    /// The names it offers (its own, its provides) that other installed
    /// packages offer too (alpm::shared_provides).
    pub shared_provides: Vec<alpm::SharedProvide>,
}

/// LocalPackage of `name`, from an installed map (not installed: only the
/// name, `installed` false).
pub fn local_package(name: &str, installed: &HashMap<String, alpm::InstalledPkg>) -> LocalPackage {
    let mut cmp = check::memo_vercmp();
    match installed.get(name) {
        Some(pkg) => {
            let needed = alpm::needed_by(name, installed, &mut cmp);
            let required_by = alpm::required_by(name, installed, &mut cmp);
            let optional_for = alpm::optional_for(name, installed, &mut cmp);
            let mut explicit_dependents: Vec<String> = required_by
                .iter()
                .chain(&optional_for)
                .filter(|n| installed[*n].explicit)
                .cloned()
                .collect();
            explicit_dependents.sort();
            explicit_dependents.dedup();
            LocalPackage {
                name: name.to_string(),
                installed: true,
                version: pkg.version.clone(),
                explicit: pkg.explicit,
                required_by,
                optional_for,
                all_required_by: needed.all,
                needed_by_explicit: needed.explicit,
                covered_by: needed.covered_by,
                explicit_dependents,
                shared_provides: alpm::shared_provides(name, installed),
            }
        }
        None => LocalPackage { name: name.to_string(), ..Default::default() },
    }
}

/// LocalPackage of `name`, read from the local DB now.
pub fn run_local_package(name: &str) -> LocalPackage {
    local_package(name, &alpm::read_installed(Path::new("/var/lib/pacman/local")))
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
    capture(Command::new(program).args(args))
}

/// capture_search for a command already set up (environment and all).
fn capture(cmd: &mut Command) -> Result<String, String> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let output = cmd
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
            minka_note::note!("minka-syu: AUR search unavailable, repo results only: {e}");
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

/// What `pacman -Si` says of one repository's build, for the picker: the
/// fields shown, and what tells two builds of one version apart.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SyncInfo {
    pub architecture: String,
    pub description: String,
    /// Empty when unrecorded (-Si prints "None").
    pub packager: String,
    /// Unix seconds; None when unknown ("None", or a date in another form
    /// than check::parse_c_date reads).
    pub build_date: Option<i64>,
}

/// Parse `pacman -Si` output (C locale, UTC: see sync_info) into
/// (repo, name) -> SyncInfo. Blocks are `Key : value` lines separated by
/// blanks; only the fields we use are kept.
pub fn parse_sync_info(text: &str) -> HashMap<(String, String), SyncInfo> {
    let mut out = HashMap::new();
    let mut block: (String, String, SyncInfo) = Default::default();
    for line in text.lines().chain(std::iter::once("")) {
        if line.trim().is_empty() {
            let (repo, name, info) = std::mem::take(&mut block);
            if !repo.is_empty() && !name.is_empty() {
                out.insert((repo, name), info);
            }
            continue;
        }
        let Some((key, value)) = line.split_once(':') else { continue };
        let value = value.trim();
        match key.trim() {
            "Repository" => block.0 = value.to_string(),
            "Name" => block.1 = value.to_string(),
            "Architecture" => block.2.architecture = value.to_string(),
            "Description" => block.2.description = value.to_string(),
            "Packager" if value != "None" => block.2.packager = value.to_string(),
            "Build Date" => block.2.build_date = check::parse_build_date(value),
            _ => {}
        }
    }
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
    info: &HashMap<(String, String), SyncInfo>,
    aur_hits: &[SearchResult],
    installed: &HashMap<String, alpm::InstalledPkg>,
    cmp: &dyn Fn(&str, &str) -> Ordering,
) -> Vec<RepoOption> {
    let installed_pkg = installed.get(package);
    let is_installed = |repo: &str, version: &str, build: Option<&SyncInfo>| -> bool {
        let Some(pkg) = installed_pkg else { return false };
        if pkg.version != version {
            return false;
        }
        let same_origin = match &pkg.repo {
            Some(origin) => origin == repo,
            // No recorded origin and not offered by any sync repo:
            // it can only have come from the AUR / a local build. Offered
            // by one whose copy is the very build (date and packager known
            // and equal): that repository's package, which pacman pulled in
            // without recording it (the check's origin group).
            None => {
                (repo == "aur" && !sync.iter().any(|e| e.name == package))
                    || build.is_some_and(|b| b.build_date.is_some() && b.build_date == pkg.build_date && b.packager == pkg.packager)
            }
        };
        // A repository can rebuild a version in place. With both build
        // dates known, the build must be the installed one too: same date
        // and packager, as the check compares builds. Otherwise the
        // version and origin decide.
        let same_build = build.is_none_or(|b| match (pkg.build_date, b.build_date) {
            (Some(mine), Some(theirs)) => mine == theirs && b.packager == pkg.packager,
            _ => true,
        });
        same_origin && same_build
    };

    let mut out: Vec<RepoOption> = Vec::new();
    for (idx, entry) in sync.iter().filter(|e| e.name == package).enumerate() {
        let build = info.get(&(entry.repo.clone(), entry.name.clone()));
        out.push(RepoOption {
            target: format!("{}/{}", entry.repo, entry.name),
            is_default: idx == 0,
            is_installed: is_installed(&entry.repo, &entry.version, build),
            is_newest: false,
            repo: entry.repo.clone(),
            name: entry.name.clone(),
            version: entry.version.clone(),
            architecture: build.map(|b| b.architecture.clone()).unwrap_or_default(),
            description: build.map(|b| b.description.clone()).unwrap_or_default(),
        });
    }
    for hit in aur_hits.iter().filter(|h| h.name == package && h.repo == "aur") {
        out.push(RepoOption {
            target: format!("aur/{}", hit.name),
            is_default: false,
            is_installed: is_installed("aur", &hit.version, None),
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

/// Architecture, description and build (date, packager) for a handful
/// of repo/name targets via one `pacman -Si` call; empty on any failure
/// (the picker still works from the sync list alone).
fn sync_info(targets: &[String]) -> HashMap<(String, String), SyncInfo> {
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
    // The C locale and UTC: English field names, which the parser
    // matches, and the Build Date in check::parse_c_date's form.
    capture(Command::new("pacman").args(&args).env("LC_ALL", "C").env("TZ", "UTC0"))
        .map(|t| parse_sync_info(&t))
        .unwrap_or_default()
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
                ..Default::default()
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
        // -Si as sync_info runs it (C locale, UTC); the installed build is
        // cachyos-extra-v4's, the same date and packager.
        let info = parse_sync_info(
            "Repository      : cachyos-extra-v4\nName            : bcachefs-dkms\n\
             Architecture    : x86_64_v4\nPackager        : CachyOS <admin@cachyos.org>\n\
             Build Date      : Thu Aug 27 12:25:22 2026\n\n\
             Repository      : minka\nName            : bcachefs-dkms\nArchitecture    : x86_64\n\
             Description     : bcachefs DKMS\nOptional Deps   : linux-headers: build the module\n\
             \x20                 dkms: rebuild it\nPackager        : None\nBuild Date      : None\n",
        );
        let cachy = &info[&("cachyos-extra-v4".to_string(), "bcachefs-dkms".to_string())];
        assert_eq!((cachy.build_date, cachy.packager.as_str()), (Some(1787833522), "CachyOS <admin@cachyos.org>"));
        let own = &info[&("minka".to_string(), "bcachefs-dkms".to_string())];
        assert_eq!((own.build_date, own.packager.as_str(), own.description.as_str()), (None, "", "bcachefs DKMS"));
        let installed = HashMap::from([(
            "bcachefs-dkms".to_string(),
            alpm::InstalledPkg {
                version: "3:1.39.7-1.1".into(),
                repo: Some("cachyos-extra-v4".into()),
                description: String::new(),
                size_bytes: 0,
                explicit: true,
                build_date: Some(1787833522),
                packager: "CachyOS <admin@cachyos.org>".into(),
                ..Default::default()
            },
        )]);
        let opts = build_repo_options("bcachefs-dkms", &sync, &info, &[], &installed, &|a, b| a.cmp(b));
        assert_eq!(opts.len(), 3);
        assert!(opts[0].is_default && opts[0].is_installed && !opts[0].is_newest);
        assert_eq!(opts[0].target, "cachyos-extra-v4/bcachefs-dkms");
        assert_eq!(opts[0].architecture, "x86_64_v4");
        assert!(opts[2].is_newest && !opts[2].is_default);
        assert_eq!(opts[2].architecture, "x86_64");
        assert_eq!(opts[2].target, "minka/bcachefs-dkms");
    }

    #[test]
    fn a_build_rebuilt_in_place_is_not_the_installed_one() {
        // Installed from [minka]; [minka] has since rebuilt that version
        // without a pkgrel bump (the same-repo switch row's case).
        let sync = alpm::parse_sync_list("cachyos yy 2.0-1\nminka yy 2.0-1\n");
        let si = |repo: &str, date: &str| {
            format!("Repository      : {repo}\nName            : yy\nPackager        : P <p@example.org>\nBuild Date      : {date}\n\n")
        };
        let built = |date: i64| {
            HashMap::from([(
                "yy".to_string(),
                alpm::InstalledPkg {
                    version: "2.0-1".into(),
                    repo: Some("minka".into()),
                    build_date: Some(date),
                    packager: "P <p@example.org>".into(),
                    ..Default::default()
                },
            )])
        };
        let info = parse_sync_info(&format!("{}{}", si("cachyos", "Sun Nov  1 10:00:00 2026"), si("minka", "Sat Nov 22 01:48:00 2026")));
        let opts = build_repo_options("yy", &sync, &info, &[], &built(1725703316), &|a, b| a.cmp(b));
        assert!(opts.iter().all(|o| !o.is_installed), "{opts:#?}");
        // The same build: installed. Without -Si's details: the version
        // and origin decide, as before.
        let opts = build_repo_options("yy", &sync, &info, &[], &built(1795312080), &|a, b| a.cmp(b));
        assert!(!opts[0].is_installed && opts[1].is_installed, "{opts:#?}");
        let opts = build_repo_options("yy", &sync, &HashMap::new(), &[], &built(1725703316), &|a, b| a.cmp(b));
        assert!(!opts[0].is_installed && opts[1].is_installed, "{opts:#?}");
        // Another packager on the same date is another build.
        let other = si("minka", "Sat Nov 22 01:48:00 2026").replace("P <p@", "Q <q@");
        let opts = build_repo_options("yy", &sync, &parse_sync_info(&other), &[], &built(1795312080), &|a, b| a.cmp(b));
        assert!(!opts[1].is_installed, "{opts:#?}");
    }

    #[test]
    fn an_unrecorded_repo_build_is_installed_from_that_repo() {
        // No %INSTALLED_DB% (pulled in as a dependency): the copy that is
        // the very build is the installed one; another build is not.
        let sync = alpm::parse_sync_list("cachyos zstd 1.5.7-2\nextra zstd 1.5.7-2\n");
        let si = |repo: &str, date: &str| {
            format!("Repository      : {repo}\nName            : zstd\nPackager        : P <p@example.org>\nBuild Date      : {date}\n\n")
        };
        let info = parse_sync_info(&format!("{}{}", si("cachyos", "Sun Nov  1 10:00:00 2026"), si("extra", "Tue Aug 12 12:00:00 2025")));
        let installed = HashMap::from([(
            "zstd".to_string(),
            alpm::InstalledPkg {
                version: "1.5.7-2".into(),
                build_date: Some(1755000000),
                packager: "P <p@example.org>".into(),
                ..Default::default()
            },
        )]);
        let opts = build_repo_options("zstd", &sync, &info, &[], &installed, &|a, b| a.cmp(b));
        assert!(!opts[0].is_installed && opts[1].is_installed, "{opts:#?}");
        // Without -Si's details nothing can say which it is.
        let opts = build_repo_options("zstd", &sync, &HashMap::new(), &[], &installed, &|a, b| a.cmp(b));
        assert!(opts.iter().all(|o| !o.is_installed), "{opts:#?}");
    }

    #[test]
    fn local_package_reports_reason_and_dependents() {
        let installed = HashMap::from([
            (
                "libwpe".to_string(),
                alpm::InstalledPkg {
                    version: "1.16.3-1.1".into(),
                    provides: check::parse_depstring("libwpe-1.0.so=1-64").into_iter().collect(),
                    ..Default::default()
                },
            ),
            (
                "wpebackend-fdo".to_string(),
                alpm::InstalledPkg {
                    version: "1.16.1-1.1".into(),
                    depends: check::parse_depstring("libwpe-1.0.so=1-64").into_iter().collect(),
                    ..Default::default()
                },
            ),
            (
                "app".to_string(),
                alpm::InstalledPkg {
                    version: "1-1".into(),
                    explicit: true,
                    optdepends: check::parse_depstring("wpebackend-fdo").into_iter().collect(),
                    ..Default::default()
                },
            ),
        ]);
        let l = local_package("libwpe", &installed);
        assert!(l.installed && !l.explicit);
        assert_eq!((l.version.as_str(), l.required_by.as_slice()), ("1.16.3-1.1", &["wpebackend-fdo".to_string()][..]));
        assert!(l.optional_for.is_empty());
        // Needed only by a dependency that nothing explicit needs.
        assert_eq!((l.all_required_by.as_slice(), l.needed_by_explicit.as_slice()), (&["wpebackend-fdo".to_string()][..], &[][..]));
        let fdo = local_package("wpebackend-fdo", &installed);
        assert!(fdo.required_by.is_empty());
        assert_eq!(fdo.optional_for, ["app"]);
        assert!(fdo.all_required_by.is_empty(), "optional dependencies never need it");
        assert_eq!(fdo.explicit_dependents, ["app"], "the explicit ones among both lists");
        assert!(l.explicit_dependents.is_empty());
        assert!(l.shared_provides.is_empty(), "no other package offers its soname");
        let absent = local_package("nope", &installed);
        assert!(!absent.installed && absent.name == "nope");
    }

    #[test]
    fn escapes_regex_terms() {
        assert_eq!(escape_regex("libsigc++"), "libsigc\\+\\+");
        assert_eq!(escape_regex("gtk4"), "gtk4");
        assert_eq!(escape_regex("a.b[c]"), "a\\.b\\[c\\]");
    }
}
