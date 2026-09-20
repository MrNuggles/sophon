//! The Docker tab (feature "docker"): a background poller that lists containers
//! on each configured server and lets you sort them, expand them for live
//! CPU/RAM graphs and details, and start / stop / restart them.
//!
//! All network work runs on a dedicated thread; results are delivered to the UI
//! thread via `invoke_from_event_loop`. `list` is cheap and polled for every
//! server; the costly per-container `stats`/`inspect` calls are made only for
//! *expanded* containers (plus all running ones while sorting by CPU/RAM), so
//! the background cost stays bounded — the same discipline as the local sampler.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering::Relaxed};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use slint::{Color, ComponentHandle, Model, ModelRc, VecModel};
use sophon_core::history::Ring;
use sophon_docker::{Container, Details, Server, ServerState, Stats};
#[cfg(feature = "proxy")]
use sophon_npm::http;

use crate::graph::{self, nice_max};
use crate::{AppWindow, DockerHead, DockerKV, DockerPage, DockerRow, fmt, settings};

const POLL_VISIBLE: Duration = Duration::from_secs(2);
const POLL_HIDDEN: Duration = Duration::from_secs(15);

/// Table layout: right gutter and gap between columns.
const GUTTER: f32 = 14.0;
const GAP: f32 = 8.0;
/// (id, label, default width) per column, in canonical sort order (settings'
/// DOCKER_COLS). The first displayed column stretches over the leftover width.
#[cfg(feature = "proxy")]
const COLS: [(&str, &str, f32); 7] = [
    ("ports", "Ports", 190.0),
    ("name", "Name", 240.0),
    ("state", "State", 92.0),
    ("age", "Age", 160.0),
    ("cpu", "CPU", 64.0),
    ("ram", "RAM", 92.0),
    ("proxied", "Proxied", 70.0),
];
#[cfg(not(feature = "proxy"))]
const COLS: [(&str, &str, f32); 6] = [
    ("ports", "Ports", 190.0),
    ("name", "Name", 240.0),
    ("state", "State", 92.0),
    ("age", "Age", 160.0),
    ("cpu", "CPU", 64.0),
    ("ram", "RAM", 92.0),
];

/// (label, width) per column id, honouring saved resizes.
fn col_spec(id: &str, widths: &[(String, u32)]) -> (&'static str, f32) {
    let saved = widths.iter().find(|(c, _)| c == id).map(|(_, w)| *w as f32);
    COLS.iter().find(|(i, ..)| *i == id).map_or(("?", 100.0), |(_, l, w)| (*l, saved.unwrap_or(*w)))
}

fn sort_index(id: &str) -> i32 {
    settings::DOCKER_COLS.iter().position(|c| *c == id).unwrap_or(0) as i32
}

/// Display order with each column's geometry: (id, sort id, label, r, width,
/// flex) — r is the distance from the table's right edge to the column's left
/// side; the flex column's r is to its right boundary.
fn layout() -> Vec<(String, i32, &'static str, f32, f32, bool)> {
    let order: Vec<String> = settings::get(|s| s.docker_cols.clone());
    let widths: Vec<(String, u32)> = settings::get(|s| s.docker_col_widths.clone());
    let mut r = GUTTER;
    let mut out = Vec::new();
    for (idx, id) in order.iter().enumerate().rev() {
        let (label, w) = col_spec(id, &widths);
        if idx == 0 {
            out.push((id.clone(), sort_index(id), label, r, w, true));
        } else {
            r += w;
            out.push((id.clone(), sort_index(id), label, r, w, false));
            r += GAP;
        }
    }
    out.reverse();
    out
}

fn render_heads(d: &DockerUi) {
    let heads: Vec<DockerHead> = layout()
        .into_iter()
        .map(|(id, key, label, r, w, flex)| DockerHead { id: id.into(), key, label: label.into(), r, w, flex })
        .collect();
    update_model(&d.heads, heads);
}

/// Sort keys, matching the chips in docker.slint.
pub const SORTS: [&str; 6] = ["Ports", "Name", "State", "Age", "CPU", "RAM"];

/// Per-container CPU%/memory history for the expanded graphs (short: ~1 min).
#[derive(Default)]
struct Hist {
    cpu: Ring<60>,
    mem: Ring<60>,
    limit: u64,
}

