//! Docker Engine API client for Sophon: a tiny, dependency-free HTTP/1.1 + JSON
//! layer plus container models and operations. Talks plain HTTP to a Docker
//! socket proxy on the LAN — never the raw socket over TCP (see SECURITY.md).
//!
//! Only the endpoints Sophon needs are used: list containers, and start / stop
//! / restart. A matching socket-proxy allowlist is `CONTAINERS=1 POST=1`.

pub mod http;
pub mod json;

use json::Value;

/// A configured Docker endpoint (a socket proxy on one server).
#[derive(Clone, Debug, PartialEq)]
pub struct Server {
    pub name: String,
    pub host: String,
    pub port: u16,
}

impl Server {
    /// Parses `host:port` (or a bare host, default port 2375). Also accepts an
    /// `http://` prefix for convenience.
    pub fn parse(name: &str, addr: &str) -> Option<Server> {
        let addr = addr.trim().trim_start_matches("http://").trim_end_matches('/');
        if addr.is_empty() {
            return None;
        }
        let (host, port) = match addr.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().ok()?),
            None => (addr.to_string(), 2375),
        };
        if host.is_empty() {
            return None;
        }
        Some(Server { name: name.trim().to_string(), host, port })
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Port {
    pub private: u16,
    pub public: u16,
    /// "tcp" / "udp".
    pub proto: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Container {
    pub id: String,
    pub name: String,
    pub image: String,
    /// "running", "exited", "created", "paused", "restarting", "dead".
    pub state: String,
    /// Human status, e.g. "Up 3 hours" / "Exited (0) 2 days ago".
    pub status: String,
    pub ports: Vec<Port>,
}

impl Container {
    pub fn running(&self) -> bool {
        self.state == "running"
    }
}

/// A snapshot of one server: either its containers, or why it couldn't be read.
#[derive(Clone, Debug)]
pub struct ServerState {
    pub server: Server,
    pub error: Option<String>,
    pub containers: Vec<Container>,
}

/// Lists all containers (running and stopped) on a server.
pub fn list(server: &Server) -> Result<Vec<Container>, http::Error> {
    let resp = http::get(&server.host, server.port, "/containers/json?all=1")?;
    if resp.status != 200 {
        return Err(http::Error::Protocol(status_hint(resp.status)));
    }
    let v = json::parse(&resp.body).ok_or_else(|| http::Error::Protocol("invalid JSON".into()))?;
    Ok(v.arr().iter().map(container_from).collect())
}

/// One control action. `op` is "start", "stop" or "restart".
pub fn action(server: &Server, id: &str, op: &str) -> Result<(), http::Error> {
    if !matches!(op, "start" | "stop" | "restart") {
        return Err(http::Error::Protocol("unknown action".into()));
    }
    // Only hex/limited container ids are ever passed (Docker ids are hex), but
    // guard anyway so nothing odd reaches the request line.
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(http::Error::Protocol("bad container id".into()));
    }
    let resp = http::post(&server.host, server.port, &format!("/containers/{id}/{op}"))?;
    // 204 = done, 304 = already in that state (fine).
    match resp.status {
        204 | 304 => Ok(()),
        s => Err(http::Error::Protocol(status_hint(s))),
    }
}

fn status_hint(status: u16) -> String {
    match status {
        403 => "the proxy denied this (check its CONTAINERS/POST allowlist)".into(),
        404 => "not found".into(),
        500 => "the Docker daemon returned an error".into(),
        s => format!("HTTP {s}"),
    }
}

fn container_from(v: &Value) -> Container {
    let names = v.get("Names").arr();
    let name = names.first().map(|n| n.str().trim_start_matches('/').to_string()).unwrap_or_default();
    let ports = v
        .get("Ports")
        .arr()
        .iter()
        .filter(|p| p.get("PublicPort").num() > 0.0) // only published ports
        .map(|p| Port {
            private: p.get("PrivatePort").num() as u16,
            public: p.get("PublicPort").num() as u16,
            proto: p.get("Type").str().to_string(),
        })
        .collect();
    Container {
        id: v.get("Id").str().chars().take(64).filter(|c| c.is_ascii_alphanumeric()).collect(),
        name,
        image: v.get("Image").str().to_string(),
        state: v.get("State").str().to_string(),
        status: v.get("Status").str().to_string(),
        ports,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_parse() {
        assert_eq!(Server::parse("A", "http://10.0.0.5:2375/"), Some(Server { name: "A".into(), host: "10.0.0.5".into(), port: 2375 }));
        assert_eq!(Server::parse("B", "nas").unwrap().port, 2375);
        assert_eq!(Server::parse("C", ""), None);
    }

    #[test]
    fn parses_container_list() {
        let j = br#"[{"Id":"deadbeef00","Names":["/web"],"Image":"nginx:latest","State":"running",
            "Status":"Up 3 hours","Ports":[{"PrivatePort":80,"PublicPort":8080,"Type":"tcp"},{"PrivatePort":9000,"Type":"tcp"}]}]"#;
        let v = json::parse(j).unwrap();
        let c = container_from(&v.arr()[0]);
        assert_eq!(c.name, "web");
        assert!(c.running());
        assert_eq!(c.ports.len(), 1); // only the published one
        assert_eq!(c.ports[0].public, 8080);
    }

    #[test]
    fn action_validates_id() {
        let s = Server { name: "x".into(), host: "localhost".into(), port: 1 };
        assert!(action(&s, "../etc", "start").is_err());
        assert!(action(&s, "abc", "delete").is_err());
    }
}
