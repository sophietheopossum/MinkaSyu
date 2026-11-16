//! minka-syu-gui — MinkaSyu's window on AntiWebUI: updates, browse,
//! installed packages, and the MinkaPKG sources. A view over the core
//! (`minka-syu serve`); `minka-syu ui` starts it, and falls back to the
//! QML window (ui/) when this binary is missing or
//! MINKA_SYU_FRONTEND=quickshell.

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
    let tab = value("--tab").and_then(|t| match t.as_str() {
        "updates" => Some(app::Tab::Updates),
        "browse" => Some(app::Tab::Browse),
        "installed" => Some(app::Tab::Installed),
        "sources" => Some(app::Tab::Sources),
        _ => None,
    });
    (tab, value("--detail"))
}

fn main() {
    if let instance::Claim::Duplicate = instance::claim() {
        // The running window raises itself on our probe.
        return;
    }
    let result = App::new()
        .title("MinkaSyu")
        .app_id(instance::APP_ID)
        .size(880, 700)
        .run(|| {
            theme::install_widget_theme();
            let app = app::App::new();
            let (tab, detail) = start_options();
            app.start(tab, detail);
            views::root(app)
        });
    if let Err(e) = result {
        eprintln!("minka-syu-gui: {e}");
        std::process::exit(1);
    }
}
