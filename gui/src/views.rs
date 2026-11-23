//! The window: header with tabs, an error bar, one tab body, and the
//! package detail overlay. Ported from ui/shell.qml and ui/modules/*.qml;
//! the Sources tab is new (MinkaPKG through minkarepo).

use crate::app::{App, Detail, Tab, icon_path, open_url};
use crate::model::*;
use crate::theme::*;
use anti_webui::prelude::*;
use std::rc::Rc;

pub fn root(app: App) -> Div {
    let body = div()
        .flex_1()
        .min_h(0.0_f32)
        .w_full()
        .flex()
        .flex_col()
        .child(dyn_view(move || {
            let tab = app.tab.get();
            // Built untracked: only the tab switch rebuilds the body; everything
            // inside reacts through its own closures.
            untrack(|| match tab {
                Tab::Updates => updates_tab(app),
                Tab::Browse => browse_tab(app),
                Tab::Installed => installed_tab(app),
                Tab::Sources => sources_tab(app),
            })
        }));

    div()
        .size_full()
        .relative()
        .bg(color(GROUND))
        .text_color(color(TEXT))
        .font_family(FONT_FAMILY)
        .text_size(FONT_SIZE)
        .scrollbar_color(color_alpha(TEXT_FAINT, 0.6_f32))
        .on_key_down(move |e: &KeyboardEvent| {
            if e.key == Key::Named(NamedKey::Escape) && app.detail.with_untracked(|d| d.is_some()) {
                e.prevent_default();
                app.close_detail();
            }
        })
        .child(
            div()
                .size_full()
                .p(18.0_f32)
                .flex()
                .flex_col()
                .gap(12.0_f32)
                .child(
                    // The busy line sits close under the header, as part of it.
                    div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .flex_none()
                        .gap(2.0_f32)
                        .child(header(app))
                        .child(busy_strip(move || match app.tab.get() {
                            Tab::Updates => {
                                app.checking.get() || app.state.with(|s| s.is_none())
                            }
                            Tab::Browse => app.searching.get(),
                            Tab::Installed => app.installed_loading.get(),
                            Tab::Sources => app.sources_loading.get(),
                        })),
                )
                .child(error_bar(app))
                .child(run_card(app))
                .child(body),
        )
        .child(show(
            move || app.detail.with(|d| d.is_some()),
            move || untrack(|| detail_overlay(app)),
        ))
}

// ---- header ---------------------------------------------------------------

fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

fn subtitle(app: App) -> String {
    match app.tab.get() {
        Tab::Browse => {
            if app.searching.get() {
                "searching repos + AUR…".into()
            } else if app.searched.get() {
                let n = app.results.with(|r| r.len());
                format!(
                    "{}{}",
                    plural(n, "result"),
                    if app.truncated.get() {
                        " (truncated — refine the query)"
                    } else {
                        ""
                    }
                )
            } else {
                "search the sync repos and the AUR".into()
            }
        }
        Tab::Installed => {
            if app.installed_loading.get() {
                "reading the local database…".into()
            } else if app.installed_loaded.get() {
                format!(
                    "{} of {} packages shown",
                    app.visible_installed.with(|v| v.len()),
                    app.installed.with(|v| v.len())
                )
            } else {
                "not loaded yet".into()
            }
        }
        Tab::Sources => match app.sources.get() {
            _ if app.sources_loading.get()
                && app
                    .sources
                    .with(|s| s.as_ref().is_none_or(|s| s.rows.is_empty())) =>
            {
                "asking upstream about every package…".into()
            }
            None => "not checked yet".into(),
            Some(s) => {
                let tracked = s.rows.iter().filter(|r| r.tracked).count();
                let work = s.rows.iter().filter(|r| source_actionable(r)).count();
                let errors = s
                    .rows
                    .iter()
                    .filter(|r| r.tracked && r.status == "error")
                    .count();
                let mut out = format!(
                    "{} tracked · {} with something to do",
                    plural(tracked, "package"),
                    work
                );
                if errors > 0 {
                    out += &format!(" · {errors} cannot be checked");
                }
                let untracked = s.rows.len() - tracked;
                if untracked > 0 {
                    out += &format!(" · {untracked} not tracked");
                }
                out
            }
        },
        Tab::Updates => {
            if app.state.with(|s| s.is_none()) {
                return "waiting for the core…".into();
            }
            if app.checking.get() {
                return "checking…".into();
            }
            let v = app.updates_view();
            let checked_at = app.state.with(|s| s.as_ref().map_or(0, |s| s.checked_at));
            let held = if v.held > 0 {
                format!(" · {} held", v.held)
            } else {
                String::new()
            };
            if v.pending == 0 {
                return if checked_at > 0 {
                    format!("everything is up to date{held}")
                } else {
                    "no check yet".into()
                };
            }
            let mut s = format!("{} available", plural(v.pending, "update"));
            if v.repo_changes > 0 {
                // The QML's wording: "1 changes repository", "3 change repository".
                let verb = if v.repo_changes == 1 {
                    "changes"
                } else {
                    "change"
                };
                s += &format!(" — {} {verb} repository", v.repo_changes);
            }
            // Unticked switches are not in ignored_names (they are only left
            // out), so the count comes from the ticks.
            let unticked = v.pending - v.selected;
            if unticked > 0 {
                s += &format!(" · {unticked} unticked");
            }
            s + &held
        }
    }
}

fn age_line(app: App) -> String {
    let now = app.now.get();
    match app.tab.get() {
        Tab::Updates => match app.state.with(|s| s.as_ref().map(|s| s.checked_at)) {
            None => String::new(),
            Some(at) => age_label("checked", at, now),
        },
        Tab::Sources => match app
            .sources
            .with(|s| s.as_ref().map(|s| (s.checked_at, s.minkarepo.clone())))
        {
            None => String::new(),
            Some((at, path)) => format!("{} · {path}", age_label("checked", at, now)),
        },
        _ => String::new(),
    }
}

/// The tabs as one connected group. Sources is there only when the core
/// found MinkaPKG's `minkarepo`.
fn tab_switch(app: App, with_sources: bool) -> Div {
    let mut tabs = vec![
        ("Updates", Tab::Updates),
        ("Browse", Tab::Browse),
        ("Installed", Tab::Installed),
    ];
    if with_sources {
        tabs.push(("Sources", Tab::Sources));
    }
    let order: Rc<[Tab]> = tabs.iter().map(|t| t.1).collect();
    let shown = Rc::clone(&order);
    segmented(
        tabs.iter().map(|t| t.0).collect(),
        move |i| app.tab.get() == shown[i],
        move |i| {
            let tab = order[i];
            app.tab.set(tab);
            if tab == Tab::Installed && !app.installed_loaded.get_untracked() {
                app.load_installed();
            }
        },
    )
}

fn header(app: App) -> Div {
    // A memo, so the tabs are built again only when Sources comes or goes,
    // not on every answer about the sources.
    let has_sources = memo(move || {
        app.sources
            .with(|s| s.as_ref().is_some_and(|s| s.available))
    });
    div()
        .w_full()
        .flex()
        .flex_row()
        .items_start()
        .gap(10.0_f32)
        .child(
            div()
                .flex_1()
                .min_w(0.0_f32)
                .flex()
                .flex_col()
                .gap(2.0_f32)
                .child(
                    ui_text(
                        move || match app.tab.get() {
                            Tab::Updates => "System updates",
                            Tab::Browse => "Browse packages",
                            Tab::Installed => "Installed packages",
                            Tab::Sources => "My packages (MinkaPKG)",
                        },
                        FONT_SIZE + 4.0_f32,
                        TEXT,
                    )
                    .bold(),
                )
                .child(
                    ui_text(move || subtitle(app), FONT_SIZE - 1.0_f32, TEXT_MUTED).dyn_style(
                        move |s| {
                            let amber = app.tab.get() == Tab::Updates
                                && app.updates_view().repo_changes > 0;
                            s.text_color(color(if amber { WARN_AMBER } else { TEXT_MUTED }))
                        },
                    ),
                )
                .child(ui_text(
                    move || age_line(app),
                    FONT_SIZE - 2.0_f32,
                    TEXT_FAINT,
                )),
        )
        // The tab's own button goes before the tabs, which then stay where
        // they are (at the right edge) on every tab.
        .child(show(
            move || app.tab.get() == Tab::Updates,
            move || {
                push_button(
                    move || {
                        if app.checking.get() {
                            "Checking…".into()
                        } else {
                            "Recheck".into()
                        }
                    },
                    false,
                    move || !app.checking.get(),
                    move || app.recheck(),
                )
            },
        ))
        .child(show(
            move || app.tab.get() == Tab::Sources,
            move || {
                push_button(
                    move || {
                        if app.sources_loading.get() {
                            "Checking…".into()
                        } else {
                            "Check upstream".into()
                        }
                    },
                    false,
                    move || !app.sources_loading.get(),
                    move || app.load_sources(true),
                )
            },
        ))
        .child(dyn_view(move || {
            let with_sources = has_sources.get();
            untrack(|| tab_switch(app, with_sources))
        }))
}

