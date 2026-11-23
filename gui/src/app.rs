//! The window's state (signals) and what its buttons do. A view over the
//! core: checking, searching and listing happen in `minka-syu serve`, and
//! every mutation (upgrade, install, remove, a MinkaPKG bump) happens in a
//! terminal the core opens. Selections live here, in memory only.

use crate::core;
use crate::model::*;
use anti_webui::prelude::*;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Updates,
    Browse,
    Installed,
    Sources,
}

/// The package the detail overlay shows, and the row facts it starts on.
#[derive(Debug, Clone, PartialEq)]
pub struct Detail {
    pub name: String,
    pub context: Context,
    pub version: String,
    pub repo: String,
    pub description: String,
}

/// An answer that belongs to one package name: a slow reply for a package
/// clicked past must not land in the open detail view.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Loaded<T> {
    pub name: String,
    pub loading: bool,
    pub loaded: bool,
    pub data: T,
}

#[derive(Clone, Copy)]
pub struct App {
    pub tab: Signal<Tab>,
    /// None until the core's first answer.
    pub state: Signal<Option<State>>,
    /// Bumped on every new state, so list rows rebuild with it.
    pub state_gen: Signal<u64>,
    pub checking: Signal<bool>,
    pub error: Signal<String>,
    /// How upgrades, installs and removals ended, when that is worth a
    /// card (it failed, or its output had error lines that matter): one
    /// card per step, newest first, each there until dismissed.
    pub run_reports: Signal<Vec<RunReport>>,
    pub sel: Signal<UpdateSelection>,
    /// The origin group's names are shown.
    pub origin_open: Signal<bool>,
    /// The untracked card was dismissed; it comes back when the list changes.
    pub untracked_dismissed: Signal<bool>,

    pub query: Signal<String>,
    pub searching: Signal<bool>,
    pub searched: Signal<bool>,
    pub results: Signal<Vec<SearchResult>>,
    pub truncated: Signal<bool>,
    pub install_sel: Signal<BTreeSet<String>>,

    pub installed: Signal<Vec<InstalledEntry>>,
    pub installed_loading: Signal<bool>,
    pub installed_loaded: Signal<bool>,
    pub filter: Signal<String>,
    pub explicit_only: Signal<bool>,
    /// `installed` through the filter: what the list rows index.
    pub visible_installed: Signal<Vec<InstalledEntry>>,
    pub remove_sel: Signal<BTreeSet<String>>,

    pub detail: Signal<Option<Detail>>,
    pub detail_target: Signal<String>,
    pub appstream: Signal<Loaded<Option<Appstream>>>,
    pub repos: Signal<Loaded<Vec<RepoOption>>>,
    /// The local DB's word on the detail view's package: its install reason.
    pub local: Signal<Loaded<Option<LocalPackage>>>,
    /// The package whose install reason a terminal is changing, until that
    /// step reports back. The shown reason is the DB's, never this guess.
    pub reason_busy: Signal<Option<String>>,
    /// The first screenshot, once downloaded (images load from files only).
    pub shot: Signal<Option<PathBuf>>,

    pub sources: Signal<Option<Sources>>,
    pub sources_loading: Signal<bool>,
    pub sources_gen: Signal<u64>,
    pub source_sel: Signal<BTreeSet<String>>,
    /// Packages already given their initial tick (see `source_default_ticks`).
    pub source_seeded: Signal<BTreeSet<String>>,
    pub show_untracked: Signal<bool>,
    /// Packages with a track/untrack request in flight.
    pub source_busy: Signal<BTreeSet<String>>,

