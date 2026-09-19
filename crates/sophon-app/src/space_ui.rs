//! The disk space map: scan lifecycle, treemap layout and actions.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use std::rc::Rc;

use slint::{Color, ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};
use sophon_core::space::{self, CATEGORIES, Mount, Node, NodeKind, Progress, ScanOptions, ScanResult};

use crate::settings;
use crate::{AppWindow, Block, LegendItem, Space, actions, fmt, ui};

/// Upper bound on drawn blocks; beyond this, extra detail isn't visible anyway.
const MAX_BLOCKS: usize = 2500;
/// Height of a folder's label strip when its contents are drawn inside it.
const HEADER: f32 = 18.0;
const PALETTE: [u32; 10] =
    [0x3db4ff, 0xb27bff, 0x2fd39a, 0xffb13d, 0xff6fa5, 0x7c8cff, 0xff7a59, 0x4dd0e1, 0xc0ca33, 0xf06292];

thread_local! {
    static STATE: RefCell<Option<SpaceUi>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut SpaceUi) -> R) -> Option<R> {
    STATE.with_borrow_mut(|s| s.as_mut().map(f))
}

struct SpaceUi {
    app: slint::Weak<AppWindow>,
    disk: Option<String>,
    mounts: Vec<Mount>,
    mount: usize,
    results: HashMap<PathBuf, ScanResult>,
    /// Scans loaded from the cache, with when they were taken.
    cached: HashMap<PathBuf, std::time::SystemTime>,
    scanning: Option<(PathBuf, Arc<Progress>, Instant)>,
    progress_timer: Timer,
    /// Child indices from the scan root to the folder being shown.
    view: Vec<u32>,
    /// Per drawn block: child indices from the scan root.
    refs: Vec<Vec<u32>>,
    size: (f32, f32),
    /// Selected blocks, as child-index paths from the scan root.
    selection: Vec<Vec<u32>>,
    blocks: Rc<VecModel<Block>>,
    pending_trash: Vec<Vec<u32>>,
    base_summary: String,
}

pub fn install(app: &AppWindow) {
    let blocks = Rc::new(VecModel::default());
    app.global::<Space>().set_blocks(ModelRc::from(blocks.clone()));
    STATE.set(Some(SpaceUi {
        app: app.as_weak(),
        disk: None,
        mounts: Vec::new(),
        mount: 0,
        results: HashMap::new(),
        cached: HashMap::new(),
        scanning: None,
        progress_timer: Timer::default(),
        view: Vec::new(),
        refs: Vec::new(),
        size: (0.0, 0.0),
        selection: Vec::new(),
        blocks,
        pending_trash: Vec::new(),
        base_summary: String::new(),
    }));
    let g = app.global::<Space>();
    g.on_scan(|| {
        with(|s| s.scan());
    });
    g.on_cancel(|| {
        with(|s| {
            if let Some((_, p, _)) = &s.scanning {
                p.cancel.store(true, Relaxed);
            }
        });
    });
    g.on_select(|i, add| {
        with(|s| s.select(i, add));
    });
    g.on_select_mount(|i| {
        with(|s| {
            s.mount = i.max(0) as usize;
            s.view.clear();
            s.selection.clear();
            s.refresh();
        });
    });
    g.on_resized(|w, h| {
        with(|s| {
            if (s.size.0 - w).abs() > 0.5 || (s.size.1 - h).abs() > 0.5 {
                s.size = (w, h);
                s.relayout();
            }
        });
    });
    g.on_zoom(|i| {
        with(|s| s.zoom(i as usize));
    });
    g.on_crumb(|i| {
        with(|s| {
            s.view.truncate(i.max(0) as usize);
            s.selection.clear();
            s.relayout();
        });
    });
    g.on_action(|i, action| {
        with(|s| s.action(i, &action));
    });
    g.on_confirm_trash(|| {
        with(|s| s.confirm_trash());
    });
    g.on_cancel_trash(|| {
        with(|s| {
            s.pending_trash.clear();
            s.global(|g| g.set_confirm(SharedString::new()));
        });
    });
}

