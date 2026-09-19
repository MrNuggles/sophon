//! UI state: turns [`Snapshot`]s into Slint models and handles user actions.
//! Everything here runs on the UI thread.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant, SystemTime};

use slint::{ComponentHandle, Model, ModelRc, SharedString, TimerMode, VecModel};
use sophon_core::Snapshot;
use sophon_core::apps::Kind;
use sophon_core::gpu::ENGINES;
use sophon_core::history::Ring;
use sophon_core::mem::pct;
use sophon_core::process::{Process, SPARK};

use crate::actions::{self, Signal};
use crate::settings::{self, COLUMNS};
use crate::graph::{self, Timeline, nice_max, rgb, tint};
use crate::{
    AppWindow, Chart, Cols, HeadInfo, Hover, Link, Marker, Mini, Series, PerfDetail, ProcDetail, ProcRow, Segment, Shared, Stat, Tile, Totals, Tray, fmt,
};

// Metric colours come from settings; second series use fixed companions.
fn c_cpu() -> u32 {
    settings::get(|s| s.color_cpu)
}
fn c_mem() -> u32 {
    settings::get(|s| s.color_mem)
}
fn c_gpu() -> u32 {
    settings::get(|s| s.color_gpu)
}
fn c_disk() -> u32 {
    settings::get(|s| s.color_disk)
}
fn c_net() -> u32 {
    settings::get(|s| s.color_net)
}
const DISK_WRITE: u32 = 0xff7a59;
const NET_TX: u32 = 0x7c8cff;

/// A redraw that hasn't reached the screen after this long means the window is hidden.
const STALL: Duration = Duration::from_millis(2500);

thread_local! {
    static STATE: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    STATE.with_borrow_mut(|s| s.as_mut().map(f))
}

#[derive(Clone, PartialEq, Default)]
enum Sel {
    #[default]
    None,
    Pid(u32),
    Group(String),
    /// A section header (Apps / Background / System); clicking collapses it.
    Section(Kind),
}

#[derive(Clone, Copy, PartialEq)]
enum TileId {
    Cpu,
    Mem,
    Gpu(usize),
    Disk(usize),
    Net(usize),
}

struct Ui {
    app: slint::Weak<AppWindow>,
    shared: Arc<Shared>,
    snap: Snapshot,
    filter: String,
    sort_col: i32,
    sort_desc: bool,
    expanded: HashSet<String>,
    collapsed_sections: HashSet<Kind>,
    /// Tree mode: processes whose children are hidden.
    tree_collapsed: HashSet<u32>,
    /// An action waiting for the confirmation dialog.
    pending: Option<(String, Sel)>,
    own_uid: u32,
    selected: Sel,
    rows: Rc<VecModel<ProcRow>>,
    heads: Rc<VecModel<HeadInfo>>,
    row_sel: Vec<Sel>,
    tiles: Rc<VecModel<Tile>>,
    tile_ids: Vec<TileId>,
    /// Visible time span of Performance graphs, in seconds.
    view_span: f32,
    /// End of the visible range when paused (zoomed into the past); `None` follows live.
    view_end: Option<f32>,
    /// Wall-clock time matching `snap.now`, for labelling samples.
    snap_wall: SystemTime,
    /// Who was busiest at each sample, for naming spike culprits.
    moments: VecDeque<Moment>,
    epoch: i32,
    tier10: Tier,
    tier60: Tier,
    long_fed: u64,
    toast_timer: slint::Timer,
    /// When a redraw was requested that hasn't been presented yet.
    frame_pending: Option<Instant>,
    tray: Option<slint::Weak<Tray>>,
    show_own: bool,
    /// The Settings overlay (opened from the header ☰) is showing; nothing
    /// under it is rendered or sampled at full rate.
    settings_open: bool,
    /// Frames presented since the last stall check (debug output).
    frames: u32,
    /// Window size when it was closed to the tray.
    tray_size: Option<slint::PhysicalSize>,
    /// Last summary pushed to the tray (it's only sent over D-Bus when it changes).
    tray_text: String,
    /// With SOPHON_DEBUG: (model-build µs, ticks, UI-thread CPU µs at window start).
    debug: Option<(u64, u64, u64)>,
}

pub fn install(app: &AppWindow, shared: Arc<Shared>) {
    let rows = Rc::new(VecModel::default());
    let tiles = Rc::new(VecModel::default());
    app.set_rows(ModelRc::from(rows.clone()));
    app.set_tiles(ModelRc::from(tiles.clone()));
    STATE.set(Some(Ui {
        app: app.as_weak(),
        shared,
        snap: Snapshot::default(),
        filter: String::new(),
        sort_col: settings::get(|s| s.sort_col),
        sort_desc: settings::get(|s| s.sort_desc),
        expanded: HashSet::new(),
        collapsed_sections: HashSet::new(),
        tree_collapsed: HashSet::new(),
        pending: None,
        own_uid: unsafe { libc::getuid() },
        selected: Sel::None,
        rows,
        heads: Rc::new(VecModel::default()),
        row_sel: Vec::new(),
        tiles,
        tile_ids: Vec::new(),
        view_span: settings::RANGE_SECS[settings::get(|s| s.default_range)],
        view_end: None,
        snap_wall: SystemTime::now(),
        moments: VecDeque::new(),
        epoch: 0,
        tier10: Tier::new(10.0),
        tier60: Tier::new(60.0),
        long_fed: 0,
        toast_timer: slint::Timer::default(),
        frame_pending: None,
        tray: None,
        frames: 0,
        show_own: true,
        settings_open: false,
        tray_size: None,
        tray_text: String::new(),
        debug: std::env::var_os("SOPHON_DEBUG").map(|_| (0, 0, sophon_core::sys::thread_cpu_us())),
    }));

    app.on_tab_changed(|tab| {
        with(|ui| ui.tab_changed(tab));
    });
    app.on_filter_changed(|text| {
        with(|ui| {
            ui.filter = text.trim().to_lowercase();
            ui.render_processes();
        });
    });
    app.on_sort_by(|col| {
        with(|ui| ui.sort_by(col));
    });
    app.on_select_row(|i| {
        with(|ui| ui.select_row(i as usize));
    });
    app.on_toggle_row(|i| {
        with(|ui| ui.toggle_row(i as usize));
    });
    app.on_row_action(|i, action| {
        with(|ui| {
            ui.select_row(i as usize);
            ui.act(&action);
        });
    });
    app.on_options_changed(|| {
        with(|ui| {
            if let Some(app) = ui.app.upgrade() {
                let (grouping, kernel) = (app.get_grouping().clamp(0, 2) as usize, app.get_show_kernel());
                settings::update(|s| {
                    s.grouping = grouping;
                    s.show_kernel = kernel;
                });
            }
            ui.render_processes();
        });
        crate::settings_page::refresh();
    });
    app.on_activate_row(|i| {
        with(|ui| {
            match settings::get(|s| s.double_click) {
                0 => ui.select_row(i as usize),
                1 => ui.toggle_row(i as usize),
                _ => {
                    ui.select_row(i as usize);
                    ui.act("open");
                }
            }
        });
    });
    app.on_column_dropped(|col, frac| {
        with(|ui| ui.drop_column(col, frac));
    });
    app.on_column_resized(|col, width| {
        if let Some(&(id, ..)) = COLUMNS.get(col.max(0) as usize) {
            settings::update(|s| {
                s.column_widths.retain(|(c, _)| c != id);
                s.column_widths.push((id.to_string(), width.clamp(36.0, 400.0) as u32));
            });
        }
        with(|ui| ui.render_processes());
        crate::settings_page::refresh();
    });
    app.on_column_hide(|col| {
        settings::update(|s| match col {
            -1 => s.hidden_columns.clear(),
            -2 => {
                let d = settings::Settings::default();
                s.columns = d.columns;
                s.hidden_columns.clear();
                s.column_widths.clear();
            }
            c => {
                if let Some(&(id, ..)) = COLUMNS.get(c.max(0) as usize)
                    && !s.hidden_columns.iter().any(|h| h == id)
                {
                    s.hidden_columns.push(id.to_string());
                }
            }
        });
        with(|ui| ui.render_processes());
        crate::settings_page::refresh();
    });
    app.on_confirm_accepted(|| {
        with(|ui| {
            if let Some(app) = ui.app.upgrade() {
                app.set_confirm_text(SharedString::new());
            }
            if let Some((action, sel)) = ui.pending.take() {
                ui.selected = sel;
                ui.run_action(&action);
            }
        });
        // The same dialog also serves the Docker tab's stop confirmation.
        #[cfg(feature = "docker")]
        crate::docker_panel::confirm_accepted();
    });
    app.on_confirm_cancelled(|| {
        with(|ui| {
            ui.pending = None;
            if let Some(app) = ui.app.upgrade() {
                app.set_confirm_text(SharedString::new());
            }
        });
        #[cfg(feature = "docker")]
        crate::docker_panel::cancel_confirm();
    });
    app.on_go(|pid| {
        with(|ui| ui.go(pid as u32));
    });
    app.on_detail_action(|action| {
        with(|ui| ui.act(&action));
    });
    app.on_close_detail(|| {
        with(|ui| {
            ui.selected = Sel::None;
            ui.render_processes();
        });
    });
    app.on_tile_action(|i, action| {
        with(|ui| ui.tile_action(i.max(0) as usize, &action));
    });
    app.global::<Hover>().set_range(settings::get(|s| s.default_range) as i32);
    app.on_tile_changed(|_| {
        with(|ui| {
            if let Some(app) = ui.app.upgrade() {
                app.set_perf_scroll(0.0);
            }
            ui.render_perf();
        });
    });
    let hover = app.global::<Hover>();
    hover.on_probe(|key, x, _| {
        // Evaluated while Slint renders; never while our own state is borrowed.
        STATE.with(|s| s.try_borrow().ok().and_then(|g| g.as_ref().map(|ui| ui.probe(&key, x)))).unwrap_or_default().into()
    });
    hover.on_zoom(|from, to| {
        with(|ui| {
            let tl = ui.timeline();
            let (a, b) = (tl.time_at(from), tl.time_at(to));
            if b - a >= 5.0 {
                ui.view_end = Some(b);
                ui.view_span = b - a;
            }
            ui.render_perf();
        });
    });
    hover.on_reset(|| {
        with(|ui| {
            ui.view_end = None;
            ui.view_span = settings::RANGE_SECS[ui.app.upgrade().map_or(0, |a| a.global::<Hover>().get_range().clamp(0, 4) as usize)];
            ui.render_perf();
        });
    });
    hover.on_set_range(|i| {
        with(|ui| {
            ui.view_end = None;
            ui.view_span = settings::RANGE_SECS[i.clamp(0, 4) as usize];
            ui.render_perf();
        });
    });
    hover.on_marker(|pid| {
        with(|ui| {
            if pid <= 0 {
                return;
            }
            if let Some(app) = ui.app.upgrade() {
                app.set_tab(0);
            }
            ui.tab_changed(0);
            ui.go(pid as u32);
        });
    });
    app.on_delete_pressed(|shift| {
        with(|ui| match settings::get(|s| s.delete_key) {
            // End task (Shift forces), ask first, or force kill.
            0 => ui.act(if shift { "kill" } else { "end" }),
            1 => ui.confirm(if shift { "kill" } else { "end" }),
            _ => ui.act("kill"),
        });
    });
    let (grouping, kernel) = settings::get(|s| (s.grouping as i32, s.show_kernel));
    app.set_grouping(grouping);
    app.set_show_kernel(kernel);
}

