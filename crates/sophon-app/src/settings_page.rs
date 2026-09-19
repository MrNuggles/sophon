//! The Settings tab: builds its rows from [`Settings`] and applies every change live.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use slint::{Color, ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::graph::rgb;
use crate::settings::{self, COLUMNS, SWATCHES, Settings, TRAY_SECS, UPDATE_MS};
use crate::{AppWindow, SettingItem, SettingsPage, Shared, Theme};

pub const CATEGORIES: [&str; 7] =
    ["General", "Appearance", "Resource use", "Processes tab", "Performance tab", "Space map", "Tray"];

struct Page {
    app: slint::Weak<AppWindow>,
    shared: Arc<Shared>,
    category: usize,
    search: String,
    /// Performance tiles currently known: (stable id, label).
    tiles: Vec<(String, String)>,
    /// Mount points for the space map's default drive.
    mounts: Vec<String>,
    admin_status: String,
    items: std::rc::Rc<VecModel<SettingItem>>,
}

thread_local! {
    static PAGE: RefCell<Option<Page>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut Page) -> R) -> Option<R> {
    PAGE.with_borrow_mut(|p| p.as_mut().map(f))
}

pub fn install(app: &AppWindow, shared: Arc<Shared>) {
    let items = std::rc::Rc::new(VecModel::default());
    let g = app.global::<SettingsPage>();
    g.set_items(ModelRc::from(items.clone()));
    g.set_categories(ModelRc::new(VecModel::from(CATEGORIES.iter().map(|c| SharedString::from(*c)).collect::<Vec<_>>())));
    g.set_swatches(ModelRc::new(VecModel::from(SWATCHES.iter().map(|&c| rgb(c)).collect::<Vec<Color>>())));
    PAGE.set(Some(Page {
        app: app.as_weak(),
        shared,
        category: 0,
        search: String::new(),
        tiles: Vec::new(),
        mounts: Vec::new(),
        admin_status: String::new(),
        items,
    }));

    g.on_select_category(|i| {
        with(|p| {
            p.category = i.max(0) as usize;
            p.search.clear();
        });
        refresh();
    });
    g.on_search(|text| {
        with(|p| p.search = text.trim().to_lowercase());
        refresh();
    });
    g.on_toggle(|id, v| change(&id, |s| set_bool(s, &id, v)));
    g.on_choose(|id, i| change(&id, |s| set_choice(s, &id, i.max(0) as usize)));
    g.on_pick_color(|id, c| {
        let hex = ((c.red() as u32) << 16) | ((c.green() as u32) << 8) | c.blue() as u32;
        change(&id, |s| set_color(s, &id, hex));
    });
    g.on_slide(|id, v| change(&id, |s| set_number(s, &id, v)));
    g.on_reset(|id| change(&id, |s| reset(s, &id)));
    g.on_order_move(|id, from, to| {
        let tiles = with(|p| p.tiles.iter().map(|t| t.0.clone()).collect::<Vec<_>>()).unwrap_or_default();
        change(&id, |s| {
            let list = match id.as_str() {
                "columns" => &mut s.columns,
                "tiles" => {
                    if s.tile_order.is_empty() {
                        s.tile_order = tiles.clone();
                    }
                    &mut s.tile_order
                }
                _ => return,
            };
            let (from, to) = (from.max(0) as usize, to.max(0) as usize);
            if from < list.len() && to < list.len() {
                let item = list.remove(from);
                list.insert(to, item);
            }
        });
    });
    g.on_order_toggle(|id, i, visible| {
        let tiles = with(|p| p.tiles.iter().map(|t| t.0.clone()).collect::<Vec<_>>()).unwrap_or_default();
        change(&id, |s| {
            let (order, hidden) = match id.as_str() {
                "columns" => (s.columns.clone(), &mut s.hidden_columns),
                "tiles" => (if s.tile_order.is_empty() { tiles.clone() } else { ordered_tiles(&s.tile_order, &tiles) }, &mut s.hidden_tiles),
                _ => return,
            };
            let Some(key) = order.get(i.max(0) as usize) else { return };
            hidden.retain(|h| h != key);
            if !visible {
                hidden.push(key.clone());
            }
        });
    });
    g.on_list_add(|id| {
        if id.as_str() != "space_excluded" {
            return;
        }
        // Native folder picker (KDE or GNOME), off the UI thread.
        std::thread::spawn(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
            let out = std::process::Command::new("kdialog")
                .args(["--getexistingdirectory", &home, "--title", "Exclude a folder from the space map"])
                .output()
                .or_else(|_| {
                    std::process::Command::new("zenity")
                        .args(["--file-selection", "--directory", "--title=Exclude a folder from the space map"])
                        .output()
                });
            if let Ok(o) = out
                && o.status.success()
            {
                let path = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if !path.is_empty() {
                    let _ = slint::invoke_from_event_loop(move || {
                        change("space_excluded", |s| {
                            if !s.space_excluded.contains(&path) {
                                s.space_excluded.push(path);
                            }
                        })
                    });
                }
            }
        });
    });
    g.on_list_remove(|id, i| {
        change(&id, |s| {
            if id.as_str() == "space_excluded" && (i as usize) < s.space_excluded.len() {
                s.space_excluded.remove(i as usize);
            }
        })
    });
    g.on_press(|id| {
        if id.as_str() == "admin" {
            crate::admin::toggle();
        }
    });

    apply_all();
    refresh();
}