/// Called whenever the Performance tab renders; `disk` is the selected disk, if any.
pub fn show_disk(disk: Option<&str>) {
    with(|s| {
        if s.disk.as_deref() == disk {
            return;
        }
        s.disk = disk.map(str::to_string);
        s.view.clear();
        let Some(disk) = disk else {
            s.global(|g| g.set_available(false));
            return;
        };
        let all = space::mounts();
        crate::settings_page::set_mounts(all.iter().filter(|m| m.total > 0).map(|m| m.path.display().to_string()).collect());
        s.mounts = all.into_iter().filter(|m| m.disk == disk && m.total > 0).collect();
        // The default drive from settings, else the mount holding the user's home, else the biggest.
        let preferred = settings::get(|p| p.space_default.clone());
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        s.mount = s
            .mounts
            .iter()
            .position(|m| !preferred.is_empty() && m.path == Path::new(&preferred))
            .or_else(|| {
                s.mounts
                    .iter()
                    .enumerate()
                    .filter(|(_, m)| home.starts_with(&m.path))
                    .max_by_key(|(_, m)| m.path.as_os_str().len())
                    .or_else(|| s.mounts.iter().enumerate().max_by_key(|(_, m)| m.total))
                    .map(|(i, _)| i)
            })
            .unwrap_or(0);
        let labels: Vec<SharedString> = s
            .mounts
            .iter()
            .map(|m| format!("{}   {} free", m.path.display(), fmt::bytes_short(m.free)).into())
            .collect();
        let has = !s.mounts.is_empty();
        s.global(|g| {
            g.set_available(has);
            g.set_mounts(ModelRc::new(VecModel::from(labels)));
        });
        s.refresh();
    });
}

/// Scan finished (on the UI thread).
fn done(result: ScanResult) {
    with(|s| {
        s.progress_timer.stop();
        s.scanning = None;
        if result.cancelled {
            ui::toast("Scan cancelled");
        } else {
            s.cached.remove(&result.path);
            s.results.insert(result.path.clone(), result);
            s.view.clear();
            s.selection.clear();
        }
        s.refresh();
    });
}

impl SpaceUi {
    fn global(&self, f: impl FnOnce(Space)) {
        if let Some(app) = self.app.upgrade() {
            f(app.global::<Space>());
        }
    }

    fn current(&self) -> Option<&Mount> {
        self.mounts.get(self.mount)
    }

    /// Syncs everything but the blocks to the current mount's state.
    fn refresh(&mut self) {
        let Some(m) = self.current().cloned() else { return };
        if !self.results.contains_key(&m.path)
            && self.scanning.is_none()
            && settings::get(|p| p.space_remember)
            && let Some((result, when)) = space::load(&cache_file(&m.path))
        {
            self.cached.insert(m.path.clone(), when);
            self.results.insert(m.path.clone(), result);
        }
        let scanning_here = self.scanning.as_ref().is_some_and(|(p, _, _)| *p == m.path);
        let state = if scanning_here {
            1
        } else if self.results.contains_key(&m.path) {
            2
        } else {
            0
        };
        let intro = format!(
            "See what's filling {}: {} of {} used.\nThe scan reads every folder in the background at idle priority, so it won't slow your games. \
It takes from a few seconds to a few minutes.",
            m.path.display(),
            fmt::bytes_short(m.total - m.free),
            fmt::bytes_short(m.total)
        );
        let mount = self.mount as i32;
        self.global(|g| {
            g.set_mount(mount);
            g.set_state(state);
            g.set_intro(intro.into());
        });
        if state == 2 {
            self.relayout();
        }
    }

    fn scan(&mut self) {
        if self.scanning.is_some() {
            return ui::toast("A scan is already running");
        }
        let Some(path) = self.current().map(|m| m.path.clone()) else { return };
        let progress = Arc::new(Progress::default());
        let (p, root) = (progress.clone(), path.clone());
        let prefs = settings::get(|s| s.clone());
        let options = ScanOptions {
            threads: std::thread::available_parallelism().map_or(4, |n| n.get()).min(6),
            idle: prefs.space_priority == 0,
            include_hidden: prefs.space_hidden_files,
            exclude: prefs.space_excluded.iter().map(PathBuf::from).collect(),
        };
        let remember = prefs.space_remember;
        std::thread::Builder::new()
            .name("space-scan".into())
            .spawn(move || {
                let result = space::scan(&root, &p, &options);
                if remember && !result.cancelled {
                    let file = cache_file(&root);
                    if let Some(dir) = file.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    let _ = space::save(&result, &file);
                }
                let _ = slint::invoke_from_event_loop(move || done(result));
            })
            .ok();
        self.scanning = Some((path, progress, Instant::now()));
        self.refresh();
        self.update_progress();
        self.progress_timer.start(TimerMode::Repeated, Duration::from_millis(250), || {
            with(|s| s.update_progress());
        });
    }

