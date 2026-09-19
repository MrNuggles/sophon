//! Pretty names: maps processes to installed applications using `.desktop`
//! files, plus the systemd unit each process runs in (desktops launch every
//! app in its own `app-<id>-<n>.scope` or `app-<id>@<n>.service`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone)]
pub struct AppEntry {
    pub id: Arc<str>,
    pub name: Arc<str>,
    pub icon: Option<Arc<str>>,
}

/// Where a process belongs, mirroring Windows' Task Manager sections.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub enum Kind {
    /// Launched as an application (desktop app scope/service).
    App,
    /// Part of the user session but not an app (services, agents, shells).
    #[default]
    Background,
    /// System services and kernel threads.
    System,
}

/// Programs whose `Exec=` line says nothing about which app is running.
const WRAPPERS: &[&str] = &[
    "env", "sh", "bash", "zsh", "fish", "python", "python3", "perl", "ruby", "node", "java", "gjs",
    "flatpak", "snap", "wine", "wine64", "electron", "sudo", "pkexec", "xdg-open", "gtk-launch",
    "kioclient", "kioclient5", "kstart", "kstart5", "systemd-run", "dbus-launch", "gamemoderun",
];

/// Friendly descriptions for well-known processes that have no desktop entry.
const KNOWN: &[(&str, &str)] = &[
    ("systemd", "System and service manager"),
    ("kwin_wayland", "KWin window manager"),
    ("kwin_x11", "KWin window manager"),
    ("plasmashell", "Plasma desktop shell"),
    ("gnome-shell", "GNOME Shell"),
    ("mutter", "Mutter window manager"),
    ("Xwayland", "XWayland"),
    ("Xorg", "X server"),
    ("pipewire", "PipeWire media server"),
    ("pipewire-pulse", "PipeWire PulseAudio server"),
    ("wireplumber", "PipeWire session manager"),
    ("dbus-broker", "D-Bus message bus"),
    ("dbus-daemon", "D-Bus message bus"),
    ("NetworkManager", "Network manager"),
    ("systemd-journald", "System log"),
    ("systemd-logind", "Login manager"),
    ("systemd-udevd", "Device manager"),
    ("systemd-resolved", "DNS resolver"),
    ("systemd-oomd", "Out-of-memory killer"),
    ("polkitd", "Authorization manager"),
    ("gamescope", "Gamescope compositor"),
    ("gamemoded", "Feral GameMode"),
    ("steamwebhelper", "Steam web helper"),
    ("reaper", "Steam game launcher"),
    ("pressure-vessel-wrap", "Steam runtime container"),
    ("wineserver", "Wine server"),
    ("xdg-desktop-portal", "Desktop portal"),
    ("xdg-desktop-portal-kde", "KDE desktop portal"),
    ("xdg-desktop-portal-gnome", "GNOME desktop portal"),
    ("kded6", "KDE daemon"),
    ("ksmserver", "KDE session manager"),
    ("baloo_file", "KDE file indexer"),
    ("tracker-miner-fs-3", "GNOME file indexer"),
    ("sddm", "Login screen"),
    ("gdm", "Login screen"),
    ("firewalld", "Firewall"),
    ("chronyd", "Time sync"),
    ("upowerd", "Power manager"),
    ("bluetoothd", "Bluetooth"),
    ("cupsd", "Printing"),
    ("sophon", "Sophon"),
];

pub struct AppIndex {
    entries: Vec<AppEntry>,
    by_id: HashMap<String, usize>,
    by_exec: HashMap<String, usize>,
    known: HashMap<&'static str, Arc<str>>,
}

impl AppIndex {
    /// Scans the XDG application directories. Runs once at startup.
    pub fn load() -> Self {
        let mut index = Self {
            entries: Vec::new(),
            by_id: HashMap::new(),
            by_exec: HashMap::new(),
            known: KNOWN.iter().map(|&(k, v)| (k, Arc::from(v))).collect(),
        };
        for dir in application_dirs() {
            index.scan_dir(&dir, &dir);
        }
        index
    }

    pub fn empty() -> Self {
        Self { entries: Vec::new(), by_id: HashMap::new(), by_exec: HashMap::new(), known: HashMap::new() }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, i: usize) -> &AppEntry {
        &self.entries[i]
    }

    fn scan_dir(&mut self, root: &Path, dir: &Path) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for ent in rd.flatten() {
            let path = ent.path();
            if path.is_dir() {
                self.scan_dir(root, &path);
                continue;
            }
            if path.extension().is_none_or(|e| e != "desktop") {
                continue;
            }
            // Desktop file id: relative path with '/' -> '-', minus ".desktop".
            let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy();
            let id = rel.trim_end_matches(".desktop").replace('/', "-");
            if self.by_id.contains_key(&id.to_lowercase()) {
                continue; // earlier dirs take precedence
            }
            if let Some(parsed) = parse_desktop(&path) {
                self.insert(id, parsed);
            }
        }
    }

    fn insert(&mut self, id: String, d: Desktop) {
        let i = self.entries.len();
        let lower = id.to_lowercase();
        // Reverse-DNS ids ("org.kde.dolphin") are also reachable by their last part.
        if let Some(last) = lower.rsplit('.').next().filter(|l| *l != lower) {
            self.by_exec.entry(last.to_string()).or_insert(i);
        }
        if let Some(exec) = d.exec {
            // An entry named after its executable (steam.desktop for `steam`) beats others.
            let own = lower.rsplit('.').next() == Some(exec.as_str());
            if own {
                self.by_exec.insert(exec, i);
            } else {
                self.by_exec.entry(exec).or_insert(i);
            }
        }
        if let Some(class) = d.wm_class {
            self.by_exec.entry(class.to_lowercase()).or_insert(i);
        }
        self.by_id.insert(lower, i);
        self.entries.push(AppEntry { id: id.into(), name: d.name.into(), icon: d.icon.map(Into::into) });
    }

    /// Best app match for a process by executable/command name.
    pub fn for_process(&self, exe_base: Option<&str>, comm: &str) -> Option<usize> {
        let mut keys = exe_base.into_iter().chain(std::iter::once(comm));
        keys.find_map(|k| {
            let k = k.to_lowercase();
            if WRAPPERS.contains(&k.as_str()) {
                return None;
            }
            self.by_exec.get(&k).copied()
        })
    }

    /// App launched as the given systemd unit, e.g. `app-org.kde.konsole-1234.scope`.
    pub fn for_unit(&self, unit: &str) -> Option<usize> {
        let id = unit_app_id(unit)?;
        let lower = id.to_lowercase();
        if let Some(&i) = self.by_id.get(&lower) {
            return Some(i);
        }
        // Drop a launcher prefix: app-gnome-<id>, app-flatpak-<id>, app-kde-<id>.
        if let Some((_, rest)) = lower.split_once('-')
            && let Some(&i) = self.by_id.get(rest)
        {
            return Some(i);
        }
        self.by_exec.get(&lower).copied()
    }

    /// Friendly description for well-known system processes.
    pub fn known(&self, comm: &str) -> Option<Arc<str>> {
        self.known.get(comm).cloned()
    }
}

