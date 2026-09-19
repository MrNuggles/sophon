//! The tiny HTTP/1.1 + JSON layer shared by Sophon's API clients
//! ([`sophon-docker`], [`sophon-npm`]). Hand-rolled and dependency-free:
//! blocking HTTP with `Content-Length`/chunked/read-to-close support, and a
//! small JSON parser. Plain HTTP only — the API endpoints Sophon talks to live
//! on the trusted LAN (see SECURITY.md).
//!
//! [`sophon-docker`]: ../sophon_docker/index.html
pub mod http;
pub mod json;
