# Sophon — Project Status

_Last updated: 2026-09-19_

A fast, beautiful, resource-light task manager for Linux (Rust + Slint), built
for a gamer's machine. This file tracks **what exists and where we are**. For how
it works, see [SPEC.md](SPEC.md); for features, [README.md](README.md); for the
security model, [SECURITY.md](SECURITY.md).

## Where we are right now

- **Tagged release:** `v0.2` on `main` — the full task manager (Processes,
  Performance, Settings) plus tray, admin helper, the security pass, the modular
  panel system, and the optional Docker tab.
- **Build:** clean in both configs (`cargo build --release` and
  `--features docker`); no warnings.
- **Tests:** 34 passing (7 app + 13 core + 5 net + 5 docker + 4 npm).
- **Size:** ~16k lines across five crates.
- **Footprint:** ~25 MB RAM, ~1.2% of one core with the window open, ~0.13% in
  the tray. Sampler ~2.6 ms CPU/tick.

## Build & run

```bash
cargo build --release && ./target/release/sophon
# Without any network code (no Docker tab at all):
cargo build --release --no-default-features && ./target/release/sophon
# Add Sophon to the app launcher (name + icon in the taskbar):
./target/release/sophon --install
```

## What works today

**Processes**
- Apps / Background / System sections (collapsible), grouped by systemd scope;
  tree and flat views. Pretty names from `.desktop` files.
- Per-process CPU, private memory, GPU, GPU memory, network down/up; heat-mapped
  cells; live per-row CPU sparkline. Column drag/reorder/resize/hide.
- Details pane (parent, clickable children, exe, cmdline, unit). End task / end
  tree / force kill / open file location. Search; Delete key.

**Performance**
- Rewind: 10 min (up to 24 h) history on every graph; synchronized hover
  crosshair with clock time; drag-to-zoom + pause; spike markers naming the
  culprit apps (click to jump).
- CPU (total / per-thread / per-core), memory + composition + zram, AMD GPU
  (per-engine, VRAM, temps, power, fan, clocks), disks, network.
- **Space map**: parallel treemap scan at idle priority; select / multi-select,
  zoom, breadcrumbs, hover, hold-for-path, right-click → open / Trash; colour by
  folder / file type / age; remembered scans.

**Settings** — lives outside the tab bar: a ☰ button beside the theme toggle
opens it as an overlay (Escape or a tab click closes it). Live preview, search,
per-setting reset; plain-text config. Eight categories (General, Appearance,
Resource use, Processes, Performance, Space map, Tray, **Tabs**) — Tabs turns
the optional tabs on or off without a restart (Performance on by default,
Docker off); a hidden tab fetches nothing at all. Processes is the permanent
dashboard tab.

**Tray & window** — close sends Sophon to the tray (Steam-style, no taskbar
entry); tray hover shows a live summary; Ctrl+Q / tray menu quits.

**Admin features (opt-in)** — one pkexec prompt starts a tiny root helper
(Sophon never runs as root); enables ending any process and per-app UDP network
via a kernel eBPF counter.

**Docker tab (optional, off by default)** — connect to Docker on LAN
servers via a socket proxy; list containers (running + stopped) with ports and
server; **collapsible server headers**; sort by Ports / Name / State / Age /
CPU / RAM; **expand a container** for live CPU/memory graphs and full details;
start / stop / restart (stopping asks first). Right-click a header to
show/hide its containers or rename the server. Live updates; add/remove servers
in-tab; hide the whole tab from Settings → Tabs (its poller then parks).

**Proxy tab (optional, off by default)** — talks to Nginx Proxy Manager's API
(admin port, default 81): every proxy host with its domains, forward target
and port, enabled state and certificate status (expiry shown, flagged red when
lapsed). Enable it in Settings → Tabs and connect with your NPM login right in
the tab — credentials live only in this machine's settings file (written 0600)
and the client is read-only (login, list hosts, list certificates — nothing
else). Refresh / forget buttons in the header; click a row for full details.
Ships with a mock NPM API for offline testing (`sophon-npm --example
npm-mock`).

