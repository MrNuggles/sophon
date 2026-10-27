//! Update checker and one-click updater (feature "updates").
//!
//! Once a day (and on demand from Settings → General → Updates) Sophon asks
//! GitHub for the latest release. If it is newer than the running build, a
//! banner offers to install it:
//!
//! * **AppImage** — the new AppImage replaces the running file in place.
//! * **.deb / .rpm** (installed at /usr/bin/sophon) — the package is
//!   downloaded, then installed with `pkexec apt-get|dnf|zypper|rpm`, so the
//!   system's own authentication dialog asks for the password. Sophon itself
//!   never runs as root.
//! * **Anything else** (a build from source) — the banner links to the
//!   release page.
//!
//! Every download is checked against the release's SHA256SUMS before it is
//! used, and downloads are only accepted from this project's GitHub releases.
//!
//! HTTPS is done by the system `curl` (argv array, never a shell): Sophon has
//! no TLS stack of its own and `sophon-net` is plain HTTP. All network work
//! happens on one worker thread; the UI thread only runs a coarse timer. Turn
//! the check off in Settings and nothing is ever fetched.

use std::cell::RefCell;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, atomic::Ordering::Relaxed};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use slint::{ComponentHandle, SharedString};
use sophon_net::json;

use crate::{AppWindow, Shared, Updates, settings};

const API: &str = "https://api.github.com/repos/MrNuggles/sophon/releases/latest";
const DOWNLOADS: &str = "https://github.com/MrNuggles/sophon/releases/download/";
const PAGES: &str = "https://github.com/MrNuggles/sophon/releases/";
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// First automatic check after launch, so startup stays quiet.
const FIRST_CHECK: Duration = Duration::from_secs(20);
/// How often the (cheap) "is a check due?" test runs.
const TICK: Duration = Duration::from_secs(6 * 3600);
/// Minimum gap between automatic checks, across restarts.
const DUE_SECS: u64 = 20 * 3600;

// ---- Where to look ------------------------------------------------------------

/// Where releases come from. Production is always GitHub over HTTPS;
/// `SOPHON_UPDATE_API` points it at a local file for testing (and then also
/// allows `file://` downloads).
#[derive(Clone)]
struct Net {
    api: String,
    local: bool,
}

impl Net {
    fn from_env() -> Net {
        match std::env::var("SOPHON_UPDATE_API") {
            Ok(url) if !url.is_empty() => Net { api: url, local: true },
            _ => Net { api: API.into(), local: false },
        }
    }
}

// ---- Versions and releases ------------------------------------------------------

/// "v1.2.3", "1.2" or "1.2.3-rc1" as numbers; anything else is rejected.
fn parse_version(v: &str) -> Option<[u64; 3]> {
    let core = v.trim().trim_start_matches('v').split(['-', '+']).next()?;
    let mut out = [0u64; 3];
    let mut n = 0;
    for part in core.split('.') {
        if n == 3 {
            return None;
        }
        out[n] = part.parse().ok()?;
        n += 1;
    }
    Some(out)
}

pub fn is_newer(latest: &str, current: &str) -> bool {
    matches!((parse_version(latest), parse_version(current)), (Some(a), Some(b)) if a > b)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Asset {
    pub name: String,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    /// "0.3.0" — numbers only, safe to show and to build paths from.
    pub version: String,
    pub page: String,
    pub assets: Vec<Asset>,
}

/// File names that are safe to use as a path component and as a command argument.
fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && !name.starts_with(['.', '-'])
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
}

