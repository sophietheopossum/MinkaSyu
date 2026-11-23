//! Compare: the source trees behind two builds of one package, side by
//! side in meld (cachyos-v4/nvidia-settings against
//! extra-x86-64-v4/nvidia-settings, aur/lib32-libcaca against
//! minka/lib32-libcaca).
//!
//! Who built a package decides where its tree is (classify):
//! - Arch ([core], [extra], [multilib]): its GitLab packaging project at
//!   the tag of its version. Arch's dbscripts refuse a package whose
//!   PKGBUILD is not the one at that signed tag, so the tree is exact by
//!   construction.
//! - ALHP, and CachyOS's rebuilds of Arch ([cachyos-core-*],
//!   [cachyos-extra-*]): the same Arch tag, for the version less its ".N"
//!   rebuild suffix, with the pkgrel line rewritten to the suffixed one
//!   (and, by ALHP since 21/9/2026, `<=` bounds in conflicts and replaces
//!   raised). That is all either changes in the tree; the rest is build
//!   flags.
//! - CachyOS's own packages ([cachyos], [cachyos-v4], ...): CachyOS's
//!   GitHub repositories, at the commit whose PKGBUILD hashes to the
//!   build's .BUILDINFO pkgbuild_sha256sum; for [cachyos], the AUR's
//!   history too, which it built from before cachyos-aur-derived began.
//! - The AUR: the package base's git repository.
//! - [minka]: MinkaPKG, the checkout minkarepo lives in, read only.
//!
//! Classification goes by repository and packager, never by architecture
//! or the ".N" suffix: CachyOS's v4 repositories hold x86_64 files with
//! ".1" suffixes too, exactly like ALHP's.
//!
//! What is downloaded is data: nothing here sources, runs or builds a
//! PKGBUILD, which is only ever read as text, and a symlink in a tree is
//! turned into a file saying where it pointed (meld follows symlinks).
//! Each tree is written afresh to the cache dir's
//! compare/<repo>/<pkgbase>/<version>/, with a MINKA-SYU-SOURCE.txt on top
//! saying where it came from and how sure that is (one name on both sides,
//! so meld lines the two up). Git clones are kept under compare/git/ and
//! fetched again when older than FETCH_AFTER. compare/.lock lets one
//! compare at a time write there, whichever process runs it.

use crate::{alpm, check, sources, valid_package_name};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

/// The file at the top of each tree that says where it came from.
const PROVENANCE_FILE: &str = "MINKA-SYU-SOURCE.txt";

/// How long a cached clone is used before it is fetched again.
const FETCH_AFTER: Duration = Duration::from_secs(10 * 60);

/// How long after a build a commit naming it can land: CachyOS committed
/// lib32-zlib-ng 28 h after building it.
const COMMIT_SLACK: i64 = 2 * 86400;

/// How many of a repository's servers are asked for a .BUILDINFO.
const MIRRORS_TRIED: usize = 8;

const ARCH_GITLAB: &str = "https://gitlab.archlinux.org/archlinux/packaging/packages";

/// One repository's build of a package (the AUR's: its current version).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Build {
    pub repo: String,
    pub name: String,
    pub pkgbase: String,
    pub version: String,
    pub packager: String,
    /// Unix seconds. For the AUR, the installed build's when it is this
    /// version and no repository's.
    pub build_date: Option<i64>,
    /// The package file (%FILENAME%); empty for the AUR.
    pub filename: String,
}

/// Who built it, which decides where its tree is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Origin {
    Arch,
    Alhp,
    CachyosRebuild,
    CachyosOwn,
    Aur,
    Minka,
}

/// One side of a comparison, as the CLI prints it and serve sends it.
#[derive(Debug, Clone, Serialize)]
pub struct Side {
    pub repo: String,
    pub version: String,
    pub dir: String,
    /// How sure the tree is ("hash-verified: …", "approximate: …").
    pub matched: String,
    /// MINKA-SYU-SOURCE.txt, line by line.
    pub provenance: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Comparison {
    pub package: String,
    pub left: Side,
    pub right: Side,
}

/// Where a tree came from, for the provenance file.
struct Found {
    source: String,
    revision: String,
    matched: String,
    flags: String,
}

/// Writes the trees of `package` as `left` and `right` build it ("aur" for
/// the AUR's current version) and says where each came from.
pub fn compare(package: &str, left: &str, right: &str) -> Result<Comparison, String> {
    for (what, name) in [("package", package), ("repository", left), ("repository", right)] {
        if !valid_package_name(name) {
            return Err(format!("invalid {what} name: {name:?}"));
        }
    }
    if left == right {
        return Err(format!("{left} twice: compare two repositories' builds"));
    }
    // Both builds are looked up first: a repository without the package,
    // or one with no known source, fails before anything is fetched.
    let describe = |repo: &str| if repo == "aur" { describe_aur(package) } else { describe_sync(repo, package) };
    let (left, right) = (describe(left)?, describe(right)?);
    let (left_origin, right_origin) = (classify(&left)?, classify(&right)?);
    let _lock = lock_compares()?;
    Ok(Comparison { package: package.to_string(), left: side(left, left_origin)?, right: side(right, right_origin)? })
}

/// Waits for any other compare to finish, the CLI's or another serve's:
/// two at once would clone into one .part directory, or empty a tree the
/// other is writing (or meld is about to show). Held until dropped.
fn lock_compares() -> Result<std::fs::File, String> {
    let root = compare_root();
    std::fs::create_dir_all(&root).map_err(|e| format!("creating {}: {e}", root.display()))?;
    let path = root.join(".lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| format!("opening {}: {e}", path.display()))?;
    file.lock().map_err(|e| format!("locking {}: {e}", path.display()))?;
    Ok(file)
}

fn side(build: Build, origin: Origin) -> Result<Side, String> {
    let dest = fresh_tree_dir(&build)?;
    let found = match write_tree(&build, origin, &dest).and_then(|found| defuse_symlinks(&dest).map(|()| found)) {
        Ok(found) => found,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dest);
            return Err(format!("{}/{}: {e}", build.repo, build.name));
        }
    };
    let provenance = provenance_lines(&build, &found);
    write_new(&dest, PROVENANCE_FILE, (provenance.join("\n") + "\n").as_bytes())?;
    Ok(Side {
        repo: build.repo,
        version: build.version,
        dir: dest.display().to_string(),
        matched: found.matched,
        provenance,
    })
}

fn write_tree(b: &Build, origin: Origin, dest: &Path) -> Result<Found, String> {
    match origin {
        Origin::Aur => aur_tree(b, dest),
        Origin::Minka => minka_tree(b, buildinfo(b).as_ref(), dest),
        Origin::CachyosOwn => cachyos_tree(b, buildinfo(b).as_ref(), dest),
        Origin::Arch | Origin::Alhp | Origin::CachyosRebuild => arch_tree(b, origin, buildinfo(b).as_ref(), dest),
    }
}

/// meld on the two trees, detached like the terminals main.rs opens: a
/// process group of its own (closing the window must not take meld with
/// it), no stdio (stdout is serve's pipe), and a thread to reap it.
pub fn open_meld(c: &Comparison) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    let mut child = Command::new("meld")
        .arg(&c.left.dir)
        .arg(&c.right.dir)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("starting meld: {e} (meld is optional: pacman -S meld)"))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

// -- which build, and who made it -----------------------------------------

/// The print format of describe_sync: name, repo, version, pkgbase,
/// packager, build date, file name.
const DESCRIBE_FORMAT: &str = "%n|%r|%v|%e|%p|%b|%f";

/// A repository's build of `name`: one `pacman -Sp` (print mode: no root,
/// no lock, no download) on the check's private DB when there is one, as
/// the window's repository list reads it, else the system's.
fn describe_sync(repo: &str, name: &str) -> Result<Build, String> {
    let mut cmd = Command::new("pacman");
    cmd.args(["-Sp", "-dd", "--logfile", "/dev/null", "--print-format", DESCRIBE_FORMAT]);
    let db = check::checkdb_path();
    if db.join("sync").exists() {
        cmd.arg("--dbpath").arg(&db);
    }
    let out = cmd
        .arg(format!("{repo}/{name}"))
        .env("LC_ALL", "C")
        .env("TZ", "UTC0")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("running pacman -Sp: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let last = err.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("pacman -Sp failed").trim();
        return Err(format!("{repo}/{name}: {}", last.trim_start_matches("error: ")));
    }
    parse_describe(&String::from_utf8_lossy(&out.stdout)).ok_or_else(|| format!("{repo}/{name}: pacman printed no build"))
}

/// A DESCRIBE_FORMAT line.
pub fn parse_describe(text: &str) -> Option<Build> {
    let f: Vec<&str> = text.lines().next()?.split('|').collect();
    let [name, repo, version, pkgbase, packager, date, filename] = f.as_slice() else {
        return None;
    };
    if name.is_empty() || repo.is_empty() || version.is_empty() {
        return None;
    }
    Some(Build {
        repo: repo.to_string(),
        name: name.to_string(),
        pkgbase: if pkgbase.is_empty() { name } else { pkgbase }.to_string(),
        version: version.to_string(),
        packager: packager.to_string(),
        build_date: check::parse_build_date(date),
        filename: filename.to_string(),
    })
}

/// The AUR's current version of `name`, from its RPC. The installed
/// package lends its build date when it is that version and none of the
/// repositories' builds (paru's build of it): the commit choice goes by it.
fn describe_aur(name: &str) -> Result<Build, String> {
    let (pkgbase, version) = parse_aur_base(&check::aur_info(&[name.to_string()])?, name)?;
    let desc = Path::new("/var/lib/pacman/local").join(format!("{name}-{version}")).join("desc");
    let installed = std::fs::read_to_string(desc).ok().and_then(|t| alpm::parse_desc(&t).1);
    let (packager, build_date) = match installed {
        Some(p) if p.repo.is_none() && !repo_packager(&p.packager) => (p.packager, p.build_date),
        _ => (String::new(), None),
    };
    Ok(Build { repo: "aur".into(), name: name.into(), pkgbase, version, packager, build_date, filename: String::new() })
}

/// An AUR RPC info answer's package base and version for `name`.
pub fn parse_aur_base(text: &str, name: &str) -> Result<(String, String), String> {
    let v: serde_json::Value = serde_json::from_str(text).map_err(|e| format!("unreadable AUR answer: {e}"))?;
    if v.get("type").and_then(|t| t.as_str()) == Some("error") {
        return Err(format!("the AUR answered: {}", v.get("error").and_then(|e| e.as_str()).unwrap_or("an error")));
    }
    let hit = v
        .get("results")
        .and_then(|r| r.as_array())
        .and_then(|r| r.iter().find(|r| r.get("Name").and_then(|n| n.as_str()) == Some(name)))
        .ok_or_else(|| format!("the AUR has no package {name}"))?;
    let field = |k: &str| hit.get(k).and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let base = field("PackageBase");
    Ok((if base.is_empty() { name.to_string() } else { base }, field("Version")))
}

/// A packager that only a repository's builds carry.
fn repo_packager(packager: &str) -> bool {
    packager.ends_with("@archlinux.org>") || packager.starts_with("ALHP ") || packager.ends_with("@cachyos.org>")
}

/// The level a CachyOS repository of its own builds for: cachyos-v4 is
/// "v4"; plain [cachyos] has none.
fn cachyos_level(repo: &str) -> Option<&str> {
    repo.strip_prefix("cachyos-").filter(|l| matches!(*l, "v3" | "v4" | "znver4"))
}