/// Failures keep the old data on screen, so the failure has to show
/// somewhere that is not the list.
fn error_bar(app: App) -> impl IntoView {
    show(
        move || app.error.with(|e| !e.is_empty()),
        move || {
            div()
                .w_full()
                .flex()
                .flex_row()
                .items_center()
                .gap(10.0_f32)
                .px(8.0_f32)
                .py(6.0_f32)
                .rounded(5.0_f32)
                .bg(color(SURFACE))
                .border(1.0_f32)
                .border_color(color(RED_DIM))
                .child(div().flex_1().min_w(0.0_f32).child(ui_text(
                    move || app.error.get(),
                    FONT_SIZE - 1.0_f32,
                    RED,
                )))
                .child(text_button(
                    || "dismiss".into(),
                    TEXT_MUTED,
                    move || app.error.set(String::new()),
                ))
        },
    )
}

/// How upgrades, installs and removals ended, when one failed or its
/// output had error lines that matter: a card per step, newest first. Its
/// terminal may have closed by now, so its error lines are kept here,
/// selectable, with the path of the full log.
fn run_card(app: App) -> impl IntoView {
    show(
        move || app.run_reports.with(|r| !r.is_empty()),
        move || {
            dyn_view(move || {
                let mut cards = div().w_full().flex().flex_col().flex_none().gap(6.0_f32);
                for (i, report) in app.run_reports.get().into_iter().enumerate() {
                    // Older cards stay shorter, so the list stays in view.
                    let max_h = if i == 0 { RUN_OUTPUT_MAX_H } else { RUN_OUTPUT_OLDER_MAX_H };
                    cards = cards.child(run_report_card(app, report, max_h));
                }
                cards
            })
        },
    )
}

/// How high the output may grow before it scrolls: the list below stays in view.
const RUN_OUTPUT_MAX_H: f32 = 220.0_f32;
/// The same for the cards under the newest one: about six lines.
const RUN_OUTPUT_OLDER_MAX_H: f32 = 96.0_f32;

fn run_report_card(app: App, r: RunReport, output_max_h: f32) -> Div {
    // Red for a failure; amber for a step that finished but printed errors.
    let (tone, outline) = if r.ok { (WARN_AMBER, WARN_AMBER) } else { (RED, RED_DIM) };
    let mut card = outlined_card_in(outline)
        .w_full()
        .flex_none()
        .gap(6.0_f32)
        .px(10.0_f32)
        .py(8.0_f32)
        .child(
            div()
                .w_full()
                .flex()
                .flex_row()
                .items_center()
                .gap(10.0_f32)
                .child(
                    div()
                        .flex_1()
                        .min_w(0.0_f32)
                        .child(
                            ui_text(
                                if r.headline.is_empty() {
                                    "The last step failed, and no details came back".to_string()
                                } else {
                                    r.headline.clone()
                                },
                                FONT_SIZE,
                                tone,
                            )
                            .bold(),
                        ),
                )
                .child({
                    let this = r.clone();
                    push_button(
                        || "Dismiss".to_string(),
                        false,
                        || true,
                        move || app.run_reports.update(|cards| dismiss_run(cards, &this)),
                    )
                }),
        );
    if !r.command.is_empty() {
        card = card.child(mono_text(r.command.join(" "), FONT_SIZE - 2.0_f32, TEXT_FAINT));
    }
    card = card.child(
        // The text block is the scroller's direct child (the toolkit measures
        // scroll extent from those), padded clear of its 12 px scrollbar zone.
        div()
            .w_full()
            .max_h(output_max_h)
            .overflow_y_scroll()
            .rounded(4.0_f32)
            .bg(color(GROUND))
            .selection_color(color_alpha(RED_DIM, 0.6_f32))
            .child(
                div()
                    .w_full()
                    .py(6.0_f32)
                    .pl(8.0_f32)
                    .pr(14.0_f32)
                    .child(mono_text(r.output_text(), FONT_SIZE - 2.0_f32, TEXT)),
            ),
    );
    if !r.log.is_empty() {
        card = card.child(faint_note(format!("Full log: {}", r.log)));
    }
    card
}

fn faint_note(source: impl IntoTextSource) -> Text {
    ui_text(source, FONT_SIZE - 2.0_f32, TEXT_FAINT)
}

fn footer(note: impl Fn() -> String + 'static, buttons: Vec<Div>) -> Div {
    let mut row = div()
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(12.0_f32)
        .child(div().flex_1().min_w(0.0_f32).child(faint_note(note)));
    for b in buttons {
        row = row.child(b);
    }
    row
}

/// A vertical scroller over one content column. The column is the direct
/// child (the toolkit measures scroll extent from direct children only);
/// the right padding keeps controls out of the 12 px scrollbar zone.
fn scroller(column: Div) -> Div {
    div()
        .flex_1()
        .min_h(0.0_f32)
        .w_full()
        .overflow_y_scroll()
        .child(column.w_full().pr(14.0_f32))
}

fn empty_text(source: impl IntoTextSource) -> Div {
    div()
        .w_full()
        .flex()
        .justify_center()
        .pt(24.0_f32)
        .child(ui_text(source, FONT_SIZE, TEXT_MUTED))
}

fn name_link(name: impl Fn() -> String + 'static, size: f32, on_click: impl Fn() + 'static) -> Div {
    div()
        .flex_none()
        .max_w(pct(100.0_f32))
        .overflow_hidden()
        .cursor(CursorIcon::Pointer)
        .hover(|s| s.opacity(0.75_f32))
        .on_click(move |e: &PointerEvent| {
            e.stop_propagation();
            on_click();
        })
        .child(ui_text(name, size, TEXT).whitespace_nowrap())
}

// ---- updates --------------------------------------------------------------

fn locked_of(app: App, name: &str) -> Option<String> {
    app.state.with(|s| {
        let s = s.as_ref()?;
        let u = s.updates.iter().find(|u| u.name == name)?;
        app.sel.with(|sel| locked_by(&s.updates, &sel.ignored, u))
    })
}