/// Shared with the poller thread.
struct Net {
    servers: Mutex<Vec<Server>>,
    /// Containers whose stats/inspect the poller should fetch (expanded rows).
    expanded: Mutex<HashSet<String>>,
    /// Servers (host:port) whose header is collapsed; their containers' stats
    /// are skipped — nothing hidden is polled.
    collapsed: Mutex<HashSet<String>>,
    /// The tab is enabled in Settings → Tabs; false parks the poller entirely.
    enabled: AtomicBool,
    /// The NPM connection (feature "proxy"), for the Proxied column. Set at
    /// install and refreshed from settings on every UI render.
    #[cfg(feature = "proxy")]
    npm: Mutex<Option<NpmCreds>>,
    sort: AtomicU8,
    visible: AtomicBool,
    wake: mpsc::Sender<()>,
}

/// The NPM login, shared with the poller thread.
#[cfg(feature = "proxy")]
#[derive(Clone)]
struct NpmCreds {
    server: sophon_npm::Server,
    email: String,
    password: String,
}

#[cfg(feature = "proxy")]
impl NpmCreds {
    fn from_settings() -> Option<NpmCreds> {
        let (url, email, password) =
            settings::get(|s| (s.npm_url.clone(), s.npm_email.clone(), s.npm_password.clone()));
        if url.is_empty() || email.is_empty() || password.is_empty() {
            return None;
        }
        Some(NpmCreds { server: sophon_npm::Server::parse("", &url)?, email, password })
    }
}

/// One NPM reverse-proxy target: (forward host, forward port, domain).
#[cfg(feature = "proxy")]
type NpmTarget = (String, u16, String);

/// One poller delivery.
struct Poll {
    states: Vec<ServerState>,
    stats: HashMap<String, Stats>,
    details: HashMap<String, Details>,
}

struct DockerUi {
    app: slint::Weak<AppWindow>,
    net: Arc<Net>,
    states: Vec<ServerState>,
    server_count: usize,
    expanded: HashSet<String>,
    /// Server addresses (host:port) whose containers are hidden.
    collapsed: HashSet<String>,
    sort: usize,
    sort_desc: bool,
    hist: HashMap<String, Hist>,
    latest: HashMap<String, Stats>,
    details: HashMap<String, Details>,
    /// A stop awaiting the shared confirmation dialog: (container id, "stop").
    pending: Option<(String, String)>,
    rows: std::rc::Rc<VecModel<DockerRow>>,
    heads: std::rc::Rc<VecModel<DockerHead>>,
    /// Flattened NPM targets for the Proxied column (feature "proxy").
    #[cfg(feature = "proxy")]
    npm: Vec<NpmTarget>,
}

thread_local! {
    static STATE: RefCell<Option<DockerUi>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut DockerUi) -> R) -> Option<R> {
    STATE.with_borrow_mut(|s| s.as_mut().map(f))
}

fn load_servers() -> Vec<Server> {
    settings::get(|s| s.docker_servers.clone())
        .iter()
        .filter_map(|(name, addr)| Server::parse(name, addr))
        .collect()
}

pub fn install(app: &AppWindow) {
    let (wake, rx) = mpsc::channel();
    let servers = load_servers();
    let (sort, sort_desc) = settings::get(|s| (s.docker_sort.min(SORTS.len() - 1), s.docker_sort_desc));
    let net = Arc::new(Net {
        servers: Mutex::new(servers.clone()),
        expanded: Mutex::new(HashSet::new()),
        collapsed: Mutex::new(HashSet::new()),
        enabled: AtomicBool::new(settings::get(|s| s.tabs_docker)),
        #[cfg(feature = "proxy")]
        npm: Mutex::new(NpmCreds::from_settings()),
        sort: AtomicU8::new(sort as u8),
        visible: AtomicBool::new(false),
        wake,
    });
    let rows = std::rc::Rc::new(VecModel::default());
    let heads = std::rc::Rc::new(VecModel::default());
    let g = app.global::<DockerPage>();
    g.set_rows(ModelRc::from(rows.clone()));
    g.set_heads(ModelRc::from(heads.clone()));
    g.set_sort(sort as i32);
    g.set_sort_desc(sort_desc);

    STATE.set(Some(DockerUi {
        app: app.as_weak(),
        net: net.clone(),
        states: Vec::new(),
        server_count: servers.len(),
        expanded: HashSet::new(),
        collapsed: HashSet::new(),
        sort,
        sort_desc,
        hist: HashMap::new(),
        latest: HashMap::new(),
        details: HashMap::new(),
        pending: None,
        rows,
        heads,
        #[cfg(feature = "proxy")]
        npm: Vec::new(),
    }));

    g.on_action(|id, op| {
        with(|d| d.container_action(&id, &op));
    });
    g.on_server_op(|idx, op| {
        with(|d| d.server_op(idx as usize, &op));
    });
    g.on_toggle_server(|idx| {
        with(|d| d.toggle_server(idx.max(0) as usize));
    });
    g.on_rename_server(|idx, name| {
        with(|d| d.rename_server(idx.max(0) as usize, &name));
    });
    g.on_add_server(|name, addr| {
        with(|d| d.add_server(&name, &addr));
    });
    g.on_toggle(|id| {
        with(|d| d.toggle(&id));
    });
    g.on_sort_by(|key| {
        with(|d| d.set_sort(key.max(0) as usize));
    });
    g.on_column_dropped(|id, x, w| {
        with(|d| d.column_dropped(id.to_string(), x, w));
    });
    g.on_columns_reset(|| {
        with(|d| {
            settings::update(|s| {
                s.docker_cols = settings::DOCKER_COLS_DEFAULT.iter().map(|c| c.to_string()).collect();
                s.docker_col_widths.clear();
            });
            render_rows(d);
        });
    });
    g.on_column_resized(|id, w| {
        with(|d| d.column_resized(&id, w));
    });

    std::thread::Builder::new().name("docker".into()).spawn(move || poller(net, rx)).ok();
    render();
}