/// Extracts the application id from an XDG-style systemd unit name.
pub fn unit_app_id(unit: &str) -> Option<String> {
    let rest = unit.strip_prefix("app-")?;
    let rest = if let Some(r) = rest.strip_suffix(".scope") {
        // app-<id>-<random>.scope
        match r.rsplit_once('-') {
            Some((id, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_alphanumeric()) => id,
            _ => r,
        }
    } else {
        let r = rest.strip_suffix(".service").unwrap_or(rest);
        r.split('@').next().unwrap_or(r)
    };
    Some(rest.replace("\\x2d", "-"))
}

/// Section a process belongs in, from its cgroup path.
pub fn kind_for_cgroup(path: &str) -> Kind {
    let leaf = path.rsplit('/').next().unwrap_or("");
    if leaf.starts_with("app-") && (path.contains("/app.slice/") || leaf.ends_with(".scope")) {
        Kind::App
    } else if path.contains("/user@") || path.contains("/session-") {
        Kind::Background
    } else {
        Kind::System
    }
}

struct Desktop {
    name: String,
    icon: Option<String>,
    exec: Option<String>,
    wm_class: Option<String>,
}

fn parse_desktop(path: &Path) -> Option<Desktop> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut in_entry = false;
    let (mut name, mut icon, mut exec, mut class) = (None, None, None, None);
    let mut app = true;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            if in_entry {
                break;
            }
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        match k.trim() {
            "Name" => name = Some(v.trim().to_string()),
            "Icon" => icon = Some(v.trim().to_string()),
            // Launcher shortcuts (Steam games etc.) run another program; don't let them claim it.
            "Exec" if v.contains("://") => {}
            "Exec" => exec = exec_basename(v),
            "StartupWMClass" => class = Some(v.trim().to_string()),
            "Type" => app = v.trim() == "Application",
            "Hidden" if v.trim() == "true" => return None,
            _ => {}
        }
    }
    if !app {
        return None;
    }
    Some(Desktop { name: name?, icon, exec, wm_class: class })
}

/// `env FOO=1 "/usr/bin/steam" %U` -> `steam`. `None` for wrappers.
fn exec_basename(exec: &str) -> Option<String> {
    let prog = exec
        .split_whitespace()
        .map(|t| t.trim_matches('"'))
        .find(|t| *t != "env" && !t.contains('='))?;
    let base = prog.rsplit('/').next()?.to_lowercase();
    (!base.is_empty() && !WRAPPERS.contains(&base.as_str())).then_some(base)
}

fn application_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"));
    let data_dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    let mut dirs = vec![data_home.join("applications")];
    dirs.extend(data_dirs.split(':').filter(|d| !d.is_empty()).map(|d| Path::new(d).join("applications")));
    for flatpak in [data_home.join("flatpak/exports/share"), PathBuf::from("/var/lib/flatpak/exports/share")] {
        let d = flatpak.join("applications");
        if !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_ids() {
        assert_eq!(unit_app_id("app-com.anthropic.Claude-85404.scope").as_deref(), Some("com.anthropic.Claude"));
        assert_eq!(unit_app_id("app-steam@autostart.service").as_deref(), Some("steam"));
        assert_eq!(unit_app_id("app-org.kde.konsole@3f2a.service").as_deref(), Some("org.kde.konsole"));
        assert_eq!(unit_app_id("app-gnome-org.gnome.Nautilus-1234.scope").as_deref(), Some("gnome-org.gnome.Nautilus"));
        assert_eq!(unit_app_id("plasma-dolphin.service"), None);
    }

    #[test]
    fn kinds() {
        let base = "/user.slice/user-1000.slice/user@1000.service";
        assert_eq!(kind_for_cgroup(&format!("{base}/app.slice/app-steam@autostart.service")), Kind::App);
        assert_eq!(kind_for_cgroup(&format!("{base}/background.slice/plasma-dolphin.service")), Kind::Background);
        assert_eq!(kind_for_cgroup("/system.slice/NetworkManager.service"), Kind::System);
    }

    #[test]
    fn exec_parsing() {
        assert_eq!(exec_basename("env FOO=1 \"/usr/bin/steam\" %U").as_deref(), Some("steam"));
        assert_eq!(exec_basename("python3 /opt/x.py"), None);
    }
}
