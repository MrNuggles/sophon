//! Memory, swap and zram.

use crate::history::Ring;
use crate::parse;
use crate::sys::{CPath, Dir};

#[derive(Clone, Copy, Default)]
pub struct Zram {
    /// Uncompressed size of the data stored.
    pub original: u64,
    /// RAM actually used to hold it.
    pub used: u64,
}

#[derive(Clone, Default)]
pub struct MemState {
    pub total: u64,
    pub available: u64,
    pub used: u64,
    pub free: u64,
    pub cached: u64,
    pub buffers: u64,
    pub shmem: u64,
    pub dirty: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    pub committed: u64,
    pub commit_limit: u64,
    pub zram: Option<Zram>,
    /// Percent of RAM in use.
    pub history: Ring,
    pub swap_history: Ring,
}

pub struct MemCollector {
    pub state: MemState,
    sys_block: Option<Dir>,
    zram_devices: Vec<CPath>,
}

impl MemCollector {
    pub fn new() -> Self {
        let sys_block = Dir::open(c"/sys/block").ok();
        let mut zram_devices = Vec::new();
        if let Some(d) = &sys_block
            && let Ok(mut entries) = d.entries()
        {
            while let Some(name) = entries.next_name() {
                if name.starts_with(b"zram") {
                    let mut p = CPath::from(name);
                    p.push(b"/mm_stat");
                    zram_devices.push(p);
                }
            }
        }
        Self { state: MemState::default(), sys_block, zram_devices }
    }

    pub fn sample(&mut self, proc: &Dir, buf: &mut Vec<u8>) {
        let s = &mut self.state;
        if proc.read(c"meminfo", buf).is_ok() {
            let (mut swap_free, mut sreclaimable) = (0, 0);
            for line in parse::lines(buf) {
                let Some((k, v)) = parse::key_value(line) else { continue };
                let v = parse::u64(v) * 1024;
                match k {
                    b"MemTotal" => s.total = v,
                    b"MemFree" => s.free = v,
                    b"MemAvailable" => s.available = v,
                    b"Buffers" => s.buffers = v,
                    b"Cached" => s.cached = v,
                    b"SReclaimable" => sreclaimable = v,
                    b"Shmem" => s.shmem = v,
                    b"Dirty" => s.dirty = v,
                    b"SwapTotal" => s.swap_total = v,
                    b"SwapFree" => swap_free = v,
                    b"Committed_AS" => s.committed = v,
                    b"CommitLimit" => s.commit_limit = v,
                    _ => {}
                }
            }
            s.cached += sreclaimable;
            s.used = s.total.saturating_sub(s.available);
            s.swap_used = s.swap_total.saturating_sub(swap_free);
            s.history.push(pct(s.used, s.total));
            s.swap_history.push(pct(s.swap_used, s.swap_total));
        }

        if let Some(d) = &self.sys_block
            && !self.zram_devices.is_empty()
        {
            let mut z = Zram::default();
            for p in &self.zram_devices {
                if d.read(p.as_cstr(), buf).is_ok() {
                    // orig_data_size compr_data_size mem_used_total ...
                    let mut f = parse::fields(buf).map(parse::u64);
                    z.original += f.next().unwrap_or(0);
                    f.next();
                    z.used += f.next().unwrap_or(0);
                }
            }
            s.zram = Some(z);
        }
    }
}

impl Default for MemCollector {
    fn default() -> Self {
        Self::new()
    }
}

pub fn pct(part: u64, whole: u64) -> f32 {
    if whole == 0 { 0.0 } else { (part as f64 / whole as f64 * 100.0) as f32 }
}
