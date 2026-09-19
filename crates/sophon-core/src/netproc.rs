//! Per-process network throughput for TCP, without root.
//!
//! The kernel's sock_diag netlink interface reports byte counters for every
//! TCP socket (what `ss -ti` shows); sockets are matched to processes through
//! the socket inodes found by [`crate::fdwalk::FdWalker`]. UDP sockets have
//! no byte counters there, so UDP traffic (games, voice, QUIC) isn't counted;
//! that would need eBPF and elevated privileges.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Instant;

const NETLINK_SOCK_DIAG: libc::c_int = 4;
const SOCK_DIAG_BY_FAMILY: u16 = 20;
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const INET_DIAG_INFO: u16 = 2;
/// Offsets into `struct tcp_info` (linux/tcp.h).
const TCPI_BYTES_ACKED: usize = 120;
const TCPI_BYTES_RECEIVED: usize = 128;
/// Offsets of `idiag_uid` / `idiag_inode` in `struct inet_diag_msg`.
const IDIAG_UID: usize = 64;
const IDIAG_INODE: usize = 68;
/// The socket cookie is the last 8 bytes of `inet_diag_sockid`, which starts
/// at offset 4 in `inet_diag_msg` (family, state, timer, retrans, then sockid).
const IDIAG_COOKIE: usize = 44;
/// Unmatched traffic must persist this many ticks before every process is rescanned.
const SWEEP_AFTER: u32 = 10;
/// And full sweeps are at least this many ticks apart.
const SWEEP_EVERY: u64 = 30;
/// A process counts as network-active (rescanned often) for this many ticks after traffic.
const ACTIVE_TICKS: u64 = 10;
/// Full dumps walk the kernel's whole TCP hash table (milliseconds on big-RAM
/// machines), so they run only this often to find new connections...
const DUMP_EVERY: u64 = 5;
/// ...while sockets with recent traffic are looked up individually every tick.
const LOOKUP_TICKS: u64 = 30;
/// TCP states that can carry data (skips LISTEN and TIME_WAIT).
const DATA_STATES: u32 = (1 << 1) | (1 << 2) | (1 << 4) | (1 << 5) | (1 << 8) | (1 << 9) | (1 << 11);

#[derive(Clone, Copy, Default, Debug)]
pub struct ProcNet {
    pub rx_bps: f64,
    pub tx_bps: f64,
}

struct Sock {
    rx: u64,
    tx: u64,
    seen: u64,
    family: u8,
    /// `struct inet_diag_sockid`, for exact lookups.
    id: [u8; 48],
    /// SO_COOKIE, matching the eBPF counters' key.
    cookie: u64,
    last_traffic: u64,
}

pub struct NetProcScanner {
    nl: Option<OwnedFd>,
    buf: Vec<u8>,
    socks: HashMap<u64, Sock>,
    owner: HashMap<u64, u32>,
    /// pid -> last tick it had traffic.
    active: HashMap<u32, u64>,
    /// socket cookie -> owning pid (for eBPF byte attribution). Includes UDP.
    cookie_owner: HashMap<u64, u32>,
    /// Last eBPF cumulative bytes per cookie.
    ebpf_prev: HashMap<u64, (u64, u64)>,
    /// True once eBPF byte counting takes over from tcp_info.
    ebpf: bool,
    tick: u64,
    last: Option<Instant>,
    /// Traffic this tick on our sockets whose process isn't known yet.
    unmatched: u32,
    /// Consecutive ticks with unmatched traffic.
    unmatched_ticks: u32,
    last_sweep: u64,
    uid: u32,
    pub usage: HashMap<u32, ProcNet>,
}