pub fn set_visible(visible: bool) {
    with(|d| {
        d.net.visible.store(visible, Relaxed);
        if visible {
            let _ = d.net.wake.send(());
        }
    });
}

/// Runs the stop the confirmation dialog was holding, if any.
pub fn confirm_accepted() {
    with(|d| {
        if let Some((id, op)) = d.pending.take() {
            d.run_action(&id, &op);
        }
    });
}

/// Drops a pending stop confirmation without acting.
pub fn cancel_confirm() {
    with(|d| d.pending = None);
}

/// Enables or disables the whole tab (Settings → Tabs). Disabled, the poller
/// parks and every bit of its data is dropped.
pub fn set_enabled(on: bool) {
    with(|d| {
        if d.net.enabled.swap(on, Relaxed) == on {
            return;
        }
        if !on {
            d.expanded.clear();
            d.collapsed.clear();
            d.hist.clear();
            d.latest.clear();
            d.details.clear();
            d.states.clear();
            d.net.expanded.lock().unwrap().clear();
            d.net.collapsed.lock().unwrap().clear();
        }
        let _ = d.net.wake.send(());
        render_rows(d);
    });
}

fn apply(poll: Poll) {
    with(|d| {
        if !d.net.enabled.load(Relaxed) {
            return; // the tab was disabled mid-flight; drop the stale poll
        }
        d.states = poll.states;
        // Latest stats (for sorting) and expanded history (for graphs).
        for (id, s) in &poll.stats {
            d.latest.insert(id.clone(), *s);
            if d.expanded.contains(id) {
                let h = d.hist.entry(id.clone()).or_default();
                h.cpu.push(s.cpu_pct);
                h.mem.push(s.mem_used as f32);
                h.limit = s.mem_limit;
            }
        }
        d.details.extend(poll.details);
        // Forget containers that no longer exist.
        let live: HashSet<&str> = d.states.iter().flat_map(|s| &s.containers).map(|c| c.id.as_str()).collect();
        d.hist.retain(|id, _| live.contains(id.as_str()));
        d.latest.retain(|id, _| live.contains(id.as_str()));
        d.details.retain(|id, _| live.contains(id.as_str()));
        d.sync_expanded();
        render_rows(d);
    });
}

/// Logs in (once) and flattens the NPM proxy hosts into reverse-proxy
/// targets: (forward host, forward port, domain). A forward host of
/// localhost resolves to the NPM machine itself.
#[cfg(feature = "proxy")]
fn npm_fetch(creds: &NpmCreds, token: &mut Option<String>) -> Result<Vec<NpmTarget>, http::Error> {
    let token = match token {
        Some(t) => t.clone(),
        None => {
            let t = sophon_npm::login(&creds.server, &creds.email, &creds.password)?;
            *token = Some(t.clone());
            t
        }
    };
    let hosts = sophon_npm::proxy_hosts(&creds.server, &token)?;
    let mut out = Vec::new();
    for h in &hosts {
        let local = matches!(h.forward_host.as_str(), "127.0.0.1" | "localhost" | "::1");
        let host = if local { creds.server.host.clone() } else { h.forward_host.clone() };
        for d in &h.domains {
            out.push((host.clone(), h.forward_port, d.clone()));
        }
    }
    Ok(out)
}

