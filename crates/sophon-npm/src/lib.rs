//! Nginx Proxy Manager (NPM) API client for Sophon: login and read proxy hosts
//! plus their certificates, over the shared HTTP/JSON layer in sophon-net.
//! Talks plain HTTP to the NPM admin port (default 81) on the trusted LAN —
//! see SECURITY.md.
//!
//! Only the endpoints Sophon needs are used: token login, list proxy hosts,
//! list certificates. A dedicated NPM user for Sophon is recommended, and the
//! credentials live only in the local settings file.

pub use sophon_net::http;

use sophon_net::json::Value;

/// A configured NPM endpoint (the admin port on one server).
#[derive(Clone, Debug, PartialEq)]
pub struct Server {
    pub name: String,
    pub host: String,
    pub port: u16,
}

impl Server {
    /// Parses `host:port` (or a bare host, default port 81). Accepts an
    /// `http://` prefix for convenience; `https://` is stripped — Sophon talks
    /// plain HTTP to the admin port on the LAN (see SECURITY.md).
    pub fn parse(name: &str, addr: &str) -> Option<Server> {
        let addr = addr.trim().trim_start_matches("http://").trim_start_matches("https://").trim_end_matches('/');
        if addr.is_empty() {
            return None;
        }
        let (host, port) = match addr.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().ok()?),
            None => (addr.to_string(), 81),
        };
        if host.is_empty() {
            return None;
        }
        Some(Server { name: name.trim().to_string(), host, port })
    }
}

/// One proxy host: a domain routed by NPM to an internal service.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProxyHost {
    pub id: u64,
    /// All domains that route here, e.g. ["jellyfin.example.com"].
    pub domains: Vec<String>,
    pub forward_scheme: String,
    pub forward_host: String,
    pub forward_port: u16,
    pub enabled: bool,
    pub ssl_forced: bool,
    /// 0 = no certificate (plain HTTP host).
    pub certificate_id: u64,
    pub caching: bool,
    pub block_exploits: bool,
    pub http2: bool,
    pub websockets: bool,
    /// "2026-09-19 20:00:00" — last change, as NPM reports it.
    pub modified_on: String,
}

/// A certificate known to NPM (Let's Encrypt or custom).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Certificate {
    pub id: u64,
    pub nice_name: String,
    pub domains: Vec<String>,
    pub provider: String,
    /// "2026-10-01 03:48:00" — when it expires.
    pub expires: String,
}