/// Reads the GitHub "latest release" document. Drafts and pre-releases, and
/// any asset that doesn't come from this project's releases, are dropped.
fn parse_release(body: &[u8], net: &Net) -> Option<Release> {
    let v = json::parse(body)?;
    if v.get("draft").bool() || v.get("prerelease").bool() {
        return None;
    }
    let tag = v.get("tag_name").str();
    let [a, b, c] = parse_version(tag)?;
    let page = match v.get("html_url").str() {
        u if u.starts_with(PAGES) || (net.local && !u.is_empty()) => u.to_string(),
        _ if safe_name(tag) => format!("{PAGES}tag/{tag}"),
        _ => PAGES.to_string(),
    };
    let assets = v
        .get("assets")
        .arr()
        .iter()
        .filter_map(|x| {
            let (name, url) = (x.get("name").str(), x.get("browser_download_url").str());
            (safe_name(name) && (url.starts_with(DOWNLOADS) || (net.local && !url.is_empty())))
                .then(|| Asset { name: name.into(), url: url.into() })
        })
        .collect();
    Some(Release { version: format!("{a}.{b}.{c}"), page, assets })
}

// ---- How Sophon was installed ---------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    AppImage(PathBuf),
    Deb,
    Rpm,
    /// Built from source or unpackaged: link to the release page instead.
    Manual,
}

fn ran_ok(prog: &str, args: &[&str]) -> bool {
    Command::new(prog).args(args).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

fn detect() -> Kind {
    if let Some(p) = std::env::var_os("APPIMAGE").map(PathBuf::from)
        && p.is_file()
    {
        return Kind::AppImage(p);
    }
    // After an in-place upgrade the kernel reports "... (deleted)".
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().trim_end_matches(" (deleted)").to_string()).unwrap_or_default();
    if exe == "/usr/bin/sophon" {
        if ran_ok("dpkg", &["-S", "/usr/bin/sophon"]) {
            return Kind::Deb;
        }
        if ran_ok("rpm", &["-qf", "/usr/bin/sophon"]) {
            return Kind::Rpm;
        }
    }
    Kind::Manual
}

/// The release file that matches how Sophon is installed here.
fn pick<'a>(rel: &'a Release, kind: &Kind) -> Option<&'a Asset> {
    let arch = std::env::consts::ARCH;
    let deb_arch = match arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    rel.assets.iter().find(|a| match kind {
        Kind::AppImage(_) => a.name.ends_with(".AppImage") && a.name.contains(arch),
        Kind::Deb => a.name.ends_with(&format!("_{deb_arch}.deb")),
        Kind::Rpm => a.name.ends_with(&format!(".{arch}.rpm")),
        Kind::Manual => false,
    })
}

fn have(prog: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|d| d.join(prog).is_file()))
}

/// The privileged command that installs a downloaded package. `pkg` is an
/// absolute path in Sophon's private cache directory.
fn install_command(kind: &Kind, pkg: &Path, has: &dyn Fn(&str) -> bool) -> Option<Vec<String>> {
    let pkg = pkg.to_string_lossy().into_owned();
    let cmd: &[&str] = match kind {
        Kind::Deb => &["pkexec", "apt-get", "install", "-y"],
        Kind::Rpm if has("dnf") => &["pkexec", "dnf", "install", "-y"],
        Kind::Rpm if has("zypper") => &["pkexec", "zypper", "--non-interactive", "--allow-unsigned-rpm", "install"],
        Kind::Rpm => &["pkexec", "rpm", "-U", "--quiet"],
        _ => return None,
    };
    Some(cmd.iter().map(|s| s.to_string()).chain([pkg]).collect())
}

// ---- Checksums ------------------------------------------------------------------

struct Sha256 {
    h: [u32; 8],
    buf: [u8; 64],
    fill: usize,
    total: u64,
}

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
    0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
    0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
    0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

impl Sha256 {
    fn new() -> Sha256 {
        Sha256 { h: [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19], buf: [0; 64], fill: 0, total: 0 }
    }

