//! Per-process GPU usage from DRM fdinfo (`/proc/<pid>/fdinfo/<fd>`).
//!
//! GPU fds are found by the shared [`crate::fdwalk::FdWalker`]; each tick only
//! the fds already known to be GPU clients are re-read.

use std::collections::HashMap;
use std::time::Instant;

use super::ENGINES;
use crate::parse;
use crate::sys::{CPath, Dir};

/// A client whose engine counters haven't moved for this many reads is idle...
const IDLE_AFTER: u32 = 3;
/// ...and is then only re-read every this many ticks.
const IDLE_EVERY: u64 = 5;

#[derive(Clone, Copy, Default, Debug)]
pub struct ProcGpu {
    /// Utilisation of the busiest engine, percent of the whole GPU.
    pub busy: f32,
    pub vram: u64,
    pub gtt: u64,
}

/// A GPU fd we read each tick. Duplicate fds of one client are dropped.
struct Watch {
    pid: u32,
    fd: u32,
    key: Option<(u16, u32)>,
}

struct Client {
    /// Cumulative busy time per engine class, in ns, at `at`.
    engines: [u64; ENGINES.len()],
    at: f64,
    idle: u32,
    vram: u64,
    gtt: u64,
    last_tick: u64,
}

pub struct FdinfoScanner {
    watches: Vec<Watch>,
    clients: HashMap<(u16, u32), Client>,
    /// Interned `drm-pdev` values; index is the device id used in keys.
    devices: Vec<Box<[u8]>>,
    seen: Vec<(u16, u32)>,
    tick: u64,
    epoch: Instant,
    /// Per-process results for the latest tick.
    pub usage: HashMap<u32, ProcGpu>,
    /// Per-device engine utilisation, parallel to `devices`.
    pub engine_totals: Vec<[f32; ENGINES.len()]>,
}

impl FdinfoScanner {
    pub fn new() -> Self {
        Self {
            watches: Vec::new(),
            clients: HashMap::new(),
            devices: Vec::new(),
            seen: Vec::new(),
            tick: 0,
            epoch: Instant::now(),
            usage: HashMap::new(),
            engine_totals: Vec::new(),
        }
    }

    /// Engine totals for the device at `pci_slot`, if any client used it.
    pub fn engines_for(&self, pci_slot: &str) -> Option<[f32; ENGINES.len()]> {
        let i = self.devices.iter().position(|d| &**d == pci_slot.as_bytes())?;
        self.engine_totals.get(i).copied()
    }

    /// Adds GPU fds found by the shared fd walker.
    pub fn add_candidates(&mut self, fds: &[(u32, u32)]) {
        for &(pid, fd) in fds {
            if !self.watches.iter().any(|w| w.pid == pid && w.fd == fd) {
                self.watches.push(Watch { pid, fd, key: None });
            }
        }
    }

    /// Pids with GPU fds (rescanned often by the fd walker to catch new ones).
    pub fn gpu_pids(&self) -> impl Iterator<Item = u32> + '_ {
        self.watches.iter().map(|w| w.pid)
    }

    pub fn sample(&mut self, proc: &Dir, buf: &mut Vec<u8>) {
        self.tick += 1;
        let tick = self.tick;
        let now = self.epoch.elapsed().as_secs_f64();

        self.usage.clear();
        self.seen.clear();
        for t in &mut self.engine_totals {
            *t = [0.0; ENGINES.len()];
        }

        for w in &mut self.watches {
            // Idle clients: reuse their memory figures, skip the read.
            if tick % IDLE_EVERY != 0
                && let Some(key) = w.key
                && let Some(c) = self.clients.get_mut(&key)
                && c.idle >= IDLE_AFTER
            {
                c.last_tick = tick;
                self.seen.push(key);
                let u = self.usage.entry(w.pid).or_default();
                u.vram += c.vram;
                u.gtt += c.gtt;
                continue;
            }

            let mut p = CPath::pid(w.pid, b"/fdinfo/");
            p.push_u32(w.fd);
            if proc.read(p.as_cstr(), buf).is_err() {
                w.pid = 0; // fd or process gone
                continue;
            }
            let Some(info) = parse_fdinfo(buf, &mut self.devices) else {
                w.pid = 0;
                continue;
            };
            let key = (info.device, info.client_id);
            if self.seen.contains(&key) {
                w.pid = 0; // another fd for a client we already read
                continue;
            }
            w.key = Some(key);
            self.seen.push(key);
            if self.engine_totals.len() < self.devices.len() {
                self.engine_totals.resize(self.devices.len(), [0.0; ENGINES.len()]);
            }

            let c = self.clients.entry(key).or_insert(Client {
                engines: info.engines,
                at: now,
                idle: 0,
                vram: 0,
                gtt: 0,
                last_tick: 0,
            });
            let dt_ns = (now - c.at) * 1e9;
            let mut busiest = 0f32;
            let mut active = false;
            if dt_ns > 0.0 {
                for (e, (&cur, &before)) in info.engines.iter().zip(&c.engines).enumerate() {
                    let delta = cur.saturating_sub(before);
                    active |= delta > 0;
                    let pct = (delta as f64 / dt_ns * 100.0).min(100.0) as f32;
                    busiest = busiest.max(pct);
                    self.engine_totals[info.device as usize][e] += pct;
                }
                c.idle = if active { 0 } else { c.idle.saturating_add(1) };
            }
            c.engines = info.engines;
            c.at = now;
            c.vram = info.vram;
            c.gtt = info.gtt;
            c.last_tick = tick;

            let u = self.usage.entry(w.pid).or_default();
            u.busy = (u.busy + busiest).min(100.0);
            u.vram += info.vram;
            u.gtt += info.gtt;
        }

        self.watches.retain(|w| w.pid != 0);
        self.clients.retain(|_, c| c.last_tick == tick);
        for t in &mut self.engine_totals {
            for v in t {
                *v = v.min(100.0);
            }
        }
    }
}

