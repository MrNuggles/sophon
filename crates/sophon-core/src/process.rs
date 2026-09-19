//! The process table: one pass over `/proc` per refresh.
//!
//! Per process and per refresh this reads `stat` and `statm` only. Everything
//! that is fixed for a process's lifetime (exe, cmdline, owner, cgroup,
//! pretty name) is read once, when the pid is first seen (or re-execs).

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::time::Instant;

use crate::apps::{self, AppIndex, Kind};
use crate::gpu::fdinfo::ProcGpu;
use crate::history::Ring;
use crate::parse;
use crate::sys::{self, CPath, Dir, DirStream};

/// Per-process CPU history length, for row sparklines.
pub const SPARK: usize = 60;

/// `PF_KTHREAD` in `/proc/<pid>/stat` flags.
const PF_KTHREAD: u64 = 0x0020_0000;

#[derive(Clone)]
pub struct Process {
    pub pid: u32,
    pub ppid: u32,
    pub uid: u32,
    pub user: Arc<str>,
    /// Kernel `comm` (process name, max 15 chars).
    pub name: Arc<str>,
    pub exe: Option<Arc<str>>,
    pub cmdline: Arc<str>,
    /// Application or friendly name, if known.
    pub pretty: Option<Arc<str>>,
    pub icon: Option<Arc<str>>,
    /// systemd unit (cgroup leaf), e.g. `app-steam@autostart.service`.
    pub unit: Arc<str>,
    /// Display name of the app/unit this process belongs to.
    pub group: Arc<str>,
    pub kind: Kind,
    pub kernel_thread: bool,
    pub state: u8,
    pub threads: u32,
    /// Seconds since boot at process start.
    pub started_s: u64,
    /// Percent of the whole machine (all threads = 100%).
    pub cpu: f32,
    pub cpu_history: Ring<SPARK>,
    /// Anonymous resident memory: closest to Windows' "private working set".
    pub mem_private: u64,
    pub mem_rss: u64,
    pub gpu: f32,
    pub gpu_mem: u64,
    /// TCP throughput, bytes per second.
    pub net_rx: f64,
    pub net_tx: f64,
}

impl Process {
    pub fn display_name(&self) -> &str {
        self.pretty.as_deref().unwrap_or(&self.name)
    }
}

struct Entry {
    p: Process,
    start_ticks: u64,
    prev_cpu_ticks: u64,
    /// When `prev_cpu_ticks` was taken (seconds since the table was created).
    prev_at: f64,
    primed: bool,
    /// Resident pages (from `stat`) when `statm` was last read.
    statm_rss: u64,
    /// Consecutive refreshes with no CPU time used.
    idle: u32,
    seen: u64,
}

/// Kernel threads are re-read only every this many refreshes (hidden by default, rarely interesting).
const KTHREAD_EVERY: u64 = 5;
/// A process idle for this many refreshes...
const IDLE_AFTER: u32 = 3;
/// ...is re-read only every this many (staggered by pid). A wake-up shows within this many ticks.
const IDLE_EVERY: u64 = 3;

pub struct ProcessTable {
    entries: HashMap<u32, Entry>,
    stream: DirStream,
    pids: Vec<u32>,
    users: HashMap<u32, Arc<str>>,
    apps: AppIndex,
    tick: u64,
    epoch: Instant,
    clk_tck: f64,
    page: u64,
    threads: f64,
    scratch: Vec<u8>,
}

