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
    /// On pacman's own upgrade: a newer build in a lower-ranked repository,
    /// noted and not taken (an untick would --ignore pacman's upgrade too).
    /// The detail view offers it; choosing it makes the row a target.
    pub newer_repo: Option<String>,
    pub newer_version: Option<String>,
    /// What that build would replace: once it is chosen, the row's.
    pub newer_replaces: Vec<Replaced>,
    pub replaces: Vec<Replaced>,
    pub held: bool,
    /// "ignore" | "usage"
    pub held_by: Option<String>,
    pub superseded_by: Vec<String>,
    pub new_install: bool,
    pub reinstalls: Vec<String>,
    /// Same version, another repository's build: the installed build is in
    /// no repository. Also an explicit target; unticked, it is only left
    /// out, never passed to --ignore.
    pub switch_build: bool,
    /// Not empty only on the origin group's row: packages with no recorded
    /// repository whose build a repository has, reinstalled from it as
    /// repo/name targets. One row, one update, however many there are.
    pub origin_group: Vec<OriginMember>,
}

impl Update {
    pub fn is_origin_group(&self) -> bool {
        !self.origin_group.is_empty()
    }
}

/// One package of the origin group (the core's OriginMember).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct OriginMember {
    pub name: String,
    pub version: String,
    /// The repository whose copy is the installed build: its target.
    pub repo: String,
    pub explicit: bool,
}

impl OriginMember {
    pub fn target(&self) -> String {
        format!("{}/{}", self.repo, self.name)
    }
}

/// An installed package nothing tracks (the core's Untracked).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Untracked {
    pub name: String,
    pub version: String,
    pub explicit: bool,
    pub required_by: Vec<String>,
    pub optional_for: Vec<String>,
    pub installed_repo: Option<String>,
    /// "no_source" | "other_build"
    pub kind: String,
    pub repo_versions: Vec<String>,
    pub aur_version: Option<String>,
    pub hint: String,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct State {
    pub checked_at: i64,
    pub updates: Vec<Update>,
    pub repo_change_count: usize,
    pub untracked: Vec<Untracked>,
    /// Why no package in no repository was checked against the AUR.
    pub aur_unreachable: Option<String>,
}

/// What the local DB says of one package (the core's LocalPackage): the
/// detail view's install reason.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct LocalPackage {
    pub name: String,
    pub installed: bool,
    pub version: String,
    pub explicit: bool,
    pub required_by: Vec<String>,
    /// What optionally depends on it: then `pacman -Qdt` skips it as a
    /// dependency, and `-Qdtt` lists it.
    pub optional_for: Vec<String>,
}

/// What changing a package's install reason would mean, in one line, or
/// None when it means nothing worth saying.
pub fn reason_consequence(p: &LocalPackage) -> Option<String> {
    if !p.installed || !p.required_by.is_empty() {
        return None;
    }
    if !p.optional_for.is_empty() {
        let names = name_list(&p.optional_for, 4);
        return Some(if p.explicit {
            format!("Nothing requires it, only optional for {names}: as a dependency, pacman -Qdtt would list it, -Qdt would not.")
        } else {
            format!("Nothing requires it, only optional for {names}: pacman -Qdt skips it, -Qdtt lists it.")
        });
    }
    Some(if p.explicit {
        "Nothing requires it: as a dependency it would be an orphan (pacman -Qdt lists those).".into()
    } else {
        "Nothing requires it: it is an orphan (pacman -Qdt lists it).".into()
    })
}

/// "a, b, c and 3 more", with at most `max` names.
fn name_list(names: &[String], max: usize) -> String {
    let shown: Vec<&str> = names.iter().take(max).map(String::as_str).collect();
    let more = names.len().saturating_sub(max);
    format!("{}{}", shown.join(", "), if more > 0 { format!(" and {more} more") } else { String::new() })
}

/// "Required by a, b, c" with at most `max` names.
pub fn required_by_text(names: &[String], max: usize) -> String {
    if names.is_empty() {
        return "Required by nothing".into();
    }
    format!("Required by {}", name_list(names, max))
}

/// "Optional for a, b" with at most `max` names; empty for none.
pub fn optional_for_text(names: &[String], max: usize) -> String {
    if names.is_empty() {
        return String::new();
    }
    format!("Optional for {}", name_list(names, max))
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

/// Whether `repo`'s build at `version` is another build than row `u`'s
/// own candidate that the row can keep instead: not on a switch, whose
/// installed build no repository has (a copy of that version is what the
/// switch installs), nor on the origin group, which is no package. A
/// replacer that is installed already offers its own build, to remove
/// what it replaces: that is its candidate, never a keep.
fn other_build(u: &Update, repo: &str, version: &str) -> bool {
    !u.switch_build && !u.is_origin_group() && (u.candidate_repo != repo || u.new_version != version)
}

/// Whether `repo`'s build at `version` stands for what is installed, as
/// far as a row can tell: the repository the package was installed from,
/// at the installed version. The row carries no build date, so a copy
/// that repository rebuilt in place counts too (the detail view does not
/// mark that one installed). Choosing it keeps the package as it is
/// (UpdateSelection::choose_build), and a choice of it is never applied
/// (override_invalid): the two must agree, or the button ticks a row
/// whose choice is then thrown away, and the run takes the row's own
/// candidate, which she passed over. The Installed tab reinstalls a
/// rebuilt copy.
pub fn names_installed_build(u: &Update, repo: &str, version: &str) -> bool {
    other_build(u, repo, version) && u.installed_repo.as_deref() == Some(repo) && version == u.old_version
}

/// A saved repo choice that no longer applies: the update is held, it
/// became a replacement only its own repo performs, or it names the build
/// that is installed (names_installed_build). Keeping that build is no
/// upgrade: as a choice it left the row ticked with nothing to do (bpf on
/// 23/11/2026, its later repository's build chosen back for ALHP's
/// installed one), so it is never stored (UpdateSelection::choose_build)
/// and never applied; one that became installed since it was chosen goes
/// with the next state (UpdateSelection::reconcile). A switch keeps every
/// choice.
pub fn override_invalid(u: &Update, ov: &Override) -> bool {
    u.held
        || names_installed_build(u, &ov.repo, &ov.version)
        || (!u.explicit_target && !u.replaces.is_empty() && ov.repo != u.candidate_repo)
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
    copy.new_version = ov.version.clone();
    copy.candidate_repo = ov.repo.clone();
    // What a later build would replace does not carry over to another one.
    // The newer build a plain row notes brings what it replaces itself.
    if ov.repo != u.candidate_repo {
        let noted = u.newer_repo.as_deref() == Some(ov.repo.as_str())
            && u.newer_version.as_deref() == Some(ov.version.as_str());
        copy.replaces = if noted { u.newer_replaces.clone() } else { Vec::new() };
    }
    // -Syu never touches the package of an explicit row (a later-repo
    // row's first repository is not newer than what is installed, and a
    // switch has nothing newer): whichever build is chosen, only a
    // repo/name target installs it. Choosing the first repository's own
    // copy once made such a row plain, ticked, and never applied. A plain
    // row becomes a target when the build is not pacman's own, whose repo
    // and version it keeps as first_repo/first_version.
    copy.explicit_target = u.explicit_target || ov.repo != u.candidate_repo;
    if copy.explicit_target && !u.explicit_target {
        copy.first_repo = Some(u.candidate_repo.clone());
        copy.first_version = Some(u.new_version.clone());
    }
    copy.transition = match &u.installed_repo {
        None => "unknown_source".into(),
        Some(r) if *r == ov.repo => "same".into(),
        Some(_) => "changed".into(),
    };
    copy
}

/// What a row is, as far as its tick goes: an untick means something else
/// for each (Plain: --ignore; the others: only left out of the targets).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Plain,
    /// A later-repo build: a repo/name target.
    Explicit,
    Switch,
    Group,
}