    fn block(&mut self, b: &[u8; 64]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([b[i * 4], b[i * 4 + 1], b[i * 4 + 2], b[i * 4 + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let mut v = self.h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7].wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [t1.wrapping_add(t2), v[0], v[1], v[2], v[3].wrapping_add(t1), v[4], v[5], v[6]];
        }
        for (h, x) in self.h.iter_mut().zip(v) {
            *h = h.wrapping_add(x);
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.total += data.len() as u64;
        while !data.is_empty() {
            let n = (64 - self.fill).min(data.len());
            self.buf[self.fill..self.fill + n].copy_from_slice(&data[..n]);
            self.fill += n;
            data = &data[n..];
            if self.fill == 64 {
                let b = self.buf;
                self.block(&b);
                self.fill = 0;
            }
        }
    }

    fn finish(mut self) -> String {
        let bits = self.total * 8;
        self.update(&[0x80]);
        while self.fill != 56 {
            self.update(&[0]);
        }
        self.update(&bits.to_be_bytes());
        self.h.iter().map(|x| format!("{x:08x}")).collect()
    }
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let (mut h, mut buf) = (Sha256::new(), vec![0u8; 1 << 16]);
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Ok(h.finish());
        }
        h.update(&buf[..n]);
    }
}

/// The hash listed for `name` in a `sha256sum`-format file.
fn parse_sums(text: &str, name: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let (hash, file) = l.split_once(char::is_whitespace)?;
        let ok = hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit());
        (ok && file.trim().trim_start_matches('*') == name).then(|| hash.to_ascii_lowercase())
    })
}

// ---- Network and install (worker thread) -----------------------------------------

/// Fetches `url` with the system curl: to `dest` if given, otherwise returned.
fn curl(net: &Net, url: &str, dest: Option<&Path>, max_bytes: u64, max_secs: u32) -> Result<Vec<u8>, String> {
    let mut c = Command::new("curl");
    c.args(["--fail", "--silent", "--show-error", "--location", "--connect-timeout", "15"])
        .args(["--proto", if net.local { "=https,file" } else { "=https" }, "--proto-redir", "=https"])
        .args(["--max-time", &max_secs.to_string(), "--max-filesize", &max_bytes.to_string()])
        .args(["--user-agent", &format!("sophon/{CURRENT}"), "--header", "Accept: application/vnd.github+json"]);
    if let Some(d) = dest {
        c.arg("--output").arg(d);
    }
    c.arg("--").arg(url).stdin(Stdio::null());
    let out = c.output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound { "curl isn't installed, so Sophon can't check for updates".to_string() } else { format!("Couldn't run curl: {e}") }
    })?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr);
        let why = why.lines().last().unwrap_or("").trim().trim_start_matches("curl: ");
        return Err(match out.status.code() {
            Some(6 | 7 | 28) => "Couldn't reach GitHub — are you offline?".to_string(),
            Some(22) if why.contains("404") => "GitHub has no public release to show (404). If the Sophon repository is private, updates can't be checked.".to_string(),
            _ if why.is_empty() => "The download failed".to_string(),
            _ => format!("The download failed: {why}"),
        });
    }
    Ok(out.stdout)
}

fn fetch_latest(net: &Net) -> Result<Release, String> {
    let body = curl(net, &net.api, None, 1 << 20, 20)?;
    parse_release(&body, net).ok_or_else(|| "GitHub's answer wasn't a release Sophon understands".to_string())
}

/// Downloads `asset` to `dest` and checks it against the release's SHA256SUMS.
/// `dest` is removed again if anything is wrong with it.
fn download_verified(net: &Net, rel: &Release, asset: &Asset, dest: &Path) -> Result<(), String> {
    let sums = rel.assets.iter().find(|a| a.name == "SHA256SUMS").ok_or("This release has no SHA256SUMS to check against")?;
    let sums = curl(net, &sums.url, None, 1 << 20, 30)?;
    let want = parse_sums(&String::from_utf8_lossy(&sums), &asset.name).ok_or("SHA256SUMS doesn't list this download")?;
    let result = curl(net, &asset.url, Some(dest), 400 << 20, 900).and_then(|_| {
        let got = sha256_file(dest).map_err(|e| format!("Couldn't read the download: {e}"))?;
        if got == want { Ok(()) } else { Err("The download is corrupt (checksum mismatch), so it was discarded".to_string()) }
    });
    if result.is_err() {
        let _ = std::fs::remove_file(dest);
    }
    result.map(|_| ())
}