/// Receives the flattened NPM targets from the poller thread.
#[cfg(feature = "proxy")]
fn apply_npm(targets: Vec<NpmTarget>) {
    with(|d| {
        d.npm = targets;
        render_rows(d);
    });
}

/// The NPM domains that reverse-proxy this container, with their forward
/// ports: a target matches when it points at the container's own server and
/// one of the container's published ports (localhost targets were already
/// resolved to the NPM host).
#[cfg(feature = "proxy")]
fn proxied_matches(d: &DockerUi, server_host: &str, c: &Container) -> Vec<(String, u16)> {
    if d.npm.is_empty() {
        return Vec::new();
    }
    d.npm
        .iter()
        .filter_map(|(h, port, dom)| {
            (h == server_host && c.ports.iter().any(|p| p.public == *port)).then(|| (dom.clone(), *port))
        })
        .collect()
}

#[cfg(not(feature = "proxy"))]
fn proxied_matches(_d: &DockerUi, _server_host: &str, _c: &Container) -> Vec<(String, u16)> {
    Vec::new()
}

fn poller(net: Arc<Net>, wake: mpsc::Receiver<()>) {
    let mut tick: usize = 0;
    #[cfg(feature = "proxy")]
    let mut npm_token: Option<String> = None;
    #[cfg(feature = "proxy")]
    let mut npm_ok = false;
    loop {
        // Settings → Tabs off: the whole tab is dormant — nothing is fetched
        // until it's turned back on.
        if !net.enabled.load(Relaxed) {
            if wake.recv().is_err() {
                return;
            }
            continue;
        }
        let servers = net.servers.lock().unwrap().clone();
        let visible = net.visible.load(Relaxed);
        let expanded = if visible { net.expanded.lock().unwrap().clone() } else { HashSet::new() };
        let collapsed = net.collapsed.lock().unwrap().clone();
        // Fetch all running containers' stats only to sort by usage, and only
        // while the tab is visible — otherwise `list` alone keeps state fresh.
        let sort_usage = visible && matches!(net.sort.load(Relaxed) as usize, 4 | 5);
        let mut stats = HashMap::new();
        let mut details = HashMap::new();
        let mut states: Vec<ServerState> = Vec::new();
        for srv in &servers {
            // A collapsed header hides its containers; skip their stats.
            let srv_collapsed = collapsed.contains(&format!("{}:{}", srv.host, srv.port));
            match sophon_docker::list(srv) {
                Ok(cs) => {
                    // The CPU/RAM columns: a rotating window of ~6 stat
                    // requests per poll sweeps every running container over
                    // several polls (one-shot stats cost a request each).
                    let step = (cs.len() + 5) / 6;
                    for (i, c) in cs.iter().enumerate() {
                        if !c.running() {
                            continue;
                        }
                        let in_window = !srv_collapsed && (i + tick) % step.max(1) == 0;
                        let want_stats = expanded.contains(&c.id) || sort_usage || in_window;
                        if want_stats && let Ok(s) = sophon_docker::stats(srv, &c.id) {
                            stats.insert(c.id.clone(), s);
                        }
                        if expanded.contains(&c.id) && let Ok(dt) = sophon_docker::inspect(srv, &c.id) {
                            details.insert(c.id.clone(), dt);
                        }
                    }
                    states.push(ServerState { server: srv.clone(), error: None, containers: cs });
                }
                Err(e) => states.push(ServerState { server: srv.clone(), error: Some(e.to_string()), containers: Vec::new() }),
            }
        }
        // The Proxied column (feature "proxy"): refresh the NPM host list
        // while the tab is visible — immediately, then every ~30 s (NPM hosts
        // change rarely). Errors stay silent; the Proxy tab reports those.
        #[cfg(feature = "proxy")]
        if net.visible.load(Relaxed) && (!npm_ok || tick % 15 == 0) {
            let creds = net.npm.lock().unwrap().clone();
            if let Some(creds) = creds {
                match npm_fetch(&creds, &mut npm_token) {
                    Ok(flat) => {
                        npm_ok = true;
                        if slint::invoke_from_event_loop(move || apply_npm(flat)).is_err() {
                            return;
                        }
                    }
                    Err(_) => npm_ok = false,
                }
            }
        }
        tick = tick.wrapping_add(1);
        if slint::invoke_from_event_loop(move || apply(Poll { states, stats, details })).is_err() {
            return;
        }
        let interval = if net.visible.load(Relaxed) { POLL_VISIBLE } else { POLL_HIDDEN };
        let deadline = Instant::now() + interval;
        match wake.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(()) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        while wake.try_recv().is_ok() {}
    }
}

