//! GPUs.
//!
//! Device-wide stats come from a vendor backend implementing [`GpuDevice`].
//! Per-process usage comes from DRM fdinfo ([`fdinfo`]), which is vendor-neutral
//! (amdgpu, i915, xe, nouveau, ...). NVIDIA's proprietary driver will slot in
//! later as another `GpuDevice` backed by NVML (loaded with `dlopen`, so it is
//! never a build or runtime requirement) plus an NVML per-process source.

pub mod amdgpu;
pub mod fdinfo;

use std::sync::Arc;

use crate::history::Ring;
use crate::parse;
use crate::sys::{CPath, Dir};

/// Engine classes tracked from fdinfo, in display order.
pub const ENGINES: [&str; 5] = ["3D", "Compute", "Copy", "Video decode", "Video encode"];

#[derive(Clone, Default)]
pub struct GpuState {
    pub name: Arc<str>,
    pub driver: Arc<str>,
    /// PCI address, e.g. `0000:0c:00.0` (matches fdinfo's `drm-pdev`).
    pub pci_slot: Arc<str>,
    pub busy: f32,
    pub mem_busy: Option<f32>,
    pub vram_used: u64,
    pub vram_total: u64,
    pub gtt_used: u64,
    pub gtt_total: u64,
    pub temp_edge: Option<f32>,
    pub temp_hotspot: Option<f32>,
    pub temp_mem: Option<f32>,
    pub power_w: Option<f32>,
    pub power_cap_w: Option<f32>,
    pub fan_rpm: Option<u32>,
    pub core_mhz: Option<u32>,
    pub mem_mhz: Option<u32>,
    /// Per-engine utilisation summed over all clients (see [`ENGINES`]).
    pub engines: [f32; ENGINES.len()],
    pub history: Ring,
    pub vram_history: Ring,
    pub power_history: Ring,
    pub engine_history: [Ring; ENGINES.len()],
}

pub trait GpuDevice: Send {
    fn state(&self) -> &GpuState;
    fn state_mut(&mut self) -> &mut GpuState;
    fn sample(&mut self, buf: &mut Vec<u8>);
}

/// Finds all supported GPUs.
pub fn detect() -> Vec<Box<dyn GpuDevice>> {
    let mut out: Vec<Box<dyn GpuDevice>> = Vec::new();
    let Ok(drm) = Dir::open(c"/sys/class/drm") else { return out };
    let Ok(mut entries) = drm.entries() else { return out };
    let mut buf = Vec::new();
    while let Some(name) = entries.next_name() {
        // cardN only (not connectors like card1-DP-1, not render nodes).
        let Some(num) = name.strip_prefix(b"card") else { continue };
        if !num.iter().all(u8::is_ascii_digit) {
            continue;
        }
        let mut p = CPath::from(name);
        p.push(b"/device");
        let Ok(dev) = drm.open_at(p.as_cstr()) else { continue };
        let driver = link_basename(&dev, c"driver", &mut buf);
        let slot = link_basename(&drm, p.as_cstr(), &mut buf);
        match driver.as_str() {
            "amdgpu" => out.push(Box::new(amdgpu::AmdGpu::open(dev, &driver, &slot))),
            // "i915" | "xe" => Intel backend (todo)
            // "nvidia" => NVML backend (todo)
            _ => {}
        }
    }
    out
}

fn link_basename(dir: &Dir, path: &std::ffi::CStr, buf: &mut Vec<u8>) -> String {
    if dir.read_link(path, buf).is_err() {
        return String::new();
    }
    let base = buf.rsplit(|&b| b == b'/').next().unwrap_or(&[]);
    String::from_utf8_lossy(base).into_owned()
}

/// Marketing name from the system's PCI ID database, e.g. `Radeon RX 6950 XT`.
pub fn pci_device_name(vendor: u16, device: u16) -> Option<String> {
    let data = std::fs::read("/usr/share/hwdata/pci.ids")
        .or_else(|_| std::fs::read("/usr/share/misc/pci.ids"))
        .ok()?;
    let vendor_hex = format!("{vendor:04x}");
    let device_hex = format!("{device:04x}");
    let mut in_vendor = false;
    for line in data.split(|&b| b == b'\n') {
        if line.first().is_some_and(|b| b.is_ascii_hexdigit()) {
            if in_vendor {
                return None;
            }
            in_vendor = line.starts_with(vendor_hex.as_bytes());
        } else if in_vendor
            && let Some(rest) = line.strip_prefix(b"\t")
            && rest.starts_with(device_hex.as_bytes())
        {
            let full = String::from_utf8_lossy(parse::trim(&rest[4..])).into_owned();
            // "Navi 21 [Radeon RX 6950 XT]" -> "Radeon RX 6950 XT"
            let short = match (full.rfind('['), full.rfind(']')) {
                (Some(a), Some(b)) if b > a => full[a + 1..b].to_string(),
                _ => full,
            };
            return Some(short);
        }
    }
    None
}

pub(crate) fn read_hex(dir: &Dir, path: &std::ffi::CStr, buf: &mut Vec<u8>) -> u16 {
    if dir.read(path, buf).is_err() {
        return 0;
    }
    let s = parse::trim(buf);
    let s = s.strip_prefix(b"0x").unwrap_or(s);
    u16::from_str_radix(std::str::from_utf8(s).unwrap_or("0"), 16).unwrap_or(0)
}
