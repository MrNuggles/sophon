//! The panel (tab) registry — the single source of truth for which tabs exist,
//! their order, and their titles.
//!
//! Adding a feature tab means touching exactly three places:
//!   1. add a `Panel` entry here (feature-gated if optional);
//!   2. install its controller in `main.rs` (one `#[cfg]`'d call);
//!   3. add one `if root.current-id == "<id>": <View> {}` line in `app.slint`.
//!
//! "processes" is the app's dashboard and is permanent. Every other tab is
//! optional: compiled in with its Cargo feature (docker) and shown or hidden
//! live from Settings → Tabs. Settings itself is not a tab — it opens from
//! the ☰ button beside the theme toggle. Nothing else in the app hard-codes
//! a tab number — the current panel is resolved by `id`.

/// A tab in the header. `id` matches the `if root.current-id == "<id>"` block
/// in app.slint and is stable across builds (used for settings, etc.).
pub struct Panel {
    pub id: &'static str,
    pub title: &'static str,
}

/// The enabled content tabs, in display order. Optional tabs are compiled in
/// only when their Cargo feature is on, and appear only when enabled.
pub fn panels() -> Vec<Panel> {
    let mut v = vec![Panel { id: "processes", title: "Processes" }];
    if crate::settings::get(|s| s.tabs_performance) {
        v.push(Panel { id: "performance", title: "Performance" });
    }
    #[cfg(feature = "docker")]
    if crate::settings::get(|s| s.tabs_docker) {
        v.push(Panel { id: "docker", title: "Docker" });
    }
    v
}

/// The panel id at a tab index (used to resolve what to render).
pub fn id_at(index: i32) -> &'static str {
    let list = panels();
    list.get(index.max(0) as usize).map_or("processes", |p| p.id)
}