impl DockerUi {
    fn find_server(&self, id: &str) -> Option<Server> {
        self.states.iter().find(|s| s.containers.iter().any(|c| c.id == id)).map(|s| s.server.clone())
    }

    fn container_action(&mut self, id: &str, op: &str) {
        // Stopping is destructive, so it asks first (start/restart stay one
        // click, matching every Docker UI).
        if op == "stop" {
            return self.confirm_stop(id);
        }
        self.run_action(id, op);
    }

    /// Shows the shared confirmation dialog for stopping a container.
    fn confirm_stop(&mut self, id: &str) {
        let Some(name) = self.states.iter().flat_map(|s| &s.containers).find(|c| c.id == id).map(|c| c.name.clone()) else { return };
        // Only one confirm may be armed at a time; opening ours retires any
        // pending Processes action (ui.rs does the same for us).
        crate::ui::cancel_confirm();
        self.pending = Some((id.to_string(), "stop".into()));
        if let Some(app) = self.app.upgrade() {
            app.set_confirm_button("Stop container".into());
            app.set_confirm_text(
                format!("Stop {name}?\n\nDocker will ask it to close and force it off after 10 seconds. Nothing is deleted — you can start it again any time.").into(),
            );
        }
    }

    fn run_action(&self, id: &str, op: &str) {
        let Some(server) = self.find_server(id) else { return };
        let name = self.states.iter().flat_map(|s| &s.containers).find(|c| c.id == id).map(|c| c.name.clone()).unwrap_or_default();
        let (id, op) = (id.to_string(), op.to_string());
        let net = self.net.clone();
        crate::ui::toast(format!("{}ing {name}…", op_verb(&op)));
        std::thread::spawn(move || {
            let msg = match sophon_docker::action(&server, &id, &op) {
                Ok(()) => format!("{name} {}", op_done(&op)),
                Err(e) => format!("Couldn't {op} {name}: {e}"),
            };
            let _ = slint::invoke_from_event_loop(move || {
                crate::ui::toast(msg);
                let _ = net.wake.send(());
            });
        });
    }

    fn toggle(&mut self, id: &str) {
        if !self.expanded.remove(id) {
            self.expanded.insert(id.to_string());
        }
        self.sync_expanded();
        let _ = self.net.wake.send(()); // fetch its stats/details promptly
        render_rows(self);
    }

    /// Shows or hides one server's containers (its header was clicked).
    fn toggle_server(&mut self, idx: usize) {
        let Some(st) = self.states.get(idx) else { return };
        if !self.collapsed.remove(&server_key(&st.server)) {
            self.collapsed.insert(server_key(&st.server));
        }
        self.sync_expanded();
        render_rows(self);
    }

    /// Renames a server (right-click its header → Rename). The config entry is
    /// matched by address, the stable part; an empty name falls back to the
    /// address, like adding a server without one.
    fn rename_server(&mut self, idx: usize, name: &str) {
        let Some(st) = self.states.get(idx) else { return };
        let name = name.trim();
        let name = if name.is_empty() { format!("{}:{}", st.server.host, st.server.port) } else { name.to_string() };
        let addr = server_key(&st.server);
        settings::update(|s| {
            if let Some((n, _)) = s.docker_servers.iter_mut().find(|(_, a)| *a == addr) {
                *n = name.clone();
            }
        });
        // Show it immediately; the next poll carries it everywhere else.
        if let Some(st) = self.states.get_mut(idx) {
            st.server.name = name;
        }
        self.reload_servers();
    }

    /// Publishes what the poller should fetch: details for expanded rows, minus
    /// anything under a collapsed server header — nothing hidden is polled.
    fn sync_expanded(&mut self) {
        let mut hidden: HashSet<String> = HashSet::new();
        let mut collapsed: HashSet<String> = HashSet::new();
        for st in &self.states {
            if self.collapsed.contains(&server_key(&st.server)) {
                collapsed.insert(server_key(&st.server));
                hidden.extend(st.containers.iter().map(|c| c.id.clone()));
            }
        }
        *self.net.expanded.lock().unwrap() = self.expanded.difference(&hidden).cloned().collect();
        *self.net.collapsed.lock().unwrap() = collapsed;
    }