    fn update_progress(&self) {
        let Some((path, p, started)) = &self.scanning else { return };
        let text = format!(
            "Scanning {}…   {} files   ·   {}   ·   {:.0} s",
            path.display(),
            fmt::thousands(p.files.load(Relaxed)),
            fmt::bytes_short(p.bytes.load(Relaxed)),
            started.elapsed().as_secs_f32()
        );
        self.global(|g| g.set_progress(text.into()));
    }

    fn result(&self) -> Option<&ScanResult> {
        self.results.get(&self.current()?.path)
    }

    fn relayout(&mut self) {
        let (w, h) = self.size;
        let Some(res) = self.results.get(&self.current().map(|m| m.path.clone()).unwrap_or_default()) else { return };
        if res.root.at(&self.view).is_none() {
            self.view.clear();
        }
        let node = res.root.at(&self.view).expect("root exists");

        // Crumbs: mount path, then each folder zoomed into.
        let mut crumbs: Vec<SharedString> = vec![res.path.display().to_string().into()];
        let mut n = &res.root;
        let mut dir = res.path.clone();
        for &i in &self.view {
            n = &n.children[i as usize];
            crumbs.push(n.name.as_ref().into());
            dir.push(&*n.name);
        }

        let prefs = settings::get(|p| (p.space_color, p.space_min_block));
        let now_secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()) as u32;
        let mut lay = Layout {
            blocks: Vec::new(),
            refs: Vec::new(),
            total: node.size.max(1),
            scope: node.name.to_string(),
            color_mode: prefs.0,
            min_block: prefs.1,
            now: now_secs,
        };
        if w > 10.0 && h > 10.0 {
            lay.place(node, &self.view, &dir, Rect { x: 0.0, y: 0.0, w, h }, 0, None);
        }
        for (b, r) in lay.blocks.iter_mut().zip(&lay.refs) {
            b.selected = self.selection.contains(r);
        }
        let when = match self.cached.get(&res.path).and_then(|t| t.elapsed().ok()) {
            Some(age) => format!("remembered scan from {}", fmt::ago(age.as_secs())),
            None => format!("scanned in {:.1} s", res.elapsed.as_secs_f32()),
        };
        let legend: Vec<LegendItem> = match prefs.0 {
            1 => CATEGORIES.iter().zip(TYPE_COLORS).map(|(l, c)| LegendItem { color: rgb(c), label: (*l).into() }).collect(),
            2 => AGES.iter().map(|(label, _, c)| LegendItem { color: argb(*c, 0.6), label: (*label).into() }).collect(),
            _ => Vec::new(),
        };
        self.global(|g| g.set_legend(ModelRc::new(VecModel::from(legend))));
        let summary = format!(
            "{} in {} files   ·   {}{}   ·   Sizes are space allocated on disk.",
            fmt::bytes_short(node.size),
            fmt::thousands(node.files),
            when,
            if res.denied > 0 { format!("   ·   {} folders couldn't be read", res.denied) } else { String::new() }
        );
        self.refs = lay.refs;
        self.blocks.set_vec(lay.blocks);
        self.base_summary = summary;
        self.global(|g| g.set_crumbs(ModelRc::new(VecModel::from(crumbs))));
        self.update_summary();
    }

    /// Summary line: scan totals, plus the selection when there is one.
    fn update_summary(&self) {
        let count = self.selection.len();
        let text = if count == 0 {
            format!(
                "{}   ·   Click to select, Shift+click to select more, double-click to open a folder, right-click for actions.",
                self.base_summary
            )
        } else {
            let size: u64 = self.selection_roots().iter().filter_map(|p| self.result()?.root.at(p)).map(|n| n.size).sum();
            format!(
                "{count} selected  ({})   ·   Right-click or press Delete to move to the Trash, Escape to clear.   ·   {}",
                fmt::bytes_short(size),
                self.base_summary
            )
        };
        self.global(|g| {
            g.set_summary(text.into());
            g.set_selected_count(count as i32);
        });
    }

    /// Click on block `i`: select it alone, or toggle it with Shift. `i < 0` clears.
    fn select(&mut self, i: i32, add: bool) {
        match usize::try_from(i).ok().and_then(|i| self.refs.get(i)).cloned() {
            None => self.selection.clear(),
            Some(path) if add => {
                if let Some(pos) = self.selection.iter().position(|p| *p == path) {
                    self.selection.remove(pos);
                } else {
                    self.selection.push(path);
                }
            }
            Some(path) => self.selection = vec![path],
        }
        // Update only the blocks whose state changed.
        for (row, r) in self.refs.iter().enumerate() {
            let selected = self.selection.contains(r);
            if let Some(mut b) = self.blocks.row_data(row)
                && b.selected != selected
            {
                b.selected = selected;
                self.blocks.set_row_data(row, b);
            }
        }
        self.update_summary();
    }

    /// The selection without items inside other selected folders (trashing the folder covers them).
    fn selection_roots(&self) -> Vec<Vec<u32>> {
        self.selection
            .iter()
            .filter(|p| !self.selection.iter().any(|q| q.len() < p.len() && p.starts_with(q)))
            .cloned()
            .collect()
    }

    fn node(&self, i: i32) -> Option<(&Node, &[u32], PathBuf)> {
        let res = self.result()?;
        let path = self.refs.get(usize::try_from(i).ok()?)?;
        let node = res.root.at(path)?;
        Some((node, path, full_path(res, path)))
    }

    fn zoom(&mut self, i: usize) {
        let Some((node, path, _)) = self.node(i as i32) else { return };
        if node.kind == NodeKind::Dir && !node.children.is_empty() {
            self.view = path.to_vec();
            self.selection.clear();
            self.relayout();
        }
    }

    fn action(&mut self, i: i32, action: &str) {
        // Trash acts on the whole selection when invoked on a selected block (or from the keyboard).
        if action == "trash" {
            let on_selection = i < 0
                || usize::try_from(i).ok().and_then(|i| self.refs.get(i)).is_some_and(|r| self.selection.contains(r));
            let items = if on_selection {
                self.selection_roots()
            } else {
                usize::try_from(i).ok().and_then(|i| self.refs.get(i)).cloned().into_iter().collect()
            };
            return self.ask_trash(items);
        }
        let Some((node, _, path)) = self.node(i) else { return };
        let summary = matches!(node.kind, NodeKind::SmallFiles | NodeKind::Unaccounted);
        // Summary blocks aren't real paths: act on the folder that contains them.
        let target = if summary { path.parent().map(Path::to_path_buf).unwrap_or(path) } else { path };
        match action {
            "open" => {
                std::thread::spawn(move || {
                    let _ = std::process::Command::new("xdg-open").arg(target).spawn();
                });
            }
            "reveal" => {
                let t = target.to_string_lossy().into_owned();
                std::thread::spawn(move || actions::open_location(&t));
            }
            "zoom" => self.zoom(i as usize),
            _ => {}
        }
    }

    /// Validates the items and asks for confirmation.
    fn ask_trash(&mut self, items: Vec<Vec<u32>>) {
        let Some(res) = self.result() else { return };
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        let trash = home.join(".local/share/Trash");
        let mut ok = Vec::new();
        let (mut skipped_summary, mut refused, mut in_trash) = (0, 0, 0);
        for idx in items {
            let Some(node) = res.root.at(&idx) else { continue };
            let path = full_path(res, &idx);
            if matches!(node.kind, NodeKind::SmallFiles | NodeKind::Unaccounted) {
                skipped_summary += 1;
            } else if path == home || self.mounts.iter().any(|m| m.path == path) {
                refused += 1;
            } else if path.starts_with(&trash) {
                in_trash += 1;
            } else {
                ok.push((idx, path, node.size, node.kind));
            }
        }
        if ok.is_empty() {
            return ui::toast(if in_trash > 0 {
                "That's already in the Trash. Empty the Trash from your file manager to free the space."
            } else if refused > 0 {
                "Sophon won't trash your home folder or a whole drive"
            } else {
                "Those blocks group many small items. Zoom in to choose real files or folders."
            });
        }
        let total: u64 = ok.iter().map(|(_, _, size, _)| size).sum();
        let text = if ok.len() == 1 {
            let (_, path, size, kind) = &ok[0];
            let what = if *kind == NodeKind::Dir { "folder" } else { "file" };
            format!("Move this {what} to the Trash?\n\n{}\n{}", path.display(), fmt::bytes_short(*size))
        } else {
            let mut list: Vec<String> = ok.iter().take(6).map(|(_, p, size, _)| format!("•  {}   {}", p.display(), fmt::bytes_short(*size))).collect();
            if ok.len() > 6 {
                list.push(format!("…and {} more", ok.len() - 6));
            }
            format!("Move {} items ({}) to the Trash?\n\n{}", ok.len(), fmt::bytes_short(total), list.join("\n"))
        };
        let mut note = String::from("\n\nYou can restore them from the Trash until you empty it.");
        if skipped_summary + refused + in_trash > 0 {
            note.push_str(&format!(
                "\n{} selected block(s) will be skipped (already in the Trash, summaries, your home folder or a drive).",
                skipped_summary + refused + in_trash
            ));
        }
        self.pending_trash = ok.into_iter().map(|(idx, ..)| idx).collect();
        self.global(|g| g.set_confirm(format!("{text}{note}").into()));
    }

    fn confirm_trash(&mut self) {
        self.global(|g| g.set_confirm(SharedString::new()));
        let items = std::mem::take(&mut self.pending_trash);
        let Some(res) = self.result() else { return };
        let jobs: Vec<(Vec<u32>, PathBuf)> = items.into_iter().map(|idx| (full_path(res, &idx), idx)).map(|(p, i)| (i, p)).collect();
        let mount = res.path.clone();
        std::thread::spawn(move || {
            let outcomes: Vec<(Vec<u32>, PathBuf, Result<(), String>)> = jobs
                .into_iter()
                .map(|(idx, path)| {
                    let out = std::process::Command::new("gio").arg("trash").arg("--").arg(&path).output();
                    let r = match out {
                        Ok(o) if o.status.success() => Ok(()),
                        Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
                        Err(e) => Err(e.to_string()),
                    };
                    (idx, path, r)
                })
                .collect();
            let _ = slint::invoke_from_event_loop(move || trashed(mount, outcomes));
        });
    }
}