impl NetProcScanner {
    pub fn new() -> Self {
        let fd = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, NETLINK_SOCK_DIAG) };
        Self {
            nl: (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) }),
            buf: vec![0; 64 * 1024],
            socks: HashMap::new(),
            owner: HashMap::new(),
            active: HashMap::new(),
            cookie_owner: HashMap::new(),
            ebpf_prev: HashMap::new(),
            ebpf: false,
            tick: 0,
            last: None,
            unmatched: 0,
            unmatched_ticks: 0,
            last_sweep: 0,
            uid: unsafe { libc::getuid() },
            usage: HashMap::new(),
        }
    }

    pub fn available(&self) -> bool {
        self.nl.is_some()
    }

    /// Processes that recently had traffic (worth rescanning often).
    pub fn active_pids(&self) -> impl Iterator<Item = u32> + '_ {
        self.active.keys().copied()
    }

    /// Unmatched traffic this tick: rescan the known network processes now (cheap).
    pub fn wants_hot_rescan(&self) -> bool {
        self.unmatched > 0
    }

    /// Unmatched traffic that persists: scan every process's fds (costly, rare).
    pub fn wants_sweep(&self) -> bool {
        self.unmatched_ticks >= SWEEP_AFTER && self.tick.saturating_sub(self.last_sweep) >= SWEEP_EVERY
    }

    pub fn swept(&mut self) {
        self.last_sweep = self.tick;
        self.unmatched_ticks = 0;
    }

    /// Records socket owners found by the fd walker.
    pub fn learn(&mut self, sockets: &[(u64, u32)]) {
        for &(inode, pid) in sockets {
            if let Some(sock) = self.socks.get(&inode) {
                self.owner.insert(inode, pid);
                if sock.cookie != 0 {
                    self.cookie_owner.insert(sock.cookie, pid);
                }
            }
        }
        // Forget cookies whose socket is gone.
        if self.cookie_owner.len() > 4096 {
            let live: std::collections::HashSet<u64> = self.socks.values().map(|s| s.cookie).collect();
            self.cookie_owner.retain(|c, _| live.contains(c));
        }
    }

    /// Whether byte counts should come from eBPF (admin) rather than tcp_info.
    pub fn set_ebpf(&mut self, on: bool) {
        if self.ebpf != on {
            self.ebpf = on;
            self.ebpf_prev.clear();
        }
    }

    /// Attributes eBPF per-socket byte deltas to processes by cookie.
    /// `counters` is cumulative (cookie, rx, tx). Call in place of tcp_info.
    pub fn apply_ebpf(&mut self, counters: &[(u64, u64, u64)], dt: f64) {
        self.usage.clear();
        if dt <= 0.0 {
            for &(c, rx, tx) in counters {
                self.ebpf_prev.insert(c, (rx, tx));
            }
            return;
        }
        let tick = self.tick;
        for &(cookie, rx, tx) in counters {
            let prev = self.ebpf_prev.insert(cookie, (rx, tx)).unwrap_or((rx, tx));
            let (drx, dtx) = (rx.saturating_sub(prev.0), tx.saturating_sub(prev.1));
            if drx + dtx == 0 {
                continue;
            }
            if let Some(&pid) = self.cookie_owner.get(&cookie) {
                let u = self.usage.entry(pid).or_default();
                u.rx_bps += drx as f64 / dt;
                u.tx_bps += dtx as f64 / dt;
                self.active.insert(pid, tick);
            }
        }
        if self.ebpf_prev.len() > 1 << 16 {
            self.ebpf_prev.clear();
        }
    }

    pub fn sample(&mut self) {
        self.tick += 1;
        let tick = self.tick;
        let now = Instant::now();
        let dt = self.last.map_or(0.0, |l| now.duration_since(l).as_secs_f64());
        self.last = Some(now);
        self.usage.clear();
        self.unmatched = 0;
        let full = tick % DUMP_EVERY == 1;
        if self.ebpf {
            // Identification only: cookies -> inodes for both TCP and UDP.
            if full {
                for proto in [libc::IPPROTO_TCP, libc::IPPROTO_UDP] {
                    for family in [libc::AF_INET as u8, libc::AF_INET6 as u8] {
                        let mut req = request(family, true, self.tick as u32);
                        req[17] = proto as u8;
                        req[18] = 0; // no INET_DIAG_INFO needed
                        req[20..24].copy_from_slice(&u32::MAX.to_ne_bytes());
                        let _ = self.exchange(&req);
                    }
                }
                self.socks.retain(|_, s| s.seen == tick);
                let socks = &self.socks;
                self.owner.retain(|inode, _| socks.contains_key(inode));
            }
            // Byte attribution happens in apply_ebpf(), called by the sampler.
            self.unmatched_ticks = 0;
            self.active.retain(|_, t| tick - *t < ACTIVE_TICKS);
            return;
        }
        if full {
            for family in [libc::AF_INET as u8, libc::AF_INET6 as u8] {
                let mut req = request(family, true, self.tick as u32);
                req[20..24].copy_from_slice(&DATA_STATES.to_ne_bytes());
                if self.exchange(&req).is_err() {
                    return;
                }
            }
        } else {
            let targets: Vec<(u8, [u8; 48])> = self
                .socks
                .values()
                .filter(|s| tick - s.last_traffic <= LOOKUP_TICKS)
                .map(|s| (s.family, s.id))
                .collect();
            for (family, id) in targets {
                let mut req = request(family, false, self.tick as u32);
                req[24..72].copy_from_slice(&id);
                let _ = self.exchange(&req);
            }
        }
        if dt > 0.0 {
            for u in self.usage.values_mut() {
                u.rx_bps /= dt;
                u.tx_bps /= dt;
            }
        } else {
            self.usage.clear();
        }
        self.unmatched_ticks = if self.unmatched > 0 { self.unmatched_ticks + 1 } else { 0 };
        if full {
            // A dump lists every live socket: anything not refreshed is gone.
            self.socks.retain(|_, s| s.seen == tick);
            let socks = &self.socks;
            self.owner.retain(|inode, _| socks.contains_key(inode));
        }
        self.active.retain(|_, t| tick - *t < ACTIVE_TICKS);
    }

    /// Sends one request and processes every reply until the kernel is done.
    fn exchange(&mut self, req: &[u8; 72]) -> io::Result<()> {
        let Some(nl) = &self.nl else { return Ok(()) };
        let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        addr.nl_family = libc::AF_NETLINK as u16;
        let sent = unsafe {
            libc::sendto(
                nl.as_raw_fd(),
                req.as_ptr().cast(),
                req.len(),
                0,
                (&addr as *const libc::sockaddr_nl).cast(),
                std::mem::size_of::<libc::sockaddr_nl>() as u32,
            )
        };
        if sent < 0 {
            return Err(io::Error::last_os_error());
        }
        let dump = u16::from_ne_bytes([req[6], req[7]]) & libc::NLM_F_DUMP as u16 != 0;
        let tick = self.tick;
        loop {
            let n = unsafe { libc::recv(nl.as_raw_fd(), self.buf.as_mut_ptr().cast(), self.buf.len(), 0) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            let mut off = 0usize;
            let n = n as usize;
            while off + 16 <= n {
                let len = u32::from_ne_bytes(self.buf[off..off + 4].try_into().unwrap()) as usize;
                let kind = u16::from_ne_bytes(self.buf[off + 4..off + 6].try_into().unwrap());
                if len < 16 || off + len > n {
                    break;
                }
                if kind == NLMSG_DONE || kind == NLMSG_ERROR {
                    // A failed single lookup means the socket closed.
                    return Ok(());
                }
                let msg = &self.buf[off + 16..off + len];
                // Other users' sockets can't be attributed (their fds aren't readable).
                if let Some((inode, rx, tx)) = parse_msg(msg, self.uid) {
                    let family = msg[0];
                    let mut id = [0u8; 48];
                    id.copy_from_slice(&msg[4..52]);
                    let cookie = u64::from_ne_bytes(msg[IDIAG_COOKIE..IDIAG_COOKIE + 8].try_into().unwrap_or_default());
                    let s = self.socks.entry(inode).or_insert(Sock { rx, tx, seen: 0, family, id, cookie, last_traffic: 0 });
                    s.cookie = cookie;
                    let (drx, dtx) = (rx.saturating_sub(s.rx), tx.saturating_sub(s.tx));
                    s.rx = rx;
                    s.tx = tx;
                    s.seen = tick;
                    if drx + dtx > 0 {
                        s.last_traffic = tick;
                        match self.owner.get(&inode) {
                            Some(&pid) => {
                                let u = self.usage.entry(pid).or_default();
                                u.rx_bps += drx as f64;
                                u.tx_bps += dtx as f64;
                                self.active.insert(pid, tick);
                            }
                            None => self.unmatched += 1,
                        }
                    }
                }
                off += (len + 3) & !3;
            }
            if !dump {
                return Ok(());
            }
        }
    }
}