    fn set_sort(&mut self, key: usize) {
        if key == self.sort {
            self.sort_desc = !self.sort_desc;
        } else {
            self.sort = key;
            // Ports/Name/Age default ascending; State/CPU/RAM default descending.
            self.sort_desc = matches!(key, 2 | 4 | 5);
        }
        self.net.sort.store(self.sort as u8, Relaxed);
        settings::update(|s| {
            s.docker_sort = self.sort;
            s.docker_sort_desc = self.sort_desc;
        });
        let _ = self.net.wake.send(());
        if let Some(app) = self.app.upgrade() {
            let g = app.global::<DockerPage>();
            g.set_sort(self.sort as i32);
            g.set_sort_desc(self.sort_desc);
        }
        render_rows(self);
    }

    /// The column with this id was dragged and dropped at `x` of a header
    /// `header_w` wide (both in header coordinates — no window-size guessing).
    fn column_dropped(&mut self, id: String, x: f32, header_w: f32) {
        let order: Vec<String> = settings::get(|s| s.docker_cols.clone());
        // Column centres, in the same header coordinates as the drop point.
        let centers: Vec<(String, f32)> = layout()
            .into_iter()
            .map(|(cid, _, _, r, w, flex)| {
                let cx = if flex { 12.0 + (header_w - r - 12.0) / 2.0 } else { header_w - r + w / 2.0 };
                (cid, cx)
            })
            .collect();
        let new_order = crate::panels::reorder_after_drop(&order, &id, x, &centers);
        settings::update(|s| s.docker_cols = new_order);
        render_rows(self);
    }

    /// Saves a column's dragged width (the header edge was released).
    fn column_resized(&mut self, id: &str, w: f32) {
        let w = w.clamp(48.0, 500.0).round() as u32;
        settings::update(|s| {
            s.docker_col_widths.retain(|(c, _)| c != id);
            s.docker_col_widths.push((id.to_string(), w));
        });
        render_rows(self);
    }

    fn server_op(&mut self, idx: usize, op: &str) {
        match op {
            "refresh" => {
                let _ = self.net.wake.send(());
            }
            "remove" => {
                let name = self
                    .states
                    .get(idx)
                    .map(|s| s.server.name.clone())
                    .unwrap_or_else(|| format!("server {idx}"));
                settings::update(|s| {
                    if idx < s.docker_servers.len() {
                        s.docker_servers.remove(idx);
                    }
                });
                self.reload_servers();
                crate::ui::toast(format!("Removed {name} — its containers are untouched"));
            }
            _ => {}
        }
    }

    fn add_server(&mut self, name: &str, addr: &str) {
        if Server::parse(name, addr).is_none() {
            return crate::ui::toast("Enter an address like 10.0.0.5:2375");
        }
        let addr = addr.trim().to_string();
        let name = if name.trim().is_empty() { addr.clone() } else { name.trim().to_string() };
        if settings::get(|s| s.docker_servers.iter().any(|(_, a)| a == &addr)) {
            return crate::ui::toast(format!("{name} is already in the list"));
        }
        settings::update(|s| s.docker_servers.push((name.clone(), addr)));
        self.reload_servers();
    }

    fn reload_servers(&mut self) {
        let servers = load_servers();
        self.server_count = servers.len();
        *self.net.servers.lock().unwrap() = servers;
        let _ = self.net.wake.send(());
        render_rows(self);
    }
}

fn render() {
    with(render_rows);
}

