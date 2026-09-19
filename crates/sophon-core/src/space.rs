//! Disk space scanning for the space map (treemap).
//!
//! Scans run on demand, in parallel, at idle CPU and I/O priority so they
//! don't disturb games. To keep memory small the tree holds every folder but
//! only each folder's largest files; the rest are folded into one
//! "smaller files" entry per folder.

use std::collections::HashSet;
use std::ffi::CString;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::thread::{Scope, ScopedJoinHandle};
use std::time::{Duration, Instant};

/// Files at least this big get their own block.
const KEEP_FILE: u64 = 1 << 20;
/// At most this many individual files per folder.
const KEEP_FILES: usize = 48;
/// Folders smaller than this are folded into their parent's "smaller items".
const KEEP_DIR: u64 = 1 << 20;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeKind {
    Dir,
    File,
    /// Many small files, summed.
    SmallFiles,
    /// A folder we weren't allowed to read.
    Unreadable,
    /// Space the filesystem reports as used that the scan didn't find.
    Unaccounted,
}

/// File-type groups for the "colour by type" view.
pub const CATEGORIES: [&str; 7] = ["Video", "Audio", "Images", "Archives & disk images", "Documents & code", "Programs & game data", "Other"];

/// Category of a file name, by extension.
pub fn category(name: &str) -> usize {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "mp4" | "mkv" | "webm" | "avi" | "mov" | "m4v" | "wmv" | "flv" | "ts" | "m2ts" => 0,
        "mp3" | "flac" | "ogg" | "opus" | "wav" | "m4a" | "aac" | "wma" | "alac" => 1,
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "heic" | "bmp" | "tif" | "tiff" | "svg" | "raw" | "cr2" | "nef" | "psd" | "xcf" | "kra" => 2,
        "zip" | "7z" | "rar" | "tar" | "gz" | "xz" | "zst" | "bz2" | "iso" | "img" | "qcow2" | "vdi" | "vmdk" | "appimage" | "deb" | "rpm" | "flatpak" => 3,
        "txt" | "md" | "pdf" | "doc" | "docx" | "odt" | "xls" | "xlsx" | "ods" | "ppt" | "pptx" | "rs" | "c" | "h" | "cpp" | "py" | "js" | "ts2"
        | "java" | "go" | "json" | "toml" | "yaml" | "yml" | "xml" | "html" | "css" | "csv" | "log" | "sql" | "sh" => 4,
        "exe" | "dll" | "so" | "bin" | "pak" | "pck" | "vpk" | "assets" | "bundle" | "dat" | "unity3d" | "ucas" | "utoc" | "arc" | "gguf"
        | "safetensors" | "ckpt" | "pt" | "onnx" | "wasm" | "o" | "a" | "jar" | "msi" => 5,
        _ => 6,
    }
}

#[derive(Debug)]
pub struct Node {
    pub name: Box<str>,
    /// Allocated bytes on disk (like `du`), including everything below.
    pub size: u64,
    pub kind: NodeKind,
    /// Files in this subtree.
    pub files: u64,
    /// Bytes per file-type category, in KiB (see [`CATEGORIES`]).
    pub cats: [u32; CATEGORIES.len()],
    /// Newest modification time in this subtree (seconds since the epoch).
    pub newest: u32,
    /// Largest first.
    pub children: Vec<Node>,
}

impl Node {
    fn new(name: Box<str>, kind: NodeKind) -> Self {
        Node { name, size: 0, kind, files: 0, cats: [0; CATEGORIES.len()], newest: 0, children: Vec::new() }
    }

    /// The category holding the most bytes here.
    pub fn dominant(&self) -> usize {
        self.cats.iter().enumerate().max_by_key(|(_, v)| **v).map_or(CATEGORIES.len() - 1, |(i, _)| i)
    }

    /// Follows child indices from this node.
    pub fn at(&self, path: &[u32]) -> Option<&Node> {
        path.iter().try_fold(self, |n, &i| n.children.get(i as usize))
    }

