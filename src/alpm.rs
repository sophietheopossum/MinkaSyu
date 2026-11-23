//! Readers for ALPM's on-disk databases, narrow enough to stay honest:
//! the local DB for what is installed (and which repo it came from), and
//! `pacman -Sl` output for what the sync DBs offer, in pacman.conf order.

use crate::check::{DepSpec, dep_matches, parse_depstring};
use serde::Serialize;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct InstalledPkg {
    pub version: String,
    /// The sync repo the package was installed from (`%INSTALLED_DB%`).
    /// libalpm records it only for a named target and for a plain
    /// sysupgrade of an installed package: a package pulled in as a
    /// dependency or as a replacement has none until its next upgrade,
    /// and neither has one installed from a file (most AUR/manual builds)
    /// or before libalpm recorded it.
    pub repo: Option<String>,
    pub description: String,
    /// Installed size in bytes (`%SIZE%`); 0 when unrecorded.
    pub size_bytes: u64,
    /// Explicitly installed, as opposed to pulled in as a dependency.
    /// libalpm writes `%REASON% 1` for dependencies and omits the
    /// field entirely for explicit installs.
    pub explicit: bool,
    /// When the installed build was made (`%BUILDDATE%`, Unix seconds).
    /// With `packager`, what tells two builds of one version apart.
    pub build_date: Option<i64>,
    /// Who made it (`%PACKAGER%`); empty when unrecorded.
    pub packager: String,
    /// What it depends on (`%DEPENDS%`), with the constraints.
    pub depends: Vec<DepSpec>,
    /// What it optionally depends on (`%OPTDEPENDS%`), less the
    /// descriptions.
    pub optdepends: Vec<DepSpec>,
    /// What it provides (`%PROVIDES%`), with the versions.
    pub provides: Vec<DepSpec>,
    /// How libalpm checked the file when it was installed
    /// (`%VALIDATION%`): "none", or any of "md5", "sha256" and "pgp".
    /// Empty when unrecorded. A repository's package always has a
    /// checksum or a signature; "none" alone means a file installed with
    /// -U and no signature (a local or AUR build).
    pub validation: Vec<String>,
}

impl InstalledPkg {
    /// Installed from a file with no signature or checksum (`-U` of a
    /// local or AUR build), not from a repository.
    pub fn unsigned_file(&self) -> bool {
        self.validation.len() == 1 && self.validation[0] == "none"
    }
}

/// An %OPTDEPENDS% line's depstring: "name[op ver]: description" less the
/// description. libalpm cuts at ": ", since an epoch has a bare ':'.
fn optdep_string(line: &str) -> &str {
    line.split_once(": ").map_or(line, |(dep, _)| dep).trim()
}

/// Parse one local-DB `desc` file into (name, package).
/// Format: `%FIELD%` line, value line(s), blank line, repeated.
/// Only the first value line of each field we care about is read, except
/// for the lists (%DEPENDS%, %OPTDEPENDS%, %PROVIDES%, %VALIDATION%),
/// which run to the blank line.
pub fn parse_desc(text: &str) -> (Option<String>, Option<InstalledPkg>) {
    let mut name = None;
    let mut version = None;
    let mut repo = None;
    let mut description = None;
    let mut size = None;
    let mut reason = None;
    let mut build_date = None;
    let mut packager = None;
    let (mut depends, mut optdepends, mut provides) = (Vec::new(), Vec::new(), Vec::new());
    let mut validation = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let list = match line.trim() {
            "%DEPENDS%" => Some(&mut depends),
            "%OPTDEPENDS%" => Some(&mut optdepends),
            "%PROVIDES%" => Some(&mut provides),
            _ => None,
        };
        let optional = line.trim() == "%OPTDEPENDS%";
        if let Some(list) = list {
            for value in lines.by_ref() {
                let value = if optional { optdep_string(value) } else { value.trim() };
                if value.is_empty() {
                    break;
                }
                list.extend(parse_depstring(value));
            }
            continue;
        }
        if line.trim() == "%VALIDATION%" {
            for value in lines.by_ref() {
                if value.trim().is_empty() {
                    break;
                }
                validation.push(value.trim().to_string());
            }
            continue;
        }
        let target = match line.trim() {
            "%NAME%" => &mut name,
            "%VERSION%" => &mut version,
            "%INSTALLED_DB%" => &mut repo,
            "%DESC%" => &mut description,
            "%SIZE%" => &mut size,
            "%REASON%" => &mut reason,
            "%BUILDDATE%" => &mut build_date,
            "%PACKAGER%" => &mut packager,
            _ => continue,
        };
        if let Some(value) = lines.next() {
            let value = value.trim();
            if !value.is_empty() {
                *target = Some(value.to_string());
            }
        }
    }
    let pkg = version.map(|version| InstalledPkg {
        version,
        repo,
        description: description.unwrap_or_default(),
        size_bytes: size.and_then(|s| s.parse().ok()).unwrap_or(0),
        explicit: reason.as_deref() != Some("1"),
        build_date: build_date.and_then(|d| d.parse().ok()),
        packager: packager.unwrap_or_default(),
        depends,
        optdepends,
        provides,
        validation,
    });
    (name, pkg)
}