/// Who built `b`: by its packager and repository, never by its
/// architecture or ".N" suffix (both ALHP and CachyOS's v4 repositories
/// have x86_64 files with ".1").
pub fn classify(b: &Build) -> Result<Origin, String> {
    let repo = b.repo.as_str();
    if b.packager.starts_with("ALHP ") {
        Ok(Origin::Alhp)
    } else if repo.starts_with("cachyos-core-") || repo.starts_with("cachyos-extra-") {
        Ok(Origin::CachyosRebuild)
    } else if repo == "cachyos" || cachyos_level(repo).is_some() {
        Ok(Origin::CachyosOwn)
    } else if repo == "aur" {
        Ok(Origin::Aur)
    } else if repo == "minka" {
        Ok(Origin::Minka)
    } else if matches!(repo, "core" | "extra" | "multilib") || b.packager.ends_with("@archlinux.org>") {
        Ok(Origin::Arch)
    } else {
        Err(format!("no known source for {repo} (its {} is built by {})", b.name, b.packager))
    }
}

// -- version text -----------------------------------------------------------

/// Arch's git tag for a version: the first ':' (the epoch's) as '-', and
/// every '~' as '.' (devtools' get_tag_from_pkgver).
pub fn arch_tag(version: &str) -> String {
    version.replacen(':', "-", 1).replace('~', ".")
}

/// Arch's GitLab project for a pkgbase, devtools'
/// gitlab_project_name_to_path, in its order: a '+' between an
/// alphanumeric run and a letter becomes '-' (dvd+rw-tools), every other
/// '+' "plus" (libsigc++), anything but [A-Za-z0-9_.-] '-', a run of two or
/// more '_'/'-' one '-', and "tree" "unix-tree" (a name GitLab reserves).
pub fn gitlab_project(pkgbase: &str) -> String {
    let s: Vec<char> = pkgbase.chars().collect();
    // s/([a-zA-Z0-9]+)\+([a-zA-Z]+)/\1-\2/g: a match ends where its
    // letters do, and the next one is looked for from there.
    let mut words = String::new();
    let mut i = 0;
    while i < s.len() {
        let run = s[i..].iter().position(|c| !c.is_ascii_alphanumeric()).map_or(s.len(), |p| i + p);
        if run > i && s.get(run) == Some(&'+') && s.get(run + 1).is_some_and(|c| c.is_ascii_alphabetic()) {
            let end = s[run + 1..].iter().position(|c| !c.is_ascii_alphabetic()).map_or(s.len(), |p| run + 1 + p);
            words.extend(&s[i..run]);
            words.push('-');
            words.extend(&s[run + 1..end]);
            i = end;
        } else {
            words.push(s[i]);
            i += 1;
        }
    }
    let mut out = String::new();
    for c in words.replace('+', "plus").chars() {
        let c = if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') { c } else { '-' };
        if matches!(c, '_' | '-') && out.ends_with(['_', '-']) {
            // The run collapses to one '-', whatever it started with.
            out.pop();
            out.push('-');
        } else {
            out.push(c);
        }
    }
    if out == "tree" { "unix-tree".to_string() } else { out }
}

/// The version a rebuild was made from and its own pkgrel: "5.44.0-1.2"
/// is Arch's 5.44.0-1, rebuilt as pkgrel 1.2. None without a ".N".
pub fn split_rebuild(version: &str) -> Option<(String, String)> {
    let (head, pkgrel) = version.rsplit_once('-')?;
    let (base, n) = pkgrel.rsplit_once('.')?;
    if base.is_empty() || n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((format!("{head}-{base}"), pkgrel.to_string()))
}

/// The PKGBUILD with every top-level `pkgrel=` line (quoted or spaced or
/// not) rewritten to `pkgrel=<pkgrel>`, the one edit ALHP and CachyOS make
/// to Arch's tree. None without such a line.
pub fn rewrite_pkgrel(text: &str, pkgrel: &str) -> Option<String> {
    let mut hit = false;
    let lines: Vec<String> = text
        .split('\n')
        .map(|line| {
            if line.strip_prefix("pkgrel").is_some_and(|r| r.trim_start_matches([' ', '\t']).starts_with('=')) {
                hit = true;
                format!("pkgrel={pkgrel}")
            } else {
                line.to_string()
            }
        })
        .collect();
    hit.then(|| lines.join("\n"))
}

/// ALHP's other edit, made since 21/9/2026 (ALHP.GO's
/// versionbound_rewrite): in conflicts and replaces arrays, each whole
/// `name<=V-R` with a plain-number R becomes `name<V-(R+1)`, so the bound
/// no longer covers its own ".N" rebuild of V-R. Like ALHP, all or nothing:
/// a bound it could not have found as written (`$pkgver` in it) leaves the
/// text as it is. None when nothing changes.
pub fn raise_bounds(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut changed = false;
    while let Some(open) = bound_array(rest) {
        let len = array_len(&rest[open..]);
        out.push_str(&rest[..open]);
        let body = &rest[open..open + len];
        let mut token = String::new();
        for c in body.chars().chain(std::iter::once('\n')) {
            if matches!(c, '\'' | '"' | '(' | ')' | ' ' | '\t' | '\n' | '\r') {
                match raise_bound(&token) {
                    Some(Some(raised)) => {
                        out.push_str(&raised);
                        changed = true;
                    }
                    Some(None) => return None,
                    None => out.push_str(&token),
                }
                token.clear();
                out.push(c);
            } else {
                token.push(c);
            }
        }
        out.pop(); // the '\n' chained on
        rest = &rest[open + len..];
    }
    out.push_str(rest);
    changed.then_some(out)
}

/// A token's bound raised; Some(None) for a bound written with a variable
/// (which ALHP, reading values from the .SRCINFO, cannot find as written),
/// None for anything else.
fn raise_bound(token: &str) -> Option<Option<String>> {
    let (name, bound) = token.split_once("<=")?;
    if token.contains(['$', '`']) {
        return Some(None);
    }
    let (version, rel) = bound.rsplit_once('-')?;
    let plain = !name.is_empty()
        && !name.ends_with(".so")
        && !version.is_empty()
        && (1..=9).contains(&rel.len())
        && rel.bytes().all(|b| b.is_ascii_digit());
    plain.then(|| rel.parse::<u64>().ok().map(|r| format!("{name}<{version}-{}", r + 1)))
}

/// Where the next conflicts or replaces array (also conflicts_<name>, and
/// inside package functions) opens: the index just after its '('.
fn bound_array(text: &str) -> Option<usize> {
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        let start = line.trim_start_matches([' ', '\t']);
        let open = ["conflicts", "replaces"].iter().find_map(|key| {
            let r = start.strip_prefix(key)?;
            let r = match r.strip_prefix('_') {
                Some(suffix) => suffix.trim_start_matches(|c: char| c.is_ascii_alphanumeric() || c == '_'),
                None => r,
            };
            let r = r.trim_start_matches([' ', '\t']);
            let r = r.strip_prefix('+').unwrap_or(r).strip_prefix('=')?;
            r.trim_start_matches([' ', '\t']).strip_prefix('(')
        });
        if let Some(r) = open {
            return Some(at + line.len() - r.len());
        }
        at += line.len();
    }
    None
}

/// The length of an array's body, up to its closing ')' outside quotes
/// and comments (the rest of the text when it has none).
fn array_len(body: &str) -> usize {
    let b = body.as_bytes();
    let (mut i, mut quote) = (0, None);
    while i < b.len() {
        match (quote, b[i]) {
            (Some(b'"'), b'\\') => i += 1,
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, c @ (b'\'' | b'"')) => quote = Some(c),
            (None, b'\\') => i += 1,
            (None, b'#') if i == 0 || b[i - 1].is_ascii_whitespace() || b[i - 1] == b'(' => {
                i = body[i..].find('\n').map_or(b.len(), |n| i + n);
                continue;
            }
            (None, b')') => return i,
            _ => {}
        }
        i += 1;
    }
    b.len()
}

/// A top-level array's words (`key=(…)` at the start of a line), quotes
/// off and comments dropped. None when there is none, it is never closed,
/// or it is added to (`key+=`), so that no one place has all of it.
pub fn top_array(text: &str, key: &str) -> Option<Vec<String>> {
    if text.lines().any(|l| l.trim_start().starts_with(&format!("{key}+="))) {
        return None;
    }
    let open = format!("{key}=(");
    let at = if text.starts_with(&open) { 0 } else { text.find(&format!("\n{open}"))? + 1 } + open.len();
    let mut words = Vec::new();
    let (mut word, mut in_word, mut quote) = (String::new(), false, None);
    let mut chars = text[at..].chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => word.push(c),
            (None, '\'' | '"') => (quote, in_word) = (Some(c), true),
            (None, ')') => {
                if in_word {
                    words.push(word);
                }
                return Some(words);
            }
            (None, '#') if !in_word => {
                chars.by_ref().find(|&c| c == '\n');
            }
            (None, '\\') => {
                if let Some(next) = chars.next().filter(|&n| n != '\n') {
                    word.push(next);
                    in_word = true;
                }
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            (None, c) => {
                word.push(c);
                in_word = true;
            }
        }
    }
    None
}

/// The local files a PKGBUILD's source array lists, each with the sum its
/// sha256sums (else b2sums) array gives: (file, the tool that sums it,
/// sum). URLs, names it computes and SKIP are left out. None when the
/// arrays cannot be paired as text (missing, added to, or of different
/// lengths).
pub fn local_checksums(text: &str) -> Option<Vec<(String, &'static str, String)>> {
    let sources = top_array(text, "source")?;
    let (tool, sums) =
        [("sha256sums", "sha256sum"), ("b2sums", "b2sum")].into_iter().find_map(|(key, tool)| Some((tool, top_array(text, key)?)))?;
    if sums.len() != sources.len() {
        return None;
    }
    let files = sources.into_iter().zip(sums).filter_map(|(source, sum)| {
        if source.contains("://") || sum.eq_ignore_ascii_case("SKIP") || sum.contains('$') {
            return None;
        }
        // makepkg looks a local file up by its name alone.
        let name = source.split("::").next()?.rsplit('/').next()?;
        let usable = !name.is_empty() && name != "." && name != ".." && !name.contains(['$', '`']);
        usable.then(|| (name.to_string(), tool, sum.to_ascii_lowercase()))
    });
    Some(files.collect())
}

/// CachyOS's builder turns a PKGBUILD's level switch on for the repository
/// it builds for (go's `_optimize_v4=${_optimize_v4-}` was built as
/// `…-y}`), as its kernel builder does with more of them.
pub fn toggle_level(text: &str, level: &str) -> Option<String> {
    let off = format!("_optimize_{level}=${{_optimize_{level}-}}");
    let on = format!("_optimize_{level}=${{_optimize_{level}-y}}");
    text.contains(&off).then(|| text.replace(&off, &on))
}

fn full_version(epoch: Option<&str>, pkgver: &str, pkgrel: &str) -> String {
    match epoch.filter(|e| !e.is_empty() && *e != "0") {
        Some(e) => format!("{e}:{pkgver}-{pkgrel}"),
        None => format!("{pkgver}-{pkgrel}"),
    }
}

/// [epoch:]pkgver-pkgrel from a .SRCINFO's pkgbase section, by key (epoch
/// comes after pkgrel there).
pub fn srcinfo_version(text: &str) -> Option<String> {
    let (mut epoch, mut pkgver, mut pkgrel) = (None, None, None);
    for line in text.lines() {
        if line.starts_with("pkgname") {
            break;
        }
        match line.trim().split_once(" = ") {
            Some(("epoch", v)) => epoch = epoch.or(Some(v)),
            Some(("pkgver", v)) => pkgver = pkgver.or(Some(v)),
            Some(("pkgrel", v)) => pkgrel = pkgrel.or(Some(v)),
            _ => {}
        }
    }
    Some(full_version(epoch, pkgver?, pkgrel?))
}

/// A literal top-level assignment's value, quotes off; None for anything
/// computed.
fn assigned(rest: &str) -> Option<&str> {
    let rest = rest.trim();
    let v = match rest.chars().next()? {
        q @ ('\'' | '"') => rest[1..].split(q).next()?,
        _ => rest.split_whitespace().next()?,
    };
    (!v.is_empty() && !v.contains(['$', '`', '(', ')', ' '])).then_some(v)
}

