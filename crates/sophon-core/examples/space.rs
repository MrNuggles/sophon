//! Times a space scan: `cargo run --release -p sophon-core --example space -- /home`

use std::sync::atomic::Ordering::Relaxed;

fn main() {
    let root = std::env::args().nth(1).unwrap_or_else(|| "/home".into());
    for m in sophon_core::space::mounts() {
        println!("mount {:<14} {:<8} {:<10} disk={:<8} {:.1}/{:.1} GB", m.path.display(), m.fs_type, m.device, m.disk, (m.total - m.free) as f64 / 1e9, m.total as f64 / 1e9);
    }
    let progress = sophon_core::space::Progress::default();
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(8);
    let r = sophon_core::space::scan(root.as_ref(), &progress, &sophon_core::space::ScanOptions { threads, ..Default::default() });
    println!("\n{root}: {} files, {} dirs, {:.1} GB in {:?} ({} unreadable)", progress.files.load(Relaxed), progress.dirs.load(Relaxed), r.root.size as f64 / 1e9, r.elapsed, r.denied);
    for c in r.root.children.iter().take(8) {
        println!("  {:>8.2} GB  {:?}  {}", c.size as f64 / 1e9, c.kind, c.name);
    }
    fn count(n: &sophon_core::space::Node) -> usize { 1 + n.children.iter().map(count).sum::<usize>() }
    println!("tree nodes kept: {}", count(&r.root));
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    println!("{}", status.lines().filter(|l| l.starts_with("VmRSS") || l.starts_with("VmHWM")).collect::<Vec<_>>().join("  "));
}