/// Replaces the running AppImage with the new one (same path, so launchers
/// and autostart entries keep working). The old process keeps running from
/// the old inode until it restarts.
fn install_appimage(net: &Net, rel: &Release, asset: &Asset, target: &Path, status: &dyn Fn(&str)) -> Result<(), String> {
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "sophon".into());
    let tmp = target.with_file_name(format!(".{name}.update"));
    status("Downloading the update…");
    download_verified(net, rel, asset, &tmp).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    let finish = || -> std::io::Result<()> {
        let mut magic = [0u8; 4];
        std::fs::File::open(&tmp)?.read_exact(&mut magic)?;
        if &magic != b"\x7fELF" {
            return Err(std::io::Error::other("not an executable AppImage"));
        }
        let mode = std::fs::metadata(target).map(|m| m.permissions().mode()).unwrap_or(0o755) | 0o111;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
        std::fs::rename(&tmp, target)
    };
    finish().map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        match e.kind() {
            std::io::ErrorKind::PermissionDenied => format!("Sophon can't write to {}", target.parent().unwrap_or(Path::new("/")).display()),
            _ => format!("Couldn't replace the AppImage: {e}"),
        }
    })
}

fn cache_dir() -> Result<PathBuf, String> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .ok_or("No cache folder to download into")?;
    let dir = base.join("sophon").join("update");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("Couldn't create {}: {e}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    Ok(dir)
}

/// Downloads the package, then hands it to the system package manager through
/// pkexec (the system's own password dialog).
fn install_package(net: &Net, rel: &Release, asset: &Asset, kind: &Kind, status: &dyn Fn(&str)) -> Result<(), String> {
    if !have("pkexec") {
        return Err("pkexec (polkit) isn't installed, so Sophon can't install the package".into());
    }
    let dir = cache_dir()?;
    let pkg = dir.join(&asset.name);
    status("Downloading the update…");
    let cmd = install_command(kind, &pkg, &have).ok_or("This install can't be updated automatically")?;
    download_verified(net, rel, asset, &pkg)?;
    status("Waiting for your password, then installing…");
    let out = Command::new(&cmd[0]).args(&cmd[1..]).stdin(Stdio::null()).output().map_err(|e| format!("Couldn't start {}: {e}", cmd[0]));
    let _ = std::fs::remove_dir_all(&dir);
    let out = out?;
    match out.status.code() {
        Some(0) => Ok(()),
        Some(126 | 127) => Err("Cancelled — nothing was installed".into()),
        _ => {
            let err = String::from_utf8_lossy(&out.stderr);
            Err(format!("The package manager failed: {}", err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("unknown error").trim()))
        }
    }
}

enum Cmd {
    Check { manual: bool },
    Install(Release, Kind),
}

fn worker(rx: mpsc::Receiver<Cmd>) {
    let net = Net::from_env();
    let mut kind: Option<Kind> = None;
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Check { manual } => {
                let kind = kind.get_or_insert_with(detect).clone();
                let res = fetch_latest(&net);
                post(move |u| u.checked(res, kind, manual));
            }
            Cmd::Install(rel, kind) => {
                let status = |s: &str| {
                    let s = s.to_string();
                    post(move |u| u.set_phase(Phase::Working(s)));
                };
                let res = match (pick(&rel, &kind), &kind) {
                    (None, _) => Err("This release has no download for your system".to_string()),
                    (Some(a), Kind::AppImage(path)) => install_appimage(&net, &rel, a, path, &status),
                    (Some(a), k) => install_package(&net, &rel, a, k, &status),
                };
                post(move |u| u.installed(res));
            }
        }
    }
}

// ---- UI-thread state ----------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Phase {
    Idle,
    Checking,
    UpToDate,
    Available,
    Working(String),
    Ready,
    Failed(String),
}

struct Ui {
    app: slint::Weak<AppWindow>,
    shared: Arc<Shared>,
    tx: mpsc::Sender<Cmd>,
    phase: Phase,
    release: Option<Release>,
    kind: Kind,
    /// The banner was closed for this version.
    dismissed: Option<String>,
    notified: Option<String>,
    /// Keeps the periodic "is a check due?" timer alive.
    _timer: slint::Timer,
}

