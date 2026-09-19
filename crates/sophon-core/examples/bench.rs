//! Measures the sampler's cost: `cargo run --release -p sophon-core --example bench`

use std::time::{Duration, Instant};

fn main() {
    let t = Instant::now();
    let mut s = sophon_core::Sampler::new().expect("sampler");
    println!("startup (incl. .desktop index): {:?}", t.elapsed());
    s.sample(true);
    std::thread::sleep(Duration::from_millis(500));

    let n = 40;
    let mut stages = [0u64; 7];
    let (mut full, mut light) = (Duration::ZERO, Duration::ZERO);
    let mut snap = s.sample(true);
    for i in 0..n {
        std::thread::sleep(Duration::from_millis(50));
        let t = Instant::now();
        snap = s.sample(i % 2 == 0);
        if i % 2 == 0 { full += t.elapsed() } else { light += t.elapsed() }
        if i % 2 == 0 { for (a, b) in stages.iter_mut().zip(snap.own.stage_us) { *a += b as u64 } }
    }
    let half = n as u32 / 2;
    println!("tick with processes:    {:?}", full / half);
    println!("tick without processes: {:?}", light / half);
    println!("stages (us, full ticks): cpu {} mem {} net {} disk {} gpu {} procs {} gpu-procs {}", stages[0]/20, stages[1]/20, stages[2]/20, stages[3]/20, stages[4]/20, stages[5]/20, stages[6]/20);
    println!("processes: {}, threads: {}", snap.processes.len(), snap.thread_count);
    println!("cpu: {} ({}C/{}T) {:.1}%  {} MHz  temp {:?}  power {:?} W",
        snap.cpu.model, snap.cpu.cores, snap.cpu.threads, snap.cpu.usage, snap.cpu.avg_freq_mhz, snap.cpu.temp_c, snap.cpu.power_w);
    println!("mem: {:.1} / {:.1} GiB, zram {:?}", gib(snap.mem.used), gib(snap.mem.total), snap.mem.zram.map(|z| (gib(z.original), gib(z.used))));
    for g in &snap.gpus {
        println!("gpu: {} busy {}%  vram {:.2}/{:.1} GiB  edge {:?} hot {:?}  {:?} W  fan {:?}  {:?}/{:?} MHz  engines {:?}",
            g.name, g.busy, gib(g.vram_used), gib(g.vram_total), g.temp_edge, g.temp_hotspot, g.power_w, g.fan_rpm, g.core_mhz, g.mem_mhz, g.engines);
    }
    for d in &snap.disks {
        println!("disk: {} {} {:.0} GB  r {:.0} B/s w {:.0} B/s active {:.1}%  temp {:?}", d.name, d.model, d.size as f64 / 1e9, d.read_bps, d.write_bps, d.active_pct, d.temp_c);
    }
    for n in &snap.nets {
        println!("net: {} phys={} wifi={} up={} speed={:?} rx {:.0} B/s tx {:.0} B/s", n.name, n.physical, n.wireless, n.up, n.speed_mbps, n.rx_bps, n.tx_bps);
    }
    let mut procs: Vec<_> = snap.processes.iter().collect();
    procs.sort_by(|a, b| b.mem_private.cmp(&a.mem_private));
    println!("\ntop by private memory:");
    for p in procs.iter().take(12) {
        println!("  {:>7} {:<16} {:<28} {:>8.1} MiB  cpu {:>5.2}%  gpu {:>5.1}% {:>7.1} MiB  {:?}  group={}",
            p.pid, p.name, p.display_name(), p.mem_private as f64 / 1048576.0, p.cpu, p.gpu, p.gpu_mem as f64 / 1048576.0, p.kind, p.group);
    }
    // Steady state at the real 1 s tick rate: what the app will actually cost.
    let steady = 10;
    let mut cpu_us = [0u64; 7];
    for _ in 0..steady {
        std::thread::sleep(Duration::from_secs(1));
        snap = s.sample(true);
        for (a, b) in cpu_us.iter_mut().zip(snap.own.stage_us) { *a += b as u64 }
    }
    let total: u64 = cpu_us.iter().sum::<u64>() / steady;
    println!("\nsteady state at 1 Hz, CPU time per tick: {total} us = {:.3}% of one core", total as f64 / 1e4);
    println!("  cpu {} mem {} net {} disk {} gpu {} procs {} gpu-procs {}", cpu_us[0]/steady, cpu_us[1]/steady, cpu_us[2]/steady, cpu_us[3]/steady, cpu_us[4]/steady, cpu_us[5]/steady, cpu_us[6]/steady);
    let mut by_net: Vec<_> = snap.processes.iter().filter(|p| p.net_rx + p.net_tx > 0.0).collect();
    by_net.sort_by(|a, b| (b.net_rx + b.net_tx).total_cmp(&(a.net_rx + a.net_tx)));
    println!("\ntop by TCP traffic:");
    for p in by_net.iter().take(6) {
        println!("  {:>7} {:<16} down {:>10.1} KB/s   up {:>8.1} KB/s", p.pid, p.name, p.net_rx / 1024.0, p.net_tx / 1024.0);
    }
    println!("own: cpu {:.2}% of a core, private {:.1} MiB", snap.own.cpu, snap.own.mem_private as f64 / 1048576.0);
}

fn gib(b: u64) -> f64 {
    b as f64 / (1u64 << 30) as f64
}
