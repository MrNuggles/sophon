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
    #[cfg_attr(not(any(feature = "docker", feature = "proxy")), allow(unused_mut))]
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

/// New column order after dragging column `drag` and dropping it at `x`.
/// `centers` holds every other column's centre in the same coordinates as
/// `x` (both in header pixels). Columns that aren't in `centers` (hidden
/// ones) keep their relative position.
pub fn reorder_after_drop(order: &[String], drag: &str, x: f32, centers: &[(String, f32)]) -> Vec<String> {
    let mut out: Vec<String> = order.iter().filter(|c| c.as_str() != drag).cloned().collect();
    // Land immediately after the last surviving column centred left of the
    // drop point (or at the start if none is).
    let pos = match centers.iter().filter(|(c, cx)| c.as_str() != drag && *cx <= x).last() {
        Some((before, _)) => out.iter().position(|c| c == before).map_or(out.len(), |p| p + 1),
        None => 0,
    };
    out.insert(pos.min(out.len()), drag.to_string());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn centers(v: &[(&str, f32)]) -> Vec<(String, f32)> {
        v.iter().map(|(c, x)| (c.to_string(), *x)).collect()
    }

    #[test]
    fn drop_moves_the_column() {
        let order = vec!["a".into(), "b".into(), "c".into()];
        let cs = centers(&[("a", 100.0), ("b", 300.0), ("c", 500.0)]);
        // Drag "c" to the far left: before "a".
        assert_eq!(reorder_after_drop(&order, "c", 0.0, &cs), vec!["c", "a", "b"]);
        // Drag "a" to the far right: after "c".
        assert_eq!(reorder_after_drop(&order, "a", 600.0, &cs), vec!["b", "c", "a"]);
        // Drag "b" left of "a"'s centre.
        assert_eq!(reorder_after_drop(&order, "b", 50.0, &cs), vec!["b", "a", "c"]);
    }

    #[test]
    fn drop_on_its_own_centre_is_a_noop() {
        let order = vec!["a".into(), "b".into(), "c".into()];
        let cs = centers(&[("a", 100.0), ("b", 300.0), ("c", 500.0)]);
        // "b" dropped exactly on "b": target is "c" (b excluded), lands before
        // "c" — i.e. back where it was.
        assert_eq!(reorder_after_drop(&order, "b", 300.0, &cs), vec!["a", "b", "c"]);
        // Between "a" and "b" is also a no-op for "b".
        assert_eq!(reorder_after_drop(&order, "b", 200.0, &cs), vec!["a", "b", "c"]);
    }

    #[test]
    fn hidden_columns_keep_their_relative_position() {
        // "h" is hidden (not in centres); dragging "c" before "a" must not
        // disturb it, and a far-right drop lands after the last visible
        // column ("b"), before the hidden one.
        let order = vec!["a".into(), "h".into(), "b".into(), "c".into()];
        let cs = centers(&[("a", 100.0), ("b", 300.0)]);
        assert_eq!(reorder_after_drop(&order, "c", 0.0, &cs), vec!["c", "a", "h", "b"]);
        assert_eq!(reorder_after_drop(&order, "a", 600.0, &cs), vec!["h", "b", "a", "c"]);
    }

    #[test]
    fn boundaries_are_clamped() {
        let order = vec!["a".into(), "b".into()];
        let cs = centers(&[("a", 100.0), ("b", 300.0)]);
        // Negative x: still lands at the start.
        assert_eq!(reorder_after_drop(&order, "b", -50.0, &cs), vec!["b", "a"]);
        // No centres at all (single column): unchanged.
        assert_eq!(reorder_after_drop(&order, "a", 100.0, &[]), vec!["a", "b"]);
    }
}