/// The installed packages with a dependency in `deps_of` that `name`
/// satisfies, sorted, as libalpm's alpm_pkg_compute_requiredby matches
/// them (_alpm_depcmp: by name and version, or by a provide, an
/// unversioned provide only for an unversioned dependency). A package that
/// depends on itself counts, as in pacman.
fn dependents(
    name: &str,
    installed: &HashMap<String, InstalledPkg>,
    deps_of: fn(&InstalledPkg) -> &[DepSpec],
    cmp: &mut dyn FnMut(&str, &str) -> Ordering,
) -> Vec<String> {
    let Some(pkg) = installed.get(name) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for (other, p) in installed {
        if deps_of(p).iter().any(|d| dep_matches(d, name, &pkg.version, &pkg.provides, cmp)) {
            out.push(other.clone());
        }
    }
    out.sort();
    out
}

/// The installed packages that depend on `name`: pacman's "Required By".
/// Nothing there and nothing in optional_for: `pacman -Qdt` lists it,
/// when it is a dependency.
pub fn required_by(
    name: &str,
    installed: &HashMap<String, InstalledPkg>,
    cmp: &mut dyn FnMut(&str, &str) -> Ordering,
) -> Vec<String> {
    dependents(name, installed, |p| &p.depends, cmp)
}

/// The installed packages that optionally depend on `name`: pacman's
/// "Optional For". `pacman -Qdt` skips a package these name; `-Qdtt` lists
/// it too.
pub fn optional_for(
    name: &str,
    installed: &HashMap<String, InstalledPkg>,
    cmp: &mut dyn FnMut(&str, &str) -> Ordering,
) -> Vec<String> {
    dependents(name, installed, |p| &p.optdepends, cmp)
}

/// Who needs an installed package, as `needed_by` finds it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Needed {
    /// Every installed package that needs it, directly or through others,
    /// sorted: what `pacman -Rc` of it would remove along with it.
    pub all: Vec<String>,
    /// The explicitly installed packages in `all`.
    pub explicit: Vec<String>,
    /// What excuses the packages that depend on it but are not in `all`:
    /// the installed packages outside `all` that satisfy those dependencies
    /// as well (ca-certificates-utils, which provides ca-certificates).
    /// Sorted, each once.
    pub covered_by: Vec<String>,
}