thread_local! {
    static STATE: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

thread_local! {
    static ROW: RefCell<(String, String)> = RefCell::new(("Check now".into(), format!("Version {CURRENT}")));
}

fn with<R>(f: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    STATE.with_borrow_mut(|s| s.as_mut().map(f))
}

/// Runs `f` on the UI thread (from the worker).
fn post(f: impl FnOnce(&mut Ui) + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || {
        with(f);
    });
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl Ui {
    fn version(&self) -> &str {
        self.release.as_ref().map_or("", |r| r.version.as_str())
    }

    fn busy(&self) -> bool {
        matches!(self.phase, Phase::Checking | Phase::Working(_))
    }

    fn set_phase(&mut self, phase: Phase) {
        self.phase = phase;
        self.render();
    }

    fn render(&self) {
        let Some(app) = self.app.upgrade() else { return };
        let g = app.global::<Updates>();
        let v = self.version();
        let banner = match &self.phase {
            Phase::Available => self.dismissed.as_deref() != Some(v),
            Phase::Working(_) | Phase::Ready => true,
            _ => false,
        };
        let (text, action) = match &self.phase {
            Phase::Available => (
                format!("Sophon {v} is available — you have {CURRENT}."),
                if self.kind == Kind::Manual { "View release" } else { "Update now" }.to_string(),
            ),
            Phase::Working(s) => (s.clone(), "Working…".to_string()),
            Phase::Ready => (format!("Sophon {v} is installed. Restart to start using it."), "Restart now".to_string()),
            _ => (String::new(), String::new()),
        };
        ROW.set(self.row_text());
        g.set_banner(banner);
        g.set_text(text.into());
        g.set_action(action.into());
        drop(app);
        crate::settings_page::refresh();
    }

    fn row_text(&self) -> (String, String) {
        let v = self.version();
        match &self.phase {
            Phase::Idle => ("Check now".into(), format!("Version {CURRENT}")),
            Phase::Checking => ("Checking…".into(), format!("Version {CURRENT} — checking GitHub…")),
            Phase::UpToDate => ("Check now".into(), format!("Version {CURRENT} — you're up to date")),
            Phase::Available if self.kind == Kind::Manual => ("View release".into(), format!("Version {v} is available (built from source: download it from GitHub)")),
            Phase::Available => (format!("Update to {v}"), format!("Version {v} is available — you have {CURRENT}")),
            Phase::Working(s) => ("Working…".into(), s.clone()),
            Phase::Ready => ("Restart".into(), format!("Version {v} is installed — restart to use it")),
            Phase::Failed(e) => ("Check now".into(), e.clone()),
        }
    }

    fn start_check(&mut self, manual: bool) {
        if self.busy() {
            return;
        }
        self.set_phase(Phase::Checking);
        let _ = self.tx.send(Cmd::Check { manual });
    }

    fn checked(&mut self, res: Result<Release, String>, kind: Kind, manual: bool) {
        self.kind = kind;
        match res {
            Ok(rel) => {
                settings::update(|s| s.update_last_check = now());
                if is_newer(&rel.version, CURRENT) {
                    let version = rel.version.clone();
                    self.release = Some(rel);
                    self.set_phase(Phase::Available);
                    if self.notified.as_deref() != Some(&version) {
                        self.notified = Some(version.clone());
                        self.notify(&version);
                    }
                } else {
                    self.set_phase(Phase::UpToDate);
                    if manual {
                        crate::ui::toast(format!("Sophon {CURRENT} is the latest version"));
                    }
                }
            }
            // A failed background check stays quiet; a failed manual one says why.
            Err(e) if manual => {
                crate::ui::toast(e.clone());
                self.set_phase(Phase::Failed(e));
            }
            Err(_) => self.set_phase(Phase::Idle),
        }
    }

    /// A desktop notification, for when the window is in the tray and the
    /// banner can't be seen. Best effort: needs `notify-send`.
    fn notify(&self, version: &str) {
        if !(self.shared.hidden.load(Relaxed) || self.shared.in_tray.load(Relaxed)) {
            return;
        }
        let _ = Command::new("notify-send")
            .args(["--app-name=Sophon", "--icon=sophon", "Sophon update available"])
            .arg(format!("Version {version} is ready to install. Open Sophon to update."))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }

    fn installed(&mut self, res: Result<(), String>) {
        match res {
            Ok(()) => self.set_phase(Phase::Ready),
            Err(e) => {
                crate::ui::toast(e);
                self.set_phase(Phase::Available);
            }
        }
    }

    /// The primary action for the current state (banner button and the
    /// Settings button do the same thing).
    fn act(&mut self) {
        match self.phase.clone() {
            Phase::Available => match (&self.release, &self.kind) {
                (Some(r), Kind::Manual) => {
                    crate::actions::open_url(&r.page);
                }
                (Some(r), kind) => {
                    let (r, kind) = (r.clone(), kind.clone());
                    self.set_phase(Phase::Working("Starting the update…".into()));
                    let _ = self.tx.send(Cmd::Install(r, kind));
                }
                _ => {}
            },
            Phase::Ready => restart(&self.kind),
            Phase::Working(_) | Phase::Checking => {}
            Phase::Idle | Phase::UpToDate | Phase::Failed(_) => self.start_check(true),
        }
    }
}

/// Starts the new version and quits this one.
fn restart(kind: &Kind) {
    // current_exe() can't be trusted after the file was replaced.
    let exe = match kind {
        Kind::AppImage(p) => p.clone(),
        _ => PathBuf::from("/usr/bin/sophon"),
    };
    match Command::new(&exe).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
        Ok(_) => {
            let _ = slint::quit_event_loop();
        }
        Err(e) => crate::ui::toast(format!("Couldn't start {}: {e}", exe.display())),
    }
}