pub fn set_tray(tray: slint::Weak<Tray>) {
    with(|ui| ui.tray = Some(tray));
}

/// Shows a short message at the bottom of the window.
pub fn toast(msg: impl Into<SharedString>) {
    with(|ui| ui.toast(msg));
}

/// Drops a pending Processes confirmation without acting (the Docker tab is
/// taking over the shared dialog).
#[cfg(feature = "docker")]
pub fn cancel_confirm() {
    with(|ui| ui.pending = None);
}

/// Called with each new snapshot from the sampler thread (via the event loop).
/// Keeps sampling decisions in step with the Settings overlay: while it's
/// open nothing under it is rendered, and the process table isn't sampled at
/// full rate (the Docker poller likewise drops to its hidden cadence).
fn sync_settings_open(ui: &mut Ui) {
    let Some(app) = ui.app.upgrade() else { return };
    let open = app.get_settings_open();
    if open == ui.settings_open {
        return;
    }
    ui.settings_open = open;
    let id = if open { "settings" } else { crate::panels::id_at(app.get_tab()) };
    ui.shared.processes_visible.store(id == "processes", Relaxed);
    crate::panels::set_visible(id);
    if !open && id == "processes" {
        let _ = ui.shared.wake.send(());
    }
}

pub fn apply(snap: Snapshot) {
    with(|ui| {
        sync_settings_open(ui);
        let t0 = sophon_core::sys::thread_cpu_us();
        ui.snap = snap;
        ui.snap_wall = SystemTime::now();
        ui.record_moment();
        ui.feed_long_history();
        ui.frame_pending.get_or_insert_with(Instant::now);
        ui.render();
        if let Some(stats) = &mut ui.debug {
            let now = sophon_core::sys::thread_cpu_us();
            stats.0 += now - t0;
            stats.1 += 1;
            if stats.1 == 10 {
                eprintln!(
                    "sophon: model build {} µs/tick, UI thread total {} µs/tick",
                    stats.0 / 10,
                    (now - stats.2) / 10
                );
                *stats = (0, 0, now);
            }
        }
    });
}

/// A frame reached the compositor: the window is visible.
pub fn frame_presented(shared: &Shared) {
    with(|ui| {
        ui.frame_pending = None;
        ui.frames += 1;
    });
    if !shared.in_tray.load(Relaxed) {
        set_hidden(shared, false);
    }
}

/// The window gained focus: it's definitely visible (e.g. restored from the taskbar).
pub fn focused(shared: &Shared) {
    if !shared.in_tray.load(Relaxed) {
        set_hidden(shared, false);
    }
}

/// The window was closed to the tray; remembers its size for when it comes back.
pub fn to_tray(shared: &Shared, size: slint::PhysicalSize) {
    with(|ui| ui.tray_size = Some(size));
    shared.in_tray.store(true, Relaxed);
    set_hidden(shared, true);
    debug("window closed to tray");
}

/// The window is coming back from the tray. Returns the size it had.
pub fn from_tray(shared: &Shared) -> Option<slint::PhysicalSize> {
    let was = shared.in_tray.swap(false, Relaxed);
    set_hidden(shared, false);
    debug("window opened from tray");
    if was { with(|ui| ui.tray_size.take()).flatten() } else { None }
}

fn debug(msg: &str) {
    if std::env::var_os("SOPHON_DEBUG").is_some() {
        eprintln!("sophon: {msg}");
    }
}

/// Checks whether redraws have stalled (Wayland stops frame callbacks for hidden windows).
pub fn check_stalled(shared: &Shared) {
    if std::env::var_os("SOPHON_DEBUG").is_some() {
        let frames = with(|ui| std::mem::take(&mut ui.frames)).unwrap_or(0);
        eprintln!("sophon: {frames} frames presented, hidden={}", shared.hidden.load(Relaxed));
    }
    if with(|ui| ui.frame_pending.is_some_and(|t| t.elapsed() > STALL)).unwrap_or(false) {
        set_hidden(shared, true);
    }
}

pub fn set_hidden(shared: &Shared, hidden: bool) {
    if shared.hidden.swap(hidden, Relaxed) != hidden {
        with(|ui| {
            if let Some(app) = ui.app.upgrade() {
                ui.render_status(&app);
            }
        });
        if std::env::var_os("SOPHON_DEBUG").is_some() {
            eprintln!("sophon: window {}", if hidden { "hidden: sampling slowed" } else { "visible" });
        }
        if !hidden {
            let _ = shared.wake.send(());
        }
    }
}

impl Ui {
    fn render(&mut self) {
        let Some(app) = self.app.upgrade() else { return };
        self.render_status(&app);
        if app.get_settings_open() {
            return; // the overlay owns the view; nothing under it changes
        }
        match crate::panels::id_at(app.get_tab()) {
            "processes" => self.render_processes(),
            "performance" => self.render_perf(),
            _ => {}
        }
    }

    fn tab_changed(&mut self, tab: i32) {
        self.settings_open = false; // a tab click always closes the overlay
        self.shared.tab.store(tab as u8, Relaxed);
        let id = crate::panels::id_at(tab);
        self.shared.processes_visible.store(id == "processes", Relaxed);
        crate::panels::set_visible(id);
        if id == "processes" {
            // The process table isn't refreshed while hidden behind another tab.
            let _ = self.shared.wake.send(());
        }
        self.render();
    }

    fn render_status(&mut self, app: &AppWindow) {
        // Compact summary for the tray tooltip and (while minimized) the taskbar entry.
        let prefs = settings::get(|s| (s.tip_cpu, s.tip_ram, s.tip_gpu, s.tip_temps, s.tip_net, s.show_own_usage));
        let s = &self.snap;
        let g = s.gpus.first();
        let mut parts: Vec<String> = Vec::new();
        if prefs.0 {
            parts.push(format!("CPU {:.0}%{}", s.cpu.usage, if prefs.3 { format!("  {}", fmt::temp(s.cpu.temp_c)) } else { String::new() }));
        }
        if prefs.1 {
            parts.push(format!("RAM {} / {}", fmt::bytes_short(s.mem.used), fmt::bytes_short(s.mem.total)));
        }
        if prefs.2
            && let Some(g) = g
        {
            parts.push(format!("GPU {:.0}%{}", g.busy, if prefs.3 { format!("  {}", fmt::temp(g.temp_edge)) } else { String::new() }));
        }
        if prefs.4 {
            let rx: f64 = s.nets.iter().filter(|n| n.physical).map(|n| n.rx_bps).sum();
            let tx: f64 = s.nets.iter().filter(|n| n.physical).map(|n| n.tx_bps).sum();
            parts.push(format!("↓ {}  ↑ {}", fmt::rate_bits(rx), fmt::rate_bits(tx)));
        }
        let summary = parts.join("   ·   ");
        if s.tick > 0 && summary != self.tray_text {
            if let Some(tray) = self.tray.as_ref().and_then(|t| t.upgrade()) {
                tray.set_summary(if summary.is_empty() { "Sophon".into() } else { format!("Sophon\n{summary}").into() });
            }
            self.tray_text = summary;
        }
        self.show_own = prefs.5;
        let title = if self.shared.hidden.load(Relaxed) && s.tick > 0 {
            format!("Sophon  —  {}", self.tray_text)
        } else {
            "Sophon".into()
        };
        if app.get_window_title() != title.as_str() {
            app.set_window_title(title.into());
        }
        let s = &self.snap;
        let procs = s.processes.iter().filter(|p| !p.kernel_thread).count();
        app.set_status_left(
            format!(
                "{procs} processes   ·   {} threads   ·   Up {}",
                fmt::thousands(s.thread_count as u64),
                fmt::duration(s.cpu.uptime_s)
            )
            .into(),
        );
        app.set_status_right(if !self.show_own {
            SharedString::new()
        } else {
            format!(
                "Sophon   {:.1}% CPU   ·   {}   ·   {} µs/sample",
                s.own.cpu,
                fmt::bytes(s.own.mem_private),
                s.own.stage_us.iter().sum::<u32>()
            )
            .into()
        });
    }

    fn toast(&mut self, msg: impl Into<SharedString>) {
        let Some(app) = self.app.upgrade() else { return };
        app.set_toast(msg.into());
        let weak = self.app.clone();
        self.toast_timer.start(TimerMode::SingleShot, Duration::from_millis(3800), move || {
            if let Some(app) = weak.upgrade() {
                app.set_toast(SharedString::new());
            }
        });
    }

    // ---- Processes -------------------------------------------------------

    fn sort_by(&mut self, col: i32) {
        if self.sort_col == col {
            self.sort_desc = !self.sort_desc;
        } else {
            self.sort_col = col;
            // Numbers default to biggest first, text to A-Z.
            self.sort_desc = !matches!(col, 0 | 6);
        }
        let (c, d) = (self.sort_col, self.sort_desc);
        settings::update(|s| {
            s.sort_col = c;
            s.sort_desc = d;
        });
        self.render_processes();
    }

    fn select_row(&mut self, i: usize) {
        if let Some(Sel::Section(kind)) = self.row_sel.get(i) {
            if !self.collapsed_sections.remove(kind) {
                self.collapsed_sections.insert(*kind);
            }
            self.render_processes();
            return;
        }
        if let Some(sel) = self.row_sel.get(i) {
            if *sel != Sel::None {
                self.selected = sel.clone();
                self.render_processes();
            }
        }
    }

    fn toggle_row(&mut self, i: usize) {
        if let Some(Sel::Section(_)) = self.row_sel.get(i) {
            return self.select_row(i);
        }
        if let (Some(Sel::Pid(pid)), 1) = (self.row_sel.get(i), settings::get(|s| s.grouping)) {
            let pid = *pid;
            if !self.tree_collapsed.remove(&pid) {
                self.tree_collapsed.insert(pid);
            }
            return self.render_processes();
        }
        if let Some(Sel::Group(key)) = self.row_sel.get(i) {
            if !self.expanded.remove(key) {
                self.expanded.insert(key.clone());
            }
            self.render_processes();
        }
    }

    fn go(&mut self, pid: u32) {
        if let Some(p) = self.snap.processes.iter().find(|p| p.pid == pid) {
            self.expanded.insert(group_key(p));
            self.selected = Sel::Pid(pid);
            self.render_processes();
        }
    }