/// The same from a PKGBUILD's top-level assignments, read as text: None
/// when pkgver or pkgrel is computed (`${_major}.${_minor}`).
pub fn pkgbuild_version(text: &str) -> Option<String> {
    let (mut epoch, mut pkgver, mut pkgrel) = (None, None, None);
    for line in text.lines() {
        let Some((key, rest)) = line.split_once('=') else { continue };
        let slot = match key {
            "epoch" => &mut epoch,
            "pkgver" => &mut pkgver,
            "pkgrel" => &mut pkgrel,
            _ => continue,
        };
        if slot.is_none() {
            *slot = Some(assigned(rest));
        }
    }
    Some(full_version(epoch.flatten(), pkgver??, pkgrel??))
}

/// The pkgbase a PKGBUILD declares: pkgbase=, else its first pkgname.
pub fn declared_pkgbase(text: &str) -> Option<String> {
    let value = |key: &str| text.lines().find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix('=')));
    if let Some(base) = value("pkgbase").and_then(assigned) {
        return Some(base.to_string());
    }
    let names = value("pkgname")?.trim();
    let first = names.strip_prefix('(').unwrap_or(names).split([' ', ')']).find(|w| !w.is_empty())?;
    assigned(first).map(str::to_string)
}

/// Unix seconds in her date form, "22/11/2026 10:19 UTC".
pub fn utc_date(secs: i64) -> String {
    // Howard Hinnant's civil_from_days, check.rs' days_from_civil reversed.
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{day}/{month}/{year} {:02}:{:02} UTC", rem / 3600, rem % 3600 / 60)
}

// -- which commit -------------------------------------------------------------

/// A commit that changed a PKGBUILD (git log --raw).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Revision {
    pub commit: String,
    /// Committer time, Unix seconds.
    pub time: i64,
    /// The PKGBUILD's path in that commit.
    pub path: String,
    pub blob: String,
    /// It hashes (as the builder edits it) to the build's
    /// pkgbuild_sha256sum.
    pub hash_match: bool,
    /// Its text names the build's version.
    pub version_match: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pick {
    Hash(usize),
    Version(usize),
    /// Nothing names the version: the newest commit before the build.
    Date(usize),
}

/// Which revision a build came from: one whose PKGBUILD hashes to it, else
/// one naming its version, else the newest from before the build. Among
/// several of the first two, the newest at or before the build date plus
/// `slack` (CachyOS's commits can trail its builds), else the oldest after
/// it (MinkaPKG's commits follow her builds), or the newest when no date
/// is known. The slack is only for commits that name the build: with the
/// date alone to go by, a later commit is as likely the next change
/// (linux-cachyos-lts 6.18.64-1, 9 h after 6.18.60-3 was built).
pub fn pick_revision(revs: &[Revision], built: Option<i64>, slack: i64) -> Option<Pick> {
    let latest = built.map(|d| d + slack);
    let closest = |wanted: &dyn Fn(&Revision) -> bool| {
        let mut before: Option<usize> = None;
        let mut after: Option<usize> = None;
        for (i, r) in revs.iter().enumerate().filter(|(_, r)| wanted(r)) {
            if latest.is_none_or(|l| r.time <= l) {
                if before.is_none_or(|b| r.time > revs[b].time) {
                    before = Some(i);
                }
            } else if after.is_none_or(|a| r.time < revs[a].time) {
                after = Some(i);
            }
        }
        before.or(after)
    };
    if let Some(i) = closest(&|r| r.hash_match) {
        return Some(Pick::Hash(i));
    }
    if let Some(i) = closest(&|r| r.version_match) {
        return Some(Pick::Version(i));
    }
    let built = built?;
    let mut best: Option<usize> = None;
    for (i, r) in revs.iter().enumerate().filter(|(_, r)| r.time <= built) {
        if best.is_none_or(|b| r.time > revs[b].time) {
            best = Some(i);
        }
    }
    best.map(Pick::Date)
}

/// `git log --format='C %H %ct' --raw --no-abbrev` output: a revision for
/// each PKGBUILD a commit added or changed (the new path, for a rename).
pub fn parse_raw_log(text: &str) -> Vec<Revision> {
    let mut out = Vec::new();
    let mut commit: Option<(String, i64)> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("C ") {
            commit = rest.split_once(' ').and_then(|(c, t)| Some((c.to_string(), t.trim().parse().ok()?)));
        } else if let (Some(raw), Some((c, t))) = (line.strip_prefix(':'), &commit) {
            let Some((meta, paths)) = raw.split_once('\t') else { continue };
            let f: Vec<&str> = meta.split(' ').collect();
            if f.len() < 5 || f[4].starts_with('D') {
                continue;
            }
            out.push(Revision {
                commit: c.clone(),
                time: *t,
                path: paths.rsplit('\t').next().unwrap_or(paths).to_string(),
                blob: f[3].to_string(),
                ..Default::default()
            });
        }
    }
    out
}

/// What a build's .BUILDINFO says that matters here.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BuildInfo {
    pub pkgbuild_sha256: String,
    /// pkgver: the full version.
    pub version: String,
    pub build_date: Option<i64>,
    pub startdir: String,
    /// devtools, or makepkg for a plain build.
    pub buildtool: String,
    pub buildtoolver: String,
    /// `options = lto` or `!lto`.
    pub lto: Option<bool>,
    /// Where it was read.
    pub from: String,
}

impl BuildInfo {
    /// The pacman cache is keyed by file name, which two repositories'
    /// builds can share: only the same version and date count.
    fn is_of(&self, b: &Build) -> bool {
        self.version == b.version && (b.build_date.is_none() || self.build_date == b.build_date)
    }
}

/// None without a pkgbuild_sha256sum (a cut-off or foreign file).
pub fn parse_buildinfo(text: &str) -> Option<BuildInfo> {
    let mut bi = BuildInfo::default();
    for line in text.lines() {
        let Some((k, v)) = line.split_once(" = ") else { continue };
        let v = v.trim();
        match k.trim() {
            "pkgbuild_sha256sum" => bi.pkgbuild_sha256 = v.to_string(),
            "pkgver" => bi.version = v.to_string(),
            "builddate" => bi.build_date = v.parse().ok(),
            "startdir" => bi.startdir = v.to_string(),
            "buildtool" => bi.buildtool = v.to_string(),
            "buildtoolver" => bi.buildtoolver = v.to_string(),
            "options" if v == "lto" => bi.lto = Some(true),
            "options" if v == "!lto" => bi.lto = Some(false),
            _ => {}
        }
    }
    (bi.pkgbuild_sha256.len() == 64).then_some(bi)
}

/// The longest of `dirs` that `pkgbase` is named after: CachyOS's kernel
/// builder suffixes a directory's name (linux-cachyos-rt-bore-lto is built
/// from linux-cachyos-rt-bore).
pub fn longest_prefix_dir<'a>(dirs: impl Iterator<Item = &'a str>, pkgbase: &str) -> Option<&'a str> {
    dirs.filter(|d| pkgbase == *d || pkgbase.strip_prefix(*d).is_some_and(|r| r.starts_with('-'))).max_by_key(|d| d.len())
}

// -- the trees ----------------------------------------------------------------

/// Arch's, ALHP's or a CachyOS rebuild's tree: Arch's tag, with the pkgrel
/// a rebuild has.
fn arch_tree(b: &Build, origin: Origin, bi: Option<&BuildInfo>, dest: &Path) -> Result<Found, String> {
    let rebuild = if origin == Origin::Arch { None } else { split_rebuild(&b.version) };
    let upstream = rebuild.as_ref().map_or(b.version.as_str(), |(v, _)| v.as_str());
    let (project, tag) = (gitlab_project(&b.pkgbase), arch_tag(upstream));
    let url = format!("{ARCH_GITLAB}/{project}");
    let archive = scratch_file("tar.gz")?;
    let got = curl(&format!("{url}/-/archive/{tag}/{project}-{tag}.tar.gz"), &archive, &["--max-time", "60"]);
    let got = match got {
        Ok(200) => extract(&archive, dest, true),
        Ok(404) => Err(format!("Arch's {project} has no tag {tag}")),
        Ok(302) => Err(format!("Arch has no packaging project {project}")),
        Ok(code) => Err(format!("gitlab.archlinux.org answered {code}")),
        Err(e) => Err(e),
    };
    let _ = std::fs::remove_file(&archive);
    got?;

    let tagged = read_pkgbuild(dest)?;
    let rewritten = rebuild.as_ref().and_then(|(_, rel)| rewrite_pkgrel(&String::from_utf8_lossy(&tagged), rel));
    // ALHP's builds from 21/9/2026 on raise bounds too, older ones do not
    // (totem-pl-parser 3.26.7-1.1): the hash tells which this one is.
    let raised = rewritten.as_deref().filter(|_| origin == Origin::Alhp).and_then(raise_bounds);
    let who = if origin == Origin::Alhp { "ALHP" } else { "CachyOS" };
    let mut variants = Vec::new();
    if let Some((_, rel)) = &rebuild {
        if let Some(text) = &raised {
            variants.push((text.clone().into_bytes(), format!("with pkgrel={rel} and its conflicts/replaces bounds raised")));
        }
        if let Some(text) = &rewritten {
            variants.push((text.clone().into_bytes(), format!("with pkgrel={rel}")));
        }
    }
    variants.push((tagged.clone(), "as tagged".to_string()));
    let verified = match bi {
        Some(bi) => hashed_variant(variants, &bi.pkgbuild_sha256)?,
        None => None,
    };
    // The tree shows what was built: the variant the hash proves, else
    // the rewrite every such build makes.
    let rewritten = rewritten.map(String::into_bytes);
    let shown = verified.as_ref().map(|(t, _)| t).or(rewritten.as_ref()).unwrap_or(&tagged);
    if *shown != tagged {
        write_new(dest, "PKGBUILD", shown)?;
    }

    let how = match (&rebuild, origin) {
        (_, Origin::Arch) => format!(
            "exact by construction (Arch's repositories take a package only with the PKGBUILD at its signed tag, {tag})"
        ),
        (Some((_, rel)), _) => format!("Arch's tag {tag} with pkgrel rewritten to {rel}, as {who} builds it"),
        (None, _) => format!("Arch's tag {tag} (the version has no rebuild suffix)"),
    };
    let check = match (bi, &verified) {
        (Some(bi), Some((_, what))) => format!("hash-verified against the .BUILDINFO from {} (the PKGBUILD {what})", bi.from),
        (Some(bi), None) => format!(
            "NOT verified: {}'s .BUILDINFO names another PKGBUILD{}",
            bi.from,
            if origin == Origin::CachyosRebuild { " (CachyOS sometimes builds edits it does not publish)" } else { "" }
        ),
        (None, _) => format!(
            "not hash-verified: no .BUILDINFO found (not in the pacman cache or on the mirrors tried){}",
            if raised.is_some() {
                "; ALHP's builds since 21/9/2026 also raise its conflicts/replaces bounds, which this tree does not"
            } else {
                ""
            }
        ),
    };
    Ok(Found { source: url, revision: format!("tag {tag}"), matched: format!("{how}; {check}"), flags: flags_note(b, origin, bi) })
}

/// One repository's commits of a build's PKGBUILD, marked against it.
struct History {
    /// Its address, for the provenance file.
    url: String,
    git_dir: PathBuf,
    revs: Vec<Revision>,
    /// Each blob that hashes to the build's PKGBUILD, as the builder
    /// edited it, and how.
    built: HashMap<String, (Vec<u8>, String)>,
    /// The AUR's, which [cachyos] built from.
    aur: bool,
}