/// Trash finished: drop the items from the tree so the map updates without a rescan.
fn trashed(mount: PathBuf, mut outcomes: Vec<(Vec<u32>, PathBuf, Result<(), String>)>) {
    with(|s| {
        let Some(res) = s.results.get_mut(&mount) else { return };
        // Remove deepest/last paths first so earlier index paths stay valid.
        outcomes.sort_by(|a, b| b.0.cmp(&a.0));
        let (mut moved, mut bytes) = (0usize, 0u64);
        let mut errors = Vec::new();
        for (idx, path, outcome) in &outcomes {
            match outcome {
                Ok(()) => {
                    // Only remove if the tree still points at the same item.
                    if full_path(res, idx) == *path
                        && let Some(removed) = res.root.remove(idx)
                    {
                        moved += 1;
                        bytes += removed.size;
                    }
                    if s.view.starts_with(idx) {
                        s.view.truncate(idx.len() - 1);
                    }
                }
                Err(e) => errors.push(format!("{}: {e}", path.display())),
            }
        }
        s.selection.clear();
        s.relayout();
        let name = |n: usize| if n == 1 { "item" } else { "items" };
        match (moved, errors.len()) {
            (m, 0) => ui::toast(format!("Moved {m} {} to the Trash ({})", name(m), fmt::bytes_short(bytes))),
            (0, _) => ui::toast(format!("Couldn't move to the Trash: {}", errors[0])),
            (m, e) => ui::toast(format!("Moved {m} {} to the Trash ({}); {e} failed: {}", name(m), fmt::bytes_short(bytes), errors[0])),
        }
    });
}

