//! The panel (tab) registry — the single source of truth for which tabs exist,
//! their order, and their titles.
//!
//! Adding a feature tab means touching exactly three places:
//!   1. add a `Panel` entry here (feature-gated if optional);
//!   2. install its controller in `main.rs` (one `#[cfg]`'d call);
//!   3. add one `if root.current-id == "<id>": <View> {}` line in `app.slint`.
//!
//! Panels "processes" and "performance" are always indices 0 and 1 (the local
//! sampler drives them); optional panels and "settings" follow. Nothing else in
//! the app hard-codes a tab number — the current panel is resolved by `id`.

/// A tab in the header. `id` matches the `if root.current-id == "<id>"` block
/// in app.slint and is stable across builds (used for settings, etc.).
pub struct Panel {
    pub id: &'static str,
    pub title: &'static str,
}

/// The enabled panels, in display order. Optional panels are compiled in only
/// when their Cargo feature is on, so a build without them has no trace of them.
pub fn panels() -> Vec<Panel> {
    let mut v = vec![
        Panel { id: "processes", title: "Processes" },
        Panel { id: "performance", title: "Performance" },
    ];
    // Compiled in with the Cargo feature, then shown or hidden from
    // Settings → Tabs (nothing is fetched while it's hidden).
    #[cfg(feature = "docker")]
    if crate::settings::get(|s| s.tabs_docker) {
        v.push(Panel { id: "docker", title: "Docker" });
    }
    v.push(Panel { id: "settings", title: "Settings" });
    v
}

/// The panel id at a tab index (used to resolve what to render).
pub fn id_at(index: i32) -> &'static str {
    let list = panels();
    list.get(index.max(0) as usize).map_or("processes", |p| p.id)
}