    /// Removes the node at `path` (non-empty), subtracting its size and file
    /// count from every ancestor. Returns the removed node.
    pub fn remove(&mut self, path: &[u32]) -> Option<Node> {
        let (&first, rest) = path.split_first()?;
        let removed = if rest.is_empty() {
            if first as usize >= self.children.len() {
                return None;
            }
            self.children.remove(first as usize)
        } else {
            self.children.get_mut(first as usize)?.remove(rest)?
        };
        self.size = self.size.saturating_sub(removed.size);
        self.files = self.files.saturating_sub(removed.files);
        for (a, b) in self.cats.iter_mut().zip(removed.cats) {
            *a = a.saturating_sub(b);
        }
        Some(removed)
    }
}

/// Live counters for a running scan; shared with the UI.
#[derive(Default)]
pub struct Progress {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    pub bytes: AtomicU64,
    pub denied: AtomicU64,
    pub cancel: AtomicBool,
}

pub struct ScanResult {
    pub root: Node,
    pub path: PathBuf,
    pub elapsed: Duration,
    pub denied: u64,
    /// Used space reported by the filesystem when the scan finished.
    pub fs_used: u64,
    pub cancelled: bool,
}

/// How to scan.
#[derive(Clone, Debug)]
pub struct ScanOptions {
    pub threads: usize,
    /// Lowest CPU and idle I/O priority (yields to games).
    pub idle: bool,
    pub include_hidden: bool,
    pub exclude: Vec<PathBuf>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions { threads: 4, idle: true, include_hidden: true, exclude: Vec::new() }
    }
}

struct Ctx<'a> {
    progress: &'a Progress,
    options: &'a ScanOptions,
    /// Other mount points below the root: not ours to scan.
    skip: HashSet<PathBuf>,
    slots: AtomicUsize,
    /// (dev, inode) of multiply-linked files already counted.
    links: Mutex<HashSet<(u64, u64)>>,
}

/// Scans `root` without crossing into other mounts.
pub fn scan(root: &Path, progress: &Progress, options: &ScanOptions) -> ScanResult {
    let started = Instant::now();
    let threads = options.threads;
    if options.idle {
        background_priority();
    }
    let all = mounts();
    let is_mount_root = all.iter().any(|m| m.path == root);
    let mut skip: HashSet<PathBuf> = all.into_iter().map(|m| m.path).filter(|p| p != root && p.starts_with(root)).collect();
    skip.extend(options.exclude.iter().cloned());
    let ctx = Ctx { progress, options, skip, slots: AtomicUsize::new(threads.saturating_sub(1)), links: Mutex::default() };
    let name: Box<str> = root.to_string_lossy().into();
    let mut node = std::thread::scope(|s| walk(s, &ctx, root.to_path_buf(), name));
    let fs_used = statvfs(root).map_or(0, |(total, free)| total - free);
    // Only a whole filesystem can be compared against its used space.
    if is_mount_root && fs_used > node.size && fs_used - node.size > node.size / 100 {
        let mut gap = Node::new("Not scanned (unreadable folders, snapshots, other subvolumes, exclusions)".into(), NodeKind::Unaccounted);
        gap.size = fs_used - node.size;
        node.size = fs_used;
        node.children.push(gap);
        node.children.sort_by(|a, b| b.size.cmp(&a.size));
    }
    ScanResult {
        root: node,
        path: root.to_path_buf(),
        elapsed: started.elapsed(),
        denied: progress.denied.load(Relaxed),
        fs_used,
        cancelled: progress.cancel.load(Relaxed),
    }
}

