//! Ties the collectors together and produces a [`Snapshot`] per tick.

use std::io;
use std::sync::Arc;
use std::time::Instant;

use crate::apps::AppIndex;
use crate::cpu::{CpuCollector, CpuState};
use crate::disk::{DiskCollector, DiskState};
use crate::fdwalk::FdWalker;
use crate::gpu::{self, GpuDevice, GpuState, fdinfo::FdinfoScanner};
use crate::netproc::NetProcScanner;
use crate::mem::{MemCollector, MemState};
use crate::net::{NetCollector, NetIface};
use crate::parse;
use crate::process::{Process, ProcessTable};
use crate::sys::{self, Dir};

/// Everything the UI needs for one frame. Cheap to clone and `Send`.
#[derive(Clone, Default)]
pub struct Snapshot {
    pub tick: u64,
    pub cpu: CpuState,
    pub mem: MemState,
    pub gpus: Vec<GpuState>,
    pub disks: Vec<DiskState>,
    pub nets: Vec<NetIface>,
    pub processes: Arc<Vec<Process>>,
    /// True when `processes` was refreshed this tick.
    pub processes_updated: bool,
    pub thread_count: u32,
    /// When each history sample was taken (seconds since the sampler started),
    /// aligned with the newest end of every system history ring.
    pub times: crate::history::Ring,
    /// Seconds since the sampler started, at this snapshot.
    pub now: f32,
    /// Sophon's own footprint.
    pub own: OwnUsage,
}

#[derive(Clone, Copy, Default)]
pub struct OwnUsage {
    /// Percent of one core.
    pub cpu: f32,
    pub mem_private: u64,
    /// Wall time spent producing the last snapshot, in microseconds.
    pub sample_us: u32,
    /// Sampler thread CPU time per stage, in microseconds:
    /// cpu, mem, net, disk, gpu, processes, fd walk + gpu-procs + net-procs.
    pub stage_us: [u32; 7],
}

pub struct Sampler {
    proc: Dir,
    buf: Vec<u8>,
    cpu: CpuCollector,
    mem: MemCollector,
    net: NetCollector,
    disk: DiskCollector,
    gpus: Vec<Box<dyn GpuDevice>>,
    fdinfo: FdinfoScanner,
    fds: FdWalker,
    netproc: NetProcScanner,
    procs: ProcessTable,
    processes: Arc<Vec<Process>>,
    user_pids: Vec<u32>,
    tick: u64,
    last: Option<Instant>,
    started: Instant,
    /// Optional admin source of eBPF byte counters (cookie, rx, tx).
    ebpf_net: Box<dyn Fn() -> Option<Vec<(u64, u64, u64)>> + Send>,
    times: crate::history::Ring,
    own_prev_ticks: u64,
}

impl Sampler {
    pub fn new() -> io::Result<Self> {
        Self::with_apps(AppIndex::load())
    }

    pub fn with_apps(apps: AppIndex) -> io::Result<Self> {
        let proc = Dir::open(c"/proc")?;
        let procs = ProcessTable::new(&proc, apps)?;
        Ok(Self {
            buf: Vec::with_capacity(64 * 1024),
            cpu: CpuCollector::new(),
            mem: MemCollector::new(),
            net: NetCollector::new(),
            disk: DiskCollector::new(),
            gpus: gpu::detect(),
            fdinfo: FdinfoScanner::new(),
            fds: FdWalker::new(),
            netproc: NetProcScanner::new(),
            procs,
            processes: Arc::default(),
            user_pids: Vec::new(),
            tick: 0,
            last: None,
            started: Instant::now(),
            ebpf_net: Box::new(|| None),
            times: crate::history::Ring::default(),
            own_prev_ticks: 0,
            proc,
        })
    }

    /// Takes one sample. Pass `with_processes = false` to skip the process
    /// table (the costliest part) when nobody is looking at it.
    /// Sets the source of eBPF per-socket byte counters (admin feature).
    pub fn set_ebpf_source(&mut self, f: impl Fn() -> Option<Vec<(u64, u64, u64)>> + Send + 'static) {
        self.ebpf_net = Box::new(f);
    }