pub fn install(app: &AppWindow, shared: Arc<Shared>) {
    let (tx, rx) = mpsc::channel();
    if std::thread::Builder::new().name("updater".into()).spawn(move || worker(rx)).is_err() {
        return;
    }
    // Automatic checks: a coarse UI-thread timer asks "is one due?"; the
    // network work itself happens on the worker. Nothing runs while the
    // setting is off.
    slint::Timer::single_shot(FIRST_CHECK, auto_check);
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, TICK, auto_check);
    STATE.set(Some(Ui {
        app: app.as_weak(),
        shared,
        tx,
        phase: Phase::Idle,
        release: None,
        kind: Kind::Manual,
        dismissed: None,
        notified: None,
        _timer: timer,
    }));
    let g = app.global::<Updates>();
    g.on_act(|| {
        with(Ui::act);
    });
    g.on_notes(|| {
        with(|u| {
            if let Some(r) = &u.release {
                crate::actions::open_url(&r.page);
            }
        });
    });
    g.on_dismiss(|| {
        with(|u| {
            if u.phase == Phase::Available {
                u.dismissed = u.release.as_ref().map(|r| r.version.clone());
                u.render();
            }
        });
    });
}

fn auto_check() {
    let due = settings::get(|s| s.update_check && now().saturating_sub(s.update_last_check) >= DUE_SECS);
    if due {
        with(|u| u.start_check(false));
    }
}

/// Snapshot mode: pretends a newer release was found, without any network.
pub fn demo() {
    with(|u| {
        u.release = Some(Release { version: "9.9.9".into(), page: PAGES.into(), assets: Vec::new() });
        u.kind = Kind::Deb;
        u.set_phase(Phase::Available);
    });
}

// ---- Settings row --------------------------------------------------------------------

/// (button label, status line) for Settings → General → Updates. Kept in its
/// own cell, refreshed by `render`, because the settings page rebuilds while
/// the update state is still borrowed.
pub fn row() -> (SharedString, SharedString) {
    ROW.with_borrow(|(b, s)| (b.as_str().into(), s.as_str().into()))
}