impl ProcessTable {
    pub fn new(proc: &Dir, apps: AppIndex) -> io::Result<Self> {
        Ok(Self {
            entries: HashMap::with_capacity(1024),
            stream: proc.entries()?,
            pids: Vec::with_capacity(1024),
            users: load_users(),
            apps,
            tick: 0,
            epoch: Instant::now(),
            clk_tck: sys::clk_tck() as f64,
            page: sys::page_size(),
            threads: std::thread::available_parallelism().map_or(1, |n| n.get()) as f64,
            scratch: Vec::with_capacity(4096),
        })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Process> {
        self.entries.values().map(|e| &e.p)
    }

    /// Pids of user-space processes (candidates for GPU fd discovery).
    pub fn user_pids(&self, out: &mut Vec<u32>) {
        out.clear();
        out.extend(self.entries.values().filter(|e| !e.p.kernel_thread).map(|e| e.p.pid));
        out.sort_unstable();
    }

    pub fn apply_gpu(&mut self, usage: &HashMap<u32, ProcGpu>) {
        for e in self.entries.values_mut() {
            let u = usage.get(&e.p.pid).copied().unwrap_or_default();
            e.p.gpu = u.busy;
            e.p.gpu_mem = u.vram + u.gtt;
        }
    }

    pub fn apply_net(&mut self, usage: &HashMap<u32, crate::netproc::ProcNet>) {
        for e in self.entries.values_mut() {
            let u = usage.get(&e.p.pid).copied().unwrap_or_default();
            e.p.net_rx = u.rx_bps;
            e.p.net_tx = u.tx_bps;
        }
    }

    pub fn refresh(&mut self, proc: &Dir, buf: &mut Vec<u8>) {
        let now = self.epoch.elapsed().as_secs_f64();
        self.tick += 1;
        let tick = self.tick;
        let kthreads_due = tick % KTHREAD_EVERY == 1;

        let pids = &mut self.pids;
        pids.clear();
        self.stream.for_each_number(|pid| pids.push(pid));

        for i in 0..self.pids.len() {
            let pid = self.pids[i];
            if let Some(e) = self.entries.get_mut(&pid) {
                let skip = if e.p.kernel_thread {
                    !kthreads_due
                } else {
                    e.idle >= IDLE_AFTER && (tick + pid as u64) % IDLE_EVERY != 0
                };
                if skip {
                    e.seen = tick;
                    e.p.cpu_history.push(0.0);
                    continue;
                }
            }
            if proc.read(CPath::pid(pid, b"/stat").as_cstr(), buf).is_err() {
                continue; // exited between readdir and read
            }
            let Some(stat) = Stat::parse(buf) else { continue };

            let reuse = self
                .entries
                .get(&pid)
                .is_some_and(|e| e.start_ticks == stat.start_ticks && *e.p.name == *stat.comm_str());
            if !reuse {
                let p = self.new_process(proc, pid, &stat);
                self.entries.insert(
                    pid,
                    Entry {
                        p,
                        start_ticks: stat.start_ticks,
                        prev_cpu_ticks: 0,
                        prev_at: now,
                        primed: false,
                        statm_rss: u64::MAX,
                        idle: 0,
                        seen: 0,
                    },
                );
            }
            let e = self.entries.get_mut(&pid).expect("inserted above");
            e.seen = tick;
            let p = &mut e.p;
            p.ppid = stat.ppid;
            p.state = stat.state;
            p.threads = stat.threads;

            let cpu_ticks = stat.utime + stat.stime;
            let dt = now - e.prev_at;
            p.cpu = if e.primed && dt > 0.0 {
                let used = cpu_ticks.saturating_sub(e.prev_cpu_ticks) as f64 / self.clk_tck;
                (used / dt / self.threads * 100.0).clamp(0.0, 100.0) as f32
            } else {
                0.0
            };
            e.idle = if cpu_ticks == e.prev_cpu_ticks { e.idle.saturating_add(1) } else { 0 };
            e.prev_cpu_ticks = cpu_ticks;
            e.prev_at = now;
            e.primed = true;
            p.cpu_history.push(p.cpu);

            p.mem_rss = stat.rss_pages * self.page;
            // Private memory needs statm; skip it while the resident size is unchanged.
            if !p.kernel_thread
                && stat.rss_pages != e.statm_rss
                && proc.read(CPath::pid(pid, b"/statm").as_cstr(), &mut self.scratch).is_ok()
            {
                // size resident shared text lib data dt (pages)
                let mut f = parse::fields(&self.scratch).map(parse::u64).skip(1);
                let resident = f.next().unwrap_or(0);
                let shared = f.next().unwrap_or(0);
                p.mem_private = resident.saturating_sub(shared) * self.page;
                e.statm_rss = stat.rss_pages;
            }
        }
        self.entries.retain(|_, e| e.seen == tick);
    }

    fn new_process(&mut self, proc: &Dir, pid: u32, stat: &Stat) -> Process {
        let buf = &mut self.scratch;
        let name: Arc<str> = stat.comm_str().into();
        let kernel_thread = stat.flags & PF_KTHREAD != 0;
        let uid = proc.owner(CPath::pid(pid, b"").as_cstr()).unwrap_or(0);
        let user = self.users.get(&uid).cloned().unwrap_or_else(|| uid.to_string().into());

        let (mut exe, mut cmdline, mut unit, mut cgroup) = (None, Arc::from(""), Arc::from(""), String::new());
        if !kernel_thread {
            if proc.read_link(CPath::pid(pid, b"/exe").as_cstr(), buf).is_ok() {
                let s = String::from_utf8_lossy(buf);
                exe = Some(Arc::from(s.trim_end_matches(" (deleted)")));
            }
            if proc.read(CPath::pid(pid, b"/cmdline").as_cstr(), buf).is_ok() {
                buf.truncate(4096);
                for b in buf.iter_mut() {
                    if *b == 0 {
                        *b = b' ';
                    }
                }
                cmdline = String::from_utf8_lossy(parse::trim(buf)).into();
            }
            if proc.read(CPath::pid(pid, b"/cgroup").as_cstr(), buf).is_ok() {
                // cgroup v2: a single "0::/path" line.
                let line = parse::lines(buf).find(|l| l.starts_with(b"0::")).unwrap_or(b"");
                cgroup = String::from_utf8_lossy(parse::trim(line.get(3..).unwrap_or(b""))).into_owned();
                unit = cgroup.rsplit('/').next().unwrap_or("").into();
            }
        }

        let exe_base = exe.as_deref().and_then(|e: &str| e.rsplit('/').next());
        let by_proc = self.apps.for_process(exe_base, &name);
        let by_unit = self.apps.for_unit(&unit);
        let entry = by_proc.map(|i| self.apps.get(i));
        let pretty = entry.map(|e| e.name.clone()).or_else(|| self.apps.known(&name));
        let icon = entry.and_then(|e| e.icon.clone());

        let kind = if kernel_thread { Kind::System } else { apps::kind_for_cgroup(&cgroup) };
        let group: Arc<str> = match by_unit {
            Some(i) => self.apps.get(i).name.clone(),
            None if kernel_thread => "Kernel".into(),
            None => unit_display(&unit).into(),
        };

        Process {
            pid,
            ppid: stat.ppid,
            uid,
            user,
            name,
            exe,
            cmdline,
            pretty,
            icon,
            unit,
            group,
            kind,
            kernel_thread,
            state: stat.state,
            threads: stat.threads,
            started_s: (stat.start_ticks as f64 / self.clk_tck) as u64,
            cpu: 0.0,
            cpu_history: Ring::default(),
            mem_private: 0,
            mem_rss: 0,
            gpu: 0.0,
            gpu_mem: 0,
            net_rx: 0.0,
            net_tx: 0.0,
        }
    }
}

/// `app-steam@autostart.service` -> `steam`, `plasma-dolphin.service` -> `plasma-dolphin`.
fn unit_display(unit: &str) -> String {
    if let Some(id) = apps::unit_app_id(unit) {
        return id.rsplit('.').next().unwrap_or(&id).to_string();
    }
    let base = unit.trim_end_matches(".service").trim_end_matches(".scope").trim_end_matches(".slice");
    if base.is_empty() { "Other".into() } else { base.to_string() }
}

/// The fields of `/proc/<pid>/stat` we use, borrowed from the read buffer.
struct Stat<'a> {
    comm: &'a [u8],
    state: u8,
    ppid: u32,
    flags: u64,
    utime: u64,
    stime: u64,
    threads: u32,
    start_ticks: u64,
    rss_pages: u64,
}

