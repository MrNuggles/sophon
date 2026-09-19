//! A tiny blocking HTTP/1.1 client for the JSON APIs on Sophon's own LAN
//! (Docker socket proxies, Nginx Proxy Manager). Handles `Content-Length`,
//! chunked transfer, bearer auth and JSON bodies. No TLS — these endpoints are
//! plain HTTP on a trusted network (see SECURITY.md).

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// A parsed HTTP response.
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

#[derive(Debug)]
pub enum Error {
    /// Couldn't reach the host (down, wrong address, firewalled).
    Connect(String),
    Io(String),
    /// The response wasn't valid HTTP.
    Protocol(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Connect(s) => write!(f, "can't reach the server ({s})"),
            Error::Io(s) => write!(f, "{s}"),
            Error::Protocol(s) => write!(f, "unexpected response ({s})"),
        }
    }
}

/// Total time budget per request; a slow/hung server fails instead of blocking.
const TIMEOUT: Duration = Duration::from_secs(4);
/// Cap on a single response body, so a misbehaving endpoint can't exhaust memory.
const MAX_BODY: usize = 8 * 1024 * 1024;

pub fn get(host: &str, port: u16, path: &str) -> Result<Response, Error> {
    request("GET", host, port, path, &[], None)
}

/// GET with a bearer token (JSON APIs that authenticate per request).
pub fn get_auth(host: &str, port: u16, path: &str, token: &str) -> Result<Response, Error> {
    request("GET", host, port, path, &[("Authorization", &format!("Bearer {token}"))], None)
}

pub fn post(host: &str, port: u16, path: &str) -> Result<Response, Error> {
    request("POST", host, port, path, &[], None)
}

/// POST (or another method) with a JSON body.
pub fn request_with_body(method: &str, host: &str, port: u16, path: &str, json_body: &str) -> Result<Response, Error> {
    request(method, host, port, path, &[("Content-Type", "application/json")], Some(json_body))
}

fn request(method: &str, host: &str, port: u16, path: &str, headers: &[(&str, &str)], body: Option<&str>) -> Result<Response, Error> {
    let addr = (host, port)
        .to_socket_addrs()
        .map_err(|e| Error::Connect(e.to_string()))?
        .next()
        .ok_or_else(|| Error::Connect("no address".into()))?;
    let mut stream = TcpStream::connect_timeout(&addr, TIMEOUT).map_err(|e| Error::Connect(e.to_string()))?;
    stream.set_read_timeout(Some(TIMEOUT)).ok();
    stream.set_write_timeout(Some(TIMEOUT)).ok();

    // `Connection: close` lets us read to EOF without keep-alive bookkeeping.
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nAccept: application/json\r\n");
    for (key, value) in headers {
        req.push_str(&format!("{key}: {value}\r\n"));
    }
    match body {
        Some(b) => req.push_str(&format!("Content-Length: {}\r\n", b.len())),
        None => req.push_str("Content-Length: 0\r\n"),
    }
    req.push_str("Connection: close\r\n\r\n");
    if let Some(b) = body {
        req.push_str(b);
    }
    stream.write_all(req.as_bytes()).map_err(|e| Error::Io(e.to_string()))?;

    let mut raw = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&chunk[..n]);
                if raw.len() > MAX_BODY + 65536 {
                    return Err(Error::Protocol("response too large".into()));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(Error::Io(e.to_string())),
        }
    }
    parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> Result<Response, Error> {
    let split = find(raw, b"\r\n\r\n").ok_or_else(|| Error::Protocol("no header/body split".into()))?;
    let head = &raw[..split];
    let mut body = raw[split + 4..].to_vec();

    let mut lines = head.split(|&b| b == b'\n');
    let status_line = lines.next().ok_or_else(|| Error::Protocol("empty".into()))?;
    // "HTTP/1.1 200 OK"
    let status: u16 = status_line
        .split(|&b| b == b' ')
        .nth(1)
        .and_then(|s| std::str::from_utf8(s).ok())
        .and_then(|s| s.trim().parse().ok())
        .ok_or_else(|| Error::Protocol("no status code".into()))?;

    let mut chunked = false;
    let mut content_length: Option<usize> = None;
    for line in lines {
        let line = std::str::from_utf8(line).unwrap_or("").trim();
        if let Some((k, v)) = line.split_once(':') {
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
            if k == "transfer-encoding" && v.eq_ignore_ascii_case("chunked") {
                chunked = true;
            } else if k == "content-length" {
                content_length = v.parse().ok();
            }
        }
    }

    if chunked {
        body = dechunk(&body)?;
    } else if let Some(len) = content_length {
        body.truncate(len.min(MAX_BODY));
    }
    body.truncate(MAX_BODY);
    Ok(Response { status, body })
}

fn dechunk(mut b: &[u8]) -> Result<Vec<u8>, Error> {
    let mut out = Vec::with_capacity(b.len());
    loop {
        let nl = find(b, b"\r\n").ok_or_else(|| Error::Protocol("bad chunk header".into()))?;
        let size_str = std::str::from_utf8(&b[..nl]).unwrap_or("");
        // A chunk size may carry ";extensions"; take the hex prefix.
        let hex = size_str.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(hex, 16).map_err(|_| Error::Protocol("bad chunk size".into()))?;
        b = &b[nl + 2..];
        if size == 0 {
            return Ok(out);
        }
        if size > b.len() || out.len() + size > MAX_BODY {
            return Err(Error::Protocol("truncated chunk".into()));
        }
        out.extend_from_slice(&b[..size]);
        b = b.get(size + 2..).unwrap_or(&[]); // skip data + trailing CRLF
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_content_length() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello world";
        let r = parse_response(raw).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"hello");
    }

    #[test]
    fn parses_chunked() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let r = parse_response(raw).unwrap();
        assert_eq!(r.body, b"hello world");
    }

    #[test]
    fn reports_status() {
        let raw = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
        assert_eq!(parse_response(raw).unwrap().status, 404);
    }
}