impl RowKind {
    pub fn of(u: &Update) -> RowKind {
        if u.is_origin_group() {
            RowKind::Group
        } else if u.switch_build {
            RowKind::Switch
        } else if u.explicit_target {
            RowKind::Explicit
        } else {
            RowKind::Plain
        }
    }
}

/// The updates tab's selections. Unticked = ignored this run only.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UpdateSelection {
    /// Unticked rows, by row name.
    pub ignored: BTreeSet<String>,
    /// The origin group's members left out, by package name: a set of
    /// their own, so leaving a reinstall out never becomes the --ignore of
    /// a row the package gets later.
    pub left_out: BTreeSet<String>,
    /// Later-repo rows already given their initial untick, so a manual
    /// re-tick survives the next state refresh.
    pub seeded_explicit: BTreeSet<String>,
    /// Each row's kind in the last state: a row whose kind changes starts
    /// afresh, its untick and its seeding dropped.
    pub kinds: BTreeMap<String, RowKind>,
    pub overrides: BTreeMap<String, Override>,
}

impl UpdateSelection {
    /// After a new state: later-repo candidates start unticked (opt-in:
    /// the newest build may trade a v4-optimised one for a plain one),
    /// and names that vanished are pruned so they never ride into a
    /// later --ignore. Neither does an untick whose row changed kind (a
    /// switch that became a real upgrade starts ticked, as any update),
    /// nor a member left out of the origin group that is no member now.
    pub fn reconcile(&mut self, updates: &[Update]) {
        let mut keep = BTreeSet::new();
        let mut seeded = BTreeSet::new();
        let mut kinds = BTreeMap::new();
        let mut left_out = BTreeSet::new();
        for u in updates {
            let kind = RowKind::of(u);
            let same_kind = self.kinds.get(&u.name).is_none_or(|k| *k == kind);
            if same_kind && self.ignored.contains(&u.name) {
                keep.insert(u.name.clone());
            }
            if same_kind && self.seeded_explicit.contains(&u.name) {
                seeded.insert(u.name.clone());
            }
            if u.explicit_target && !seeded.contains(&u.name) {
                seeded.insert(u.name.clone());
                keep.insert(u.name.clone());
            }
            for m in &u.origin_group {
                if self.left_out.contains(&m.name) {
                    left_out.insert(m.name.clone());
                }
            }
            kinds.insert(u.name.clone(), kind);
        }
        self.ignored = keep;
        self.seeded_explicit = seeded;
        self.kinds = kinds;
        self.left_out = left_out;
        // A chosen build that is installed now (the run applied it) is
        // kept as choose_build keeps one: its row is unticked. Ticked, the
        // row would go back to its own candidate, which she passed over in
        // choosing; on a later-repo row, that is the newest build, which
        // needs her consent (aha: ALHP's build chosen over Arch's newer
        // one, applied, rechecked, and Arch's next on Apply).
        let ignored = &mut self.ignored;
        self.overrides.retain(|name, ov| {
            let Some(u) = updates.iter().find(|u| &u.name == name) else {
                return false;
            };
            if !u.held && names_installed_build(u, &ov.repo, &ov.version) {
                ignored.insert(name.clone());
                return false;
            }
            !override_invalid(u, ov)
        });
    }

    pub fn toggle(&mut self, name: &str) {
        if !self.ignored.remove(name) {
            self.ignored.insert(name.to_string());
        }
    }

    /// The detail view's button in the Updates context, on the build it
    /// shows. The installed build keeps the package as it is: the row is
    /// unticked (--ignore for this run, which holds back a plain row's
    /// upgrade; -Syu leaves a later-repo row's package alone anyway), and
    /// no choice is kept, since none would install anything. Any other
    /// build is chosen for the row, which is ticked.
    pub fn choose_build(&mut self, updates: &[Update], option: &RepoOption) {
        if keeps_installed(option, updates) {
            self.overrides.remove(&option.name);
            self.ignored.insert(option.name.clone());
            return;
        }
        self.overrides.insert(
            option.name.clone(),
            Override {
                repo: option.repo.clone(),
                version: option.version.clone(),
                target: option.target.clone(),
            },
        );
        self.ignored.remove(&option.name);
    }

    /// The origin group's own tick, as md3e's tri-state box sets it: on
    /// ticks every member, off none.
    pub fn set_group(&mut self, group: &Update, on: bool) {
        if on {
            self.ignored.remove(&group.name);
        } else {
            self.ignored.insert(group.name.clone());
        }
        for m in &group.origin_group {
            self.left_out.remove(&m.name);
        }
    }

    /// One member's own tick. With the group off, ticking a member turns
    /// the group on with that member alone; leaving the last one out
    /// turns it off by itself.
    pub fn toggle_member(&mut self, group: &Update, name: &str) {
        if group_ticked(group, self) {
            if !self.left_out.remove(name) {
                self.left_out.insert(name.to_string());
            }
            return;
        }
        self.ignored.remove(&group.name);
        for m in &group.origin_group {
            if m.name != name {
                self.left_out.insert(m.name.clone());
            }
        }
        self.left_out.remove(name);
    }

    /// Every row ticked, every member with them.
    pub fn tick_all(&mut self) {
        self.ignored.clear();
        self.left_out.clear();
    }

    /// Every row unticked (`names`: the state's rows).
    pub fn untick_all(&mut self, names: BTreeSet<String>) {
        self.ignored = names;
        self.left_out.clear();
    }
}

/// Whether the origin group runs: its row ticked, and a member with it.
pub fn group_ticked(group: &Update, sel: &UpdateSelection) -> bool {
    !sel.ignored.contains(&group.name) && group.origin_group.iter().any(|m| !sel.left_out.contains(&m.name))
}

/// Whether a member is reinstalled: the group runs, and it is not left out.
pub fn member_ticked(group: &Update, sel: &UpdateSelection, name: &str) -> bool {
    group_ticked(group, sel) && !sel.left_out.contains(name)
}

