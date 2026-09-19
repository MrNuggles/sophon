//! User settings: defaults, a small hand-editable config file, and the
//! descriptor table the Settings page is generated from.
//!
//! File: `~/.config/sophon/settings.conf`, one `section.key = value` per line.
//! Unknown keys are ignored and missing ones keep their defaults, so the file
//! survives upgrades in both directions.

use std::cell::RefCell;
use std::path::PathBuf;

/// Process table columns, in their default order: (id, label, default width px).
pub const COLUMNS: [(&str, &str, u32); 9] = [
    ("spark", "Activity", 64),
    ("cpu", "CPU", 68),
    ("mem", "Memory", 90),
    ("gpu", "GPU", 62),
    ("gmem", "GPU memory", 86),
    ("down", "Down", 80),
    ("up", "Up", 80),
    ("pid", "PID", 64),
    ("user", "User", 80),
];

pub const SWATCHES: [u32; 12] = [
    0x8f7dff, 0x3db4ff, 0x2fd39a, 0xffb13d, 0xff6fa5, 0xff7a59, 0x4dd0e1, 0xc0ca33, 0xf06292, 0x7c8cff, 0xe5e7eb, 0x94a3b8,
];

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    // General
    pub autostart: bool,
    pub start_in_tray: bool,
    pub close_to_tray: bool,
    /// 0 = open, 1 = toggle.
    pub tray_click: usize,
    /// 0 = never, 1 = app groups only, 2 = always.
    pub confirm_end: usize,
    pub admin_on_start: bool,
    // Appearance
    /// 0 = system, 1 = light, 2 = dark.
    pub theme: usize,
    pub accent_follow: bool,
    pub accent: u32,
    pub color_cpu: u32,
    pub color_mem: u32,
    pub color_gpu: u32,
    pub color_disk: u32,
    pub color_net: u32,
    pub ui_scale: f32,
    pub font_scale: f32,
    /// 0 = comfortable, 1 = compact.
    pub density: usize,
    pub roundness: f32,
    /// 0 = gradient, 1 = filled, 2 = line, 3 = bars.
    pub graph_style: usize,
    pub line_width: f32,
    pub graph_grid: bool,
    pub heat_intensity: f32,
    pub heat_color: u32,
    /// 0 = full, 1 = reduced, 2 = off.
    pub animations: usize,
    // Performance
    /// Index into UPDATE_MS.
    pub update_speed: usize,
    /// Index into TRAY_SECS.
    pub tray_rate: usize,
    /// 0 = CPU (software), 1 = GPU (OpenGL).
    pub renderer: usize,
    pub show_own_usage: bool,
    // Processes
    /// Column ids in display order.
    pub columns: Vec<String>,
    pub hidden_columns: Vec<String>,
    pub column_widths: Vec<(String, u32)>,
    pub sort_col: i32,
    pub sort_desc: bool,
    /// 0 = by app, 1 = tree, 2 = flat.
    pub grouping: usize,
    pub show_kernel: bool,
    pub show_other_users: bool,
    /// 0 = friendly, 1 = process name, 2 = both.
    pub name_style: usize,
    /// 0 = % of whole machine, 1 = % of one core.
    pub cpu_mode: usize,
    /// 0 = automatic, 1 = always MB.
    pub mem_units: usize,
    /// 0 = bytes, 1 = bits.
    pub net_units: usize,
    /// 0 = 30 s, 1 = 60 s.
    pub spark_len: usize,
    /// 0 = details, 1 = expand, 2 = open file location.
    pub double_click: usize,
    /// 0 = end task, 1 = ask first, 2 = force kill.
    pub delete_key: usize,
    // Performance tab
    /// Stable tile ids in display order (e.g. "cpu", "disk:nvme0n1").
    pub tile_order: Vec<String>,
    pub hidden_tiles: Vec<String>,
    pub show_virtual_nics: bool,
    /// 0 = total only, 1 = per thread, 2 = per core.
    pub cpu_view: usize,
    /// Index into RANGE_SECS.
    pub default_range: usize,
    /// 0 = 10 min, 1 = 1 h, 2 = 24 h.
    pub history: usize,
    /// 0 = off, 1 = low, 2 = normal, 3 = high.
    pub spike_sensitivity: usize,
    pub fahrenheit: bool,
    pub show_power: bool,
    // Space map
    /// Mount path, or empty for "the drive with my home folder".
    pub space_default: String,
    /// 0 = by folder, 1 = by file type, 2 = by age.
    pub space_color: usize,
    pub space_min_block: f32,
    pub space_hidden_files: bool,
    pub space_excluded: Vec<String>,
    /// 0 = idle (yields to games), 1 = normal.
    pub space_priority: usize,
    pub space_remember: bool,
    // Tray
    pub tip_cpu: bool,
    pub tip_ram: bool,
    pub tip_gpu: bool,
    pub tip_temps: bool,
    pub tip_net: bool,
}