fn render_rows(d: &mut DockerUi) {
    render_heads(d);
    // Keep the poller's NPM login in step with the Proxy tab's connect form.
    #[cfg(feature = "proxy")]
    {
        let creds = NpmCreds::from_settings();
        let mut npm = d.net.npm.lock().unwrap();
        if creds.as_ref().map(|c| (&c.server, &c.email, &c.password)) != npm.as_ref().map(|c| (&c.server, &c.email, &c.password)) {
            *npm = creds;
            let _ = d.net.wake.send(());
        }
    }
    let Some(app) = d.app.upgrade() else { return };
    let g = app.global::<DockerPage>();
    let mut rows: Vec<DockerRow> = Vec::new();
    let (mut total, mut running) = (0usize, 0usize);
    for (i, st) in d.states.iter().enumerate() {
        let open = !d.collapsed.contains(&server_key(&st.server));
        let status = match &st.error {
            Some(e) => e.clone(),
            None => {
                let r = st.containers.iter().filter(|c| c.running()).count();
                format!("{} of {} running   ·   {}:{}", r, st.containers.len(), st.server.host, st.server.port)
            }
        };
        rows.push(DockerRow {
            kind: 0,
            server: i as i32,
            name: st.server.name.as_str().into(),
            server_status: status.into(),
            server_error: st.error.is_some(),
            expanded: open,
            ..Default::default()
        });
        let mut cs: Vec<&Container> = st.containers.iter().collect();
        #[cfg(feature = "proxy")]
        let proxied: HashMap<String, u16> = cs
            .iter()
            .map(|c| {
                let port = proxied_matches(d, &st.server.host, c).iter().map(|(_, p)| *p).min().unwrap_or(0);
                (c.id.clone(), port)
            })
            .collect();
        #[cfg(not(feature = "proxy"))]
        let proxied: HashMap<String, u16> = HashMap::new();
        self_sort(&mut cs, d.sort, d.sort_desc, &d.latest, &proxied);
        for c in cs {
            // The summary counts every server's containers, shown or not.
            total += 1;
            if c.running() {
                running += 1;
            }
            if !open {
                continue;
            }
            let is_open = d.expanded.contains(&c.id);
            let matches = proxied_matches(d, &st.server.host, c);
            let proxied_port = matches.iter().map(|(_, p)| *p).min().unwrap_or(0);
            rows.push(container_row(c, i, is_open, d.latest.get(&c.id), proxied_port));
            if is_open {
                let by = matches.iter().map(|(d, p)| format!("{d} (:{p})")).collect::<Vec<_>>().join(", ");
                rows.push(detail_row(c, i, d, &by));
            }
        }
    }
    update_model(&d.rows, rows);

    // Configured/summary reflect whether SERVERS are set up, not whether a poll
    // has landed yet — so it never says "No servers yet" while connected.
    let configured = d.server_count > 0;
    g.set_configured(configured);
    g.set_summary(
        if !configured {
            "No servers yet".into()
        } else if d.states.is_empty() {
            format!("Connecting to {} server{}…", d.server_count, plural(d.server_count))
        } else {
            format!("{running} of {total} containers running across {} server{}", d.states.len(), plural(d.states.len()))
        }
        .into(),
    );
}

/// Sorts a server's containers by the chosen key.
fn self_sort(cs: &mut [&Container], sort: usize, desc: bool, latest: &HashMap<String, Stats>, proxied: &HashMap<String, u16>) {
    use std::cmp::Ordering;
    let cpu = |c: &Container| latest.get(&c.id).map_or(0.0, |s| s.cpu_pct);
    let mem = |c: &Container| latest.get(&c.id).map_or(0, |s| s.mem_used);
    let prox = |c: &Container| proxied.get(&c.id).copied().unwrap_or(0);
    cs.sort_by(|a, b| {
        let o = match sort {
            0 => a.lowest_port().cmp(&b.lowest_port()),
            1 => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            2 => b.running().cmp(&a.running()).then(a.state.cmp(&b.state)),
            3 => a.created.cmp(&b.created),
            4 => cpu(a).total_cmp(&cpu(b)),
            5 => mem(a).cmp(&mem(b)),
            _ => prox(a).cmp(&prox(b)),
        };
        let o = if desc { o.reverse() } else { o };
        // Stable tiebreak by name so the list doesn't jitter.
        o.then_with(|| a.name.cmp(&b.name)).then(a.id.cmp(&b.id) as Ordering)
    });
}

fn container_row(c: &Container, server: usize, expanded: bool, stat: Option<&Stats>, proxied_port: u16) -> DockerRow {
    // Ports stack one per line; longer lists show the first two + a count.
    let ports_list: Vec<String> =
        c.ports.iter().map(|p| format!("{}→{}/{}", p.public, p.private, p.proto)).collect();
    let ports = match ports_list.len() {
        0 => String::new(),
        1..=3 => ports_list.join("\n"),
        n => format!("{}\n+{} more", ports_list[..2].join("\n"), n - 2),
    };
    DockerRow {
        kind: 1,
        server: server as i32,
        id: c.id.as_str().into(),
        name: c.name.as_str().into(),
        image: c.image.as_str().into(),
        state: c.state.as_str().into(),
        state_color: state_color(&c.state),
        status: c.status.as_str().into(),
        ports: ports.into(),
        running: c.running(),
        expanded,
        proxied: proxied_port > 0,
        proxied_port: if proxied_port > 0 { format!(":{proxied_port}") } else { String::new() }.into(),
        cpu_text: stat.map_or_else(|| "—".into(), |s| format!("{:.1}%", s.cpu_pct)).into(),
        mem_text: stat.map_or_else(|| "—".into(), |s| fmt::bytes_short(s.mem_used)).into(),
        ..Default::default()
    }
}