    pub fn sample(&mut self, with_processes: bool) -> Snapshot {
        let started = Instant::now();
        let dt = self.last.map_or(0.0, |l| started.duration_since(l).as_secs_f64());
        self.last = Some(started);
        self.tick += 1;
        let buf = &mut self.buf;

        let mut stage_us = [0u32; 7];
        let mut mark = sys::thread_cpu_us();
        let mut lap = |i: usize| {
            let now = sys::thread_cpu_us();
            stage_us[i] = now.saturating_sub(mark) as u32;
            mark = now;
        };
        self.cpu.sample(&self.proc, buf, dt);
        lap(0);
        self.mem.sample(&self.proc, buf);
        lap(1);
        self.net.sample(&self.proc, buf, dt);
        lap(2);
        self.disk.sample(&self.proc, buf, dt);
        lap(3);
        for g in &mut self.gpus {
            g.sample(buf);
        }
        lap(4);

        let first = self.tick == 1;
        let with_processes = with_processes || first;
        if with_processes {
            self.procs.refresh(&self.proc, buf);
            self.procs.user_pids(&mut self.user_pids);
        }
        lap(5);
        // One fd walk feeds both GPU and network attribution.
        self.fds.hot.clear();
        self.fds.hot.extend(self.fdinfo.gpu_pids());
        self.fds.hot.extend(self.netproc.active_pids());
        let sweep = with_processes && self.netproc.wants_sweep();
        let hot_now = with_processes && self.netproc.wants_hot_rescan();
        self.fds.walk(&self.proc, &self.user_pids, hot_now, sweep);
        if !self.gpus.is_empty() {
            self.fdinfo.add_candidates(&self.fds.dri);
            self.fdinfo.sample(&self.proc, buf);
            for g in &mut self.gpus {
                let s = g.state_mut();
                s.engines = self.fdinfo.engines_for(&s.pci_slot).unwrap_or_default();
                for (h, &v) in s.engine_history.iter_mut().zip(&s.engines) {
                    h.push(v);
                }
            }
        }
        if with_processes && self.netproc.available() {
            self.netproc.sample();
            self.netproc.learn(&self.fds.sockets);
            if let Some(counters) = (self.ebpf_net)() {
                self.netproc.set_ebpf(true);
                self.netproc.apply_ebpf(&counters, dt);
            } else {
                self.netproc.set_ebpf(false);
            }
        }
        let mut thread_count = self.processes.iter().map(|p| p.threads).sum();
        if with_processes {
            self.procs.apply_gpu(&self.fdinfo.usage);
            self.procs.apply_net(&self.netproc.usage);
            self.processes = Arc::new(self.procs.iter().cloned().collect());
            thread_count = self.processes.iter().map(|p| p.threads).sum();
        }

        let now = started.duration_since(self.started).as_secs_f32();
        if self.tick > 1 {
            self.times.push(now);
        }
        let mut own = self.own_usage(dt, started);
        own.stage_us = stage_us;
        Snapshot {
            tick: self.tick,
            cpu: self.cpu.state.clone(),
            mem: self.mem.state.clone(),
            gpus: self.gpus.iter().map(|g| g.state().clone()).collect(),
            disks: self.disk.disks().cloned().collect(),
            nets: self.net.ifaces().cloned().collect(),
            processes: self.processes.clone(),
            processes_updated: with_processes,
            thread_count,
            times: self.times,
            now,
            own,
        }
    }

    fn own_usage(&mut self, dt: f64, started: Instant) -> OwnUsage {
        let mut own = OwnUsage::default();
        let buf = &mut self.buf;
        if self.proc.read(c"self/stat", buf).is_ok()
            && let Some(close) = buf.iter().rposition(|&b| b == b')')
        {
            let mut f = parse::fields(&buf[close + 1..]).skip(11).map(parse::u64);
            let ticks = f.next().unwrap_or(0) + f.next().unwrap_or(0);
            if dt > 0.0 && self.own_prev_ticks > 0 {
                let secs = ticks.saturating_sub(self.own_prev_ticks) as f64 / sys::clk_tck() as f64;
                own.cpu = (secs / dt * 100.0) as f32;
            }
            self.own_prev_ticks = ticks;
        }
        if self.proc.read(c"self/statm", buf).is_ok() {
            let mut f = parse::fields(buf).map(parse::u64).skip(1);
            let (resident, shared) = (f.next().unwrap_or(0), f.next().unwrap_or(0));
            own.mem_private = resident.saturating_sub(shared) * sys::page_size();
        }
        own.sample_us = started.elapsed().as_micros() as u32;
        own
    }
}