fn update_row(app: App, u: Update) -> Div {
    let held = u.held;
    let sup = superseded(&u);
    let inactive = held || sup;
    let replacement = !u.replaces.is_empty();
    let repo_changed = u.transition == "changed";

    let name: Rc<str> = Rc::from(u.name.as_str());
    let selected = {
        let n = Rc::clone(&name);
        move || !app.sel.with(|s| s.ignored.contains(&*n))
    };
    let locked = {
        let n = Rc::clone(&name);
        move || !inactive && locked_of(app, &n).is_some()
    };
    let checked = {
        let (selected, locked) = (selected.clone(), locked.clone());
        move || (selected() || locked()) && !inactive
    };
    let enabled = {
        let n = Rc::clone(&name);
        move || !inactive && locked_of(app, &n).is_none()
    };

    let mut details = div()
        .w_full()
        .flex()
        .flex_row()
        .flex_wrap()
        .column_gap(6.0_f32)
        .row_gap(1.0_f32)
        .child(mono_text(version_text(&u), FONT_SIZE - 2.0_f32, TEXT_MUTED).whitespace_nowrap());
    let from = u.installed_repo.clone().unwrap_or_default();
    if repo_changed {
        details = details.child(
            mono_text(
                format!("{from} → {}", u.candidate_repo),
                FONT_SIZE - 2.0_f32,
                WARN_AMBER,
            )
            .whitespace_nowrap(),
        );
    } else if u.transition == "unknown_source" {
        details = details.child(
            mono_text(
                format!("unknown source → {}", u.candidate_repo),
                FONT_SIZE - 2.0_f32,
                TEXT_FAINT,
            )
            .whitespace_nowrap(),
        );
    } else {
        details = details.child(
            mono_text(u.candidate_repo.clone(), FONT_SIZE - 2.0_f32, TEXT_FAINT)
                .whitespace_nowrap(),
        );
    }
    if u.switch_build {
        // Where a later-repo row says what it is newer than: the version is
        // the same, the build is the other repository's.
        details = details.child(
            mono_text(switch_note(&u), FONT_SIZE - 2.0_f32, PURPLE).whitespace_nowrap(),
        );
    } else if u.explicit_target {
        details = details.child(
            mono_text(
                format!(
                    "newer than {} {}",
                    u.first_repo.as_deref().unwrap_or("?"),
                    u.first_version.as_deref().unwrap_or("?")
                ),
                FONT_SIZE - 2.0_f32,
                PURPLE,
            )
            .whitespace_nowrap(),
        );
    }
    if held {
        details = details.child(faint_note(held_reason(&u)));
    }
    if sup {
        details = details.child(faint_note(format!(
            "replaced by {} in this run — untick that to keep it",
            u.superseded_by.join(", ")
        )));
    }
    details = details.child(show(locked.clone(), {
        let n = Rc::clone(&name);
        move || {
            let n = Rc::clone(&n);
            faint_note(move || {
                let by = locked_of(app, &n).unwrap_or_default();
                format!("{by} replaces it in this run — untick {by} first")
            })
        }
    }));

    let (version, repo) = (u.new_version.clone(), u.candidate_repo.clone());
    let shown = u.name.clone();
    let for_detail = Rc::clone(&name);
    let mut column = div()
        .flex_1()
        .min_w(0.0_f32)
        .flex()
        .flex_col()
        .gap(1.0_f32)
        .dyn_style({
            let selected = selected.clone();
            move |s| {
                s.opacity(if selected() && !inactive {
                    1.0_f32
                } else {
                    0.45_f32
                })
            }
        })
        .child(name_link(
            move || shown.clone(),
            FONT_SIZE,
            move || {
                app.open_detail(Detail {
                    name: for_detail.to_string(),
                    context: Context::Updates,
                    version: version.clone(),
                    repo: repo.clone(),
                    description: String::new(),
                })
            },
        ))
        .child(details);
    if replacement {
        let replaced: Vec<String> = u
            .replaces
            .iter()
            .map(|r| format!("{} {}", r.name, r.version))
            .collect();
        // Every package this removes must be readable: wrapped, never cut.
        column = column.child(mono_text(
            format!("replaces {}", replaced.join(", ")),
            FONT_SIZE - 2.0_f32,
            RED,
        ));
    }

    let mut row = div()
        .w_full()
        .flex()
        .flex_row()
        .items_start()
        .gap(10.0_f32)
        .child(check_box(checked, enabled, move || {
            app.sel.update(|s| s.toggle(&name))
        }))
        .child(column);
    if let Some(label) = badge_label(&u, sup) {
        let (bg, fg) = if inactive {
            (LINE, TEXT)
        } else if replacement {
            (RED_DIM, TEXT)
        } else if repo_changed {
            (WARN_AMBER, GROUND)
        } else {
            (PURPLE_DIM, TEXT)
        };
        row = row.child(chip(label, bg, fg));
    }
    row
}

fn update_list(app: App, aur: bool) -> impl IntoView {
    for_each(
        move || {
            let generation = app.state_gen.get();
            let mut rows: Vec<(u64, Update)> = app
                .updates_view()
                .effective
                .into_iter()
                .filter(|u| (u.candidate_repo == "aur") == aur)
                .map(|u| (generation, u))
                .collect();
            // The origin group is bookkeeping, not an update: after them.
            rows.sort_by_key(|(_, u)| u.is_origin_group());
            rows
        },
        |(generation, u)| {
            (
                *generation,
                u.name.clone(),
                u.candidate_repo.clone(),
                u.new_version.clone(),
            )
        },
        move |(_, u)| {
            untrack(|| {
                if u.is_origin_group() {
                    origin_group_row(app, u)
                } else {
                    update_row(app, u)
                }
            })
        },
    )
}

/// The origin group: one row for every package with no recorded
/// repository whose build a repository has. Its box ticks them all, and
/// shows a mix when some are left out; its names, shown on demand under
/// their repositories, can be left out one by one.
fn origin_group_row(app: App, u: Update) -> Div {
    let n = u.origin_group.len();
    let one = n == 1;
    let group = Rc::new(u.clone());
    let ticked = {
        let g = Rc::clone(&group);
        move || app.sel.with(|s| group_ticked(&g, s))
    };
    let tick = {
        let g = Rc::clone(&group);
        move || app.sel.with(|s| group_tick(&g, s))
    };
    // The members by repository, in pacman.conf order (the row's own list,
    // then any repository it lacks).
    let mut repos: Vec<&str> = u.candidate_repo.split(", ").filter(|r| !r.is_empty()).collect();
    for m in &u.origin_group {
        if !repos.contains(&m.repo.as_str()) {
            repos.push(&m.repo);
        }
    }
    let by_repo: Vec<(String, Vec<OriginMember>)> = repos
        .iter()
        .map(|repo| (repo.to_string(), u.origin_group.iter().filter(|m| m.repo == *repo).cloned().collect::<Vec<_>>()))
        .filter(|(_, members)| !members.is_empty())
        .collect();
    let counts: Vec<String> = by_repo.iter().map(|(repo, members)| format!("{repo} {}", members.len())).collect();
    let explicit = u.origin_group.iter().filter(|m| m.explicit).count();
    let reasons = match (explicit, n) {
        (0, 1) => "a dependency".to_string(),
        (1, 1) => "explicit".to_string(),
        (0, _) => "all dependencies".to_string(),
        (e, _) if e == n => "all explicit".to_string(),
        (e, _) => format!("{} dependenc{}, {e} explicit", n - e, if n - e == 1 { "y" } else { "ies" }),
    };

    let title = format!("{} {} no recorded repository", plural(n, "package"), if one { "has" } else { "have" });
    let explanation = if one {
        "pacman records none for packages it pulls in as dependencies or replacements. \
         Its build is the repository's own: reinstalling it from there records it (so does its next upgrade), \
         and keeps its install reason."
    } else {
        "pacman records none for packages it pulls in as dependencies or replacements. \
         Their builds are the repositories' own: reinstalling them from there records it (so does their next upgrade), \
         and keeps each install reason."
    };
    let mut column = div()
        .flex_1()
        .min_w(0.0_f32)
        .flex()
        .flex_col()
        .gap(1.0_f32)
        .dyn_style({
            let ticked = ticked.clone();
            move |s| s.opacity(if ticked() { 1.0_f32 } else { 0.45_f32 })
        })
        .child(ui_text(title, FONT_SIZE, TEXT))
        .child(ui_text(explanation, FONT_SIZE - 2.0_f32, TEXT_MUTED))
        .child(
            div()
                .w_full()
                .flex()
                .flex_row()
                .flex_wrap()
                .column_gap(6.0_f32)
                .row_gap(1.0_f32)
                .child(mono_text(format!("{} · {reasons}", counts.join(" · ")), FONT_SIZE - 2.0_f32, TEXT_FAINT))
                .child(faint_note({
                    let g = Rc::clone(&group);
                    move || match app.sel.with(|s| members_left_out(&g, s)) {
                        0 => String::new(),
                        k => format!("{k} of {n} left out"),
                    }
                })),
        )
        .child(div().pt(2.0_f32).child(text_button(
            move || match (app.origin_open.get(), one) {
                (true, true) => "hide the name".into(),
                (true, false) => "hide the names".into(),
                (false, true) => "show its name".into(),
                (false, false) => format!("show the {n} names"),
            },
            PURPLE,
            move || app.origin_open.update(|o| *o = !*o),
        )));
    column = column.child(show(
        move || app.origin_open.get(),
        move || {
            // A faint heading per repository: the repository is never cut off
            // an item, and the items size to their own text.
            let mut list = div().w_full().pt(4.0_f32).flex().flex_col().gap(2.0_f32);
            for (repo, members) in &by_repo {
                list = list.child(mono_text(format!("{repo} {}", members.len()), FONT_SIZE - 3.0_f32, TEXT_FAINT));
                let mut grid = div().w_full().flex().flex_row().flex_wrap().row_gap(0.0_f32).column_gap(8.0_f32);
                for m in members.iter().cloned() {
                    grid = grid.child(origin_member_item(app, Rc::clone(&group), m));
                }
                list = list.child(grid);
            }
            list
        },
    ));

    div()
        .w_full()
        .flex()
        .flex_row()
        .items_start()
        .gap(10.0_f32)
        .child(tri_check_box(
            move || match tick() {
                GroupTick::Off => CheckState::Off,
                GroupTick::Mixed => CheckState::Mixed,
                GroupTick::On => CheckState::On,
            },
            move |on| app.set_group(on),
        ))
        .child(column)
        .child(chip("reinstall", PURPLE_DIM, TEXT))
}

