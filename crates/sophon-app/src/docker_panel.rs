//! The Docker tab (feature "docker"): a background poller that lists containers
//! on each configured server and lets you start / stop / restart them.
//!
//! All network work runs on a dedicated thread; results are delivered to the UI
//! thread via `invoke_from_event_loop`. Polling is fast (2 s) only while the tab
//! is visible, slow (15 s) otherwise, so it costs almost nothing in the
//! background — the same discipline as the local sampler.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use slint::{Color, ComponentHandle, Model, ModelRc, VecModel};
use sophon_docker::{Container, Server, ServerState};

use crate::settings;
use crate::{AppWindow, DockerPage, DockerRow};

const POLL_VISIBLE: Duration = Duration::from_secs(2);
const POLL_HIDDEN: Duration = Duration::from_secs(15);

/// Shared with the poller thread.
struct Net {
    servers: Mutex<Vec<Server>>,
    visible: AtomicBool,
    wake: mpsc::Sender<()>,
}

struct DockerUi {
    app: slint::Weak<AppWindow>,
    net: Arc<Net>,
    states: Vec<ServerState>,
    selected: String,
    rows: std::rc::Rc<VecModel<DockerRow>>,
}

thread_local! {
    static STATE: RefCell<Option<DockerUi>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut DockerUi) -> R) -> Option<R> {
    STATE.with_borrow_mut(|s| s.as_mut().map(f))
}

/// Reads the configured servers from settings.
fn load_servers() -> Vec<Server> {
    settings::get(|s| s.docker_servers.clone())
        .iter()
        .filter_map(|(name, addr)| Server::parse(name, addr))
        .collect()
}

pub fn install(app: &AppWindow) {
    let (wake, rx) = mpsc::channel();
    let net = Arc::new(Net { servers: Mutex::new(load_servers()), visible: AtomicBool::new(false), wake });
    let rows = std::rc::Rc::new(VecModel::default());
    app.global::<DockerPage>().set_rows(ModelRc::from(rows.clone()));

    STATE.set(Some(DockerUi { app: app.as_weak(), net: net.clone(), states: Vec::new(), selected: String::new(), rows }));

    let g = app.global::<DockerPage>();
    g.on_action(|id, op| {
        with(|d| d.container_action(&id, &op));
    });
    g.on_server_op(|idx, op| {
        with(|d| d.server_op(idx as usize, &op));
    });
    g.on_add_server(|name, addr| {
        with(|d| d.add_server(&name, &addr));
    });
    g.on_select(|id| {
        with(|d| {
            d.selected = id.to_string();
            if let Some(app) = d.app.upgrade() {
                app.global::<DockerPage>().set_selected_id(id);
            }
        });
    });

    // Poller thread.
    let poll_net = net;
    std::thread::Builder::new()
        .name("docker".into())
        .spawn(move || poller(poll_net, rx))
        .ok();
    render();
}

/// Called when the Docker tab is shown or hidden (from the panel lifecycle).
pub fn set_visible(visible: bool) {
    with(|d| {
        d.net.visible.store(visible, Relaxed);
        if visible {
            let _ = d.net.wake.send(());
        }
    });
}

/// The poller delivered fresh results.
fn apply(states: Vec<ServerState>) {
    with(|d| {
        d.states = states;
        render_rows(d);
    });
}

fn poller(net: Arc<Net>, wake: mpsc::Receiver<()>) {
    loop {
        let servers = net.servers.lock().unwrap().clone();
        let states: Vec<ServerState> = servers
            .iter()
            .map(|srv| match sophon_docker::list(srv) {
                Ok(mut cs) => {
                    cs.sort_by(|a, b| b.running().cmp(&a.running()).then_with(|| a.name.cmp(&b.name)));
                    ServerState { server: srv.clone(), error: None, containers: cs }
                }
                Err(e) => ServerState { server: srv.clone(), error: Some(e.to_string()), containers: Vec::new() },
            })
            .collect();
        if slint::invoke_from_event_loop(move || apply(states)).is_err() {
            return;
        }
        let interval = if net.visible.load(Relaxed) { POLL_VISIBLE } else { POLL_HIDDEN };
        // Sleep, but wake early on config/visibility changes.
        let deadline = Instant::now() + interval;
        loop {
            match wake.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(()) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

impl DockerUi {
    fn container_action(&mut self, id: &str, op: &str) {
        // Find which server owns this container.
        let Some(server) = self.states.iter().find(|s| s.containers.iter().any(|c| c.id == id)).map(|s| s.server.clone()) else {
            return;
        };
        let name = self
            .states
            .iter()
            .flat_map(|s| &s.containers)
            .find(|c| c.id == id)
            .map(|c| c.name.clone())
            .unwrap_or_default();
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
                let _ = net.wake.send(()); // refresh soon
            });
        });
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
        let name = if name.trim().is_empty() { addr.trim().to_string() } else { name.trim().to_string() };
        if Server::parse(&name, addr).is_none() {
            return crate::ui::toast("Enter an address like 10.0.0.5:2375");
        }
        settings::update(|s| s.docker_servers.push((name, addr.trim().to_string())));
        self.reload_servers();
    }

    fn reload_servers(&mut self) {
        *self.net.servers.lock().unwrap() = load_servers();
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
        for c in &st.containers {
            total += 1;
            if c.running() {
                running += 1;
            }
            rows.push(container_row(c, i, &d.selected));
        }
    }
    let count = d.states.len();
    update_model(&d.rows, rows);
    g.set_configured(count > 0);
    g.set_summary(
        if count == 0 {
            "No servers yet".into()
        } else {
            format!("{running} of {total} containers running across {count} server{}", if count == 1 { "" } else { "s" })
        }
        .into(),
    );
}

fn container_row(c: &Container, server: usize, selected: &str) -> DockerRow {
    let ports = c
        .ports
        .iter()
        .map(|p| format!("{}→{}/{}", p.public, p.private, p.proto))
        .collect::<Vec<_>>()
        .join("  ");
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
        selected: c.id == selected,
        ..Default::default()
    }
}

fn state_color(state: &str) -> Color {
    let hex = match state {
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
