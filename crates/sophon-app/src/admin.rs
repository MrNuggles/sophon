//! Admin features: a privileged helper started with pkexec (see helper.rs).
//! Sophon itself never runs as root; this module talks to the helper over
//! a pipe and exposes safe, simple calls to the rest of the app.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering::Relaxed};

struct Helper {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

static HELPER: Mutex<Option<Helper>> = Mutex::new(None);
/// 0 off, 1 waiting for the password, 2 active.
static STATE: AtomicU8 = AtomicU8::new(0);
/// Kernel traffic counters are running (per-app UDP + TCP).
static NET: AtomicBool = AtomicBool::new(false);

pub fn is_active() -> bool {
    STATE.load(Relaxed) == 2
}

pub fn net_active() -> bool {
    NET.load(Relaxed)
}

pub fn toggle() {
    match STATE.load(Relaxed) {
        0 => enable(),
        2 => disable(),
        _ => {}
    }
}

/// Asks for the password (pkexec) and starts the helper, off the UI thread.
pub fn enable() {
    if STATE.compare_exchange(0, 1, Relaxed, Relaxed).is_err() {
        return;
    }
    status("Waiting for your password…");
    std::thread::spawn(|| {
        let result = start();
        let message = match result {
            Ok(net_note) => {
                STATE.store(2, Relaxed);
                format!("Active until Sophon closes. You can now end any process.{net_note}")
            }
            Err(e) => {
                STATE.store(0, Relaxed);
                e
            }
        };
        let _ = slint::invoke_from_event_loop(move || {
            crate::settings_page::set_admin_status(message.clone());
            if is_active() {
                crate::ui::toast("Admin features enabled");
            }
        });
    });
}

fn start() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("Couldn't find Sophon's own path: {e}"))?;
    let mut child = Command::new("pkexec")
        .arg(exe)
        .arg("--helper")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Couldn't start pkexec ({e}). Is polkit installed?"))?;
    let stdin = child.stdin.take().ok_or("no pipe")?;
    let mut stdout = BufReader::new(child.stdout.take().ok_or("no pipe")?);
    let mut line = String::new();
    if stdout.read_line(&mut line).unwrap_or(0) == 0 || line.trim() != "ready" {
        let _ = child.wait();
        return Err("Not enabled: the password prompt was cancelled or failed.".into());
    }
    *HELPER.lock().unwrap() = Some(Helper { child, stdin, stdout });
    Ok(match request("net-start") {
        Some(r) if r == "ok" => {
            NET.store(true, Relaxed);
            " Per-app network now counts UDP too (games, voice chat).".into()
        }
        Some(r) => format!(" (UDP counting unavailable: {})", r.trim_start_matches("err ")),
        None => String::new(),
    })
}

pub fn disable() {
    if let Some(mut h) = HELPER.lock().unwrap().take() {
        let _ = writeln!(h.stdin, "quit");
        drop(h.stdin);
        let _ = h.child.wait();
    }
    NET.store(false, Relaxed);
    STATE.store(0, Relaxed);
    status("Off.");
}

fn status(msg: &str) {
    let msg = msg.to_string();
    let _ = slint::invoke_from_event_loop(move || crate::settings_page::set_admin_status(msg));
}

/// One request/response round trip. `None` if the helper isn't running or died.
fn request(line: &str) -> Option<String> {
    let mut guard = HELPER.lock().unwrap();
    let h = guard.as_mut()?;
    let ok = writeln!(h.stdin, "{line}").and_then(|_| h.stdin.flush()).is_ok();
    let mut reply = String::new();
    if !ok || h.stdout.read_line(&mut reply).unwrap_or(0) == 0 {
        // The helper exited (or was killed): turn admin features off.
        *guard = None;
        NET.store(false, Relaxed);
        STATE.store(0, Relaxed);
        status("The admin helper stopped. Enable it again if you need it.");
        return None;
    }
    Some(reply.trim().to_string())
}

/// Sends a signal with admin rights. `Err` carries the helper's reason.
pub fn signal(pid: u32, kill: bool) -> Result<(), String> {
    match request(&format!("signal {pid} {}", if kill { "KILL" } else { "TERM" })) {
        Some(r) if r == "ok" => Ok(()),
        Some(r) => Err(r.trim_start_matches("err ").to_string()),
        None => Err("admin helper not running".into()),
    }
}

/// Cumulative (cookie, rx, tx) per socket from the kernel counters.
pub fn net_counters() -> Option<Vec<(u64, u64, u64)>> {
    if !net_active() {
        return None;
    }
    let reply = request("net-read")?;
    let rest = reply.strip_prefix("net")?;
    Some(
        rest.split_whitespace()
            .filter_map(|e| {
                let mut p = e.split(':').map(|n| n.parse::<u64>().ok());
                Some((p.next()??, p.next()??, p.next()??))
            })
            .collect(),
    )
}