/// One name of the origin group: its own tick (on only while the group
/// runs), the name (opens the detail view, where its install reason can be
/// changed), its version.
fn origin_member_item(app: App, group: Rc<Update>, m: OriginMember) -> Div {
    let name: Rc<str> = Rc::from(m.name.as_str());
    let selected = {
        let (n, g) = (Rc::clone(&name), Rc::clone(&group));
        move || app.sel.with(|s| member_ticked(&g, s, &n))
    };
    // Its own dimming: the column already dims a group that is off.
    let kept = {
        let n = Rc::clone(&name);
        move || !app.sel.with(|s| s.left_out.contains(&*n))
    };
    let shown = m.name.clone();
    let (detail_name, version) = (Rc::clone(&name), m.version.clone());
    let mut item = div()
        .flex_none()
        .min_w(232.0_f32)
        .max_w(pct(100.0_f32))
        .h(26.0_f32)
        .flex()
        .flex_row()
        .items_center()
        .gap(6.0_f32)
        .overflow_hidden()
        .child(check_box(selected, || true, {
            let n = Rc::clone(&name);
            move || app.toggle_member(&n)
        }))
        .child(
            div()
                .flex_none()
                .flex()
                .flex_row()
                .items_center()
                .gap(6.0_f32)
                .dyn_style(move |s| s.opacity(if kept() { 1.0_f32 } else { 0.45_f32 }))
                .child(name_link(
                    move || shown.clone(),
                    FONT_SIZE - 1.0_f32,
                    move || {
                        app.open_detail(Detail {
                            name: detail_name.to_string(),
                            context: Context::Installed,
                            version: version.clone(),
                            repo: String::new(),
                            description: String::new(),
                        })
                    },
                ))
                .child(mono_text(m.version.clone(), FONT_SIZE - 3.0_f32, TEXT_FAINT).whitespace_nowrap()),
        );
    if m.explicit {
        item = item.child(mono_text("explicit", FONT_SIZE - 3.0_f32, OK_GREEN).whitespace_nowrap());
    }
    item
}

/// Packages nothing tracks: an error to fix, in a card of its own above the
/// list, until dismissed (it comes back when the list changes). When the
/// AUR could not be asked, an amber card says so instead.
fn untracked_card(app: App) -> impl IntoView {
    let shown = move || {
        !app.untracked_dismissed.get()
            && app.state.with(|s| s.as_ref().is_some_and(|s| !s.untracked.is_empty() || s.aur_unreachable.is_some()))
    };
    show(shown, move || {
        dyn_view(move || {
            let (list, note) = app
                .state
                .with(|s| s.as_ref().map(|s| (s.untracked.clone(), s.aur_unreachable.clone())))
                .unwrap_or_default();
            untrack(|| untracked_card_body(app, list, note))
        })
    })
}

/// How high the untracked list may grow before it scrolls.
const UNTRACKED_MAX_H: f32 = 150.0_f32;

fn untracked_card_body(app: App, list: Vec<Untracked>, note: Option<String>) -> Div {
    let n = list.len();
    let (tone, outline) = if n > 0 { (RED, RED_DIM) } else { (WARN_AMBER, WARN_AMBER) };
    let headline = if n > 0 {
        format!(
            "{} nothing tracks: no repository has {} build, and no AUR package paru checks",
            plural(n, "installed package"),
            if n == 1 { "its" } else { "their" }
        )
    } else {
        "The AUR could not be asked which packages it has".to_string()
    };
    let mut card = outlined_card_in(outline)
        .w_full()
        .flex_none()
        .gap(6.0_f32)
        .px(10.0_f32)
        .py(8.0_f32)
        .child(
            div()
                .w_full()
                .flex()
                .flex_row()
                .items_center()
                .gap(10.0_f32)
                .child(div().flex_1().min_w(0.0_f32).child(ui_text(headline, FONT_SIZE, tone).bold()))
                .child(push_button(
                    || "Dismiss".to_string(),
                    false,
                    || true,
                    move || app.untracked_dismissed.set(true),
                )),
        );
    if n > 0 {
        card = card.child(faint_note(if n == 1 {
            "Nothing updates it. Worth fixing: remove it, or install a tracked build."
        } else {
            "Nothing updates them. Each one is worth fixing: remove it, or install a tracked build."
        }));
        let mut rows = div().w_full().flex().flex_col().gap(5.0_f32).py(2.0_f32).pr(14.0_f32);
        for t in list {
            rows = rows.child(untracked_row(app, t));
        }
        // The rows are the scroller's direct child (scroll extent).
        card = card.child(div().w_full().max_h(UNTRACKED_MAX_H).overflow_y_scroll().child(rows));
    }
    if let Some(note) = note {
        card = card.child(ui_text(note, FONT_SIZE - 2.0_f32, WARN_AMBER));
    }
    card
}

fn untracked_row(app: App, t: Untracked) -> Div {
    let mut needed = if t.required_by.is_empty() {
        "nothing requires it".to_string()
    } else {
        required_by_text(&t.required_by, 4).replacen("Required", "required", 1)
    };
    if !t.optional_for.is_empty() {
        needed += &format!(" · {}", optional_for_text(&t.optional_for, 4).replacen("Optional", "optional", 1));
    }
    let (name, version) = (t.name.clone(), t.version.clone());
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap(1.0_f32)
        .child(
            div()
                .w_full()
                .flex()
                .flex_row()
                .flex_wrap()
                .items_center()
                .column_gap(8.0_f32)
                .row_gap(1.0_f32)
                .child(name_link(
                    {
                        let shown = t.name.clone();
                        move || shown.clone()
                    },
                    FONT_SIZE - 1.0_f32,
                    move || {
                        app.open_detail(Detail {
                            name: name.clone(),
                            context: Context::Installed,
                            version: version.clone(),
                            repo: String::new(),
                            description: String::new(),
                        })
                    },
                ))
                .child(mono_text(t.version.clone(), FONT_SIZE - 2.0_f32, TEXT_MUTED).whitespace_nowrap())
                .child(chip(if t.explicit { "explicit" } else { "dependency" }, LINE, TEXT))
                .child(ui_text(needed, FONT_SIZE - 2.0_f32, TEXT_FAINT)),
        )
        .child(ui_text(t.hint, FONT_SIZE - 2.0_f32, TEXT_MUTED))
}

