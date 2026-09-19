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
    /// Creation time (unix seconds); used for the "age" sort.
    pub created: u64,
}

impl Container {
    pub fn running(&self) -> bool {
        self.state == "running"
    }

    /// The lowest published host port, or `u32::MAX` if none (for sorting).
    pub fn lowest_port(&self) -> u32 {
        self.ports.iter().map(|p| p.public as u32).min().unwrap_or(u32::MAX)
    }
}

/// One-shot resource stats for a running container.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Stats {
    pub cpu_pct: f32,
    pub mem_used: u64,
    pub mem_limit: u64,
}

/// Extra details shown when a container row is expanded (from inspect).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Details {
    pub command: String,
    pub restart_policy: String,
    pub health: String,
    /// (network name, IP address).
    pub networks: Vec<(String, String)>,
    /// (host source, container destination, read/write).
    pub mounts: Vec<(String, String, bool)>,
    pub env_count: usize,
    pub started_at: String,
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

/// One-shot CPU/memory stats for a container. Only meaningful while running.
pub fn stats(server: &Server, id: &str) -> Result<Stats, http::Error> {
    check_id(id)?;
    let resp = http::get(&server.host, server.port, &format!("/containers/{id}/stats?stream=false"))?;
    if resp.status != 200 {
        return Err(http::Error::Protocol(status_hint(resp.status)));
    }
    let v = json::parse(&resp.body).ok_or_else(|| http::Error::Protocol("invalid stats JSON".into()))?;
    Ok(stats_from(&v))
}

/// Inspect details for the expanded view.
pub fn inspect(server: &Server, id: &str) -> Result<Details, http::Error> {
    check_id(id)?;
    let resp = http::get(&server.host, server.port, &format!("/containers/{id}/json"))?;
    if resp.status != 200 {
        return Err(http::Error::Protocol(status_hint(resp.status)));
    }
    let v = json::parse(&resp.body).ok_or_else(|| http::Error::Protocol("invalid inspect JSON".into()))?;
    Ok(details_from(&v))
}

fn check_id(id: &str) -> Result<(), http::Error> {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(http::Error::Protocol("bad container id".into()));
    }
    Ok(())
}

/// One control action. `op` is "start", "stop" or "restart".
pub fn action(server: &Server, id: &str, op: &str) -> Result<(), http::Error> {
    if !matches!(op, "start" | "stop" | "restart") {
        return Err(http::Error::Protocol("unknown action".into()));
    }
    check_id(id)?;
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
        created: v.get("Created").num() as u64,
    }
}

fn stats_from(v: &Value) -> Stats {
    let cpu = v.get("cpu_stats");
    let pre = v.get("precpu_stats");
    let total = cpu.get("cpu_usage").get("total_usage").num();
    let pre_total = pre.get("cpu_usage").get("total_usage").num();
    let sys = cpu.get("system_cpu_usage").num();
    let pre_sys = pre.get("system_cpu_usage").num();
    let online = {
        let o = cpu.get("online_cpus").num();
        if o > 0.0 { o } else { cpu.get("cpu_usage").get("percpu_usage").arr().len().max(1) as f64 }
    };
    let (cpu_delta, sys_delta) = (total - pre_total, sys - pre_sys);
    let cpu_pct = if sys_delta > 0.0 && cpu_delta >= 0.0 { (cpu_delta / sys_delta * online * 100.0) as f32 } else { 0.0 };
    let mem = v.get("memory_stats");
    // cgroup v2 reports inactive_file in `stats`; subtract it so the figure
    // matches `docker stats` rather than counting page cache.
    let inactive = mem.get("stats").get("inactive_file").num();
    let usage = mem.get("usage").num();
    Stats {
        cpu_pct: cpu_pct.clamp(0.0, online as f32 * 100.0),
        mem_used: (usage - inactive).max(0.0) as u64,
        mem_limit: mem.get("limit").num() as u64,
    }
}

fn details_from(v: &Value) -> Details {
    let cfg = v.get("Config");
    let command = {
        let cmd = cfg.get("Cmd").arr();
        if cmd.is_empty() {
            format!("{} {}", v.get("Path").str(), v.get("Args").arr().iter().map(|a| a.str()).collect::<Vec<_>>().join(" ")).trim().to_string()
        } else {
            cmd.iter().map(|a| a.str()).collect::<Vec<_>>().join(" ")
        }
    };
    let networks = match v.get("NetworkSettings").get("Networks") {
        Value::Obj(m) => m.iter().map(|(k, nv)| (k.clone(), nv.get("IPAddress").str().to_string())).collect(),
        _ => Vec::new(),
    };
    let mounts = v
        .get("Mounts")
        .arr()
        .iter()
        .map(|m| (m.get("Source").str().to_string(), m.get("Destination").str().to_string(), m.get("RW").bool()))
        .collect();
    Details {
        command,
        restart_policy: v.get("HostConfig").get("RestartPolicy").get("Name").str().to_string(),
        health: v.get("State").get("Health").get("Status").str().to_string(),
        networks,
        mounts,
        env_count: cfg.get("Env").arr().len(),
        started_at: v.get("State").get("StartedAt").str().to_string(),
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
        assert_eq!(c.lowest_port(), 8080);
    }

    #[test]
    fn parses_stats() {
        let j = br#"{"cpu_stats":{"cpu_usage":{"total_usage":200,"percpu_usage":[1,2]},"system_cpu_usage":2000,"online_cpus":2},
            "precpu_stats":{"cpu_usage":{"total_usage":100},"system_cpu_usage":1000},
            "memory_stats":{"usage":1048576,"limit":8388608,"stats":{"inactive_file":48576}}}"#;
        let s = stats_from(&json::parse(j).unwrap());
        // cpu_delta=100, sys_delta=1000, online=2 -> 100/1000*2*100 = 20%
        assert!((s.cpu_pct - 20.0).abs() < 0.01);
        assert_eq!(s.mem_used, 1048576 - 48576);
        assert_eq!(s.mem_limit, 8388608);
    }

    #[test]
    fn parses_inspect() {
        let j = br#"{"Config":{"Cmd":["nginx","-g","daemon off;"],"Env":["A=1","B=2"]},
            "HostConfig":{"RestartPolicy":{"Name":"unless-stopped"}},
            "State":{"Health":{"Status":"healthy"},"StartedAt":"2024-01-01T00:00:00Z"},
            "NetworkSettings":{"Networks":{"bridge":{"IPAddress":"172.17.0.2"}}},
            "Mounts":[{"Source":"/srv/data","Destination":"/data","RW":true}]}"#;
        let d = details_from(&json::parse(j).unwrap());
        assert_eq!(d.command, "nginx -g daemon off;");
        assert_eq!(d.restart_policy, "unless-stopped");
        assert_eq!(d.health, "healthy");
        assert_eq!(d.networks, vec![("bridge".to_string(), "172.17.0.2".to_string())]);
        assert_eq!(d.mounts.len(), 1);
        assert_eq!(d.env_count, 2);
    }

    #[test]
    fn action_validates_id() {
        let s = Server { name: "x".into(), host: "localhost".into(), port: 1 };
        assert!(action(&s, "../etc", "start").is_err());
        assert!(action(&s, "abc", "delete").is_err());
    }
}