fn walk<'s, 'e: 's>(s: &'s Scope<'s, 'e>, ctx: &'e Ctx<'e>, path: PathBuf, name: Box<str>) -> Node {
    let mut node = Node::new(name, NodeKind::Dir);
    let Ok(entries) = fs::read_dir(&path) else {
        ctx.progress.denied.fetch_add(1, Relaxed);
        node.kind = NodeKind::Unreadable;
        return node;
    };
    ctx.progress.dirs.fetch_add(1, Relaxed);

    let mut files: Vec<(u64, Box<str>, u32)> = Vec::new();
    let mut subdirs: Vec<(PathBuf, Box<str>)> = Vec::new();
    for entry in entries.flatten() {
        if ctx.progress.cancel.load(Relaxed) {
            break;
        }
        let Ok(ft) = entry.file_type() else { continue };
        let child_name: Box<str> = entry.file_name().to_string_lossy().into();
        if !ctx.options.include_hidden && child_name.starts_with('.') {
            continue;
        }
        if ft.is_dir() {
            let p = entry.path();
            if !ctx.skip.contains(&p) {
                subdirs.push((p, child_name));
            }
        } else if !ft.is_symlink() {
            let Ok(md) = entry.metadata() else { continue };
            if md.nlink() > 1 && !ctx.links.lock().unwrap().insert((md.dev(), md.ino())) {
                continue; // hard link to a file we already counted
            }
            let size = md.blocks() * 512;
            ctx.progress.files.fetch_add(1, Relaxed);
            ctx.progress.bytes.fetch_add(size, Relaxed);
            files.push((size, child_name, md.mtime().clamp(0, u32::MAX as i64) as u32));
        }
    }

    // Recurse after the directory handle is closed, to keep open fds low.
    // Subfolders go to a new thread while one is free, otherwise run inline.
    let mut spawned: Vec<ScopedJoinHandle<'s, Node>> = Vec::new();
    let mut inline = Vec::new();
    for (p, n) in subdirs {
        let free = ctx.slots.fetch_update(Relaxed, Relaxed, |v| v.checked_sub(1)).is_ok();
        if free {
            spawned.push(s.spawn(move || {
                if ctx.options.idle {
                    background_priority();
                }
                let n = walk(s, ctx, p, n);
                ctx.slots.fetch_add(1, Relaxed);
                n
            }));
        } else {
            inline.push((p, n));
        }
    }
    for (p, n) in inline {
        node.children.push(walk(s, ctx, p, n));
    }
    for h in spawned {
        if let Ok(n) = h.join() {
            node.children.push(n);
        }
    }

    files.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    let (mut small, mut small_n) = (0u64, 0u64);
    let mut small_files = 0u64;
    let mut small_cats = [0u32; CATEGORIES.len()];
    let mut small_newest = 0u32;
    node.children.retain(|c| {
        let tiny = c.kind == NodeKind::Dir && c.size < KEEP_DIR;
        if tiny {
            small += c.size;
            small_n += 1;
            small_files += c.files;
            for (a, b) in small_cats.iter_mut().zip(c.cats) {
                *a += b;
            }
            small_newest = small_newest.max(c.newest);
        }
        !tiny
    });
    for (i, (size, name, mtime)) in files.into_iter().enumerate() {
        let cat = category(&name);
        let kib = (size / 1024).min(u32::MAX as u64) as u32;
        if i < KEEP_FILES && size >= KEEP_FILE {
            let mut cats = [0u32; CATEGORIES.len()];
            cats[cat] = kib;
            node.children.push(Node { name, size, kind: NodeKind::File, files: 1, cats, newest: mtime, children: Vec::new() });
        } else {
            small += size;
            small_n += 1;
            small_files += 1;
            small_cats[cat] = small_cats[cat].saturating_add(kib);
            small_newest = small_newest.max(mtime);
        }
    }
    if small > 0 {
        let label = format!("{small_n} smaller item{}", if small_n == 1 { "" } else { "s" });
        node.children.push(Node {
            name: label.into(),
            size: small,
            kind: NodeKind::SmallFiles,
            files: small_files,
            cats: small_cats,
            newest: small_newest,
            children: Vec::new(),
        });
    }
    node.children.retain(|c| c.size > 0 || c.kind == NodeKind::Unreadable);
    node.size = node.children.iter().map(|c| c.size).sum();
    node.files = node.children.iter().map(|c| c.files).sum();
    for c in &node.children {
        for (a, b) in node.cats.iter_mut().zip(c.cats) {
            *a = a.saturating_add(b);
        }
        node.newest = node.newest.max(c.newest);
    }
    node.children.sort_by(|a, b| b.size.cmp(&a.size));
    node.children.shrink_to_fit();
    node
}

/// Lowest CPU priority and idle I/O class for the calling thread.
fn background_priority() {
    // On Linux, setpriority(PRIO_PROCESS, 0) and ioprio_set(WHO_PROCESS, 0)
    // apply to the calling thread only.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 19);
        const IOPRIO_WHO_PROCESS: libc::c_long = 1;
        const IOPRIO_CLASS_IDLE: libc::c_long = 3;
        libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, IOPRIO_CLASS_IDLE << 13);
    }
}