impl<'a> Stat<'a> {
    fn parse(buf: &'a [u8]) -> Option<Self> {
        // "pid (comm) state ..." - comm may contain spaces and parens, so use the last ')'.
        let open = buf.iter().position(|&b| b == b'(')?;
        let close = buf.iter().rposition(|&b| b == b')')?;
        let comm = buf.get(open + 1..close)?;
        let mut s = Stat { comm, state: b'?', ppid: 0, flags: 0, utime: 0, stime: 0, threads: 0, start_ticks: 0, rss_pages: 0 };
        for (i, f) in parse::fields(buf.get(close + 1..)?).enumerate() {
            match i {
                0 => s.state = f[0],
                1 => s.ppid = parse::u64(f) as u32,
                6 => s.flags = parse::u64(f),
                11 => s.utime = parse::u64(f),
                12 => s.stime = parse::u64(f),
                17 => s.threads = parse::u64(f) as u32,
                19 => s.start_ticks = parse::u64(f),
                21 => {
                    s.rss_pages = parse::u64(f);
                    break;
                }
                _ => {}
            }
        }
        Some(s)
    }

    fn comm_str(&self) -> &str {
        std::str::from_utf8(self.comm).unwrap_or("?")
    }
}

fn load_users() -> HashMap<u32, Arc<str>> {
    let text = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
    text.lines()
        .filter_map(|l| {
            let mut f = l.split(':');
            let name = f.next()?;
            let uid = f.nth(1)?.parse().ok()?;
            Some((uid, Arc::from(name)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stat_with_tricky_comm() {
        let line = b"1234 (Web (Content) x) S 1 1234 1234 0 -1 4194560 100 0 0 0 250 50 0 0 20 0 42 0 9876 1000 200";
        let s = Stat::parse(line).unwrap();
        assert_eq!(s.comm, b"Web (Content) x");
        assert_eq!(s.state, b'S');
        assert_eq!(s.ppid, 1);
        assert_eq!((s.utime, s.stime), (250, 50));
        assert_eq!(s.threads, 42);
        assert_eq!(s.start_ticks, 9876);
        assert_eq!(s.rss_pages, 200);
    }

    #[test]
    fn unit_names() {
        assert_eq!(unit_display("app-steam@autostart.service"), "steam");
        assert_eq!(unit_display("app-org.kde.konsole-12.scope"), "konsole");
        assert_eq!(unit_display("plasma-dolphin.service"), "plasma-dolphin");
    }
}