pub const UPDATE_MS: [u64; 5] = [500, 1000, 2000, 5000, 0];
pub const TRAY_SECS: [u64; 4] = [5, 10, 30, 0];
pub const RANGE_SECS: [f32; 5] = [60.0, 300.0, 600.0, 3600.0, 86400.0];

impl Default for Settings {
    fn default() -> Self {
        Settings {
            autostart: false,
            start_in_tray: false,
            close_to_tray: true,
            tray_click: 0,
            confirm_end: 1,
            admin_on_start: false,
            theme: 0,
            accent_follow: false,
            accent: 0x8f7dff,
            color_cpu: 0x3db4ff,
            color_mem: 0xb27bff,
            color_gpu: 0x2fd39a,
            color_disk: 0xffb13d,
            color_net: 0xff6fa5,
            ui_scale: 1.0,
            font_scale: 1.0,
            density: 0,
            roundness: 1.0,
            graph_style: 0,
            line_width: 1.0,
            graph_grid: true,
            heat_intensity: 1.0,
            heat_color: 0xff9d3c,
            animations: 0,
            update_speed: 1,
            tray_rate: 0,
            renderer: 0,
            show_own_usage: true,
            columns: COLUMNS.iter().map(|c| c.0.to_string()).collect(),
            hidden_columns: Vec::new(),
            column_widths: Vec::new(),
            sort_col: 1,
            sort_desc: true,
            grouping: 0,
            show_kernel: false,
            show_other_users: true,
            name_style: 2,
            cpu_mode: 0,
            mem_units: 0,
            net_units: 0,
            spark_len: 0,
            double_click: 1,
            delete_key: 0,
            tile_order: Vec::new(),
            hidden_tiles: Vec::new(),
            show_virtual_nics: false,
            cpu_view: 1,
            default_range: 0,
            history: 0,
            spike_sensitivity: 2,
            fahrenheit: false,
            show_power: true,
            space_default: String::new(),
            space_color: 0,
            space_min_block: 3.0,
            space_hidden_files: true,
            space_excluded: Vec::new(),
            space_priority: 0,
            space_remember: true,
            tip_cpu: true,
            tip_ram: true,
            tip_gpu: true,
            tip_temps: true,
            tip_net: false,
        }
    }
}

thread_local! {
    static CURRENT: RefCell<Settings> = RefCell::new(Settings::load());
}

/// Reads the current settings.
pub fn get<R>(f: impl FnOnce(&Settings) -> R) -> R {
    CURRENT.with_borrow(f)
}

/// Changes settings and saves them. Returns what the closure returned.
pub fn update<R>(f: impl FnOnce(&mut Settings) -> R) -> R {
    let r = CURRENT.with_borrow_mut(f);
    get(|s| s.save());
    r
}

pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"))
        .join("sophon")
}

impl Settings {
    pub fn path() -> PathBuf {
        config_dir().join("settings.conf")
    }

    pub fn load() -> Self {
        let mut s = Settings::default();
        if let Ok(text) = std::fs::read_to_string(Self::path()) {
            s.parse(&text);
        }
        s
    }

    pub fn save(&self) {
        let _ = std::fs::create_dir_all(config_dir());
        let _ = std::fs::write(Self::path(), self.serialize());
    }

    pub fn column_width(&self, id: &str) -> u32 {
        self.column_widths
            .iter()
            .find(|(c, _)| c == id)
            .map(|(_, w)| *w)
            .or_else(|| COLUMNS.iter().find(|c| c.0 == id).map(|c| c.2))
            .unwrap_or(80)
    }