/// Who needs `name`, by pacman's removal check rather than its "Required
/// By": a package needs it when one of its dependencies is satisfied only
/// by `name` or by a package that already needs it (alpm_checkdeps with
/// reversedeps, repeated until nothing more breaks, as `pacman -Rc` does).
/// So a dependency another installed package still satisfies (a second
/// provider of sh or java-runtime) does not count, one that was broken
/// already is ignored as libalpm ignores it, and a cycle of dependencies
/// ends. Optional dependencies never count; `name` itself is left out.
///
/// The reverse index is built once for the whole walk, and versions are
/// compared only for dependencies that a package in the walk could
/// satisfy: `cmp` may be the vercmp tool, one process per pair.
pub fn needed_by(
    name: &str,
    installed: &HashMap<String, InstalledPkg>,
    cmp: &mut dyn FnMut(&str, &str) -> Ordering,
) -> Needed {
    if !installed.contains_key(name) {
        return Needed::default();
    }
    // Dependency name -> (who depends, the dependency); name or provide ->
    // the packages with it.
    let mut depending: HashMap<&str, Vec<(&str, &DepSpec)>> = HashMap::new();
    let mut having: HashMap<&str, Vec<&str>> = HashMap::new();
    for (pkg_name, pkg) in installed {
        for dep in &pkg.depends {
            depending.entry(&dep.name).or_default().push((pkg_name, dep));
        }
        for key in std::iter::once(pkg_name.as_str()).chain(pkg.provides.iter().map(|p| p.name.as_str())) {
            having.entry(key).or_default().push(pkg_name);
        }
    }
    let mut satisfies = |dep: &DepSpec, who: &str| {
        let pkg = &installed[who];
        dep_matches(dep, who, &pkg.version, &pkg.provides, cmp)
    };
    let mut gone: HashSet<&str> = HashSet::from([name]);
    let mut queue = vec![name];
    while let Some(removed) = queue.pop() {
        let pkg = &installed[removed];
        for key in std::iter::once(removed).chain(pkg.provides.iter().map(|p| p.name.as_str())) {
            for &(dependent, dep) in depending.get(key).into_iter().flatten() {
                if gone.contains(dependent) || !satisfies(dep, removed) {
                    continue;
                }
                let kept = having[dep.name.as_str()].iter().any(|&other| !gone.contains(other) && satisfies(dep, other));
                if !kept {
                    gone.insert(dependent);
                    queue.push(dependent);
                }
            }
        }
    }
    // Read once the walk is done: a provider that still stood when a
    // dependent was checked may have gone since (one that depends on `name`
    // itself), and then it took that dependent along and covers nothing.
    let mut covered_by: Vec<String> = Vec::new();
    let pkg = &installed[name];
    for key in std::iter::once(name).chain(pkg.provides.iter().map(|p| p.name.as_str())) {
        for &(dependent, dep) in depending.get(key).into_iter().flatten() {
            if gone.contains(dependent) || !satisfies(dep, name) {
                continue;
            }
            for &other in &having[dep.name.as_str()] {
                if !gone.contains(other) && satisfies(dep, other) {
                    covered_by.push(other.to_string());
                }
            }
        }
    }
    covered_by.sort();
    covered_by.dedup();
    gone.remove(name);
    let mut all: Vec<String> = gone.into_iter().map(String::from).collect();
    all.sort();
    let explicit = all.iter().filter(|n| installed[*n].explicit).cloned().collect();
    Needed { all, explicit, covered_by }
}

/// A name an installed package offers (its own, or a provide) that other
/// installed packages offer too, as shared_provides finds it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SharedProvide {
    /// The name; a soname with its version (`libfoo.so=1-64`).
    pub name: String,
    /// The other installed packages that offer it, sorted, each once.
    pub also: Vec<String>,
}

/// What a provide is compared by: a soname with its version, so that a
/// lib32 package and its 64-bit twin (libgtk-3.so=0-32, =0-64) do not
/// count as offering the same thing, as a soname dependency normally
/// names its architecture (compared by name, 204 sonames were shared on
/// 23/11/2026); anything else by name alone, so jdk11's java-runtime=11
/// and jdk21's java-runtime=21 are both a java-runtime.
fn offer_key(p: &DepSpec) -> String {
    match &p.constraint {
        Some((op, version)) if p.name.contains(".so") => format!("{}{op}{version}", p.name),
        _ => p.name.clone(),
    }
}