fn full_path(res: &ScanResult, idx: &[u32]) -> PathBuf {
    let mut p = res.path.clone();
    let mut n = &res.root;
    for &i in idx {
        match n.children.get(i as usize) {
            Some(c) => {
                n = c;
                p.push(&*c.name);
            }
            None => break,
        }
    }
    p
}

// ---- Treemap layout ----------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
struct Rect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

struct Layout {
    blocks: Vec<Block>,
    refs: Vec<Vec<u32>>,
    total: u64,
    scope: String,
    /// 0 by folder, 1 by file type, 2 by age.
    color_mode: usize,
    min_block: f32,
    now: u32,
}

/// Colours for [`CATEGORIES`].
const TYPE_COLORS: [u32; 7] = [0xff6fa5, 0xb27bff, 0x2fd39a, 0xffb13d, 0x3db4ff, 0xff7a59, 0x94a3b8];
/// Age buckets: (label, max age in seconds, colour).
const AGES: [(&str, u32, u32); 5] = [
    ("Today", 86_400, 0x2fd39a),
    ("This week", 7 * 86_400, 0x3db4ff),
    ("This month", 31 * 86_400, 0x8f7dff),
    ("This year", 365 * 86_400, 0xffb13d),
    ("Older", u32::MAX, 0x64748b),
];

