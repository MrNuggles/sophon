//! CPU: total and per-thread usage, clocks, temperature and package power.

use std::sync::Arc;

use crate::history::Ring;
use crate::parse;
use crate::sys::{self, CPath, Dir};

#[derive(Clone, Copy, Default)]
struct Times {
    busy: u64,
    total: u64,
}

impl Times {
    /// Parses a `cpu`/`cpuN` line of `/proc/stat`.
    fn parse(line: &[u8]) -> Self {
        let mut it = parse::fields(line).skip(1).map(parse::u64);
        // user nice system idle iowait irq softirq steal (guest time is already in user)
        let v: [u64; 8] = std::array::from_fn(|_| it.next().unwrap_or(0));
        let total: u64 = v.iter().sum();
        Self { busy: total - v[3] - v[4], total }
    }

    fn usage_since(&self, prev: &Times) -> f32 {
        let total = self.total.saturating_sub(prev.total);
        if total == 0 {
            return 0.0;
        }
        (self.busy.saturating_sub(prev.busy) as f32 / total as f32 * 100.0).clamp(0.0, 100.0)
    }
}

#[derive(Clone, Default)]
pub struct CpuState {
    pub model: Arc<str>,
    pub cores: usize,
    pub threads: usize,
    /// Physical core of each thread (index = thread), for per-core views.
    pub core_of: Vec<u32>,
    pub usage: f32,
    pub history: Ring,
    pub per_thread: Vec<f32>,
    pub per_thread_history: Vec<Ring>,
    pub freq_mhz: Vec<u32>,
    pub avg_freq_mhz: u32,
    pub max_freq_mhz: u32,
    pub temp_c: Option<f32>,
    pub power_w: Option<f32>,
    pub power_history: Ring,
    pub ctx_switches_per_s: u64,
    pub procs_running: u32,
    pub procs_blocked: u32,
    pub uptime_s: u64,
}

pub struct CpuCollector {
    pub state: CpuState,
    prev: Times,
    prev_threads: Vec<Times>,
    prev_ctxt: u64,
    sys_cpu: Option<Dir>,
    temp: Option<Dir>,
    rapl: Option<Dir>,
    prev_energy_uj: Option<u64>,
    energy_range_uj: u64,
}

impl CpuCollector {
    pub fn new() -> Self {
        let mut state = CpuState::default();
        let mut buf = Vec::new();
        let root = Dir::open(c"/").ok();

        if let Some(root) = &root
            && root.read(c"proc/cpuinfo", &mut buf).is_ok()
        {
            for line in parse::lines(&buf) {
                match parse::key_value(line) {
                    Some((b"model name", v)) if state.model.is_empty() => {
                        state.model = String::from_utf8_lossy(v).trim().into();
                    }
                    Some((b"cpu cores", v)) if state.cores == 0 => state.cores = parse::u64(v) as usize,
                    _ => {}
                }
            }
        }
        state.threads = std::thread::available_parallelism().map_or(1, |n| n.get());
        if let Some(root) = &root
            && root.read(c"proc/stat", &mut buf).is_ok()
        {
            let n = parse::lines(&buf)
                .filter(|l| l.starts_with(b"cpu") && l.get(3).is_some_and(u8::is_ascii_digit))
                .count();
            if n > 0 {
                state.threads = n;
            }
        }
        if state.cores == 0 {
            state.cores = state.threads;
        }
        state.per_thread = vec![0.0; state.threads];
        state.per_thread_history = vec![Ring::default(); state.threads];
        state.freq_mhz = vec![0; state.threads];

        let sys_cpu = Dir::open(c"/sys/devices/system/cpu").ok();
        state.core_of = (0..state.threads)
            .map(|i| {
                let mut p = CPath::from(b"cpu");
                p.push_u32(i as u32).push(b"/topology/core_id");
                sys_cpu.as_ref().and_then(|d| d.read_u64(p.as_cstr(), &mut buf)).map_or(i as u32, |c| c as u32)
            })
            .collect();
        if let Some(d) = &sys_cpu {
            state.max_freq_mhz = (d.read_u64(c"cpu0/cpufreq/cpuinfo_max_freq", &mut buf).unwrap_or(0) / 1000) as u32;
        }

        // Package energy counter; many distros restrict it to root, so it's optional.
        let rapl = Dir::open(c"/sys/class/powercap/intel-rapl:0").ok();
        let energy_range_uj = rapl
            .as_ref()
            .and_then(|d| d.read_u64(c"max_energy_range_uj", &mut buf))
            .unwrap_or(u64::MAX);
        let rapl = rapl.filter(|d| d.read_u64(c"energy_uj", &mut buf).is_some());

        Self {
            state,
            prev: Times::default(),
            prev_threads: Vec::new(),
            prev_ctxt: 0,
            sys_cpu,
            temp: sys::find_hwmon(&[b"k10temp", b"zenpower", b"coretemp", b"cpu_thermal"]),
            rapl,
            prev_energy_uj: None,
            energy_range_uj,
        }
    }