    fn render_processes(&mut self) {
        let Some(app) = self.app.upgrade() else { return };
        if crate::panels::id_at(app.get_tab()) != "processes" {
            return;
        }
        let prefs = settings::get(|s| s.clone());
        self.update_columns(&app, &prefs);
        let snap = &self.snap;
        let show_kernel = prefs.show_kernel;
        let ctx = RowCtx {
            mem_total: snap.mem.total.max(1),
            vram_total: snap.gpus.first().map_or(1, |g| g.vram_total.max(1)),
            col: self.sort_col,
            desc: self.sort_desc,
            name_style: prefs.name_style,
            cpu_scale: if prefs.cpu_mode == 1 { snap.cpu.threads.max(1) as f32 } else { 1.0 },
            mem_mb: prefs.mem_units == 1,
            net_bits: prefs.net_units == 1,
            spark_n: if prefs.spark_len == 1 { 60 } else { 30 },
        };
        let own_uid = self.own_uid;
        let visible: Vec<&Process> = snap
            .processes
            .iter()
            .filter(|p| (show_kernel || !p.kernel_thread) && (prefs.show_other_users || p.uid == own_uid) && matches(p, &self.filter))
            .collect();

        let mut rows = Vec::with_capacity(visible.len() + 32);
        let mut sel = Vec::with_capacity(visible.len() + 32);
        match prefs.grouping {
            0 => {
                for (kind, label) in [(Kind::App, "APPS"), (Kind::Background, "BACKGROUND PROCESSES"), (Kind::System, "SYSTEM")] {
                    let members: Vec<&Process> = visible.iter().copied().filter(|p| p.kind == kind).collect();
                    if members.is_empty() {
                        continue;
                    }
                    let open = !self.collapsed_sections.contains(&kind);
                    rows.push(ProcRow {
                        kind: 0,
                        expanded: open,
                        name: format!("{label}   {}", members.len()).into(),
                        ..Default::default()
                    });
                    sel.push(Sel::Section(kind));
                    if !open {
                        continue;
                    }

                    let mut groups: HashMap<&str, Vec<&Process>> = HashMap::new();
                    for p in members {
                        groups.entry(&p.group).or_default().push(p);
                    }
                    let mut groups: Vec<(&str, Vec<&Process>, Agg)> =
                        groups.into_iter().map(|(g, ps)| (g, ps.clone(), Agg::of(g, &ps))).collect();
                    groups.sort_by(|a, b| ctx.order(a.2.cmp(&b.2, ctx.col)).then_with(|| a.0.cmp(b.0)));

                    for (group, mut ps, agg) in groups {
                        if ps.len() == 1 {
                            rows.push(proc_row(ps[0], 0, &ctx));
                            sel.push(Sel::Pid(ps[0].pid));
                            continue;
                        }
                        let key = group_key(ps[0]);
                        let open = !self.filter.is_empty() || self.expanded.contains(&key);
                        rows.push(group_row(group, &key, &ps, &agg, open, &ctx));
                        sel.push(Sel::Group(key));
                        if open {
                            ps.sort_by(|a, b| ctx.order(cmp_proc(a, b, ctx.col)).then(a.pid.cmp(&b.pid)));
                            for p in ps {
                                rows.push(proc_row(p, 1, &ctx));
                                sel.push(Sel::Pid(p.pid));
                            }
                        }
                    }
                }
            }
            1 => {
                // Parent -> children, roots are processes whose parent isn't shown.
                let shown: HashSet<u32> = visible.iter().map(|p| p.pid).collect();
                let mut children: HashMap<u32, Vec<&Process>> = HashMap::new();
                let mut roots = Vec::new();
                for &p in &visible {
                    if shown.contains(&p.ppid) && p.ppid != p.pid {
                        children.entry(p.ppid).or_default().push(p);
                    } else {
                        roots.push(p);
                    }
                }
                let sort = |v: &mut Vec<&Process>| v.sort_by(|a, b| ctx.order(cmp_proc(a, b, ctx.col)).then(a.pid.cmp(&b.pid)));
                sort(&mut roots);
                for v in children.values_mut() {
                    sort(v);
                }
                let mut stack: Vec<(&Process, i32)> = roots.into_iter().rev().map(|p| (p, 0)).collect();
                while let Some((p, depth)) = stack.pop() {
                    let kids = children.get(&p.pid);
                    let has_kids = kids.is_some_and(|k| !k.is_empty());
                    let open = !self.tree_collapsed.contains(&p.pid) || !self.filter.is_empty();
                    let mut row = proc_row(p, depth.min(10), &ctx);
                    if has_kids {
                        row.kind = 1;
                        row.expanded = open;
                    }
                    rows.push(row);
                    sel.push(Sel::Pid(p.pid));
                    if has_kids && open {
                        for &k in kids.into_iter().flatten().rev() {
                            stack.push((k, depth + 1));
                        }
                    }
                }
            }
            _ => {
                let mut ps = visible.clone();
                ps.sort_by(|a, b| ctx.order(cmp_proc(a, b, ctx.col)).then(a.pid.cmp(&b.pid)));
                for p in ps {
                    rows.push(proc_row(p, 0, &ctx));
                    sel.push(Sel::Pid(p.pid));
                }
            }
        }
        update_model(&self.rows, rows);
        self.row_sel = sel;

        let g = snap.gpus.first();
        let net_rx: f64 = snap.nets.iter().filter(|n| n.physical).map(|n| n.rx_bps).sum();
        let net_tx: f64 = snap.nets.iter().filter(|n| n.physical).map(|n| n.tx_bps).sum();
        let used_pct = pct(snap.mem.used, snap.mem.total);
        let vram_pct = g.map_or(0.0, |g| pct(g.vram_used, g.vram_total));
        let cpu_total = snap.cpu.usage * ctx.cpu_scale;
        let net_text = |v: f64| if ctx.net_bits { fmt::rate_bits(v) } else { fmt::rate_bytes_short(v) };
        let totals = Totals {
            cpu: fmt::pct(cpu_total).into(),
            cpu_heat: (snap.cpu.usage / 100.0).sqrt(),
            mem: format!("{used_pct:.0}%").into(),
            mem_heat: (used_pct / 100.0).sqrt(),
            gpu: g.map_or("—".into(), |g| fmt::pct(g.busy)).into(),
            gpu_heat: g.map_or(0.0, |g| (g.busy / 100.0).sqrt()),
            gmem: g.map_or("—".into(), |_| format!("{vram_pct:.0}%")).into(),
            gmem_heat: (vram_pct / 100.0).sqrt(),
            down: net_text(net_rx).into(),
            down_heat: heat_net(net_rx),
            up: net_text(net_tx).into(),
            up_heat: heat_net(net_tx),
        };
        self.update_heads(&app, &prefs, &totals);
        app.set_totals(totals);
        let total = snap.processes.iter().filter(|p| (show_kernel || !p.kernel_thread) && (prefs.show_other_users || p.uid == own_uid)).count();
        app.set_count(if self.filter.is_empty() {
            format!("{total} running").into()
        } else {
            format!("{} of {total} match", visible.len()).into()
        });
        app.set_sort_col(self.sort_col);
        app.set_sort_desc(self.sort_desc);

        let (key, detail) = match &self.selected {
            Sel::None | Sel::Section(_) => (String::new(), None),
            Sel::Pid(pid) => (format!("p:{pid}"), self.detail(*pid)),
            Sel::Group(key) => (key.clone(), self.group_detail(key)),
        };
        app.set_selected_key(key.into());
        match detail {
            Some(d) => {
                app.set_detail(d);
                app.set_has_detail(true);
            }
            None => app.set_has_detail(false),
        }
    }

    fn detail(&self, pid: u32) -> Option<ProcDetail> {
        let procs = &self.snap.processes;
        let p = procs.iter().find(|p| p.pid == pid)?;
        let parent = procs.iter().find(|q| q.pid == p.ppid && p.ppid != 0);
        let mut children: Vec<&Process> = procs.iter().filter(|q| q.ppid == pid).collect();
        children.sort_by(|a, b| cmp_name(a.display_name(), b.display_name()));
        let peak = p.cpu_history.max();
        Some(ProcDetail {
            pid: p.pid as i32,
            title: p.display_name().into(),
            subtitle: format!("{}  ·  PID {}", p.name, p.pid).into(),
            avatar: initial(p.display_name()),
            tint: tint(&p.group),
            state: state_name(p.state).into(),
            threads: p.threads.to_string().into(),
            user: p.user.as_ref().into(),
            cpu: fmt::pct(p.cpu).into(),
            mem: fmt::bytes(p.mem_private).into(),
            rss: fmt::bytes(p.mem_rss).into(),
            gpu: fmt::pct(p.gpu).into(),
            gmem: if p.gpu_mem > 0 { fmt::bytes(p.gpu_mem).into() } else { "—".into() },
            net: format!("↓ {}    ↑ {}", fmt::rate_bytes(p.net_rx), fmt::rate_bytes(p.net_tx)).into(),
            started: fmt::ago(self.snap.cpu.uptime_s.saturating_sub(p.started_s)).into(),
            exe: p.exe.as_deref().unwrap_or("").into(),
            cmdline: p.cmdline.as_ref().into(),
            unit: if p.unit.is_empty() { "—".into() } else { p.unit.replace("\\x2d", "-").into() },
            group: p.group.as_ref().into(),
            cpu_series: graph::ring(&p.cpu_history, nice_max(peak, 1.0)),
            cpu_peak: fmt::pct(peak).into(),
            has_parent: parent.is_some(),
            parent: parent.map(link).unwrap_or_default(),
            children_label: format!("CHILDREN ({})", children.len()).into(),
            children: model(children.into_iter().map(link).collect()),
        })
    }

    /// Details for a whole app: totals and summed history across its processes,
    /// identity (exe, command line, parent) from its main process.
    fn group_detail(&self, key: &str) -> Option<ProcDetail> {
        let procs = &self.snap.processes;
        let mut members: Vec<&Process> = procs.iter().filter(|p| group_key(p) == key).collect();
        let root = self.detail(group_root(procs, key)?)?;
        members.sort_by(|a, b| b.cpu.total_cmp(&a.cpu).then(b.mem_private.cmp(&a.mem_private)));
        let hist = summed_history(&members);
        let peak = hist.iter().copied().fold(0.0, f32::max);
        let agg = Agg::of("", &members);
        let running = members.iter().filter(|p| p.state == b'R').count();
        let oldest = members.iter().map(|p| p.started_s).min().unwrap_or(0);
        let group = members.first().map_or("", |p| &p.group);
        Some(ProcDetail {
            title: group.into(),
            subtitle: format!("{} processes  ·  main PID {}", members.len(), root.pid).into(),
            avatar: initial(group),
            tint: tint(group),
            state: format!("{running} running").into(),
            net: format!("↓ {}    ↑ {}", fmt::rate_bytes(agg.down), fmt::rate_bytes(agg.up)).into(),
            threads: members.iter().map(|p| p.threads).sum::<u32>().to_string().into(),
            cpu: fmt::pct(agg.cpu).into(),
            mem: fmt::bytes(agg.mem).into(),
            rss: fmt::bytes(members.iter().map(|p| p.mem_rss).sum()).into(),
            gpu: fmt::pct(agg.gpu).into(),
            gmem: if agg.gmem > 0 { fmt::bytes(agg.gmem).into() } else { "—".into() },
            started: fmt::ago(self.snap.cpu.uptime_s.saturating_sub(oldest)).into(),
            cpu_series: graph::slice(&hist, SPARK, nice_max(peak, 1.0)),
            cpu_peak: fmt::pct(peak).into(),
            children_label: format!("PROCESSES ({})", members.len()).into(),
            children: model(members.into_iter().map(link).collect()),
            ..root
        })
    }

