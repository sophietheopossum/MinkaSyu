//! What the window shows, and the rules for ticking it: a port of the
//! QML window's logic (ui/shell.qml), kept free of the toolkit so it can be
//! unit-tested. The core owns checking and the state file; everything here
//! is a view over its answers plus selections that live in memory only.

use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Replaced {
    pub name: String,
    pub version: String,
    pub installed_repo: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Update {
    pub name: String,
    pub old_version: String,
    pub new_version: String,
    pub installed_repo: Option<String>,
    pub candidate_repo: String,
    /// same | changed | unknown_source
    pub transition: String,
    pub explicit_target: bool,
    pub first_repo: Option<String>,
    pub first_version: Option<String>,
    pub replaces: Vec<Replaced>,
    pub held: bool,
    /// "ignore" | "usage"
    pub held_by: Option<String>,
    pub superseded_by: Vec<String>,
    pub new_install: bool,
    pub reinstalls: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct State {
    pub checked_at: i64,
    pub updates: Vec<Update>,
    pub repo_change_count: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct SearchResult {
    pub name: String,
    pub repo: String,
    pub version: String,
    pub description: String,
    pub installed_version: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct SearchResponse {
    pub results: Vec<SearchResult>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct InstalledEntry {
    pub name: String,
    pub version: String,
    pub description: String,
    pub repo: String,
    pub explicit: bool,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct RepoOption {
    pub repo: String,
    pub name: String,
    pub version: String,
    pub architecture: String,
    pub description: String,
    pub target: String,
    pub is_default: bool,
    pub is_installed: bool,
    pub is_newest: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Appstream {
    pub id: String,
    pub name: String,
    pub summary: String,
    pub description: String,
    pub homepage: String,
    pub developer: String,
    pub license: String,
    pub categories: Vec<String>,
    pub icon_url: String,
    pub screenshots: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct SourceRow {
    pub package: String,
    pub kind: String,
    pub auto: bool,
    pub tracked: bool,
    pub pkgbuild: String,
    pub published: String,
    pub upstream: String,
    pub commit: String,
    pub status: String,
    pub note: String,
    pub error: String,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Sources {
    pub available: bool,
    pub minkarepo: String,
    pub checked_at: i64,
    pub rows: Vec<SourceRow>,
}

/// A manual repository choice for one pending update.
#[derive(Debug, Clone, PartialEq)]
pub struct Override {
    pub repo: String,
    pub version: String,
    pub target: String,
}

/// Which list opened the detail view; it decides what its button does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Context {
    Updates,
    Browse,
    Installed,
}

// ---- updates --------------------------------------------------------------

/// A package a replacement removes: its row is for information only.
pub fn superseded(u: &Update) -> bool {
    !u.superseded_by.is_empty()
}

/// Rows this run applies unless unticked: not held, not replaced.
pub fn row_pending(u: &Update) -> bool {
    !u.held && !superseded(u)
}

/// The ticked replacement that removes this row's package while the row
/// stays in the run (a replacer in a chain). Unticking the row would put
/// it into --ignore, which cancels the outer replacement too.
pub fn locked_by(updates: &[Update], ignored: &BTreeSet<String>, u: &Update) -> Option<String> {
    updates
        .iter()
        .find(|r| {
            r.name != u.name
                && row_pending(r)
                && !ignored.contains(&r.name)
                && (r.replaces.iter().any(|p| p.name == u.name) || r.reinstalls.contains(&u.name))
        })
        .map(|r| r.name.clone())
}

/// A saved repo choice that no longer applies: the update is held, or it
/// became a replacement only its own repo performs.
pub fn override_invalid(u: &Update, ov: &Override) -> bool {
    u.held || (!u.explicit_target && !u.replaces.is_empty() && ov.repo != u.candidate_repo)
}

/// The update as it will run with its repo override applied.
pub fn effective_update(u: &Update, overrides: &BTreeMap<String, Override>) -> Update {
    let Some(ov) = overrides.get(&u.name) else {
        return u.clone();
    };
    if override_invalid(u, ov) {
        return u.clone();
    }
    let mut copy = u.clone();
    let default_repo = if u.explicit_target {
        u.first_repo
            .clone()
            .unwrap_or_else(|| u.candidate_repo.clone())
    } else {
        u.candidate_repo.clone()
    };
    copy.new_version = ov.version.clone();
    copy.candidate_repo = ov.repo.clone();
    // What a later build would replace does not carry over to another one.
    if ov.repo != u.candidate_repo {
        copy.replaces.clear();
    }
    copy.explicit_target = ov.repo != default_repo;
    if copy.explicit_target && copy.first_repo.is_none() {
        copy.first_repo = Some(default_repo.clone());
        copy.first_version = Some(u.new_version.clone());
    }
    if !copy.explicit_target {
        copy.first_repo = None;
        copy.first_version = None;
    }
    copy.transition = match &u.installed_repo {
        None => "unknown_source".into(),
        Some(r) if *r == ov.repo => "same".into(),
        Some(_) => "changed".into(),
    };
    copy
}

/// The updates tab's selections. Unticked = ignored this run only.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UpdateSelection {
    pub ignored: BTreeSet<String>,
    /// Later-repo rows already given their initial untick, so a manual
    /// re-tick survives the next state refresh.
    pub seeded_explicit: BTreeSet<String>,
    pub overrides: BTreeMap<String, Override>,
}

impl UpdateSelection {
    /// After a new state: later-repo candidates start unticked (opt-in:
    /// the newest build may trade a v4-optimised one for a plain one),
    /// and names that vanished are pruned so they never ride into a
    /// later --ignore.
    pub fn reconcile(&mut self, updates: &[Update]) {
        let mut keep = BTreeSet::new();
        let mut seeded = BTreeSet::new();
        for u in updates {
            if self.ignored.contains(&u.name) {
                keep.insert(u.name.clone());
            }
            if self.seeded_explicit.contains(&u.name) {
                seeded.insert(u.name.clone());
            }
            if u.explicit_target && !seeded.contains(&u.name) {
                seeded.insert(u.name.clone());
                keep.insert(u.name.clone());
            }
        }
        self.ignored = keep;
        self.seeded_explicit = seeded;
        self.overrides.retain(|name, ov| {
            updates
                .iter()
                .find(|u| &u.name == name)
                .is_some_and(|u| !override_invalid(u, ov))
        });
    }

    pub fn toggle(&mut self, name: &str) {
        if !self.ignored.remove(name) {
            self.ignored.insert(name.to_string());
        }
    }
}

/// Everything the updates tab derives from the state and the selection.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UpdatesView {
    pub effective: Vec<Update>,
    pub pending: usize,
    pub held: usize,
    pub repo_changes: usize,
    /// Pending, unticked, not locked: the --ignore list.
    pub ignored_names: Vec<String>,
    pub selected: usize,
    /// Ticked later-repo candidates, as repo/name targets.
    pub explicit_targets: Vec<String>,
}

pub fn updates_view(updates: &[Update], sel: &UpdateSelection) -> UpdatesView {
    let effective: Vec<Update> = updates
        .iter()
        .map(|u| effective_update(u, &sel.overrides))
        .collect();
    let pending = updates.iter().filter(|u| row_pending(u)).count();
    let held = updates.iter().filter(|u| u.held).count();
    let repo_changes = effective
        .iter()
        .filter(|u| row_pending(u) && u.transition == "changed")
        .count();
    let ignored_names: Vec<String> = updates
        .iter()
        .filter(|u| {
            row_pending(u)
                && sel.ignored.contains(&u.name)
                && locked_by(updates, &sel.ignored, u).is_none()
        })
        .map(|u| u.name.clone())
        .collect();
    let explicit_targets = effective
        .iter()
        .filter(|u| u.explicit_target && row_pending(u) && !sel.ignored.contains(&u.name))
        .map(|u| format!("{}/{}", u.candidate_repo, u.name))
        .collect();
    UpdatesView {
        selected: pending - ignored_names.len(),
        effective,
        pending,
        held,
        repo_changes,
        ignored_names,
        explicit_targets,
    }
}

pub fn held_reason(u: &Update) -> &'static str {
    match u.held_by.as_deref() {
        Some("usage") => "held: no repository with Usage = Upgrade has it",
        _ => "held back by IgnorePkg/IgnoreGroup",
    }
}

/// Why "Use for upgrade" cannot take this build, or None when it can.
pub fn upgrade_blocked_reason(
    context: Context,
    option: Option<&RepoOption>,
    updates: &[Update],
    ignored: &BTreeSet<String>,
) -> Option<String> {
    if context != Context::Updates {
        return None;
    }
    let option = option?;
    let u = updates.iter().find(|x| x.name == option.name)?;
    if u.held {
        return Some(if u.held_by.as_deref() == Some("usage") {
            format!(
                "{}'s newer build is only in a repository without Usage = Upgrade; -Syu skips it.",
                u.name
            )
        } else {
            format!(
                "{} is held back by IgnorePkg/IgnoreGroup in pacman.conf; -Syu skips it.",
                u.name
            )
        });
    }
    if superseded(u) {
        return Some(format!(
            "{} is replaced by {} in this run. Untick that to keep it; the next check shows its own update.",
            u.name,
            u.superseded_by.join(", ")
        ));
    }
    if let Some(locker) = locked_by(updates, ignored, u) {
        return Some(format!(
            "{locker} replaces {} in this run; untick {locker} first.",
            u.name
        ));
    }
    if !u.replaces.is_empty() && !u.explicit_target && option.repo != u.candidate_repo {
        let names: Vec<&str> = u.replaces.iter().map(|r| r.name.as_str()).collect();
        return Some(format!(
            "Only {}/{} replaces {}; this build would not.",
            u.candidate_repo,
            u.name,
            names.join(", ")
        ));
    }
    None
}

/// The badge an update row carries, if any.
pub fn badge_label(u: &Update, superseded: bool) -> Option<&'static str> {
    if u.held {
        Some("held")
    } else if superseded {
        Some("replaced")
    } else if !u.replaces.is_empty() {
        Some("replaces")
    } else if u.transition == "changed" {
        Some("repo change")
    } else if u.explicit_target {
        Some("later repo")
    } else {
        None
    }
}

/// The version text of an update row. A replacer that is not installed
/// itself shows only its new version ("9.4 → 0.12" across two packages
/// would read like a downgrade).
pub fn version_text(u: &Update) -> String {
    let replacer_installed = !u.replaces.is_empty() && u.old_version != u.replaces[0].version;
    if u.new_install {
        format!("new package · {}", u.new_version)
    } else if !u.replaces.is_empty() && !replacer_installed {
        u.new_version.clone()
    } else {
        format!("{} → {}", u.old_version, u.new_version)
    }
}

// ---- browse / installed ---------------------------------------------------

/// Browse selections are repo/name targets, one build per package.
pub fn install_target_for(sel: &BTreeSet<String>, name: &str) -> Option<String> {
    sel.iter()
        .find(|k| k.as_str() == name || k.ends_with(&format!("/{name}")))
        .cloned()
}

pub fn select_install_target(sel: &mut BTreeSet<String>, target: &str, name: &str) {
    sel.retain(|k| k != name && !k.ends_with(&format!("/{name}")));
    sel.insert(target.to_string());
}

/// Ticking another repo's row of the same package moves the tick.
pub fn toggle_install_target(sel: &mut BTreeSet<String>, target: &str, name: &str) {
    if !sel.remove(target) {
        select_install_target(sel, target, name);
    }
}

pub fn filter_installed<'a>(
    list: &'a [InstalledEntry],
    needle: &str,
    explicit_only: bool,
) -> Vec<&'a InstalledEntry> {
    let needle = needle.trim().to_lowercase();
    list.iter()
        .filter(|p| !explicit_only || p.explicit)
        .filter(|p| {
            needle.is_empty()
                || p.name.to_lowercase().contains(&needle)
                || p.description.to_lowercase().contains(&needle)
        })
        .collect()
}

pub fn format_size(bytes: u64) -> String {
    if bytes == 0 {
        String::new()
    } else if bytes >= 1 << 30 {
        format!("{:.2} GiB", bytes as f64 / (1u64 << 30) as f64)
    } else if bytes >= 1 << 20 {
        format!("{:.1} MiB", bytes as f64 / (1u64 << 20) as f64)
    } else {
        format!("{} KiB", (bytes as f64 / 1024.0).round() as u64)
    }
}

/// "checked 5 min ago" and friends.
pub fn age_label(verb: &str, at: i64, now: i64) -> String {
    if at <= 0 {
        return format!("never {verb}");
    }
    let age = (now - at).max(0);
    if age < 90 {
        format!("{verb} just now")
    } else if age < 5400 {
        format!("{verb} {} min ago", (age as f64 / 60.0).round() as i64)
    } else if age < 36 * 3600 {
        format!("{verb} {} h ago", (age as f64 / 3600.0).round() as i64)
    } else {
        format!("{verb} {} d ago", (age as f64 / 86400.0).round() as i64)
    }
}

/// The repo build the detail view starts on.
pub fn default_detail_target(
    context: Context,
    name: &str,
    fallback_repo: &str,
    repos: &[RepoOption],
    effective: &[Update],
    overrides: &BTreeMap<String, Override>,
    install_sel: &BTreeSet<String>,
) -> String {
    let fallback = if fallback_repo.is_empty() {
        String::new()
    } else {
        format!("{fallback_repo}/{name}")
    };
    if repos.is_empty() {
        return fallback;
    }
    let has = |t: &str| repos.iter().any(|o| o.target == t);
    match context {
        Context::Updates => {
            if let Some(ov) = overrides.get(name).filter(|ov| has(&ov.target)) {
                return ov.target.clone();
            }
            if let Some(u) = effective.iter().find(|u| u.name == name) {
                let t = format!("{}/{}", u.candidate_repo, u.name);
                if has(&t) {
                    return t;
                }
            }
        }
        Context::Browse => {
            if let Some(t) = install_target_for(install_sel, name).filter(|t| has(t)) {
                return t;
            }
        }
        Context::Installed => {
            if let Some(o) = repos.iter().find(|o| o.is_installed) {
                return o.target.clone();
            }
        }
    }
    if !fallback.is_empty() && has(&fallback) {
        return fallback;
    }
    repos[0].target.clone()
}

// ---- sources --------------------------------------------------------------

/// Rows `minkarepo update` has something to do for.
pub fn source_actionable(r: &SourceRow) -> bool {
    r.tracked && matches!(r.status.as_str(), "update" | "update-by-hand" | "publish")
}

/// Ticks a fresh check starts with: what `update --all` would do
/// (automatic packages with work), nothing that needs hands.
pub fn source_default_ticks(rows: &[SourceRow]) -> BTreeSet<String> {
    rows.iter()
        .filter(|r| source_actionable(r) && r.auto)
        .map(|r| r.package.clone())
        .collect()
}

/// The version a revision bump builds as ("0.1.2-9" -> "0.1.2-10"), when
/// the row's update is a new commit on the same version.
pub fn next_revision(r: &SourceRow) -> Option<String> {
    if r.commit.is_empty() {
        return None;
    }
    let (version, rel) = r.pkgbuild.rsplit_once('-')?;
    let bare = version.split_once(':').map_or(version, |(_, v)| v);
    if bare != r.upstream {
        return None;
    }
    let major: u64 = rel.split('.').next()?.parse().ok()?;
    Some(format!("{version}-{}", major + 1))
}

/// A short phrase for a source row's state.
pub fn source_status_text(r: &SourceRow) -> String {
    match r.status.as_str() {
        "current" => "up to date".into(),
        "update" => "update".into(),
        "update-by-hand" => "update, by hand".into(),
        "watch" => "watched package moved".into(),
        "publish" => "built version not in the repo".into(),
        "older" => format!("OLDER than the published {}", r.published),
        "untracked" => "not tracked".into(),
        "error" => format!("cannot check: {}", r.error),
        other => other.to_string(),
    }
}

// ---- how a step in the terminal ended -------------------------------------

/// The core's {event:"run"}: how an upgrade, install or removal that ran in
/// the terminal ended (minka-syu's RunResult). The terminal may have closed
/// by the time anyone reads this, so this may be all that is left of its
/// output.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct RunReport {
    /// Which step: its result file's stem. Two can run at once.
    pub id: String,
    pub step: String,
    pub command: Vec<String>,
    pub ok: bool,
    /// Turned down at a question, or Ctrl+C: not ok, but her own doing.
    pub cancelled: bool,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub headline: String,
    pub excerpt: Vec<String>,
    pub tail: Vec<String>,
    pub log: String,
}

impl RunReport {
    /// Worth showing: a failure, or any outcome whose output has error
    /// lines (a pacman hook that failed does not fail the transaction). A
    /// cancel is not (the core's needs_attention).
    pub fn worth_showing(&self) -> bool {
        (!self.ok && !self.cancelled) || !self.excerpt.is_empty()
    }

    /// The output the report keeps, as one block: the error lines, then
    /// what came after the last of them.
    pub fn output_text(&self) -> String {
        let mut out = self.excerpt.join("\n");
        if !self.tail.is_empty() {
            if !out.is_empty() {
                out += "\n\n── after that ──\n";
            }
            out += &self.tail.join("\n");
        }
        if out.is_empty() {
            out = "(no output was recorded)".into();
        }
        out
    }
}

/// Files a step's report among the cards: newest first, one per step. A
/// report only ever replaces its own step's card, so one step ending
/// cleanly never takes away another's failure.
pub fn record_run(cards: &mut Vec<RunReport>, report: RunReport) {
    if !report.id.is_empty() {
        cards.retain(|c| c.id != report.id);
    }
    if report.worth_showing() {
        cards.insert(0, report);
    }
}

/// Takes away the card of one report.
pub fn dismiss_run(cards: &mut Vec<RunReport>, report: &RunReport) {
    cards.retain(|c| c != report);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up(name: &str) -> Update {
        Update {
            name: name.into(),
            old_version: "1-1".into(),
            new_version: "2-1".into(),
            installed_repo: Some("extra".into()),
            candidate_repo: "extra".into(),
            transition: "same".into(),
            ..Default::default()
        }
    }

    #[test]
    fn later_repo_rows_start_unticked_once() {
        let mut a = up("a");
        a.explicit_target = true;
        let updates = vec![a, up("b")];
        let mut sel = UpdateSelection::default();
        sel.reconcile(&updates);
        assert!(sel.ignored.contains("a"));
        // A manual re-tick survives the next refresh.
        sel.toggle("a");
        sel.reconcile(&updates);
        assert!(!sel.ignored.contains("a"));
        // A vanished name is pruned.
        sel.toggle("b");
        sel.reconcile(&updates[..1]);
        assert!(!sel.ignored.contains("b"));
    }

    #[test]
    fn counts_held_superseded_and_locked() {
        let mut held = up("held");
        held.held = true;
        let mut outer = up("outer");
        outer.replaces = vec![Replaced {
            name: "inner".into(),
            version: "1-1".into(),
            installed_repo: None,
        }];
        let mut inner = up("inner");
        inner.replaces = vec![Replaced {
            name: "old".into(),
            version: "1-1".into(),
            installed_repo: None,
        }];
        let mut old = up("old");
        old.superseded_by = vec!["inner".into()];
        let updates = vec![held, outer, inner, old];
        let mut sel = UpdateSelection::default();
        sel.ignored.insert("inner".into());
        let v = updates_view(&updates, &sel);
        assert_eq!(v.pending, 2);
        assert_eq!(v.held, 1);
        // inner is locked by the ticked outer: not in --ignore.
        assert!(v.ignored_names.is_empty());
        assert_eq!(
            locked_by(&updates, &sel.ignored, &updates[2]).as_deref(),
            Some("outer")
        );
        sel.ignored.insert("outer".into());
        let v = updates_view(&updates, &sel);
        assert_eq!(v.ignored_names, ["outer", "inner"]);
        assert_eq!(v.selected, 0);
    }

    #[test]
    fn override_moves_repo_and_marks_explicit() {
        let u = up("a");
        let mut ov = BTreeMap::new();
        ov.insert(
            "a".to_string(),
            Override {
                repo: "minka".into(),
                version: "3-1".into(),
                target: "minka/a".into(),
            },
        );
        let e = effective_update(&u, &ov);
        assert_eq!(e.candidate_repo, "minka");
        assert!(e.explicit_target);
        assert_eq!(e.first_repo.as_deref(), Some("extra"));
        assert_eq!(e.transition, "changed");
        let sel = UpdateSelection {
            overrides: ov,
            ..Default::default()
        };
        assert_eq!(updates_view(&[u], &sel).explicit_targets, ["minka/a"]);
    }

    #[test]
    fn replacement_only_from_its_own_repo() {
        let mut u = up("new");
        u.replaces = vec![Replaced {
            name: "old".into(),
            version: "9-1".into(),
            installed_repo: None,
        }];
        let other = RepoOption {
            name: "new".into(),
            repo: "minka".into(),
            target: "minka/new".into(),
            ..Default::default()
        };
        let r = upgrade_blocked_reason(
            Context::Updates,
            Some(&other),
            &[u.clone()],
            &BTreeSet::new(),
        );
        assert!(r.unwrap().contains("Only extra/new replaces old"));
        let own = RepoOption {
            repo: "extra".into(),
            ..other
        };
        assert!(
            upgrade_blocked_reason(Context::Updates, Some(&own), &[u], &BTreeSet::new()).is_none()
        );
    }

    #[test]
    fn install_ticks_move_between_builds() {
        let mut sel = BTreeSet::new();
        toggle_install_target(&mut sel, "extra/foo", "foo");
        toggle_install_target(&mut sel, "minka/foo", "foo");
        assert_eq!(sel.iter().collect::<Vec<_>>(), ["minka/foo"]);
        toggle_install_target(&mut sel, "minka/foo", "foo");
        assert!(sel.is_empty());
        // A name that merely ends the same is another package.
        toggle_install_target(&mut sel, "extra/libfoo", "libfoo");
        toggle_install_target(&mut sel, "extra/foo", "foo");
        assert_eq!(sel.len(), 2);
    }

    #[test]
    fn labels() {
        assert_eq!(age_label("checked", 0, 100), "never checked");
        assert_eq!(age_label("checked", 1000, 1030), "checked just now");
        assert_eq!(age_label("checked", 1, 601), "checked 10 min ago");
        assert_eq!(format_size(0), "");
        assert_eq!(format_size(2048), "2 KiB");
        assert_eq!(format_size(3 << 20), "3.0 MiB");
        let mut r = up("x");
        r.replaces = vec![Replaced {
            name: "y".into(),
            version: "1-1".into(),
            installed_repo: None,
        }];
        assert_eq!(version_text(&r), "2-1");
        assert_eq!(badge_label(&r, false), Some("replaces"));
    }

    #[test]
    fn source_ticks_default_to_automatic_work() {
        let row = |p: &str, status: &str, auto: bool, tracked: bool| SourceRow {
            package: p.into(),
            status: status.into(),
            auto,
            tracked,
            ..Default::default()
        };
        let rows = vec![
            row("a", "update", true, true),
            row("b", "update-by-hand", false, true),
            row("c", "current", true, true),
            row("d", "update", true, false),
            row("e", "publish", true, true),
        ];
        assert_eq!(
            source_default_ticks(&rows).into_iter().collect::<Vec<_>>(),
            ["a", "e"]
        );
        assert!(source_actionable(&rows[1]));
        assert!(!source_actionable(&rows[3]));

        let rev = |pkgbuild: &str, upstream: &str, commit: &str| SourceRow {
            pkgbuild: pkgbuild.into(),
            upstream: upstream.into(),
            commit: commit.into(),
            ..Default::default()
        };
        assert_eq!(
            next_revision(&rev("0.1.2-9", "0.1.2", "351c111")).as_deref(),
            Some("0.1.2-10")
        );
        assert_eq!(
            next_revision(&rev("3:1.39.7-1.1", "1.39.7", "abc")).as_deref(),
            Some("3:1.39.7-2")
        );
        // A new version is a version bump, not a revision.
        assert_eq!(next_revision(&rev("0.6.0-1", "0.7.0", "abc")), None);
        assert_eq!(next_revision(&rev("0.6.0-1", "0.6.0", "")), None);
    }

    #[test]
    fn run_reports_show_failures_and_error_lines() {
        let payload = serde_json::json!({
            "step": "install",
            "command": ["paru", "-S", "--sudoloop", "--", "foo"],
            "ok": false,
            "exit_code": 1,
            "signal": null,
            "headline": "Install failed: paru exited with status 1",
            "excerpt": ["error: target not found: foo"],
            "tail": [],
            "log": "/s/runs/install-1.log"
        });
        let r: RunReport = serde_json::from_value(payload).unwrap();
        assert!(r.worth_showing());
        assert_eq!(r.output_text(), "error: target not found: foo");

        // A clean success needs no card; one with error lines does.
        let clean = RunReport { ok: true, ..Default::default() };
        assert!(!clean.worth_showing());
        let hooks = RunReport {
            ok: true,
            excerpt: vec!["error: command failed to execute correctly".into()],
            tail: vec!["(4/4) Arming ConditionNeedsUpdate...".into()],
            ..Default::default()
        };
        assert!(hooks.worth_showing());
        assert_eq!(
            hooks.output_text(),
            "error: command failed to execute correctly\n\n── after that ──\n(4/4) Arming ConditionNeedsUpdate..."
        );
        // A cancel at a question is not.
        let cancelled = RunReport { ok: false, cancelled: true, ..Default::default() };
        assert!(!cancelled.worth_showing());
        // A step that never reported still says something.
        let lost = RunReport { ok: false, ..Default::default() };
        assert!(lost.worth_showing());
        assert_eq!(lost.output_text(), "(no output was recorded)");
    }

    #[test]
    fn a_step_s_report_leaves_other_steps_cards_alone() {
        let failed = RunReport {
            id: "install-1".into(),
            step: "install".into(),
            exit_code: Some(1),
            excerpt: vec!["error: failed to build 'foo-1-1':".into()],
            ..Default::default()
        };
        let hooks = RunReport {
            id: "apply-2".into(),
            step: "apply".into(),
            ok: true,
            excerpt: vec!["Error! Bad return status for module build".into()],
            ..Default::default()
        };
        let mut cards = Vec::new();
        record_run(&mut cards, failed.clone());
        record_run(&mut cards, hooks.clone());
        assert_eq!(cards, [hooks.clone(), failed.clone()]);
        // Another step that ended cleanly, or was cancelled, takes nothing away.
        record_run(&mut cards, RunReport { id: "install-3".into(), ok: true, ..Default::default() });
        record_run(&mut cards, RunReport { id: "remove-4".into(), cancelled: true, ..Default::default() });
        assert_eq!(cards, [hooks.clone(), failed.clone()]);
        // A step reported again replaces its own card only.
        let again = RunReport { headline: "again".into(), ..failed.clone() };
        record_run(&mut cards, again.clone());
        assert_eq!(cards, [again.clone(), hooks.clone()]);
        // Each card goes on its own.
        dismiss_run(&mut cards, &hooks);
        assert_eq!(cards, [again]);
    }
}