fn updates_tab(app: App) -> Div {
    let has_rows = move || {
        app.state
            .with(|s| s.as_ref().is_some_and(|s| !s.updates.is_empty()))
    };
    let has = move |aur: bool| {
        move || {
            app.updates_view()
                .effective
                .iter()
                .any(|u| (u.candidate_repo == "aur") == aur)
        }
    };
    let list = div()
        .flex()
        .flex_col()
        .gap(7.0_f32)
        .child(show(has(false), || section_label("REPOSITORIES")))
        .child(update_list(app, false))
        .child(show(has(true), || {
            div().pt(10.0_f32).child(section_label("AUR"))
        }))
        .child(update_list(app, true))
        .child(show(
            move || !has_rows() && !app.checking.get(),
            move || {
                empty_text(move || {
                    if app
                        .state
                        .with(|s| s.as_ref().is_some_and(|s| s.checked_at > 0))
                    {
                        "Nothing to update."
                    } else {
                        "No check has run yet — hit Recheck."
                    }
                })
            },
        ));

    let footer_note = move || {
        let v = app.updates_view();
        if v.ignored_names.is_empty() && v.explicit_targets.is_empty() && v.origin_targets.is_empty() {
            return "Opens rio running paru -Syu --sudoloop — review and confirm there.".into();
        }
        let mut s = "Opens rio: paru -Syu --sudoloop".to_string();
        if !v.ignored_names.is_empty() {
            // A zero-width space after each comma lets the list wrap; it has
            // no spaces of its own, and one unbroken word overflows the window.
            s += &format!(" --ignore={}", v.ignored_names.join(",\u{200b}"));
        }
        if !v.explicit_targets.is_empty() {
            s += &format!(" {}", v.explicit_targets.join(" "));
        }
        // There can be many: counted here, all of them in the terminal.
        if !v.origin_targets.is_empty() {
            s += &format!(" + {} reinstall target{}", v.origin_targets.len(), if v.origin_targets.len() == 1 { "" } else { "s" });
        }
        s
    };

    div()
        .flex_1()
        .min_h(0.0_f32)
        .w_full()
        .flex()
        .flex_col()
        .gap(10.0_f32)
        .child(untracked_card(app))
        .child(show(has_rows, move || {
            div()
                .w_full()
                .flex()
                .flex_row()
                .items_center()
                .gap(8.0_f32)
                .child(push_button(
                    || "Tick all".into(),
                    false,
                    || true,
                    move || app.tick_all(),
                ))
                .child(push_button(
                    || "Untick all".into(),
                    false,
                    || true,
                    move || app.untick_all(),
                ))
                .child(div().flex_1().min_w(0.0_f32).child(faint_note(move || {
                    let v = app.updates_view();
                    format!(
                        "{} of {} ticked — unticked are left out, for this run only{}",
                        v.selected,
                        v.pending,
                        if v.held > 0 {
                            "; held ones are skipped by pacman.conf"
                        } else {
                            ""
                        }
                    )
                })))
        }))
        .child(scroller(list))
        .child(footer(
            footer_note,
            vec![push_button(
                || "Update…".into(),
                true,
                move || app.updates_view().selected > 0 && !app.checking.get(),
                move || app.apply(),
            )],
        ))
}

// ---- browse / installed ---------------------------------------------------

/// What one fixed-height package row shows.
#[derive(Clone, Default)]
struct PkgLine {
    name: String,
    version: String,
    repo: String,
    description: String,
    marker: String,
    selected: bool,
}

const PKG_ROW_H: f32 = 50.0_f32;

fn package_row(
    index: usize,
    line: Rc<dyn Fn(usize) -> Option<PkgLine>>,
    on_toggle: Rc<dyn Fn(usize)>,
    on_open: Rc<dyn Fn(usize)>,
) -> Div {
    let get = move |f: fn(&PkgLine) -> String| {
        let line = Rc::clone(&line);
        move || line(index).map(|l| f(&l)).unwrap_or_default()
    };
    let selected = {
        let l = get(|l| {
            if l.selected {
                "1".into()
            } else {
                String::new()
            }
        });
        move || !l().is_empty()
    };
    let repo_is_aur = {
        let l = get(|l| l.repo.clone());
        move || l() == "aur"
    };
    let opacity_selected = selected.clone();
    let repo_text = get(|l| l.repo.clone());
    div()
        .h(PKG_ROW_H)
        .w_full()
        .pr(14.0_f32)
        .flex()
        .flex_row()
        .items_start()
        .gap(10.0_f32)
        .overflow_hidden()
        .child(check_box(selected, || true, move || on_toggle(index)))
        .child(
            div()
                .flex_1()
                .min_w(0.0_f32)
                .pt(3.0_f32)
                .flex()
                .flex_col()
                .gap(1.0_f32)
                .overflow_hidden()
                .dyn_style(move |s| s.opacity(if opacity_selected() { 1.0_f32 } else { 0.8_f32 }))
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(8.0_f32)
                        .overflow_hidden()
                        .child(name_link(get(|l| l.name.clone()), FONT_SIZE, move || {
                            on_open(index)
                        }))
                        .child(
                            mono_text(get(|l| l.version.clone()), FONT_SIZE - 2.0_f32, TEXT_MUTED)
                                .whitespace_nowrap(),
                        )
                        .child(
                            mono_text(repo_text, FONT_SIZE - 2.0_f32, TEXT_FAINT)
                                .whitespace_nowrap()
                                .dyn_style(move |s| {
                                    s.text_color(color(if repo_is_aur() {
                                        PURPLE
                                    } else {
                                        TEXT_FAINT
                                    }))
                                }),
                        )
                        .child(div().flex_1())
                        .child(
                            mono_text(get(|l| l.marker.clone()), FONT_SIZE - 2.0_f32, OK_GREEN)
                                .whitespace_nowrap(),
                        ),
                )
                .child(
                    div().w_full().overflow_hidden().child(
                        ui_text(
                            get(|l| l.description.clone()),
                            FONT_SIZE - 2.0_f32,
                            TEXT_MUTED,
                        )
                        .whitespace_nowrap(),
                    ),
                ),
        )
}

fn package_list(
    count: impl Fn() -> usize + 'static,
    line: Rc<dyn Fn(usize) -> Option<PkgLine>>,
    on_toggle: Rc<dyn Fn(usize)>,
    on_open: Rc<dyn Fn(usize)>,
) -> Div {
    div()
        .relative()
        .flex_1()
        .min_h(0.0_f32)
        .w_full()
        .overflow_hidden()
        .child(
            virtual_list(count, PKG_ROW_H, move |i| {
                package_row(
                    i,
                    Rc::clone(&line),
                    Rc::clone(&on_toggle),
                    Rc::clone(&on_open),
                )
            })
            .w_full()
            .h_full(),
        )
}

fn browse_tab(app: App) -> Div {
    let line: Rc<dyn Fn(usize) -> Option<PkgLine>> = Rc::new(move |i| {
        let r = app.results.with(|r| r.get(i).cloned())?;
        let target = format!("{}/{}", r.repo, r.name);
        Some(PkgLine {
            selected: app.install_sel.with(|s| s.contains(&target)),
            marker: r
                .installed_version
                .as_ref()
                .map(|v| format!("installed {v}"))
                .unwrap_or_default(),
            name: r.name,
            version: r.version,
            repo: r.repo,
            description: r.description,
        })
    });
    let on_toggle: Rc<dyn Fn(usize)> = Rc::new(move |i| {
        if let Some(r) = app.results.with_untracked(|r| r.get(i).cloned()) {
            let target = format!("{}/{}", r.repo, r.name);
            app.install_sel
                .update(|s| toggle_install_target(s, &target, &r.name));
        }
    });
    let on_open: Rc<dyn Fn(usize)> = Rc::new(move |i| {
        if let Some(r) = app.results.with_untracked(|r| r.get(i).cloned()) {
            app.open_detail(Detail {
                name: r.name,
                context: Context::Browse,
                version: r.version,
                repo: r.repo,
                description: r.description,
            });
        }
    });

    let input_ref = node_ref();
    request_frame(move |_| input_ref.focus());
    let picked = move || {
        app.install_sel
            .with(|s| s.iter().cloned().collect::<Vec<_>>())
    };

    div()
        .flex_1()
        .min_h(0.0_f32)
        .w_full()
        .flex()
        .flex_col()
        .gap(10.0_f32)
        .child(
            div()
                .w_full()
                .flex()
                .flex_row()
                .items_center()
                .gap(8.0_f32)
                .child(
                    text_field(app.query)
                        .placeholder("Search name or description…")
                        .node_ref(input_ref)
                        .flex_1()
                        .min_w(0.0_f32)
                        .on_submit(move |_| app.search()),
                )
                .child(push_button(
                    move || {
                        if app.searching.get() {
                            "Searching…".into()
                        } else {
                            "Search".into()
                        }
                    },
                    false,
                    move || !app.searching.get() && app.query.with(|q| !q.trim().is_empty()),
                    move || app.search(),
                )),
        )
        .child(show(
            move || !app.searched.get() && !app.searching.get(),
            || empty_text("Tick packages to install them together. Installed versions are marked."),
        ))
        .child(show(
            move || {
                app.searched.get() && !app.searching.get() && app.results.with(|r| r.is_empty())
            },
            || empty_text("No matches."),
        ))
        .child(package_list(
            move || app.results.with(|r| r.len()),
            line,
            on_toggle,
            on_open,
        ))
        .child(footer(
            move || {
                let p = picked();
                if p.is_empty() {
                    "Nothing selected. Repo results reflect the last check's databases.".into()
                } else {
                    format!("{} selected — opens rio: paru -S {}", p.len(), p.join(" "))
                }
            },
            vec![
                push_button(
                    || "Clear".into(),
                    false,
                    move || app.install_sel.with(|s| !s.is_empty()),
                    move || app.install_sel.set(Default::default()),
                ),
                push_button(
                    || "Install…".into(),
                    true,
                    move || app.install_sel.with(|s| !s.is_empty()),
                    move || app.install(picked()),
                ),
            ],
        ))
}