/// Tiles the Performance tab currently has, so they can be ordered and hidden here.
pub fn set_tiles(tiles: Vec<(String, String)>) {
    let changed = with(|p| {
        let changed = p.tiles != tiles;
        p.tiles = tiles;
        changed && p.category == 4
    });
    if changed == Some(true) {
        refresh();
    }
}

pub fn set_mounts(mounts: Vec<String>) {
    with(|p| p.mounts = mounts);
}

pub fn set_admin_status(status: String) {
    with(|p| p.admin_status = status);
    refresh();
}

/// Applies a change, saves it, and updates everything that depends on it.
fn change(id: &str, f: impl FnOnce(&mut Settings)) {
    let before = settings::get(Settings::clone);
    settings::update(f);
    let after = settings::get(Settings::clone);
    if before != after {
        apply_all();
        if before.autostart != after.autostart || before.start_in_tray != after.start_in_tray {
            write_autostart(&after);
        }
        crate::ui::settings_changed();
        crate::space_ui::settings_changed();
    }
    let _ = id;
    refresh();
}

/// Pushes settings to the theme, the sampler and the UI.
pub fn apply_all() {
    let s = settings::get(Settings::clone);
    let Some((app, shared)) = with(|p| (p.app.upgrade(), p.shared.clone())) else { return };
    if let Some(app) = app {
        let t = app.global::<Theme>();
        t.set_mode(s.theme as i32);
        match (s.accent_follow, desktop_accent()) {
            (true, Some(c)) => {
                t.set_accent_custom(rgb(c));
                t.set_accent_is_custom(true);
            }
            (true, None) => t.set_accent_is_custom(false),
            (false, _) => {
                t.set_accent_custom(rgb(s.accent));
                t.set_accent_is_custom(s.accent != 0x8f7dff);
            }
        }
        t.set_cpu(rgb(s.color_cpu));
        t.set_mem(rgb(s.color_mem));
        t.set_gpu(rgb(s.color_gpu));
        t.set_disk(rgb(s.color_disk));
        t.set_net(rgb(s.color_net));
        t.set_heat(rgb(s.heat_color));
        t.set_heat_intensity(s.heat_intensity);
        t.set_font_scale(s.font_scale);
        t.set_round(s.roundness);
        t.set_anim([1.0, 0.5, 0.0][s.animations]);
        t.set_row_height(if s.density == 1 { 26.0 } else { 32.0 });
        t.set_graph_style(s.graph_style as i32);
        t.set_line_scale(s.line_width);
        t.set_graph_grid(s.graph_grid);
    }
    crate::graph::set_style(s.graph_style);
    crate::fmt::set_fahrenheit(s.fahrenheit);
    shared.interval_ms.store(UPDATE_MS[s.update_speed], Relaxed);
    shared.tray_interval_ms.store(TRAY_SECS[s.tray_rate] * 1000, Relaxed);
    let _ = shared.wake.send(());
}

/// Rebuilds the visible rows (category page or search results).
pub fn refresh() {
    let s = settings::get(Settings::clone);
    let Some((items, rows)) = with(|p| {
        let all = build(&s, p);
        let rows: Vec<SettingItem> = if p.search.is_empty() {
            all.into_iter().filter(|i| i.category.as_str() == CATEGORIES[p.category]).collect()
        } else {
            all.into_iter()
                .filter(|i| {
                    i.kind != 7
                        && (i.label.to_lowercase().contains(&p.search) || i.hint.to_lowercase().contains(&p.search) || i.category.to_lowercase().contains(&p.search))
                })
                .collect()
        };
        (p.items.clone(), rows)
    }) else {
        return;
    };
    if items.row_count() == rows.len() {
        for (i, r) in rows.into_iter().enumerate() {
            if items.row_data(i).as_ref() != Some(&r) {
                items.set_row_data(i, r);
            }
        }
    } else {
        items.set_vec(rows);
    }
}

