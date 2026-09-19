//! Human-friendly number formatting.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

const KB: f64 = 1024.0;

static FAHRENHEIT: AtomicBool = AtomicBool::new(false);

pub fn set_fahrenheit(on: bool) {
    FAHRENHEIT.store(on, Relaxed);
}
const MB: f64 = KB * 1024.0;
const GB: f64 = MB * 1024.0;

/// Memory sizes: `512 KB`, `123.4 MB`, `2.70 GB`.
pub fn bytes(b: u64) -> String {
    let b = b as f64;
    if b < MB {
        format!("{:.0} KB", b / KB)
    } else if b < GB {
        format!("{:.1} MB", b / MB)
    } else {
        format!("{:.2} GB", b / GB)
    }
}

/// Memory always in MB: `0.5 MB`, `2,764 MB`.
pub fn bytes_mb(b: u64) -> String {
    let mb = b as f64 / MB;
    if mb < 10.0 { format!("{mb:.1} MB") } else { format!("{} MB", thousands(mb.round() as u64)) }
}

/// Large totals where precision matters less: `2.7 GB`, `16 GB`.
pub fn bytes_short(b: u64) -> String {
    let b = b as f64;
    if b >= 100.0 * GB {
        format!("{:.0} GB", b / GB)
    } else if b >= GB {
        format!("{:.1} GB", b / GB)
    } else {
        format!("{:.0} MB", b / MB)
    }
}

/// Disk-style throughput: `0 KB/s`, `12.3 MB/s`.
pub fn rate_bytes(bps: f64) -> String {
    if bps < MB {
        format!("{:.0} KB/s", bps / KB)
    } else if bps < GB {
        format!("{:.1} MB/s", bps / MB)
    } else {
        format!("{:.2} GB/s", bps / GB)
    }
}

/// Compact throughput for table cells: `512 KB/s`, `12.3 MB/s`, `1.2 GB/s`.
pub fn rate_bytes_short(bps: f64) -> String {
    if bps < MB {
        format!("{:.0} KB/s", bps / KB)
    } else if bps < 100.0 * MB {
        format!("{:.1} MB/s", bps / MB)
    } else if bps < GB {
        format!("{:.0} MB/s", bps / MB)
    } else {
        format!("{:.1} GB/s", bps / GB)
    }
}

/// Network-style throughput in bits: `0 Kbps`, `12.3 Mbps`.
pub fn rate_bits(bytes_per_s: f64) -> String {
    let bits = bytes_per_s * 8.0;
    if bits < 1e6 {
        format!("{:.0} Kbps", bits / 1e3)
    } else if bits < 1e9 {
        format!("{:.1} Mbps", bits / 1e6)
    } else {
        format!("{:.2} Gbps", bits / 1e9)
    }
}

pub fn pct(v: f32) -> String {
    if v < 0.05 {
        "0%".into()
    } else if v < 10.0 {
        format!("{v:.1}%")
    } else {
        format!("{v:.0}%")
    }
}

pub fn ghz(mhz: u32) -> String {
    format!("{:.2} GHz", mhz as f64 / 1000.0)
}

pub fn temp(c: Option<f32>) -> String {
    match c {
        None => "—".into(),
        Some(c) if FAHRENHEIT.load(Relaxed) => format!("{:.0} °F", c * 9.0 / 5.0 + 32.0),
        Some(c) => format!("{c:.0} °C"),
    }
}

pub fn watts(w: Option<f32>) -> String {
    w.map_or_else(|| "—".into(), |w| format!("{w:.0} W"))
}

/// Uptime-style: `3:04:05`, `2d 3:04:05`.
pub fn duration(secs: u64) -> String {
    let (d, h, m, s) = (secs / 86400, secs / 3600 % 24, secs / 60 % 60, secs % 60);
    if d > 0 { format!("{d}d {h}:{m:02}:{s:02}") } else { format!("{h}:{m:02}:{s:02}") }
}

/// Relative age: `12s ago`, `5m ago`, `3h 12m ago`, `2d 4h ago`.
pub fn ago(secs: u64) -> String {
    let (d, h, m) = (secs / 86400, secs / 3600 % 24, secs / 60 % 60);
    if d > 0 {
        format!("{d}d {h}h ago")
    } else if h > 0 {
        format!("{h}h {m}m ago")
    } else if m > 0 {
        format!("{m}m ago")
    } else {
        format!("{secs}s ago")
    }
}

pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(bytes(2 * 1024 * 1024 * 1024 + 700 * 1024 * 1024), "2.68 GB");
        assert_eq!(bytes(500 * 1024), "500 KB");
        assert_eq!(rate_bits(1_250_000.0), "10.0 Mbps");
        assert_eq!(pct(0.01), "0%");
        assert_eq!(pct(4.25), "4.2%");
        assert_eq!(duration(90061), "1d 1:01:01");
        assert_eq!(thousands(1234567), "1,234,567");
    }
}