/// A sock_diag request for TCP sockets: a dump, or one socket by its id.
fn request(family: u8, dump: bool, seq: u32) -> [u8; 72] {
    // nlmsghdr (16) + inet_diag_req_v2 (56)
    let mut req = [0u8; 72];
    let flags = if dump { libc::NLM_F_REQUEST | libc::NLM_F_DUMP } else { libc::NLM_F_REQUEST };
    req[0..4].copy_from_slice(&72u32.to_ne_bytes());
    req[4..6].copy_from_slice(&SOCK_DIAG_BY_FAMILY.to_ne_bytes());
    req[6..8].copy_from_slice(&(flags as u16).to_ne_bytes());
    req[8..12].copy_from_slice(&seq.to_ne_bytes());
    req[16] = family;
    req[17] = libc::IPPROTO_TCP as u8;
    req[18] = 1 << (INET_DIAG_INFO - 1);
    req[20..24].copy_from_slice(&u32::MAX.to_ne_bytes());
    req
}

impl Default for NetProcScanner {
    fn default() -> Self {
        Self::new()
    }
}

/// (inode, bytes received, bytes acked) from one inet_diag message owned by `uid`.
fn parse_msg(msg: &[u8], uid: u32) -> Option<(u64, u64, u64)> {
    let owner = u32::from_ne_bytes(msg.get(IDIAG_UID..IDIAG_UID + 4)?.try_into().ok()?);
    if owner != uid {
        return None;
    }
    let inode = u32::from_ne_bytes(msg.get(IDIAG_INODE..IDIAG_INODE + 4)?.try_into().ok()?) as u64;
    if inode == 0 {
        return None;
    }
    let mut off = 72; // sizeof(inet_diag_msg)
    while off + 4 <= msg.len() {
        let len = u16::from_ne_bytes(msg[off..off + 2].try_into().ok()?) as usize;
        let kind = u16::from_ne_bytes(msg[off + 2..off + 4].try_into().ok()?);
        if len < 4 || off + len > msg.len() {
            break;
        }
        if kind == INET_DIAG_INFO {
            let info = &msg[off + 4..off + len];
            let get = |o: usize| info.get(o..o + 8).and_then(|b| b.try_into().ok()).map(u64::from_ne_bytes);
            return Some((inode, get(TCPI_BYTES_RECEIVED)?, get(TCPI_BYTES_ACKED)?));
        }
        off += (len + 3) & !3;
    }
    None
}