// ---- Rows --------------------------------------------------------------------

fn strings(v: &[&str]) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(v.iter().map(|s| SharedString::from(*s)).collect::<Vec<_>>()))
}

fn heading(cat: &str, label: &str) -> SettingItem {
    SettingItem { category: cat.into(), label: label.into(), kind: 7, ..Default::default() }
}

fn toggle(cat: &str, id: &str, label: &str, hint: &str, on: bool, default: bool) -> SettingItem {
    SettingItem { id: id.into(), category: cat.into(), label: label.into(), hint: hint.into(), kind: 0, on, modified: on != default, ..Default::default() }
}

fn choice(cat: &str, id: &str, label: &str, hint: &str, options: &[&str], current: usize, default: usize) -> SettingItem {
    SettingItem {
        id: id.into(),
        category: cat.into(),
        label: label.into(),
        hint: hint.into(),
        kind: 1,
        options: strings(options),
        choice: current as i32,
        modified: current != default,
        ..Default::default()
    }
}

fn color(cat: &str, id: &str, label: &str, hint: &str, value: u32, default: u32) -> SettingItem {
    SettingItem { id: id.into(), category: cat.into(), label: label.into(), hint: hint.into(), kind: 2, color: rgb(value), modified: value != default, ..Default::default() }
}

#[allow(clippy::too_many_arguments)]
fn slider(cat: &str, id: &str, label: &str, hint: &str, value: f32, default: f32, (min, max, step): (f32, f32, f32), text: String) -> SettingItem {
    SettingItem {
        id: id.into(),
        category: cat.into(),
        label: label.into(),
        hint: hint.into(),
        kind: 3,
        value,
        min,
        max,
        step,
        value_text: text.into(),
        modified: (value - default).abs() > 1e-3,
        ..Default::default()
    }
}

fn pct(v: f32) -> String {
    format!("{:.0}%", v * 100.0)
}

/// Tiles in the saved order, with any new ones appended.
fn ordered_tiles(order: &[String], known: &[String]) -> Vec<String> {
    let mut out: Vec<String> = order.iter().filter(|t| known.contains(t)).cloned().collect();
    for t in known {
        if !out.contains(t) {
            out.push(t.clone());
        }
    }
    out
}

