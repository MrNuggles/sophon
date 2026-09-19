//! `sophon --helper`: the small privileged helper, started through pkexec.
//!
//! It reads one request per line on stdin and answers one line on stdout.
//! It only understands a fixed set of requests, validates every argument,
//! runs no commands and touches no files. It exits when stdin closes, so it
//! never outlives Sophon, and its kernel counters are detached with it.
//!
//! Requests:
//!   signal <pid> <TERM|KILL>   -> ok | err <reason>
//!   net-start                  -> ok | err <reason>
//!   net-read                   -> net <cookie>:<rx>:<tx> ...
//!   quit

use std::io::{BufRead, Write};

pub fn run() -> ! {
    // Die with our parent (Sophon) even if the pipe somehow stays open.
    unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) };
    let mut out = std::io::stdout().lock();
    if unsafe { libc::geteuid() } != 0 {
        let _ = writeln!(out, "err not running as root");
        std::process::exit(1);
    }
    let _ = writeln!(out, "ready");
    let _ = out.flush();
    let mut net: Option<crate::ebpf::NetCounters> = None;
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let mut parts = line.split_whitespace();
        let reply = match parts.next() {
            Some("signal") => signal(parts.next(), parts.next()),
            Some("net-start") => match &net {
                Some(_) => "ok".to_string(),
                None => match crate::ebpf::NetCounters::start() {
                    Ok(n) => {
                        net = Some(n);
                        "ok".to_string()
                    }
                    Err(e) => format!("err {e}"),
                },
            },
            Some("net-read") => match &net {
                Some(n) => {
                    let mut s = String::from("net");
                    for (cookie, rx, tx) in n.read() {
                        s.push_str(&format!(" {cookie}:{rx}:{tx}"));
                    }
                    s
                }
                None => "err not started".to_string(),
            },
            Some("quit") | None => break,
            Some(other) => format!("err unknown request {other}"),
        };
        if writeln!(out, "{reply}").and_then(|_| out.flush()).is_err() {
            break;
        }
    }
    std::process::exit(0);
}

fn signal(pid: Option<&str>, sig: Option<&str>) -> String {
    let Some(pid) = pid.and_then(|p| p.parse::<i32>().ok()).filter(|&p| p > 1) else {
        return "err bad pid".into();
    };
    let signo = match sig {
        Some("TERM") => libc::SIGTERM,
        Some("KILL") => libc::SIGKILL,
        _ => return "err bad signal".into(),
    };
    if unsafe { libc::kill(pid, signo) } == 0 {
        "ok".into()
    } else {
        format!("err {}", std::io::Error::last_os_error())
    }
}