impl Default for FdinfoScanner {
    fn default() -> Self {
        Self::new()
    }
}

struct FdInfo {
    device: u16,
    client_id: u32,
    engines: [u64; ENGINES.len()],
    vram: u64,
    gtt: u64,
}

fn engine_class(name: &[u8]) -> usize {
    match name {
        b"gfx" | b"render" | b"rcs" | b"3d" => 0,
        b"compute" | b"ccs" => 1,
        b"dma" | b"copy" | b"bcs" | b"sdma" => 2,
        n if n.starts_with(b"dec") || n.starts_with(b"jpeg") || n.starts_with(b"video") || n == b"vcs" => 3,
        n if n.starts_with(b"enc") || n == b"vecs" => 4,
        _ => 0,
    }
}

fn parse_fdinfo(buf: &[u8], devices: &mut Vec<Box<[u8]>>) -> Option<FdInfo> {
    let mut info = FdInfo { device: 0, client_id: 0, engines: [0; ENGINES.len()], vram: 0, gtt: 0 };
    let mut has_client = false;
    let (mut resident_vram, mut resident_gtt) = (None, None);
    let (mut legacy_vram, mut legacy_gtt) = (0, 0);
    for line in parse::lines(buf) {
        let Some(key) = line.strip_prefix(b"drm-") else { continue };
        let Some((k, v)) = parse::key_value(key) else { continue };
        match k {
            b"client-id" => {
                info.client_id = parse::u64(v) as u32;
                has_client = true;
            }
            b"pdev" => {
                info.device = match devices.iter().position(|d| &**d == v) {
                    Some(i) => i as u16,
                    None => {
                        devices.push(v.into());
                        (devices.len() - 1) as u16
                    }
                };
            }
            b"resident-vram" => resident_vram = Some(parse::size_with_unit(v)),
            b"resident-gtt" => resident_gtt = Some(parse::size_with_unit(v)),
            b"memory-vram" => legacy_vram = parse::size_with_unit(v),
            b"memory-gtt" => legacy_gtt = parse::size_with_unit(v),
            _ => {
                if let Some(engine) = k.strip_prefix(b"engine-") {
                    info.engines[engine_class(engine)] += parse::u64(v);
                }
            }
        }
    }
    info.vram = resident_vram.unwrap_or(legacy_vram);
    info.gtt = resident_gtt.unwrap_or(legacy_gtt);
    has_client.then_some(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_amdgpu_fdinfo() {
        let text = b"pos:\t0\ndrm-driver:\tamdgpu\ndrm-client-id:\t373\ndrm-pdev:\t0000:0c:00.0\n\
drm-resident-vram:\t114844 KiB\ndrm-memory-vram:\t114844 KiB\ndrm-memory-gtt: \t2048 KiB\n\
drm-engine-gfx:\t82307202822 ns\ndrm-engine-compute:\t96195336 ns\n";
        let mut devices = Vec::new();
        let info = parse_fdinfo(text, &mut devices).unwrap();
        assert_eq!(info.client_id, 373);
        assert_eq!(info.vram, 114844 * 1024);
        assert_eq!(info.gtt, 2048 * 1024);
        assert_eq!(info.engines[0], 82307202822);
        assert_eq!(info.engines[1], 96195336);
        assert_eq!(&*devices[0], b"0000:0c:00.0");
    }
}