fn installed_tab(app: App) -> Div {
    let line: Rc<dyn Fn(usize) -> Option<PkgLine>> = Rc::new(move |i| {
        let p = app.visible_installed.with(|v| v.get(i).cloned())?;
        Some(PkgLine {
            selected: app.remove_sel.with(|s| s.contains(&p.name)),
            marker: if p.explicit {
                format_size(p.size_bytes)
            } else {
                "dependency".into()
            },
            name: p.name,
            version: p.version,
            repo: p.repo,
            description: p.description,
        })
    });
    let on_toggle: Rc<dyn Fn(usize)> = Rc::new(move |i| {
        if let Some(name) = app
            .visible_installed
            .with_untracked(|v| v.get(i).map(|p| p.name.clone()))
        {
            app.remove_sel.update(|s| {
                if !s.remove(&name) {
                    s.insert(name);
                }
            });
        }
    });
    let on_open: Rc<dyn Fn(usize)> = Rc::new(move |i| {
        if let Some(p) = app.visible_installed.with_untracked(|v| v.get(i).cloned()) {
            app.open_detail(Detail {
                name: p.name,
                context: Context::Installed,
                version: p.version,
                repo: p.repo,
                description: p.description,
            });
        }
    });
    let picked = move || {
        app.remove_sel
            .with(|s| s.iter().cloned().collect::<Vec<_>>())
    };

    div()
        .flex_1()
        .min_h(0.0_f32)
        .w_full()
        .flex()
        .flex_col()
        .gap(10.0_f32)
        .child(
            div()
                .w_full()
                .flex()
                .flex_row()
                .items_center()
                .gap(8.0_f32)
                .child(
                    text_field(app.filter)
                        .placeholder("Filter installed packages…")
                        .flex_1()
                        .min_w(0.0_f32),
                )
                .child(filter_toggle("Explicit only", app.explicit_only))
                .child(push_button(
                    move || {
                        if app.installed_loading.get() {
                            "Loading…".into()
                        } else {
                            "Reload".into()
                        }
                    },
                    false,
                    move || !app.installed_loading.get(),
                    move || app.load_installed(),
                )),
        )
        .child(show(
            move || {
                app.installed_loaded.get()
                    && !app.installed_loading.get()
                    && app.visible_installed.with(|v| v.is_empty())
            },
            || empty_text("No installed packages match."),
        ))
        .child(package_list(
            move || app.visible_installed.with(|v| v.len()),
            line,
            on_toggle,
            on_open,
        ))
        .child(footer(
            move || {
                let p = picked();
                if p.is_empty() {
                    "Tick packages to remove. Dependencies can be filtered out with Explicit only."
                        .into()
                } else {
                    format!(
                        "{} selected — opens rio: paru -Rns {} (confirm there; Reload after)",
                        p.len(),
                        p.join(" ")
                    )
                }
            },
            vec![
                push_button(
                    || "Clear".into(),
                    false,
                    move || app.remove_sel.with(|s| !s.is_empty()),
                    move || app.remove_sel.set(Default::default()),
                ),
                push_button(
                    || "Remove…".into(),
                    true,
                    move || app.remove_sel.with(|s| !s.is_empty()),
                    move || app.remove(picked()),
                ),
            ],
        ))
}

// ---- sources --------------------------------------------------------------

fn status_hex(status: &str) -> u32 {
    match status {
        "update" => PURPLE,
        "update-by-hand" | "watch" => WARN_AMBER,
        "publish" => OK_GREEN,
        "older" | "error" => RED,
        _ => TEXT_FAINT,
    }
}

fn source_row(app: App, r: SourceRow) -> Div {
    let actionable = source_actionable(&r);
    let pkg: Rc<str> = Rc::from(r.package.as_str());
    let checked = {
        let p = Rc::clone(&pkg);
        move || app.source_sel.with(|s| s.contains(&*p))
    };
    let tracked = r.tracked;

    let mut line1 = div()
        .w_full()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .column_gap(8.0_f32)
        .row_gap(1.0_f32)
        .child(ui_text(r.package.clone(), FONT_SIZE, TEXT).whitespace_nowrap());
    if !r.kind.is_empty() {
        line1 = line1
            .child(mono_text(r.kind.clone(), FONT_SIZE - 2.0_f32, TEXT_FAINT).whitespace_nowrap());
    }
    if r.auto {
        line1 = line1.child(chip("auto", PURPLE_DIM, TEXT));
    }

    // Versions: the PKGBUILD's, where upstream is, and the repo's when it
    // differs from the PKGBUILD (built but not published, or the reverse).
    let mut versions = div()
        .w_full()
        .flex()
        .flex_row()
        .flex_wrap()
        .column_gap(6.0_f32)
        .row_gap(1.0_f32);
    let moving = matches!(r.status.as_str(), "update" | "update-by-hand" | "watch");
    let version_line = if moving && !r.upstream.is_empty() {
        // A new commit on the same version is a revision (pkgrel + 1), as
        // Arch and CachyOS version a rebuild; the commit is only a note.
        match next_revision(&r) {
            Some(next) => format!(
                "{} → {next} · new commit {}",
                r.pkgbuild,
                &r.commit[..r.commit.len().min(7)]
            ),
            None => format!("{} → {}", r.pkgbuild, r.upstream),
        }
    } else {
        r.pkgbuild.clone()
    };
    versions = versions
        .child(mono_text(version_line, FONT_SIZE - 2.0_f32, TEXT_MUTED).whitespace_nowrap());
    if r.published.is_empty() {
        versions = versions.child(
            mono_text("not in [minka]", FONT_SIZE - 2.0_f32, TEXT_FAINT).whitespace_nowrap(),
        );
    } else if r.published != r.pkgbuild {
        versions = versions.child(
            mono_text(
                format!("[minka] has {}", r.published),
                FONT_SIZE - 2.0_f32,
                TEXT_FAINT,
            )
            .whitespace_nowrap(),
        );
    }
    versions = versions.child(ui_text(
        source_status_text(&r),
        FONT_SIZE - 2.0_f32,
        status_hex(&r.status),
    ));

    let mut column = div()
        .flex_1()
        .min_w(0.0_f32)
        .flex()
        .flex_col()
        .gap(1.0_f32)
        .opacity(if actionable || r.status == "error" {
            1.0_f32
        } else {
            0.6_f32
        })
        .child(line1)
        .child(versions);
    // The note says what to re-check when bumping: shown where a bump is due.
    if !r.note.is_empty() && (moving || r.status == "publish") {
        column = column.child(faint_note(format!("re-check: {}", r.note)));
    }

    let action = {
        let p = Rc::clone(&pkg);
        move || {
            let busy = app.source_busy.with(|b| b.contains(&*p));
            match (busy, tracked) {
                (true, _) => "…".to_string(),
                (false, true) => "don't track".to_string(),
                (false, false) => "track".to_string(),
            }
        }
    };
    let toggle_pkg = Rc::clone(&pkg);
    div()
        .w_full()
        .flex()
        .flex_row()
        .items_start()
        .gap(10.0_f32)
        .child(check_box(
            checked,
            move || actionable,
            move || app.toggle_source(&toggle_pkg),
        ))
        .child(column)
        .child(if r.kind.is_empty() {
            // No upstream file: nothing for `minkarepo track` to switch.
            div().flex_none().child(
                ui_text("no upstream file", FONT_SIZE - 2.0_f32, TEXT_FAINT).whitespace_nowrap(),
            )
        } else {
            text_button(action, TEXT_MUTED, move || {
                app.set_tracked(pkg.to_string(), !tracked)
            })
        })
}

