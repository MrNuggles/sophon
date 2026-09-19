//! The Proxy tab (feature "proxy"): every host your Nginx Proxy Manager
//! routes — domains, forward target, port, SSL state — with certificates for
//! expiry. One connection, configured in-tab; the credentials live only in
//! the local settings file (written 0600).
//!
//! Same discipline as the Docker tab: all network work on a dedicated thread,
//! results to the UI thread via `invoke_from_event_loop`. NPM data changes
//! rarely, so the poller runs only while the tab is visible (30 s) and parks
//! the rest of the time — nothing hidden is fetched.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Model, ModelRc, VecModel};
use sophon_npm::{Certificate, ProxyHost, Server, http, main_domain};

use crate::{AppWindow, ProxyKV, ProxyPage, ProxyRow, settings};

const POLL_VISIBLE: Duration = Duration::from_secs(30);

/// What the user configured, shared with the poller thread.
#[derive(Clone)]
struct Creds {
    server: Server,
    email: String,
    password: String,
}

impl Creds {
    fn from_settings() -> Option<Creds> {
        let (url, email, password) = settings::get(|s| (s.npm_url.clone(), s.npm_email.clone(), s.npm_password.clone()));
        if url.is_empty() || email.is_empty() || password.is_empty() {
            return None;
        }
        Some(Creds { server: Server::parse("", &url)?, email, password })
    }
}

/// Shared with the poller thread.
struct Net {
    creds: Mutex<Option<Creds>>,
    visible: AtomicBool,
    wake: mpsc::Sender<()>,
}

/// One poller delivery.
enum Poll {
    Hosts { hosts: Vec<ProxyHost>, certs: Vec<Certificate> },
    Error(String),
}

#[derive(Clone, Default, PartialEq)]
enum Phase {
    #[default]
    NotConfigured,
    Connecting,
    Failed(String),
    Loaded,
}

struct ProxyUi {
    app: slint::Weak<AppWindow>,
    net: Arc<Net>,
    hosts: Vec<ProxyHost>,
    certs: HashMap<u64, Certificate>,
    expanded: std::collections::HashSet<u64>,
    phase: Phase,
    rows: std::rc::Rc<VecModel<ProxyRow>>,
}

thread_local! {
    static STATE: RefCell<Option<ProxyUi>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut ProxyUi) -> R) -> Option<R> {
    STATE.with_borrow_mut(|s| s.as_mut().map(f))
}

pub fn install(app: &AppWindow) {
    let (wake, rx) = mpsc::channel();
    let configured = Creds::from_settings();
    let net = Arc::new(Net {
        creds: Mutex::new(configured.clone()),
        visible: AtomicBool::new(false),
        wake,
    });
    let rows = std::rc::Rc::new(VecModel::default());
    let g = app.global::<ProxyPage>();
    g.set_rows(ModelRc::from(rows.clone()));

    let (url, email) = settings::get(|s| (s.npm_url.clone(), s.npm_email.clone()));
    let phase = if configured.is_some() { Phase::Connecting } else { Phase::NotConfigured };
    STATE.set(Some(ProxyUi {
        app: app.as_weak(),
        net: net.clone(),
        hosts: Vec::new(),
        certs: HashMap::new(),
        expanded: std::collections::HashSet::new(),
        phase: phase.clone(),
        rows,
    }));
    render_phase(&g, &phase, &url, &email);

    g.on_connect(|url, email, password| {
        with(|d| d.connect(&url, &email, &password));
    });
    g.on_forget(|| {
        with(|d| d.forget());
    });
    g.on_refresh(|| {
        with(|d| {
            let _ = d.net.wake.send(());
        });
    });
    g.on_toggle(|id| {
        with(|d| {
            let id = id.max(0) as u64;
            if !d.expanded.remove(&id) {
                d.expanded.insert(id);
            }
            render_rows(d);
        });
    });

    std::thread::Builder::new().name("proxy".into()).spawn(move || poller(net, rx)).ok();
}

pub fn set_visible(visible: bool) {
    with(|d| {
        d.net.visible.store(visible, Relaxed);
        if visible {
            let _ = d.net.wake.send(());
        }
    });
}

fn poller(net: Arc<Net>, wake: mpsc::Receiver<()>) {
    let mut token: Option<String> = None;
    loop {
        // Parked unless the tab is showing and a connection is configured.
        if !net.visible.load(Relaxed) || net.creds.lock().unwrap().is_none() {
            if wake.recv().is_err() {
                return;
            }
            continue;
        }
        let creds = net.creds.lock().unwrap().clone();
        let poll = creds.map_or(Poll::Error("Not configured".into()), |c| fetch(&c, &mut token));
        if slint::invoke_from_event_loop(move || apply(poll)).is_err() {
            return;
        }
        let deadline = Instant::now() + POLL_VISIBLE;
        match wake.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(()) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        while wake.try_recv().is_ok() {}
    }
}

