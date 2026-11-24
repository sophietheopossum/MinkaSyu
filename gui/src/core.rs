//! The update core, spoken to over its stdio: `minka-syu serve`, NDJSON in
//! both directions (MinkaLink's protocol). The QML window's Syu.qml did the
//! same through Quickshell's Process.
//!
//! The child is a plain std::process::Child, not the toolkit's Process: that
//! one SIGTERMs its child's whole process group when its owner goes away,
//! and the core starts paru's terminal. (The core now puts the terminal in a
//! group of its own as well.) When the window closes, the core reads EOF on
//! its stdin and exits by itself.

use anti_webui::prelude::*;
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::rc::Rc;
use std::time::Duration;

pub type Reply = Result<Value, String>;
type Callback = Box<dyn FnOnce(Reply)>;
type BroadcastFn = Box<dyn Fn(&str, Value)>;
type StatusFn = Box<dyn Fn(Result<(), String>)>;

enum CoreMsg {
    Line(u64, String),
    Exited(u64),
}

struct Core {
    stdin: RefCell<Option<ChildStdin>>,
    child: RefCell<Option<Child>>,
    /// Bumped per spawn: lines and exits of an older child are dropped.
    generation: Cell<u64>,
    next_id: Cell<u64>,
    pending: RefCell<HashMap<u64, Callback>>,
    sender: UiSender<CoreMsg>,
    on_broadcast: BroadcastFn,
    on_status: StatusFn,
}

thread_local! {
    static CORE: RefCell<Option<Rc<Core>>> = const { RefCell::new(None) };
}

fn core() -> Option<Rc<Core>> {
    CORE.with(|c| c.borrow().clone())
}

/// The core: $MINKA_SYU_BIN, else this binary run as `minka-syu`. MinkaDE's
/// one binary holds both, and /proc/self/exe is the build this window runs
/// even after an upgrade replaced the file, so a restarted core always
/// speaks the window's protocol.
fn core_command() -> Command {
    use std::os::unix::process::CommandExt;
    match std::env::var_os("MINKA_SYU_BIN").filter(|v| !v.is_empty()) {
        Some(p) => Command::new(p),
        None => {
            let mut core = Command::new("/proc/self/exe");
            core.arg0("minka-syu");
            core
        }
    }
}

/// Starts the core. Must run inside the window's root (the message channel
/// lives as long as that owner). `on_broadcast` receives {event, payload}
/// pushes; `on_status` hears Ok on every (re)start and Err when it dies.
pub fn start(
    on_broadcast: impl Fn(&str, Value) + 'static,
    on_status: impl Fn(Result<(), String>) + 'static,
) {
    let sender = ui_channel(|msg: CoreMsg| handle(msg));
    let core = Rc::new(Core {
        stdin: RefCell::new(None),
        child: RefCell::new(None),
        generation: Cell::new(0),
        next_id: Cell::new(1),
        pending: RefCell::new(HashMap::new()),
        sender,
        on_broadcast: Box::new(on_broadcast),
        on_status: Box::new(on_status),
    });
    CORE.with(|c| *c.borrow_mut() = Some(Rc::clone(&core)));
    spawn(&core);
}

fn spawn(core: &Rc<Core>) {
    let generation = core.generation.get() + 1;
    core.generation.set(generation);
    let child = core_command()
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            (core.on_status)(Err(format!("cannot start minka-syu serve: {e}")));
            return;
        }
    };
    let stdout = child.stdout.take();
    *core.stdin.borrow_mut() = child.stdin.take();
    *core.child.borrow_mut() = Some(child);
    if let Some(stdout) = stdout {
        let sender = core.sender.clone();
        let _ = std::thread::Builder::new()
            .name("minka-syu-core".into())
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    if !line.trim().is_empty() {
                        sender.send(CoreMsg::Line(generation, line));
                    }
                }
                sender.send(CoreMsg::Exited(generation));
            });
    }
    (core.on_status)(Ok(()));
}

fn handle(msg: CoreMsg) {
    let Some(core) = core() else { return };
    match msg {
        CoreMsg::Line(generation, line) => {
            if generation != core.generation.get() {
                return;
            }
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                return; // malformed lines are ignored, as ShojiClient does
            };
            if let Some(event) = v.get("event").and_then(Value::as_str) {
                let payload = v.get("payload").cloned().unwrap_or(Value::Null);
                (core.on_broadcast)(event, payload);
                return;
            }
            let Some(id) = v.get("id").and_then(Value::as_u64) else {
                return;
            };
            // Taken out before the call: a callback may well send the next request.
            let callback = core.pending.borrow_mut().remove(&id);
            if let Some(callback) = callback {
                let reply = match v.get("error") {
                    Some(err) => Err(err
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| err.to_string())),
                    None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
                };
                callback(reply);
            }
        }
        CoreMsg::Exited(generation) => {
            if generation != core.generation.get() {
                return;
            }
            core.stdin.borrow_mut().take();
            if let Some(mut child) = core.child.borrow_mut().take() {
                let _ = child.wait();
            }
            let orphans: Vec<Callback> = core
                .pending
                .borrow_mut()
                .drain()
                .map(|(_, cb)| cb)
                .collect();
            for cb in orphans {
                cb(Err("update core exited".into()));
            }
            (core.on_status)(Err("update core exited; restarting it".into()));
            // A crash-looping core is retried every few seconds, not spun on.
            set_timeout(Duration::from_secs(3), move || {
                if let Some(core) = self::core()
                    && core.generation.get() == generation
                {
                    spawn(&core);
                }
            });
        }
    }
}

/// One request; `on_reply` runs on the UI thread with the result or the
/// error message.
pub fn request(method: &str, params: Value, on_reply: impl FnOnce(Reply) + 'static) {
    let Some(core) = core() else {
        on_reply(Err("update core not started".into()));
        return;
    };
    let id = core.next_id.get();
    core.next_id.set(id + 1);
    let line = json!({ "id": id, "method": method, "params": params });
    let written = core.stdin.borrow_mut().as_mut().is_some_and(|stdin| {
        writeln!(stdin, "{line}")
            .and_then(|_| stdin.flush())
            .is_ok()
    });
    if written {
        core.pending.borrow_mut().insert(id, Box::new(on_reply));
    } else {
        on_reply(Err("update core is not running".into()));
    }
}
