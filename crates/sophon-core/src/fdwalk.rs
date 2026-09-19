//! One shared walk over processes' open file descriptors.
//!
//! GPU usage needs to know which fds are `/dev/dri/*`; per-process network
//! needs to know which socket inodes belong to which process. Reading every
//! fd link is the costly part, so it's done once for both, on a rolling
//! schedule: each process is rescanned relative to when it was first seen
//! (spreading the work over time), and "hot" processes more often.

use std::collections::{HashMap, HashSet};

use crate::parse;
use crate::sys::{CPath, Dir};

/// Ticks between rescans of an ordinary process...
const RESCAN_EVERY: u64 = 30;
/// ...and of a hot one (a GPU client or a process with network traffic).
const RESCAN_HOT_EVERY: u64 = 5;

#[derive(Default)]
pub struct FdWalker {
    /// pid -> tick of its last scan.
    scanned: HashMap<u32, u64>,
    tick: u64,
    link_buf: Vec<u8>,
    /// Processes to rescan often; set by the consumers each tick.
    pub hot: HashSet<u32>,
    /// GPU fds found this tick, as (pid, fd).
    pub dri: Vec<(u32, u32)>,
    /// Socket inodes found this tick, as (inode, pid).
    pub sockets: Vec<(u64, u32)>,
}

impl FdWalker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Scans the processes that are due. `hot_now` rescans hot processes
    /// immediately; `sweep` rescans everyone (both used when network traffic
    /// can't be attributed yet).
    pub fn walk(&mut self, proc: &Dir, pids: &[u32], hot_now: bool, sweep: bool) {
        self.tick += 1;
        self.dri.clear();
        self.sockets.clear();
        let tick = self.tick;
        for &pid in pids {
            let hot = self.hot.contains(&pid);
            let every = if hot { RESCAN_HOT_EVERY } else { RESCAN_EVERY };
            let forced = sweep || (hot_now && hot);
            if !forced && self.scanned.get(&pid).is_some_and(|&t| tick - t < every) {
                continue;
            }
            self.scanned.insert(pid, tick);
            let Ok(fd_dir) = proc.open_at(CPath::pid(pid, b"/fd").as_cstr()) else { continue };
            let Ok(mut entries) = fd_dir.entries() else { continue };
            while let Some(name) = entries.next_name() {
                let Some(fd) = parse::try_u64(name) else { continue };
                if fd_dir.read_link(CPath::from(name).as_cstr(), &mut self.link_buf).is_err() {
                    continue;
                }
                let link = &self.link_buf[..];
                if link.starts_with(b"/dev/dri/") {
                    self.dri.push((pid, fd as u32));
                } else if let Some(rest) = link.strip_prefix(b"socket:[") {
                    self.sockets.push((parse::u64(rest), pid));
                }
            }
        }
        if tick % RESCAN_EVERY == 0 {
            // Forget exited processes (pids are sorted by the caller).
            self.scanned.retain(|pid, _| pids.binary_search(pid).is_ok());
        }
    }
}