/// The names `name` offers, its own and its provides, that other installed
/// packages offer as well, by providing them or by being named so: ca-certificates
/// and ca-certificates-utils (which provides ca-certificates), or the JDKs'
/// java-runtime. Sorted by name; empty when `name` is not installed.
pub fn shared_provides(name: &str, installed: &HashMap<String, InstalledPkg>) -> Vec<SharedProvide> {
    let Some(pkg) = installed.get(name) else {
        return Vec::new();
    };
    // A package that provides its own name (jdk21-openjdk=21...) offers it once.
    let offers = |n: &str, p: &InstalledPkg| -> HashSet<String> {
        std::iter::once(n.to_string()).chain(p.provides.iter().map(offer_key)).collect()
    };
    let mut also: HashMap<String, Vec<String>> = offers(name, pkg).into_iter().map(|k| (k, Vec::new())).collect();
    for (other, p) in installed {
        if other == name {
            continue;
        }
        for key in offers(other, p) {
            if let Some(list) = also.get_mut(&key) {
                list.push(other.clone());
            }
        }
    }
    let mut shared: Vec<SharedProvide> = also
        .into_iter()
        .filter(|(_, list)| !list.is_empty())
        .map(|(name, mut also)| {
            also.sort();
            SharedProvide { name, also }
        })
        .collect();
    shared.sort_by(|a, b| a.name.cmp(&b.name));
    shared
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
        let (name, pkg) = parse_desc(&text);
        if let (Some(name), Some(pkg)) = (name, pkg) {
            out.insert(name, pkg);
        }
    }
    out
}

/// `%GROUPS%` of one local-DB `desc` file: (name, groups). Unlike the
/// fields parse_desc reads, this one lists one value per line up to
/// the blank line.
pub fn parse_groups(text: &str) -> (Option<String>, Vec<String>) {
    let mut name = None;
    let mut groups = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        match line.trim() {
            "%NAME%" => name = lines.next().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()),
            "%GROUPS%" => {
                for value in lines.by_ref() {
                    let value = value.trim();
                    if value.is_empty() {
                        break;
                    }
                    groups.push(value.to_string());
                }
            }
            _ => {}
        }
    }
    (name, groups)
}

