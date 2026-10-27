//! Dev tool: `SOPHON_SNAPSHOT=<dir> sophon` renders every tab and device view
//! (dark and light) to PNG files, then exits. Works however the window is
//! stacked, because Slint renders the snapshot into a buffer itself.
//! `SOPHON_SNAPSHOT_FILTER=<text>` searches before the detail shots.

use std::path::PathBuf;
use std::time::Duration;

use slint::{ComponentHandle, Model, Timer, TimerMode};

use crate::{AppWindow, Theme};

enum Step {
    Wait,
    Shot(&'static str),
    SelectRow(i32),
    ToggleRow(i32),
    Filter(String),
    Scroll(f32),
    SpaceScan,
    SpaceZoom(i32),
    SpaceSelect(i32, bool),
    /// Simulates hovering graph `key` at a fraction of its width.
    HoverAt(&'static str, f32),
    Zoom(f32, f32),
    ResetView,
    SettingsCategory(i32),
    /// Pretends a newer release exists (no network) so the banner shows.
    #[cfg(feature = "updates")]
    UpdateDemo,
    SetToggle(&'static str, bool),
    SetChoice(&'static str, i32),
    SetSlide(&'static str, f32),
    /// Opens the Trash confirmation for the selection. The snapshot never confirms.
    SpaceAskTrash,
    SpaceCancelTrash,
    /// Opens the Docker stop confirmation for a container (same rule: never
    /// confirmed, so the snapshot can't stop anything real).
    #[cfg(feature = "docker")]
    DockerAskStop,
    #[cfg(feature = "docker")]
    DockerCancelStop,
    /// Collapses the first server header (hides its containers).
    #[cfg(feature = "docker")]
    DockerToggleServer,
    /// Renames the first server via the DockerPage callback (the menu that
    /// opens the inline edit needs a live mouse).
    #[cfg(feature = "docker")]
    DockerRenameServer(&'static str),
    /// Fills the Proxy tab's connect form and submits it (only run when no
    /// real connection is configured, so a mock can't clobber real settings).
    #[cfg(feature = "proxy")]
    ProxyConnect(&'static str, &'static str, &'static str),
    /// Drops a Docker column header at an x position (regression: the drop
    /// must move the named column, not the one at its old display position).
    #[cfg(feature = "docker")]
    DockerDropColumn(&'static str, f32),
    /// Resets the Docker column order to the default.
    #[cfg(feature = "docker")]
    ColumnsReset,
    /// Drops a Processes column (by COLUMNS index) at an x position — same
    /// regression, for the Processes table.
    ProcessesDropColumn(i32, f32),
    /// Forwards to the Processes column-hide callback (-2 resets order).
    ColumnHide(i32),
    /// Switches to the Performance tab's space-map disk: the one holding "/"
    /// (the system disk), else the smallest filesystem — disk indices shuffle
    /// between boots, so this is resolved at run time.
    SpaceDiskAuto,
    /// Opens or closes the Settings overlay (it lives outside the tab bar).
    SettingsOpen(bool),
    /// Switches to the tab with this panel id, resolved at run time (tab
    /// positions shift as optional tabs are toggled).
    TabId(&'static str),
    Tile(i32),
    Mode(i32),
    Quit,
}

pub fn start(app: &AppWindow, dir: PathBuf) {
    let _ = std::fs::create_dir_all(&dir);
    // One grouped pass, in tab order: Processes → Performance (+ space) →
    // Docker → Proxy → Settings → light mode. Tab enable/disable goes through
    // the toggles' global callbacks, so nothing bounces to Settings and back.
    let mut steps = vec![Step::Wait, Step::Wait, Step::Wait, Step::Wait, Step::Shot("processes")];
    // Optional: SOPHON_SNAPSHOT_FILTER narrows the process list (search box) for the detail shots.
    if let Ok(filter) = std::env::var("SOPHON_SNAPSHOT_FILTER") {
        steps.push(Step::Filter(filter));
    }
    steps.extend([Step::SelectRow(1), Step::Wait, Step::Wait, Step::Shot("processes-detail")]);
    steps.extend([Step::ToggleRow(1), Step::Wait, Step::SelectRow(2), Step::Wait, Step::Wait, Step::Shot("processes-child")]);
    // Collapse the Apps section.
    steps.extend([Step::SelectRow(0), Step::Wait, Step::Shot("processes-collapsed"), Step::SelectRow(0)]);
    // Drop the Memory column (COLUMNS index 2) at the far left: the memory
    // cells must move under it. Then reset order/widths/hidden.
    steps.extend([
        Step::ProcessesDropColumn(2, 60.0),
        Step::Wait,
        Step::Shot("processes-dropped"),
        Step::ProcessesDropColumn(2, 60.0),
        Step::Wait,
        Step::Shot("processes-dropped-again"),
    ]);
    steps.extend([Step::ProcessesDropColumn(2, 60.0)]);
    steps.push(Step::ColumnHide(-2));
    // A few settings applied live (compact density, bar graphs, rounder corners).
    steps.extend([
        Step::SetChoice("density", 1),
        Step::SetChoice("graph_style", 3),
        Step::SetSlide("roundness", 0.2),
        Step::SetToggle("show_kernel", true),
        Step::Wait,
        Step::Shot("live-compact-bars"),
        Step::SetChoice("density", 0),
        Step::SetChoice("graph_style", 0),
        Step::SetSlide("roundness", 1.0),
        Step::SetToggle("show_kernel", false),
    ]);

    // Performance (dark): Rewind, then every tile.
    steps.push(Step::TabId("performance"));
    // Rewind: simulated hover (crosshair on every graph + tooltip), then a drag-zoom.
    steps.extend([Step::Tile(0), Step::Wait, Step::HoverAt("cpu", 0.97), Step::Wait, Step::Shot("rewind-hover")]);
    steps.extend([Step::HoverAt("", -1.0), Step::Zoom(0.6, 0.95), Step::Wait, Step::Shot("rewind-zoom"), Step::ResetView]);
    for i in 0..8 {
        steps.extend([Step::Tile(i), Step::Wait, Step::Wait]);
        steps.push(Step::Shot(["perf-0", "perf-1", "perf-2", "perf-3", "perf-4", "perf-5", "perf-6", "perf-7"][i as usize]));
    }
    steps.extend([Step::SpaceDiskAuto, Step::Wait, Step::Scroll(-600.0), Step::Wait, Step::Shot("space-intro")]);
    steps.push(Step::SpaceScan);
    steps.extend(std::iter::repeat_with(|| Step::Wait).take(25));
    steps.extend([Step::Scroll(-600.0), Step::Wait, Step::Shot("space")]);
    steps.extend([Step::SpaceZoom(0), Step::Wait, Step::Wait, Step::Shot("space-zoom")]);
    steps.extend([Step::SpaceSelect(40, false), Step::SpaceSelect(60, true), Step::SpaceSelect(80, true), Step::Wait, Step::Shot("space-selected")]);
    steps.extend([Step::SpaceAskTrash, Step::Wait, Step::Shot("space-confirm"), Step::SpaceCancelTrash, Step::SpaceSelect(-1, false)]);

    // Proxy (dark): the connect form, then the loaded table against a local
    // npm-mock (skipped when a real connection is configured). Its npm.*
    // settings feed the Docker tab's Proxied column, so this runs first.
    #[cfg(feature = "proxy")]
    {
        steps.extend([Step::SetToggle("tabs_proxy", true), Step::TabId("proxy"), Step::Wait, Step::Shot("proxy-connect")]);
        if !crate::settings::get(|s| !s.npm_url.is_empty()) {
            steps.extend([Step::ProxyConnect("http://127.0.0.1:2381", "sophon@mock", "mockpass")]);
            steps.extend(std::iter::repeat_with(|| Step::Wait).take(6));
            steps.push(Step::Shot("proxy"));
        }
        steps.extend([Step::SetToggle("tabs_proxy", false), Step::Wait, Step::Shot("live-proxy-off")]);
    }


    // Docker (dark): the toggle is a global callback, so the tab is enabled
    // without visiting Settings. Shots: the table, the stop confirmation
    // (opened and cancelled, never accepted), a collapsed header, a rename.
    #[cfg(feature = "docker")]
    {
        // Generous waits: a slow/unreachable server blocks the first poll for
        // up to its 4 s timeout per request.
        steps.extend([Step::SetToggle("tabs_docker", true)]);
        steps.extend(std::iter::repeat_with(|| Step::Wait).take(8));
        steps.extend([Step::TabId("docker")]);
        steps.extend(std::iter::repeat_with(|| Step::Wait).take(14));
        steps.push(Step::Shot("docker"));
        steps.extend([Step::DockerAskStop, Step::Wait, Step::Shot("docker-confirm"), Step::DockerCancelStop]);
        // A collapsed server header hides that server's containers.
        steps.extend([Step::DockerToggleServer, Step::Wait, Step::Shot("docker-collapsed"), Step::DockerToggleServer]);
        // Rename the first server (persisted to the config, shown at once), then put it back.
        steps.extend([Step::DockerRenameServer("Tiny renamed"), Step::Wait, Step::Shot("docker-renamed"), Step::DockerRenameServer("Tiny")]);
        // Drop "state" at the far left: the State column (dot + label) must
        // move there, data and all. Then reset the order.
        steps.extend([
            Step::DockerDropColumn("state", 60.0),
            Step::Wait,
            Step::Shot("docker-dropped"),
            Step::ColumnsReset,
        ]);
        // Off again: the tab disappears live and the view falls back to another tab.
        steps.extend([Step::SetToggle("tabs_docker", false), Step::Wait, Step::Shot("live-docker-off")]);
    }

    // Update banner, with a made-up newer release.
    #[cfg(feature = "updates")]
    steps.extend([Step::TabId("processes"), Step::UpdateDemo, Step::Wait, Step::Shot("update-banner")]);

    // Settings (dark): every category in one overlay visit.
    steps.push(Step::SettingsOpen(true));
    let setting_shots = ["settings-0", "settings-1", "settings-2", "settings-3", "settings-4", "settings-5", "settings-6", "settings-7-tabs"];
    for (i, name) in setting_shots.iter().enumerate() {
        steps.extend([Step::SettingsCategory(i as i32), Step::Wait, Step::Shot(name)]);
    }
    steps.push(Step::SettingsOpen(false));

    // Light mode: one switch, every light shot.
    steps.extend([Step::Mode(1), Step::TabId("performance"), Step::Tile(0), Step::Wait, Step::Shot("light-perf")]);
    steps.extend([Step::SpaceDiskAuto, Step::Wait, Step::Shot("space-light")]);
    steps.extend([Step::TabId("processes"), Step::Wait, Step::Shot("light-processes"), Step::Quit]);

    let weak = app.as_weak();
    let mut steps = steps.into_iter();
    let timer = Box::leak(Box::new(Timer::default()));
    timer.start(TimerMode::Repeated, Duration::from_millis(600), move || {
        let Some(app) = weak.upgrade() else { return };
        match steps.next() {
            Some(Step::Wait) => {}
            Some(Step::Shot(name)) => match app.window().take_snapshot() {
                Ok(buf) => {
                    let path = dir.join(format!("{name}.png"));                    if let Err(e) = crate::png::write(&path, buf.width(), buf.height(), buf.as_bytes()) {
                        eprintln!("snapshot {name}: {e}");
                    }
                }
                Err(e) => eprintln!("snapshot {name}: {e}"),
            },
            Some(Step::SelectRow(i)) => app.invoke_select_row(i),
            Some(Step::ToggleRow(i)) => app.invoke_toggle_row(i),
            Some(Step::Filter(f)) => app.invoke_filter_changed(f.into()),
            Some(Step::Scroll(y)) => app.set_perf_scroll(y),
            Some(Step::SpaceScan) => app.global::<crate::Space>().invoke_scan(),
            Some(Step::SpaceZoom(i)) => app.global::<crate::Space>().invoke_zoom(i),
            Some(Step::HoverAt(key, x)) => {
                let h = app.global::<crate::Hover>();
                h.set_graph(key.into());
                h.set_x(x);
            }
            Some(Step::Zoom(a, b)) => app.global::<crate::Hover>().invoke_zoom(a, b),
            Some(Step::SettingsCategory(i)) => {
                let g = app.global::<crate::SettingsPage>();
                g.set_category(i);
                g.invoke_select_category(i);
            }
            #[cfg(feature = "updates")]
            Some(Step::UpdateDemo) => crate::update::demo(),
            Some(Step::SetToggle(id, v)) => app.global::<crate::SettingsPage>().invoke_toggle(id.into(), v),
            Some(Step::SetChoice(id, v)) => app.global::<crate::SettingsPage>().invoke_choose(id.into(), v),
            Some(Step::SetSlide(id, v)) => app.global::<crate::SettingsPage>().invoke_slide(id.into(), v),
            Some(Step::ResetView) => app.global::<crate::Hover>().invoke_reset(),
            Some(Step::SpaceSelect(i, add)) => app.global::<crate::Space>().invoke_select(i, add),
            Some(Step::SpaceAskTrash) => app.global::<crate::Space>().invoke_action(-1, "trash".into()),
            Some(Step::SpaceCancelTrash) => app.global::<crate::Space>().invoke_cancel_trash(),
            #[cfg(feature = "docker")]
            Some(Step::DockerAskStop) => {
                let g = app.global::<crate::DockerPage>();
                let rows = g.get_rows();
                // Prefer a running container (it has a Stop button); any row will
                // do for the dialog shot.
                let mut first: Option<slint::SharedString> = None;
                let mut running: Option<slint::SharedString> = None;
                for i in 0..rows.row_count() {
                    let Some(r) = rows.row_data(i) else { continue };
                    if r.kind != 1 {
                        continue;
                    }
                    if first.is_none() {
                        first = Some(r.id.clone());
                    }
                    if r.running {
                        running = Some(r.id.clone());
                        break;
                    }
                }
                if let Some(id) = running.or(first) {
                    g.invoke_action(id, "stop".into());
                }
            }
            #[cfg(feature = "docker")]
            Some(Step::DockerCancelStop) => app.invoke_confirm_cancelled(),
            #[cfg(feature = "docker")]
            Some(Step::DockerToggleServer) => app.global::<crate::DockerPage>().invoke_toggle_server(0),
            #[cfg(feature = "docker")]
            Some(Step::DockerRenameServer(name)) => app.global::<crate::DockerPage>().invoke_rename_server(0, name.into()),
            #[cfg(feature = "docker")]
            Some(Step::DockerDropColumn(id, x)) => {
                app.global::<crate::DockerPage>().invoke_column_dropped(id.into(), x, 1212.0);
            }
            #[cfg(feature = "docker")]
            Some(Step::ColumnsReset) => app.global::<crate::DockerPage>().invoke_columns_reset(),
            Some(Step::ProcessesDropColumn(col, x)) => app.invoke_column_dropped(col, x, 1212.0),
            Some(Step::ColumnHide(col)) => app.invoke_column_hide(col),
            #[cfg(feature = "proxy")]
            Some(Step::ProxyConnect(url, email, pass)) => {
                app.global::<crate::ProxyPage>().invoke_connect(url.into(), email.into(), pass.into());
            }
            Some(Step::SpaceDiskAuto) => {
                // The quickest scan: the system disk if there is one, else the
                // smallest filesystem on the machine.
                let mounts = sophon_core::space::mounts();
                let target = mounts
                    .iter()
                    .filter(|m| m.total > 0)
                    .min_by_key(|m| (m.path != std::path::Path::new("/"), m.total));
                if let Some(m) = target {
                    let tiles = app.get_tiles();
                    for i in 0..tiles.row_count() {
                        let Some(t) = tiles.row_data(i) else { continue };
                        if t.title.as_str().contains(m.disk.as_str()) {
                            app.set_tile(i as i32);
                            app.invoke_tile_changed(i as i32);
                            break;
                        }
                    }
                }
            }
            Some(Step::SettingsOpen(open)) => app.set_settings_open(open),
            Some(Step::TabId(id)) => {
                let t = crate::panels::tabs().iter().position(|p| p.id() == id).unwrap_or(0) as i32;
                app.set_settings_open(false);
                app.set_tab(t);
                app.invoke_tab_changed(t);
            }
            Some(Step::Tile(i)) => {
                app.set_tile(i);
                app.invoke_tile_changed(i);
            }
            Some(Step::Mode(m)) => app.global::<Theme>().set_mode(m),
            Some(Step::Quit) | None => {
                let _ = slint::quit_event_loop();
            }
        }
    });
}