fn source_list(app: App, tracked: bool) -> impl IntoView {
    for_each(
        move || {
            let generation = app.sources_gen.get();
            app.sources.with(|s| {
                s.as_ref()
                    .map(|s| {
                        s.rows
                            .iter()
                            .filter(|r| r.tracked == tracked)
                            .map(|r| (generation, r.clone()))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            })
        },
        |(generation, r)| (*generation, r.package.clone()),
        move |(_, r)| untrack(|| source_row(app, r)),
    )
}

fn sources_tab(app: App) -> Div {
    let untracked_count = move || {
        app.sources.with(|s| {
            s.as_ref()
                .map_or(0, |s| s.rows.iter().filter(|r| !r.tracked).count())
        })
    };
    let picked = move || {
        app.source_sel
            .with(|s| s.iter().cloned().collect::<Vec<_>>())
    };
    let actionable_count = move || {
        app.sources.with(|s| {
            s.as_ref().map_or(0, |s| {
                s.rows.iter().filter(|r| source_actionable(r)).count()
            })
        })
    };

    let list = div()
        .flex()
        .flex_col()
        .gap(9.0_f32)
        .child(source_list(app, true))
        .child(show(
            move || untracked_count() > 0,
            move || {
                div().pt(10.0_f32).child(text_button(
                    move || {
                        format!(
                            "{} NOT TRACKED ({})",
                            if app.show_untracked.get() {
                                "▾"
                            } else {
                                "▸"
                            },
                            untracked_count()
                        )
                    },
                    TEXT_FAINT,
                    move || app.show_untracked.update(|s| *s = !*s),
                ))
            },
        ))
        .child(show(
            move || app.show_untracked.get(),
            move || {
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap(9.0_f32)
                    .child(source_list(app, false))
            },
        ))
        .child(show(
            move || {
                app.sources
                    .with(|s| s.as_ref().is_none_or(|s| s.rows.is_empty()))
            },
            move || {
                empty_text(move || {
                    if app.sources_loading.get() {
                        "Checking upstream for every package…"
                    } else {
                        "No packages with an upstream file."
                    }
                })
            },
        ));

    div()
        .flex_1()
        .min_h(0.0_f32)
        .w_full()
        .flex()
        .flex_col()
        .gap(10.0_f32)
        .child(
            div()
                .w_full()
                .flex()
                .flex_row()
                .items_center()
                .gap(8.0_f32)
                .child(push_button(
                    || "Tick all with work".into(),
                    false,
                    move || actionable_count() > 0,
                    move || {
                        let all = app.sources.with_untracked(|s| {
                            s.as_ref()
                                .map(|s| {
                                    s.rows
                                        .iter()
                                        .filter(|r| source_actionable(r))
                                        .map(|r| r.package.clone())
                                        .collect()
                                })
                                .unwrap_or_default()
                        });
                        app.source_sel.set(all);
                    },
                ))
                .child(push_button(
                    || "Untick all".into(),
                    false,
                    move || app.source_sel.with(|s| !s.is_empty()),
                    move || app.source_sel.set(Default::default()),
                ))
                .child(div().flex_1().min_w(0.0_f32).child(faint_note(move || {
                    format!(
                        "{} of {} ticked — \"don't track\" stops checking a package at all",
                        picked().len(),
                        actionable_count()
                    )
                }))),
        )
        .child(scroller(list))
        .child(footer(
            move || {
                let p = picked();
                if p.is_empty() {
                    "Tick what to bump. Builds run in rio and may ask for sudo (makepkg -s).".into()
                } else {
                    format!("Opens rio: minkarepo update {}", p.join(" "))
                }
            },
            vec![push_button(
                || "Update & publish…".into(),
                true,
                move || app.source_sel.with(|s| !s.is_empty()),
                move || app.update_sources(),
            )],
        ))
}

// ---- detail overlay -------------------------------------------------------

fn detail_field(label: &'static str, value: impl Fn() -> String + 'static) -> impl IntoView {
    let value = Rc::new(value);
    let shown = Rc::clone(&value);
    show(
        move || !shown().is_empty(),
        move || {
            let value = Rc::clone(&value);
            ui_text(
                move || format!("{label}:  {}", value()),
                FONT_SIZE - 1.0_f32,
                TEXT_MUTED,
            )
        },
    )
}

fn repo_option_row(app: App, o: RepoOption) -> Div {
    let target: Rc<str> = Rc::from(o.target.as_str());
    let chosen = {
        let t = Rc::clone(&target);
        move || app.detail_target.with(|cur| cur.as_str() == &*t)
    };
    let chosen_style = chosen.clone();
    let (pick_row, pick_box) = (Rc::clone(&target), Rc::clone(&target));
    let mut row = div()
        .w_full()
        .h(36.0_f32)
        .pl(6.0_f32)
        .pr(9.0_f32)
        .flex()
        .flex_row()
        .items_center()
        .gap(8.0_f32)
        .rounded(5.0_f32)
        .border(1.0_f32)
        .cursor(CursorIcon::Pointer)
        .dyn_style(move |s| {
            if chosen_style() {
                s.bg(color(SURFACE_RAISED)).border_color(color(PURPLE))
            } else {
                s.bg(color(SURFACE)).border_color(color(LINE))
            }
        })
        .on_click(move |_| app.detail_target.set(pick_row.to_string()))
        .child(check_box(
            chosen,
            || true,
            move || app.detail_target.set(pick_box.to_string()),
        ))
        .child(
            mono_text(
                o.repo.clone(),
                FONT_SIZE - 1.0_f32,
                if o.repo == "aur" { PURPLE } else { TEXT },
            )
            .whitespace_nowrap(),
        )
        .child(div().flex_1().min_w(0.0_f32).overflow_hidden().child(
            mono_text(o.version.clone(), FONT_SIZE - 2.0_f32, TEXT_MUTED).whitespace_nowrap(),
        ));
    if !o.architecture.is_empty() {
        row = row.child(
            mono_text(o.architecture.clone(), FONT_SIZE - 2.0_f32, TEXT_FAINT).whitespace_nowrap(),
        );
    }
    if o.is_default {
        row = row
            .child(ui_text("pacman default", FONT_SIZE - 2.0_f32, TEXT_FAINT).whitespace_nowrap());
    }
    if o.is_newest {
        row = row.child(ui_text("newest", FONT_SIZE - 2.0_f32, PURPLE).whitespace_nowrap());
    }
    if o.is_installed {
        row = row.child(ui_text("installed", FONT_SIZE - 2.0_f32, OK_GREEN).whitespace_nowrap());
    }
    row
}

/// An installed package's install reason, which a press changes (in a
/// terminal: `sudo pacman -D`). The buttons show what the local DB says,
/// read again once that step reports back; nothing flips before. How the
/// last change of it ended, when it failed, shows here too: its card is
/// under this view.
fn reason_section(app: App) -> Div {
    let local = move || app.local.with(|l| l.data.clone()).unwrap_or_default();
    // A change of this package's reason in flight.
    let busy = move || {
        let name = local().name;
        app.reason_busy.with(|b| b.as_deref() == Some(name.as_str()))
    };
    // Another package's: a press here waits for it (set_reason).
    let other = move || {
        let name = local().name;
        app.reason_busy.with(|b| b.as_deref().filter(|b| *b != name).map(str::to_string))
    };
    let failed = move || {
        let name = local().name;
        app.run_reports.with(|cards| reason_failure(cards, &name))
    };
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap(6.0_f32)
        .child(section_label("INSTALL REASON"))
        .child(
            div()
                .w_full()
                .flex()
                .flex_row()
                .items_center()
                .gap(12.0_f32)
                .child(segmented(
                    vec!["Dependency", "Explicit"],
                    move |i| local().explicit == (i == 1),
                    move |i| app.set_reason(i == 1),
                ))
                .child(
                    div()
                        .flex_1()
                        .min_w(0.0_f32)
                        .flex()
                        .flex_col()
                        .gap(1.0_f32)
                        .child(ui_text(
                            move || {
                                let p = local();
                                let reason = if p.explicit { "Explicitly installed" } else { "Installed as a dependency" };
                                // Nothing requiring it is the consequence line's to say.
                                let mut s = reason.to_string();
                                if !p.required_by.is_empty() {
                                    s += &format!(" · {}", required_by_text(&p.required_by, 6));
                                    if !p.optional_for.is_empty() {
                                        s += &format!(" · {}", optional_for_text(&p.optional_for, 4));
                                    }
                                }
                                s
                            },
                            FONT_SIZE - 1.0_f32,
                            TEXT_MUTED,
                        ))
                        .child(show(
                            move || !busy() && reason_consequence(&local()).is_some(),
                            move || {
                                ui_text(
                                    move || reason_consequence(&local()).unwrap_or_default(),
                                    FONT_SIZE - 2.0_f32,
                                    WARN_AMBER,
                                )
                            },
                        ))
                        .child(show(
                            move || !busy() && failed().is_some(),
                            move || {
                                ui_text(
                                    move || format!("{} · its card is under this view", failed().unwrap_or_default()),
                                    FONT_SIZE - 2.0_f32,
                                    RED,
                                )
                            },
                        ))
                        .child(show(busy, move || {
                            mono_text(
                                move || {
                                    let p = local();
                                    format!(
                                        "Waiting for the terminal: sudo pacman -D {} {}",
                                        if p.explicit { "--asdeps" } else { "--asexplicit" },
                                        p.name
                                    )
                                },
                                FONT_SIZE - 2.0_f32,
                                TEXT_FAINT,
                            )
                        }))
                        .child(show(
                            move || other().is_some(),
                            move || {
                                faint_note(move || {
                                    format!(
                                        "Waiting for another install reason change ({}) to finish in its terminal",
                                        other().unwrap_or_default()
                                    )
                                })
                            },
                        )),
                ),
        )
}

fn detail_overlay(app: App) -> Div {
    let d = move || {
        app.detail.get().unwrap_or(Detail {
            name: String::new(),
            context: Context::Updates,
            version: String::new(),
            repo: String::new(),
            description: String::new(),
        })
    };
    let a = move || app.appstream.with(|a| a.data.clone());

    let header = div()
        .w_full()
        .flex()
        .flex_row()
        .items_start()
        .gap(12.0_f32)
        .child(dyn_view(move || {
            match a().and_then(|a| icon_path(&a.icon_url)) {
                Some(path) => div().flex_none().w(56.0_f32).h(56.0_f32).child(
                    img(path)
                        .w(56.0_f32)
                        .h(56.0_f32)
                        .object_fit(ObjectFit::Contain),
                ),
                None => div(),
            }
        }))
        .child(
            div()
                .flex_1()
                .min_w(0.0_f32)
                .flex()
                .flex_col()
                .gap(1.0_f32)
                .child(
                    ui_text(
                        move || {
                            a().map(|a| a.name)
                                .filter(|n| !n.is_empty())
                                .unwrap_or_else(|| d().name)
                        },
                        FONT_SIZE + 3.0_f32,
                        TEXT,
                    )
                    .bold(),
                )
                .child(ui_text(
                    move || a().map(|a| a.summary).unwrap_or_default(),
                    FONT_SIZE - 1.0_f32,
                    TEXT_MUTED,
                ))
                .child(mono_text(
                    move || {
                        let d = d();
                        let mut s = d.name.clone();
                        if !d.version.is_empty() {
                            s += &format!("  {}", d.version);
                        }
                        if !d.repo.is_empty() {
                            s += &format!("  [{}]", d.repo);
                        }
                        if let Some(id) = a().map(|a| a.id).filter(|i| !i.is_empty()) {
                            s += &format!("  ·  {id}");
                        }
                        s
                    },
                    FONT_SIZE - 2.0_f32,
                    TEXT_FAINT,
                )),
        )
        .child(push_button(
            || "Close".into(),
            false,
            || true,
            move || app.close_detail(),
        ));

    let repos_loading = move || app.repos.with(|r| r.loading);
    let body = div()
        .flex()
        .flex_col()
        .gap(10.0_f32)
        .child(dyn_view(move || match app.shot.get() {
            Some(path) => div().w_full().h(240.0_f32).child(
                img(path)
                    .w_full()
                    .h(240.0_f32)
                    .object_fit(ObjectFit::Contain),
            ),
            None => div(),
        }))
        .child(show(
            move || app.local.with(|l| l.data.as_ref().is_some_and(|p| p.installed)),
            move || untrack(|| reason_section(app)),
        ))
        .child(show(
            move || repos_loading() || app.repos.with(|r| !r.data.is_empty()),
            || section_label("AVAILABLE REPOSITORIES"),
        ))
        .child(show(repos_loading, || {
            ui_text(
                "Loading repository versions…",
                FONT_SIZE - 1.0_f32,
                TEXT_MUTED,
            )
        }))
        .child(show(
            move || app.repos.with(|r| r.loaded && r.data.is_empty()),
            || {
                ui_text(
                    "No repository builds found for this package.",
                    FONT_SIZE - 1.0_f32,
                    TEXT_FAINT,
                )
            },
        ))
        .child(for_each(
            move || app.repos.with(|r| r.data.clone()),
            |o| o.target.clone(),
            move |o| untrack(|| repo_option_row(app, o)),
        ))
        .child(show(
            move || app.repos.with(|r| aur_page(&r.data).is_some()),
            move || {
                let url = move || app.repos.with(|r| aur_page(&r.data)).unwrap_or_default();
                text_button(url, PURPLE, move || open_url(&url()))
            },
        ))
        .child(show(
            move || app.appstream.with(|a| a.loading),
            || ui_text("Loading AppStream data…", FONT_SIZE - 1.0_f32, TEXT_MUTED),
        ))
        .child(show(
            move || app.appstream.with(|a| a.loaded && a.data.is_none()),
            || {
                ui_text(
                    "No AppStream data for this package — showing pacman data only.",
                    FONT_SIZE - 1.0_f32,
                    TEXT_FAINT,
                )
            },
        ))
        .child(ui_text(
            move || match a() {
                Some(a) if !a.description.is_empty() => a.description,
                Some(_) => String::new(),
                None if app.appstream.with(|a| a.loaded) => d().description,
                None => String::new(),
            },
            FONT_SIZE - 1.0_f32,
            TEXT,
        ))
        .child(detail_field("Developer", move || {
            a().map(|a| a.developer).unwrap_or_default()
        }))
        .child(detail_field("Categories", move || {
            a().map(|a| a.categories.join(" · ")).unwrap_or_default()
        }))
        .child(detail_field("Licence", move || {
            a().map(|a| a.license).unwrap_or_default()
        }))
        .child(show(
            move || a().is_some_and(|a| !a.homepage.is_empty()),
            move || {
                text_button(
                    move || a().map(|a| a.homepage).unwrap_or_default(),
                    PURPLE,
                    move || {
                        if let Some(url) = a().map(|a| a.homepage) {
                            open_url(&url);
                        }
                    },
                )
            },
        ));

    let blocked = move || app.blocked_reason();
    let action_label = move || {
        let ctx = app.detail.with(|d| d.as_ref().map(|d| d.context));
        match ctx {
            Some(Context::Updates) => "Use for upgrade".to_string(),
            Some(Context::Browse) => "Select for install".to_string(),
            _ => {
                if app.selected_option().is_some_and(|o| o.is_installed) {
                    "Reinstall selected".to_string()
                } else {
                    "Install selected".to_string()
                }
            }
        }
    };
    let footer_row = div()
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(10.0_f32)
        .child(
            div().flex_1().min_w(0.0_f32).child(
                mono_text(
                    move || match blocked() {
                        Some(reason) => reason,
                        None => {
                            let t = app.detail_target.get();
                            if t.is_empty() {
                                "No repository build selected.".into()
                            } else {
                                format!("Selected build: {t}")
                            }
                        }
                    },
                    FONT_SIZE - 2.0_f32,
                    TEXT_FAINT,
                )
                .dyn_style(move |s| {
                    s.text_color(color(if blocked().is_some() {
                        WARN_AMBER
                    } else {
                        TEXT_FAINT
                    }))
                }),
            ),
        )
        .child(push_button(
            action_label,
            false,
            move || app.selected_option().is_some() && app.blocked_reason().is_none(),
            move || app.choose_detail_repo(),
        ));

    // The scrim closes on a click outside the panel; the panel eats its own.
    div()
        .absolute()
        .inset(0.0_f32)
        .z_index(10)
        .p(24.0_f32)
        .flex()
        .items_center()
        .justify_center()
        .bg(color_alpha(GROUND, 0.88_f32))
        .on_click(move |_| app.close_detail())
        .child(
            div()
                .w_full()
                .max_w(680.0_f32)
                .h_full()
                .max_h(560.0_f32)
                .p(16.0_f32)
                .flex()
                .flex_col()
                .gap(10.0_f32)
                .rounded(8.0_f32)
                .bg(color(SURFACE))
                .border(1.0_f32)
                .border_color(color(LINE))
                .on_click(|e: &PointerEvent| e.stop_propagation())
                .child(header)
                .child(scroller(body))
                .child(footer_row),
        )
}