fn build(s: &Settings, p: &Page) -> Vec<SettingItem> {
    let d = Settings::default();
    let mut v = Vec::new();

    let c = CATEGORIES[0];
    v.push(heading(c, "STARTUP"));
    v.push(toggle(c, "autostart", "Start at login", "Launch Sophon automatically when you sign in.", s.autostart, d.autostart));
    v.push(toggle(c, "start_in_tray", "Start hidden in the tray", "When starting at login, go straight to the tray instead of opening the window.", s.start_in_tray, d.start_in_tray));
    v.push(heading(c, "WINDOW & TRAY"));
    v.push(choice(c, "close_to_tray", "Close button", "What the window's close button does.", &["Send to tray", "Quit Sophon"], if s.close_to_tray { 0 } else { 1 }, 0));
    v.push(choice(c, "tray_click", "Clicking the tray icon", "Open always brings the window up; Toggle also hides it when it's already open.", &["Open", "Toggle"], s.tray_click, d.tray_click));
    v.push(heading(c, "SAFETY"));
    v.push(choice(c, "confirm_end", "Confirm before ending tasks", "Ask before ending processes, so a stray click or Delete can't close something by accident.", &["Never", "App groups only", "Always"], s.confirm_end, d.confirm_end));
    v.push(heading(c, "ADMIN ACCESS"));
    let admin_active = crate::admin::is_active();
    v.push(SettingItem {
        id: "admin".into(),
        category: c.into(),
        label: "Admin features".into(),
        hint: "Asks for your password once, then lets Sophon end or re-prioritise any process (including system ones) and count UDP network traffic per app (games, voice chat). A small separate helper does the privileged work and stops when Sophon closes; Sophon itself never runs as admin.".into(),
        kind: 6,
        button: if admin_active { "Turn off".into() } else { "Enable…".into() },
        status: p.admin_status.clone().into(),
        ..Default::default()
    });
    v.push(toggle(c, "admin_on_start", "Enable admin features when Sophon starts", "Ask for your password at startup instead of when you first need it.", s.admin_on_start, d.admin_on_start));

    let c = CATEGORIES[1];
    v.push(heading(c, "THEME"));
    v.push(choice(c, "theme", "Theme", "Follow your desktop, or pick one.", &["System", "Light", "Dark"], s.theme, d.theme));
    v.push(toggle(c, "accent_follow", "Follow desktop accent colour", "Use your KDE or GNOME accent colour for highlights.", s.accent_follow, d.accent_follow));
    if !s.accent_follow {
        v.push(color(c, "accent", "Accent colour", "Buttons, selections and highlights.", s.accent, d.accent));
    }
    v.push(heading(c, "COLOURS"));
    v.push(color(c, "color_cpu", "CPU colour", "Graphs and activity lines for the processor.", s.color_cpu, d.color_cpu));
    v.push(color(c, "color_mem", "Memory colour", "Memory graphs.", s.color_mem, d.color_mem));
    v.push(color(c, "color_gpu", "GPU colour", "Graphics card graphs.", s.color_gpu, d.color_gpu));
    v.push(color(c, "color_disk", "Disk colour", "Disk graphs.", s.color_disk, d.color_disk));
    v.push(color(c, "color_net", "Network colour", "Network graphs.", s.color_net, d.color_net));
    v.push(color(c, "heat_color", "Heat-map colour", "Tint for busy cells in the process table.", s.heat_color, d.heat_color));
    v.push(slider(c, "heat_intensity", "Heat-map intensity", "How strongly busy cells are tinted. 0% turns the heat map off.", s.heat_intensity, d.heat_intensity, (0.0, 2.0, 0.1), pct(s.heat_intensity)));
    v.push(heading(c, "LAYOUT"));
    v.push(slider(c, "ui_scale", "Interface scale", "Makes everything bigger or smaller.", s.ui_scale, d.ui_scale, (0.75, 2.0, 0.05), pct(s.ui_scale)));
    if let Some(last) = v.last_mut() {
        last.restart = true;
    }
    v.push(slider(c, "font_scale", "Text size", "Scales all text.", s.font_scale, d.font_scale, (0.75, 1.5, 0.05), pct(s.font_scale)));
    v.push(choice(c, "density", "Row density", "Compact fits more processes on screen.", &["Comfortable", "Compact"], s.density, d.density));
    v.push(slider(c, "roundness", "Corner roundness", "0% gives square corners.", s.roundness, d.roundness, (0.0, 2.0, 0.1), pct(s.roundness)));
    v.push(choice(c, "animations", "Animations", "Reduced halves their length; Off makes every change instant.", &["Full", "Reduced", "Off"], s.animations, d.animations));
    v.push(heading(c, "GRAPHS"));
    v.push(choice(c, "graph_style", "Graph style", "How time-series graphs are drawn.", &["Gradient", "Filled", "Line", "Bars"], s.graph_style, d.graph_style));
    v.push(slider(c, "line_width", "Line thickness", "Thickness of graph lines.", s.line_width, d.line_width, (0.5, 3.0, 0.1), format!("{:.1}×", s.line_width)));
    v.push(toggle(c, "graph_grid", "Grid lines", "Faint guide lines behind graphs.", s.graph_grid, d.graph_grid));

    let c = CATEGORIES[2];
    v.push(heading(c, "UPDATES"));
    v.push(choice(c, "update_speed", "Update speed", "How often numbers refresh while the window is open. Faster costs a little more CPU; Paused freezes everything.", &["0.5 s", "1 s", "2 s", "5 s", "Paused"], s.update_speed, d.update_speed));
    v.push(choice(c, "tray_rate", "Updates while in the tray", "Keeps the tray summary and Rewind history going. Off uses nothing while Sophon sits in the tray.", &["5 s", "10 s", "30 s", "Off"], s.tray_rate, d.tray_rate));
    v.push(heading(c, "DRAWING"));
    v.push(choice(c, "renderer", "Renderer", "CPU drawing uses the least memory (~20 MB); GPU drawing uses a graphics context (~45 MB) but less CPU for big windows.", &["CPU", "GPU"], s.renderer, d.renderer));
    if let Some(last) = v.last_mut() {
        last.restart = true;
    }
    v.push(toggle(c, "show_own_usage", "Show Sophon's own usage", "Shows Sophon's CPU and memory in the status bar.", s.show_own_usage, d.show_own_usage));

    let c = CATEGORIES[3];
    v.push(heading(c, "COLUMNS"));
    let labels: Vec<SharedString> = s.columns.iter().map(|id| COLUMNS.iter().find(|c| c.0 == id).map_or(id.as_str(), |c| c.1).into()).collect();
    let flags: Vec<bool> = s.columns.iter().map(|id| !s.hidden_columns.contains(id)).collect();
    v.push(SettingItem {
        id: "columns".into(),
        category: c.into(),
        label: "Columns".into(),
        hint: "Drag to reorder, tick to show. You can also drag column headers in the table, and drag their left edge to resize.".into(),
        kind: 5,
        items: ModelRc::new(VecModel::from(labels)),
        item_flags: ModelRc::new(VecModel::from(flags)),
        modified: s.columns != d.columns || !s.hidden_columns.is_empty() || !s.column_widths.is_empty(),
        ..Default::default()
    });
    v.push(heading(c, "LIST"));
    v.push(choice(c, "grouping", "Arrange processes", "By app groups processes under the app that launched them; Tree shows parents and children; Flat is one list.", &["By app", "Tree", "Flat"], s.grouping, d.grouping));
    v.push(toggle(c, "show_kernel", "Show kernel threads", "Kernel worker threads (kworker, migration…). Usually noise.", s.show_kernel, d.show_kernel));
    v.push(toggle(c, "show_other_users", "Show other users' processes", "Includes system services and other accounts.", s.show_other_users, d.show_other_users));
    v.push(choice(c, "name_style", "Names", "Friendly shows app names (Waterfox); Process shows the kernel's name (Isolated Web Co); Both shows the app name with the process name underneath.", &["Friendly", "Process", "Both"], s.name_style, d.name_style));
    v.push(choice(c, "spark_len", "Activity line length", "History shown in each row's activity line.", &["30 s", "60 s"], s.spark_len, d.spark_len));
    v.push(heading(c, "NUMBERS"));
    v.push(choice(c, "cpu_mode", "CPU percentages", "Whole machine: all threads = 100% (Windows style). One core: one busy core = 100% (like top).", &["Whole machine", "One core"], s.cpu_mode, d.cpu_mode));
    v.push(choice(c, "mem_units", "Memory units", "Automatic switches between KB, MB and GB.", &["Automatic", "Always MB"], s.mem_units, d.mem_units));
    v.push(choice(c, "net_units", "Network units", "Bytes per second (like downloads) or bits per second (like internet plans).", &["Bytes", "Bits"], s.net_units, d.net_units));
    v.push(heading(c, "ACTIONS"));
    v.push(choice(c, "double_click", "Double-click a row", "What double-clicking a process or app does.", &["Details", "Expand", "Open location"], s.double_click, d.double_click));
    v.push(choice(c, "delete_key", "Delete key", "What pressing Delete does to the selection.", &["End task", "Ask first", "Force kill"], s.delete_key, d.delete_key));

    let c = CATEGORIES[4];
    v.push(heading(c, "DEVICES"));
    let known: Vec<String> = p.tiles.iter().map(|t| t.0.clone()).collect();
    let order = ordered_tiles(&s.tile_order, &known);
    let tile_labels: Vec<SharedString> = order.iter().map(|id| p.tiles.iter().find(|t| &t.0 == id).map_or(id.clone(), |t| t.1.clone()).into()).collect();
    let tile_flags: Vec<bool> = order.iter().map(|id| !s.hidden_tiles.contains(id)).collect();
    v.push(SettingItem {
        id: "tiles".into(),
        category: c.into(),
        label: "Sidebar devices".into(),
        hint: if p.tiles.is_empty() { "Open the Performance tab once to list your devices here.".into() } else { "Drag to reorder, tick to show. Right-click a tile in the sidebar to move or hide it too.".into() },
        kind: 5,
        items: ModelRc::new(VecModel::from(tile_labels)),
        item_flags: ModelRc::new(VecModel::from(tile_flags)),
        modified: !s.tile_order.is_empty() || !s.hidden_tiles.is_empty(),
        ..Default::default()
    });
    v.push(toggle(c, "show_virtual_nics", "Show virtual network adapters", "Bridges, VPN tunnels and container networks.", s.show_virtual_nics, d.show_virtual_nics));
    v.push(heading(c, "GRAPHS"));
    v.push(choice(c, "cpu_view", "CPU detail", "Below the main CPU graph: nothing, one graph per thread, or one per physical core.", &["Total only", "Per thread", "Per core"], s.cpu_view, d.cpu_view));
    v.push(choice(c, "history", "History kept", "How far back Rewind can go. Longer history is stored at lower resolution (10 s for an hour, 1 min for a day) and uses a little more memory.", &["10 minutes", "1 hour", "24 hours"], s.history, d.history));
    let ranges: &[&str] = match s.history {
        0 => &["1 min", "5 min", "10 min"],
        1 => &["1 min", "5 min", "10 min", "1 hour"],
        _ => &["1 min", "5 min", "10 min", "1 hour", "24 hours"],
    };
    v.push(choice(c, "default_range", "Default time range", "The range graphs show when you open Sophon.", ranges, s.default_range.min(ranges.len() - 1), d.default_range));
    v.push(choice(c, "spike_sensitivity", "Spike markers", "How eagerly spikes on CPU and GPU graphs are marked with their culprits.", &["Off", "Low", "Normal", "High"], s.spike_sensitivity, d.spike_sensitivity));
    v.push(heading(c, "UNITS"));
    v.push(choice(c, "fahrenheit", "Temperature", "", &["°C", "°F"], s.fahrenheit as usize, d.fahrenheit as usize));
    v.push(toggle(c, "show_power", "Show power draw", "CPU package and GPU power graphs and figures.", s.show_power, d.show_power));

    let c = CATEGORIES[5];
    v.push(heading(c, "SCANNING"));
    let mut drives = vec!["Home drive"];
    let mounts: Vec<&str> = p.mounts.iter().map(String::as_str).collect();
    drives.extend(&mounts);
    let current = if s.space_default.is_empty() { 0 } else { mounts.iter().position(|m| *m == s.space_default).map_or(0, |i| i + 1) };
    v.push(choice(c, "space_default", "Default drive", "Which drive the space map opens on.", &drives, current, 0));
    v.push(choice(c, "space_priority", "Scan priority", "Idle yields to everything else (best while gaming); Normal scans faster when the system is busy.", &["Idle", "Normal"], s.space_priority, d.space_priority));
    v.push(toggle(c, "space_hidden_files", "Include hidden files", "Files and folders whose names start with a dot (like .cache).", s.space_hidden_files, d.space_hidden_files));
    v.push(SettingItem {
        id: "space_excluded".into(),
        category: c.into(),
        label: "Excluded folders".into(),
        hint: "These folders are skipped when scanning.".into(),
        kind: 4,
        items: ModelRc::new(VecModel::from(s.space_excluded.iter().map(|e| SharedString::from(e.as_str())).collect::<Vec<_>>())),
        modified: !s.space_excluded.is_empty(),
        ..Default::default()
    });
    v.push(toggle(c, "space_remember", "Remember scans", "Keep the last scan of each drive between restarts (a few MB on disk).", s.space_remember, d.space_remember));
    v.push(heading(c, "LOOK"));
    v.push(choice(c, "space_color", "Colour blocks by", "Folder gives each top-level folder its own colour; File type colours by what's inside (video, games, code…); Age shows recently changed files brightest.", &["Folder", "File type", "Age"], s.space_color, d.space_color));
    v.push(slider(c, "space_min_block", "Smallest block", "Blocks smaller than this are left out, keeping the map readable.", s.space_min_block, d.space_min_block, (1.0, 12.0, 1.0), format!("{:.0} px", s.space_min_block)));

    let c = CATEGORIES[6];
    v.push(heading(c, "TRAY SUMMARY"));
    v.push(toggle(c, "tip_cpu", "CPU", "Shown when hovering the tray icon.", s.tip_cpu, d.tip_cpu));
    v.push(toggle(c, "tip_ram", "Memory", "", s.tip_ram, d.tip_ram));
    v.push(toggle(c, "tip_gpu", "GPU", "", s.tip_gpu, d.tip_gpu));
    v.push(toggle(c, "tip_temps", "Temperatures", "CPU and GPU temperatures.", s.tip_temps, d.tip_temps));
    v.push(toggle(c, "tip_net", "Network", "Download and upload rates.", s.tip_net, d.tip_net));
    v
}