    /// Unix seconds, bumped once a minute so "checked N min ago" stays honest.
    pub now: Signal<i64>,
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn parse<T: serde::de::DeserializeOwned + Default>(v: Value) -> T {
    serde_json::from_value(v).unwrap_or_default()
}

impl App {
    pub fn new() -> Self {
        let app = App {
            tab: signal(Tab::Updates),
            state: signal(None),
            state_gen: signal(0),
            checking: signal(false),
            error: signal(String::new()),
            run_reports: signal(Vec::new()),
            sel: signal(UpdateSelection::default()),
            origin_open: signal(false),
            untracked_dismissed: signal(false),
            query: signal(String::new()),
            searching: signal(false),
            searched: signal(false),
            results: signal(Vec::new()),
            truncated: signal(false),
            install_sel: signal(BTreeSet::new()),
            installed: signal(Vec::new()),
            installed_loading: signal(false),
            installed_loaded: signal(false),
            filter: signal(String::new()),
            explicit_only: signal(true),
            visible_installed: signal(Vec::new()),
            remove_sel: signal(BTreeSet::new()),
            detail: signal(None),
            detail_target: signal(String::new()),
            appstream: signal(Loaded::default()),
            repos: signal(Loaded::default()),
            local: signal(Loaded::default()),
            reason_busy: signal(None),
            shot: signal(None),
            sources: signal(None),
            sources_loading: signal(false),
            sources_gen: signal(0),
            source_sel: signal(BTreeSet::new()),
            source_seeded: signal(BTreeSet::new()),
            show_untracked: signal(false),
            source_busy: signal(BTreeSet::new()),
            now: signal(now_unix()),
        };
        // The installed list as filtered: rows index into it.
        effect(move || {
            let needle = app.filter.get();
            let explicit = app.explicit_only.get();
            let visible: Vec<InstalledEntry> = app.installed.with(|all| {
                filter_installed(all, &needle, explicit)
                    .into_iter()
                    .cloned()
                    .collect()
            });
            app.visible_installed.set(visible);
        });
        app
    }

    /// Starts the core and asks for the first answers. Runs in the root.
    pub fn start(self, tab: Option<Tab>, detail: Option<String>) {
        core::start(
            move |event, payload| {
                if event == "state" {
                    self.set_state(payload);
                    self.checking.set(false);
                } else if event == "run" {
                    // Two steps can run at once: each report files only
                    // its own step's card.
                    let report: RunReport = parse(payload);
                    if report.step == "reason" {
                        self.reason_done();
                        // The newest outcome of a package's reason change is
                        // the one its card (and the detail view) tells.
                        self.run_reports.update(|cards| drop_reason_cards(cards, report.command.last()));
                    }
                    self.run_reports.update(|cards| record_run(cards, report));
                }
            },
            move |status| match status {
                Ok(()) => {
                    self.error.set(String::new());
                    self.refresh();
                }
                Err(e) => {
                    self.checking.set(false);
                    self.searching.set(false);
                    self.installed_loading.set(false);
                    self.sources_loading.set(false);
                    self.error.set(e);
                }
            },
        );
        self.load_sources(false);
        self.tick_clock();
        if let Some(tab) = tab {
            self.tab.set(tab);
            if tab == Tab::Installed {
                self.load_installed();
            }
        }
        if let Some(name) = detail {
            self.open_detail(Detail {
                name,
                context: Context::Installed,
                version: String::new(),
                repo: String::new(),
                description: String::new(),
            });
        }
    }

    /// Fills the window from the core's saved answers in `dir` (its state
    /// directory), without starting the core: what `--shot` draws. Reads
    /// two files and asks nothing of anyone.
    pub fn load_saved(self, dir: &std::path::Path, tab: Tab) {
        let saved = |file: &str| {
            std::fs::read_to_string(dir.join(file))
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        };
        if let Some(v) = saved("state.json") {
            self.set_state(v);
        }
        if let Some(v) = saved("sources.json") {
            self.set_sources(parse(v));
        }
        self.tab.set(tab);
    }

    /// For `--shot … --run <result.json>`: draws the card of a step's result
    /// file (the core's $XDG_STATE_HOME/minka-syu/runs/*.json).
    pub fn load_run(self, file: &std::path::Path) {
        let report: Option<RunReport> = std::fs::read_to_string(file)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok());
        if let Some(report) = report {
            self.run_reports.update(|cards| record_run(cards, report));
        }
    }