/// Groups of every installed package that belongs to any. Read only
/// when IgnoreGroup is set, so the ordinary check does not pay for a
/// second pass over the local DB.
pub fn read_installed_groups(local_db: &Path) -> HashMap<String, Vec<String>> {
    let mut out = HashMap::new();
    let Ok(entries) = std::fs::read_dir(local_db) else {
        return out;
    };
    for entry in entries.flatten() {
        let Ok(text) = std::fs::read_to_string(entry.path().join("desc")) else {
            continue;
        };
        if let (Some(name), groups) = parse_groups(&text) {
            if !groups.is_empty() {
                out.insert(name, groups);
            }
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
    fn groups_parsing() {
        let desc = "%NAME%\nplasma-desktop\n\n%VERSION%\n6.5-1\n\n%GROUPS%\nplasma\nkde-applications\n\n%DESC%\nx\n\n";
        assert_eq!(
            parse_groups(desc),
            (Some("plasma-desktop".into()), vec!["plasma".to_string(), "kde-applications".to_string()])
        );
        assert_eq!(parse_groups("%NAME%\nsolo\n\n"), (Some("solo".into()), vec![]));
    }

    #[test]
    fn desc_with_installed_db() {
        let text = "%NAME%\nbcachefs-dkms\n\n%VERSION%\n3:1.39.7-1.1\n\n%INSTALLED_DB%\ncachyos-extra-v4\n\n%DESC%\nbcachefs DKMS\n\n%BUILDDATE%\n1725703316\n\n%PACKAGER%\nCachyOS <admin@cachyos.org>\n\n%SIZE%\n123456\n\n%REASON%\n1\n";
        let (name, pkg) = parse_desc(text);
        assert_eq!(name.as_deref(), Some("bcachefs-dkms"));
        let pkg = pkg.expect("pkg");
        assert_eq!(pkg.version, "3:1.39.7-1.1");
        assert_eq!(pkg.repo.as_deref(), Some("cachyos-extra-v4"));
        assert_eq!(pkg.description, "bcachefs DKMS");
        assert_eq!(pkg.size_bytes, 123456);
        assert!(!pkg.explicit, "REASON 1 is a dependency install");
        assert_eq!(pkg.build_date, Some(1725703316));
        assert_eq!(pkg.packager, "CachyOS <admin@cachyos.org>");
    }

    /// Depstrings, parsed.
    fn deps(list: &[&str]) -> Vec<DepSpec> {
        list.iter().filter_map(|d| parse_depstring(d)).collect()
    }

    fn parsed(descs: &[&str]) -> HashMap<String, InstalledPkg> {
        descs
            .iter()
            .filter_map(|t| match parse_desc(t) {
                (Some(n), Some(p)) => Some((n, p)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn desc_lists_and_required_by() {
        // libwpe on 22/11/2026: wpebackend-fdo needs it through a soname it
        // provides, not by name; app only optionally.
        let libwpe = "%NAME%\nlibwpe\n\n%VERSION%\n1.16.3-1.1\n\n%DEPENDS%\nlibxkbcommon\nglibc>=2.40\n\n%PROVIDES%\nlibwpe-1.0.so=1-64\n\n%REASON%\n1\n\n%VALIDATION%\nsha256\npgp\n";
        let fdo = "%NAME%\nwpebackend-fdo\n\n%VERSION%\n1.16.1-1.1\n\n%DEPENDS%\nlibwpe-1.0.so=1-64\nwayland\n\n%VALIDATION%\nnone\n";
        let app = "%NAME%\napp\n\n%VERSION%\n1-1\n\n%DEPENDS%\nwpebackend-fdo>=1.16\n\n%OPTDEPENDS%\nlibwpe: not a dependency\nepoch>=1:2.0: an epoch in the constraint\n\n";
        let (_, p) = parse_desc(libwpe);
        let p = p.expect("pkg");
        assert_eq!(p.depends, deps(&["libxkbcommon", "glibc>=2.40"]));
        assert_eq!(p.provides, deps(&["libwpe-1.0.so=1-64"]));
        assert_eq!((p.validation.as_slice(), p.unsigned_file()), (&["sha256".to_string(), "pgp".to_string()][..], false));
        let installed = parsed(&[libwpe, fdo, app]);
        assert!(installed["wpebackend-fdo"].unsigned_file());
        assert_eq!(installed["app"].optdepends, deps(&["libwpe", "epoch>=1:2.0"]));
        let cmp: &mut dyn FnMut(&str, &str) -> Ordering = &mut crate::check::system_vercmp;
        assert_eq!(required_by("libwpe", &installed, cmp), ["wpebackend-fdo"]);
        assert_eq!(optional_for("libwpe", &installed, cmp), ["app"]);
        assert_eq!(required_by("wpebackend-fdo", &installed, cmp), ["app"]);
        assert!(optional_for("wpebackend-fdo", &installed, cmp).is_empty());
        assert!(required_by("app", &installed, cmp).is_empty(), "optdepends do not count");
        assert!(required_by("absent", &installed, cmp).is_empty());
    }

    #[test]
    fn required_by_checks_versions_as_libalpm_does() {
        // 22/11/2026: lib32-acl provides libacl.so=1-32, rsync needs the
        // 64-bit libacl.so=1-64; the names alone would collide.
        let lib32_acl = "%NAME%\nlib32-acl\n\n%VERSION%\n2.3.2-1\n\n%PROVIDES%\nlibacl.so=1-32\n\n";
        let acl = "%NAME%\nacl\n\n%VERSION%\n2.3.2-1\n\n%PROVIDES%\nlibacl.so=1-64\n\n";
        let rsync = "%NAME%\nrsync\n\n%VERSION%\n3.4.1-2\n\n%DEPENDS%\nlibacl.so=1-64\nglibc>=2.40\nfoo>=2\n\n";
        let wine = "%NAME%\nwine\n\n%VERSION%\n10.0-1\n\n%DEPENDS%\nlibacl.so=1-32\n\n";
        // An unversioned provide never satisfies a versioned dependency.
        let foo = "%NAME%\nfoo-compat\n\n%VERSION%\n3-1\n\n%PROVIDES%\nfoo\n\n";
        let glibc = "%NAME%\nglibc\n\n%VERSION%\n2.39-1\n\n";
        let installed = parsed(&[lib32_acl, acl, rsync, wine, foo, glibc]);
        let cmp: &mut dyn FnMut(&str, &str) -> Ordering = &mut crate::check::system_vercmp;
        assert_eq!(required_by("lib32-acl", &installed, cmp), ["wine"], "only the 32-bit soname");
        assert_eq!(required_by("acl", &installed, cmp), ["rsync"]);
        assert!(required_by("foo-compat", &installed, cmp).is_empty());
        // glibc 2.39 fails rsync's glibc>=2.40, so rsync does not name it.
        let mut fixed = |a: &str, b: &str| a.cmp(b);
        assert!(required_by("glibc", &installed, &mut fixed).is_empty());
        let newer = parsed(&["%NAME%\nglibc\n\n%VERSION%\n2.41-1\n\n", rsync]);
        assert_eq!(required_by("glibc", &newer, &mut fixed), ["rsync"]);
        // A package that depends on itself counts, as in pacman.
        let selfish = parsed(&["%NAME%\nperl-x\n\n%VERSION%\n1-1\n\n%DEPENDS%\nperl-x\n\n"]);
        assert_eq!(required_by("perl-x", &selfish, &mut fixed), ["perl-x"]);
    }

    /// An installed package at 1-1: its dependencies, its provides, and
    /// whether it was installed explicitly.
    fn pkg(name: &str, depends: &[&str], provides: &[&str], explicit: bool) -> (String, InstalledPkg) {
        let pkg = InstalledPkg { version: "1-1".into(), explicit, depends: deps(depends), provides: deps(provides), ..Default::default() };
        (name.to_string(), pkg)
    }

    /// needed_by with versions compared as strings, which these tests keep
    /// apart.
    fn needs(name: &str, installed: &[(String, InstalledPkg)]) -> (Vec<String>, Vec<String>) {
        let map: HashMap<String, InstalledPkg> = installed.iter().cloned().collect();
        let n = needed_by(name, &map, &mut |a: &str, b: &str| a.cmp(b));
        (n.all, n.explicit)
    }

    #[test]
    fn needed_by_follows_dependencies_to_explicit_packages() {
        // A chain: app (explicit) -> mid -> lib, and an optional user.
        let chain = [
            pkg("lib", &[], &[], false),
            pkg("mid", &["lib"], &[], false),
            pkg("app", &["mid"], &[], true),
            pkg("viewer", &[], &[], true),
        ];
        let mut chain = chain.to_vec();
        chain[3].1.optdepends = deps(&["lib"]);
        assert_eq!(needs("lib", &chain), (vec!["app".into(), "mid".into()], vec!["app".into()]));
        assert_eq!(needs("app", &chain), (vec![], vec![]), "an explicit package nothing needs");
        assert_eq!(needs("viewer", &chain), (vec![], vec![]));
        assert_eq!(needs("absent", &chain), (vec![], vec![]));
        // A cycle of dependencies with no explicit package behind it: both
        // are required by something, so `pacman -Qdt` lists neither, yet
        // nothing installed on purpose needs lib.
        let cycle = [pkg("lib", &[], &[], false), pkg("a", &["lib", "b"], &[], false), pkg("b", &["a"], &[], false)];
        assert_eq!(needs("lib", &cycle), (vec!["a".into(), "b".into()], vec![]));
        // A package that depends on itself neither needs itself nor stops
        // the walk.
        let selfish = [pkg("lib", &["lib"], &[], false), pkg("perl-x", &["perl-x", "lib"], &[], true)];
        assert_eq!(needs("lib", &selfish), (vec!["perl-x".into()], vec!["perl-x".into()]));
        assert_eq!(needs("perl-x", &selfish), (vec![], vec![]));
    }

    #[test]
    fn needed_by_matches_provides_and_versions() {
        // Through a soname the package provides, not its name.
        let soname = [pkg("libwpe", &[], &["libwpe-1.0.so=1-64"], false), pkg("fdo", &["libwpe-1.0.so=1-64"], &[], true)];
        assert_eq!(needs("libwpe", &soname), (vec!["fdo".into()], vec!["fdo".into()]));
        // A version the installed one does not satisfy: that dependency
        // was broken already, so removing it breaks nothing new.
        let versioned = [pkg("x", &[], &[], false), pkg("new", &["x>=2"], &[], true), pkg("old", &["x>=1"], &[], true)];
        assert_eq!(needs("x", &versioned), (vec!["old".into()], vec!["old".into()]));
    }

    #[test]
    fn needed_by_counts_only_what_no_other_package_satisfies() {
        // Two java runtimes: removing either leaves the app its runtime.
        let two = [
            pkg("jre-a", &[], &["java-runtime=21"], false),
            pkg("jre-b", &[], &["java-runtime=21"], false),
            pkg("app", &["java-runtime"], &[], true),
        ];
        assert_eq!(needs("jre-a", &two), (vec![], vec![]));
        // Unless the other one goes too: jre-b depends on jre-a, so
        // `pacman -Rc jre-a` takes jre-b, and then the app has none.
        let mut chained = two.to_vec();
        chained[1].1.depends = deps(&["jre-a"]);
        assert_eq!(needs("jre-a", &chained), (vec!["app".into(), "jre-b".into()], vec!["app".into()]));
        assert_eq!(needs("jre-b", &chained), (vec![], vec![]), "jre-a still provides it");
        // A provide that fails the constraint is no alternative.
        let mut old = two.to_vec();
        old[1].1.provides = deps(&["java-runtime=11"]);
        old[2].1.depends = deps(&["java-runtime>=17"]);
        assert_eq!(needs("jre-a", &old), (vec!["app".into()], vec!["app".into()]));
    }

    fn covers(name: &str, installed: &[(String, InstalledPkg)]) -> Vec<String> {
        let map: HashMap<String, InstalledPkg> = installed.iter().cloned().collect();
        needed_by(name, &map, &mut |a: &str, b: &str| a.cmp(b)).covered_by
    }

    #[test]
    fn needed_by_names_what_covers_a_dependent_it_excuses() {
        // ca-certificates on 23/11/2026: what requires it by name is just as
        // satisfied by ca-certificates-utils, which provides that name.
        let certs = [
            pkg("ca-certificates", &["ca-certificates-mozilla"], &[], false),
            pkg("ca-certificates-mozilla", &["ca-certificates-utils"], &[], false),
            pkg("ca-certificates-utils", &[], &["ca-certificates"], false),
            pkg("curl", &["ca-certificates"], &[], false),
            pkg("aria2", &["ca-certificates"], &[], true),
        ];
        assert_eq!(needs("ca-certificates", &certs), (vec![], vec![]));
        assert_eq!(covers("ca-certificates", &certs), ["ca-certificates-utils"], "each once");
        // Two other providers both cover, and what is needed has no cover.
        let three = [
            pkg("jre-a", &[], &["java-runtime=21"], false),
            pkg("jre-b", &[], &["java-runtime=21"], false),
            pkg("jre-c", &[], &["java-runtime=21"], false),
            pkg("app", &["java-runtime"], &[], true),
            pkg("ide", &["jre-a"], &[], true),
        ];
        assert_eq!(needs("jre-a", &three), (vec!["ide".into()], vec!["ide".into()]));
        assert_eq!(covers("jre-a", &three), ["jre-b", "jre-c"]);
        // A provider that depends on jre-a itself (through mid) goes with it,
        // even when the walk reaches the app before it: no cover.
        let mut through = three.to_vec();
        through[1].1.depends = deps(&["mid"]);
        through.push(pkg("mid", &["jre-a"], &[], false));
        assert_eq!(needs("jre-a", &through).0, ["ide", "jre-b", "mid"]);
        assert_eq!(covers("jre-a", &through), ["jre-c"]);
        // With no provider left, the app needs it and nothing is covered.
        through[2].1.depends = deps(&["jre-a"]);
        assert_eq!(needs("jre-a", &through).0, ["app", "ide", "jre-b", "jre-c", "mid"]);
        assert!(covers("jre-a", &through).is_empty());
        // A package that depends on itself is no dependent to excuse.
        assert!(covers("lib", &[pkg("lib", &["lib"], &[], false)]).is_empty());
    }

    fn shared(name: &str, installed: &[(String, InstalledPkg)]) -> Vec<(String, Vec<String>)> {
        let map: HashMap<String, InstalledPkg> = installed.iter().cloned().collect();
        shared_provides(name, &map).into_iter().map(|s| (s.name, s.also)).collect()
    }

    fn row(name: &str, also: &[&str]) -> (String, Vec<String>) {
        (name.to_string(), also.iter().map(|a| a.to_string()).collect())
    }

    #[test]
    fn shared_provides_finds_other_packages_offering_a_name() {
        // A virtual several packages provide, at different versions; each
        // provides its own name too (jdk21-openjdk=21.0.13...), which is no
        // one else's.
        let jdks = [
            pkg("jdk21-openjdk", &[], &["java-runtime=21", "jdk21-openjdk=21.0.13", "java-runtime-openjdk=21"], false),
            pkg("jdk11-openjdk", &[], &["java-runtime=11", "java-runtime-openjdk=11"], false),
            pkg("jdk25-graalvm-bin", &[], &["java-runtime=25"], true),
            pkg("app", &["java-runtime"], &[], true),
        ];
        assert_eq!(
            shared("jdk21-openjdk", &jdks),
            [row("java-runtime", &["jdk11-openjdk", "jdk25-graalvm-bin"]), row("java-runtime-openjdk", &["jdk11-openjdk"])]
        );
        assert_eq!(shared("jdk25-graalvm-bin", &jdks), [row("java-runtime", &["jdk11-openjdk", "jdk21-openjdk"])]);
        assert!(shared("app", &jdks).is_empty());
        assert!(shared("absent", &jdks).is_empty());
        // A provide that is another installed package's name, seen from both
        // sides (ca-certificates on 23/11/2026); ca-certificates-java is
        // ca-certificates-utils' alone.
        let certs = [
            pkg("ca-certificates", &["ca-certificates-mozilla"], &[], false),
            pkg("ca-certificates-utils", &[], &["ca-certificates", "ca-certificates-java"], false),
            pkg("ca-certificates-mozilla", &["ca-certificates-utils"], &[], false),
        ];
        assert_eq!(shared("ca-certificates", &certs), [row("ca-certificates", &["ca-certificates-utils"])]);
        assert_eq!(shared("ca-certificates-utils", &certs), [row("ca-certificates", &["ca-certificates"])]);
        assert!(shared("ca-certificates-mozilla", &certs).is_empty());
    }

    #[test]
    fn shared_provides_compares_sonames_with_their_versions() {
        // lib32 and 64-bit twins offer one soname at two versions: not shared.
        let twins = [
            pkg("gtk3", &[], &["libgtk-3.so=0-64", "gtk3-print-backends"], false),
            pkg("lib32-gtk3", &[], &["libgtk-3.so=0-32"], false),
        ];
        assert!(shared("gtk3", &twins).is_empty());
        assert!(shared("lib32-gtk3", &twins).is_empty());
        // The same soname at the same version from two packages is.
        let mut forked = twins.to_vec();
        forked.push(pkg("gtk3-classic", &[], &["libgtk-3.so=0-64"], false));
        assert_eq!(shared("gtk3", &forked), [row("libgtk-3.so=0-64", &["gtk3-classic"])]);
        assert_eq!(shared("gtk3-classic", &forked), [row("libgtk-3.so=0-64", &["gtk3"])]);
        assert!(shared("lib32-gtk3", &forked).is_empty());
    }

    #[test]
    fn desc_without_installed_db() {
        let text = "%NAME%\ngitkraken\n\n%VERSION%\n12.6.1-1\n";
        let (name, pkg) = parse_desc(text);
        assert_eq!(name.as_deref(), Some("gitkraken"));
        let pkg = pkg.expect("pkg");
        assert_eq!(pkg.version, "12.6.1-1");
        assert_eq!(pkg.repo, None);
        assert!(pkg.explicit, "no REASON field means explicitly installed");
        assert_eq!((pkg.build_date, pkg.packager.as_str()), (None, ""));
        assert!(pkg.validation.is_empty() && !pkg.unsigned_file(), "unrecorded is not unsigned");
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