// ---- Changes ---------------------------------------------------------------------

fn set_bool(s: &mut Settings, id: &str, v: bool) {
    match id {
        "autostart" => s.autostart = v,
        "start_in_tray" => s.start_in_tray = v,
        "admin_on_start" => s.admin_on_start = v,
        "accent_follow" => s.accent_follow = v,
        "graph_grid" => s.graph_grid = v,
        "show_own_usage" => s.show_own_usage = v,
        "show_kernel" => s.show_kernel = v,
        "show_other_users" => s.show_other_users = v,
        "show_virtual_nics" => s.show_virtual_nics = v,
        "show_power" => s.show_power = v,
        "space_hidden_files" => s.space_hidden_files = v,
        "space_remember" => s.space_remember = v,
        "tip_cpu" => s.tip_cpu = v,
        "tip_ram" => s.tip_ram = v,
        "tip_gpu" => s.tip_gpu = v,
        "tip_temps" => s.tip_temps = v,
        "tip_net" => s.tip_net = v,
        _ => {}
    }
}

fn set_choice(s: &mut Settings, id: &str, i: usize) {
    match id {
        "close_to_tray" => s.close_to_tray = i == 0,
        "tray_click" => s.tray_click = i,
        "confirm_end" => s.confirm_end = i,
        "theme" => s.theme = i,
        "density" => s.density = i,
        "animations" => s.animations = i,
        "graph_style" => s.graph_style = i,
        "update_speed" => s.update_speed = i,
        "tray_rate" => s.tray_rate = i,
        "renderer" => s.renderer = i,
        "grouping" => s.grouping = i,
        "name_style" => s.name_style = i,
        "spark_len" => s.spark_len = i,
        "cpu_mode" => s.cpu_mode = i,
        "mem_units" => s.mem_units = i,
        "net_units" => s.net_units = i,
        "double_click" => s.double_click = i,
        "delete_key" => s.delete_key = i,
        "cpu_view" => s.cpu_view = i,
        "history" => {
            s.history = i;
            s.default_range = s.default_range.min(i + 2);
        }
        "default_range" => s.default_range = i,
        "spike_sensitivity" => s.spike_sensitivity = i,
        "fahrenheit" => s.fahrenheit = i == 1,
        "space_priority" => s.space_priority = i,
        "space_color" => s.space_color = i,
        "space_default" => {
            let mounts = with(|p| p.mounts.clone()).unwrap_or_default();
            s.space_default = if i == 0 { String::new() } else { mounts.get(i - 1).cloned().unwrap_or_default() };
        }
        _ => {}
    }
}