    /// For `--shot … --detail <package> <local.json>`: opens the detail
    /// view of a package, its install reason from the file. Without a core
    /// the other answers come back empty.
    pub fn load_detail(self, name: &str, local: &std::path::Path) {
        let data: Option<LocalPackage> = std::fs::read_to_string(local)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok());
        self.open_detail(Detail {
            name: name.to_string(),
            context: Context::Installed,
            version: data.as_ref().map(|p| p.version.clone()).unwrap_or_default(),
            repo: String::new(),
            description: String::new(),
        });
        self.local.set(Loaded {
            name: name.to_string(),
            loading: false,
            loaded: true,
            data,
        });
    }

    fn tick_clock(self) {
        set_timeout(Duration::from_secs(60), move || {
            if !self.now.is_alive() {
                return;
            }
            self.now.set(now_unix());
            self.tick_clock();
        });
    }

    /// The request's error, if any, lands in the error bar; failures keep
    /// the old data on screen, so they must be visible somewhere else.
    fn request(self, method: &str, params: Value, on_ok: impl FnOnce(Value) + 'static) {
        core::request(method, params, move |reply| match reply {
            Ok(v) => on_ok(v),
            Err(e) => self.error.set(e),
        });
    }

    fn set_state(self, v: Value) {
        let state: State = parse(v);
        self.sel.update(|s| s.reconcile(&state.updates));
        let same_untracked = self
            .state
            .with_untracked(|old| old.as_ref().is_some_and(|o| o.untracked == state.untracked));
        if !same_untracked {
            self.untracked_dismissed.set(false);
        }
        self.state.set(Some(state));
        self.state_gen.update(|g| *g += 1);
    }

    pub fn refresh(self) {
        self.request("get_state", Value::Null, move |v| self.set_state(v));
    }

    pub fn recheck(self) {
        if self.checking.get_untracked() {
            return;
        }
        self.checking.set(true);
        self.error.set(String::new());
        // On success the core broadcasts the fresh state; the reply only
        // ends the in-flight flag. On failure the old state stays.
        core::request("check", Value::Null, move |reply| {
            self.checking.set(false);
            if let Err(e) = reply {
                self.error.set(e);
            }
        });
    }

    pub fn updates_view(self) -> UpdatesView {
        let state = self.state.get();
        let updates = state.as_ref().map(|s| s.updates.as_slice()).unwrap_or(&[]);
        self.sel.with(|sel| updates_view(updates, sel))
    }

    pub fn apply(self) {
        let view = untrack(|| self.updates_view());
        self.error.set(String::new());
        let mut targets = view.explicit_targets;
        targets.extend(view.origin_targets);
        self.request(
            "apply",
            json!({ "ignore": view.ignored_names, "targets": targets }),
            |_| {},
        );
    }

    /// The state's rows as they are now, for an action (untracked).
    fn updates_untracked(self) -> Vec<Update> {
        self.state
            .with_untracked(|s| s.as_ref().map(|s| s.updates.clone()).unwrap_or_default())
    }

    fn origin_group(self) -> Option<Update> {
        self.state
            .with_untracked(|s| s.as_ref().and_then(|s| s.updates.iter().find(|u| u.is_origin_group()).cloned()))
    }

    /// The origin group's own tick (see UpdateSelection::set_group).
    pub fn set_group(self, on: bool) {
        if let Some(group) = self.origin_group() {
            self.sel.update(|s| s.set_group(&group, on));
        }
    }

    /// One member's tick (see UpdateSelection::toggle_member).
    pub fn toggle_member(self, name: &str) {
        if let Some(group) = self.origin_group() {
            self.sel.update(|s| s.toggle_member(&group, name));
        }
    }

    pub fn tick_all(self) {
        self.sel.update(|s| s.tick_all());
    }

    pub fn untick_all(self) {
        let names: BTreeSet<String> = self
            .state
            .with_untracked(|s| {
                s.as_ref()
                    .map(|s| s.updates.iter().map(|u| u.name.clone()).collect())
            })
            .unwrap_or_default();
        self.sel.update(|s| s.untick_all(names));
    }

    pub fn search(self) {
        let query = self.query.get_untracked().trim().to_string();
        if self.searching.get_untracked() || query.is_empty() {
            return;
        }
        self.searching.set(true);
        self.searched.set(true);
        self.error.set(String::new());
        core::request("search", json!({ "query": query }), move |reply| {
            self.searching.set(false);
            match reply {
                Ok(v) => {
                    let found: SearchResponse = parse(v);
                    self.results.set(found.results);
                    self.truncated.set(found.truncated);
                }
                Err(e) => self.error.set(e),
            }
        });
    }

    pub fn load_installed(self) {
        if self.installed_loading.get_untracked() {
            return;
        }
        self.installed_loading.set(true);
        self.error.set(String::new());
        core::request("installed", Value::Null, move |reply| {
            self.installed_loading.set(false);
            match reply {
                Ok(v) => {
                    self.installed.set(parse(v));
                    self.installed_loaded.set(true);
                }
                Err(e) => self.error.set(e),
            }
        });
    }

    pub fn install(self, targets: Vec<String>) {
        if targets.is_empty() {
            return;
        }
        self.error.set(String::new());
        self.request("install", json!({ "packages": targets }), |_| {});
    }

    pub fn remove(self, names: Vec<String>) {
        if names.is_empty() {
            return;
        }
        self.error.set(String::new());
        self.request("remove", json!({ "packages": names }), |_| {});
    }

    // ---- detail -----------------------------------------------------------

    pub fn open_detail(self, d: Detail) {
        let name = d.name.clone();
        let start = untrack(|| {
            self.sel
                .with(|s| s.overrides.get(&name).map(|o| o.target.clone()))
                .or_else(|| self.install_sel.with(|s| install_target_for(s, &name)))
                .unwrap_or_else(|| {
                    if d.repo.is_empty() {
                        String::new()
                    } else {
                        format!("{}/{}", d.repo, name)
                    }
                })
        });
        self.detail_target.set(start);
        self.detail.set(Some(d));
        self.shot.set(None);
        self.appstream.set(Loaded {
            name: name.clone(),
            loading: true,
            loaded: false,
            data: None,
        });
        self.repos.set(Loaded {
            name: name.clone(),
            loading: true,
            loaded: false,
            data: Vec::new(),
        });

        self.load_local(name.clone());
        let n = name.clone();
        core::request("appstream", json!({ "package": name }), move |reply| {
            if self.appstream.with_untracked(|a| a.name != n) {
                return;
            }
            let data: Option<Appstream> = reply.ok().and_then(|v| serde_json::from_value(v).ok());
            let shot = data.as_ref().and_then(|a| a.screenshots.first().cloned());
            self.appstream.set(Loaded {
                name: n.clone(),
                loading: false,
                loaded: true,
                data,
            });
            if let Some(url) = shot {
                self.fetch_shot(n, url);
            }
        });
        let n = name.clone();
        core::request("package_repos", json!({ "package": name }), move |reply| {
            if self.repos.with_untracked(|r| r.name != n) {
                return;
            }
            let data: Vec<RepoOption> = reply.ok().map(parse).unwrap_or_default();
            self.repos.set(Loaded {
                name: n,
                loading: false,
                loaded: true,
                data,
            });
            self.sync_detail_target();
        });
    }

    pub fn close_detail(self) {
        self.detail.set(None);
    }

    /// Asks the local DB about `name` for the detail view (its install
    /// reason, what requires it). The answer lands only while that package
    /// is still the one shown.
    fn load_local(self, name: String) {
        let keep = self.local.with_untracked(|l| (l.name == name).then(|| l.data.clone()).flatten());
        self.local.set(Loaded {
            name: name.clone(),
            loading: true,
            loaded: false,
            data: keep,
        });
        let n = name.clone();
        core::request("local_package", json!({ "package": name }), move |reply| {
            if !self.local.is_alive() || self.local.with_untracked(|l| l.name != n) {
                return;
            }
            let data: Option<LocalPackage> = reply.ok().and_then(|v| serde_json::from_value(v).ok());
            self.local.set(Loaded {
                name: n,
                loading: false,
                loaded: true,
                data,
            });
        });
    }

    /// Marks the detail view's package a dependency or explicit, in a
    /// terminal (`sudo pacman -D`). The view keeps showing what the DB
    /// says until that step reports back (reason_done).
    pub fn set_reason(self, explicit: bool) {
        if self.reason_busy.with_untracked(|b| b.is_some()) {
            return;
        }
        let Some(p) = self.local.with_untracked(|l| l.data.clone()).filter(|p| p.installed) else {
            return;
        };
        if p.explicit == explicit {
            return;
        }
        self.reason_busy.set(Some(p.name.clone()));
        let flag = if explicit { "--asexplicit" } else { "--asdeps" };
        let name = p.name.clone();
        core::request("set_reason", json!({ "package": p.name, "explicit": explicit }), move |reply| {
            if let Err(e) = reply {
                self.reason_busy.set(None);
                // A card, not the error bar: the detail view over it shows
                // the same report (reason_section), and so does the list.
                let report = RunReport {
                    step: "reason".into(),
                    headline: format!("Install reason change did not start: {e}"),
                    command: ["sudo", "pacman", "-D", flag, "--", &name].iter().map(|a| a.to_string()).collect(),
                    ..Default::default()
                };
                self.run_reports.update(|cards| {
                    drop_reason_cards(cards, report.command.last());
                    record_run(cards, report);
                });
            }
        });
    }

    /// A reason change ended, whichever way: read the DB again, for the
    /// open detail view and the installed list.
    fn reason_done(self) {
        self.reason_busy.set(None);
        if let Some(name) = self.detail.with_untracked(|d| d.as_ref().map(|d| d.name.clone())) {
            self.load_local(name);
        }
        if self.installed_loaded.get_untracked() {
            self.load_installed();
        }
    }

    /// A chosen build the repo list no longer offers falls back to the
    /// context's default.
    fn sync_detail_target(self) {
        let Some(d) = self.detail.get_untracked() else {
            return;
        };
        let repos = self.repos.with_untracked(|r| r.data.clone());
        let target = self.detail_target.get_untracked();
        if repos.iter().any(|o| o.target == target) {
            return;
        }
        let effective = untrack(|| self.updates_view().effective);
        let t = self.sel.with_untracked(|s| {
            self.install_sel.with_untracked(|inst| {
                default_detail_target(
                    d.context,
                    &d.name,
                    &d.repo,
                    &repos,
                    &effective,
                    &s.overrides,
                    inst,
                )
            })
        });
        self.detail_target.set(t);
    }

    pub fn selected_option(self) -> Option<RepoOption> {
        let target = self.detail_target.get();
        self.repos
            .with(|r| r.data.iter().find(|o| o.target == target).cloned())
    }

    pub fn blocked_reason(self) -> Option<String> {
        let ctx = self.detail.with(|d| d.as_ref().map(|d| d.context))?;
        let option = self.selected_option();
        let state = self.state.get();
        let updates = state.as_ref().map(|s| s.updates.as_slice()).unwrap_or(&[]);
        self.sel
            .with(|s| upgrade_blocked_reason(ctx, option.as_ref(), updates, &s.ignored))
    }

    pub fn choose_detail_repo(self) {
        let Some(option) = untrack(|| self.selected_option()) else {
            return;
        };
        if untrack(|| self.blocked_reason()).is_some() {
            return;
        }
        let Some(ctx) = self
            .detail
            .with_untracked(|d| d.as_ref().map(|d| d.context))
        else {
            return;
        };
        match ctx {
            Context::Updates => {
                let updates = self.updates_untracked();
                self.sel.update(|s| s.choose_build(&updates, &option));
            }
            Context::Browse => self
                .install_sel
                .update(|s| select_install_target(s, &option.target, &option.name)),
            Context::Installed => self.install(vec![option.target.clone()]),
        }
        self.close_detail();
    }

    /// Screenshots are URLs; the toolkit loads images from files only, so
    /// they are fetched into the cache first (curl, off the UI thread).
    fn fetch_shot(self, name: String, url: String) {
        task(move || download_shot(&url)).then(move |path| {
            if !self.shot.is_alive() || self.appstream.with_untracked(|a| a.name != name) {
                return;
            }
            self.shot.set(path);
        });
    }

    // ---- sources ----------------------------------------------------------

    pub fn load_sources(self, refresh: bool) {
        if self.sources_loading.get_untracked() {
            return;
        }
        self.sources_loading.set(true);
        core::request("sources", json!({ "refresh": refresh }), move |reply| {
            self.sources_loading.set(false);
            match reply {
                Ok(v) => self.set_sources(parse(v)),
                Err(e) => self.error.set(e),
            }
        });
    }

    fn set_sources(self, s: Sources) {
        // Automatic packages with work start ticked, once each; a manual
        // untick survives the next check. Ticks on rows with nothing to do
        // are dropped.
        let mut seeded = self.source_seeded.get_untracked();
        let mut ticks = self.source_sel.get_untracked();
        ticks.retain(|p| {
            s.rows
                .iter()
                .any(|r| &r.package == p && source_actionable(r))
        });
        for p in source_default_ticks(&s.rows) {
            if seeded.insert(p.clone()) {
                ticks.insert(p);
            }
        }
        self.source_seeded.set(seeded);
        self.source_sel.set(ticks);
        if !s.available && self.tab.get_untracked() == Tab::Sources {
            self.tab.set(Tab::Updates);
        }
        self.sources.set(Some(s));
        self.sources_gen.update(|g| *g += 1);
    }

    pub fn toggle_source(self, package: &str) {
        let p = package.to_string();
        self.source_sel.update(|s| {
            if !s.remove(&p) {
                s.insert(p);
            }
        });
    }

    pub fn set_tracked(self, package: String, tracked: bool) {
        if self.source_busy.with_untracked(|b| b.contains(&package)) {
            return;
        }
        self.source_busy.update(|b| {
            b.insert(package.clone());
        });
        let p = package.clone();
        core::request(
            "sources_track",
            json!({ "package": package, "tracked": tracked }),
            move |reply| {
                self.source_busy.update(|b| {
                    b.remove(&p);
                });
                match reply {
                    Ok(v) => self.set_sources(parse(v)),
                    Err(e) => self.error.set(e),
                }
            },
        );
    }

    pub fn update_sources(self) {
        let picked: Vec<String> = self.source_sel.get_untracked().into_iter().collect();
        if picked.is_empty() {
            return;
        }
        self.error.set(String::new());
        self.request("sources_update", json!({ "packages": picked }), |_| {});
    }
}

