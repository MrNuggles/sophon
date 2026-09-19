//! AMD GPUs via the amdgpu sysfs interface and its hwmon.

use super::{GpuDevice, GpuState, pci_device_name, read_hex};
use crate::mem::pct;
use crate::sys::{self, Dir};

pub struct AmdGpu {
    state: GpuState,
    dev: Dir,
    hwmon: Option<Dir>,
}

impl AmdGpu {
    pub fn open(dev: Dir, driver: &str, slot: &str) -> Self {
        let mut buf = Vec::new();
        let vendor = read_hex(&dev, c"vendor", &mut buf);
        let device = read_hex(&dev, c"device", &mut buf);
        let name = pci_device_name(vendor, device).unwrap_or_else(|| format!("AMD GPU {device:04x}"));
        let name = if name.starts_with("AMD") { name } else { format!("AMD {name}") };
        let hwmon = sys::device_hwmon(&dev, b".");
        let state = GpuState {
            name: name.into(),
            driver: driver.into(),
            pci_slot: slot.into(),
            vram_total: dev.read_u64(c"mem_info_vram_total", &mut buf).unwrap_or(0),
            gtt_total: dev.read_u64(c"mem_info_gtt_total", &mut buf).unwrap_or(0),
            power_cap_w: hwmon
                .as_ref()
                .and_then(|h| h.read_u64(c"power1_cap", &mut buf))
                .map(|uw| uw as f32 / 1e6),
            ..Default::default()
        };
        Self { state, dev, hwmon }
    }
}

impl GpuDevice for AmdGpu {
    fn state(&self) -> &GpuState {
        &self.state
    }

    fn state_mut(&mut self) -> &mut GpuState {
        &mut self.state
    }

    fn sample(&mut self, buf: &mut Vec<u8>) {
        let s = &mut self.state;
        let d = &self.dev;
        s.busy = d.read_u64(c"gpu_busy_percent", buf).unwrap_or(0) as f32;
        s.mem_busy = d.read_u64(c"mem_busy_percent", buf).map(|v| v as f32);
        s.vram_used = d.read_u64(c"mem_info_vram_used", buf).unwrap_or(0);
        s.gtt_used = d.read_u64(c"mem_info_gtt_used", buf).unwrap_or(0);
        s.history.push(s.busy);
        s.vram_history.push(pct(s.vram_used, s.vram_total));

        if let Some(h) = &self.hwmon {
            let temp = |p, buf: &mut Vec<u8>| h.read_u64(p, buf).map(|m| m as f32 / 1000.0);
            s.temp_edge = temp(c"temp1_input", buf);
            s.temp_hotspot = temp(c"temp2_input", buf);
            s.temp_mem = temp(c"temp3_input", buf);
            s.power_w = h
                .read_u64(c"power1_average", buf)
                .or_else(|| h.read_u64(c"power1_input", buf))
                .map(|uw| uw as f32 / 1e6);
            s.fan_rpm = h.read_u64(c"fan1_input", buf).map(|v| v as u32);
            s.core_mhz = h.read_u64(c"freq1_input", buf).map(|hz| (hz / 1_000_000) as u32);
            s.mem_mhz = h.read_u64(c"freq2_input", buf).map(|hz| (hz / 1_000_000) as u32);
            if let Some(w) = s.power_w {
                s.power_history.push(w);
            }
        }
    }
}