/// CachyOS's own package: the commit of CachyOS-PKGBUILDS, linux-cachyos
/// (kernels) or cachyos-aur-derived its PKGBUILD came from, or for
/// [cachyos] the AUR's. Every repository is searched, as a pkgbase can be
/// in more than one (an old clipman in CachyOS-PKGBUILDS, the one built in
/// cachyos-aur-derived): a hash match anywhere beats a commit naming the
/// version, which beats the date; the list order breaks ties.
fn cachyos_tree(b: &Build, bi: Option<&BuildInfo>, dest: &Path) -> Result<Found, String> {
    let kernel = b.pkgbase.starts_with("linux-cachyos");
    let repos: &[&str] = if kernel { &["linux-cachyos"] } else { &["CachyOS-PKGBUILDS", "cachyos-aur-derived"] };
    let mut found: Vec<(History, Pick)> = Vec::new();
    let mut too_new = Vec::new();
    for name in repos {
        let url = format!("https://github.com/CachyOS/{name}");
        let git_dir = compare_root().join("git").join(format!("{name}.git"));
        mirror(&format!("{url}.git"), &git_dir, true)?;
        let paths = if kernel {
            let top = run_text(git(&git_dir).args(["ls-tree", "--name-only", "HEAD"]))?;
            longest_prefix_dir(top.lines(), &b.pkgbase).map(|d| vec![format!("{d}/PKGBUILD")]).unwrap_or_default()
        } else {
            pkgbase_paths(&git_dir, &b.pkgbase)?
        };
        if paths.is_empty() {
            continue;
        }
        let mut revs = path_history(&git_dir, "--branches", &paths, false)?;
        // A commit from after the build (and its slack) cannot be it.
        revs.retain(|r| b.build_date.is_none_or(|d| r.time <= d + COMMIT_SLACK));
        // Kernel PKGBUILDs are edited per variant as they are built, so no
        // commit hashes to one: only the date can place them.
        let built = if kernel { HashMap::new() } else { mark_revisions(&git_dir, &mut revs, b, bi, true)? };
        match pick_revision(&revs, b.build_date, COMMIT_SLACK) {
            Some(pick) => found.push((History { url, git_dir, revs, built, aur: false }, pick)),
            None => too_new.push(format!("{url} has {}", paths.join(", "))),
        }
    }
    let rank = |p: &Pick| match p {
        Pick::Hash(_) => 0,
        Pick::Version(_) => 1,
        Pick::Date(_) => 2,
    };
    let best = found.iter().map(|(_, p)| rank(p)).min().unwrap_or(3);
    // [cachyos] built AUR PKGBUILDs before cachyos-aur-derived began
    // (23/6/2026), some with the pkgrel rewritten: worth a look when no
    // CachyOS commit is the build, or names its version.
    let mut aur_note = String::new();
    if b.repo == "cachyos" && !kernel && (best > 1 || (best == 1 && bi.is_some())) {
        match cachyos_from_aur(b, bi) {
            Ok(Some(hit)) => found.push(hit),
            Ok(None) => aur_note = format!(", and no commit of the AUR's {} is it", b.pkgbase),
            Err(e) => aur_note = format!(" ({e})"),
        }
    }
    let Some((h, pick)) = found.into_iter().min_by_key(|(_, p)| rank(p)) else {
        return Err(if too_new.is_empty() {
            format!("source not published by CachyOS: no {} in {}{aur_note}", b.pkgbase, repos.join(" or "))
        } else {
            format!("{} but no commit of it before the build{aur_note}", too_new.join(" and "))
        });
    };
    let rev = pick_rev(&h.revs, pick);
    let dir = rev.path.trim_end_matches("PKGBUILD").trim_end_matches('/');
    // A full clone (the AUR's) has every blob, so asking costs nothing.
    export(&h.git_dir, &rev.commit, dir, dest, true)?;
    let mut matched = match pick {
        Pick::Hash(_) => {
            let (text, how) = &h.built[&rev.blob];
            write_new(dest, "PKGBUILD", text)?;
            let which = format!("the PKGBUILD ({how}) is the one {}'s .BUILDINFO names", bi_from(bi));
            match unverified_files(dest, text)? {
                Some(bad) if !bad.is_empty() => {
                    format!("PKGBUILD exact: {which}; local files {} differ from what the build verified", bad.join(", "))
                }
                Some(_) => format!("hash-verified: {which}, and its local files match its checksums"),
                None => format!("hash-verified: {which}; the other files are as committed"),
            }
        }
        Pick::Version(_) => {
            format!("approximate: {}; the newest commit naming {} by the build date", no_hash(bi, "commit's"), b.version)
        }
        Pick::Date(_) if kernel => "approximate: CachyOS's kernel builder edits the PKGBUILD for each variant, so no \
                                    commit holds what it built; the newest commit before the build"
            .to_string(),
        Pick::Date(_) => format!("approximate: no commit names {}; the newest before the build", b.version),
    };
    if b.build_date.is_some_and(|d| rev.time > d) {
        matched += " (committed after the build: CachyOS's commits can trail its builds)";
    }
    if h.aur {
        matched += "; CachyOS built the AUR's PKGBUILD";
    }
    Ok(Found {
        source: if dir.is_empty() { h.url } else { format!("{} ({dir})", h.url) },
        revision: format!("{} ({})", rev.commit, utc_date(rev.time)),
        matched,
        flags: flags_note(b, Origin::CachyosOwn, bi),
    })
}

/// [cachyos]'s build of an AUR PKGBUILD, taken as it is or with the pkgrel
/// rewritten to the build's dotted one (xob 0.3-1.1): the AUR commit that
/// is the build, else the newest naming its version. None when no commit
/// is either: the AUR's package of that name may be another one.
fn cachyos_from_aur(b: &Build, bi: Option<&BuildInfo>) -> Result<Option<(History, Pick)>, String> {
    let aur = aur_history(&b.pkgbase, &b.version)?;
    let mut h = History { url: aur.url, git_dir: aur.git_dir, revs: aur.revs, built: HashMap::new(), aur: true };
    h.revs.retain(|r| b.build_date.is_none_or(|d| r.time <= d + COMMIT_SLACK));
    if let Some(bi) = bi {
        let rel = split_rebuild(&b.version).map(|(_, rel)| rel);
        let blobs: HashSet<&String> = h.revs.iter().map(|r| &r.blob).collect();
        for blob in blobs {
            let text = &aur.texts[blob];
            let mut variants = vec![(text.clone(), "as committed".to_string())];
            if let Some(rel) = &rel {
                if let Some(rewritten) = rewrite_pkgrel(&String::from_utf8_lossy(text), rel) {
                    variants.push((rewritten.into_bytes(), format!("with pkgrel={rel}")));
                }
            }
            if let Some(found) = hashed_variant(variants, &bi.pkgbuild_sha256)? {
                h.built.insert(blob.clone(), found);
            }
        }
    }
    for r in &mut h.revs {
        r.hash_match = h.built.contains_key(&r.blob);
    }
    let pick = pick_revision(&h.revs, b.build_date, COMMIT_SLACK).filter(|p| !matches!(p, Pick::Date(_)));
    Ok(pick.map(|p| (h, p)))
}

/// A package base's AUR git repository, cloned or fetched: a revision for
/// each commit with a PKGBUILD, marked when it names `version` (by its
/// .SRCINFO, else the PKGBUILD's text: commits imported from Arch's svn
/// era have no .SRCINFO), and each PKGBUILD's text by blob.
struct AurHistory {
    url: String,
    git_dir: PathBuf,
    /// The newest commit, the AUR's current tree.
    head: String,
    revs: Vec<Revision>,
    texts: HashMap<String, Vec<u8>>,
}

fn aur_history(pkgbase: &str, version: &str) -> Result<AurHistory, String> {
    let url = format!("https://aur.archlinux.org/{pkgbase}.git");
    let git_dir = compare_root().join("git").join("aur").join(format!("{pkgbase}.git"));
    mirror(&url, &git_dir, false)?;
    // An unknown package base clones as an empty repository.
    let log = run_text(git(&git_dir).args(["log", "--format=%H %ct", "HEAD"]))
        .map_err(|_| format!("the AUR has no git repository {pkgbase}"))?;
    let commits: Vec<(String, i64)> =
        log.lines().filter_map(|l| l.split_once(' ').and_then(|(c, t)| Some((c.to_string(), t.parse().ok()?)))).collect();
    let names: Vec<String> = commits.iter().flat_map(|(c, _)| [format!("{c}:.SRCINFO"), format!("{c}:PKGBUILD")]).collect();
    let mut objects = cat_files(&git_dir, &names)?.into_iter();
    let mut revs = Vec::new();
    let mut texts = HashMap::new();
    for (commit, time) in &commits {
        let (srcinfo, pkgbuild) = (objects.next().flatten(), objects.next().flatten());
        let Some((blob, text)) = pkgbuild else { continue };
        let named = srcinfo
            .and_then(|(_, s)| srcinfo_version(&String::from_utf8_lossy(&s)))
            .or_else(|| pkgbuild_version(&String::from_utf8_lossy(&text)));
        revs.push(Revision {
            commit: commit.clone(),
            time: *time,
            path: "PKGBUILD".into(),
            blob: blob.clone(),
            hash_match: false,
            version_match: named.as_deref() == Some(version),
        });
        texts.entry(blob).or_insert(text);
    }
    let head = commits.first().map(|(c, _)| c.clone()).unwrap_or_default();
    Ok(AurHistory { url, git_dir, head, revs, texts })
}

/// The AUR's tree of `b`: the commit a local build of it came from, else
/// the newest naming its version (by the installed build's date, when it
/// is paru's build).
fn aur_tree(b: &Build, dest: &Path) -> Result<Found, String> {
    let mut h = aur_history(&b.pkgbase, &b.version)?;
    let wants = local_build_hashes(b);
    // Each PKGBUILD blob a local build was made from, with that package file.
    let mut built: HashMap<&String, &str> = HashMap::new();
    if !wants.is_empty() {
        for (blob, text) in &h.texts {
            let hash = sha256(text)?;
            if let Some((_, file)) = wants.iter().find(|(want, _)| *want == hash) {
                built.insert(blob, file);
            }
        }
    }
    for r in &mut h.revs {
        r.hash_match = built.contains_key(&r.blob);
    }
    let pick =
        pick_revision(&h.revs, b.build_date, 0).ok_or_else(|| format!("no commit of the AUR's {} names {}", b.pkgbase, b.version))?;
    let rev = pick_rev(&h.revs, pick);
    export(&h.git_dir, &rev.commit, "", dest, false)?;
    let matched = match pick {
        Pick::Hash(_) => format!("hash-verified: the PKGBUILD is the one {}'s .BUILDINFO names", built[&rev.blob]),
        Pick::Version(_) if b.build_date.is_some() => {
            format!("approximate: the newest commit naming {} by your installed build's date", b.version)
        }
        Pick::Version(_) if rev.commit == h.head => "the AUR's current tree (HEAD)".to_string(),
        Pick::Version(_) => format!("the newest commit naming {}, the AUR's current version", b.version),
        Pick::Date(_) => format!("approximate: no commit names {}; the newest before your installed build", b.version),
    };
    Ok(Found {
        source: h.url,
        revision: format!("{} ({})", rev.commit, utc_date(rev.time)),
        matched,
        flags: flags_note(b, Origin::Aur, None),
    })
}

/// Local package files of `b` (the pacman cache, paru's clone directory),
/// read only: each .BUILDINFO's PKGBUILD hash, with the file's path.
fn local_build_hashes(b: &Build) -> Vec<(String, String)> {
    let prefix = format!("{}-{}-", b.name, b.version);
    let dirs = [PathBuf::from("/var/cache/pacman/pkg"), check::xdg_cache_home().join("paru/clone").join(&b.pkgbase)];
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let file = entry.file_name().to_string_lossy().into_owned();
            if !file.starts_with(&prefix) || !file.contains(".pkg.tar") || file.ends_with(".sig") {
                continue;
            }
            if let Some(bi) = read_buildinfo(&entry.path()).filter(|bi| bi.version == b.version) {
                out.push((bi.pkgbuild_sha256, entry.path().display().to_string()));
            }
        }
    }
    out
}