    /// Runs an action on the current selection, asking first if settings say so.
    fn act(&mut self, action: &str) {
        let destructive = matches!(action, "end" | "kill" | "tree");
        let needs = match settings::get(|s| s.confirm_end) {
            0 => false,
            1 => matches!(self.selected, Sel::Group(_)) || action == "tree",
            _ => true,
        };
        if destructive && needs {
            return self.confirm(action);
        }
        self.run_action(action);
    }

    /// Shows the confirmation dialog for a destructive action on the selection.
    fn confirm(&mut self, action: &str) {
        let Some(app) = self.app.upgrade() else { return };
        // Only one confirm may be armed at a time; opening ours retires any
        // pending Docker stop (docker_panel.rs does the same for us).
        #[cfg(feature = "docker")]
        crate::docker_panel::cancel_confirm();
        let what = match &self.selected {
            Sel::Pid(pid) => self.snap.processes.iter().find(|p| p.pid == *pid).map(|p| format!("{} (PID {pid})", p.display_name())),
            Sel::Group(key) => {
                let n = self.snap.processes.iter().filter(|p| group_key(p) == *key).count();
                self.snap.processes.iter().find(|p| group_key(p) == *key).map(|p| format!("{} and all {n} of its processes", p.group))
            }
            _ => None,
        };
        let Some(what) = what else { return };
        let (text, button) = match action {
            "kill" => (format!("Force kill {what}?\n\nIt won't get a chance to save anything."), "Force kill"),
            "tree" => (format!("End {what} and everything it started?"), "End process tree"),
            _ => (format!("End {what}?\n\nIt will be asked to close, so it can save its work."), "End task"),
        };
        self.pending = Some((action.to_string(), self.selected.clone()));
        app.set_confirm_button(button.into());
        app.set_confirm_text(text.into());
    }

    fn run_action(&mut self, action: &str) {
        let procs = self.snap.processes.clone();
        let (pids, label) = match &self.selected {
            Sel::None | Sel::Section(_) => return,
            Sel::Pid(pid) => match procs.iter().find(|p| p.pid == *pid) {
                Some(p) => (vec![*pid], p.display_name().to_string()),
                None => return self.toast("That process has already exited"),
            },
            Sel::Group(key) => {
                let pids: Vec<u32> = procs.iter().filter(|p| group_key(p) == *key).map(|p| p.pid).collect();
                let name = procs.iter().find(|p| group_key(p) == *key).map_or(String::new(), |p| p.group.to_string());
                (pids, format!("{name} ({} processes)", procs.iter().filter(|p| group_key(p) == *key).count()))
            }
        };
        let report = |out: actions::Outcome, done: String| {
            if out.denied > 0 {
                let hint = if crate::admin::is_active() { "" } else { "  ·  Enable admin features in Settings to end these." };
                format!(
                    "Not allowed to end {} process{} (owned by another user or the system).{hint}",
                    out.denied,
                    if out.denied == 1 { "" } else { "es" }
                )
            } else {
                done
            }
        };
        match action {
            "end" => {
                let out = actions::signal(&pids, Signal::Term);
                self.toast(report(out, format!("Asked {label} to close")));
            }
            "kill" => {
                let out = actions::signal(&pids, Signal::Kill);
                self.toast(report(out, format!("Force-killed {label}")));
            }
            "tree" => {
                let all = actions::tree(&procs, &pids);
                let extra = all.len().saturating_sub(pids.len());
                let out = actions::signal(&all, Signal::Term);
                self.toast(report(out, format!("Ended {label} and {extra} descendant process{}", if extra == 1 { "" } else { "es" })));
            }
            "open" => {
                let root = match &self.selected {
                    Sel::Group(key) => group_root(&procs, key),
                    _ => pids.first().copied(),
                };
                match root.and_then(|pid| procs.iter().find(|p| p.pid == pid)).and_then(|p| p.exe.clone()) {
                    Some(exe) => {
                        std::thread::spawn(move || actions::open_location(&exe));
                    }
                    None => self.toast("Can't see this process's executable (it belongs to another user)"),
                }
            }
            "parent" => {
                if let Some(ppid) = pids.first().and_then(|pid| procs.iter().find(|p| p.pid == *pid)).map(|p| p.ppid) {
                    if ppid > 0 {
                        self.go(ppid);
                    }
                }
            }
            _ => {}
        }
    }

    // ---- Performance -----------------------------------------------------

    fn render_perf(&mut self) {
        let Some(app) = self.app.upgrade() else { return };
        if crate::panels::id_at(app.get_tab()) != "performance" {
            return;
        }
        let s = &self.snap;
        let prefs = settings::get(|s| s.clone());
        let mut all = vec![TileId::Cpu, TileId::Mem];
        all.extend((0..s.gpus.len()).map(TileId::Gpu));
        all.extend((0..s.disks.len()).map(TileId::Disk));
        all.extend(s.nets.iter().enumerate().filter(|(_, n)| n.physical || prefs.show_virtual_nics).map(|(i, _)| TileId::Net(i)));
        // Saved order first, then anything new; hidden tiles left out.
        let keyed: Vec<(String, TileId)> = all.iter().map(|&id| (self.tile_key(id), id)).collect();
        crate::settings_page::set_tiles(keyed.iter().map(|(k, id)| (k.clone(), self.tile(*id).title.to_string())).collect());
        let mut ids: Vec<TileId> = Vec::new();
        for key in &prefs.tile_order {
            if let Some((_, id)) = keyed.iter().find(|(k, _)| k == key) {
                ids.push(*id);
            }
        }
        for (_, id) in &keyed {
            if !ids.contains(id) {
                ids.push(*id);
            }
        }
        ids.retain(|id| !prefs.hidden_tiles.contains(&self.tile_key(*id)));
        if ids.is_empty() {
            ids.push(TileId::Cpu);
        }
        self.feed_long_history();
        let s = &self.snap;
        update_model(&self.tiles, ids.iter().map(|&id| self.tile(id)).collect());
        let current = (app.get_tile().max(0) as usize).min(ids.len() - 1);
        let mut detail = self.perf_detail(ids[current], &prefs);
        let prefix = match ids[current] {
            TileId::Cpu => "cpu".to_string(),
            TileId::Mem => "mem".to_string(),
            TileId::Gpu(i) => format!("gpu{i}"),
            TileId::Disk(i) => format!("disk{i}"),
            TileId::Net(i) => format!("net{i}"),
        };
        detail.main.key = prefix.clone().into();
        detail.second.key = format!("{prefix}.2").into();
        detail.main.markers = model(if self.view_span <= 600.0 { self.markers(ids[current], prefs.spike_sensitivity) } else { Vec::new() });
        app.set_perf(detail);
        self.epoch = self.epoch.wrapping_add(1);
        let live = self.view_end.is_none();
        let tl = self.timeline();
        let (start_label, end_label) = if live {
            (format!("{} ago", span_words(self.view_span)), "now".to_string())
        } else {
            (self.clock(tl.start), format!("{}  (paused)", self.clock(tl.end)))
        };
        let hover = app.global::<Hover>();
        let ranges: Vec<SharedString> = ["1 min", "5 min", "10 min", "1 hour", "24 hours"][..3 + prefs.history].iter().map(|r| SharedString::from(*r)).collect();
        if hover.get_ranges().row_count() != ranges.len() {
            hover.set_ranges(ModelRc::new(VecModel::from(ranges)));
        }
        hover.set_epoch(self.epoch);
        hover.set_live(live);
        hover.set_start_label(start_label.into());
        hover.set_end_label(end_label.into());
        crate::space_ui::show_disk(match ids[current] {
            TileId::Disk(i) => Some(&s.disks[i].name),
            _ => None,
        });
        self.tile_ids = ids;
    }

    fn tile(&self, id: TileId) -> Tile {
        let s = &self.snap;
        match id {
            TileId::Cpu => Tile {
                title: "CPU".into(),
                subtitle: format!("{}   {}", fmt::pct(s.cpu.usage), fmt::ghz(s.cpu.avg_freq_mhz)).into(),
                a: graph::recent(&s.cpu.history, 60, 100.0),
                color_a: rgb(c_cpu()),
                ..Default::default()
            },
            TileId::Mem => Tile {
                title: "Memory".into(),
                subtitle: format!(
                    "{} / {}  ({:.0}%)",
                    fmt::bytes_short(s.mem.used),
                    fmt::bytes_short(s.mem.total),
                    pct(s.mem.used, s.mem.total)
                )
                .into(),
                a: graph::recent(&s.mem.history, 60, 100.0),
                color_a: rgb(c_mem()),
                ..Default::default()
            },
            TileId::Gpu(i) => {
                let g = &s.gpus[i];
                Tile {
                    title: format!("GPU {i}").into(),
                    subtitle: format!("{}   ·   {}", fmt::pct(g.busy), fmt::temp(g.temp_edge)).into(),
                    a: graph::recent(&g.history, 60, 100.0),
                    color_a: rgb(c_gpu()),
                    ..Default::default()
                }
            }
            TileId::Disk(i) => {
                let d = &s.disks[i];
                Tile {
                    title: format!("Disk {i} ({})", d.name).into(),
                    subtitle: format!("{}   ·   {}", disk_kind(d), fmt::pct(d.active_pct)).into(),
                    a: graph::recent(&d.active_history, 60, 100.0),
                    color_a: rgb(c_disk()),
                    ..Default::default()
                }
            }
            TileId::Net(i) => {
                let n = &s.nets[i];
                let max = nice_max(n.rx_history.max().max(n.tx_history.max()), 12_500.0);
                Tile {
                    title: net_title(n).into(),
                    subtitle: if n.up {
                        format!("↓ {}   ↑ {}", fmt::rate_bits(n.rx_bps), fmt::rate_bits(n.tx_bps)).into()
                    } else {
                        "Not connected".into()
                    },
                    a: graph::recent(&n.rx_history, 60, max),
                    b: graph::recent(&n.tx_history, 60, max),
                    has_b: true,
                    color_a: rgb(c_net()),
                    color_b: rgb(NET_TX),
                }
            }
        }
    }