#[derive(Clone, Debug)]
pub struct Mount {
    pub path: PathBuf,
    pub fs_type: String,
    /// Block device, e.g. `nvme0n1p3`.
    pub device: String,
    /// Whole disk the device belongs to, e.g. `nvme0n1`.
    pub disk: String,
    pub total: u64,
    pub free: u64,
}

/// Real (block-device backed) mounts, sorted by path.
pub fn mounts() -> Vec<Mount> {
    let text = fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    let mut out: Vec<Mount> = Vec::new();
    for line in text.lines() {
        // id parent major:minor root mountpoint options [optional...] - fstype source superopts
        let Some((pre, post)) = line.split_once(" - ") else { continue };
        let path = unescape(pre.split(' ').nth(4).unwrap_or(""));
        let mut post = post.split(' ');
        let (fs_type, source) = (post.next().unwrap_or(""), post.next().unwrap_or(""));
        let Some(device) = source.strip_prefix("/dev/") else { continue };
        let device = device.rsplit('/').next().unwrap_or(device).to_string();
        if out.iter().any(|m| m.path == Path::new(&path)) {
            continue;
        }
        let disk = fs::read_link(format!("/sys/class/block/{device}"))
            .ok()
            .and_then(|l| {
                let parts: Vec<String> = l.iter().map(|c| c.to_string_lossy().into_owned()).collect();
                // .../nvme0n1/nvme0n1p3 -> nvme0n1; .../nvme0n1 -> nvme0n1
                let n = parts.len();
                if n >= 2 && parts[n - 1].starts_with(&parts[n - 2]) { Some(parts[n - 2].clone()) } else { parts.last().cloned() }
            })
            .unwrap_or_else(|| device.clone());
        let (total, free) = statvfs(Path::new(&path)).unwrap_or((0, 0));
        out.push(Mount { path: path.into(), fs_type: fs_type.into(), device, disk, total, free });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// (total, available-to-user) bytes of the filesystem at `path`.
pub fn statvfs(path: &Path) -> Option<(u64, u64)> {
    let c = CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let frsize = st.f_frsize as u64;
    Some((st.f_blocks as u64 * frsize, st.f_bavail as u64 * frsize))
}

/// mountinfo escapes spaces etc. as `\040`.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            out.push((b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---- Remembering scans --------------------------------------------------------------

const CACHE_MAGIC: &[u8; 8] = b"SOPHSP01";

/// Writes a scan to `path` in a compact binary form.
pub fn save(result: &ScanResult, path: &Path) -> std::io::Result<()> {
    use std::io::Write;
    let mut out = std::io::BufWriter::new(fs::File::create(path)?);
    out.write_all(CACHE_MAGIC)?;
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    out.write_all(&secs.to_le_bytes())?;
    out.write_all(&(result.elapsed.as_millis() as u64).to_le_bytes())?;
    out.write_all(&result.denied.to_le_bytes())?;
    out.write_all(&result.fs_used.to_le_bytes())?;
    let path_bytes = result.path.as_os_str().as_encoded_bytes();
    out.write_all(&(path_bytes.len() as u32).to_le_bytes())?;
    out.write_all(path_bytes)?;
    fn node(out: &mut impl Write, n: &Node) -> std::io::Result<()> {
        let name = n.name.as_bytes();
        out.write_all(&(name.len().min(u16::MAX as usize) as u16).to_le_bytes())?;
        out.write_all(&name[..name.len().min(u16::MAX as usize)])?;
        out.write_all(&n.size.to_le_bytes())?;
        out.write_all(&[n.kind as u8])?;
        out.write_all(&n.files.to_le_bytes())?;
        out.write_all(&n.newest.to_le_bytes())?;
        for c in n.cats {
            out.write_all(&c.to_le_bytes())?;
        }
        out.write_all(&(n.children.len() as u32).to_le_bytes())?;
        n.children.iter().try_for_each(|c| node(out, c))
    }
    node(&mut out, &result.root)?;
    out.flush()
}

/// Reads a scan written by [`save`]; returns it with the time it was taken.
pub fn load(path: &Path) -> Option<(ScanResult, std::time::SystemTime)> {
    let data = fs::read(path).ok()?;
    let mut r = Reader { data: &data, pos: 0 };
    if r.take(8)? != CACHE_MAGIC {
        return None;
    }
    let when = std::time::UNIX_EPOCH + Duration::from_secs(r.u64()?);
    let elapsed = Duration::from_millis(r.u64()?);
    let denied = r.u64()?;
    let fs_used = r.u64()?;
    let plen = r.u32()? as usize;
    let root_path = PathBuf::from(String::from_utf8_lossy(r.take(plen)?).into_owned());
    fn node(r: &mut Reader, depth: u32) -> Option<Node> {
        if depth > 4096 {
            return None;
        }
        let nlen = r.u16()? as usize;
        let name: Box<str> = String::from_utf8_lossy(r.take(nlen)?).into();
        let size = r.u64()?;
        let kind = match r.take(1)?[0] {
            0 => NodeKind::Dir,
            1 => NodeKind::File,
            2 => NodeKind::SmallFiles,
            3 => NodeKind::Unreadable,
            _ => NodeKind::Unaccounted,
        };
        let files = r.u64()?;
        let newest = r.u32()?;
        let mut cats = [0u32; CATEGORIES.len()];
        for c in &mut cats {
            *c = r.u32()?;
        }
        let count = r.u32()? as usize;
        let mut children = Vec::with_capacity(count.min(1 << 16));
        for _ in 0..count {
            children.push(node(r, depth + 1)?);
        }
        Some(Node { name, size, kind, files, cats, newest, children })
    }
    let root = node(&mut r, 0)?;
    Some((ScanResult { root, path: root_path, elapsed, denied, fs_used, cancelled: false }, when))
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.data.get(self.pos..self.pos + n)?;
        self.pos += n;
        Some(s)
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_a_tree() {
        let dir = std::env::temp_dir().join(format!("sophon-space-test-{}", std::process::id()));
        fs::create_dir_all(dir.join("a/b")).unwrap();
        fs::write(dir.join("a/big.bin"), vec![1u8; 3 << 20]).unwrap();
        fs::write(dir.join("a/b/small.txt"), b"hello").unwrap();
        fs::write(dir.join("tiny.txt"), b"x").unwrap();
        let progress = Progress::default();
        let r = scan(&dir, &progress, &ScanOptions::default());
        fs::remove_dir_all(&dir).unwrap();

        let root = &r.root;
        assert_eq!(root.files, 3);
        let a = root.children.iter().find(|c| &*c.name == "a").unwrap();
        assert!(a.children.iter().any(|c| &*c.name == "big.bin" && c.kind == NodeKind::File));
        assert!(a.size >= 3 << 20);
        assert_eq!(root.children[0].name, a.name, "largest first");
    }

    #[test]
    fn remove_updates_ancestors() {
        let node = |name: &str, size, kind, files, children| Node { name: Box::from(name), size, kind, files, cats: [0; 7], newest: 0, children };
        let leaf = node("f", 10, NodeKind::File, 1, vec![]);
        let mid = node("m", 10, NodeKind::Dir, 1, vec![leaf]);
        let mut root = node("r", 15, NodeKind::Dir, 2, vec![mid]);
        let removed = root.remove(&[0, 0]).unwrap();
        assert_eq!(&*removed.name, "f");
        assert_eq!((root.size, root.files), (5, 1));
        assert_eq!(root.children[0].size, 0);
    }

    #[test]
    fn cache_round_trip() {
        let dir = std::env::temp_dir().join(format!("sophon-space-cache-{}", std::process::id()));
        fs::create_dir_all(dir.join("x")).unwrap();
        fs::write(dir.join("x/movie.mkv"), vec![0u8; 2 << 20]).unwrap();
        let r = scan(&dir, &Progress::default(), &ScanOptions::default());
        let file = dir.with_extension("bin");
        save(&r, &file).unwrap();
        let (back, _) = load(&file).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        fs::remove_file(&file).unwrap();
        assert_eq!(back.root.size, r.root.size);
        assert_eq!(back.root.files, r.root.files);
        assert_eq!(back.root.dominant(), 0, "video dominates");
        assert_eq!(back.path, r.path);
    }

    #[test]
    fn categories() {
        assert_eq!(category("Film.MKV"), 0);
        assert_eq!(category("song.flac"), 1);
        assert_eq!(category("game.pak"), 5);
        assert_eq!(category("README"), 6);
    }

    #[test]
    fn mountinfo_unescape() {
        assert_eq!(unescape("/mnt/My\\040Drive"), "/mnt/My Drive");
    }
}
