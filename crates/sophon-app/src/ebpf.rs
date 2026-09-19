//! Per-socket traffic counters in the kernel (root only; used by the helper).
//!
//! Two tiny cgroup_skb programs, attached to the root cgroup for ingress and
//! egress, add each packet's length to a hash map keyed by its socket cookie.
//! Every path through the program returns 1 ("allow"), so it can never drop
//! traffic. Programs are attached with BPF links: closing the link fds (for
//! example when the helper exits) detaches them.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

const BPF_MAP_CREATE: libc::c_long = 0;
const BPF_MAP_LOOKUP_ELEM: libc::c_long = 1;
const BPF_MAP_GET_NEXT_KEY: libc::c_long = 4;
const BPF_PROG_LOAD: libc::c_long = 5;
const BPF_LINK_CREATE: libc::c_long = 28;
const BPF_MAP_TYPE_LRU_HASH: u32 = 9;
const BPF_PROG_TYPE_CGROUP_SKB: u32 = 8;
const BPF_CGROUP_INET_INGRESS: u32 = 0;
const BPF_CGROUP_INET_EGRESS: u32 = 1;
const MAX_SOCKETS: u32 = 65536;

pub struct NetCounters {
    rx: OwnedFd,
    tx: OwnedFd,
    /// Held to keep the programs attached.
    _links: Vec<OwnedFd>,
}

fn bpf(cmd: libc::c_long, attr: &mut [u8; 128]) -> io::Result<i32> {
    let r = unsafe { libc::syscall(libc::SYS_bpf, cmd, attr.as_mut_ptr(), attr.len() as u32) };
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(r as i32) }
}

fn put32(a: &mut [u8; 128], off: usize, v: u32) {
    a[off..off + 4].copy_from_slice(&v.to_ne_bytes());
}

fn put64(a: &mut [u8; 128], off: usize, v: u64) {
    a[off..off + 8].copy_from_slice(&v.to_ne_bytes());
}

/// One BPF instruction: opcode, dst/src registers, offset, immediate.
fn insn(code: u8, dst: u8, src: u8, off: i16, imm: i32) -> [u8; 8] {
    let mut b = [0u8; 8];
    b[0] = code;
    b[1] = (src << 4) | (dst & 0x0f);
    b[2..4].copy_from_slice(&off.to_ne_bytes());
    b[4..8].copy_from_slice(&imm.to_ne_bytes());
    b
}

/// `map[socket_cookie(skb)] += skb->len; return 1;`
fn program(map_fd: i32) -> Vec<u8> {
    const MOV64_X: u8 = 0xbf;
    const MOV64_K: u8 = 0xb7;
    const ADD64_K: u8 = 0x07;
    const CALL: u8 = 0x85;
    const EXIT: u8 = 0x95;
    const JA: u8 = 0x05;
    const JEQ_K: u8 = 0x15;
    const STX_DW: u8 = 0x7b;
    const LDX_W: u8 = 0x61;
    const ATOMIC_DW: u8 = 0xdb;
    const LD_IMM64: u8 = 0x18;
    const PSEUDO_MAP_FD: u8 = 1;
    const GET_SOCKET_COOKIE: i32 = 46;
    const MAP_LOOKUP: i32 = 1;
    const MAP_UPDATE: i32 = 2;
    let prog: Vec<[u8; 8]> = vec![
        insn(MOV64_X, 6, 1, 0, 0),                 // 0: r6 = skb
        insn(CALL, 0, 0, 0, GET_SOCKET_COOKIE),    // 1: r0 = cookie
        insn(STX_DW, 10, 0, -8, 0),                // 2: key on stack
        insn(LD_IMM64, 1, PSEUDO_MAP_FD, 0, map_fd), // 3: r1 = map
        insn(0, 0, 0, 0, 0),                       // 4
        insn(MOV64_X, 2, 10, 0, 0),                // 5: r2 = &key
        insn(ADD64_K, 2, 0, 0, -8),                // 6
        insn(CALL, 0, 0, 0, MAP_LOOKUP),           // 7: r0 = &value
        insn(JEQ_K, 0, 0, 3, 0),                   // 8: missing -> 12
        insn(LDX_W, 1, 6, 0, 0),                   // 9: r1 = skb->len
        insn(ATOMIC_DW, 0, 1, 0, 0),               // 10: *value += r1 (atomic add)
        insn(JA, 0, 0, 10, 0),                     // 11: -> 22
        insn(LDX_W, 1, 6, 0, 0),                   // 12: r1 = skb->len
        insn(STX_DW, 10, 1, -16, 0),               // 13: value on stack
        insn(LD_IMM64, 1, PSEUDO_MAP_FD, 0, map_fd), // 14: r1 = map
        insn(0, 0, 0, 0, 0),                       // 15
        insn(MOV64_X, 2, 10, 0, 0),                // 16: r2 = &key
        insn(ADD64_K, 2, 0, 0, -8),                // 17
        insn(MOV64_X, 3, 10, 0, 0),                // 18: r3 = &value
        insn(ADD64_K, 3, 0, 0, -16),               // 19
        insn(MOV64_K, 4, 0, 0, 0),                 // 20: flags = BPF_ANY
        insn(CALL, 0, 0, 0, MAP_UPDATE),           // 21
        insn(MOV64_K, 0, 0, 0, 1),                 // 22: return 1 (allow)
        insn(EXIT, 0, 0, 0, 0),                    // 23
    ];
    prog.concat()
}

