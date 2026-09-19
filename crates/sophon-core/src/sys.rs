//! Thin, allocation-free wrappers over the syscalls the sampler uses.

use std::ffi::CStr;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// A NUL-terminated path assembled on the stack.
pub struct CPath {
    buf: [u8; 256],
    len: usize,
}

impl CPath {
    pub fn new() -> Self {
        Self { buf: [0; 256], len: 0 }
    }

    pub fn from(s: &[u8]) -> Self {
        let mut p = Self::new();
        p.push(s);
        p
    }

    /// `"<pid><suffix>"`, e.g. `CPath::pid(42, b"/stat")`.
    pub fn pid(pid: u32, suffix: &[u8]) -> Self {
        let mut p = Self::new();
        p.push_u32(pid).push(suffix);
        p
    }

    pub fn push(&mut self, s: &[u8]) -> &mut Self {
        let end = (self.len + s.len()).min(self.buf.len() - 1);
        self.buf[self.len..end].copy_from_slice(&s[..end - self.len]);
        self.len = end;
        self.buf[end] = 0;
        self
    }

    pub fn push_u32(&mut self, mut v: u32) -> &mut Self {
        let mut tmp = [0u8; 10];
        let mut i = tmp.len();
        loop {
            i -= 1;
            tmp[i] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        self.push(&tmp[i..])
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    pub fn as_cstr(&self) -> &CStr {
        CStr::from_bytes_until_nul(&self.buf[..=self.len]).unwrap_or(c"")
    }
}

impl Default for CPath {
    fn default() -> Self {
        Self::new()
    }
}

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(r) }
}

/// An open directory handle; all reads are `openat`-relative to it.
pub struct Dir(OwnedFd);

impl Dir {
    pub fn open(path: &CStr) -> io::Result<Self> {
        open_dir(libc::AT_FDCWD, path)
    }

    pub fn open_at(&self, path: &CStr) -> io::Result<Self> {
        open_dir(self.0.as_raw_fd(), path)
    }

    /// Reads the whole file into `buf` (cleared first). Allocates only if the
    /// file outgrows the buffer's existing capacity.
    pub fn read(&self, path: &CStr, buf: &mut Vec<u8>) -> io::Result<usize> {
        let fd = cvt(unsafe {
            libc::openat(self.0.as_raw_fd(), path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC)
        })?;
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        read_all(fd.as_raw_fd(), buf)
    }

    pub fn read_u64(&self, path: &CStr, buf: &mut Vec<u8>) -> Option<u64> {
        self.read(path, buf).ok()?;
        crate::parse::try_u64(buf)
    }

    pub fn exists(&self, path: &CStr) -> bool {
        unsafe { libc::faccessat(self.0.as_raw_fd(), path.as_ptr(), libc::F_OK, 0) == 0 }
    }

    pub fn read_link(&self, path: &CStr, buf: &mut Vec<u8>) -> io::Result<usize> {
        buf.clear();
        if buf.capacity() < 256 {
            buf.reserve(256);
        }
        loop {
            let cap = buf.capacity();
            let n = unsafe {
                libc::readlinkat(self.0.as_raw_fd(), path.as_ptr(), buf.as_mut_ptr().cast(), cap)
            };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            if (n as usize) < cap {
                unsafe { buf.set_len(n as usize) };
                return Ok(n as usize);
            }
            buf.reserve(cap * 2);
        }
    }

    /// Owner uid of a path (for `/proc/<pid>` this is the process's effective uid).
    pub fn owner(&self, path: &CStr) -> io::Result<u32> {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        cvt(unsafe { libc::fstatat(self.0.as_raw_fd(), path.as_ptr(), &mut st, 0) })?;
        Ok(st.st_uid)
    }

    /// A re-windable stream over this directory's entries.
    pub fn entries(&self) -> io::Result<DirStream> {
        let fd = cvt(unsafe { libc::fcntl(self.0.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) })?;
        let dir = unsafe { libc::fdopendir(fd) };
        if dir.is_null() {
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(e);
        }
        Ok(DirStream(dir))
    }
}

fn open_dir(at: libc::c_int, path: &CStr) -> io::Result<Dir> {
    let fd = cvt(unsafe {
        libc::openat(at, path.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC)
    })?;
    Ok(Dir(unsafe { OwnedFd::from_raw_fd(fd) }))
}