/// The Settings button was pressed.
pub fn press() {
    with(Ui::act);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net() -> Net {
        Net { api: API.into(), local: false }
    }

    #[test]
    fn versions_compare_numerically() {
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("v1.0.0", "0.99.99"));
        assert!(is_newer("0.2.1", "0.2.0"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(!is_newer("0.1.9", "0.2.0"));
        assert!(!is_newer("garbage", "0.2.0"));
        assert!(!is_newer("1.2.3.4", "0.2.0"));
        assert_eq!(parse_version("v1.2.3-rc1"), Some([1, 2, 3]));
        assert_eq!(parse_version("2"), Some([2, 0, 0]));
    }

    #[test]
    fn sha256_matches_known_vectors() {
        let hash = |data: &[u8]| {
            let mut h = Sha256::new();
            h.update(data);
            h.finish()
        };
        assert_eq!(hash(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(hash(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        // Spans two blocks (56+ bytes forces an extra padding block).
        assert_eq!(hash(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"), "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1");
        // Fed in odd-sized pieces gives the same answer.
        let mut h = Sha256::new();
        for chunk in vec![b'a'; 1_000_000].chunks(777) {
            h.update(chunk);
        }
        assert_eq!(h.finish(), "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0");
    }

    #[test]
    fn sums_file_lookup() {
        let h = "a".repeat(64);
        let text = format!("{h}  Sophon-0.3.0-x86_64.AppImage\n{}  *other.deb\nnot a line\n", "B".repeat(64));
        assert_eq!(parse_sums(&text, "Sophon-0.3.0-x86_64.AppImage"), Some(h));
        assert_eq!(parse_sums(&text, "other.deb"), Some("b".repeat(64)));
        assert_eq!(parse_sums(&text, "missing"), None);
        assert_eq!(parse_sums("short  file", "file"), None);
    }

    const RELEASE: &str = r#"{
        "tag_name": "v0.3.0", "draft": false, "prerelease": false,
        "html_url": "https://github.com/MrNuggles/sophon/releases/tag/v0.3.0",
        "assets": [
            {"name": "Sophon-0.3.0-x86_64.AppImage", "browser_download_url": "https://github.com/MrNuggles/sophon/releases/download/v0.3.0/Sophon-0.3.0-x86_64.AppImage"},
            {"name": "sophon_0.3.0_amd64.deb", "browser_download_url": "https://github.com/MrNuggles/sophon/releases/download/v0.3.0/sophon_0.3.0_amd64.deb"},
            {"name": "sophon-0.3.0-1.x86_64.rpm", "browser_download_url": "https://github.com/MrNuggles/sophon/releases/download/v0.3.0/sophon-0.3.0-1.x86_64.rpm"},
            {"name": "SHA256SUMS", "browser_download_url": "https://github.com/MrNuggles/sophon/releases/download/v0.3.0/SHA256SUMS"},
            {"name": "evil.deb", "browser_download_url": "https://evil.example/evil.deb"},
            {"name": "../../etc/passwd", "browser_download_url": "https://github.com/MrNuggles/sophon/releases/download/v0.3.0/x"}
        ]}"#;

    #[test]
    fn release_parsing_drops_foreign_and_unsafe_assets() {
        let r = parse_release(RELEASE.as_bytes(), &net()).unwrap();
        assert_eq!(r.version, "0.3.0");
        let names: Vec<_> = r.assets.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["Sophon-0.3.0-x86_64.AppImage", "sophon_0.3.0_amd64.deb", "sophon-0.3.0-1.x86_64.rpm", "SHA256SUMS"]);
    }

    #[test]
    fn drafts_prereleases_and_junk_are_ignored() {
        assert!(parse_release(br#"{"tag_name":"v9.0.0","prerelease":true}"#, &net()).is_none());
        assert!(parse_release(br#"{"tag_name":"v9.0.0","draft":true}"#, &net()).is_none());
        assert!(parse_release(br#"{"tag_name":"nightly"}"#, &net()).is_none());
        assert!(parse_release(b"not json", &net()).is_none());
        // A page link that isn't ours is replaced by the project's own.
        let r = parse_release(br#"{"tag_name":"v1.0.0","html_url":"https://evil.example/x"}"#, &net()).unwrap();
        assert_eq!(r.page, "https://github.com/MrNuggles/sophon/releases/tag/v1.0.0");
    }

    #[test]
    fn asset_matches_install_kind() {
        let r = parse_release(RELEASE.as_bytes(), &net()).unwrap();
        if std::env::consts::ARCH == "x86_64" {
            assert_eq!(pick(&r, &Kind::Deb).unwrap().name, "sophon_0.3.0_amd64.deb");
            assert_eq!(pick(&r, &Kind::Rpm).unwrap().name, "sophon-0.3.0-1.x86_64.rpm");
            assert_eq!(pick(&r, &Kind::AppImage("/x".into())).unwrap().name, "Sophon-0.3.0-x86_64.AppImage");
        }
        assert!(pick(&r, &Kind::Manual).is_none());
    }

    #[test]
    fn package_install_commands() {
        let pkg = Path::new("/home/u/.cache/sophon/update/sophon_0.3.0_amd64.deb");
        let none = |_: &str| false;
        assert_eq!(install_command(&Kind::Deb, pkg, &none).unwrap(), ["pkexec", "apt-get", "install", "-y", pkg.to_str().unwrap()]);
        assert_eq!(install_command(&Kind::Rpm, pkg, &|p| p == "dnf").unwrap()[..3], ["pkexec", "dnf", "install"]);
        assert_eq!(install_command(&Kind::Rpm, pkg, &|p| p == "zypper").unwrap()[1], "zypper");
        assert_eq!(install_command(&Kind::Rpm, pkg, &none).unwrap()[1], "rpm");
        assert!(install_command(&Kind::Manual, pkg, &none).is_none());
    }

    /// End to end against a local "GitHub": check, download, verify, and
    /// replace an AppImage in place. Needs the system curl.
    #[test]
    fn appimage_update_end_to_end() {
        if !have("curl") {
            return;
        }
        let dir = std::env::temp_dir().join(format!("sophon-update-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let new = [b"\x7fELF".as_slice(), b"new version payload"].concat();
        let appimage = dir.join("Sophon-0.3.0-x86_64.AppImage");
        std::fs::write(&appimage, &new).unwrap();
        let mut h = Sha256::new();
        h.update(&new);
        std::fs::write(dir.join("SHA256SUMS"), format!("{}  Sophon-0.3.0-x86_64.AppImage\n", h.finish())).unwrap();
        let url = |f: &str| format!("file://{}/{f}", dir.display());
        let api = dir.join("latest.json");
        std::fs::write(
            &api,
            format!(
                r#"{{"tag_name":"v0.3.0","html_url":"https://github.com/MrNuggles/sophon/releases/tag/v0.3.0","assets":[
                {{"name":"Sophon-0.3.0-x86_64.AppImage","browser_download_url":"{}"}},
                {{"name":"SHA256SUMS","browser_download_url":"{}"}}]}}"#,
                url("Sophon-0.3.0-x86_64.AppImage"),
                url("SHA256SUMS")
            ),
        )
        .unwrap();
        let net = Net { api: format!("file://{}", api.display()), local: true };

        let rel = fetch_latest(&net).unwrap();
        assert!(is_newer(&rel.version, "0.2.0"));
        let asset = pick(&rel, &Kind::AppImage("/x".into())).cloned();
        if std::env::consts::ARCH != "x86_64" {
            return;
        }
        let asset = asset.unwrap();

        // The "installed" AppImage the update will replace.
        let target = dir.join("installed").join("Sophon.AppImage");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"\x7fELFold").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        install_appimage(&net, &rel, &asset, &target, &|_| {}).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), new);
        assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o111, 0o111);
        assert!(!target.with_file_name(".Sophon.AppImage.update").exists());

        // A tampered download is refused and the installed file is untouched.
        std::fs::write(&appimage, b"\x7fELFtampered").unwrap();
        std::fs::write(&target, b"\x7fELFold").unwrap();
        let err = install_appimage(&net, &rel, &asset, &target, &|_| {}).unwrap_err();
        assert!(err.contains("checksum"), "{err}");
        assert_eq!(std::fs::read(&target).unwrap(), b"\x7fELFold");
        assert!(!target.with_file_name(".Sophon.AppImage.update").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