    fn tile_key(&self, id: TileId) -> String {
        let s = &self.snap;
        match id {
            TileId::Cpu => "cpu".into(),
            TileId::Mem => "mem".into(),
            TileId::Gpu(i) => format!("gpu:{}", s.gpus[i].pci_slot),
            TileId::Disk(i) => format!("disk:{}", s.disks[i].name),
            TileId::Net(i) => format!("net:{}", s.nets[i].name),
        }
    }

    /// Right-click actions on a sidebar tile.
    fn tile_action(&mut self, index: usize, action: &str) {
        let Some(&id) = self.tile_ids.get(index) else { return };
        let key = self.tile_key(id);
        let order: Vec<String> = self.tile_ids.iter().map(|&t| self.tile_key(t)).collect();
        settings::update(|s| {
            match action {
                "up" | "down" => {
                    let mut o = order.clone();
                    // Keep hidden tiles' saved positions too.
                    for k in &s.tile_order {
                        if !o.contains(k) {
                            o.push(k.clone());
                        }
                    }
                    if let Some(i) = o.iter().position(|k| *k == key) {
                        let j = if action == "up" { i.saturating_sub(1) } else { (i + 1).min(o.len() - 1) };
                        o.swap(i, j);
                    }
                    s.tile_order = o;
                }
                "hide" => {
                    if order.len() > 1 && !s.hidden_tiles.contains(&key) {
                        s.hidden_tiles.push(key.clone());
                    }
                }
                "show-all" => s.hidden_tiles.clear(),
                _ => {}
            }
        });
        self.render_perf();
        crate::settings_page::refresh();
    }

    fn perf_detail(&self, id: TileId, prefs: &settings::Settings) -> PerfDetail {
        let s = &self.snap;
        let tl = self.timeline();
        // Long ranges use the coarse history for the big charts; the small
        // per-thread/engine graphs always show the last 10 minutes.
        let long = self.view_span > 600.0;
        let mini_tl = if long { Timeline { times: &s.times, start: s.now - 600.0, end: s.now } } else { tl };
        match id {
            TileId::Cpu => {
                let c = &s.cpu;
                let pmax = nice_max(c.power_history.max(), 50.0);
                let cols = if c.threads >= 16 { 8 } else { c.threads.clamp(1, 4) } as i32;
                PerfDetail {
                    title: "CPU".into(),
                    subtitle: c.model.as_ref().into(),
                    main: chart("% Utilization", "100%", self.single("cpu", &c.history, 100.0, c_cpu())),
                    has_second: c.power_w.is_some() && prefs.show_power,
                    second: chart("Package power", &format!("{pmax:.0} W"), self.single("cpu.2", &c.power_history, pmax, DISK_WRITE)),
                    minis_label: match prefs.cpu_view {
                        1 => "Logical processors",
                        2 => "Physical cores",
                        _ => "",
                    }
                    .into(),
                    minis: model(match prefs.cpu_view {
                        1 => c
                            .per_thread_history
                            .iter()
                            .enumerate()
                            .map(|(i, h)| Mini {
                                key: format!("cpu.t{i}").into(),
                                title: i.to_string().into(),
                                value: format!("{} · {:.1}", fmt::pct(h.last()), c.freq_mhz[i] as f32 / 1000.0).into(),
                                a: mini_tl.series(h, 100.0),
                            })
                            .collect(),
                        2 => per_core(c)
                            .into_iter()
                            .map(|(core, h)| Mini {
                                key: "".into(),
                                title: format!("Core {core}").into(),
                                value: fmt::pct(h.last()).into(),
                                a: mini_tl.series(&h, 100.0),
                            })
                            .collect(),
                        _ => Vec::new(),
                    }),
                    mini_cols: if prefs.cpu_view == 2 { cols.min(8).max(1) / if c.threads > c.cores { 2 } else { 1 } } else { cols },
                    mini_color: rgb(c_cpu()),
                    stats: stats(&[
                        ("Utilization", fmt::pct(c.usage)),
                        ("Speed", fmt::ghz(c.avg_freq_mhz)),
                        ("Max speed", fmt::ghz(c.max_freq_mhz)),
                        ("Temperature", fmt::temp(c.temp_c)),
                        ("Package power", if prefs.show_power { fmt::watts(c.power_w) } else { "hidden".into() }),
                        ("Cores / threads", format!("{} / {}", c.cores, c.threads)),
                        ("Processes", s.processes.iter().filter(|p| !p.kernel_thread).count().to_string()),
                        ("Threads", fmt::thousands(s.thread_count as u64)),
                        ("Running / blocked", format!("{} / {}", c.procs_running, c.procs_blocked)),
                        ("Context switches", format!("{}/s", fmt::thousands(c.ctx_switches_per_s))),
                        ("Up time", fmt::duration(c.uptime_s)),
                    ]),
                    ..Default::default()
                }
            }
            TileId::Mem => {
                let m = &s.mem;
                let total = m.total.max(1) as f32;
                let used = m.used as f32 / total;
                let cache = (m.cached as f32 / total).min(1.0 - used);
                let free = (1.0 - used - cache).max(0.0);
                let mut rows = vec![
                    ("In use", fmt::bytes(m.used)),
                    ("Available", fmt::bytes(m.available)),
                    ("Cached", fmt::bytes(m.cached)),
                    ("Free", fmt::bytes(m.free)),
                    ("Committed", format!("{} / {}", fmt::bytes_short(m.committed), fmt::bytes_short(m.commit_limit))),
                    ("Shared", fmt::bytes(m.shmem)),
                    ("Dirty", fmt::bytes(m.dirty)),
                    ("Swap", format!("{} / {}", fmt::bytes_short(m.swap_used), fmt::bytes_short(m.swap_total))),
                ];
                if let Some(z) = m.zram.filter(|z| z.original > 0) {
                    rows.push((
                        "zram",
                        format!("{} → {} ({:.1}×)", fmt::bytes_short(z.original), fmt::bytes_short(z.used), z.original as f64 / z.used.max(1) as f64),
                    ));
                }
                PerfDetail {
                    title: "Memory".into(),
                    subtitle: format!("{} total", fmt::bytes_short(m.total)).into(),
                    main: chart("Memory usage", &fmt::bytes_short(m.total), self.single("mem", &m.history, 100.0, c_mem())),
                    has_second: m.swap_total > 0,
                    second: chart("Swap usage", &fmt::bytes_short(m.swap_total), self.single("mem.2", &m.swap_history, 100.0, c_mem())),
                    segments_label: "Composition".into(),
                    segments: model(vec![
                        segment(0.0, used, c_mem(), "In use", fmt::bytes_short(m.used)),
                        segment(used, cache, NET_TX, "Cached", fmt::bytes_short(m.cached)),
                        segment(used + cache, free, 0x5a6274, "Free", fmt::bytes_short(m.free)),
                    ]),
                    stats: stats(&rows),
                    ..Default::default()
                }
            }
            TileId::Gpu(i) => {
                let g = &s.gpus[i];
                PerfDetail {
                    title: format!("GPU {i}").into(),
                    subtitle: g.name.as_ref().into(),
                    main: chart("% Utilization", "100%", self.single(&format!("gpu{i}"), &g.history, 100.0, c_gpu())),
                    has_second: g.vram_total > 0,
                    second: chart("Dedicated memory (VRAM)", &fmt::bytes_short(g.vram_total), self.single(&format!("gpu{i}.2"), &g.vram_history, 100.0, c_mem())),
                    minis_label: "Engines (from per-process usage)".into(),
                    minis: model(
                        ENGINES
                            .iter()
                            .zip(&g.engine_history)
                            .zip(&g.engines)
                            .enumerate()
                            .map(|(j, ((name, h), v))| Mini {
                                key: format!("gpu{i}.e{j}").into(),
                                title: (*name).into(),
                                value: fmt::pct(*v).into(),
                                a: mini_tl.series(h, 100.0),
                            })
                            .collect(),
                    ),
                    mini_cols: ENGINES.len() as i32,
                    mini_color: rgb(c_gpu()),
                    stats: stats(&[
                        ("Utilization", fmt::pct(g.busy)),
                        ("Memory controller", g.mem_busy.map_or("—".into(), fmt::pct)),
                        ("VRAM", format!("{} / {}", fmt::bytes_short(g.vram_used), fmt::bytes_short(g.vram_total))),
                        ("Shared (GTT)", format!("{} / {}", fmt::bytes_short(g.gtt_used), fmt::bytes_short(g.gtt_total))),
                        ("Edge temp", fmt::temp(g.temp_edge)),
                        ("Hotspot temp", fmt::temp(g.temp_hotspot)),
                        ("Memory temp", fmt::temp(g.temp_mem)),
                        (
                            "Power",
                            match (g.power_w, g.power_cap_w) {
                                (Some(w), Some(cap)) => format!("{w:.0} / {cap:.0} W"),
                                (w, _) => fmt::watts(w),
                            },
                        ),
                        ("Fan", g.fan_rpm.map_or("—".into(), |r| format!("{r} RPM"))),
                        ("Core clock", g.core_mhz.map_or("—".into(), |m| format!("{m} MHz"))),
                        ("Memory clock", g.mem_mhz.map_or("—".into(), |m| format!("{m} MHz"))),
                        ("Driver", format!("{}  ·  {}", g.driver, g.pci_slot)),
                    ]),
                    ..Default::default()
                }
            }
            TileId::Disk(i) => {
                let d = &s.disks[i];
                let max = nice_max(d.read_history.max().max(d.write_history.max()), 1024.0 * 1024.0);
                PerfDetail {
                    title: format!("Disk {i}").into(),
                    subtitle: format!("{}  ({})", d.model, d.name).into(),
                    main: chart("Active time", "100%", self.single(&format!("disk{i}"), &d.active_history, 100.0, c_disk())),
                    has_second: true,
                    second: Chart {
                        legend_a: "Read".into(),
                        legend_b: "Write".into(),
                        ..chart("Transfer rate", &fmt::rate_bytes(max as f64), self.pair(&format!("disk{i}.r"), &format!("disk{i}.w"), &d.read_history, &d.write_history, max, c_disk(), DISK_WRITE))
                    },
                    stats: stats(&[
                        ("Active time", fmt::pct(d.active_pct)),
                        ("Average response", format!("{:.1} ms", d.response_ms)),
                        ("Read speed", fmt::rate_bytes(d.read_bps)),
                        ("Write speed", fmt::rate_bytes(d.write_bps)),
                        ("Capacity", fmt::bytes_short(d.size)),
                        ("Type", disk_kind(d).to_string()),
                        ("Temperature", fmt::temp(d.temp_c)),
                        ("Read since boot", fmt::bytes_short(d.read_total)),
                        ("Written since boot", fmt::bytes_short(d.write_total)),
                    ]),
                    ..Default::default()
                }
            }
            TileId::Net(i) => {
                let n = &s.nets[i];
                let max = nice_max(n.rx_history.max().max(n.tx_history.max()), 12_500.0);
                PerfDetail {
                    title: net_title(n).into(),
                    subtitle: n.name.as_ref().into(),
                    main: Chart {
                        legend_a: "Receive".into(),
                        legend_b: "Send".into(),
                        ..chart("Throughput", &fmt::rate_bits(max as f64), self.pair(&format!("net{i}.rx"), &format!("net{i}.tx"), &n.rx_history, &n.tx_history, max, c_net(), NET_TX))
                    },
                    stats: stats(&[
                        ("Receive", fmt::rate_bits(n.rx_bps)),
                        ("Send", fmt::rate_bits(n.tx_bps)),
                        ("Status", if n.up { "Connected".into() } else { "Not connected".into() }),
                        ("Link speed", n.speed_mbps.map_or("—".into(), |m| format!("{m} Mbps"))),
                        ("Received since boot", fmt::bytes_short(n.rx_total)),
                        ("Sent since boot", fmt::bytes_short(n.tx_total)),
                    ]),
                    ..Default::default()
                }
            }
        }
    }