fn set_color(s: &mut Settings, id: &str, c: u32) {
    match id {
        "accent" => s.accent = c,
        "color_cpu" => s.color_cpu = c,
        "color_mem" => s.color_mem = c,
        "color_gpu" => s.color_gpu = c,
        "color_disk" => s.color_disk = c,
        "color_net" => s.color_net = c,
        "heat_color" => s.heat_color = c,
        _ => {}
    }
}

fn set_number(s: &mut Settings, id: &str, v: f32) {
    match id {
        "heat_intensity" => s.heat_intensity = v,
        "ui_scale" => s.ui_scale = v,
        "font_scale" => s.font_scale = v,
        "roundness" => s.roundness = v,
        "line_width" => s.line_width = v,
        "space_min_block" => s.space_min_block = v,
        _ => {}
    }
}

fn reset(s: &mut Settings, id: &str) {
    let d = Settings::default();
    match id {
        "columns" => {
            s.columns = d.columns;
            s.hidden_columns.clear();
            s.column_widths.clear();
        }
        "tiles" => {
            s.tile_order.clear();
            s.hidden_tiles.clear();
        }
        "space_excluded" => s.space_excluded.clear(),
        "space_default" => s.space_default.clear(),
        "close_to_tray" => s.close_to_tray = d.close_to_tray,
        "fahrenheit" => s.fahrenheit = d.fahrenheit,
        _ => {
            // Every other id maps to one field; copy it from the defaults via the setters.
            let mut probe = d.clone();
            set_bool(s, id, get_bool(&probe, id).unwrap_or(false));
            if let Some(i) = get_choice(&probe, id) {
                set_choice(s, id, i);
            }
            if let Some(c) = get_color(&probe, id) {
                set_color(s, id, c);
            }
            if let Some(n) = get_number(&mut probe, id) {
                set_number(s, id, n);
            }
        }
    }
}