/// How many members are left out of a group that runs (none of one that
/// does not: they are all out, which its box says).
pub fn members_left_out(group: &Update, sel: &UpdateSelection) -> usize {
    if !group_ticked(group, sel) {
        return 0;
    }
    group.origin_group.iter().filter(|m| sel.left_out.contains(&m.name)).count()
}

/// The group box: on with every member, mixed with some left out, off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupTick {
    Off,
    Mixed,
    On,
}

pub fn group_tick(group: &Update, sel: &UpdateSelection) -> GroupTick {
    if !group_ticked(group, sel) {
        GroupTick::Off
    } else if members_left_out(group, sel) > 0 {
        GroupTick::Mixed
    } else {
        GroupTick::On
    }
}

/// Everything the updates tab derives from the state and the selection.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UpdatesView {
    pub effective: Vec<Update>,
    pub pending: usize,
    pub held: usize,
    pub repo_changes: usize,
    /// Pending, unticked, not locked, not a switch: the --ignore list.
    pub ignored_names: Vec<String>,
    pub selected: usize,
    /// Ticked later-repo candidates, as repo/name targets.
    pub explicit_targets: Vec<String>,
    /// The origin group's ticked members, as repo/name targets: sent with
    /// the others, shown apart (there can be many).
    pub origin_targets: Vec<String>,
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
    let unticked: Vec<&Update> = updates
        .iter()
        .filter(|u| {
            let off = if u.is_origin_group() {
                !group_ticked(u, sel)
            } else {
                sel.ignored.contains(&u.name)
            };
            row_pending(u) && off && locked_by(updates, &sel.ignored, u).is_none()
        })
        .collect();
    // An unticked switch or origin group is only left out of the targets:
    // there is no upgrade of it to hold back.
    let ignored_names: Vec<String> = unticked
        .iter()
        .filter(|u| !u.switch_build && !u.is_origin_group())
        .map(|u| u.name.clone())
        .collect();
    let explicit_targets = effective
        .iter()
        .filter(|u| {
            u.explicit_target && row_pending(u) && !u.is_origin_group() && !sel.ignored.contains(&u.name)
        })
        .map(|u| format!("{}/{}", u.candidate_repo, u.name))
        .collect();
    let origin_targets = updates
        .iter()
        .filter(|u| u.is_origin_group() && row_pending(u) && group_ticked(u, sel))
        .flat_map(|u| u.origin_group.iter())
        .filter(|m| !sel.left_out.contains(&m.name))
        .map(OriginMember::target)
        .collect();
    UpdatesView {
        selected: pending - unticked.len(),
        effective,
        pending,
        held,
        repo_changes,
        ignored_names,
        explicit_targets,
        origin_targets,
    }
}

pub fn held_reason(u: &Update) -> &'static str {
    match u.held_by.as_deref() {
        Some("usage") => "held: no repository with Usage = Upgrade has it",
        _ => "held back by IgnorePkg/IgnoreGroup",
    }
}

/// Why the detail view's button ("Use for upgrade", or "Keep installed
/// build") cannot take this build, or None when it can.
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

/// Whether choosing `option` in the Updates context keeps what is
/// installed: the detail view marks it installed, or the row takes it
/// for the installed build (names_installed_build, which override_invalid
/// applies), and it is not the row's own candidate (other_build). Not on
/// a switch (one marked installed there is a build whose date the detail
/// view could not read), nor on an origin group member, which is no row
/// of its own.
pub fn keeps_installed(option: &RepoOption, updates: &[Update]) -> bool {
    updates.iter().any(|u| {
        u.name == option.name
            && (names_installed_build(u, &option.repo, &option.version)
                || (option.is_installed && other_build(u, &option.repo, &option.version)))
    })
}

/// The detail view's button in the Updates context.
pub fn update_action_label(option: Option<&RepoOption>, updates: &[Update]) -> &'static str {
    if option.is_some_and(|o| keeps_installed(o, updates)) {
        "Keep installed build"
    } else {
        "Use for upgrade"
    }
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
    } else if u.switch_build {
        Some("switch")
    } else if u.explicit_target {
        Some("later repo")
    } else {
        None
    }
}

/// What a switch row does, in words (the core's Update::switch_note).
pub fn switch_note(u: &Update) -> String {
    if u.installed_repo.as_deref() == Some(u.candidate_repo.as_str()) {
        format!("reinstall {}'s current build", u.candidate_repo)
    } else {
        format!("switch to {}'s build", u.candidate_repo)
    }
}

/// The newer build a row notes and does not take (the core's
/// Update::newer_note, less what it replaces: newer_replaces_note), while
/// it is pacman's own upgrade: once another build is chosen for it, it
/// says what that is newer than instead.
pub fn newer_note(u: &Update) -> Option<String> {
    if u.explicit_target {
        return None;
    }
    Some(format!(
        "newer build in {}: {}",
        u.newer_repo.as_ref()?,
        u.newer_version.as_ref()?
    ))
}

