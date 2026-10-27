//! Sophon: a fast, beautiful task manager for Linux.

slint::include_modules!();

mod actions;
mod fmt;
mod graph;
mod icon;
mod png;
mod snapshot;
mod settings;
mod space_ui;
mod ui;
mod admin;
mod panels;
#[cfg(feature = "docker")]
mod docker_panel;
#[cfg(feature = "proxy")]
mod proxy_panel;
#[cfg(feature = "updates")]
mod update;
mod ebpf;
mod helper;
mod settings_page;

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use slint::winit_030::{EventResult, WinitWindowAccessor, winit::event::WindowEvent};
use sophon_core::Sampler;


/// State shared between the UI thread and the sampler thread.
pub struct Shared {
    /// Current tab (0 = processes); the process table is only sampled when shown.
    pub tab: AtomicU8,
    pub hidden: AtomicBool,
    /// The Processes panel is the current tab (drives whether we sample it).
    pub processes_visible: AtomicBool,
    /// Sampling interval while visible, in ms (0 = paused). From settings.
    pub interval_ms: AtomicU64,
    /// Sampling interval while in the tray, in ms (0 = off).
    pub tray_interval_ms: AtomicU64,
    /// The window was closed to the tray (unmapped, no taskbar entry).
    pub in_tray: AtomicBool,
    /// Wakes the sampler early (e.g. when the window becomes visible again).
    pub wake: mpsc::Sender<()>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    match std::env::args().nth(1).as_deref() {
        Some("--helper") => helper::run(),
        Some("--install") => return Ok(icon::install(false)?),
        Some("--uninstall") => return Ok(icon::install(true)?),
        Some("--help" | "-h") => {
            println!("sophon            run\nsophon --install  add Sophon to the app launcher (name + icon in the taskbar)\nsophon --uninstall  remove it");
            return Ok(());
        }
        _ => {}
    }
    let start_in_tray = std::env::args().any(|a| a == "--tray");
    let prefs = settings::get(|s| s.clone());
    // CPU rendering with partial redraws is the lightest option for a UI that
    // changes once a second; GPU rendering is a setting. SLINT_BACKEND overrides.
    if std::env::var_os("SLINT_BACKEND").is_none() {
        let backend = if prefs.renderer == 1 { "winit-femtovg" } else { "winit-software" };
        // SAFETY: no other threads exist yet.
        unsafe { std::env::set_var("SLINT_BACKEND", backend) };
    }
    if (prefs.ui_scale - 1.0).abs() > 0.01 && std::env::var_os("SLINT_SCALE_FACTOR").is_none() {
        // SAFETY: as above.
        unsafe { std::env::set_var("SLINT_SCALE_FACTOR", prefs.ui_scale.to_string()) };
    }

    let app = AppWindow::new()?;
    // Lets the desktop match the window to sophon.desktop (taskbar name and icon).
    // Must follow platform init (AppWindow::new) and precede the window being shown.
    slint::set_xdg_app_id("sophon")?;
    app.set_app_icon(icon::image(64));
    // Restores the Settings → General "Always on top" choice at startup.
    set_always_on_top(&app, prefs.always_on_top);
    let (wake, rx) = mpsc::channel();
    let shared = Arc::new(Shared { tab: AtomicU8::new(0), hidden: AtomicBool::new(false),
        in_tray: AtomicBool::new(false),
        processes_visible: AtomicBool::new(true),
        interval_ms: AtomicU64::new(settings::UPDATE_MS[prefs.update_speed]),
        tray_interval_ms: AtomicU64::new(settings::TRAY_SECS[prefs.tray_rate] * 1000),
        wake,
    });
    ui::install(&app, shared.clone());
    space_ui::install(&app);
    settings_page::install(&app, shared.clone());
    #[cfg(feature = "updates")]
    update::install(&app, shared.clone());
    for tab in panels::all_tabs() {
        tab.install(&app);
    }
    // Data-driven tab bar from the panel registry.
    let reg = panels::tabs();
    let labels: Vec<slint::SharedString> = reg.iter().map(|p| p.title().into()).collect();
    let ids: Vec<slint::SharedString> = reg.iter().map(|p| p.id().into()).collect();
    app.set_tab_labels(slint::ModelRc::new(slint::VecModel::from(labels)));
    app.set_tab_ids(slint::ModelRc::new(slint::VecModel::from(ids)));

