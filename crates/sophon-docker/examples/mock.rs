//! A tiny mock Docker API for trying the Docker tab without a server.
//!
//! Usage: `cargo run -p sophon-docker --example mock -- [port]` (default 2375),
//! then point Sophon at `127.0.0.1:<port>`. Serves list/stats/inspect for three
//! fake containers (stats drift so the graphs move) and answers start/stop/
//! restart with 204 — printing every request, so a run's log shows exactly
//! which control calls were made.
//!
//! Std only, like the rest of the crate. For the socket-proxy allowlist this
//! mimics, see docs/docker.md.
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;

/// The fake fleet: (id, name, image, state, status, published ports, created).
/// State is mutable so a POST is reflected in later `list` calls.
struct Fake {
    id: &'static str,
    name: &'static str,
    image: &'static str,
    state: &'static str,
    status: &'static str,
    ports: &'static [(u16, u16, &'static str)],
    created: u64,
}

const FAKES: [Fake; 3] = [
    Fake {
        id: "8f14e45fceea167a5a36dedd4bea2543a1b2c3d4e5f6071829304a5b6c7d8e9f",
        name: "veloren-server",
        image: "veloren/server:weekly",
        state: "running",
        status: "Up 2 hours",
        ports: &[(14004, 14004, "tcp"), (14004, 14004, "udp")],
        created: 1755000000,
    },
    Fake {
        id: "c9f17208ceea167a5a36dedd4bea2543a9b8c7d6e5f40312233445566778899aa",
        name: "web-server",
        image: "nginx:1.27-alpine",
        state: "running",
        status: "Up 6 days",
        ports: &[(80, 8080, "tcp")],
        created: 1750000000,
    },
    Fake {
        id: "45c48cce2e2d7fbdea1afc51c7c6ad2deea1212ab3c4d5e6f708192a3b4c5d6",
        name: "restic-backup",
        image: "restic/restic:0.17",
        state: "exited",
        status: "Exited (0) 3 days ago",
        ports: &[],
        created: 1749000000,
    },
];

/// States as mutated by start/stop/restart, by container id.
static STATES: Mutex<[&'static str; 3]> = Mutex::new([FAKES[0].state, FAKES[1].state, FAKES[2].state]);
/// Stats call counter, for drifting values.
static TICK: AtomicU64 = AtomicU64::new(1);

fn main() {
    let port: u16 = std::env::args().nth(1).and_then(|p| p.parse().ok()).unwrap_or(2375);
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("mock: cannot bind 127.0.0.1:{port}: {e}");
            return;
        }
    };
    println!("mock: Docker API on 127.0.0.1:{port} (Ctrl+C to stop)");
    for stream in listener.incoming() {
        if let Ok(stream) = stream {
            std::thread::spawn(move || serve(stream));
        }
    }
}

fn serve(mut stream: std::net::TcpStream) {
    let mut buf = [0u8; 4096];
    let mut head = Vec::new();
    // Read until the end of the header (the client sends no body).
    loop {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                head.extend_from_slice(&buf[..n]);
                if head.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
        }
    }
    let text = String::from_utf8_lossy(&head);
    let line = text.lines().next().unwrap_or_default();
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    println!("mock: {method} {path}");
    let path = path.split('?').next().unwrap_or(path);

    let (status, body) = match (method, path) {
        ("GET", "/containers/json") => (200, list_json()),
        ("GET", p) if p.starts_with("/containers/") && p.ends_with("/stats") => {
            if index_of(container_id(p, "/stats")).is_some() { (200, stats_json()) } else { (404, "{}".into()) }
        }
        ("GET", p) if p.starts_with("/containers/") && p.ends_with("/json") => {
            match index_of(container_id(p, "/json")) {
                Some(i) => (200, inspect_json(i)),
                None => (404, "{}".into()),
            }
        }
        ("POST", p) if p.starts_with("/containers/") => {
            // /containers/<id>/<op>
            match p.trim_start_matches("/containers/").split_once('/') {
                Some((id, op)) if matches!(op, "start" | "stop" | "restart") => match index_of(id) {
                    Some(i) => {
                        let mut states = STATES.lock().unwrap();
                        states[i] = if op == "stop" { "exited" } else { "running" };
                        (204, String::new())
                    }
                    None => (404, "{}".into()),
                },
                _ => (400, "{}".into()),
            }
        }
        _ => (404, "{}".into()),
    };
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        _ => "Error",
    };
    let resp = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
}

/// The id segment of `/containers/<id><suffix>`.
fn container_id<'a>(path: &'a str, suffix: &str) -> &'a str {
    path.trim_start_matches("/containers/").strip_suffix(suffix).unwrap_or("")
}

fn index_of(id: &str) -> Option<usize> {
    FAKES.iter().position(|f| f.id == id)
}

fn list_json() -> String {
    let states = STATES.lock().unwrap();
    let items: Vec<String> = FAKES
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let ports: Vec<String> = f
                .ports
                .iter()
                .map(|&(private, public, proto)| {
                    format!(r#"{{"PrivatePort":{private},"PublicPort":{public},"Type":"{proto}"}}"#)
                })
                .collect();
            format!(
                r#"{{"Id":"{}","Names":["/{}"],"Image":"{}","State":"{}","Status":"{}","Ports":[{}],"Created":{}}}"#,
                f.id, f.name, f.image, states[i], f.status, ports.join(","), f.created
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}

/// Stats that drift with each call, so the expanded graphs move.
fn stats_json() -> String {
    let n = TICK.fetch_add(1, Relaxed) as f64;
    // cpu_pct = (total - pre_total) / (sys - pre_sys) * online * 100.
    let total = |k: f64| k * (30.0 + 25.0 * (k / 5.0).sin());
    let sys = |k: f64| k * 1000.0;
    let mem_used = 300.0 + 40.0 * (n / 7.0).sin();
    format!(
        r#"{{"cpu_stats":{{"cpu_usage":{{"total_usage":{},"percpu_usage":[1,2]}},"system_cpu_usage":{},"online_cpus":2}},
            "precpu_stats":{{"cpu_usage":{{"total_usage":{}}},"system_cpu_usage":{}}},
            "memory_stats":{{"usage":{},"limit":{},"stats":{{"inactive_file":52428800}}}}}}"#,
        total(n) as u64,
        sys(n) as u64,
        total(n - 1.0) as u64,
        sys(n - 1.0) as u64,
        (mem_used * 1024.0 * 1024.0) as u64,
        2u64 * 1024 * 1024 * 1024
    )
}

fn inspect_json(i: usize) -> String {
    let f = &FAKES[i];
    let cmd = match i {
        0 => r#""Cmd":["veloren-server-cli"]"#,
        1 => r#""Cmd":["nginx","-g","daemon off;"]"#,
        _ => r#""Cmd":["restic","backup","/data"]"#,
    };
    let health = if i == 1 { "\"Health\":{\"Status\":\"healthy\"}" } else { "" };
    format!(
        r#"{{"Config":{{{cmd},"Env":["A=1","B=2"]}},
            "HostConfig":{{"RestartPolicy":{{"Name":"unless-stopped"}}}},
            "State":{{{health},"StartedAt":"2026-09-17T08:00:00Z"}},
            "NetworkSettings":{{"Networks":{{"bridge":{{"IPAddress":"172.17.0.{}"}}}}}},
            "Mounts":[{{"Source":"/srv/{}","Destination":"/data","RW":true}}]}}"#,
        i + 2,
        f.name.split('-').next().unwrap_or("data")
    )
}