    // ---- Rewind ------------------------------------------------------------

    fn timeline(&self) -> Timeline<'_> {
        let end = self.view_end.unwrap_or(self.snap.now);
        Timeline { times: &self.snap.times, start: end - self.view_span, end }
    }

    /// Wall-clock `HH:MM:SS` for a sample time.
    fn clock(&self, t: f32) -> String {
        let ago = Duration::from_secs_f32((self.snap.now - t).max(0.0));
        let when = self.snap_wall.checked_sub(ago).unwrap_or(self.snap_wall);
        let secs = when.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs()) as libc::time_t;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        unsafe { libc::localtime_r(&secs, &mut tm) };
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    }

    /// Hover text for graph `key` at fraction `x` of the visible range.
    fn probe(&self, key: &str, x: f32) -> String {
        if let Some(text) = self.probe_long(key, x) {
            return text;
        }
        let s = &self.snap;
        let tl = self.timeline();
        let t = tl.time_at(x);
        let pct_of = |r: &Ring| tl.value_at(r, t).map(|(st, v)| (st, fmt::pct(v)));
        let (sample_t, text) = match key {
            "cpu" => pct_of(&s.cpu.history).unzip(),
            "cpu.2" => tl.value_at(&s.cpu.power_history, t).map(|(st, v)| (st, format!("{v:.0} W"))).unzip(),
            "mem" => tl
                .value_at(&s.mem.history, t)
                .map(|(st, v)| (st, format!("{:.0}%  ·  {}", v, fmt::bytes_short((v as f64 / 100.0 * s.mem.total as f64) as u64))))
                .unzip(),
            "mem.2" => tl
                .value_at(&s.mem.swap_history, t)
                .map(|(st, v)| (st, format!("{:.0}%  ·  {}", v, fmt::bytes_short((v as f64 / 100.0 * s.mem.swap_total as f64) as u64))))
                .unzip(),
            k => {
                if let Some(i) = k.strip_prefix("cpu.t").and_then(|n| n.parse::<usize>().ok()) {
                    s.cpu.per_thread_history.get(i).and_then(pct_of).unzip()
                } else if let Some(rest) = k.strip_prefix("gpu") {
                    let (i, sub) = rest.split_once('.').map_or((rest, ""), |(a, b)| (a, b));
                    let Some(g) = i.parse::<usize>().ok().and_then(|i| s.gpus.get(i)) else { return String::new() };
                    match sub {
                        "" => pct_of(&g.history).unzip(),
                        "2" => tl
                            .value_at(&g.vram_history, t)
                            .map(|(st, v)| (st, format!("{:.0}%  ·  {}", v, fmt::bytes_short((v as f64 / 100.0 * g.vram_total as f64) as u64))))
                            .unzip(),
                        e => e
                            .strip_prefix('e')
                            .and_then(|j| j.parse::<usize>().ok())
                            .and_then(|j| g.engine_history.get(j))
                            .and_then(pct_of)
                            .unzip(),
                    }
                } else if let Some(rest) = k.strip_prefix("disk") {
                    let (i, sub) = rest.split_once('.').map_or((rest, ""), |(a, b)| (a, b));
                    let Some(d) = i.parse::<usize>().ok().and_then(|i| s.disks.get(i)) else { return String::new() };
                    if sub == "2" {
                        let r = tl.value_at(&d.read_history, t);
                        let w = tl.value_at(&d.write_history, t);
                        (r.map(|v| v.0), r.zip(w).map(|(r, w)| format!("Read {}\nWrite {}", fmt::rate_bytes(r.1 as f64), fmt::rate_bytes(w.1 as f64))))
                    } else {
                        pct_of(&d.active_history).unzip()
                    }
                } else if let Some(n) = k.strip_prefix("net").and_then(|i| i.parse::<usize>().ok()).and_then(|i| s.nets.get(i)) {
                    let r = tl.value_at(&n.rx_history, t);
                    let w = tl.value_at(&n.tx_history, t);
                    (r.map(|v| v.0), r.zip(w).map(|(r, w)| format!("↓ {}\n↑ {}", fmt::rate_bits(r.1 as f64), fmt::rate_bits(w.1 as f64))))
                } else {
                    (None, None)
                }
            }
        };
        match (sample_t, text) {
            (Some(st), Some(text)) => {
                let ago = s.now - st;
                let when = if ago < 1.5 { "now".to_string() } else { format!("{}  ({} ago)", self.clock(st), span_words(ago)) };
                format!("{when}\n{text}")
            }
            _ => "No data here yet".into(),
        }
    }

    /// Notes who was busiest at this sample.
    fn record_moment(&mut self) {
        if !self.snap.processes_updated {
            return;
        }
        // Culprits are grouped like the Processes tab: one entry per app.
        let top = |key: fn(&Process) -> f32, n: usize| -> Vec<(u32, String, f32)> {
            let mut groups: HashMap<String, (f32, usize, u32, f32, String)> = HashMap::new();
            for p in self.snap.processes.iter().filter(|p| !p.kernel_thread && key(p) > 0.0) {
                let label = if p.kind == Kind::App { p.group.to_string() } else { p.display_name().to_string() };
                let g = groups.entry(label).or_insert((0.0, 0, p.pid, 0.0, p.display_name().to_string()));
                g.0 += key(p);
                g.1 += 1;
                if key(p) > g.3 {
                    g.2 = p.pid;
                    g.3 = key(p);
                    g.4 = p.display_name().to_string();
                }
            }
            let mut v: Vec<_> = groups.into_iter().filter(|(_, g)| g.0 >= 0.5).collect();
            v.sort_by(|a, b| b.1.0.total_cmp(&a.1.0));
            v.into_iter()
                .take(n)
                .map(|(label, (sum, count, pid, _, busiest))| {
                    let name = match count {
                        1 if busiest == label => label,
                        1 => format!("{label}  ({busiest})"),
                        c => format!("{label}  ({c} processes, mostly {busiest})"),
                    };
                    (pid, name, sum)
                })
                .collect()
        };
        let m = Moment { t: self.snap.now, cpu: top(|p| p.cpu, 3), gpu: top(|p| p.gpu, 3) };
        self.moments.push_back(m);
        while self.moments.len() > 1200 {
            self.moments.pop_front();
        }
    }

    /// Spike markers for the main graph of CPU and GPU pages.
    fn markers(&self, id: TileId, sensitivity: usize) -> Vec<Marker> {
        if sensitivity == 0 {
            return Vec::new();
        }
        let s = &self.snap;
        let (series, gpu, what) = match id {
            TileId::Cpu => (&s.cpu.history, false, "CPU"),
            TileId::Gpu(i) => (&s.gpus[i].history, true, "GPU"),
            _ => return Vec::new(),
        };
        let tl = self.timeline();
        let pts = tl.samples(series);
        if pts.len() < 5 {
            return Vec::new();
        }
        let mut sorted: Vec<f32> = pts.iter().map(|p| p.1).collect();
        sorted.sort_by(f32::total_cmp);
        let median = sorted[sorted.len() / 2];
        let (above, floor) = [(0.0, 0.0), (30.0, 50.0), (20.0, 35.0), (12.0, 25.0)][sensitivity.min(3)];
        let threshold = (median + above).max(floor);
        let span = tl.end - tl.start;
        let mut peaks: Vec<(f32, f32)> = Vec::new();
        for i in 0..pts.len() {
            let (t, v) = pts[i];
            if v < threshold {
                continue;
            }
            let lo = i.saturating_sub(4);
            let hi = (i + 5).min(pts.len());
            if pts[lo..hi].iter().any(|&(_, w)| w > v) {
                continue;
            }
            peaks.push((t, v));
        }
        peaks.sort_by(|a, b| b.1.total_cmp(&a.1));
        let mut chosen: Vec<(f32, f32)> = Vec::new();
        for p in peaks {
            if chosen.len() >= 6 {
                break;
            }
            if chosen.iter().all(|c| (c.0 - p.0).abs() > span / 25.0) {
                chosen.push(p);
            }
        }
        chosen.sort_by(|a, b| a.0.total_cmp(&b.0));
        chosen
            .into_iter()
            .map(|(t, v)| {
                let moment = self.moments.iter().min_by(|a, b| (a.t - t).abs().total_cmp(&(b.t - t).abs())).filter(|m| (m.t - t).abs() <= 6.0);
                let culprits = moment.map(|m| if gpu { &m.gpu } else { &m.cpu });
                let mut tip = format!("{}  ·  {what} {:.0}%", self.clock(t), v);
                let mut pid = 0;
                match culprits {
                    Some(list) if !list.is_empty() => {
                        pid = list[0].0 as i32;
                        for (_, name, value) in list {
                            tip.push_str(&format!("\n{name}   {}", fmt::pct(*value)));
                        }
                        tip.push_str("\nClick to see the top process");
                    }
                    _ => tip.push_str("\n(no process data for this moment)"),
                }
                if std::env::var_os("SOPHON_DEBUG").is_some() {
                    eprintln!("sophon: marker: {}", tip.replace('\n', " | "));
                }
                Marker { x: (t - tl.start) / span, tip: tip.into(), pid }
            })
            .collect()
    }

    // ---- Columns ---------------------------------------------------------------

    /// Visible columns in display order: (column index, width px).
    fn visible_columns(prefs: &settings::Settings) -> Vec<(usize, u32)> {
        prefs
            .columns
            .iter()
            .filter(|id| !prefs.hidden_columns.contains(id))
            .filter_map(|id| COLUMNS.iter().position(|c| c.0 == id).map(|i| (i, prefs.column_width(id))))
            .collect()
    }

    fn update_columns(&self, app: &AppWindow, prefs: &settings::Settings) {
        let visible = Self::visible_columns(prefs);
        let mut w = vec![0f32; COLUMNS.len()];
        let mut r = vec![0f32; COLUMNS.len()];
        let mut from_right = 0f32;
        for &(i, width) in visible.iter().rev() {
            from_right += width as f32;
            w[i] = width as f32;
            r[i] = from_right;
        }
        let cols = app.global::<Cols>();
        let w_model: Vec<f32> = w;
        if cols.get_w().iter().collect::<Vec<f32>>() != w_model || cols.get_total() != from_right {
            cols.set_w(ModelRc::new(VecModel::from(w_model)));
            cols.set_r(ModelRc::new(VecModel::from(r)));
            cols.set_total(from_right);
        }
    }

    fn update_heads(&self, app: &AppWindow, prefs: &settings::Settings, t: &Totals) {
        let cpu_hint = if prefs.cpu_mode == 1 {
            "Processor use where one fully busy core is 100% (like top). The line to the left shows recent activity."
        } else {
            "Share of the whole processor (all threads = 100%). The line to the left shows recent activity."
        };
        let net_hint = "Per-app figures count TCP traffic (downloads, most web browsing). UDP traffic (games, voice chat, some video) needs admin features (Settings → General). The total above includes everything.";
        let mut from_right = 0f32;
        let visible = Self::visible_columns(prefs);
        let mut heads: Vec<HeadInfo> = Vec::new();
        for &(i, width) in visible.iter().rev() {
            from_right += width as f32;
            let (total, heat, hint, sort): (SharedString, f32, &str, i32) = match i {
                0 => ("".into(), 0.0, "Recent CPU activity of each row.", -1),
                1 => (t.cpu.clone(), t.cpu_heat, cpu_hint, 1),
                2 => (t.mem.clone(), t.mem_heat, "Private memory: what this process alone uses (like Windows’ working set). Shared libraries aren’t counted.", 2),
                3 => (t.gpu.clone(), t.gpu_heat, "Utilisation of the busiest GPU engine this process uses (3D, compute, video…).", 3),
                4 => (t.gmem.clone(), t.gmem_heat, "Video memory (VRAM) plus system memory the GPU uses for this process.", 4),
                5 => (t.down.clone(), t.down_heat, net_hint, 7),
                6 => (t.up.clone(), t.up_heat, net_hint, 8),
                7 => ("".into(), 0.0, "Process ID. Select a row and press Delete to end it.", 5),
                _ => ("".into(), 0.0, "The account the process runs as.", 6),
            };
            heads.push(HeadInfo { col: i as i32, label: COLUMNS[i].1.into(), total, heat, hint: hint.into(), sort, r: from_right, w: width as f32 });
        }
        heads.reverse();
        update_model(&self.heads, heads);
        if app.get_heads().row_count() != self.heads.row_count() {
            app.set_heads(ModelRc::from(self.heads.clone()));
        }
    }

    /// A column header was dragged and dropped at `frac` of the header width.
    fn drop_column(&mut self, col: i32, frac: f32) {
        let Some(app) = self.app.upgrade() else { return };
        let Some(&(id, ..)) = COLUMNS.get(col.max(0) as usize) else { return };
        let width = app.window().size().width as f32 / app.window().scale_factor();
        let x = frac * width;
        let prefs = settings::get(|s| s.clone());
        // Left edges of visible columns, in the same coordinates.
        let visible = Self::visible_columns(&prefs);
        let right_edge = width - 14.0 - if app.get_has_detail() { 360.0 } else { 0.0 };
        let mut from_right = 0f32;
        let mut centers: Vec<(String, f32)> = Vec::new();
        for &(i, w) in visible.iter().rev() {
            from_right += w as f32;
            centers.push((COLUMNS[i].0.to_string(), right_edge - from_right + w as f32 / 2.0));
        }
        centers.reverse();
        let target = centers.iter().filter(|(c, _)| c != id).position(|(_, cx)| x < *cx);
        settings::update(|s| {
            s.columns.retain(|c| c != id);
            let others: Vec<String> = centers.iter().map(|(c, _)| c.clone()).filter(|c| c != id).collect();
            // Insert before the column we were dropped left of, keeping hidden columns in place.
            let pos = match target.and_then(|t| others.get(t)) {
                Some(before) => s.columns.iter().position(|c| c == before).unwrap_or(s.columns.len()),
                None => others.last().and_then(|l| s.columns.iter().position(|c| c == l)).map_or(s.columns.len(), |p| p + 1),
            };
            s.columns.insert(pos, id.to_string());
        });
        self.render_processes();
        crate::settings_page::refresh();
    }

    // ---- Long history ------------------------------------------------------------

    /// Adds this sample to the coarse tiers (only kept if the setting asks for them).
    fn feed_long_history(&mut self) {
        let history = settings::get(|s| s.history);
        if history == 0 || self.snap.tick == self.long_fed {
            return;
        }
        self.long_fed = self.snap.tick;
        let s = &self.snap;
        let mut v: Vec<(String, f32)> = vec![("cpu".into(), s.cpu.usage), ("mem".into(), s.mem.history.last()), ("mem.2".into(), s.mem.swap_history.last())];
        if let Some(w) = s.cpu.power_w {
            v.push(("cpu.2".into(), w));
        }
        for (i, g) in s.gpus.iter().enumerate() {
            v.push((format!("gpu{i}"), g.busy));
            v.push((format!("gpu{i}.2"), g.vram_history.last()));
        }
        for (i, d) in s.disks.iter().enumerate() {
            v.push((format!("disk{i}"), d.active_pct));
            v.push((format!("disk{i}.r"), d.read_bps as f32));
            v.push((format!("disk{i}.w"), d.write_bps as f32));
        }
        for (i, n) in s.nets.iter().enumerate() {
            v.push((format!("net{i}.rx"), n.rx_bps as f32));
            v.push((format!("net{i}.tx"), n.tx_bps as f32));
        }
        let now = s.now;
        self.tier10.feed(now, &v);
        if history >= 2 {
            self.tier60.feed(now, &v);
        }
    }

    /// The coarse tier to use for the current range, if any.
    fn tier(&self) -> Option<&Tier> {
        match self.view_span {
            s if s <= 600.0 => None,
            s if s <= 4.0 * 3600.0 => Some(&self.tier10),
            _ => Some(&self.tier60),
        }
    }

    fn series(&self, key: &str, short: &Ring, max: f32) -> Series {
        if let Some(t) = self.tier()
            && let Some(ring) = t.series.get(key)
        {
            let tl = self.timeline();
            return Timeline { times: &t.times, start: tl.start, end: tl.end }.series(ring, max);
        }
        self.timeline().series(short, max)
    }

    fn single(&self, key: &str, r: &Ring, max: f32, color: u32) -> Tile {
        Tile { a: self.series(key, r, max), color_a: rgb(color), ..Default::default() }
    }

    #[allow(clippy::too_many_arguments)]
    fn pair(&self, ka: &str, kb: &str, a: &Ring, b: &Ring, max: f32, ca: u32, cb: u32) -> Tile {
        Tile {
            a: self.series(ka, a, max),
            b: self.series(kb, b, max),
            has_b: true,
            color_a: rgb(ca),
            color_b: rgb(cb),
            ..Default::default()
        }
    }

    /// Hover text on long ranges, from the coarse tier.
    fn probe_long(&self, key: &str, x: f32) -> Option<String> {
        let t = self.tier()?;
        let tl0 = self.timeline();
        let tl = Timeline { times: &t.times, start: tl0.start, end: tl0.end };
        let at = tl.time_at(x);
        let get = |k: &str| t.series.get(k).and_then(|r| tl.value_at(r, at));
        let (st, text) = if let Some(i) = key.strip_prefix("net") {
            let (r, w) = (get(&format!("net{i}.rx"))?, get(&format!("net{i}.tx"))?);
            (r.0, format!("↓ {}\n↑ {}", fmt::rate_bits(r.1 as f64), fmt::rate_bits(w.1 as f64)))
        } else if let Some(i) = key.strip_prefix("disk").and_then(|k| k.strip_suffix(".2")) {
            let (r, w) = (get(&format!("disk{i}.r"))?, get(&format!("disk{i}.w"))?);
            (r.0, format!("Read {}\nWrite {}", fmt::rate_bytes(r.1 as f64), fmt::rate_bytes(w.1 as f64)))
        } else if key == "cpu.2" {
            let v = get(key)?;
            (v.0, format!("{:.0} W", v.1))
        } else {
            let v = get(key)?;
            (v.0, fmt::pct(v.1))
        };
        Some(format!("{}  (average of {})\n{text}", self.clock(st), span_words(t.period)))
    }
}