    // Visibility: X11 reports occlusion directly; on Wayland we notice that
    // frames stop being presented (compositors withhold frame callbacks).
    let events_shared = shared.clone();
    app.window().on_winit_window_event(move |_, event| {
        match event {
            WindowEvent::RedrawRequested => ui::frame_presented(&events_shared),
            WindowEvent::Occluded(hidden) => ui::set_hidden(&events_shared, *hidden),
            WindowEvent::Focused(true) => ui::focused(&events_shared),
            _ => {}
        }
        EventResult::Propagate
    });
    let stall_shared = shared.clone();
    let stall_timer = slint::Timer::default();
    stall_timer.start(slint::TimerMode::Repeated, Duration::from_secs(1), move || ui::check_stalled(&stall_shared));

    let sampler_shared = shared.clone();
    std::thread::Builder::new()
        .name("sampler".into())
        .spawn(move || sampler_loop(rx, sampler_shared))?;

    // Like Steam: closing the window sends Sophon to the tray (no taskbar entry;
    // the compositor animates it away). The tray icon brings it back.
    let close_shared = shared.clone();
    let weak = app.as_weak();
    app.window().on_close_requested(move || {
        if !settings::get(|s| s.close_to_tray) {
            let _ = slint::quit_event_loop();
            return slint::CloseRequestResponse::HideWindow;
        }
        if let Some(app) = weak.upgrade() {
            ui::to_tray(&close_shared, app.window().size());
        }
        slint::CloseRequestResponse::HideWindow
    });
    app.on_quit(|| {
        debug_log("quit (Ctrl+Q)");
        let _ = slint::quit_event_loop();
    });

    // Tray icon: hover for a live summary, click (or "Open") to bring the window back.
    let tray = Tray::new()?;
    tray.set_image(icon::image(64));
    tray.set_summary("Sophon".into());
    let tray_shared = shared.clone();
    let weak = app.as_weak();
    tray.on_open(move || {
        let Some(app) = weak.upgrade() else { return };
        // "Toggle" hides an open window instead of raising it.
        if settings::get(|s| s.tray_click == 1) && !tray_shared.in_tray.load(Relaxed) && !tray_shared.hidden.load(Relaxed) {
            ui::to_tray(&tray_shared, app.window().size());
            let _ = app.hide();
            return;
        }
        // Wayland apps can't raise or un-minimize themselves, but a freshly mapped
        // window always comes up in front, so re-map it if it's already open.
        if !tray_shared.in_tray.load(Relaxed) {
            let _ = app.hide();
        }
        let _ = app.show();
        // A window that started hidden in the tray was never mapped; on
        // Wayland its first show() creates the surface but never draws into
        // it. One hide/show cycle gets it mapped properly (the same path as
        // reopening after close-to-tray, which works).
        if NEVER_SHOWN.swap(false, Relaxed) {
            let _ = app.hide();
            let _ = app.show();
        }
        apply_on_top(&app);
        if let Some(size) = ui::from_tray(&tray_shared) {
            app.window().set_size(size);
        }
        if std::env::var_os("SOPHON_DEBUG").is_some() {
            let s = app.window().size();
            eprintln!("sophon: window shown at {}x{}", s.width, s.height);
        }
    });
    tray.on_quit(|| {
        debug_log("quit (tray menu)");
        let _ = slint::quit_event_loop();
    });
    ui::set_tray(tray.as_weak());

    if let Some(dir) = std::env::var_os("SOPHON_SNAPSHOT") {
        snapshot::start(&app, dir.into());
    }

    if start_in_tray {
        NEVER_SHOWN.store(true, Relaxed);
        // Launched at login with "start hidden in the tray".
        ui::to_tray(&shared, app.window().size());
    } else {
        app.show()?;
        apply_on_top(&app);
    }
    if prefs.admin_on_start {
        admin::enable();
    }
    // Keep running while the window is minimized or hidden; only an explicit quit ends it.
    let result = slint::run_event_loop_until_quit();
    debug_log(&format!("event loop ended: {result:?}"));
    result?;
    drop(tray);
    Ok(())
}