/// [minka]'s tree: the MinkaPKG commit whose PKGBUILD the build's
/// .BUILDINFO names, else the working tree when it is that PKGBUILD
/// (built before committing), else a commit naming the version. MinkaPKG
/// is only read: git log, cat-file and archive.
fn minka_tree(b: &Build, bi: Option<&BuildInfo>, dest: &Path) -> Result<Found, String> {
    let minkarepo = sources::find_minkarepo().ok_or("no minkarepo found, so no MinkaPKG checkout to read")?;
    let repo = std::fs::canonicalize(&minkarepo)
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .ok_or_else(|| format!("cannot resolve {}", minkarepo.display()))?;
    // The directory a build was made in, which is not always its name
    // (firestorm-nolvc is built in firestorm-novlc/).
    let dir = bi
        .and_then(|bi| bi.startdir.rsplit('/').next())
        .filter(|d| path_name(d))
        .unwrap_or(b.pkgbase.as_str())
        .to_string();
    // --follow: directories have been renamed (rbrowser-git to rbrowser).
    let mut revs = path_history(&repo, "HEAD", &[format!("{dir}/PKGBUILD")], true)?;
    mark_revisions(&repo, &mut revs, b, bi, false)?;
    let pick = pick_revision(&revs, b.build_date, 0);
    let source = format!("MinkaPKG ({})", repo.display());
    let flags = flags_note(b, Origin::Minka, bi);
    if !matches!(pick, Some(Pick::Hash(_))) {
        let worktree = repo.join(&dir).join("PKGBUILD");
        if let (Some(bi), Ok(text)) = (bi, read_regular(&worktree)) {
            if sha256(&text)? == bi.pkgbuild_sha256 {
                copy_worktree(&repo, &dir, dest)?;
                return Ok(Found {
                    source: format!("{source}, {dir}/"),
                    revision: "the working tree".to_string(),
                    matched: format!("uncommitted: no commit has the PKGBUILD {}'s .BUILDINFO names, but the working tree does", bi.from),
                    flags,
                });
            }
        }
    }
    let pick = pick.ok_or_else(|| format!("no MinkaPKG commit has {dir}/PKGBUILD at {}", b.version))?;
    let rev = pick_rev(&revs, pick);
    let commit_dir = rev.path.trim_end_matches("PKGBUILD").trim_end_matches('/');
    export(&repo, &rev.commit, commit_dir, dest, false)?;
    let matched = match pick {
        Pick::Hash(_) => format!("hash-verified: the PKGBUILD is the one {}'s .BUILDINFO names", bi_from(bi)),
        Pick::Version(_) => format!("unverified: {}; a commit naming {}", no_hash(bi, "commit's"), b.version),
        Pick::Date(_) => format!("inexact: no commit names {}; the newest by the build date", b.version),
    };
    Ok(Found {
        source: format!("{source}, {commit_dir}/"),
        revision: format!("{} ({})", rev.commit, utc_date(rev.time)),
        matched,
        flags,
    })
}

/// The files git knows of (or would) in MinkaPKG's `dir`, copied into
/// `dest`: not src/, pkg/, built packages or makepkg's clones, which
/// .gitignore keeps out.
fn copy_worktree(repo: &Path, dir: &str, dest: &Path) -> Result<(), String> {
    let list = run_text(git(repo).args(["ls-files", "-co", "--exclude-standard", "-z", "--"]).arg(dir))?;
    for path in list.split('\0').filter(|p| !p.is_empty()) {
        let Some(rel) = path.strip_prefix(dir).map(|r| r.trim_start_matches('/')) else { continue };
        let Ok(data) = read_regular(&repo.join(path)) else { continue };
        let target = dest.join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("creating {}: {e}", parent.display()))?;
        }
        std::fs::write(&target, data).map_err(|e| format!("writing {}: {e}", target.display()))?;
    }
    Ok(())
}

/// For each revision, whether its PKGBUILD (as the builder edits it)
/// hashes to the build's .BUILDINFO and whether it names the build's
/// version; returns the PKGBUILD text each matching blob was built as.
/// `cachyos`: a blobless clone of CachyOS's, whose builder may also have
/// switched the repository's level on.
fn mark_revisions(
    repo: &Path,
    revs: &mut [Revision],
    b: &Build,
    bi: Option<&BuildInfo>,
    cachyos: bool,
) -> Result<HashMap<String, (Vec<u8>, String)>, String> {
    let mut blobs: Vec<String> = revs.iter().map(|r| r.blob.clone()).collect();
    blobs.sort();
    blobs.dedup();
    // Only CachyOS's clones are blobless; MinkaPKG is never fetched into.
    if cachyos {
        fetch_blobs(repo, &blobs)?;
    }
    let level = if cachyos { cachyos_level(&b.repo) } else { None };
    let mut built = HashMap::new();
    let mut versions = HashMap::new();
    for (blob, object) in blobs.iter().zip(cat_files(repo, &blobs)?) {
        let Some((_, text)) = object else { continue };
        let words = String::from_utf8_lossy(&text).into_owned();
        versions.insert(blob.clone(), pkgbuild_version(&words));
        let Some(bi) = bi else { continue };
        let mut variants = vec![(text.clone(), "as committed".to_string())];
        if let Some((l, toggled)) = level.and_then(|l| Some((l, toggle_level(&words, l)?))) {
            variants.push((toggled.into_bytes(), format!("with _optimize_{l} switched on")));
        }
        if let Some(found) = hashed_variant(variants, &bi.pkgbuild_sha256)? {
            built.insert(blob.clone(), found);
        }
    }
    for r in revs.iter_mut() {
        r.hash_match = built.contains_key(&r.blob);
        r.version_match = versions.get(&r.blob).is_some_and(|v| v.as_deref() == Some(b.version.as_str()));
    }
    Ok(built)
}

fn pick_rev(revs: &[Revision], pick: Pick) -> &Revision {
    match pick {
        Pick::Hash(i) | Pick::Version(i) | Pick::Date(i) => &revs[i],
    }
}

fn bi_from(bi: Option<&BuildInfo>) -> &str {
    bi.map_or("the build", |bi| bi.from.as_str())
}

/// Why a tree is not hash-verified.
fn no_hash(bi: Option<&BuildInfo>, whose: &str) -> String {
    match bi {
        Some(bi) => format!("no {whose} PKGBUILD is the one {}'s .BUILDINFO names", bi.from),
        None => "no .BUILDINFO found (not in the pacman cache or on the mirrors tried)".to_string(),
    }
}

/// The PKGBUILDs of `pkgbase` in a CachyOS repository, at any depth and in
/// any branch's history (directories move, and some are deleted):
/// directories named after it, and variants of it (openblas/openblas-v4,
/// zstd/zstd-pgo) whose newest PKGBUILD declares it its pkgbase.
fn pkgbase_paths(repo: &Path, pkgbase: &str) -> Result<Vec<String>, String> {
    let named = format!(":(glob)**/{pkgbase}/PKGBUILD");
    let variant = format!(":(glob)**/{pkgbase}-*/PKGBUILD");
    let log = run_text(git(repo).args(["log", "--branches", "--format=", "--name-only", "--no-renames", "--", &named, &variant]))?;
    let mut seen = HashSet::new();
    let paths: Vec<String> = log.lines().filter(|l| !l.is_empty() && seen.insert(*l)).map(str::to_string).collect();
    let is_named = |p: &String| p.trim_end_matches("/PKGBUILD").rsplit('/').next() == Some(pkgbase);
    let (mut out, variants): (Vec<String>, Vec<String>) = paths.into_iter().partition(is_named);
    if variants.is_empty() {
        return Ok(out);
    }
    // Each variant's newest PKGBUILD says whose it is.
    let mut newest: HashMap<String, String> = HashMap::new();
    for r in path_history(repo, "--branches", &variants, false)? {
        newest.entry(r.path).or_insert(r.blob);
    }
    let blobs: Vec<String> = newest.values().cloned().collect();
    fetch_blobs(repo, &blobs)?;
    let declared: HashMap<&String, Option<String>> = blobs
        .iter()
        .zip(cat_files(repo, &blobs)?)
        .map(|(blob, o)| (blob, o.and_then(|(_, t)| declared_pkgbase(&String::from_utf8_lossy(&t)))))
        .collect();
    let declares = |p: &String| newest.get(p).and_then(|b| declared.get(b).cloned().flatten()).as_deref() == Some(pkgbase);
    out.extend(variants.into_iter().filter(declares));
    Ok(out)
}

// -- build flags and provenance ------------------------------------------------

/// What built it beyond the tree: the makepkg.conf, where it is published.
fn flags_note(b: &Build, origin: Origin, bi: Option<&BuildInfo>) -> String {
    match origin {
        Origin::Arch => {
            // buildtoolver "1:1.5.3-1-any" is devtools 1.5.3. No link to
            // the file itself: its path and tag names changed over the
            // releases (20221012's is makepkg-x86_64.conf, untagged "v").
            let ver = bi
                .filter(|bi| bi.buildtool == "devtools")
                .map(|bi| bi.buildtoolver.rsplit(':').next().unwrap_or_default())
                .and_then(|v| v.split('-').next())
                .filter(|v| !v.is_empty());
            let release = ver.map_or(String::new(), |v| format!(" {v}"));
            format!("the makepkg.conf of Arch's devtools{release} (https://gitlab.archlinux.org/archlinux/devtools)")
        }
        Origin::Alhp => {
            // "ALHP x86-64-v4 <alhp@harting.dev>"
            let level = b.packager.split_whitespace().nth(1).unwrap_or("x86-64-v4");
            let conf = |lto: bool| format!("makepkg-{level}{}.conf", if lto { "" } else { "-non-lto" });
            match bi.and_then(|bi| bi.lto) {
                Some(lto) => format!("ALHP's published {}: https://alhp.dev/makepkg/{}", conf(lto), conf(lto)),
                None => format!(
                    "ALHP's published {} (or {} when LTO is off for the package): https://alhp.dev/makepkg/",
                    conf(true),
                    conf(false)
                ),
            }
        }
        Origin::CachyosRebuild => "CachyOS's build flags, which it does not publish; the tree is Arch's".to_string(),
        Origin::CachyosOwn => "CachyOS's makepkg.conf, which it does not publish (what the PKGBUILD sets is in the tree)".to_string(),
        Origin::Aur => "your makepkg.conf: paru builds AUR packages here".to_string(),
        Origin::Minka => "your makepkg.conf: minkarepo builds [minka] here".to_string(),
    }
}

fn provenance_lines(b: &Build, f: &Found) -> Vec<String> {
    let mut lines = vec![
        format!("repository: {}", b.repo),
        format!("package: {}", b.name),
        format!("pkgbase: {}", b.pkgbase),
        format!("version: {}", b.version),
    ];
    let installed = if b.repo == "aur" { " (your installed build)" } else { "" };
    if !b.packager.is_empty() {
        lines.push(format!("packager: {}{installed}", b.packager));
    }
    if let Some(d) = b.build_date {
        lines.push(format!("build date: {}{installed}", utc_date(d)));
    }
    lines.push(format!("source: {}", f.source));
    lines.push(format!("revision: {}", f.revision));
    lines.push(format!("matched: {}", f.matched));
    lines.push(format!("build flags: {}", f.flags));
    lines
}

// -- files, downloads and git ---------------------------------------------------

fn compare_root() -> PathBuf {
    check::cache_dir().join("compare")
}

