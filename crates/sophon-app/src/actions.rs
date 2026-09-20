//! Things the user can do to processes.

use std::collections::HashMap;
use std::process::{Command, Stdio};

use sophon_core::process::Process;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Signal {
    /// Polite request to exit (Windows' "End task").
    Term,
    /// Immediate, cannot be ignored.
    Kill,
}

/// Outcome of signalling a set of processes.
#[derive(Default)]
pub struct Outcome {
    pub sent: usize,
    pub denied: usize,
    pub gone: usize,
}

pub fn signal(pids: &[u32], sig: Signal) -> Outcome {
    let signo = match sig {
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    let own = std::process::id();
    let mut out = Outcome::default();
    for &pid in pids {
        if pid <= 1 || pid == own {
            out.denied += 1;
            continue;
        }
        if unsafe { libc::kill(pid as libc::pid_t, signo) } == 0 {
            out.sent += 1;
            continue;
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) => out.gone += 1,
            // Not permitted: retry through the admin helper if it's running.
            Some(libc::EPERM) if crate::admin::signal(pid, sig == Signal::Kill).is_ok() => out.sent += 1,
            _ => out.denied += 1,
        }
    }
    out
}

/// `roots` plus all their descendants, deepest first (children die before parents,
/// so nothing gets re-parented and survives).
pub fn tree(procs: &[Process], roots: &[u32]) -> Vec<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for p in procs {
        children.entry(p.ppid).or_default().push(p.pid);
    }
    let mut order = Vec::new();
    let mut stack: Vec<(u32, usize)> = roots.iter().map(|&r| (r, 0)).collect();
    let mut seen = std::collections::HashSet::new();
    while let Some((pid, depth)) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        order.push((pid, depth));
        for &c in children.get(&pid).map(Vec::as_slice).unwrap_or(&[]) {
            stack.push((c, depth + 1));
        }
    }
    order.sort_by(|a, b| b.1.cmp(&a.1));
    order.into_iter().map(|(pid, _)| pid).collect()
}

/// Shows the file in the user's file manager (Dolphin, Nautilus, ...),
/// falling back to opening its folder.
pub fn open_location(path: &str) -> bool {
    let uri = format!("file://{path}");
    let shown = Command::new("dbus-send")
        .args([
            "--session",
            "--print-reply",
            "--dest=org.freedesktop.FileManager1",
            "--type=method_call",
            "/org/freedesktop/FileManager1",
            "org.freedesktop.FileManager1.ShowItems",
            &format!("array:string:{uri}"),
            "string:",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if shown {
        return true;
    }
    let dir = std::path::Path::new(path).parent().map_or("/".into(), |d| d.to_string_lossy().into_owned());
    Command::new("xdg-open").arg(dir).stdout(Stdio::null()).stderr(Stdio::null()).spawn().is_ok()
}

/// Opens a URL (e.g. a Docker container's reverse-proxy address) in the
/// default browser.
pub fn open_url(url: &str) -> bool {
    Command::new("xdg-open")
        .arg(url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok()
}