fn rgb(c: u32) -> Color {
    Color::from_rgb_u8((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

impl Layout {
    fn place(&mut self, node: &Node, path: &[u32], dir: &Path, r: Rect, depth: u32, hue: Option<u32>) {
        let sizes: Vec<f64> = node.children.iter().map(|c| c.size as f64).collect();
        for (i, (c, cr)) in node.children.iter().zip(squarify(&sizes, r)).enumerate() {
            if self.blocks.len() >= MAX_BLOCKS {
                return;
            }
            if cr.w < self.min_block || cr.h < self.min_block {
                continue;
            }
            let hue = hue.unwrap_or(PALETTE[i % PALETTE.len()]);
            let color = match (c.kind, self.color_mode) {
                (NodeKind::Unreadable, _) => argb(0xff6b6b, 0.25),
                (NodeKind::Unaccounted, _) => argb(0x5a6274, 0.16),
                (kind, 1) => {
                    // Folders are drawn faintly so nested files show through.
                    let a = match kind {
                        NodeKind::File => 0.6,
                        NodeKind::SmallFiles => 0.35,
                        _ => 0.18,
                    };
                    argb(TYPE_COLORS[c.dominant()], a)
                }
                (kind, 2) => {
                    let age = self.now.saturating_sub(c.newest);
                    let col = AGES.iter().find(|a| age <= a.1).map_or(0x64748b, |a| a.2);
                    argb(col, if kind == NodeKind::Dir { 0.18 } else { 0.55 })
                }
                (NodeKind::Dir, _) => argb(hue, 0.24),
                (NodeKind::File, _) => argb(hue, 0.50),
                (NodeKind::SmallFiles, _) => argb(0x8d95a6, 0.20),
            };
            let nested = c.kind == NodeKind::Dir && !c.children.is_empty() && cr.w > 56.0 && cr.h > 42.0;
            let mut child_path = path.to_vec();
            child_path.push(i as u32);
            let child_dir = dir.join(&*c.name);
            let shown = if matches!(c.kind, NodeKind::SmallFiles | NodeKind::Unaccounted) { dir } else { &child_dir };
            let share = c.size as f64 / self.total as f64 * 100.0;
            let tip = match c.kind {
                NodeKind::Dir => format!(
                    "{}\n{}   ·   {} files   ·   {share:.1}% of {}",
                    shown.display(),
                    fmt::bytes_short(c.size),
                    fmt::thousands(c.files),
                    self.scope
                ),
                NodeKind::Unaccounted => format!("{}\n{}", c.name, fmt::bytes_short(c.size)),
                _ => format!("{}{}\n{}   ·   {share:.1}% of {}", shown.display(), if c.kind == NodeKind::SmallFiles { format!("  ({})", c.name) } else { String::new() }, fmt::bytes_short(c.size), self.scope),
            };
            self.blocks.push(Block {
                x: cr.x,
                y: cr.y,
                w: cr.w,
                h: cr.h,
                color,
                label: c.name.as_ref().into(),
                detail: fmt::bytes_short(c.size).into(),
                tip: tip.into(),
                header: nested,
                selected: false,
            });
            self.refs.push(child_path.clone());
            if nested {
                let inner = Rect { x: cr.x + 3.0, y: cr.y + HEADER, w: cr.w - 6.0, h: cr.h - HEADER - 3.0 };
                self.place(c, &child_path, &child_dir, inner, depth + 1, Some(hue));
            }
        }
    }
}

fn argb(rgb: u32, alpha: f32) -> Color {
    Color::from_argb_u8((alpha * 255.0) as u8, (rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

/// Squarified treemap (Bruls, Huizing, van Wijk): lays `sizes` (largest first)
/// into `r`, keeping blocks as close to square as possible.
fn squarify(sizes: &[f64], r: Rect) -> Vec<Rect> {
    let total: f64 = sizes.iter().sum();
    let mut out = Vec::with_capacity(sizes.len());
    if total <= 0.0 || r.w <= 0.0 || r.h <= 0.0 {
        return out;
    }
    let scale = (r.w as f64 * r.h as f64) / total;
    let areas: Vec<f64> = sizes.iter().map(|s| s * scale).collect();
    let mut rect = r;
    let mut i = 0;
    while i < areas.len() {
        let short = rect.w.min(rect.h) as f64;
        let mut j = i;
        let mut row = 0.0;
        let mut best = f64::INFINITY;
        while j < areas.len() {
            let sum = row + areas[j];
            let worst = worst_ratio(&areas[i..=j], sum, short);
            if worst > best {
                break;
            }
            best = worst;
            row = sum;
            j += 1;
        }
        if rect.w >= rect.h {
            // Row becomes a column on the left.
            let cw = (row / rect.h as f64) as f32;
            let mut y = rect.y;
            for &a in &areas[i..j] {
                let h = (a / cw as f64) as f32;
                out.push(Rect { x: rect.x, y, w: cw, h });
                y += h;
            }
            rect.x += cw;
            rect.w -= cw;
        } else {
            let rh = (row / rect.w as f64) as f32;
            let mut x = rect.x;
            for &a in &areas[i..j] {
                let w = (a / rh as f64) as f32;
                out.push(Rect { x, y: rect.y, w, h: rh });
                x += w;
            }
            rect.y += rh;
            rect.h -= rh;
        }
        i = j;
    }
    out
}

fn worst_ratio(row: &[f64], sum: f64, short: f64) -> f64 {
    let (min, max) = row.iter().fold((f64::INFINITY, 0f64), |(lo, hi), &a| (lo.min(a), hi.max(a)));
    let s2 = short * short;
    let sum2 = sum * sum;
    (s2 * max / sum2).max(sum2 / (s2 * min))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn squarify_fills_the_rect() {
        let r = Rect { x: 0.0, y: 0.0, w: 600.0, h: 400.0 };
        let sizes = [6.0, 6.0, 4.0, 3.0, 2.0, 2.0, 1.0];
        let rects = squarify(&sizes, r);
        assert_eq!(rects.len(), sizes.len());
        let area: f32 = rects.iter().map(|q| q.w * q.h).sum();
        assert!((area - 600.0 * 400.0).abs() < 1.0);
        let total: f64 = sizes.iter().sum();
        for (q, s) in rects.iter().zip(sizes) {
            let expected = s / total * 240_000.0;
            assert!(((q.w * q.h) as f64 - expected).abs() < 1.0, "area proportional to size");
            assert!(q.x >= -0.01 && q.y >= -0.01 && q.x + q.w <= 600.01 && q.y + q.h <= 400.01, "inside");
        }
    }
}

/// Settings changed: redraw the map with the new look.
pub fn settings_changed() {
    with(|s| s.relayout());
}

/// Where a mount's last scan is remembered.
fn cache_file(mount: &Path) -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache"));
    let name: String = mount.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    base.join("sophon/space").join(format!("{name}.bin"))
}