/// Fetches hosts and certificates, reusing the token and re-logging in once
/// if it expired.
fn fetch(creds: &Creds, token: &mut Option<String>) -> Poll {
    let get = |token: &str| -> Result<(Vec<ProxyHost>, Vec<Certificate>), http::Error> {
        Ok((sophon_npm::proxy_hosts(&creds.server, token)?, sophon_npm::certificates(&creds.server, token)?))
    };
    // Reuse the cached token; a "not authorised" reply means it expired.
    if let Some(t) = token.as_deref() {
        match get(t) {
            Ok(out) => return Poll::Hosts { hosts: out.0, certs: out.1 },
            Err(e) if expired(&e) => *token = None,
            Err(e) => return Poll::Error(e.to_string()),
        }
    }
    match sophon_npm::login(&creds.server, &creds.email, &creds.password) {
        Ok(t) => match get(&t) {
            Ok(out) => {
                *token = Some(t);
                Poll::Hosts { hosts: out.0, certs: out.1 }
            }
            Err(e) => Poll::Error(e.to_string()),
        },
        Err(e) => Poll::Error(e.to_string()),
    }
}

fn expired(e: &http::Error) -> bool {
    matches!(e, http::Error::Protocol(p) if p.contains("not authorised"))
}

fn apply(poll: Poll) {
    with(|d| {
        match poll {
            Poll::Hosts { hosts, certs } => {
                d.hosts = hosts;
                d.certs = certs.into_iter().map(|c| (c.id, c)).collect();
                d.phase = Phase::Loaded;
            }
            Poll::Error(e) => {
                // While the tab is hidden a stale wake can land a poll after
                // Forget; only surface errors when a connection is configured.
                if d.net.creds.lock().unwrap().is_some() {
                    d.phase = Phase::Failed(e);
                }
            }
        }
        // A full render: the phase flags decide form-vs-table, not just rows.
        d.render();
    });
}

impl ProxyUi {
    /// Saves the connection (it becomes the settings file's npm.*) and retries.
    fn connect(&mut self, url: &str, email: &str, password: &str) {
        let (url, email) = (url.trim(), email.trim());
        if Server::parse("", url).is_none() {
            return self.status("Enter an address like http://192.168.1.209:81");
        }
        if email.is_empty() || password.is_empty() {
            return self.status("Enter the email and password for NPM.");
        }
        settings::update(|s| {
            s.npm_url = url.to_string();
            s.npm_email = email.to_string();
            s.npm_password = password.to_string();
        });
        *self.net.creds.lock().unwrap() = Creds::from_settings();
        self.phase = Phase::Connecting;
        self.status("");
        let _ = self.net.wake.send(());
        self.render();
    }

    /// Clears the stored connection and returns to the empty form.
    fn forget(&mut self) {
        settings::update(|s| {
            s.npm_url.clear();
            s.npm_email.clear();
            s.npm_password.clear();
        });
        *self.net.creds.lock().unwrap() = None;
        let _ = self.net.wake.send(()); // the poller parks; a stale poll is dropped below
        (self.hosts, self.certs, self.expanded) = (Vec::new(), HashMap::new(), Default::default());
        self.phase = Phase::NotConfigured;
        self.status("");
        self.render();
    }

    /// Sets the form's status line (empty clears it).
    fn status(&mut self, msg: &str) {
        if let Some(app) = self.app.upgrade() {
            app.global::<ProxyPage>().set_status(msg.into());
        }
    }

    fn render(&mut self) {
        let (url, email) = settings::get(|s| (s.npm_url.clone(), s.npm_email.clone()));
        let phase = self.phase.clone();
        if let Some(app) = self.app.upgrade() {
            render_phase(&app.global::<ProxyPage>(), &phase, &url, &email);
        }
        render_rows(self);
    }
}

/// Pushes the phase into the page flags: which of the form / connecting note /
/// table the view shows.
fn render_phase(g: &ProxyPage, phase: &Phase, url: &str, email: &str) {
    let (configured, failed, connecting) = match phase {
        Phase::NotConfigured => (false, false, false),
        Phase::Connecting => (true, false, true),
        Phase::Failed(_) => (true, true, false),
        Phase::Loaded => (true, false, false),
    };
    g.set_configured(configured);
    g.set_failed(failed);
    g.set_connecting(connecting);
    g.set_url(url.into());
    g.set_email(email.into());
    if let Phase::Failed(e) = phase {
        g.set_status(e.clone().into());
    }
}

