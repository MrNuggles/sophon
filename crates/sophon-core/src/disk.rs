//! Block devices: throughput, active time and response time from `/proc/diskstats`.

use std::sync::Arc;

use crate::history::Ring;
use crate::parse;
use crate::sys::{self, CPath, Dir};

#[derive(Clone, Default)]
pub struct DiskState {
    pub name: Arc<str>,
    pub model: Arc<str>,
    pub size: u64,
    pub rotational: bool,
    pub removable: bool,
    pub read_bps: f64,
    pub write_bps: f64,
    /// Percent of the interval the device had I/O in flight (Windows' "Active time").
    pub active_pct: f32,
    /// Average time per completed request, in milliseconds.
    pub response_ms: f32,
    pub read_total: u64,
    pub write_total: u64,
    pub temp_c: Option<f32>,
    pub read_history: Ring,
    pub write_history: Ring,
    pub active_history: Ring,
}

struct Track {
    state: DiskState,
    hwmon: Option<Dir>,
    prev: [u64; 3],
    primed: bool,
    seen: bool,
}

pub struct DiskCollector {
    tracks: Vec<Track>,
    /// Names in diskstats we decided not to show (partitions, loop, zram, ...).
    ignored: Vec<Box<[u8]>>,
    sys_block: Option<Dir>,
    tick: u64,
}

/// Drive temperature reads can issue a SMART command to the drive, so they are rare.
const TEMP_EVERY: u64 = 30;

impl DiskCollector {
    pub fn new() -> Self {
        Self { tracks: Vec::new(), ignored: Vec::new(), sys_block: Dir::open(c"/sys/block").ok(), tick: 0 }
    }

    pub fn disks(&self) -> impl Iterator<Item = &DiskState> {
        self.tracks.iter().map(|t| &t.state)
    }

    pub fn sample(&mut self, proc: &Dir, buf: &mut Vec<u8>, dt: f64) {
        if proc.read(c"diskstats", buf).is_err() {
            return;
        }
        for t in &mut self.tracks {
            t.seen = false;
        }
        let mut scratch = Vec::new();
        self.tick += 1;
        let read_temps = self.tick % TEMP_EVERY == 1;
        for line in parse::lines(buf) {
            let mut f = parse::fields(line);
            let (Some(_), Some(_), Some(name)) = (f.next(), f.next(), f.next()) else { continue };
            if self.ignored.iter().any(|n| &**n == name) {
                continue;
            }
            let v: [u64; 11] = std::array::from_fn(|_| f.next().map(parse::u64).unwrap_or(0));
            // reads, sectors read, ms reading, writes, sectors written, ms writing, ms doing I/O
            let (reads, sect_r, ms_r, writes, sect_w, ms_w, ms_io) = (v[0], v[2], v[3], v[4], v[6], v[7], v[9]);

            let idx = match self.tracks.iter().position(|t| t.state.name.as_bytes() == name) {
                Some(i) => i,
                None => match self.discover(name, &mut scratch) {
                    Some(track) => {
                        self.tracks.push(track);
                        self.tracks.len() - 1
                    }
                    None => {
                        self.ignored.push(name.into());
                        continue;
                    }
                },
            };
            let t = &mut self.tracks[idx];
            let s = &mut t.state;
            t.seen = true;
            let (read_b, write_b) = (sect_r * 512, sect_w * 512);
            if t.primed && dt > 0.0 {
                s.read_bps = read_b.saturating_sub(s.read_total) as f64 / dt;
                s.write_bps = write_b.saturating_sub(s.write_total) as f64 / dt;
                s.active_pct = (ms_io.saturating_sub(t.prev[0]) as f64 / (dt * 10.0)).min(100.0) as f32;
                let ios = (reads + writes).saturating_sub(t.prev[1]);
                let ms = (ms_r + ms_w).saturating_sub(t.prev[2]);
                s.response_ms = if ios > 0 { ms as f32 / ios as f32 } else { 0.0 };
                s.read_history.push(s.read_bps as f32);
                s.write_history.push(s.write_bps as f32);
                s.active_history.push(s.active_pct);
            }
            s.read_total = read_b;
            s.write_total = write_b;
            t.prev = [ms_io, reads + writes, ms_r + ms_w];
            t.primed = true;
            if read_temps {
                s.temp_c = t
                .hwmon
                .as_ref()
                .and_then(|h| h.read_u64(c"temp1_input", &mut scratch))
                .map(|m| m as f32 / 1000.0);
            }
        }
        self.tracks.retain(|t| t.seen);
    }

    /// Whole physical disks only; returns `None` for partitions and virtual devices.
    fn discover(&self, name: &[u8], scratch: &mut Vec<u8>) -> Option<Track> {
        const SKIP: [&[u8]; 5] = [b"loop", b"ram", b"zram", b"dm-", b"sr"];
        if SKIP.iter().any(|p| name.starts_with(p)) {
            return None;
        }
        let sys = self.sys_block.as_ref()?;
        let dir = sys.open_at(CPath::from(name).as_cstr()).ok()?;
        let size = dir.read_u64(c"size", scratch)? * 512;
        if size == 0 {
            return None;
        }
        let model = sys::read_string(&dir, c"device/model")
            .or_else(|| sys::read_string(&dir, c"device/name"))
            .unwrap_or_default();
        let state = DiskState {
            name: String::from_utf8_lossy(name).into(),
            model: model.into(),
            size,
            rotational: dir.read_u64(c"queue/rotational", scratch) == Some(1),
            removable: dir.read_u64(c"removable", scratch) == Some(1),
            ..Default::default()
        };
        Some(Track { state, hwmon: sys::device_hwmon(&dir, b"device"), prev: [0; 3], primed: false, seen: false })
    }
}

impl Default for DiskCollector {
    fn default() -> Self {
        Self::new()
    }
}