fn detail_row(c: &Container, server: usize, d: &DockerUi, proxied_by: &str) -> DockerRow {
    let stat = d.latest.get(&c.id).copied().unwrap_or_default();
    let hist = d.hist.get(&c.id);
    let cpu_peak = hist.map_or(0.0, |h| h.cpu.max());
    let mem_peak = hist.map_or(0.0, |h| h.mem.max());
    let mem_limit = hist.map(|h| h.limit).filter(|&l| l > 0).unwrap_or(stat.mem_limit);
    let cpu_series = hist.map(|h| graph::ring(&h.cpu, nice_max(cpu_peak, 100.0))).unwrap_or_default();
    let mem_series = hist.map(|h| graph::ring(&h.mem, nice_max(mem_peak, 1.0))).unwrap_or_default();

    let mut facts: Vec<DockerKV> = Vec::new();
    let mut kv = |label: &str, value: String| {
        if !value.is_empty() {
            facts.push(DockerKV { label: label.into(), value: value.into() });
        }
    };
    let det = d.details.get(&c.id);
    kv("Image", c.image.clone());
    kv("Container ID", c.id.chars().take(12).collect());
    if let Some(det) = det {
        kv("Command", det.command.clone());
        kv("Restart policy", det.restart_policy.clone());
        if !det.health.is_empty() {
            kv("Health", det.health.clone());
        }
        for (net, ip) in &det.networks {
            kv("Network", format!("{net}   {ip}"));
        }
        for (src, dst, rw) in det.mounts.iter().take(6) {
            kv("Mount", format!("{src}  →  {dst}{}", if *rw { "" } else { "  (ro)" }));
        }
        if det.env_count > 0 {
            kv("Environment", format!("{} variables", det.env_count));
        }
    } else if c.running() {
        kv("Details", "loading…".into());
    }
    let all_ports = c.ports.iter().map(|p| format!("{}→{}/{}", p.public, p.private, p.proto)).collect::<Vec<_>>().join("\n");
    kv("Published ports", if all_ports.is_empty() { "none".into() } else { all_ports });
    if !proxied_by.is_empty() {
        kv("Proxied by", proxied_by.to_string());
    }

    DockerRow {
        kind: 2,
        server: server as i32,
        id: c.id.as_str().into(),
        cpu_series,
        cpu_text: format!("{:.1}%", stat.cpu_pct).into(),
        mem_series,
        mem_text: if mem_limit > 0 {
            format!("{} / {}", fmt::bytes_short(stat.mem_used), fmt::bytes_short(mem_limit))
        } else {
            fmt::bytes_short(stat.mem_used).into()
        }
        .into(),
        facts: ModelRc::new(VecModel::from(facts)),
        running: c.running(),
        ..Default::default()
    }
}

/// Stable key for a server (its address), so collapse state survives reloads.
fn server_key(s: &Server) -> String {
    format!("{}:{}", s.host, s.port)
}

fn state_color(state: &str) -> Color {
    let hex: u32 = match state {
        "running" => 0x2fd39a,
        "paused" => 0xffb13d,
        "restarting" => 0x3db4ff,
        "created" => 0x8d95a6,
        "exited" | "dead" => 0xff6b6b,
        _ => 0x8d95a6,
    };
    Color::from_rgb_u8((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

fn op_verb(op: &str) -> &str {
    match op {
        "start" => "Start",
        "stop" => "Stopp",
        _ => "Restart",
    }
}

fn op_done(op: &str) -> &str {
    match op {
        "start" => "started",
        "stop" => "stopped",
        _ => "restarted",
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

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

/// The registry handle: what panels.rs sees for this tab.
pub struct Controller;

impl crate::panels::Tab for Controller {
    fn id(&self) -> &'static str {
        "docker"
    }
    fn title(&self) -> &'static str {
        "Docker"
    }
    fn enabled(&self) -> bool {
        settings::get(|s| s.tabs_docker)
    }
    fn install(&self, app: &AppWindow) {
        install(app);
    }
    fn set_visible(&self, visible: bool) {
        set_visible(visible);
    }
}