/// Logs in and returns the bearer token used by every other call.
pub fn login(server: &Server, email: &str, password: &str) -> Result<String, http::Error> {
    let body = format!(r#"{{"identity":{},"secret":{}}}"#, json_quote(email), json_quote(password));
    let resp = http::request_with_body("POST", &server.host, server.port, "/api/tokens", &body)?;
    if resp.status != 200 {
        return Err(http::Error::Protocol(status_hint(resp.status)));
    }
    let v = sophon_net::json::parse(&resp.body).ok_or_else(|| http::Error::Protocol("invalid JSON".into()))?;
    let token = v.get("token").str();
    if token.is_empty() {
        return Err(http::Error::Protocol("no token in reply".into()));
    }
    Ok(token.to_string())
}

/// Lists every proxy host configured in NPM.
pub fn proxy_hosts(server: &Server, token: &str) -> Result<Vec<ProxyHost>, http::Error> {
    let resp = http::get_auth(&server.host, server.port, "/api/nginx/proxy-hosts", token)?;
    if resp.status != 200 {
        return Err(http::Error::Protocol(status_hint(resp.status)));
    }
    let v = sophon_net::json::parse(&resp.body).ok_or_else(|| http::Error::Protocol("invalid JSON".into()))?;
    Ok(v.arr().iter().map(host_from).collect())
}

/// Lists every certificate NPM knows (used for expiry and names).
pub fn certificates(server: &Server, token: &str) -> Result<Vec<Certificate>, http::Error> {
    let resp = http::get_auth(&server.host, server.port, "/api/nginx/certificates", token)?;
    if resp.status != 200 {
        return Err(http::Error::Protocol(status_hint(resp.status)));
    }
    let v = sophon_net::json::parse(&resp.body).ok_or_else(|| http::Error::Protocol("invalid JSON".into()))?;
    Ok(v.arr().iter().map(cert_from).collect())
}

/// The main domain of a host — the one shown in the row.
pub fn main_domain(host: &ProxyHost) -> &str {
    host.domains.first().map(String::as_str).unwrap_or("")
}

fn host_from(v: &Value) -> ProxyHost {
    ProxyHost {
        id: v.get("id").num() as u64,
        domains: v.get("domain_names").arr().iter().map(|d| d.str().to_string()).collect(),
        forward_scheme: v.get("forward_scheme").str().to_string(),
        forward_host: v.get("forward_host").str().to_string(),
        forward_port: v.get("forward_port").num() as u16,
        enabled: v.get("enabled").bool(),
        ssl_forced: v.get("ssl_forced").bool(),
        certificate_id: v.get("certificate_id").num() as u64,
        caching: v.get("caching_enabled").bool(),
        block_exploits: v.get("block_exploits").bool(),
        http2: v.get("http2_support").bool(),
        websockets: v.get("allow_websocket_upgrade").bool(),
        modified_on: v.get("modified_on").str().to_string(),
    }
}

fn cert_from(v: &Value) -> Certificate {
    Certificate {
        id: v.get("id").num() as u64,
        nice_name: v.get("nice_name").str().to_string(),
        domains: v.get("domains").arr().iter().map(|d| d.str().to_string()).collect(),
        provider: v.get("provider").str().to_string(),
        expires: v.get("expires").str().to_string(),
    }
}

fn status_hint(status: u16) -> String {
    match status {
        400 => "couldn't log in — check the email and password".into(),
        401 | 403 => "not authorised (expired token or restricted user)".into(),
        404 => "not found — is that the NPM admin port?".into(),
        s => format!("HTTP {s}"),
    }
}

/// Escapes a string as a JSON string value (login bodies).
fn json_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_parse() {
        assert_eq!(
            Server::parse("Monster", "http://192.168.1.209:81/"),
            Some(Server { name: "Monster".into(), host: "192.168.1.209".into(), port: 81 })
        );
        assert_eq!(Server::parse("x", "npm").unwrap().port, 81);
        assert_eq!(Server::parse("x", "https://npm.example.org"), Some(Server { name: "x".into(), host: "npm.example.org".into(), port: 81 }));
        assert_eq!(Server::parse("x", ""), None);
    }

    #[test]
    fn parses_proxy_hosts() {
        let j = br#"[{"id":7,"domain_names":["jellyfin.example.com","jf.example.org"],"forward_scheme":"http",
            "forward_host":"192.168.1.10","forward_port":8096,"enabled":true,"ssl_forced":true,
            "certificate_id":3,"caching_enabled":false,"block_exploits":true,"http2_support":true,
            "allow_websocket_upgrade":true,"modified_on":"2026-08-01 12:00:00"},
            {"id":8,"domain_names":["plain.example.com"],"forward_scheme":"http","forward_host":"127.0.0.1",
            "forward_port":9000,"enabled":false,"certificate_id":0}]"#;
        let v = sophon_net::json::parse(j).unwrap();
        let hosts: Vec<ProxyHost> = v.arr().iter().map(host_from).collect();
        assert_eq!(hosts.len(), 2);
        let h = &hosts[0];
        assert_eq!(h.id, 7);
        assert_eq!(h.domains.len(), 2);
        assert_eq!(main_domain(h), "jellyfin.example.com");
        assert_eq!(h.forward_port, 8096);
        assert!(h.enabled && h.ssl_forced && h.block_exploits && h.http2 && h.websockets);
        assert_eq!(h.certificate_id, 3);
        assert!(!hosts[1].enabled);
        assert_eq!(hosts[1].certificate_id, 0);
    }

    #[test]
    fn parses_certificates() {
        let j = br#"[{"id":3,"nice_name":"wildcard","domains":["example.com"],"provider":"letsencrypt",
            "expires":"2026-10-01 03:48:00","is_deleted":false}]"#;
        let v = sophon_net::json::parse(j).unwrap();
        let certs: Vec<Certificate> = v.arr().iter().map(cert_from).collect();
        assert_eq!(certs.len(), 1);
        assert_eq!(certs[0].id, 3);
        assert_eq!(certs[0].provider, "letsencrypt");
        assert_eq!(certs[0].expires, "2026-10-01 03:48:00");
    }

    #[test]
    fn json_quote_escapes() {
        assert_eq!(json_quote("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(json_quote("p\nw"), "\"p\\u000aw\"");
    }
}
