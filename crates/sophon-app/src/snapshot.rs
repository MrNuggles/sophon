//! Dev tool: `SOPHON_SNAPSHOT=<dir> sophon` renders every tab and device view
//! (dark and light) to PNG files, then exits. Works however the window is
//! stacked, because Slint renders the snapshot into a buffer itself.
//! `SOPHON_SNAPSHOT_FILTER=<text>` searches before the detail shots.

use std::path::PathBuf;
use std::time::Duration;

use slint::{ComponentHandle, Timer, TimerMode};
#[cfg(feature = "docker")]
use slint::Model;

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
    let mut steps = vec![Step::Wait, Step::Wait, Step::Wait, Step::Wait, Step::Shot("processes")];
    // Optional: SOPHON_SNAPSHOT_FILTER narrows the process list (search box) for the detail shots.
    if let Ok(filter) = std::env::var("SOPHON_SNAPSHOT_FILTER") {
        steps.push(Step::Filter(filter));
    }
    steps.extend([Step::SelectRow(1), Step::Wait, Step::Wait, Step::Shot("processes-detail")]);
    steps.extend([Step::ToggleRow(1), Step::Wait, Step::SelectRow(2), Step::Wait, Step::Wait, Step::Shot("processes-child")]);
    // Collapse the Apps section.
    steps.extend([Step::SelectRow(0), Step::Wait, Step::Shot("processes-collapsed"), Step::SelectRow(0)]);
    steps.push(Step::TabId("performance"));
    // Rewind: simulated hover (crosshair on every graph + tooltip), then a drag-zoom.
    steps.extend([Step::Tile(0), Step::Wait, Step::HoverAt("cpu", 0.97), Step::Wait, Step::Shot("rewind-hover")]);
    steps.extend([Step::HoverAt("", -1.0), Step::Zoom(0.6, 0.95), Step::Wait, Step::Shot("rewind-zoom"), Step::ResetView]);
    for i in 0..8 {
        steps.extend([Step::Tile(i), Step::Wait, Step::Wait]);
        steps.push(Step::Shot(["perf-0", "perf-1", "perf-2", "perf-3", "perf-4", "perf-5", "perf-6", "perf-7"][i as usize]));
    }
    // Space map on the disk holding /home (tile 4 on the dev machine).
    let disk_tile = std::env::var("SOPHON_SNAPSHOT_DISK").ok().and_then(|v| v.parse().ok()).unwrap_or(4);
    steps.extend([Step::Tile(disk_tile), Step::Wait, Step::Scroll(-600.0), Step::Wait, Step::Shot("space-intro")]);
    steps.push(Step::SpaceScan);
    steps.extend(std::iter::repeat_with(|| Step::Wait).take(25));
    steps.extend([Step::Scroll(-600.0), Step::Wait, Step::Shot("space")]);
    steps.extend([Step::SpaceZoom(0), Step::Wait, Step::Wait, Step::Shot("space-zoom")]);
    steps.extend([Step::SpaceSelect(40, false), Step::SpaceSelect(60, true), Step::SpaceSelect(80, true), Step::Wait, Step::Shot("space-selected")]);
    steps.extend([Step::SpaceAskTrash, Step::Wait, Step::Shot("space-confirm"), Step::SpaceCancelTrash, Step::SpaceSelect(-1, false)]);
    steps.extend([Step::Mode(1), Step::Wait, Step::Shot("space-light")]);
    // Docker (feature builds): it ships disabled, so enable it from
    // Settings → Tabs first, then shoot the tab — rows, the stop confirmation
    // (opened and cancelled, never accepted), a collapsed header, a rename.
    #[cfg(feature = "docker")]
    {
        steps.extend([Step::SettingsOpen(true), Step::SetToggle("tabs_docker", true)]);
        steps.extend(std::iter::repeat_with(|| Step::Wait).take(5));
        steps.extend([Step::TabId("docker")]);
        steps.extend(std::iter::repeat_with(|| Step::Wait).take(6));
        steps.push(Step::Shot("docker"));
        steps.extend([Step::DockerAskStop, Step::Wait, Step::Shot("docker-confirm"), Step::DockerCancelStop]);
        // A collapsed server header hides that server's containers.
        steps.extend([Step::DockerToggleServer, Step::Wait, Step::Shot("docker-collapsed"), Step::DockerToggleServer]);
        // Rename the first server (persisted to the config, shown at once), then put it back.
        steps.extend([Step::DockerRenameServer("Tiny renamed"), Step::Wait, Step::Shot("docker-renamed"), Step::DockerRenameServer("Tiny")]);
        // Off again from Settings: the tab disappears live.
        steps.extend([
            Step::SettingsOpen(true),
            Step::SetToggle("tabs_docker", false),
            Step::Wait,
            Step::TabId("processes"),
            Step::Wait,
            Step::Shot("live-docker-off"),
            Step::SettingsOpen(false),
        ]);
    }
    // Settings: every category. Settings is an overlay now — opened from the
    // header's ☰ button, not the tab bar.
    steps.push(Step::SettingsOpen(true));
    // Change a few settings, then show the Processes tab to prove they apply live.
    steps.extend([Step::SetChoice("density", 1), Step::SetChoice("graph_style", 3), Step::SetSlide("roundness", 0.2), Step::SetToggle("show_kernel", true)]);
    steps.extend([Step::TabId("processes"), Step::Wait, Step::Shot("live-compact-bars"), Step::SettingsOpen(true)]);
    // Reset them.
    steps.extend([Step::SetChoice("density", 0), Step::SetChoice("graph_style", 0), Step::SetSlide("roundness", 1.0), Step::SetToggle("show_kernel", false)]);
    let setting_shots = ["settings-0", "settings-1", "settings-2", "settings-3", "settings-4", "settings-5", "settings-6", "settings-7-tabs"];
    for (i, name) in setting_shots.iter().enumerate() {
        steps.extend([Step::SettingsCategory(i as i32), Step::Wait, Step::Shot(name)]);
    }
    steps.extend([Step::SettingsOpen(false)]);
    steps.extend([Step::TabId("performance"), Step::Wait, Step::Mode(1), Step::Tile(0), Step::Wait, Step::Wait, Step::Shot("light-perf")]);
    steps.extend([Step::TabId("processes"), Step::Wait, Step::Wait, Step::Shot("light-processes"), Step::Quit]);

    let weak = app.as_weak();
    let mut steps = steps.into_iter();
    let timer = Box::leak(Box::new(Timer::default()));
    timer.start(TimerMode::Repeated, Duration::from_millis(600), move || {
        let Some(app) = weak.upgrade() else { return };
        match steps.next() {
            Some(Step::Wait) => {}
            Some(Step::Shot(name)) => match app.window().take_snapshot() {
                Ok(buf) => {
                    let path = dir.join(format!("{name}.png"));
                    if let Err(e) = crate::png::write(&path, buf.width(), buf.height(), buf.as_bytes()) {
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
            Some(Step::SettingsOpen(open)) => app.set_settings_open(open),
            Some(Step::TabId(id)) => {
                let t = crate::panels::panels().iter().position(|p| p.id == id).unwrap_or(0) as i32;
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