fn render_rows(d: &mut ProxyUi) {
    let Some(app) = d.app.upgrade() else { return };
    let g = app.global::<ProxyPage>();
    let mut rows: Vec<ProxyRow> = Vec::new();
    d.hosts.sort_by(|a, b| main_domain(a).to_lowercase().cmp(&main_domain(b).to_lowercase()));
    let summary = match &d.phase {
        Phase::Loaded => {
            let enabled = d.hosts.iter().filter(|h| h.enabled).count();
            format!(
                "{} of {} hosts · {}",
                enabled,
                d.hosts.len(),
                d.net.creds.lock().unwrap().as_ref().map_or_else(String::new, |c| format!("{}:{}", c.server.host, c.server.port))
            )
            .into()
        }
        Phase::Connecting => "Connecting…".into(),
        _ => "".into(),
    };
    g.set_summary(summary);
    for h in &d.hosts {
        let (ssl_label, ssl_tone) = match (h.certificate_id, h.ssl_forced, d.certs.get(&h.certificate_id)) {
            (_, _, Some(c)) if cert_expired(c) => ("Cert expired", 3),
            (0, _, _) => ("HTTP", 2),
            (_, true, _) => ("SSL forced", 0),
            (_, _, Some(_)) => ("SSL", 0),
            _ => ("SSL", 1),
        };
        rows.push(ProxyRow {
            id: h.id as i32,
            kind: 0,
            domain: sophon_npm::main_domain(h).into(),
            extra_domains: if h.domains.len() > 1 { format!("+{}", h.domains.len() - 1) } else { String::new() }.into(),
            target: format!("{}://{}:{}", h.forward_scheme, h.forward_host, h.forward_port).into(),
            enabled: h.enabled,
            ssl_label: ssl_label.into(),
            ssl_tone,
            expanded: d.expanded.contains(&h.id),
            ..Default::default()
        });
        if d.expanded.contains(&h.id) {
            rows.push(detail_row(h, d));
        }
    }
    update_model(&d.rows, rows);
    let empty = d.hosts.is_empty();
    g.set_empty(empty);
}

/// Days until a certificate's "YYYY-MM-DD HH:MM:SS" expiry (negative = past).
fn days_until(expires: &str) -> Option<i64> {
    let date = expires.split(' ').next()?;
    let mut parts = date.split('-');
    let (y, mo, d) = (parts.next()?.parse().ok()?, parts.next()?.parse().ok()?, parts.next()?.parse().ok()?);
    let days = |y: i64, m: i64, d: i64| -> i64 {
        // Days since 1970-01-01 (Howard Hinnant's algorithm), good into 2400.
        let y = if m <= 2 { y - 1 } else { y };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = y - era * 400;
        let mp = (m + 9) % 12;
        let doy = (153 * mp + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146097 + doe - 719468
    };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64 / 86_400;
    Some(days(y, mo, d) - now)
}

fn cert_expired(c: &Certificate) -> bool {
    days_until(&c.expires).is_some_and(|d| d < 0)
}

fn detail_row(h: &ProxyHost, d: &ProxyUi) -> ProxyRow {
    let mut facts: Vec<ProxyKV> = Vec::new();
    let mut kv = |label: &str, value: String| {
        if !value.is_empty() {
            facts.push(ProxyKV { label: label.into(), value: value.into() });
        }
    };
    kv("All domains", h.domains.join("  "));
    kv("Forwards to", format!("{}://{}:{}", h.forward_scheme, h.forward_host, h.forward_port));
    if h.certificate_id > 0
        && let Some(c) = d.certs.get(&h.certificate_id)
    {
        kv("Certificate", format!("{} ({})", if c.nice_name.is_empty() { c.domains.join(", ") } else { c.nice_name.clone() }, c.provider));
        kv("Expires", match days_until(&c.expires) {
            Some(d) if d < 0 => format!("{} — EXPIRED", c.expires),
            Some(d) if d <= 30 => format!("{} — {} days", c.expires, d),
            _ => c.expires.clone(),
        });
    }
    let mut flags = Vec::new();
    if h.ssl_forced { flags.push("SSL forced"); }
    if h.http2 { flags.push("HTTP/2"); }
    if h.websockets { flags.push("WebSockets"); }
    if h.caching { flags.push("Caching"); }
    if h.block_exploits { flags.push("Block exploits"); }
    kv("Features", flags.join("  ·  "));
    kv("Last change", h.modified_on.clone());
    ProxyRow { id: h.id as i32, kind: 1, facts: ModelRc::new(VecModel::from(facts)), ..Default::default() }
}

fn update_model(m: &VecModel<ProxyRow>, new: Vec<ProxyRow>) {
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