// ---- Row building ------------------------------------------------------------

struct RowCtx {
    mem_total: u64,
    vram_total: u64,
    col: i32,
    desc: bool,
    /// 0 friendly, 1 process name, 2 both.
    name_style: usize,
    /// 1 for "% of machine", thread count for "% of one core".
    cpu_scale: f32,
    mem_mb: bool,
    net_bits: bool,
    spark_n: usize,
}

impl RowCtx {
    fn mem(&self, b: u64) -> SharedString {
        if self.mem_mb { fmt::bytes_mb(b).into() } else { fmt::bytes(b).into() }
    }

    fn net(&self, bps: f64) -> SharedString {
        if bps < 1024.0 {
            SharedString::new()
        } else if self.net_bits {
            fmt::rate_bits(bps).into()
        } else {
            fmt::rate_bytes_short(bps).into()
        }
    }
}

impl RowCtx {
    fn order(&self, o: Ordering) -> Ordering {
        if self.desc { o.reverse() } else { o }
    }
}

struct Agg<'a> {
    name: &'a str,
    cpu: f32,
    mem: u64,
    gpu: f32,
    gmem: u64,
    down: f64,
    up: f64,
    pid: u32,
    user: &'a str,
}

impl<'a> Agg<'a> {
    fn of(name: &'a str, ps: &[&'a Process]) -> Self {
        Agg {
            name,
            cpu: ps.iter().map(|p| p.cpu).sum(),
            mem: ps.iter().map(|p| p.mem_private).sum(),
            gpu: ps.iter().map(|p| p.gpu).sum::<f32>().min(100.0),
            gmem: ps.iter().map(|p| p.gpu_mem).sum(),
            down: ps.iter().map(|p| p.net_rx).sum(),
            up: ps.iter().map(|p| p.net_tx).sum(),
            pid: ps.iter().map(|p| p.pid).min().unwrap_or(0),
            user: ps.first().map_or("", |p| &p.user),
        }
    }

    fn cmp(&self, o: &Agg, col: i32) -> Ordering {
        match col {
            0 => cmp_name(self.name, o.name),
            1 => self.cpu.total_cmp(&o.cpu),
            2 => self.mem.cmp(&o.mem),
            3 => self.gpu.total_cmp(&o.gpu),
            4 => self.gmem.cmp(&o.gmem),
            5 => self.pid.cmp(&o.pid),
            7 => self.down.total_cmp(&o.down),
            8 => self.up.total_cmp(&o.up),
            _ => self.user.cmp(o.user),
        }
    }
}

