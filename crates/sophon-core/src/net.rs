//! Network interfaces: throughput per interface from `/proc/net/dev`.

use std::sync::Arc;

use crate::history::Ring;
use crate::parse;
use crate::sys::{CPath, Dir};

#[derive(Clone, Default)]
pub struct NetIface {
    pub name: Arc<str>,
    /// Backed by real hardware (not a bridge, veth, tun, ...).
    pub physical: bool,
    pub wireless: bool,
    pub up: bool,
    pub speed_mbps: Option<u32>,
    pub rx_bps: f64,
    pub tx_bps: f64,
    /// Bytes since boot.
    pub rx_total: u64,
    pub tx_total: u64,
    pub rx_history: Ring,
    pub tx_history: Ring,
}

struct Track {
    iface: NetIface,
    seen: bool,
}

pub struct NetCollector {
    tracks: Vec<Track>,
    sys: Option<Dir>,
}

impl NetCollector {
    pub fn new() -> Self {
        Self { tracks: Vec::new(), sys: Dir::open(c"/sys/class/net").ok() }
    }

    pub fn ifaces(&self) -> impl Iterator<Item = &NetIface> {
        self.tracks.iter().map(|t| &t.iface)
    }

    pub fn sample(&mut self, proc: &Dir, buf: &mut Vec<u8>, dt: f64) {
        if proc.read(c"net/dev", buf).is_err() {
            return;
        }
        for t in &mut self.tracks {
            t.seen = false;
        }
        // Owned copy only when an interface first appears.
        let mut state_buf = Vec::new();
        for line in parse::lines(buf).skip(2) {
            let Some(colon) = line.iter().position(|&b| b == b':') else { continue };
            let name = parse::trim(&line[..colon]);
            if name == b"lo" {
                continue;
            }
            let mut f = parse::fields(&line[colon + 1..]).map(parse::u64);
            let rx = f.next().unwrap_or(0);
            let tx = f.nth(7).unwrap_or(0);

            let idx = match self.tracks.iter().position(|t| t.iface.name.as_bytes() == name) {
                Some(i) => i,
                None => {
                    let mut iface = NetIface {
                        name: String::from_utf8_lossy(name).into(),
                        rx_total: rx,
                        tx_total: tx,
                        ..Default::default()
                    };
                    if let Some(sys) = &self.sys {
                        let mut p = CPath::from(name);
                        p.push(b"/device");
                        iface.physical = sys.exists(p.as_cstr());
                        let mut p = CPath::from(name);
                        p.push(b"/wireless");
                        iface.wireless = sys.exists(p.as_cstr());
                    }
                    self.tracks.push(Track { iface, seen: true });
                    self.tracks.len() - 1
                }
            };
            let t = &mut self.tracks[idx];
            t.seen = true;
            let i = &mut t.iface;
            if dt > 0.0 {
                i.rx_bps = rx.saturating_sub(i.rx_total) as f64 / dt;
                i.tx_bps = tx.saturating_sub(i.tx_total) as f64 / dt;
            }
            i.rx_total = rx;
            i.tx_total = tx;
            i.rx_history.push(i.rx_bps as f32);
            i.tx_history.push(i.tx_bps as f32);

            if let Some(sys) = &self.sys
                && i.physical
            {
                let mut p = CPath::from(name);
                p.push(b"/operstate");
                i.up = sys.read(p.as_cstr(), &mut state_buf).is_ok() && parse::trim(&state_buf) == b"up";
                let mut p = CPath::from(name);
                p.push(b"/speed");
                i.speed_mbps = if i.up {
                    sys.read_u64(p.as_cstr(), &mut state_buf).filter(|&s| s > 0).map(|s| s as u32)
                } else {
                    None
                };
            }
        }
        self.tracks.retain(|t| t.seen);
    }
}

impl Default for NetCollector {
    fn default() -> Self {
        Self::new()
    }
}