/// A version as a path component: what pacman's versions are made of.
fn valid_version(v: &str) -> bool {
    !v.is_empty() && !v.starts_with('.') && v.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | ':' | '~' | '-'))
}

/// A package, base or repository name as a path component: a name, and
/// not "." or ".." (no package name starts with a dot).
fn path_name(name: &str) -> bool {
    valid_package_name(name) && !name.starts_with('.')
}

/// compare/<repo>/<pkgbase>/<version>/, empty.
fn fresh_tree_dir(b: &Build) -> Result<PathBuf, String> {
    if !path_name(&b.repo) || !path_name(&b.pkgbase) || !valid_version(&b.version) {
        return Err(format!("{}/{}: unusable pkgbase or version {:?} {:?}", b.repo, b.name, b.pkgbase, b.version));
    }
    let dir = compare_root().join(&b.repo).join(&b.pkgbase).join(&b.version);
    // remove_dir_all removes a symlink itself, never what it points to.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    Ok(dir)
}

/// A file of its own under compare/tmp/ for one download.
fn scratch_file(ext: &str) -> Result<PathBuf, String> {
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = compare_root().join("tmp");
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    Ok(dir.join(format!("{}-{}.{ext}", std::process::id(), N.fetch_add(1, Ordering::Relaxed))))
}

/// A file's content, only when it is a regular file: a symlink in a
/// downloaded tree is never followed.
fn read_regular(path: &Path) -> Result<Vec<u8>, String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))
}

fn read_pkgbuild(dest: &Path) -> Result<Vec<u8>, String> {
    read_regular(&dest.join("PKGBUILD")).map_err(|e| format!("the tree has no readable PKGBUILD: {e}"))
}

/// Puts `data` at `dest/name` without following anything the tree has
/// there: the old entry is removed and the file created exclusively.
fn write_new(dest: &Path, name: &str, data: &[u8]) -> Result<(), String> {
    replace_file(&dest.join(name), data)
}

fn replace_file(path: &Path, data: &[u8]) -> Result<(), String> {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.is_dir() {
            return Err(format!("{} is a directory", path.display()));
        }
        std::fs::remove_file(path).map_err(|e| format!("replacing {}: {e}", path.display()))?;
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .and_then(|mut f| f.write_all(data))
        .map_err(|e| format!("writing {}: {e}", path.display()))
}

/// Turns each symlink under `dir` into a file saying where it pointed, the
/// way git stores one, so meld still shows a difference. meld follows
/// symlinks into directories (and writes through them), and one in a
/// downloaded tree can point anywhere: /, or ~/.ssh.
fn defuse_symlinks(dir: &Path) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("reading {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("reading {}: {e}", dir.display()))?;
        let path = entry.path();
        // The entry's own type: a symlink is not followed.
        let kind = entry.file_type().map_err(|e| format!("{}: {e}", path.display()))?;
        if kind.is_symlink() {
            let target = std::fs::read_link(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            replace_file(&path, format!("symlink to {}\n", target.display()).as_bytes())?;
        } else if kind.is_dir() {
            defuse_symlinks(&path)?;
        }
    }
    Ok(())
}

/// Which of a tree's local files are not what its PKGBUILD's checksums
/// say, and so not what its build checked: CachyOS committed gcc's
/// PKGBUILD as built without the c89, c99 and patch it was built with.
/// None when there is nothing to check (no local files, or checksums
/// that cannot be read as text).
fn unverified_files(dest: &Path, pkgbuild: &[u8]) -> Result<Option<Vec<String>>, String> {
    let Some(files) = local_checksums(&String::from_utf8_lossy(pkgbuild)).filter(|f| !f.is_empty()) else {
        return Ok(None);
    };
    let mut differ = Vec::new();
    for (name, tool, sum) in files {
        let same = match read_regular(&dest.join(&name)) {
            Ok(data) => checksum(tool, &data)? == sum,
            Err(_) => false,
        };
        if !same && !differ.contains(&name) {
            differ.push(name);
        }
    }
    Ok(Some(differ))
}

/// Unpacks an archive into `dest` with bsdtar's defaults, which refuse
/// absolute paths, '..' and writing through symlinks (no -P).
fn extract(archive: &Path, dest: &Path, strip_top: bool) -> Result<(), String> {
    let mut cmd = Command::new("bsdtar");
    cmd.arg("-xf").arg(archive).arg("-C").arg(dest);
    if strip_top {
        cmd.args(["--strip-components", "1"]);
    }
    run_text(&mut cmd).map(drop)
}

fn sha256(data: &[u8]) -> Result<String, String> {
    checksum("sha256sum", data)
}

/// `data`'s sum by `tool` (sha256sum, b2sum), in lowercase hex.
fn checksum(tool: &str, data: &[u8]) -> Result<String, String> {
    let mut child = Command::new(tool)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("running {tool}: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(data).map_err(|e| format!("writing to {tool}: {e}"))?;
    }
    let out = child.wait_with_output().map_err(|e| format!("running {tool}: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace().next().map(str::to_ascii_lowercase).ok_or_else(|| format!("{tool} printed nothing"))
}

/// Which of `texts` (each a PKGBUILD a builder may have hashed, and how it
/// came to be) has the sha256 `want`, as it is or without its final
/// newline (CachyOS's builds before about June 2025 hashed it without).
fn hashed_variant(texts: Vec<(Vec<u8>, String)>, want: &str) -> Result<Option<(Vec<u8>, String)>, String> {
    for (text, how) in texts {
        if sha256(&text)? == want {
            return Ok(Some((text, how)));
        }
        if let Some(cut) = text.strip_suffix(b"\n") {
            if sha256(cut)? == want {
                return Ok(Some((cut.to_vec(), format!("{how}, without its final newline"))));
            }
        }
    }
    Ok(None)
}

/// curl into `out`, politely: a generic user agent and a 5 s connect
/// timeout (`extra` adds the rest). The HTTP status (0 for file://) when
/// the transfer happened, whatever it was.
fn curl(url: &str, out: &Path, extra: &[&str]) -> Result<u16, String> {
    let output = Command::new("curl")
        .args(["-sS", "--connect-timeout", "5", "--max-filesize", "67108864", "-A", check::USER_AGENT, "-w", "%{http_code}", "-o"])
        .arg(out)
        .args(extra)
        .arg("--")
        .arg(url)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("running curl: {e}"))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(err.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("curl failed").trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().parse().unwrap_or(0))
}

/// A file name for a URL path: everything but unreserved characters
/// percent-encoded (an epoch's ':', and '+', which some mirrors read as a
/// space).
fn url_encode(name: &str) -> String {
    name.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            other => format!("%{other:02X}"),
        })
        .collect()
}

fn read_buildinfo(file: &Path) -> Option<BuildInfo> {
    let out = Command::new("bsdtar").arg("-xOqf").arg(file).arg(".BUILDINFO").stdin(Stdio::null()).output().ok()?;
    parse_buildinfo(&String::from_utf8_lossy(&out.stdout)).map(|bi| BuildInfo { from: file.display().to_string(), ..bi })
}

/// A repository build's .BUILDINFO: from the pacman cache, else the head
/// of the package on the repository's servers. It is the first member, so
/// 256 KiB is plenty, and bsdtar reads zstd itself and stops at it (-q),
/// before the cut-off end. None when nothing has it.
fn buildinfo(b: &Build) -> Option<BuildInfo> {
    if b.filename.is_empty() || b.filename.contains('/') {
        return None;
    }
    let cached = Path::new("/var/cache/pacman/pkg").join(&b.filename);
    if let Some(bi) = read_buildinfo(&cached).filter(|bi| bi.is_of(b)) {
        return Some(bi);
    }
    let head = scratch_file("head").ok()?;
    let mut found = None;
    for server in servers(&b.repo).into_iter().take(MIRRORS_TRIED) {
        let url = format!("{}/{}", server.trim_end_matches('/'), url_encode(&b.filename));
        // Any failure (404, 418 to a user agent it dislikes) means: try the next.
        if curl(&url, &head, &["-L", "--fail", "-r", "0-262143", "--max-time", "20"]).is_ok() {
            found = read_buildinfo(&head).filter(|bi| bi.is_of(b)).map(|bi| BuildInfo { from: url.clone(), ..bi });
            if found.is_some() {
                break;
            }
        }
    }
    let _ = std::fs::remove_file(&head);
    found
}

/// A repository's servers in pacman.conf order (pacman-conf expands
/// $repo and $arch itself).
fn servers(repo: &str) -> Vec<String> {
    let Ok(out) = Command::new("pacman-conf").args(["-r", repo]).stdin(Stdio::null()).output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix("Server = "))
        .map(|s| s.trim().replace("$repo", repo).replace("$arch", "x86_64"))
        .collect()
}

/// git without the user's configuration (no url rewrites to ssh, no
/// credential helpers, no log settings), never prompting, with a generic
/// user agent, and giving up on a transfer that stalls. An empty
/// GIT_ASKPASS is what keeps an askpass dialog away (VS Code's terminal
/// sets one, a desktop session SSH_ASKPASS) when GitHub answers 401 for a
/// repository gone private; GIT_TERMINAL_PROMPT only covers the terminal.
/// No inherited GIT_DIR and the like can point `-C` elsewhere.
fn git(repo: &Path) -> Command {
    let mut cmd = Command::new("git");
    for inherited in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_OBJECT_DIRECTORY", "GIT_COMMON_DIR"] {
        cmd.env_remove(inherited);
    }
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(["-c", "credential.helper=", "-c"])
        .arg(format!("http.userAgent={}", check::USER_AGENT))
        .args(["-c", "http.lowSpeedLimit=1000", "-c", "http.lowSpeedTime=20", "-C"])
        .arg(repo)
        .stdin(Stdio::null());
    cmd
}

