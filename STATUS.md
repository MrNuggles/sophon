# Sophon — Project Status

_Last updated: 2026-09-19_

A fast, beautiful, resource-light task manager for Linux (Rust + Slint), built
for a gamer's machine. This file tracks **what exists and where we are**. For how
it works, see [SPEC.md](SPEC.md); for features, [README.md](README.md); for the
security model, [SECURITY.md](SECURITY.md).

## Where we are right now

- **Tagged release:** `v0.1` on `main` — the full task manager (Processes,
  Performance, Settings) plus tray, admin helper, and the security pass.
- **Active branch:** `docker-modular` (ahead of `main`) — adds the modular panel
  system and the optional Docker tab. **Not yet merged into `main`** (pending a
  real-server tryout on the two LAN Docker hosts).
- **Build:** clean in both configs (`cargo build --release` and
  `--features docker`); no warnings.
- **Tests:** 30 passing (7 app + 13 core + 10 docker).
- **Size:** ~14k lines across three crates.
- **Footprint:** ~25 MB RAM, ~1.2% of one core with the window open, ~0.13% in
  the tray. Sampler ~2.6 ms CPU/tick.

## Build & run

```bash
# The core app (no network code):
cargo build --release && ./target/release/sophon
# With the optional Docker tab:
cargo build --release --features docker && ./target/release/sophon
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

**Settings** — live preview, search, per-setting reset; plain-text config.
Seven categories (General, Appearance, Resource use, Processes, Performance,
Space map, Tray).

**Tray & window** — close sends Sophon to the tray (Steam-style, no taskbar
entry); tray hover shows a live summary; Ctrl+Q / tray menu quits.

**Admin features (opt-in)** — one pkexec prompt starts a tiny root helper
(Sophon never runs as root); enables ending any process and per-app UDP network
via a kernel eBPF counter.

**Docker tab (optional, `--features docker`)** — connect to Docker on LAN
servers via a socket proxy; list containers (running + stopped) with ports and
server; sort by Ports / Name / State / Age / CPU / RAM; **expand a container**
for live CPU/memory graphs and full details; start / stop / restart. Live
updates; add/remove servers in-tab.

## Verified vs. not verified

- **Verified here:** all builds, 30 unit tests, every view rendered to PNG
  (snapshot mode), the local sampler against this machine, the Docker client
  end-to-end against a mock API (list / stats / inspect / start / stop, sorting,
  expand, error handling), and resource-use measurements.
- **Needs your hands (couldn't test from here):**
  - The two password-gated admin paths — killing a system process, and UDP
    per-app counting (the eBPF program loading as root).
  - The Docker tab against your **real** servers (mock only, so far).
  - Hover / drag / context-menu interactions that need a live mouse.

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
10. **Modular panels + Docker tab** (this branch) — panel registry, dependency-
    free Docker client, expandable rows with graphs, sorting.

## Open decisions / next steps

- [ ] **Merge `docker-modular` → `main`** once the real servers check out.
- [ ] Optional: confirmation dialog on container **stop** (currently none, to
      match Portainer/Docker Desktop).
- [ ] Packaging for distribution: install the binary to a **root-owned** path
      and ship a **polkit policy** (required before admin features are safe on
      shared machines — see SECURITY.md).
- [ ] Steam/Proton game grouping with cover art; app icons from the icon theme;
      pause/resume an app (`cgroup.freeze`); NVIDIA/Intel GPU backends;
      `--tui` mode; RPM packaging. (Full list in README's roadmap.)

## Repo map

```
crates/sophon-core     UI-free sampler (libc only)
crates/sophon-docker   optional Docker client (std only; feature "docker")
crates/sophon-app      Slint GUI (binary "sophon")
CLAUDE.md   the two rules + working notes      SPEC.md     full build blueprint
README.md   features + build                   SECURITY.md trust model
docs/docker.md  Docker + socket-proxy setup    STATUS.md   this file
```
