//! AppStream details for a package, via appstreamcli.
//!
//! Rather than parse the swcatalog XML (and grow XML+gzip deps), the
//! core asks appstreamcli, which already pools the Arch catalogs
//! (archlinux-appstream-data), local metainfo, and Flathub. One
//! `search --details` per click (~0.1s against its cache), filtered
//! to the component whose Package: exactly matches -- search itself
//! is fuzzy and happily returns addons and Flatpak twins first.
//!
//! Coverage is what the catalogs have: GUI apps in Arch repos and
//! installed apps with metainfo. Libraries and most AUR packages
//! simply have none, and the UI falls back to pacman data.

use serde::Serialize;
use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AppstreamInfo {
    pub id: String,
    pub name: String,
    pub summary: String,
    pub description: String,
    pub homepage: String,
    pub developer: String,
    pub license: String,
    pub categories: Vec<String>,
    /// file:// URL of a resolved local icon, or "" when none found.
    pub icon_url: String,
    pub screenshots: Vec<String>,
}

/// One output block: ordered (key, value-lines) pairs. appstreamcli
/// wraps long values onto indented continuation lines and puts lists
/// (categories) on "- item" lines, so a field's value is a line list.
type Block = Vec<(String, Vec<String>)>;

fn parse_blocks(text: &str) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut current: Block = Vec::new();
    for line in text.lines() {
        if line.trim() == "---" {
            if !current.is_empty() {
                blocks.push(std::mem::take(&mut current));
            }
            continue;
        }
        if line.starts_with(|c: char| c.is_whitespace()) {
            if let Some((_, lines)) = current.last_mut() {
                lines.push(line.trim().to_string());
            }
            continue;
        }
        if let Some((key, value)) = line.split_once(':') {
            let value = value.trim();
            let mut lines = Vec::new();
            if !value.is_empty() {
                lines.push(value.to_string());
            }
            current.push((key.trim().to_string(), lines));
        }
    }
    if !current.is_empty() {
        blocks.push(current);
    }
    blocks
}

fn field<'a>(block: &'a Block, key: &str) -> Option<&'a [String]> {
    block.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_slice())
}