fn get_bool(s: &Settings, id: &str) -> Option<bool> {
    Some(match id {
        "autostart" => s.autostart,
        "start_in_tray" => s.start_in_tray,
        "admin_on_start" => s.admin_on_start,
        "accent_follow" => s.accent_follow,
        "graph_grid" => s.graph_grid,
        "show_own_usage" => s.show_own_usage,
        "show_kernel" => s.show_kernel,
        "show_other_users" => s.show_other_users,
        "show_virtual_nics" => s.show_virtual_nics,
        "show_power" => s.show_power,
        "space_hidden_files" => s.space_hidden_files,
        "space_remember" => s.space_remember,
        "tip_cpu" => s.tip_cpu,
        "tip_ram" => s.tip_ram,
        "tip_gpu" => s.tip_gpu,
        "tip_temps" => s.tip_temps,
        "tip_net" => s.tip_net,
        _ => return None,
    })
}

fn get_choice(s: &Settings, id: &str) -> Option<usize> {
    Some(match id {
        "tray_click" => s.tray_click,
        "confirm_end" => s.confirm_end,
        "theme" => s.theme,
        "density" => s.density,
        "animations" => s.animations,
        "graph_style" => s.graph_style,
        "update_speed" => s.update_speed,
        "tray_rate" => s.tray_rate,
        "renderer" => s.renderer,
        "grouping" => s.grouping,
        "name_style" => s.name_style,
        "spark_len" => s.spark_len,
        "cpu_mode" => s.cpu_mode,
        "mem_units" => s.mem_units,
        "net_units" => s.net_units,
        "double_click" => s.double_click,
        "delete_key" => s.delete_key,
        "cpu_view" => s.cpu_view,
        "history" => s.history,
        "default_range" => s.default_range,
        "spike_sensitivity" => s.spike_sensitivity,
        "space_priority" => s.space_priority,
        "space_color" => s.space_color,
        _ => return None,
    })
}

