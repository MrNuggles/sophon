//! The panel (tab) registry — the single source of truth for which tabs exist,
//! their order, and their titles.
//!
//! "processes" is the app's dashboard and is permanent. Every other tab is an
//! optional [`Tab`]: compiled in with its Cargo feature (docker, proxy) and
//! shown or hidden live from Settings → Tabs. Settings itself is not a tab —
//! it opens from the ☰ button beside the theme toggle. Nothing else in the app
//! hard-codes a tab number — the current panel is resolved by `id`.
//!
//! Adding a tab means:
//!   1. write its module: a controller plus a `Tab` impl (see
//!      `docker_panel.rs` / `proxy_panel.rs`);
//!   2. register one `static` here in [`panels`] (behind its `#[cfg]` if the
//!      module is feature-gated);
//!   3. add one `if root.current-id == "<id>": <View> {}` line in `app.slint`
//!      (plus a `tabs_*` field and toggle row if it's optional).
//! Visibility, install, the tab bar and Settings → Tabs all follow from the
//! registry — nothing else in the app needs touching.

use crate::AppWindow;

/// One tab in the header. `id` matches the `if root.current-id == "<id>"`
/// block in app.slint and is stable across builds (used for settings, etc.).
pub trait Tab {
    fn id(&self) -> &'static str;
    fn title(&self) -> &'static str;
    /// Whether the tab is on (its Settings → Tabs toggle). Permanent tabs are
    /// always true.
    fn enabled(&self) -> bool {
        true
    }
    /// Installs the tab's controller (callbacks, poller thread). Runs once, at
    /// startup, for every compiled-in tab.
    fn install(&self, _app: &AppWindow) {}
    /// The tab's view became visible or hidden; drives its poller. Called on
    /// every tab switch and when the Settings overlay opens or closes.
    fn set_visible(&self, _visible: bool) {}
}

/// The app's dashboard: permanent, rendered by ui.rs itself.
struct ProcessesTab;
impl Tab for ProcessesTab {
    fn id(&self) -> &'static str {
        "processes"
    }
    fn title(&self) -> &'static str {
        "Processes"
    }
}

struct PerformanceTab;
impl Tab for PerformanceTab {
    fn id(&self) -> &'static str {
        "performance"
    }
    fn title(&self) -> &'static str {
        "Performance"
    }
    fn enabled(&self) -> bool {
        crate::settings::get(|s| s.tabs_performance)
    }
}

#[cfg(feature = "docker")]
static DOCKER: crate::docker_panel::Controller = crate::docker_panel::Controller;
#[cfg(feature = "proxy")]
static PROXY: crate::proxy_panel::Controller = crate::proxy_panel::Controller;

/// Every compiled-in tab, in display order — enabled or not. Install must
/// reach all of them (their callbacks and pollers exist regardless of the
/// Settings toggle; the toggle only gates polling and the tab bar).
pub fn all_tabs() -> Vec<&'static dyn Tab> {
    let mut v: Vec<&'static dyn Tab> = vec![&ProcessesTab, &PerformanceTab];
    #[cfg(feature = "docker")]
    v.push(&DOCKER);
    #[cfg(feature = "proxy")]
    v.push(&PROXY);
    v
}

/// The enabled tabs — what the tab bar shows and what tab indices resolve to.
pub fn tabs() -> Vec<&'static dyn Tab> {
    let mut v = all_tabs();
    v.retain(|t| t.enabled());
    v
}

/// The tab id at a tab index (used to resolve what to render).
pub fn id_at(index: i32) -> &'static str {
    let list = tabs();
    list.get(index.max(0) as usize).map_or("processes", |t| t.id())
}

/// Visibility changed for the view named `id`: notify every tab's poller
/// (visible tabs poll; hidden ones park).
pub fn set_visible(id: &str) {
    for t in all_tabs() {
        t.set_visible(t.id() == id);
    }
}
