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

use crate::graph::{self, nice_max};
use crate::{AppWindow, DockerKV, DockerPage, DockerRow, fmt, settings};

const POLL_VISIBLE: Duration = Duration::from_secs(2);
const POLL_HIDDEN: Duration = Duration::from_secs(15);

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
    sort: AtomicU8,
    visible: AtomicBool,
    wake: mpsc::Sender<()>,
}

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
    sort: usize,
    sort_desc: bool,
    hist: HashMap<String, Hist>,
    latest: HashMap<String, Stats>,
    details: HashMap<String, Details>,
    rows: std::rc::Rc<VecModel<DockerRow>>,
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
        sort: AtomicU8::new(sort as u8),
        visible: AtomicBool::new(false),
        wake,
    });
    let rows = std::rc::Rc::new(VecModel::default());
    let g = app.global::<DockerPage>();
    g.set_rows(ModelRc::from(rows.clone()));
    g.set_sort(sort as i32);
    g.set_sort_desc(sort_desc);

    STATE.set(Some(DockerUi {
        app: app.as_weak(),
        net: net.clone(),
        states: Vec::new(),
        server_count: servers.len(),
        expanded: HashSet::new(),
        sort,
        sort_desc,
        hist: HashMap::new(),
        latest: HashMap::new(),
        details: HashMap::new(),
        rows,
    }));

    g.on_action(|id, op| {
        with(|d| d.container_action(&id, &op));
    });
    g.on_server_op(|idx, op| {
        with(|d| d.server_op(idx as usize, &op));
    });
    g.on_add_server(|name, addr| {
        with(|d| d.add_server(&name, &addr));
    });
    g.on_toggle(|id| {
        with(|d| d.toggle(&id));
    });
    g.on_set_sort(|key| {
        with(|d| d.set_sort(key.max(0) as usize));
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

fn apply(poll: Poll) {
    with(|d| {
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
        render_rows(d);
    });
}

fn poller(net: Arc<Net>, wake: mpsc::Receiver<()>) {
    loop {
        let servers = net.servers.lock().unwrap().clone();
        let visible = net.visible.load(Relaxed);
        let expanded = if visible { net.expanded.lock().unwrap().clone() } else { HashSet::new() };
        // Fetch all running containers' stats only to sort by usage, and only
        // while the tab is visible — otherwise `list` alone keeps state fresh.
        let sort_usage = visible && matches!(net.sort.load(Relaxed) as usize, 4 | 5);
        let mut stats = HashMap::new();
        let mut details = HashMap::new();
        let states: Vec<ServerState> = servers
            .iter()
            .map(|srv| match sophon_docker::list(srv) {
                Ok(cs) => {
                    for c in &cs {
                        if !c.running() {
                            continue;
                        }
                        let want_stats = expanded.contains(&c.id) || sort_usage;
                        if want_stats && let Ok(s) = sophon_docker::stats(srv, &c.id) {
                            stats.insert(c.id.clone(), s);
                        }
                        if expanded.contains(&c.id) && let Ok(dt) = sophon_docker::inspect(srv, &c.id) {
                            details.insert(c.id.clone(), dt);
                        }
                    }
                    ServerState { server: srv.clone(), error: None, containers: cs }
                }
                Err(e) => ServerState { server: srv.clone(), error: Some(e.to_string()), containers: Vec::new() },
            })
            .collect();
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
        *self.net.expanded.lock().unwrap() = self.expanded.clone();
        let _ = self.net.wake.send(()); // fetch its stats/details promptly
        render_rows(self);
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

    fn server_op(&mut self, idx: usize, op: &str) {
        match op {
            "refresh" => {
                let _ = self.net.wake.send(());
            }
            "remove" => {
                settings::update(|s| {
                    if idx < s.docker_servers.len() {
                        s.docker_servers.remove(idx);
                    }
                });
                self.reload_servers();
            }
            _ => {}
        }
    }

    fn add_server(&mut self, name: &str, addr: &str) {
        if Server::parse(name, addr).is_none() {
            return crate::ui::toast("Enter an address like 10.0.0.5:2375");
        }
        let name = if name.trim().is_empty() { addr.trim().to_string() } else { name.trim().to_string() };
        settings::update(|s| s.docker_servers.push((name, addr.trim().to_string())));
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
    let Some(app) = d.app.upgrade() else { return };
    let g = app.global::<DockerPage>();
    let mut rows: Vec<DockerRow> = Vec::new();
    let (mut total, mut running) = (0usize, 0usize);
    for (i, st) in d.states.iter().enumerate() {
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
            ..Default::default()
        });
        let mut cs: Vec<&Container> = st.containers.iter().collect();
        self_sort(&mut cs, d.sort, d.sort_desc, &d.latest);
        for c in cs {
            total += 1;
            if c.running() {
                running += 1;
            }
            let open = d.expanded.contains(&c.id);
            rows.push(container_row(c, i, open));
            if open {
                rows.push(detail_row(c, i, d));
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
fn self_sort(cs: &mut [&Container], sort: usize, desc: bool, latest: &HashMap<String, Stats>) {
    use std::cmp::Ordering;
    let cpu = |c: &Container| latest.get(&c.id).map_or(0.0, |s| s.cpu_pct);
    let mem = |c: &Container| latest.get(&c.id).map_or(0, |s| s.mem_used);
    cs.sort_by(|a, b| {
        let o = match sort {
            0 => a.lowest_port().cmp(&b.lowest_port()),
            1 => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            2 => b.running().cmp(&a.running()).then(a.state.cmp(&b.state)),
            3 => a.created.cmp(&b.created),
            4 => cpu(a).total_cmp(&cpu(b)),
            _ => mem(a).cmp(&mem(b)),
        };
        let o = if desc { o.reverse() } else { o };
        // Stable tiebreak by name so the list doesn't jitter.
        o.then_with(|| a.name.cmp(&b.name)).then(a.id.cmp(&b.id) as Ordering)
    });
}

fn container_row(c: &Container, server: usize, expanded: bool) -> DockerRow {
    let ports = c.ports.iter().map(|p| format!("{}→{}/{}", p.public, p.private, p.proto)).collect::<Vec<_>>().join("  ");
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
        ..Default::default()
    }
}

fn detail_row(c: &Container, server: usize, d: &DockerUi) -> DockerRow {
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
    let all_ports = c.ports.iter().map(|p| format!("{}→{}/{}", p.public, p.private, p.proto)).collect::<Vec<_>>().join("    ");
    kv("Published ports", if all_ports.is_empty() { "none".into() } else { all_ports });

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