fn get_color(s: &Settings, id: &str) -> Option<u32> {
    Some(match id {
        "accent" => s.accent,
        "color_cpu" => s.color_cpu,
        "color_mem" => s.color_mem,
        "color_gpu" => s.color_gpu,
        "color_disk" => s.color_disk,
        "color_net" => s.color_net,
        "heat_color" => s.heat_color,
        _ => return None,
    })
}

fn get_number(s: &mut Settings, id: &str) -> Option<f32> {
    Some(match id {
        "heat_intensity" => s.heat_intensity,
        "ui_scale" => s.ui_scale,
        "font_scale" => s.font_scale,
        "roundness" => s.roundness,
        "line_width" => s.line_width,
        "space_min_block" => s.space_min_block,
        _ => return None,
    })
}

/// The desktop's accent colour: KDE (kdeglobals) or GNOME (gsettings).
fn desktop_accent() -> Option<u32> {
    let home = std::env::var("HOME").ok()?;
    if let Ok(text) = std::fs::read_to_string(format!("{home}/.config/kdeglobals")) {
        let mut section = "";
        let (mut accent, mut selection) = (None, None);
        for line in text.lines() {
            if line.starts_with('[') {
                section = line;
                continue;
            }
            let parse = |v: &str| {
                let p: Vec<u32> = v.split(',').filter_map(|x| x.trim().parse().ok()).collect();
                (p.len() >= 3).then(|| (p[0] << 16) | (p[1] << 8) | p[2])
            };
            if let Some(v) = line.strip_prefix("AccentColor=") {
                accent = parse(v);
            } else if section == "[Colors:Selection]"
                && let Some(v) = line.strip_prefix("BackgroundNormal=")
            {
                selection = parse(v);
            }
        }
        if let Some(c) = accent.or(selection) {
            return Some(c);
        }
    }
    let out = std::process::Command::new("gsettings").args(["get", "org.gnome.desktop.interface", "accent-color"]).output().ok()?;
    let name = String::from_utf8_lossy(&out.stdout).trim().trim_matches('\'').to_string();
    Some(match name.as_str() {
        "blue" => 0x3584e4,
        "teal" => 0x2190a4,
        "green" => 0x3a944a,
        "yellow" => 0xc88800,
        "orange" => 0xed5b00,
        "red" => 0xe62d42,
        "pink" => 0xd56199,
        "purple" => 0x9141ac,
        "slate" => 0x6f8396,
        _ => return None,
    })
}

/// Writes or removes ~/.config/autostart/sophon.desktop.
fn write_autostart(s: &Settings) {
    let path = settings::config_dir().parent().map(|p| p.join("autostart/sophon.desktop"));
    let Some(path) = path else { return };
    if !s.autostart {
        let _ = std::fs::remove_file(&path);
        return;
    }
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "sophon".into());
    let args = if s.start_in_tray { " --tray" } else { "" };
    let _ = std::fs::create_dir_all(path.parent().expect("has parent"));
    let _ = std::fs::write(
        &path,
        format!("[Desktop Entry]\nType=Application\nName=Sophon\nExec={exe}{args}\nIcon=sophon\nX-GNOME-Autostart-enabled=true\nNoDisplay=true\n"),
    );
}