## Verified vs. not verified

- **Verified here:** all builds, 30 unit tests, every view rendered to PNG
  (snapshot mode), the local sampler against this machine, the Docker client
  end-to-end against a mock API (list / stats / inspect / start / stop, sorting,
  expand, error handling), and resource-use measurements. The stop confirmation
  and collapsible headers were verified against the mock and a live server
  (read-only — snapshot mode never accepts the dialog; the mock's request log
  showed no control requests). Toggling the tab off and on from Settings → Tabs
  was verified live (tab bar rebuilds, data re-fetches; screenshot pair).
  The dashboard layout (Processes permanent, ☰ settings button, Tabs defaults)
  is likewise snapshot-verified from a fresh config. The Proxy tab was
  verified against the npm-mock: connect form → login → loaded table (the
  mock's log shows one login, then token reuse across polls).
- **Verified by you:** the Docker tab against both of your real servers
  (2026-09-19).
- **Needs your hands (couldn't test from here):**
  - The two password-gated admin paths — killing a system process, and UDP
    per-app counting (the eBPF program loading as root).
  - Hover / drag / context-menu interactions that need a live mouse (including
    the new header right-click menu, and Escape closing the stop dialog).

## Journey so far (high level)

1. **v0.1 core** — Rust + Slint spike; zero-alloc `/proc` + `/sys` sampler;
   Processes and Performance tabs; efficiency work to hit the budget.
2. **Space map** — parallel treemap scanner, interactive treemap, Trash.
3. **Two standing rules** written to `CLAUDE.md`: low resource use always, and
   everything interactive.
4. **Network per process**, then **removed the Pressure panel** (always 0 here).
5. **Tray + close-to-tray** (reworked to the Steam-style unmap after a first,
   buggy minimize approach).
6. **Settings system** — full config, live preview, all categories.
7. **Admin helper + eBPF** — opt-in root helper for privileged actions and UDP.
8. **Rewind** — interactive, explorable graphs with spike-culprit markers.
9. **Security audit** — no shell execution, bounds-checked parsing, isolated
   root helper; `SECURITY.md` + hardening.
10. **Modular panels + Docker tab** (v0.2) — panel registry, dependency-free
    Docker client, expandable rows with graphs, sorting; then collapsible
    server headers, stop confirmation, and a mock Docker API for offline
    testing (`sophon-docker --example mock`).
11. **Dashboard layout** — Processes is the permanent dashboard tab;
    Performance and Docker are toggles in Settings → Tabs (on/off by default
    respectively); Settings moved out of the tab bar to a ☰ header button;
    Docker became a default Cargo feature.

12. **Proxy tab** — Nginx Proxy Manager client + tab (the third optional tab):
    connect form in-tab, proxy hosts with certificates, credentials stored
    locally (settings file now 0600); HTTP/JSON layer extracted to
    `sophon-net` for both clients.

## Open decisions / next steps

- [ ] Packaging for distribution: install the binary to a **root-owned** path
      and ship a **polkit policy** (required before admin features are safe on
      shared machines — see SECURITY.md).
- [ ] Steam/Proton game grouping with cover art; app icons from the icon theme;
      pause/resume an app (`cgroup.freeze`); NVIDIA/Intel GPU backends;
      `--tui` mode; RPM packaging. (Full list in README's roadmap.)

## Repo map

```
crates/sophon-core     UI-free sampler (libc only)
crates/sophon-net     shared HTTP/JSON layer (std only)
crates/sophon-docker  Docker client (std only; default feature)
crates/sophon-npm     Nginx Proxy Manager client (std only; default feature)
crates/sophon-app     Slint GUI (binary "sophon")
CLAUDE.md   the two rules + working notes      SPEC.md     full build blueprint
README.md   features + build                   SECURITY.md trust model
docs/docker.md  Docker + socket-proxy setup    STATUS.md   this file
```
