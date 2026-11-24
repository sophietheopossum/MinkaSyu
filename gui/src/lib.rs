//! minka-syu-gui — MinkaSyu's window on AntiWebUI: updates, browse,
//! installed packages, and the MinkaPKG sources. A view over the core
//! (`minka-syu serve`); `minka-syu ui` starts it.

mod app;
mod core;
mod instance;
mod model;
mod theme;
mod views;

use anti_webui::prelude::*;

/// `--tab updates|browse|installed|sources` opens on that tab;
/// `--detail <package>` also opens a package's detail view.
fn start_options() -> (Option<app::Tab>, Option<String>) {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let tab = value("--tab").and_then(|t| tab_named(&t));
    (tab, value("--detail"))
}

fn tab_named(name: &str) -> Option<app::Tab> {
    match name {
        "updates" => Some(app::Tab::Updates),
        "browse" => Some(app::Tab::Browse),
        "installed" => Some(app::Tab::Installed),
        "sources" => Some(app::Tab::Sources),
        _ => None,
    }
}

/// `--shot <tab> <file.png>` writes that tab to a picture and exits: no
/// window, no core, only the core's last saved answers (its state
/// directory). For checking the look of the window. With `--busy` after
/// the file, the tab is drawn as it looks while the core is working; with
/// `--run <result.json>` (once per card), with the card of that step's
/// result; with `--detail <package> <local.json>`, with that package's
/// detail view open, its install reason from the file (what
/// `minka-syu reason --json <package>` prints); with `--names`, with the
/// origin group's names shown; with `--leave-out <name>` (once per name),
/// with the origin group ticked and those members left out; with
/// `--height <px>`, that many pixels high instead of 875 (the list below
/// the fold).
fn shot() -> Option<i32> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let at = args.iter().position(|a| a == "--shot")?;
    let (Some(tab), Some(file)) = (args.get(at + 1).and_then(|t| tab_named(t)), args.get(at + 2))
    else {
        minka_note::note!("usage: minka-syu-gui --shot updates|browse|installed|sources <file.png>");
        return Some(2);
    };
    let busy = args.iter().any(|a| a == "--busy");
    let names = args.iter().any(|a| a == "--names");
    let height = args
        .iter()
        .position(|a| a == "--height")
        .and_then(|i| args.get(i + 1)?.parse::<u32>().ok())
        .unwrap_or(875);
    let runs: Vec<std::path::PathBuf> = args
        .iter()
        .enumerate()
        .filter(|(_, a)| *a == "--run")
        .filter_map(|(i, _)| args.get(i + 1))
        .map(std::path::PathBuf::from)
        .collect();
    let left_out: Vec<String> = args
        .iter()
        .enumerate()
        .filter(|(_, a)| *a == "--leave-out")
        .filter_map(|(i, _)| args.get(i + 1).cloned())
        .collect();
    let detail = args
        .iter()
        .position(|a| a == "--detail")
        .and_then(|i| Some((args.get(i + 1)?.clone(), std::path::PathBuf::from(args.get(i + 2)?))));
    let dir = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(".local/state")))
        .unwrap_or_default()
        .join("minka-syu");
    let drawn = anti_webui::app::headless::render_png(file, 1100, height, 1.25, move || {
        theme::install();
        let app = app::App::new();
        app.load_saved(&dir, tab);
        for file in &runs {
            app.load_run(file);
        }
        app.origin_open.set(names);
        if !left_out.is_empty() {
            app.set_group(true);
            for name in &left_out {
                app.toggle_member(name);
            }
        }
        if let Some((name, local)) = &detail {
            app.load_detail(name, local);
        }
        if busy {
            app.checking.set(true);
            app.searching.set(true);
            app.installed_loading.set(true);
            app.sources_loading.set(true);
        }
        views::root(app)
    });
    match drawn {
        Ok(()) => Some(0),
        Err(e) => {
            minka_note::note!("minka-syu-gui: {e}");
            Some(1)
        }
    }
}

pub fn main() {
    if let Some(code) = shot() {
        std::process::exit(code);
    }
    if let instance::Claim::Duplicate = instance::claim() {
        // The running window raises itself on our probe.
        return;
    }
    let result = App::new()
        .title("MinkaSyu")
        .app_id(instance::APP_ID)
        .size(880, 700)
        .run(|| {
            theme::install();
            let app = app::App::new();
            let (tab, detail) = start_options();
            app.start(tab, detail);
            views::root(app)
        });
    if let Err(e) = result {
        minka_note::note!("minka-syu-gui: {e}");
        std::process::exit(1);
    }
}