    fn parse(&mut self, text: &str) {
        let (mut columns, mut excluded) = (Vec::new(), Vec::new());
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else { continue };
            let (key, value) = (key.trim(), value.trim());
            let b = || value == "true";
            let n = || value.parse::<usize>().unwrap_or(0);
            let f = || value.parse::<f32>().unwrap_or(1.0);
            let c = || u32::from_str_radix(value.trim_start_matches('#'), 16).unwrap_or(0);
            let list = || value.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect::<Vec<_>>();
            match key {
                "general.autostart" => self.autostart = b(),
                "general.start_in_tray" => self.start_in_tray = b(),
                "general.close_to_tray" => self.close_to_tray = b(),
                "general.tray_click" => self.tray_click = n().min(1),
                "general.confirm_end" => self.confirm_end = n().min(2),
                "general.admin_on_start" => self.admin_on_start = b(),
                "appearance.theme" => self.theme = n().min(2),
                "appearance.accent_follow" => self.accent_follow = b(),
                "appearance.accent" => self.accent = c(),
                "appearance.color_cpu" => self.color_cpu = c(),
                "appearance.color_mem" => self.color_mem = c(),
                "appearance.color_gpu" => self.color_gpu = c(),
                "appearance.color_disk" => self.color_disk = c(),
                "appearance.color_net" => self.color_net = c(),
                "appearance.ui_scale" => self.ui_scale = f().clamp(0.75, 2.0),
                "appearance.font_scale" => self.font_scale = f().clamp(0.75, 1.5),
                "appearance.density" => self.density = n().min(1),
                "appearance.roundness" => self.roundness = f().clamp(0.0, 2.0),
                "appearance.graph_style" => self.graph_style = n().min(3),
                "appearance.line_width" => self.line_width = f().clamp(0.5, 3.0),
                "appearance.graph_grid" => self.graph_grid = b(),
                "appearance.heat_intensity" => self.heat_intensity = f().clamp(0.0, 2.0),
                "appearance.heat_color" => self.heat_color = c(),
                "appearance.animations" => self.animations = n().min(2),
                "performance.update_speed" => self.update_speed = n().min(UPDATE_MS.len() - 1),
                "performance.tray_rate" => self.tray_rate = n().min(TRAY_SECS.len() - 1),
                "performance.renderer" => self.renderer = n().min(1),
                "performance.show_own_usage" => self.show_own_usage = b(),
                "processes.columns" => columns = list(),
                "processes.hidden_columns" => self.hidden_columns = list(),
                "processes.column_widths" => {
                    self.column_widths = list()
                        .iter()
                        .filter_map(|p| p.split_once(':').and_then(|(c, w)| Some((c.to_string(), w.parse().ok()?))))
                        .collect()
                }
                "processes.sort_col" => self.sort_col = value.parse().unwrap_or(1),
                "processes.sort_desc" => self.sort_desc = b(),
                "processes.grouping" => self.grouping = n().min(2),
                "processes.show_kernel" => self.show_kernel = b(),
                "processes.show_other_users" => self.show_other_users = b(),
                "processes.name_style" => self.name_style = n().min(2),
                "processes.cpu_mode" => self.cpu_mode = n().min(1),
                "processes.mem_units" => self.mem_units = n().min(1),
                "processes.net_units" => self.net_units = n().min(1),
                "processes.spark_len" => self.spark_len = n().min(1),
                "processes.double_click" => self.double_click = n().min(2),
                "processes.delete_key" => self.delete_key = n().min(2),
                "performance_tab.tile_order" => self.tile_order = list(),
                "performance_tab.hidden_tiles" => self.hidden_tiles = list(),
                "performance_tab.show_virtual_nics" => self.show_virtual_nics = b(),
                "performance_tab.cpu_view" => self.cpu_view = n().min(2),
                "performance_tab.default_range" => self.default_range = n().min(RANGE_SECS.len() - 1),
                "performance_tab.history" => self.history = n().min(2),
                "performance_tab.spike_sensitivity" => self.spike_sensitivity = n().min(3),
                "performance_tab.fahrenheit" => self.fahrenheit = b(),
                "performance_tab.show_power" => self.show_power = b(),
                "space.default" => self.space_default = value.to_string(),
                "space.color" => self.space_color = n().min(2),
                "space.min_block" => self.space_min_block = f().clamp(1.0, 20.0),
                "space.hidden_files" => self.space_hidden_files = b(),
                "space.exclude" => excluded.push(value.to_string()),
                "space.priority" => self.space_priority = n().min(1),
                "space.remember" => self.space_remember = b(),
                "tray.cpu" => self.tip_cpu = b(),
                "tray.ram" => self.tip_ram = b(),
                "tray.gpu" => self.tip_gpu = b(),
                "tray.temps" => self.tip_temps = b(),
                "tray.net" => self.tip_net = b(),
                _ => {}
            }
        }
        // Keep every known column exactly once, in the saved order.
        if !columns.is_empty() {
            let mut order: Vec<String> = columns.into_iter().filter(|c| COLUMNS.iter().any(|k| k.0 == c)).collect();
            for (id, ..) in COLUMNS {
                if !order.iter().any(|c| c == id) {
                    order.push(id.to_string());
                }
            }
            self.columns = order;
        }
        self.space_excluded = excluded;
    }

    fn serialize(&self) -> String {
        let hex = |c: u32| format!("#{c:06x}");
        let mut o = String::from("# Sophon settings. Edit while Sophon is closed, or use the Settings tab.\n");
        let mut kv = |k: &str, v: String| {
            o.push_str(k);
            o.push_str(" = ");
            o.push_str(&v);
            o.push('\n');
        };
        kv("general.autostart", self.autostart.to_string());
        kv("general.start_in_tray", self.start_in_tray.to_string());
        kv("general.close_to_tray", self.close_to_tray.to_string());
        kv("general.tray_click", self.tray_click.to_string());
        kv("general.confirm_end", self.confirm_end.to_string());
        kv("general.admin_on_start", self.admin_on_start.to_string());
        kv("appearance.theme", self.theme.to_string());
        kv("appearance.accent_follow", self.accent_follow.to_string());
        kv("appearance.accent", hex(self.accent));
        kv("appearance.color_cpu", hex(self.color_cpu));
        kv("appearance.color_mem", hex(self.color_mem));
        kv("appearance.color_gpu", hex(self.color_gpu));
        kv("appearance.color_disk", hex(self.color_disk));
        kv("appearance.color_net", hex(self.color_net));
        kv("appearance.ui_scale", self.ui_scale.to_string());
        kv("appearance.font_scale", self.font_scale.to_string());
        kv("appearance.density", self.density.to_string());
        kv("appearance.roundness", self.roundness.to_string());
        kv("appearance.graph_style", self.graph_style.to_string());
        kv("appearance.line_width", self.line_width.to_string());
        kv("appearance.graph_grid", self.graph_grid.to_string());
        kv("appearance.heat_intensity", self.heat_intensity.to_string());
        kv("appearance.heat_color", hex(self.heat_color));
        kv("appearance.animations", self.animations.to_string());
        kv("performance.update_speed", self.update_speed.to_string());
        kv("performance.tray_rate", self.tray_rate.to_string());
        kv("performance.renderer", self.renderer.to_string());
        kv("performance.show_own_usage", self.show_own_usage.to_string());
        kv("processes.columns", self.columns.join(","));
        kv("processes.hidden_columns", self.hidden_columns.join(","));
        kv(
            "processes.column_widths",
            self.column_widths.iter().map(|(c, w)| format!("{c}:{w}")).collect::<Vec<_>>().join(","),
        );
        kv("processes.sort_col", self.sort_col.to_string());
        kv("processes.sort_desc", self.sort_desc.to_string());
        kv("processes.grouping", self.grouping.to_string());
        kv("processes.show_kernel", self.show_kernel.to_string());
        kv("processes.show_other_users", self.show_other_users.to_string());
        kv("processes.name_style", self.name_style.to_string());
        kv("processes.cpu_mode", self.cpu_mode.to_string());
        kv("processes.mem_units", self.mem_units.to_string());
        kv("processes.net_units", self.net_units.to_string());
        kv("processes.spark_len", self.spark_len.to_string());
        kv("processes.double_click", self.double_click.to_string());
        kv("processes.delete_key", self.delete_key.to_string());
        kv("performance_tab.tile_order", self.tile_order.join(","));
        kv("performance_tab.hidden_tiles", self.hidden_tiles.join(","));
        kv("performance_tab.show_virtual_nics", self.show_virtual_nics.to_string());
        kv("performance_tab.cpu_view", self.cpu_view.to_string());
        kv("performance_tab.default_range", self.default_range.to_string());
        kv("performance_tab.history", self.history.to_string());
        kv("performance_tab.spike_sensitivity", self.spike_sensitivity.to_string());
        kv("performance_tab.fahrenheit", self.fahrenheit.to_string());
        kv("performance_tab.show_power", self.show_power.to_string());
        kv("space.default", self.space_default.clone());
        kv("space.color", self.space_color.to_string());
        kv("space.min_block", self.space_min_block.to_string());
        kv("space.hidden_files", self.space_hidden_files.to_string());
        for e in &self.space_excluded {
            kv("space.exclude", e.clone());
        }
        kv("space.priority", self.space_priority.to_string());
        kv("space.remember", self.space_remember.to_string());
        kv("tray.cpu", self.tip_cpu.to_string());
        kv("tray.ram", self.tip_ram.to_string());
        kv("tray.gpu", self.tip_gpu.to_string());
        kv("tray.temps", self.tip_temps.to_string());
        kv("tray.net", self.tip_net.to_string());
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let mut s = Settings::default();
        s.theme = 2;
        s.accent = 0x123456;
        s.columns.swap(1, 2);
        s.hidden_columns = vec!["pid".into()];
        s.column_widths = vec![("cpu".into(), 100)];
        s.space_excluded = vec!["/home/me/a, b".into(), "/tmp".into()];
        s.font_scale = 1.2;
        let mut back = Settings::default();
        back.parse(&s.serialize());
        assert_eq!(back, s);
    }

    #[test]
    fn tolerates_junk_and_missing_columns() {
        let mut s = Settings::default();
        s.parse("# hi\nnonsense\nprocesses.columns = mem,bogus,cpu\nappearance.theme = 9\n");
        assert_eq!(s.columns[0], "mem");
        assert_eq!(s.columns[1], "cpu");
        assert_eq!(s.columns.len(), COLUMNS.len());
        assert_eq!(s.theme, 2);
    }
}