fn create_map() -> io::Result<OwnedFd> {
    let mut a = [0u8; 128];
    put32(&mut a, 0, BPF_MAP_TYPE_LRU_HASH);
    put32(&mut a, 4, 8); // key: u64 cookie
    put32(&mut a, 8, 8); // value: u64 bytes
    put32(&mut a, 12, MAX_SOCKETS);
    let fd = bpf(BPF_MAP_CREATE, &mut a)?;
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn load(map: &OwnedFd, attach: u32) -> io::Result<OwnedFd> {
    let code = program(map.as_raw_fd());
    let license = b"GPL\0";
    let mut log = vec![0u8; 16 * 1024];
    let mut a = [0u8; 128];
    put32(&mut a, 0, BPF_PROG_TYPE_CGROUP_SKB);
    put32(&mut a, 4, (code.len() / 8) as u32);
    put64(&mut a, 8, code.as_ptr() as u64);
    put64(&mut a, 16, license.as_ptr() as u64);
    put32(&mut a, 24, 1); // log level
    put32(&mut a, 28, log.len() as u32);
    put64(&mut a, 32, log.as_mut_ptr() as u64);
    a[48..54].copy_from_slice(b"sophon");
    put32(&mut a, 68, attach); // expected_attach_type
    match bpf(BPF_PROG_LOAD, &mut a) {
        Ok(fd) => Ok(unsafe { OwnedFd::from_raw_fd(fd) }),
        Err(e) => {
            let msg = String::from_utf8_lossy(&log[..log.iter().position(|&b| b == 0).unwrap_or(0)]).trim().to_string();
            let last = msg.lines().last().unwrap_or("").to_string();
            Err(io::Error::new(e.kind(), format!("kernel rejected the counter ({e}){}", if last.is_empty() { String::new() } else { format!(": {last}") })))
        }
    }
}

fn attach(prog: &OwnedFd, cgroup: &OwnedFd, kind: u32) -> io::Result<OwnedFd> {
    let mut a = [0u8; 128];
    put32(&mut a, 0, prog.as_raw_fd() as u32);
    put32(&mut a, 4, cgroup.as_raw_fd() as u32);
    put32(&mut a, 8, kind);
    let fd = bpf(BPF_LINK_CREATE, &mut a)?;
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

impl NetCounters {
    pub fn start() -> io::Result<Self> {
        let cg = unsafe { libc::open(c"/sys/fs/cgroup".as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
        if cg < 0 {
            return Err(io::Error::last_os_error());
        }
        let cgroup = unsafe { OwnedFd::from_raw_fd(cg) };
        let (rx, tx) = (create_map()?, create_map()?);
        let rx_prog = load(&rx, BPF_CGROUP_INET_INGRESS)?;
        let tx_prog = load(&tx, BPF_CGROUP_INET_EGRESS)?;
        let links = vec![attach(&rx_prog, &cgroup, BPF_CGROUP_INET_INGRESS)?, attach(&tx_prog, &cgroup, BPF_CGROUP_INET_EGRESS)?];
        Ok(NetCounters { rx, tx, _links: links })
    }

    /// (cookie, received bytes, sent bytes) for every socket seen so far.
    pub fn read(&self) -> Vec<(u64, u64, u64)> {
        let rx = dump(&self.rx);
        let tx = dump(&self.tx);
        let mut all: std::collections::HashMap<u64, (u64, u64)> = std::collections::HashMap::with_capacity(rx.len() + tx.len());
        for (k, v) in rx {
            all.entry(k).or_default().0 = v;
        }
        for (k, v) in tx {
            all.entry(k).or_default().1 = v;
        }
        all.into_iter().map(|(k, (r, t))| (k, r, t)).collect()
    }
}

fn dump(map: &OwnedFd) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let mut key: u64 = 0;
    let mut next: u64 = 0;
    let mut first = true;
    loop {
        let mut a = [0u8; 128];
        put32(&mut a, 0, map.as_raw_fd() as u32);
        put64(&mut a, 8, if first { 0 } else { &key as *const u64 as u64 });
        put64(&mut a, 16, &mut next as *mut u64 as u64);
        if bpf(BPF_MAP_GET_NEXT_KEY, &mut a).is_err() {
            break;
        }
        first = false;
        key = next;
        let mut value: u64 = 0;
        let mut l = [0u8; 128];
        put32(&mut l, 0, map.as_raw_fd() as u32);
        put64(&mut l, 8, &key as *const u64 as u64);
        put64(&mut l, 16, &mut value as *mut u64 as u64);
        if bpf(BPF_MAP_LOOKUP_ELEM, &mut l).is_ok() {
            out.push((key, value));
        }
        if out.len() > MAX_SOCKETS as usize {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_always_allows() {
        let p = program(3);
        assert_eq!(p.len() % 8, 0);
        let insns: Vec<&[u8]> = p.chunks(8).collect();
        // The only exit is preceded by r0 = 1.
        let exits: Vec<usize> = insns.iter().enumerate().filter(|(_, i)| i[0] == 0x95).map(|(n, _)| n).collect();
        assert_eq!(exits, vec![insns.len() - 1]);
        let before = insns[insns.len() - 2];
        assert_eq!((before[0], before[1] & 0x0f, i32::from_ne_bytes(before[4..8].try_into().unwrap())), (0xb7, 0, 1));
        // Both jumps land on valid instructions.
        for (n, i) in insns.iter().enumerate() {
            if i[0] == 0x05 || i[0] == 0x15 {
                let off = i16::from_ne_bytes([i[2], i[3]]) as isize;
                let target = n as isize + 1 + off;
                assert!(target > n as isize && (target as usize) < insns.len(), "jump at {n} lands in range");
            }
        }
    }
}