fn cmp_proc(a: &Process, b: &Process, col: i32) -> Ordering {
    match col {
        0 => cmp_name(a.display_name(), b.display_name()),
        1 => a.cpu.total_cmp(&b.cpu),
        2 => a.mem_private.cmp(&b.mem_private),
        3 => a.gpu.total_cmp(&b.gpu),
        4 => a.gpu_mem.cmp(&b.gpu_mem),
        5 => a.pid.cmp(&b.pid),
        7 => a.net_rx.total_cmp(&b.net_rx),
        8 => a.net_tx.total_cmp(&b.net_tx),
        _ => a.user.cmp(&b.user),
    }
}

fn cmp_name(a: &str, b: &str) -> Ordering {
    a.bytes().map(|c| c.to_ascii_lowercase()).cmp(b.bytes().map(|c| c.to_ascii_lowercase()))
}

fn matches(p: &Process, filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    let has = |s: &str| s.to_lowercase().contains(filter);
    p.pid.to_string() == filter
        || has(&p.name)
        || p.pretty.as_deref().is_some_and(has)
        || has(&p.group)
        || has(&p.cmdline)
}

fn group_key(p: &Process) -> String {
    format!("g:{:?}:{}", p.kind, p.group)
}

/// The process that best represents a group: a root (parent outside the group),
/// preferring the one named like the app, then the one using the most memory.
fn group_root(procs: &[Process], key: &str) -> Option<u32> {
    let members: Vec<&Process> = procs.iter().filter(|p| group_key(p) == key).collect();
    let pids: HashSet<u32> = members.iter().map(|p| p.pid).collect();
    let score = |p: &&&Process| (p.display_name().eq_ignore_ascii_case(&p.group), p.mem_private);
    members
        .iter()
        .filter(|p| !pids.contains(&p.ppid))
        .max_by_key(score)
        .or_else(|| members.iter().max_by_key(score))
        .map(|p| p.pid)
}

/// Scaled so ~10 MB/s is fully hot.
fn heat_net(bps: f64) -> f32 {
    (bps / 10e6).sqrt().min(1.0) as f32
}

fn heat_cpu(v: f32) -> f32 {
    (v / 25.0).sqrt().min(1.0)
}

fn heat_of(part: u64, scale: u64) -> f32 {
    (part as f32 / scale.max(1) as f32).sqrt().min(1.0)
}

fn proc_row(p: &Process, depth: i32, ctx: &RowCtx) -> ProcRow {
    let (name, sub): (&str, SharedString) = if p.kernel_thread {
        (&p.name, "kernel thread".into())
    } else {
        match (ctx.name_style, &p.pretty) {
            (1, Some(pretty)) => (&p.name, pretty.as_ref().into()),
            (1, None) | (0, _) => (if ctx.name_style == 1 { &p.name } else { p.display_name() }, SharedString::new()),
            (_, Some(_)) => (p.display_name(), p.name.as_ref().into()),
            (_, None) => (&p.name, SharedString::new()),
        }
    };
    let recent: Vec<f32> = p.cpu_history.iter().skip(p.cpu_history.len().saturating_sub(ctx.spark_n)).collect();
    let peak = recent.iter().copied().fold(2.0, f32::max);
    ProcRow {
        kind: 2,
        depth,
        expanded: false,
        key: format!("p:{}", p.pid).into(),
        pid: p.pid as i32,
        name: name.into(),
        sub,
        avatar: initial(name),
        tint: tint(&p.group),
        cpu: fmt::pct(p.cpu * ctx.cpu_scale).into(),
        cpu_heat: heat_cpu(p.cpu),
        mem: ctx.mem(p.mem_private),
        mem_heat: heat_of(p.mem_private, ctx.mem_total / 10),
        gpu: fmt::pct(p.gpu).into(),
        gpu_heat: (p.gpu / 100.0).sqrt(),
        gmem: if p.gpu_mem > 0 { ctx.mem(p.gpu_mem) } else { SharedString::new() },
        gmem_heat: heat_of(p.gpu_mem, ctx.vram_total / 4),
        down: ctx.net(p.net_rx),
        down_heat: heat_net(p.net_rx),
        up: ctx.net(p.net_tx),
        up_heat: heat_net(p.net_tx),
        pid_text: p.pid.to_string().into(),
        user: p.user.as_ref().into(),
        spark: graph::slice(&recent, ctx.spark_n, peak),
    }
}

fn group_row(name: &str, key: &str, ps: &[&Process], agg: &Agg, open: bool, ctx: &RowCtx) -> ProcRow {
    let hist = summed_history(ps);
    let recent = &hist[SPARK - ctx.spark_n..];
    let peak = recent.iter().copied().fold(2.0, f32::max);
    ProcRow {
        kind: 1,
        depth: 0,
        expanded: open,
        key: key.into(),
        pid: agg.pid as i32,
        name: name.into(),
        sub: format!("{} processes", ps.len()).into(),
        avatar: initial(name),
        tint: tint(name),
        cpu: fmt::pct(agg.cpu * ctx.cpu_scale).into(),
        cpu_heat: heat_cpu(agg.cpu),
        mem: ctx.mem(agg.mem),
        mem_heat: heat_of(agg.mem, ctx.mem_total / 10),
        gpu: fmt::pct(agg.gpu).into(),
        gpu_heat: (agg.gpu / 100.0).sqrt(),
        gmem: if agg.gmem > 0 { ctx.mem(agg.gmem) } else { SharedString::new() },
        gmem_heat: heat_of(agg.gmem, ctx.vram_total / 4),
        down: ctx.net(agg.down),
        down_heat: heat_net(agg.down),
        up: ctx.net(agg.up),
        up_heat: heat_net(agg.up),
        pid_text: SharedString::new(),
        user: agg.user.into(),
        spark: graph::slice(recent, ctx.spark_n, peak),
    }
}

/// Members' CPU histories summed, aligned on the newest sample.
fn summed_history(ps: &[&Process]) -> [f32; SPARK] {
    let mut hist = [0f32; SPARK];
    for p in ps {
        let offset = SPARK - p.cpu_history.len();
        for (i, v) in p.cpu_history.iter().enumerate() {
            hist[offset + i] += v;
        }
    }
    hist
}

fn link(p: &Process) -> Link {
    Link {
        pid: p.pid as i32,
        name: p.display_name().into(),
        sub: format!("{}  ·  PID {}  ·  {}", p.name, p.pid, fmt::pct(p.cpu)).into(),
        avatar: initial(p.display_name()),
        tint: tint(&p.group),
    }
}

fn initial(name: &str) -> SharedString {
    name.chars()
        .find(|c| c.is_alphanumeric())
        .map_or("?".into(), |c| c.to_uppercase().collect::<String>().into())
}

fn state_name(s: u8) -> &'static str {
    match s {
        b'R' => "Running",
        b'S' => "Sleeping",
        b'D' => "Waiting on I/O",
        b'Z' => "Zombie",
        b'T' => "Stopped",
        b't' => "Traced",
        b'I' => "Idle",
        b'X' => "Dead",
        _ => "Unknown",
    }
}

fn disk_kind(d: &sophon_core::disk::DiskState) -> &'static str {
    match (d.rotational, d.removable, d.name.starts_with("nvme")) {
        (_, true, _) => "Removable",
        (true, _, _) => "HDD",
        (_, _, true) => "NVMe SSD",
        _ => "SSD",
    }
}

fn net_title(n: &sophon_core::net::NetIface) -> &'static str {
    if n.wireless { "Wi-Fi" } else { "Ethernet" }
}

// ---- Small builders ----------------------------------------------------------


struct Moment {
    t: f32,
    cpu: Vec<(u32, String, f32)>,
    gpu: Vec<(u32, String, f32)>,
}

/// "45 seconds", "5 minutes", "1 minute 30 seconds".
fn span_words(secs: f32) -> String {
    let s = secs.round() as u64;
    match (s / 60, s % 60) {
        (0, s) => format!("{s} second{}", if s == 1 { "" } else { "s" }),
        (m, 0) => format!("{m} minute{}", if m == 1 { "" } else { "s" }),
        (m, s) => format!("{m} min {s} s"),
    }
}

fn model<T: Clone + 'static>(v: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(v))
}

/// Replaces a model's contents, touching only rows that changed so Slint
/// re-renders as little as possible.
fn update_model<T: Clone + PartialEq + 'static>(m: &VecModel<T>, new: Vec<T>) {
    if m.row_count() != new.len() {
        m.set_vec(new);
        return;
    }
    for (i, row) in new.into_iter().enumerate() {
        if m.row_data(i).as_ref() != Some(&row) {
            m.set_row_data(i, row);
        }
    }
}

/// Average of each physical core's threads, as (core id, history).
fn per_core(c: &sophon_core::cpu::CpuState) -> Vec<(u32, Ring)> {
    let mut cores: Vec<u32> = c.core_of.clone();
    cores.sort_unstable();
    cores.dedup();
    cores
        .into_iter()
        .map(|core| {
            let threads: Vec<&Ring> = c.per_thread_history.iter().enumerate().filter(|(i, _)| c.core_of.get(*i) == Some(&core)).map(|(_, h)| h).collect();
            let len = threads.iter().map(|h| h.len()).min().unwrap_or(0);
            let mut avg = Ring::default();
            for k in (0..len).rev() {
                let sum: f32 = threads.iter().filter_map(|h| h.back(k)).sum();
                avg.push(sum / threads.len().max(1) as f32);
            }
            (core, avg)
        })
        .collect()
}

/// Coarse history for long Rewind ranges.
const LONG: usize = 1440;

#[derive(Default)]
struct Tier {
    /// Seconds per bucket.
    period: f32,
    times: Ring<LONG>,
    series: HashMap<String, Ring<LONG>>,
    acc: HashMap<String, (f32, u32)>,
    bucket_start: f32,
}

impl Tier {
    fn new(period: f32) -> Self {
        Tier { period, ..Default::default() }
    }

    fn feed(&mut self, now: f32, values: &[(String, f32)]) {
        if self.acc.is_empty() {
            self.bucket_start = now;
        }
        for (k, v) in values {
            let a = self.acc.entry(k.clone()).or_default();
            a.0 += v;
            a.1 += 1;
        }
        if now - self.bucket_start >= self.period {
            for (k, (sum, n)) in self.acc.drain() {
                self.series.entry(k).or_default().push(sum / n.max(1) as f32);
            }
            self.times.push(now);
        }
    }
}

fn chart(label: &str, scale: &str, tile: Tile) -> Chart {
    Chart { label: label.into(), scale: scale.into(), tile, ..Default::default() }
}

fn stats(rows: &[(&str, String)]) -> ModelRc<Stat> {
    model(rows.iter().map(|(l, v)| Stat { label: (*l).into(), value: v.into() }).collect())
}

fn segment(start: f32, frac: f32, color: u32, label: &str, value: String) -> Segment {
    Segment { start, frac, color: rgb(color), label: label.into(), value: value.into() }
}

/// Settings changed: re-render with the new preferences.
pub fn settings_changed() {
    with(|ui| ui.render());
}