/// Opens a link in the desktop's browser, detached (its own process
/// group, reaped in the background).
pub fn open_url(url: &str) {
    use std::os::unix::process::CommandExt;
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return;
    }
    if let Ok(mut child) = std::process::Command::new("xdg-open")
        .arg(url)
        .process_group(0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

fn shot_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache")
        });
    base.join("minka-syu/screenshots")
}

/// The cached file for a screenshot URL, downloading it when missing.
fn download_shot(url: &str) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return None;
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    url.hash(&mut h);
    let ext = url
        .rsplit('.')
        .next()
        .map(|e| e.to_ascii_lowercase())
        .filter(|e| matches!(e.as_str(), "png" | "jpg" | "jpeg" | "webp" | "gif"))
        .unwrap_or_else(|| "png".into());
    let dir = shot_dir();
    let path = dir.join(format!("{:016x}.{ext}", h.finish()));
    if path.is_file() {
        return Some(path);
    }
    std::fs::create_dir_all(&dir).ok()?;
    let part = path.with_extension(format!("{ext}.part"));
    let ok = std::process::Command::new("curl")
        .args([
            "-fsSL",
            "--max-time",
            "20",
            "--max-filesize",
            "20000000",
            "-o",
        ])
        .arg(&part)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if ok && std::fs::rename(&part, &path).is_ok() {
        Some(path)
    } else {
        let _ = std::fs::remove_file(&part);
        None
    }
}

/// An AppStream icon_url (file://, spaces as %20) as a path, if the toolkit
/// can draw that format at all (JPEG XL icons it cannot).
pub fn icon_path(icon_url: &str) -> Option<PathBuf> {
    let path = icon_url.strip_prefix("file://")?.replace("%20", " ");
    let lower = path.to_ascii_lowercase();
    [".png", ".svg", ".jpg", ".jpeg", ".webp", ".gif"]
        .iter()
        .any(|e| lower.ends_with(e))
        .then(|| PathBuf::from(path))
}