/// Rebuilds the tab bar from the panel registry after a Settings → Tabs
/// change. A cheap no-op unless the enabled tabs actually changed.
pub fn sync_tabs(app: &AppWindow) {
    use slint::Model;
    let reg = panels::tabs();
    let current = app.get_tab_ids();
    if current.row_count() == reg.len()
        && (0..reg.len()).all(|i| current.row_data(i).is_some_and(|id| id.as_str() == reg[i].id()))
    {
        return;
    }
    let labels: Vec<slint::SharedString> = reg.iter().map(|p| p.title().into()).collect();
    let ids: Vec<slint::SharedString> = reg.iter().map(|p| p.id().into()).collect();
    app.set_tab_labels(slint::ModelRc::new(slint::VecModel::from(labels)));
    app.set_tab_ids(slint::ModelRc::new(slint::VecModel::from(ids)));
    // The current tab may have shifted or vanished; re-resolve what's shown.
    let tab = app.get_tab().clamp(0, reg.len() as i32 - 1);
    app.set_tab(tab);
    app.invoke_tab_changed(tab);
}

fn sampler_loop(wake: mpsc::Receiver<()>, shared: Arc<Shared>) {
    let mut sampler = match Sampler::new() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("sophon: cannot read /proc: {e}");
            return;
        }
    };
    // Prime the counters, then take a quick first real sample so the window fills fast.
    sampler.set_ebpf_source(admin::net_counters);
    sampler.sample(true);
    std::thread::sleep(Duration::from_millis(250));

    let mut next = Instant::now();
    let mut tick: u64 = 0;
    loop {
        let hidden = shared.hidden.load(Relaxed);
        tick += 1;
        // The process table is the costliest part: every tick while it's on screen,
        // every other tick otherwise (Rewind uses it to name spike culprits), and
        // at the slow hidden cadence so spikes while gaming can still be explained.
        let with_processes = hidden || shared.processes_visible.load(Relaxed) || tick % 2 == 0;
        let snap = sampler.sample(with_processes);
        if slint::invoke_from_event_loop(move || ui::apply(snap)).is_err() {
            return; // UI is gone
        }

        let ms = if hidden { shared.tray_interval_ms.load(Relaxed) } else { shared.interval_ms.load(Relaxed) };
        // 0 means paused (or "off" in the tray): sleep until something changes.
        let interval = if ms == 0 { Duration::from_secs(3600) } else { Duration::from_millis(ms) };
        let now = Instant::now();
        next = if next + interval > now { next + interval } else { now + interval };
        match wake.recv_timeout(next.saturating_duration_since(now)) {
            Ok(()) => {
                while wake.try_recv().is_ok() {}
                next = Instant::now();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn debug_log(msg: &str) {
    if std::env::var_os("SOPHON_DEBUG").is_some() {
        eprintln!("sophon: {msg}");
    }
}

/// Applies the Settings → General "Always on top" choice through the winit
/// window (Slint's window API doesn't expose the window level). X11 and
/// Windows honour it; on Wayland the compositor decides.
pub fn set_always_on_top(app: &AppWindow, on: bool) {
    ON_TOP.store(on, Relaxed);
    // Touching the winit window creates it. Doing that before the first show
    // (e.g. when starting hidden in the tray) leaves a Wayland window that
    // never draws, so it waits until the window is up (see `apply_on_top`).
    if app.window().is_visible() {
        apply_on_top(app);
    }
}

/// Sophon started hidden in the tray and the window hasn't been shown yet.
static NEVER_SHOWN: AtomicBool = AtomicBool::new(false);

/// The last "always on top" choice, applied whenever the window is shown.
static ON_TOP: AtomicBool = AtomicBool::new(false);

fn apply_on_top(app: &AppWindow) {
    use slint::winit_030::winit::window::WindowLevel;
    let level = if ON_TOP.load(Relaxed) { WindowLevel::AlwaysOnTop } else { WindowLevel::Normal };
    let _ = app.window().with_winit_window(|w| w.set_window_level(level));
}