fn run_text(cmd: &mut Command) -> Result<String, String> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let out = cmd.output().map_err(|e| format!("running {program}: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "{program} failed: {}",
            err.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("no message").trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A bare clone of `url` at `dir` (blobless: commits and trees only, blobs
/// when asked for), fetched again when older than FETCH_AFTER. A fetch
/// that fails leaves the older copy in use, with a note on stderr.
fn mirror(url: &str, dir: &Path, blobless: bool) -> Result<(), String> {
    if dir.join("HEAD").is_file() {
        let fetched = ["FETCH_HEAD", "HEAD"]
            .iter()
            .filter_map(|f| std::fs::metadata(dir.join(f)).and_then(|m| m.modified()).ok())
            .max()
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if fetched.elapsed().is_ok_and(|age| age < FETCH_AFTER) {
            return Ok(());
        }
        if let Err(e) = run_text(git(dir).args(["fetch", "--quiet", "--prune", "origin", "+refs/heads/*:refs/heads/*"])) {
            eprintln!("minka-syu: fetching {url}: {e}; using the copy fetched before");
        }
        return Ok(());
    }
    // Cloned beside it and moved in, so an interrupted clone is never used.
    let part = PathBuf::from(format!("{}.part", dir.display()));
    let _ = std::fs::remove_dir_all(&part);
    let parent = dir.parent().ok_or("a clone needs a parent directory")?;
    std::fs::create_dir_all(parent).map_err(|e| format!("creating {}: {e}", parent.display()))?;
    let mut cmd = git(parent);
    cmd.args(["clone", "--bare", "--quiet"]);
    if blobless {
        cmd.arg("--filter=blob:none");
    }
    run_text(cmd.arg("--").arg(url).arg(&part))?;
    std::fs::rename(&part, dir).map_err(|e| format!("moving the clone of {url} into place: {e}"))
}

/// The commits of `rev` that changed any of `paths`, newest first (log
/// --raw: trees only, which a blobless clone has). `follow` tracks one
/// path through renames.
fn path_history(repo: &Path, rev: &str, paths: &[String], follow: bool) -> Result<Vec<Revision>, String> {
    let mut cmd = git(repo);
    cmd.args(["log", "--format=C %H %ct", "--raw", "--no-abbrev", if follow { "--follow" } else { "--no-renames" }, rev, "--"]);
    Ok(parse_raw_log(&run_text(cmd.args(paths))?))
}

/// git cat-file --batch or --batch-check over `input`, written from a
/// thread so a large answer can never stall the two against each other.
fn cat_file(repo: &Path, mode: &str, input: String) -> Result<Vec<u8>, String> {
    let mut child = git(repo)
        // A blob a blobless clone lacks reads as missing instead of costing
        // a round trip of its own: fetch_blobs gets them first, in one.
        .env("GIT_NO_LAZY_FETCH", "1")
        .args(["cat-file", mode])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("running git cat-file: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("git cat-file has no stdin")?;
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let out = child.wait_with_output().map_err(|e| format!("running git cat-file: {e}"))?;
    let _ = writer.join();
    if !out.status.success() {
        return Err(format!("git cat-file failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(out.stdout)
}

/// A git object's id and content.
type Object = (String, Vec<u8>);

/// Objects by name (an id, or "<rev>:<path>"); None for one that does not
/// exist.
fn cat_files(repo: &Path, names: &[String]) -> Result<Vec<Option<Object>>, String> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    parse_batch(&cat_file(repo, "--batch", names.join("\n") + "\n")?, names.len())
}

/// `git cat-file --batch` output for `n` names.
pub fn parse_batch(mut data: &[u8], n: usize) -> Result<Vec<Option<Object>>, String> {
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let nl = data.iter().position(|&c| c == b'\n').ok_or("git cat-file stopped short")?;
        let header = String::from_utf8_lossy(&data[..nl]).into_owned();
        data = &data[nl + 1..];
        let f: Vec<&str> = header.split(' ').collect();
        let size = match f.as_slice() {
            [_, "missing"] | [_, "ambiguous"] => {
                out.push(None);
                continue;
            }
            [_, _, size] => size.parse::<usize>().map_err(|_| format!("git cat-file: {header}"))?,
            _ => return Err(format!("git cat-file: {header}")),
        };
        if data.len() < size + 1 {
            return Err("git cat-file stopped short".to_string());
        }
        out.push(Some((f[0].to_string(), data[..size].to_vec())));
        data = &data[size + 1..];
    }
    Ok(out)
}

/// Gets the blobs a blobless clone lacks among `oids` in one fetch.
fn fetch_blobs(repo: &Path, oids: &[String]) -> Result<(), String> {
    if oids.is_empty() {
        return Ok(());
    }
    let check = String::from_utf8_lossy(&cat_file(repo, "--batch-check", oids.join("\n") + "\n")?).into_owned();
    let missing: Vec<&str> = check.lines().filter_map(|l| l.strip_suffix(" missing")).collect();
    if missing.is_empty() {
        return Ok(());
    }
    run_text(
        git(repo)
            .args(["fetch", "--quiet", "--no-tags", "--no-write-fetch-head", "--recurse-submodules=no", "--filter=blob:none", "origin"])
            .args(&missing),
    )
    .map(drop)
}

/// Writes `<commit>:<dir>` (the whole commit when `dir` is empty) into
/// `dest` through git archive, a blobless clone's blobs fetched first in
/// one go.
fn export(repo: &Path, commit: &str, dir: &str, dest: &Path, blobless: bool) -> Result<(), String> {
    let tree = if dir.is_empty() { commit.to_string() } else { format!("{commit}:{dir}") };
    if blobless {
        let listing = run_text(git(repo).args(["ls-tree", "-r", &tree]))?;
        let blobs: Vec<String> = listing
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split_whitespace().collect();
                (f.get(1) == Some(&"blob")).then(|| f[2].to_string())
            })
            .collect();
        fetch_blobs(repo, &blobs)?;
    }
    let tar = scratch_file("tar")?;
    let made = run_text(git(repo).args(["archive", "--format=tar", "-o"]).arg(&tar).arg(&tree)).and_then(|_| extract(&tar, dest, false));
    let _ = std::fs::remove_file(&tar);
    made
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_from_versions() {
        assert_eq!(arch_tag("5.44.0-1"), "5.44.0-1");
        assert_eq!(arch_tag("1:1.6.11-2"), "1-1.6.11-2");
        assert_eq!(arch_tag("16.2.1+r23+gd564253eb6c8-1"), "16.2.1+r23+gd564253eb6c8-1");
        assert_eq!(arch_tag("1.0~rc1-3"), "1.0.rc1-3");
    }

    #[test]
    fn gitlab_projects_from_pkgbases() {
        for (base, project) in [
            ("libsigc++", "libsigcplusplus"),
            ("libsigc++-3.0", "libsigcplusplus-3.0"),
            ("dvd+rw-tools", "dvd-rw-tools"),
            ("gtk2+extra", "gtk2-extra"),
            ("c++utilities", "cplusplusutilities"),
            ("libxml++2.6", "libxmlplusplus2.6"),
            ("memtest86+", "memtest86plus"),
            ("tree", "unix-tree"),
            ("trees", "trees"),
            ("wpa_supplicant", "wpa_supplicant"),
            ("a__b--c", "a-b-c"),
            ("a+b2+c", "a-b2-c"),
            ("name@host", "name-host"),
        ] {
            assert_eq!(gitlab_project(base), project, "{base}");
        }
    }

    #[test]
    fn rebuild_suffixes() {
        assert_eq!(split_rebuild("5.44.0-1.2"), Some(("5.44.0-1".into(), "1.2".into())));
        assert_eq!(split_rebuild("1:1.2.12-6.1"), Some(("1:1.2.12-6".into(), "6.1".into())));
        assert_eq!(split_rebuild("17.0+r134+g77d25a1e6-2.1"), Some(("17.0+r134+g77d25a1e6-2".into(), "2.1".into())));
        assert_eq!(split_rebuild("620.73.04-1"), None);
        assert_eq!(split_rebuild("1.0-1.rc"), None);
    }

    fn build(repo: &str, packager: &str, version: &str, filename: &str) -> Build {
        Build {
            repo: repo.into(),
            name: "pkg".into(),
            pkgbase: "pkg".into(),
            version: version.into(),
            packager: packager.into(),
            build_date: Some(1),
            filename: filename.into(),
        }
    }

    #[test]
    fn classifies_by_repository_and_packager() {
        const ALHP: &str = "ALHP x86-64-v4 <alhp@harting.dev>";
        const CACHY: &str = "CachyOS <admin@cachyos.org>";
        let alhp = build("extra-x86-64-v4", ALHP, "2.62.1-1.1", "at-spi2-core-2.62.1-1.1-x86_64.pkg.tar.zst");
        assert_eq!(classify(&alhp), Ok(Origin::Alhp));
        // The same version, file arch and suffix from CachyOS: a rebuild
        // of its own, which %ARCH% and the suffix could never tell apart.
        let cachy = build("cachyos-extra-v4", CACHY, "2.62.1-1.1", "at-spi2-core-2.62.1-1.1-x86_64.pkg.tar.zst");
        assert_eq!(classify(&cachy), Ok(Origin::CachyosRebuild));
        assert_eq!(classify(&build("cachyos-core-v4", CACHY, "5.44.0-1.1", "")), Ok(Origin::CachyosRebuild));
        // cachyos-v4 has x86_64_v3 files of CachyOS's own.
        let own = build("cachyos-v4", CACHY, "3.54.1-1", "sqlite-3.54.1-1-x86_64_v3.pkg.tar.zst");
        assert_eq!(classify(&own), Ok(Origin::CachyosOwn));
        assert_eq!(classify(&build("cachyos", CACHY, "1-1", "")), Ok(Origin::CachyosOwn));
        assert_eq!(classify(&build("extra", "Peter Jung <ptr1337@archlinux.org>", "620.73.04-1", "")), Ok(Origin::Arch));
        assert_eq!(classify(&build("core", "Someone <x@archlinux.org>", "1-1", "")), Ok(Origin::Arch));
        assert_eq!(classify(&build("aur", "", "1-1", "")), Ok(Origin::Aur));
        assert_eq!(classify(&build("minka", "Unknown Packager", "1-1", "")), Ok(Origin::Minka));
        let other = classify(&build("kotontrion", "kotontrion <kotontrion@tutanota.de>", "1-1", ""));
        assert!(other.is_err_and(|e| e.starts_with("no known source for kotontrion")));
    }

    #[test]
    fn pkgrel_rewrites() {
        let text = "pkgname=x\npkgver=1.0\npkgrel=1\n\nbuild() {\n  pkgrel=7\n}\n";
        assert_eq!(rewrite_pkgrel(text, "1.1").unwrap(), "pkgname=x\npkgver=1.0\npkgrel=1.1\n\nbuild() {\n  pkgrel=7\n}\n");
        assert_eq!(rewrite_pkgrel("pkgrel='2'", "2.3").unwrap(), "pkgrel=2.3");
        assert_eq!(rewrite_pkgrel("pkgrel = \"4\"\n", "4.1").unwrap(), "pkgrel=4.1\n");
        assert_eq!(rewrite_pkgrel("pkgrel\t=5\n", "5.2").unwrap(), "pkgrel=5.2\n");
        assert_eq!(rewrite_pkgrel("pkgrel_extra=1\n", "1.1"), None);
        assert_eq!(rewrite_pkgrel("pkgver=1\n", "1.1"), None);
    }

    #[test]
    fn level_switches() {
        let go = "_optimize_v4=${_optimize_v4-}\n_optimize_v3=${_optimize_v3-}\n";
        assert_eq!(toggle_level(go, "v4").unwrap(), "_optimize_v4=${_optimize_v4-y}\n_optimize_v3=${_optimize_v3-}\n");
        assert_eq!(toggle_level("pkgver=1\n", "v4"), None);
    }

    #[test]
    fn versions_from_text() {
        let srcinfo = "pkgbase = lib32-portaudio\n\tpkgver = 19.7.0\n\tpkgrel = 5\n\tepoch = 1\n\npkgname = lib32-portaudio\n\tpkgver = 9\n";
        assert_eq!(srcinfo_version(srcinfo).as_deref(), Some("1:19.7.0-5"));
        assert_eq!(srcinfo_version("pkgbase = x\n\tpkgver = 2.3\n\tpkgrel = 1\n").as_deref(), Some("2.3-1"));
        assert_eq!(srcinfo_version("pkgbase = x\n\tpkgver = 2.3\n"), None);
        let pkgbuild = "pkgname=x\npkgver='1.2.3' # upstream\npkgrel=\"2\"\nepoch=1\nbuild() {\npkgver=9\n}\n";
        assert_eq!(pkgbuild_version(pkgbuild).as_deref(), Some("1:1.2.3-2"));
        assert_eq!(pkgbuild_version("pkgver=0.99.beta20\npkgrel=2\nepoch=0\n").as_deref(), Some("0.99.beta20-2"));
        assert_eq!(pkgbuild_version("pkgver=${_major}.${_minor}\npkgrel=1\n"), None);
        assert_eq!(declared_pkgbase("pkgbase=openblas\npkgname=(openblas lapack)\n").as_deref(), Some("openblas"));
        assert_eq!(declared_pkgbase("pkgname=('zstd' 'lib32-zstd')\n").as_deref(), Some("zstd"));
        assert_eq!(declared_pkgbase("pkgname=zstd-pgo\n").as_deref(), Some("zstd-pgo"));
    }

    fn rev(commit: &str, time: i64, hash_match: bool, version_match: bool) -> Revision {
        Revision { commit: commit.into(), time, hash_match, version_match, ..Default::default() }
    }

    #[test]
    fn commit_choice() {
        // Newest first, as git log lists them.
        let revs = [rev("d", 40, false, true), rev("c", 30, true, true), rev("b", 20, true, true), rev("a", 10, false, false)];
        // A hash match wins over a newer version match; of two, the newest
        // by the build (lib32-pam: a later commit changed only update.sh).
        assert_eq!(pick_revision(&revs, Some(35), 0), Some(Pick::Hash(1)));
        assert_eq!(pick_revision(&revs, Some(25), 0), Some(Pick::Hash(2)));
        // Built before any of them was committed (MinkaPKG): the oldest after.
        assert_eq!(pick_revision(&revs, Some(15), 0), Some(Pick::Hash(2)));
        // No date: the newest.
        assert_eq!(pick_revision(&revs, None, 0), Some(Pick::Hash(1)));
        let unhashed = [rev("d", 40, false, true), rev("c", 30, false, true), rev("a", 10, false, false)];
        assert_eq!(pick_revision(&unhashed, Some(35), 0), Some(Pick::Version(1)));
        assert_eq!(pick_revision(&unhashed, None, 0), Some(Pick::Version(0)));
        // A commit naming the build can trail it (lib32-zlib-ng, 28 h).
        assert_eq!(pick_revision(&unhashed, Some(35), 10), Some(Pick::Version(0)));
        let unnamed = [rev("c", 30, false, false), rev("a", 10, false, false)];
        assert_eq!(pick_revision(&unnamed, Some(20), 0), Some(Pick::Date(1)));
        // ...but one that does not is no likelier the build than the next
        // change (linux-cachyos-lts): the date alone allows no slack.
        assert_eq!(pick_revision(&unnamed, Some(25), 10), Some(Pick::Date(1)));
        assert_eq!(pick_revision(&unnamed, Some(5), 10), None);
        assert_eq!(pick_revision(&unnamed, None, 0), None);
        assert_eq!(pick_revision(&[], Some(5), 0), None);
    }

    #[test]
    fn alhp_bound_rewrites() {
        // at-spi2-core 2.62.1-1.1: ALHP built atk<2.38.0-3.
        let text = "pkgrel=1.1\nconflicts=('at-spi2-atk<=2.38.0-2' \"atk<=2.38.0-2\")\ndepends=(glib2 'x<=1-1')\n";
        assert_eq!(
            raise_bounds(text).unwrap(),
            "pkgrel=1.1\nconflicts=('at-spi2-atk<2.38.0-3' \"atk<2.38.0-3\")\ndepends=(glib2 'x<=1-1')\n"
        );
        // In package functions, spaced, multi-line, with an epoch; and
        // what it leaves: sonames, dotted and non-numeric pkgrels, `>=`.
        let split = "package_a() {\n  replaces+=(\n    'b<=1:580.119.02-3' # old\n    'libc.so<=6-1'\n    'c<=1-1.1' 'd<=1-rc' 'e>=1-1')\n}\n";
        assert_eq!(
            raise_bounds(split).unwrap(),
            "package_a() {\n  replaces+=(\n    'b<1:580.119.02-4' # old\n    'libc.so<=6-1'\n    'c<=1-1.1' 'd<=1-rc' 'e>=1-1')\n}\n"
        );
        assert_eq!(raise_bounds("conflicts_x86_64=(f<=2-9)").unwrap(), "conflicts_x86_64=(f<2-10)");
        // A bound ALHP could not find as written changes nothing at all.
        assert_eq!(raise_bounds("conflicts=('a<=1-1' \"b<=${pkgver}-1\")\n"), None);
        assert_eq!(raise_bounds("conflicts=('a>=1-1')\nmyconflicts=('a<=1-1')\n"), None);
    }

    #[test]
    fn checksum_arrays() {
        // CachyOS's gcc: a URL with a variable, then local files.
        let gcc = "_commit=abc\nsource=(git+https://sourceware.org/git/gcc.git#commit=$_commit\n        c89 c99 # standards\n        \
                   gcc-ada-repro.patch\n)\nsha256sums=('AAA'\n            'bbb'\n            'ccc'\n            SKIP)\n";
        assert_eq!(
            local_checksums(gcc).unwrap(),
            [("c89".to_string(), "sha256sum", "bbb".to_string()), ("c99".into(), "sha256sum", "ccc".into())]
        );
        let renamed = "source=(\"x.tar.gz::https://h/x\" \"$pkgname.patch\" 'dir/fix.patch')\nb2sums=('1' '2' '3')\n";
        assert_eq!(local_checksums(renamed).unwrap(), [("fix.patch".to_string(), "b2sum", "3".to_string())]);
        // Arrays that cannot be paired as text.
        assert_eq!(local_checksums("source=(a b)\nsha256sums=('1')\n"), None);
        assert_eq!(local_checksums("source=(a)\nsource+=(b)\nsha256sums=(1 2)\n"), None);
        assert_eq!(local_checksums("source=(a)\n"), None);
        assert_eq!(top_array("  source=(nested)\nsource=(top)\n", "source").unwrap(), ["top"]);
    }

    #[test]
    fn symlinks_become_notes() {
        let dir = std::env::temp_dir().join(format!("minka-syu-compare-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("PKGBUILD"), "pkgname=x\n").unwrap();
        std::os::unix::fs::symlink("/", dir.join("root")).unwrap();
        std::os::unix::fs::symlink("../PKGBUILD", dir.join("sub/up")).unwrap();
        defuse_symlinks(&dir).unwrap();
        let note = |p: &str| (std::fs::symlink_metadata(dir.join(p)).unwrap().is_file(), std::fs::read_to_string(dir.join(p)).unwrap());
        assert_eq!(note("root"), (true, "symlink to /\n".to_string()));
        assert_eq!(note("sub/up"), (true, "symlink to ../PKGBUILD\n".to_string()));
        assert_eq!(note("PKGBUILD"), (true, "pkgname=x\n".to_string()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn raw_log_parsing() {
        let log = "C b27b1a8 1793500169\n\n:100644 100644 982f 3a73 M\tfirestorm-novlc/PKGBUILD\n\
                   C fffc2bb 1793418186\n\n:100644 100644 5988 982f R083\tfirestorm-nolvc/PKGBUILD\tfirestorm-novlc/PKGBUILD\n\
                   C 0ld0000 1700000000\n\n:100644 000000 1111 0000 D\tgone/PKGBUILD\n";
        let revs = parse_raw_log(log);
        assert_eq!(revs.len(), 2);
        assert_eq!((revs[0].commit.as_str(), revs[0].time, revs[0].blob.as_str()), ("b27b1a8", 1793500169, "3a73"));
        assert_eq!(revs[1].path, "firestorm-novlc/PKGBUILD");
    }

    #[test]
    fn buildinfo_parsing() {
        let text = "format = 2\npkgname = nvidia-settings\npkgver = 620.73.04-1.1\n\
                    pkgbuild_sha256sum = 11e8dc84b357d4637ffff633f08389692a9ccf833f2a2fb672dadc17c133ef0d\n\
                    builddate = 1795134202\nstartdir = /startdir\nbuildtool = devtools\nbuildtoolver = 1:1.5.3-1-any\n\
                    options = strip\noptions = lto\n";
        let bi = parse_buildinfo(text).unwrap();
        assert_eq!((bi.version.as_str(), bi.build_date, bi.lto), ("620.73.04-1.1", Some(1795134202), Some(true)));
        let mut b = build("extra-x86-64-v4", "", "620.73.04-1.1", "");
        b.build_date = Some(1795134202);
        assert!(bi.is_of(&b));
        b.build_date = Some(1795134203);
        assert!(!bi.is_of(&b), "another repository's file of that name");
        assert_eq!(parse_buildinfo("format = 2\npkgname = cut\n"), None);
        // Named, not linked: devtools' file moved and its tags gained a "v".
        let devtools = flags_note(&build("extra", "", "1-1", ""), Origin::Arch, Some(&bi));
        assert_eq!(devtools, "the makepkg.conf of Arch's devtools 1.5.3 (https://gitlab.archlinux.org/archlinux/devtools)");
        let makepkg = BuildInfo { buildtool: "makepkg".into(), buildtoolver: "7.1.0".into(), ..bi.clone() };
        let plain = flags_note(&build("extra", "", "1-1", ""), Origin::Arch, Some(&makepkg));
        assert_eq!(plain, "the makepkg.conf of Arch's devtools (https://gitlab.archlinux.org/archlinux/devtools)");
        let alhp = flags_note(&build("core-x86-64-v4", "ALHP x86-64-v4 <alhp@harting.dev>", "1-1.1", ""), Origin::Alhp, Some(&bi));
        assert!(alhp.ends_with("https://alhp.dev/makepkg/makepkg-x86-64-v4.conf"), "{alhp}");
    }

    #[test]
    fn describe_and_aur_answers_parse() {
        let line = "libxnvctrl|extra|620.73.04-1|nvidia-settings|Peter Jung <ptr1337@archlinux.org>|Thu Nov 19 12:45:25 2026|libxnvctrl-620.73.04-1-x86_64.pkg.tar.zst\n";
        let b = parse_describe(line).unwrap();
        assert_eq!((b.pkgbase.as_str(), b.repo.as_str(), b.filename.as_str()), ("nvidia-settings", "extra", "libxnvctrl-620.73.04-1-x86_64.pkg.tar.zst"));
        assert_eq!(b.build_date, check::parse_c_date("Thu Nov 19 12:45:25 2026"));
        assert_eq!(parse_describe("x|core|1-1||p|Thu Nov 19 12:45:25 2026|f").unwrap().pkgbase, "x");
        assert_eq!(parse_describe("too|few"), None);
        let rpc = r#"{"resultcount":1,"results":[{"Name":"lib32-gst-plugins-good","PackageBase":"lib32-gstreamer","Version":"1.28.9-1"}],"type":"multiinfo","version":5}"#;
        assert_eq!(parse_aur_base(rpc, "lib32-gst-plugins-good"), Ok(("lib32-gstreamer".into(), "1.28.9-1".into())));
        assert!(parse_aur_base(rpc, "other").is_err());
        assert!(parse_aur_base(r#"{"type":"error","error":"Rate limited"}"#, "x").is_err_and(|e| e.contains("Rate limited")));
    }

    #[test]
    fn small_helpers() {
        assert_eq!(utc_date(1795134202), "20/11/2026 00:23 UTC");
        assert_eq!(utc_date(951782400), "29/2/2000 00:00 UTC");
        assert_eq!(url_encode("bcachefs-dkms-3:1.39.7-2-x86_64.pkg.tar.zst"), "bcachefs-dkms-3%3A1.39.7-2-x86_64.pkg.tar.zst");
        assert_eq!(url_encode("libsigc++-2.12.2-1.1-x86_64.pkg.tar.zst"), "libsigc%2B%2B-2.12.2-1.1-x86_64.pkg.tar.zst");
        let dirs = ["linux-cachyos", "linux-cachyos-rt-bore", "linux-cachyos-lts", "README.md"];
        assert_eq!(longest_prefix_dir(dirs.into_iter(), "linux-cachyos-rt-bore-lto"), Some("linux-cachyos-rt-bore"));
        assert_eq!(longest_prefix_dir(dirs.into_iter(), "linux-cachyos"), Some("linux-cachyos"));
        assert_eq!(longest_prefix_dir(dirs.into_iter(), "linux-cachyosx"), None);
        assert!(valid_version("1:1.2.12-6.1") && valid_version("1.0~rc1-1"));
        assert!(!valid_version("..") && !valid_version("1/2-1") && !valid_version(""));
        let batch = b"aaa blob 3\nabc\nmain:.SRCINFO missing\nbbb blob 0\n\n";
        let parsed = parse_batch(batch, 3).unwrap();
        assert_eq!(parsed[0], Some(("aaa".to_string(), b"abc".to_vec())));
        assert_eq!(parsed[1], None);
        assert_eq!(parsed[2], Some(("bbb".to_string(), Vec::new())));
        assert!(parse_batch(b"aaa blob 9\nabc\n", 1).is_err());
    }
}
