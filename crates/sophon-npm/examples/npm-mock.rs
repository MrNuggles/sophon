//! A tiny mock Nginx Proxy Manager API for trying the Proxy tab without a
//! server. Usage: `cargo run -p sophon-npm --example npm-mock -- [port]` (default
//! 2381), then point Sophon at `http://127.0.0.1:<port>` — any email and
//! password are accepted. Every request is printed to stdout. The fake fleet
//! is deliberately varied: a wildcard-cert host, a plain-HTTP one, a
//! two-domain host, a disabled one and one whose certificate has lapsed.
//!
//! Std only, like the rest of the crate.
use std::io::{Read, Write};
use std::net::TcpListener;

struct FakeHost {
    id: u64,
    domains: &'static [&'static str],
    host: &'static str,
    port: u16,
    enabled: bool,
    ssl_forced: bool,
    cert: u64,
    caching: bool,
    exploits: bool,
    http2: bool,
    websockets: bool,
}

const HOSTS: [FakeHost; 6] = [
    FakeHost {
        id: 7,
        domains: &["jellyfin.lcdrdata.org"],
        host: "192.168.1.209",
        port: 8096,
        enabled: true,
        ssl_forced: true,
        cert: 3,
        caching: false,
        exploits: true,
        http2: true,
        websockets: true,
    },
    FakeHost {
        id: 9,
        domains: &["grafana.lcdrdata.org"],
        host: "127.0.0.1",
        port: 3000,
        enabled: true,
        ssl_forced: true,
        cert: 3,
        caching: false,
        exploits: true,
        http2: true,
        websockets: true,
    },
    // Points at the docker mock's web-server (127.0.0.1:2376, public 8080),
    // so cross-referencing the two mocks lights the Proxied dot.
    FakeHost {
        id: 11,
        domains: &["web.lcdrdata.org"],
        host: "127.0.0.1",
        port: 8080,
        enabled: true,
        ssl_forced: true,
        cert: 3,
        caching: false,
        exploits: true,
        http2: true,
        websockets: false,
    },
    FakeHost {
        id: 12,
        domains: &["files.lcdrdata.org", "dl.lcdrdata.org"],
        host: "192.168.1.208",
        port: 8444,
        enabled: true,
        ssl_forced: false,
        cert: 0,
        caching: true,
        exploits: false,
        http2: false,
        websockets: false,
    },
    FakeHost {
        id: 15,
        domains: &["staging.lcdrdata.org"],
        host: "192.168.1.209",
        port: 5000,
        enabled: false,
        ssl_forced: false,
        cert: 4,
        caching: false,
        exploits: false,
        http2: false,
        websockets: false,
    },
    FakeHost {
        id: 18,
        domains: &["ha.lcdrdata.org"],
        host: "192.168.1.50",
        port: 8123,
        enabled: true,
        ssl_forced: true,
        cert: 3,
        caching: false,
        exploits: true,
        http2: true,
        websockets: true,
    },
];

const CERTS: [(u64, &str, &str, &str); 2] = [
    (3, "lcdrdata.org wildcard", "lcdrdata.org", "2026-10-01 03:48:00"),
    (4, "staging cert", "staging.lcdrdata.org", "2026-07-01 00:00:00"),
];

fn main() {
    let port: u16 = std::env::args().nth(1).and_then(|p| p.parse().ok()).unwrap_or(2381);
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("mock: cannot bind 127.0.0.1:{port}: {e}");
            return;
        }
    };
    println!("mock: NPM API on 127.0.0.1:{port} — any email/password logs in (Ctrl+C to stop)");
    for stream in listener.incoming() {
        if let Ok(stream) = stream {
            std::thread::spawn(move || serve(stream));
        }
    }
}

fn serve(mut stream: std::net::TcpStream) {
    let mut buf = [0u8; 4096];
    let mut raw = Vec::new();
    loop {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if raw.windows(4).any(|w| w == b"\r\n\r\n") && body_complete(&raw) {
                    break;
                }
            }
        }
    }
    let text = String::from_utf8_lossy(&raw);
    let line = text.lines().next().unwrap_or_default();
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    println!("mock: {method} {path}");
    let path = path.split('?').next().unwrap_or(path);
    let body_text = text.split("\r\n\r\n").nth(1).unwrap_or("");

    let (status, body) = match (method, path) {
        ("POST", "/api/tokens") => {
            if body_text.contains("\"identity\"") {
                (200, r#"{"token":"mock-token","expires":"2026-09-20T20:00:00Z"}"#.into())
            } else {
                (400, r#"{"error":{"message":"Identity or Secret is wrong"}}"#.into())
            }
        }
        ("GET", "/api/nginx/proxy-hosts") => (200, hosts_json()),
        ("GET", "/api/nginx/certificates") => (200, certs_json()),
        ("GET", "/api/") => (200, r#"{"status":"OK","setup":true,"version":{"major":2,"minor":15,"revision":1}}"#.into()),
        _ => (404, "{}".into()),
    };
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Error",
    };
    let resp = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
}

/// True once the body announced by `Content-Length` has fully arrived.
fn body_complete(raw: &[u8]) -> bool {
    let split = match raw.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(s) => s,
        None => return false,
    };
    let head = String::from_utf8_lossy(&raw[..split]);
    let len = head
        .lines()
        .find_map(|l| l.split_once(':'))
        .filter(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    raw.len() >= split + 4 + len
}

fn hosts_json() -> String {
    let items: Vec<String> = HOSTS
        .iter()
        .map(|h| {
            let domains: Vec<String> = h.domains.iter().map(|d| format!(r#""{d}""#)).collect();
            format!(
                r#"{{"id":{},"domain_names":[{}],"forward_scheme":"http","forward_host":"{}","forward_port":{},
                "enabled":{},"ssl_forced":{},"certificate_id":{},"caching_enabled":{},"block_exploits":{},
                "http2_support":{},"allow_websocket_upgrade":{},"modified_on":"2026-08-01 12:00:00"}}"#,
                h.id,
                domains.join(","),
                h.host,
                h.port,
                h.enabled,
                h.ssl_forced,
                h.cert,
                h.caching,
                h.exploits,
                h.http2,
                h.websockets
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}

fn certs_json() -> String {
    let items: Vec<String> = CERTS
        .iter()
        .map(|(id, name, domain, expires)| {
            format!(
                r#"{{"id":{},"nice_name":"{}","domains":["{}"],"provider":"letsencrypt","expires":"{}","is_deleted":false}}"#,
                id, name, domain, expires
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}