/// Single-line fields, unwrapped: continuation lines joined by spaces.
fn joined(block: &Block, key: &str) -> String {
    field(block, key)
        .map(|lines| {
            lines
                .iter()
                .filter(|l| !l.is_empty())
                .cloned()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

/// Description keeps paragraph breaks: wrapped lines join with a
/// space, blank lines become paragraph separators.
fn description(block: &Block) -> String {
    let Some(lines) = field(block, "Description") else {
        return String::new();
    };
    let mut out = String::new();
    for line in lines {
        if line.is_empty() {
            if !out.is_empty() && !out.ends_with("\n\n") {
                out.push_str("\n\n");
            }
        } else {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push(' ');
            }
            out.push_str(line);
        }
    }
    out.trim().to_string()
}

fn bullet_list(block: &Block, key: &str) -> Vec<String> {
    field(block, key)
        .map(|lines| {
            lines
                .iter()
                .filter_map(|l| l.strip_prefix("- ").map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Find the block for exactly this package and shape it. Pure over
/// appstreamcli's stdout apart from `resolve_icon`, passed in so the
/// parser tests stay hermetic.
pub fn select_component(
    text: &str,
    package: &str,
    resolve_icon: &dyn Fn(&str) -> String,
) -> Option<AppstreamInfo> {
    for block in parse_blocks(text) {
        if joined(&block, "Package") != package {
            continue;
        }
        let id = joined(&block, "Identifier")
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string();
        let mut screenshots = Vec::new();
        let default_shot = joined(&block, "Default Screenshot URL");
        if default_shot.starts_with("http") {
            screenshots.push(default_shot);
        }
        for item in bullet_list(&block, "Screenshots") {
            if item.starts_with("http") && !screenshots.contains(&item) {
                screenshots.push(item);
            }
        }
        let license = {
            let l = joined(&block, "Licence");
            if l.is_empty() { joined(&block, "License") } else { l }
        };
        return Some(AppstreamInfo {
            id,
            name: joined(&block, "Name"),
            summary: joined(&block, "Summary"),
            description: description(&block),
            homepage: joined(&block, "Homepage"),
            developer: joined(&block, "Developer"),
            license,
            categories: bullet_list(&block, "Categories"),
            icon_url: resolve_icon(&joined(&block, "Icon")),
            screenshots,
        });
    }
    None
}

/// Resolve an Icon value to a local file URL: a catalog filename
/// (`ardour_ardour9.jxl`) under the swcatalog iconsets, or a stock
/// icon name in the hicolor theme. "" when nothing usable exists --
/// QML then just shows no icon.
fn resolve_icon_path(icon: &str) -> String {
    if icon.is_empty() {
        return String::new();
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if icon.contains('.') {
        if let Ok(sets) = std::fs::read_dir("/usr/share/swcatalog/icons") {
            for set in sets.flatten() {
                for size in ["128x128", "64x64", "48x48"] {
                    candidates.push(set.path().join(size).join(icon));
                }
            }
        }
        if let Ok(sizes) = std::fs::read_dir("/var/lib/flatpak/appstream/flathub/x86_64/active/icons") {
            for size in sizes.flatten() {
                candidates.push(size.path().join(icon));
            }
        }
    } else {
        for ext in ["png", "svg"] {
            candidates.push(PathBuf::from(format!(
                "/usr/share/icons/hicolor/128x128/apps/{icon}.{ext}"
            )));
            candidates.push(PathBuf::from(format!(
                "/usr/share/icons/hicolor/64x64/apps/{icon}.{ext}"
            )));
            candidates.push(PathBuf::from(format!("/usr/share/pixmaps/{icon}.{ext}")));
        }
    }
    for path in candidates {
        if path.exists() {
            return format!("file://{}", path.display().to_string().replace(' ', "%20"));
        }
    }
    String::new()
}

/// AppStream info for one package name, or None when no catalog
/// component claims it. appstreamcli missing also yields None: this
/// is garnish, never worth failing the window over.
pub fn run_appstream(package: &str) -> Option<AppstreamInfo> {
    let output = Command::new("appstreamcli")
        .args(["search", "--details", "--", package])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    select_component(&text, package, &resolve_icon_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
Identifier: org.gimp.GIMP.Plugin.BIMP [addon]\nName: BIMP\nBundle: flatpak:runtime/org.gimp.GIMP.Plugin.BIMP/x86_64/2-3.36\n---\nIdentifier: ardour9.desktop [desktop-application]\nName: Ardour\nSummary: Digital Audio Workstation\nPackage: ardour\nHomepage: https://ardour.org\nIcon: ardour_ardour9.jxl\nDeveloper: Ardour\n  Developers\nDescription: \n  Ardour is a multi-channel digital audio workstation, allowing you to record, edit, mix and master\n  audio and MIDI projects.\n  \n  Targeted at audio engineers.\nDefault Screenshot URL: https://example.org/shot.png\nLicence: GPL-2.0+\nCategories: \n  - AudioVideo\n  - Audio\n---\nIdentifier: org.ardour.Ardour [desktop-application]\nName: Ardour\nBundle: flatpak:app/org.ardour.Ardour/x86_64/stable\n";

    #[test]
    fn selects_exact_package_block() {
        let info = select_component(SAMPLE, "ardour", &|icon| format!("ICON:{icon}"))
            .expect("ardour block");
        assert_eq!(info.id, "ardour9.desktop");
        assert_eq!(info.name, "Ardour");
        assert_eq!(info.developer, "Ardour Developers");
        assert_eq!(info.license, "GPL-2.0+");
        assert_eq!(info.categories, ["AudioVideo", "Audio"]);
        assert_eq!(info.screenshots, ["https://example.org/shot.png"]);
        assert_eq!(info.icon_url, "ICON:ardour_ardour9.jxl");
        assert!(info.description.starts_with("Ardour is a multi-channel"));
        assert!(info.description.contains("\n\nTargeted at audio engineers."));
        assert!(!info.description.contains("master\n  audio"));
    }

    #[test]
    fn no_exact_package_no_result() {
        assert!(select_component(SAMPLE, "gimp", &|_| String::new()).is_none());
        assert!(select_component("No component matching 'x' found.\n", "x", &|_| String::new()).is_none());
        assert!(select_component("", "x", &|_| String::new()).is_none());
    }
}