fn read_all(fd: libc::c_int, buf: &mut Vec<u8>) -> io::Result<usize> {
    buf.clear();
    if buf.capacity() < 4096 {
        buf.reserve(4096);
    }
    loop {
        if buf.len() == buf.capacity() {
            buf.reserve(buf.capacity());
        }
        let spare = buf.capacity() - buf.len();
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().add(buf.len()).cast(), spare) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if n == 0 {
            return Ok(buf.len());
        }
        unsafe { buf.set_len(buf.len() + n as usize) };
    }
}

/// Directory entry stream that is rewound rather than reopened each tick.
pub struct DirStream(*mut libc::DIR);

// The DIR* is only ever used by the thread that owns the stream.
unsafe impl Send for DirStream {}

impl DirStream {
    pub fn rewind(&mut self) {
        unsafe { libc::rewinddir(self.0) }
    }

    /// Next entry name, skipping `.` and `..`. The slice is valid until the next call.
    pub fn next_name(&mut self) -> Option<&[u8]> {
        loop {
            let ent = unsafe { libc::readdir64(self.0) };
            if ent.is_null() {
                return None;
            }
            let name = unsafe { CStr::from_ptr((*ent).d_name.as_ptr()) }.to_bytes();
            if name != b"." && name != b".." {
                return Some(name);
            }
        }
    }

    /// Calls `f` for every numeric entry name (pids in `/proc`, fds in `fd/`).
    pub fn for_each_number(&mut self, mut f: impl FnMut(u32)) {
        self.rewind();
        while let Some(name) = self.next_name() {
            if let Some(n) = crate::parse::try_u64(name) {
                f(n as u32);
            }
        }
    }
}

impl Drop for DirStream {
    fn drop(&mut self) {
        unsafe { libc::closedir(self.0) };
    }
}

/// Clock ticks per second used by `/proc/<pid>/stat` times.
pub fn clk_tck() -> u64 {
    (unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).max(1) as u64
}

pub fn page_size() -> u64 {
    (unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).max(4096) as u64
}

/// Finds a hwmon directory by its `name` file (e.g. `k10temp`).
pub fn find_hwmon(names: &[&[u8]]) -> Option<Dir> {
    let root = Dir::open(c"/sys/class/hwmon").ok()?;
    let mut entries = root.entries().ok()?;
    let mut buf = Vec::new();
    let mut found = Vec::new();
    while let Some(entry) = entries.next_name() {
        let mut p = CPath::from(entry);
        p.push(b"/name");
        if root.read(p.as_cstr(), &mut buf).is_ok() {
            let name = crate::parse::trim(&buf);
            if let Some(rank) = names.iter().position(|n| *n == name) {
                found.push((rank, CPath::from(entry)));
            }
        }
    }
    found.sort_by_key(|(rank, _)| *rank);
    found.first().and_then(|(_, p)| root.open_at(p.as_cstr()).ok())
}

/// Finds the first `hwmon*` directory under `dir/<sub>` (used for devices that
/// carry their own hwmon, like GPUs and NVMe drives).
pub fn device_hwmon(dir: &Dir, sub: &[u8]) -> Option<Dir> {
    let base = dir.open_at(CPath::from(sub).as_cstr()).ok()?;
    let mut entries = base.entries().ok()?;
    while let Some(name) = entries.next_name() {
        if name == b"hwmon" {
            // Some drivers (amdgpu) nest it: device/hwmon/hwmonN.
            let nested = base.open_at(c"hwmon").ok()?;
            return device_hwmon(&nested, b".");
        }
        if name.starts_with(b"hwmon") {
            let p = CPath::from(name);
            return base.open_at(p.as_cstr()).ok();
        }
    }
    None
}

/// Reads a small text file into an owned string (for one-off, non-hot-path use).
pub fn read_string(dir: &Dir, path: &CStr) -> Option<String> {
    let mut buf = Vec::new();
    dir.read(path, &mut buf).ok()?;
    let s = String::from_utf8_lossy(crate::parse::trim(&buf)).into_owned();
    (!s.is_empty()).then_some(s)
}

/// CPU time consumed by the calling thread, in microseconds.
pub fn thread_cpu_us() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1000
}
