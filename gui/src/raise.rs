//! One window per session, raised when a second launch (the bar's updates
//! pill clicked again) is turned away by `minka-instance`. The window raises
//! itself through the ShojiWM IPC (`windows.activate`): the toolkit has no
//! self-activation.

use minka_instance::{Event, Instance};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

pub const APP_ID: &str = "minka-syu";

fn runtime_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Raises the window for each launch turned away, on a thread of its own:
/// the IPC round trip can take a while. Unguarded, there are none.
pub fn on_turned_away(instance: Instance) {
    let _ = std::thread::Builder::new()
        .name("minka-syu-raise".into())
        .spawn(move || {
            for event in instance.events() {
                if let Event::TurnedAway = event {
                    raise_self();
                }
            }
        });
}

/// `workspaces.get`, find our window by app id, `windows.activate` it.
fn raise_self() {
    let (Some(dir), Some(display)) = (runtime_dir(), std::env::var_os("WAYLAND_DISPLAY")) else {
        return;
    };
    let sock = dir.join(format!("shojiwm-{}.sock", display.to_string_lossy()));
    let Ok(mut stream) = UnixStream::connect(&sock) else {
        return;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    if writeln!(stream, r#"{{"id":1,"method":"workspaces.get"}}"#).is_err() {
        return;
    }
    let Ok(reader_stream) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(reader_stream);
    let mut line = String::new();
    // Broadcasts may arrive first; read until our reply.
    for _ in 0..50 {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if v.get("id").and_then(Value::as_u64) != Some(1) {
            continue;
        }
        // ShojiWM's activate toggles like a dock click: on the focused
        // window it MINIMISES. Already in front means nothing to do.
        if let Some(id) = v
            .get("result")
            .and_then(find_window)
            .filter(|w| !w.focused || w.minimized)
            .map(|w| w.id)
        {
            let request = serde_json::json!({ "id": 2, "method": "windows.activate", "params": { "windowId": id } });
            let _ = writeln!(stream, "{request}");
            line.clear();
            let _ = reader.read_line(&mut line);
        }
        return;
    }
}

#[derive(Debug, PartialEq)]
struct OwnWindow {
    id: String,
    focused: bool,
    minimized: bool,
}

/// The first window anywhere in the reply whose appId is ours.
fn find_window(v: &Value) -> Option<OwnWindow> {
    match v {
        Value::Object(map) => {
            if map.get("appId").and_then(Value::as_str) == Some(APP_ID)
                && let Some(id) = map.get("id").and_then(Value::as_str)
            {
                let flag = |k: &str| map.get(k).and_then(Value::as_bool).unwrap_or(false);
                return Some(OwnWindow {
                    id: id.to_string(),
                    focused: flag("focused"),
                    minimized: flag("minimized"),
                });
            }
            map.values().find_map(find_window)
        }
        Value::Array(items) => items.iter().find_map(find_window),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_own_window_at_any_depth() {
        let v = serde_json::json!({
            "currentMonitor": "eDP-1",
            "monitors": [{ "workspaces": [{ "windows": [
                { "id": "7", "appId": "firefox" },
                { "id": "9", "appId": "minka-syu", "title": "MinkaSyu" }
            ]}]}]
        });
        let w = find_window(&v).unwrap();
        assert_eq!(w.id, "9");
        assert!(!w.focused && !w.minimized);
        assert_eq!(find_window(&serde_json::json!({ "monitors": [] })), None);
    }
}