/// What the noted newer build would replace, on a line of its own that
/// wraps (every package a choice removes must be readable), while the row
/// shows the note. Chosen, the build's replacements are the row's own.
pub fn newer_replaces_note(u: &Update) -> Option<String> {
    newer_note(u)?;
    if u.newer_replaces.is_empty() {
        return None;
    }
    let replaced: Vec<String> = u.newer_replaces.iter().map(|r| format!("{} {}", r.name, r.version)).collect();
    Some(format!("that build replaces {}", replaced.join(", ")))
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

/// The AUR's page for a package the detail view offers an AUR build of,
/// where its comments (build problems get reported there), votes and
/// maintainer are.
pub fn aur_page(repos: &[RepoOption]) -> Option<String> {
    repos
        .iter()
        .find(|o| o.repo == "aur")
        .map(|o| format!("https://aur.archlinux.org/packages/{}", o.name))
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

/// Takes away the cards of earlier install reason changes of `package`
/// (their command's last word): a new outcome tells the story.
pub fn drop_reason_cards(cards: &mut Vec<RunReport>, package: Option<&String>) {
    let Some(package) = package else { return };
    cards.retain(|c| !(c.step == "reason" && c.command.last() == Some(package)));
}

/// The headline of the failed install reason change of `package` that a
/// card still shows (dismissing the card ends it here too).
pub fn reason_failure(cards: &[RunReport], package: &str) -> Option<String> {
    cards
        .iter()
        .find(|r| r.step == "reason" && r.command.last().is_some_and(|p| p == package))
        .filter(|r| !r.ok && !r.cancelled)
        .map(|r| r.headline.clone())
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
    fn aur_page_only_when_the_aur_offers_it() {
        let repo = RepoOption {
            name: "lib32-taglib".into(),
            repo: "multilib".into(),
            target: "multilib/lib32-taglib".into(),
            ..Default::default()
        };
        assert_eq!(aur_page(std::slice::from_ref(&repo)), None);
        let aur = RepoOption {
            repo: "aur".into(),
            target: "aur/lib32-taglib".into(),
            ..repo.clone()
        };
        assert_eq!(
            aur_page(&[repo, aur]).as_deref(),
            Some("https://aur.archlinux.org/packages/lib32-taglib")
        );
    }

    fn switch(name: &str, from: Option<&str>, to: &str) -> Update {
        Update {
            name: name.into(),
            old_version: "0.99.beta20-2".into(),
            new_version: "0.99.beta20-2".into(),
            installed_repo: from.map(String::from),
            candidate_repo: to.into(),
            transition: if from.is_some() { "changed" } else { "unknown_source" }.into(),
            explicit_target: true,
            switch_build: true,
            ..Default::default()
        }
    }

    #[test]
    fn switch_rows_start_unticked_and_are_never_ignored() {
        let updates = vec![switch("lib32-libcaca", Some("multilib"), "minka"), up("mesa")];
        let mut sel = UpdateSelection::default();
        sel.reconcile(&updates);
        // Opt-in, like a later-repo row: unticked, and not in --ignore.
        assert!(sel.ignored.contains("lib32-libcaca"));
        let v = updates_view(&updates, &sel);
        assert_eq!((v.pending, v.selected, v.repo_changes), (2, 1, 1));
        assert!(v.ignored_names.is_empty(), "{:?}", v.ignored_names);
        assert!(v.explicit_targets.is_empty());
        // Ticked: its repo/name target.
        sel.toggle("lib32-libcaca");
        let v = updates_view(&updates, &sel);
        assert_eq!(v.selected, 2);
        assert_eq!(v.explicit_targets, ["minka/lib32-libcaca"]);
        // Unticking everything ignores only the real upgrade.
        sel.toggle("lib32-libcaca");
        sel.toggle("mesa");
        let v = updates_view(&updates, &sel);
        assert_eq!((v.selected, v.ignored_names.clone()), (0, vec!["mesa".to_string()]));
        // A held switch is shown only.
        let mut held = switch("held", Some("multilib"), "minka");
        held.held = true;
        let v = updates_view(&[held], &UpdateSelection::default());
        assert_eq!((v.pending, v.held, v.selected), (0, 1, 0));
        assert!(v.explicit_targets.is_empty() && v.ignored_names.is_empty());
    }

    #[test]
    fn a_switch_stays_a_target_whatever_build_is_chosen() {
        let u = switch("lib32-libcaca", Some("multilib"), "minka");
        let ov = |repo: &str| {
            BTreeMap::from([(
                u.name.clone(),
                Override {
                    repo: repo.into(),
                    version: "0.99.beta20-2".into(),
                    target: format!("{repo}/lib32-libcaca"),
                },
            )])
        };
        for repo in ["minka", "cachyos"] {
            let e = effective_update(&u, &ov(repo));
            assert!(e.explicit_target && e.switch_build, "{repo}");
            assert_eq!(e.first_repo, None);
            let sel = UpdateSelection { overrides: ov(repo), ..Default::default() };
            assert_eq!(updates_view(std::slice::from_ref(&u), &sel).explicit_targets, [format!("{repo}/lib32-libcaca")]);
        }
    }

    #[test]
    fn switch_labels() {
        let u = switch("lib32-libcaca", Some("multilib"), "minka");
        assert_eq!(badge_label(&u, false), Some("repo change"));
        assert_eq!(switch_note(&u), "switch to minka's build");
        assert_eq!(version_text(&u), "0.99.beta20-2 → 0.99.beta20-2");
        let foreign = switch("python-pkg_resources", None, "cachyos");
        assert_eq!(badge_label(&foreign, false), Some("switch"));
        let same = switch("x", Some("minka"), "minka");
        assert_eq!(switch_note(&same), "reinstall minka's current build");
        // The core's state file carries it as switch_build.
        let parsed: Update = serde_json::from_value(serde_json::json!({
            "name": "lib32-libcaca", "switch_build": true, "explicit_target": true
        }))
        .unwrap();
        assert!(parsed.switch_build);
    }

    fn origin(members: &[&str]) -> Update {
        Update {
            name: "(no recorded repository)".into(),
            old_version: format!("{} packages", members.len()),
            new_version: "same builds".into(),
            candidate_repo: "extra".into(),
            transition: "unknown_source".into(),
            explicit_target: true,
            origin_group: members
                .iter()
                .map(|n| OriginMember { name: n.to_string(), version: "1-1".into(), repo: "extra".into(), explicit: false })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn the_origin_group_is_one_row_ticked_as_a_whole_or_by_member() {
        let group = origin(&["zstd", "perl-clone", "simdutf"]);
        let updates = vec![group.clone(), up("mesa")];
        let mut sel = UpdateSelection::default();
        sel.reconcile(&updates);
        // Opt-in, like a switch row: unticked, and never in --ignore. Its
        // members are unticked with it: their boxes say what runs.
        let v = updates_view(&updates, &sel);
        assert_eq!((v.pending, v.selected), (2, 1));
        assert!(v.ignored_names.is_empty() && v.origin_targets.is_empty() && v.explicit_targets.is_empty(), "{v:?}");
        assert!(group.origin_group.iter().all(|m| !member_ticked(&group, &sel, &m.name)));
        assert_eq!((group_tick(&group, &sel), members_left_out(&group, &sel)), (GroupTick::Off, 0));
        // Ticked: every member's own target, and still one update.
        sel.set_group(&group, true);
        let v = updates_view(&updates, &sel);
        assert_eq!(v.selected, 2);
        assert_eq!(v.origin_targets, ["extra/zstd", "extra/perl-clone", "extra/simdutf"]);
        assert!(v.explicit_targets.is_empty(), "the row is no target of its own");
        assert_eq!(group_tick(&group, &sel), GroupTick::On);
        // One member left out: the rest go, nothing is --ignored, the box
        // is mixed.
        sel.toggle_member(&group, "perl-clone");
        let v = updates_view(&updates, &sel);
        assert_eq!(v.origin_targets, ["extra/zstd", "extra/simdutf"]);
        assert!(v.ignored_names.is_empty() && !sel.ignored.contains("perl-clone"));
        assert_eq!((group_tick(&group, &sel), members_left_out(&group, &sel)), (GroupTick::Mixed, 1));
        // Member ticks survive a new state.
        sel.reconcile(&updates);
        assert!(sel.left_out.contains("perl-clone"));
        // Every member left out: the group is off; ticking it brings them all.
        sel.toggle_member(&group, "zstd");
        sel.toggle_member(&group, "simdutf");
        let v = updates_view(&updates, &sel);
        assert_eq!((v.selected, v.origin_targets.len()), (1, 0));
        assert_eq!((group_tick(&group, &sel), members_left_out(&group, &sel)), (GroupTick::Off, 0));
        sel.set_group(&group, true);
        assert_eq!(updates_view(&updates, &sel).origin_targets.len(), 3);
        sel.set_group(&group, false);
        assert!(updates_view(&updates, &sel).origin_targets.is_empty());
        // With the group off, ticking a member turns it on with that one.
        sel.toggle_member(&group, "zstd");
        let v = updates_view(&updates, &sel);
        assert_eq!(v.origin_targets, ["extra/zstd"]);
        assert_eq!((group_tick(&group, &sel), members_left_out(&group, &sel)), (GroupTick::Mixed, 2));
        // Untick all, tick all: members follow.
        let names = updates.iter().map(|u| u.name.clone()).collect();
        sel.untick_all(names);
        assert!(group.origin_group.iter().all(|m| !member_ticked(&group, &sel, &m.name)));
        sel.tick_all();
        assert_eq!(updates_view(&updates, &sel).origin_targets.len(), 3);
        // A member that left the group is pruned.
        sel.toggle_member(&group, "zstd");
        sel.reconcile(&[origin(&["perl-clone", "simdutf"]), up("mesa")]);
        assert!(!sel.left_out.contains("zstd") && !sel.ignored.contains("zstd"));
    }

    #[test]
    fn a_member_left_out_then_upgraded_is_never_ignored() {
        // 22/11/2026 review: zstd left out of the reinstall, then a check
        // finds a real upgrade of it. Leaving it out meant "no reinstall",
        // not --ignore=zstd.
        let group = origin(&["zstd", "perl-clone", "simdutf"]);
        let mut sel = UpdateSelection::default();
        sel.reconcile(&[group.clone(), up("mesa")]);
        sel.set_group(&group, true);
        sel.toggle_member(&group, "zstd");
        let later = vec![origin(&["perl-clone", "simdutf"]), up("zstd"), up("mesa")];
        sel.reconcile(&later);
        let v = updates_view(&later, &sel);
        assert!(v.ignored_names.is_empty(), "{:?}", v.ignored_names);
        assert_eq!(v.selected, 3, "the upgrade starts ticked, as any update");
        assert!(!sel.left_out.contains("zstd"));
    }

    /// bpf on 23/11/2026: installed from ALHP's extra-x86-64-v4 (also the
    /// first repository with it), while Arch's extra has a newer pkgrel.
    fn bpf() -> Update {
        Update {
            name: "bpf".into(),
            old_version: "7.3.5-1.1".into(),
            new_version: "7.3.5-2".into(),
            installed_repo: Some("extra-x86-64-v4".into()),
            candidate_repo: "extra".into(),
            transition: "changed".into(),
            explicit_target: true,
            first_repo: Some("extra-x86-64-v4".into()),
            first_version: Some("7.3.5-1.1".into()),
            ..Default::default()
        }
    }

    fn build(name: &str, repo: &str, version: &str, installed: bool) -> RepoOption {
        RepoOption {
            name: name.into(),
            repo: repo.into(),
            version: version.into(),
            target: format!("{repo}/{name}"),
            is_installed: installed,
            ..Default::default()
        }
    }

    #[test]
    fn choosing_the_installed_build_keeps_it() {
        let updates = vec![bpf(), up("mesa")];
        let mut sel = UpdateSelection::default();
        sel.reconcile(&updates);
        let installed = build("bpf", "extra-x86-64-v4", "7.3.5-1.1", true);
        let newest = build("bpf", "extra", "7.3.5-2", false);
        assert_eq!(update_action_label(Some(&installed), &updates), "Keep installed build");
        assert_eq!(update_action_label(Some(&newest), &updates), "Use for upgrade");
        assert_eq!(update_action_label(None, &updates), "Use for upgrade");
        // Ticked by hand, then the installed build chosen: no choice is
        // kept, and the row is unticked instead of ticked with nothing to do.
        sel.toggle("bpf");
        sel.choose_build(&updates, &installed);
        assert!(sel.overrides.is_empty() && sel.ignored.contains("bpf"), "{sel:?}");
        let v = updates_view(&updates, &sel);
        assert!(v.explicit_targets.is_empty(), "{v:?}");
        assert_eq!(v.selected, 1);
        // The newest build: a ticked target, as before.
        sel.choose_build(&updates, &newest);
        assert!(!sel.ignored.contains("bpf"));
        assert_eq!(updates_view(&updates, &sel).explicit_targets, ["extra/bpf"]);
        // Back to the installed build: that choice goes, the row is unticked.
        sel.choose_build(&updates, &installed);
        assert!(sel.overrides.is_empty() && sel.ignored.contains("bpf"));

        // ALHP rebuilt 7.3.5-1.1 in place: the detail view no longer marks
        // its copy installed (another build date), but the row takes it for
        // the installed build, so the button keeps that too. As a choice,
        // override_invalid threw it away and the ticked row installed
        // extra's 7.3.5-2 (review, 23/11/2026).
        let rebuilt = build("bpf", "extra-x86-64-v4", "7.3.5-1.1", false);
        assert_eq!(update_action_label(Some(&rebuilt), &updates), "Keep installed build");
        sel.toggle("bpf");
        sel.choose_build(&updates, &rebuilt);
        assert!(sel.overrides.is_empty() && sel.ignored.contains("bpf"), "{sel:?}");
        assert!(updates_view(&updates, &sel).explicit_targets.is_empty());

        // A plain row: unticked is --ignore, held for this run.
        let plain = vec![from_lo("foo")];
        let mut sel = UpdateSelection::default();
        sel.reconcile(&plain);
        sel.choose_build(&plain, &build("foo", "lo", "1-1", true));
        assert_eq!(updates_view(&plain, &sel).ignored_names, ["foo"]);
        assert!(sel.overrides.is_empty());
        // The same for lo's copy rebuilt in place, which was a ticked
        // choice the run then dropped, upgrading from extra instead.
        let rebuilt = build("foo", "lo", "1-1", false);
        let mut sel = UpdateSelection::default();
        sel.reconcile(&plain);
        assert_eq!(update_action_label(Some(&rebuilt), &plain), "Keep installed build");
        sel.choose_build(&plain, &rebuilt);
        let v = updates_view(&plain, &sel);
        assert_eq!(v.ignored_names, ["foo"]);
        assert!(v.explicit_targets.is_empty() && sel.overrides.is_empty(), "{v:?}");
    }

    /// A plain upgrade from extra of a package installed from lo.
    fn from_lo(name: &str) -> Update {
        let mut u = up(name);
        u.installed_repo = Some("lo".into());
        u.transition = "changed".into();
        u
    }

    /// A replacer that is installed already: its own build is its
    /// candidate, which removes what it replaces.
    fn installed_replacer() -> Update {
        let mut u = up("coreutils-uutils");
        u.old_version = "0.12.0-1".into();
        u.new_version = "0.12.0-1".into();
        u.candidate_repo = "minka".into();
        u.installed_repo = Some("minka".into());
        u.replaces = vec![Replaced { name: "coreutils-hybrid".into(), version: "9.4_0.0.28-1".into(), installed_repo: None }];
        u
    }

    #[test]
    fn the_button_never_ticks_a_choice_the_run_drops() {
        // Every build the detail view could offer each kind of row, marked
        // installed or not: a build the button stores is one the run
        // applies, and one it does not store unticks the row.
        let rows = vec![bpf(), from_lo("foo"), switch("x", Some("minka"), "minka"), installed_replacer(), nvidia_settings()];
        for u in &rows {
            let repos = [
                u.installed_repo.clone(),
                Some(u.candidate_repo.clone()),
                u.first_repo.clone(),
                u.newer_repo.clone(),
                Some("other".to_string()),
            ];
            let versions = [
                Some(u.old_version.clone()),
                Some(u.new_version.clone()),
                u.first_version.clone(),
                u.newer_version.clone(),
                Some("9-9".to_string()),
            ];
            for repo in repos.iter().flatten() {
                for version in versions.iter().flatten() {
                    for marked in [false, true] {
                        let option = build(&u.name, repo, version, marked);
                        let updates = std::slice::from_ref(u);
                        if upgrade_blocked_reason(Context::Updates, Some(&option), updates, &BTreeSet::new()).is_some() {
                            continue;
                        }
                        let mut sel = UpdateSelection::default();
                        sel.choose_build(updates, &option);
                        match sel.overrides.get(&u.name) {
                            Some(ov) => {
                                assert!(!override_invalid(u, ov), "{option:?} on {u:?}");
                                assert!(!sel.ignored.contains(&u.name), "{option:?}");
                                assert_eq!(update_action_label(Some(&option), updates), "Use for upgrade");
                            }
                            None => {
                                assert!(sel.ignored.contains(&u.name), "{option:?}");
                                assert_eq!(update_action_label(Some(&option), updates), "Keep installed build");
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn keeping_is_only_for_rows_that_offer_another_build() {
        // A replacer installed already: its own build is the candidate, and
        // choosing it keeps the replacement as it is.
        let replacer = installed_replacer();
        for marked in [true, false] {
            let own = build("coreutils-uutils", "minka", "0.12.0-1", marked);
            assert!(!keeps_installed(&own, std::slice::from_ref(&replacer)));
        }
        // A switch: its installed build is in no repository.
        let sw = switch("x", Some("minka"), "minka");
        for marked in [true, false] {
            assert!(!keeps_installed(&build("x", "minka", "0.99.beta20-2", marked), std::slice::from_ref(&sw)));
        }
        // No row of that name (an origin group member).
        let group = origin(&["zstd"]);
        let zstd = build("zstd", "extra", "1-1", true);
        assert!(!keeps_installed(&zstd, std::slice::from_ref(&group)));
        let mut sel = UpdateSelection::default();
        sel.choose_build(&[group], &zstd);
        assert!(!sel.ignored.contains("zstd"), "never an --ignore of a later row");
    }

    #[test]
    fn a_choice_of_the_installed_build_is_never_applied() {
        // A choice that names the installed build (installed since it was
        // made, see the next test): the row is as the check made it, and
        // the next state drops the choice and unticks the row, as the
        // button would have.
        let u = bpf();
        let installed = Override {
            repo: "extra-x86-64-v4".into(),
            version: "7.3.5-1.1".into(),
            target: "extra-x86-64-v4/bpf".into(),
        };
        assert!(override_invalid(&u, &installed));
        let mut sel = UpdateSelection::default();
        sel.reconcile(std::slice::from_ref(&u));
        sel.toggle("bpf");
        sel.overrides.insert("bpf".into(), installed);
        assert_eq!(effective_update(&u, &sel.overrides), u, "the row as the check made it");
        sel.reconcile(std::slice::from_ref(&u));
        assert!(sel.overrides.is_empty() && sel.ignored.contains("bpf"), "{sel:?}");
        assert!(updates_view(std::slice::from_ref(&u), &sel).explicit_targets.is_empty());
        // The same on a plain row: unticked, its upgrade held for the run.
        let plain = from_lo("foo");
        let ov = Override { repo: "lo".into(), version: "1-1".into(), target: "lo/foo".into() };
        assert!(override_invalid(&plain, &ov));
        let mut sel = UpdateSelection::default();
        sel.overrides.insert("foo".into(), ov);
        sel.reconcile(std::slice::from_ref(&plain));
        assert_eq!(updates_view(std::slice::from_ref(&plain), &sel).ignored_names, ["foo"]);
        // Not on a switch, whose reinstall from its own repository has that
        // repository and version too ...
        let same = switch("x", Some("minka"), "minka");
        let ov = Override { repo: "minka".into(), version: "0.99.beta20-2".into(), target: "minka/x".into() };
        assert!(!override_invalid(&same, &ov));
        let mut sel = UpdateSelection::default();
        sel.reconcile(std::slice::from_ref(&same));
        sel.toggle("x");
        sel.overrides.insert("x".into(), ov);
        sel.reconcile(std::slice::from_ref(&same));
        assert_eq!(updates_view(std::slice::from_ref(&same), &sel).explicit_targets, ["minka/x"]);
        // ... nor on a replacer installed already, whose own build is its
        // candidate: the row stays ticked.
        let replacer = installed_replacer();
        let ov = Override {
            repo: "minka".into(),
            version: "0.12.0-1".into(),
            target: "minka/coreutils-uutils".into(),
        };
        assert!(!override_invalid(&replacer, &ov));
        let mut sel = UpdateSelection::default();
        sel.overrides.insert(replacer.name.clone(), ov);
        sel.reconcile(std::slice::from_ref(&replacer));
        assert!(sel.ignored.is_empty() && sel.overrides.len() == 1, "{sel:?}");
    }

    #[test]
    fn a_chosen_build_once_installed_never_brings_the_newest() {
        // Review, 23/11/2026: aha installed from cachyos-extra-v4 (the
        // first repository) at 0.5.1-3.1, ALHP's 0.5.1-3.2 and Arch's
        // 0.5.1-4 newer. She picks ALHP's build, applies, and the check
        // runs again: the row is Arch's build still, which she passed over,
        // and which the choice must not leave ticked.
        let row = |installed_repo: &str, old: &str| Update {
            name: "aha".into(),
            old_version: old.into(),
            new_version: "0.5.1-4".into(),
            installed_repo: Some(installed_repo.into()),
            candidate_repo: "extra".into(),
            transition: "changed".into(),
            explicit_target: true,
            first_repo: Some("cachyos-extra-v4".into()),
            first_version: Some("0.5.1-3.1".into()),
            ..Default::default()
        };
        let before = vec![row("cachyos-extra-v4", "0.5.1-3.1")];
        let mut sel = UpdateSelection::default();
        sel.reconcile(&before);
        sel.choose_build(&before, &build("aha", "extra-x86-64-v4", "0.5.1-3.2", false));
        assert_eq!(updates_view(&before, &sel).explicit_targets, ["extra-x86-64-v4/aha"]);
        // Applied, and checked again.
        let after = vec![row("extra-x86-64-v4", "0.5.1-3.2")];
        sel.reconcile(&after);
        assert!(sel.overrides.is_empty() && sel.ignored.contains("aha"), "{sel:?}");
        let v = updates_view(&after, &sel);
        assert!(v.explicit_targets.is_empty(), "{v:?}");
        assert_eq!(v.selected, 0);
        // Ticked by hand, it is Arch's build, as she asks.
        sel.toggle("aha");
        assert_eq!(updates_view(&after, &sel).explicit_targets, ["extra/aha"]);
    }

    #[test]
    fn a_later_repo_row_given_its_first_repo_s_build_stays_a_target() {
        // Installed from ALHP (1.0-1.1); cachyos (first) has an older 1.0-1,
        // extra the newer 1.0-2. -Syu leaves the package alone, so choosing
        // cachyos's copy is a target too, never a ticked row doing nothing.
        let u = Update {
            name: "foo".into(),
            old_version: "1.0-1.1".into(),
            new_version: "1.0-2".into(),
            installed_repo: Some("extra-x86-64-v4".into()),
            candidate_repo: "extra".into(),
            transition: "changed".into(),
            explicit_target: true,
            first_repo: Some("cachyos-extra-v4".into()),
            first_version: Some("1.0-1".into()),
            ..Default::default()
        };
        let updates = vec![u.clone()];
        let mut sel = UpdateSelection::default();
        sel.reconcile(&updates);
        sel.choose_build(&updates, &build("foo", "cachyos-extra-v4", "1.0-1", false));
        let e = effective_update(&u, &sel.overrides);
        assert!(e.explicit_target && e.candidate_repo == "cachyos-extra-v4", "{e:?}");
        assert_eq!(e.first_repo.as_deref(), Some("cachyos-extra-v4"));
        assert_eq!(updates_view(&updates, &sel).explicit_targets, ["cachyos-extra-v4/foo"]);
    }

    /// nvidia-settings on 23/11/2026: pacman's upgrade from cachyos-v4, and
    /// ALHP's suffix rebuild noted.
    fn nvidia_settings() -> Update {
        Update {
            name: "nvidia-settings".into(),
            old_version: "615.78.08-1".into(),
            new_version: "620.73.04-1".into(),
            installed_repo: Some("cachyos-v4".into()),
            candidate_repo: "cachyos-v4".into(),
            transition: "same".into(),
            newer_repo: Some("extra-x86-64-v4".into()),
            newer_version: Some("620.73.04-1.1".into()),
            ..Default::default()
        }
    }

    #[test]
    fn a_noted_newer_build_starts_as_pacman_s_upgrade_and_can_be_chosen() {
        let updates = vec![nvidia_settings()];
        let mut sel = UpdateSelection::default();
        sel.reconcile(&updates);
        let v = updates_view(&updates, &sel);
        // Ticked, no target, nothing ignored: -Syu upgrades it from cachyos-v4.
        assert_eq!((v.pending, v.selected, v.repo_changes), (1, 1, 0));
        assert!(v.explicit_targets.is_empty() && v.ignored_names.is_empty(), "{v:?}");
        assert_eq!(newer_note(&v.effective[0]).as_deref(), Some("newer build in extra-x86-64-v4: 620.73.04-1.1"));
        assert_eq!(badge_label(&v.effective[0], false), None);
        // The detail view's newest build: a target, which the row says.
        let newest = build("nvidia-settings", "extra-x86-64-v4", "620.73.04-1.1", false);
        assert_eq!(update_action_label(Some(&newest), &updates), "Use for upgrade");
        sel.choose_build(&updates, &newest);
        let v = updates_view(&updates, &sel);
        assert_eq!(v.explicit_targets, ["extra-x86-64-v4/nvidia-settings"]);
        assert!(v.ignored_names.is_empty());
        assert_eq!((v.selected, v.repo_changes), (1, 1));
        let e = &v.effective[0];
        assert_eq!((e.first_repo.as_deref(), e.first_version.as_deref()), (Some("cachyos-v4"), Some("620.73.04-1")));
        assert_eq!(newer_note(e), None, "it says what it is newer than instead");
        // Unticked, it is held back like any plain row.
        sel.toggle("nvidia-settings");
        let v = updates_view(&updates, &sel);
        assert!(v.explicit_targets.is_empty());
        assert_eq!(v.ignored_names, ["nvidia-settings"]);

        // The row an older core made of it (the newer build, unticked)
        // starts ticked once the state says it is pacman's upgrade.
        let mut promoted = nvidia_settings();
        promoted.candidate_repo = "extra-x86-64-v4".into();
        promoted.new_version = "620.73.04-1.1".into();
        promoted.explicit_target = true;
        promoted.first_repo = Some("cachyos-v4".into());
        promoted.first_version = Some("620.73.04-1".into());
        promoted.newer_repo = None;
        promoted.newer_version = None;
        let mut sel = UpdateSelection::default();
        sel.reconcile(&[promoted]);
        assert!(sel.ignored.contains("nvidia-settings"));
        sel.reconcile(&updates);
        assert!(updates_view(&updates, &sel).ignored_names.is_empty());

        // The state file carries it.
        let parsed: Update = serde_json::from_value(serde_json::json!({
            "name": "nvidia-settings", "candidate_repo": "cachyos-v4",
            "newer_repo": "extra-x86-64-v4", "newer_version": "620.73.04-1.1"
        }))
        .unwrap();
        assert_eq!(parsed.newer_version.as_deref(), Some("620.73.04-1.1"));
    }

    #[test]
    fn a_chosen_noted_build_says_what_it_replaces() {
        // Review, 23/11/2026: ax is -Syu's upgrade from repoA; repoB's newer
        // build replaces zm. The row says so while it notes that build, and
        // once it is chosen the row is that replacement.
        let ax = Update {
            name: "ax".into(),
            old_version: "1.0-1".into(),
            new_version: "1.1-1".into(),
            installed_repo: Some("repoA".into()),
            candidate_repo: "repoA".into(),
            transition: "same".into(),
            newer_repo: Some("repoB".into()),
            newer_version: Some("2.0-1".into()),
            newer_replaces: vec![Replaced { name: "zm".into(), version: "1.0-1".into(), installed_repo: None }],
            ..Default::default()
        };
        let updates = vec![ax.clone()];
        assert_eq!(newer_note(&ax).as_deref(), Some("newer build in repoB: 2.0-1"));
        assert_eq!(newer_replaces_note(&ax).as_deref(), Some("that build replaces zm 1.0-1"));
        assert_eq!(badge_label(&ax, false), None);
        let mut sel = UpdateSelection::default();
        sel.reconcile(&updates);
        sel.choose_build(&updates, &build("ax", "repoB", "2.0-1", false));
        let v = updates_view(&updates, &sel);
        let e = &v.effective[0];
        assert_eq!(e.replaces.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["zm"]);
        assert_eq!(badge_label(e, false), Some("replaces"));
        assert_eq!((newer_note(e), newer_replaces_note(e)), (None, None));
        assert_eq!(v.explicit_targets, ["repoB/ax"]);
        // Another build replaces nothing of it.
        sel.choose_build(&updates, &build("ax", "repoC", "2.0-1", false));
        assert!(updates_view(&updates, &sel).effective[0].replaces.is_empty());
        // Without a note, nothing to say.
        let plain = Update { newer_replaces: Vec::new(), ..ax };
        assert_eq!(newer_replaces_note(&plain), None);
        // The state file carries it.
        let parsed: Update = serde_json::from_value(serde_json::json!({
            "name": "ax", "newer_replaces": [{ "name": "zm", "version": "1.0-1" }]
        }))
        .unwrap();
        assert_eq!(parsed.newer_replaces.len(), 1);
    }

    #[test]
    fn a_row_that_changes_kind_starts_afresh() {
        // A switch row is seeded unticked; when the repository ships a newer
        // version it becomes a plain upgrade, which starts ticked.
        let mut sel = UpdateSelection::default();
        sel.reconcile(&[switch("lib32-libcaca", Some("multilib"), "minka"), up("mesa")]);
        assert!(sel.ignored.contains("lib32-libcaca"));
        let later = vec![up("lib32-libcaca"), up("mesa")];
        sel.reconcile(&later);
        let v = updates_view(&later, &sel);
        assert!(v.ignored_names.is_empty(), "{:?}", v.ignored_names);
        assert_eq!(v.selected, 2);
        // A plain row she unticked stays unticked while it stays plain ...
        sel.toggle("mesa");
        sel.reconcile(&later);
        assert_eq!(updates_view(&later, &sel).ignored_names, ["mesa"]);
        // ... and a row that turns into a later-repo one is seeded again.
        let mut explicit = up("mesa");
        explicit.explicit_target = true;
        sel.toggle("mesa");
        let again = vec![up("lib32-libcaca"), explicit];
        sel.reconcile(&again);
        assert!(sel.ignored.contains("mesa") && sel.seeded_explicit.contains("mesa"));
        // A re-tick of it survives the next state.
        sel.toggle("mesa");
        sel.reconcile(&again);
        assert!(!sel.ignored.contains("mesa"));
    }

    #[test]
    fn state_carries_the_group_and_the_untracked() {
        let state: State = serde_json::from_value(serde_json::json!({
            "updates": [{"name": "(no recorded repository)", "explicit_target": true,
                "origin_group": [{"name": "zstd", "version": "1-1", "repo": "extra", "explicit": false}]}],
            "untracked": [{"name": "gitcomet", "version": "0.2.5-1", "explicit": true, "required_by": [],
                "kind": "no_source", "hint": "find its source"}],
            "aur_unreachable": null
        }))
        .unwrap();
        assert!(state.updates[0].is_origin_group());
        assert_eq!(state.updates[0].origin_group[0].target(), "extra/zstd");
        assert_eq!(state.untracked[0].name, "gitcomet");
        // And an older state, with neither.
        let old: State = serde_json::from_value(serde_json::json!({"updates": [{"name": "a"}]})).unwrap();
        assert!(old.untracked.is_empty() && old.aur_unreachable.is_none() && !old.updates[0].is_origin_group());
    }

    #[test]
    fn reason_lines() {
        let mut p = LocalPackage { name: "libwpe".into(), installed: true, explicit: true, ..Default::default() };
        assert!(reason_consequence(&p).unwrap().contains("would be an orphan"));
        p.explicit = false;
        assert!(reason_consequence(&p).unwrap().contains("it is an orphan"));
        // Only optional dependencies name it: -Qdt skips it, -Qdtt lists it.
        p.optional_for = vec!["yt-dlp".into(), "aria2p".into()];
        let line = reason_consequence(&p).unwrap();
        assert!(line.contains("only optional for yt-dlp, aria2p") && line.contains("-Qdtt lists it"), "{line}");
        assert!(!line.contains("pacman -Qdt lists it"), "{line}");
        p.explicit = true;
        assert!(reason_consequence(&p).unwrap().contains("-Qdtt would list it, -Qdt would not"));
        assert_eq!(optional_for_text(&p.optional_for, 1), "Optional for yt-dlp and 1 more");
        assert_eq!(optional_for_text(&[], 1), "");
        p.required_by = vec!["wpebackend-fdo".into()];
        assert_eq!(reason_consequence(&p), None);
        assert_eq!(required_by_text(&p.required_by, 6), "Required by wpebackend-fdo");
        let many: Vec<String> = (0..9).map(|i| format!("p{i}")).collect();
        assert_eq!(required_by_text(&many, 2), "Required by p0, p1 and 7 more");
        assert_eq!(required_by_text(&[], 2), "Required by nothing");
    }

    #[test]
    fn a_failed_reason_change_is_told_until_its_card_goes() {
        let report = |pkg: &str, ok: bool, id: &str| RunReport {
            id: id.into(),
            step: "reason".into(),
            command: ["sudo", "pacman", "-D", "--asexplicit", "--", pkg].iter().map(|a| a.to_string()).collect(),
            ok,
            headline: if ok { "done".into() } else { format!("Install reason change failed: {pkg}") },
            ..Default::default()
        };
        let mut cards = Vec::new();
        record_run(&mut cards, report("libwpe", false, "a"));
        record_run(&mut cards, report("aria2", false, "b"));
        assert_eq!(reason_failure(&cards, "libwpe").as_deref(), Some("Install reason change failed: libwpe"));
        assert_eq!(reason_failure(&cards, "zstd"), None);
        // A later outcome of the same package replaces its card; a clean one
        // leaves none, and the line goes.
        let ok = report("libwpe", true, "c");
        drop_reason_cards(&mut cards, ok.command.last());
        record_run(&mut cards, ok);
        assert_eq!(reason_failure(&cards, "libwpe"), None);
        assert!(reason_failure(&cards, "aria2").is_some(), "another package's card stays");
        // A cancel is no failure to tell.
        let mut cancelled = report("aria2", false, "d");
        cancelled.cancelled = true;
        assert_eq!(reason_failure(&[cancelled], "aria2"), None);
        // Dismissed: gone here too.
        let card = cards[0].clone();
        dismiss_run(&mut cards, &card);
        assert_eq!(reason_failure(&cards, "aria2"), None);
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