    pub fn sample(&mut self, proc: &Dir, buf: &mut Vec<u8>, dt: f64) {
        let s = &mut self.state;
        if proc.read(c"stat", buf).is_ok() {
            let first = self.prev_threads.is_empty();
            if first {
                self.prev_threads = vec![Times::default(); s.threads];
            }
            let mut ctxt = 0;
            for line in parse::lines(buf) {
                if let Some(rest) = line.strip_prefix(b"cpu") {
                    let t = Times::parse(line);
                    if rest.first() == Some(&b' ') {
                        s.usage = t.usage_since(&self.prev);
                        self.prev = t;
                    } else {
                        let i = parse::u64(rest) as usize;
                        if i < s.threads {
                            s.per_thread[i] = t.usage_since(&self.prev_threads[i]);
                            self.prev_threads[i] = t;
                        }
                    }
                } else if let Some(v) = line.strip_prefix(b"ctxt ") {
                    ctxt = parse::u64(v);
                } else if let Some(v) = line.strip_prefix(b"procs_running ") {
                    s.procs_running = parse::u64(v) as u32;
                } else if let Some(v) = line.strip_prefix(b"procs_blocked ") {
                    s.procs_blocked = parse::u64(v) as u32;
                }
            }
            if !first && dt > 0.0 {
                s.ctx_switches_per_s = (ctxt.saturating_sub(self.prev_ctxt) as f64 / dt) as u64;
                s.history.push(s.usage);
                for (h, &u) in s.per_thread_history.iter_mut().zip(&s.per_thread) {
                    h.push(u);
                }
            }
            self.prev_ctxt = ctxt;
        }

        if let Some(d) = &self.sys_cpu {
            let mut sum = 0u64;
            for i in 0..s.threads {
                let mut p = CPath::from(b"cpu");
                p.push_u32(i as u32).push(b"/cpufreq/scaling_cur_freq");
                let mhz = d.read_u64(p.as_cstr(), buf).unwrap_or(0) / 1000;
                s.freq_mhz[i] = mhz as u32;
                sum += mhz;
            }
            s.avg_freq_mhz = (sum / s.threads.max(1) as u64) as u32;
        }

        s.temp_c = self
            .temp
            .as_ref()
            .and_then(|d| d.read_u64(c"temp1_input", buf))
            .map(|m| m as f32 / 1000.0);

        if let Some(d) = &self.rapl
            && let Some(e) = d.read_u64(c"energy_uj", buf)
        {
            if let Some(prev) = self.prev_energy_uj
                && dt > 0.0
            {
                let delta = if e >= prev { e - prev } else { self.energy_range_uj - prev + e };
                let w = (delta as f64 / 1e6 / dt) as f32;
                s.power_w = Some(w);
                s.power_history.push(w);
            }
            self.prev_energy_uj = Some(e);
        }

        if proc.read(c"uptime", buf).is_ok() {
            s.uptime_s = parse::u64(buf);
        }
    }
}

impl Default for CpuCollector {
    fn default() -> Self {
        Self::new()
    }
}
