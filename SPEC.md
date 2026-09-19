# Sophon — Complete Build Specification

This document is a from-scratch blueprint for **Sophon**, a Windows-style task
manager for Linux. It is detailed enough to rebuild the application as it exists
today. Read it top to bottom before writing code; the two design rules in §1
constrain every later decision.

Sophon targets Linux desktops (developed on Nobara / Fedora, KDE Plasma on
Wayland, AMD GPU, but written to work on KDE and GNOME, Wayland and X11). It is
written in **Rust** with the **Slint** GUI toolkit. There is no daemon and no
background service; when closed it uses nothing.

---

## 1. The two rules (non-negotiable)

Every feature obeys both.

### Rule 1 — Low resource usage, at all times
Sophon runs next to games. Its footprint is a feature, never traded for polish.
- **Budgets:** sampler < 0.5% of one core; whole app ~1% of one core with the
  window visible; ~0 when hidden/minimized/in-tray; < 40 MB PSS.
- No work nobody can see: don't sample or render for a hidden tab or window.
- No continuous animation. Animate only in response to input, briefly.
- Hot paths don't allocate: reuse buffers, parse bytes by hand, read invariant
  data once per process.
- Heavy work (disk scans) runs on demand, off the UI thread, at idle CPU/IO
  priority, with visible progress and a cancel.

### Rule 2 — Everything is interactive
- Every visible element has hover feedback; clickable things use a pointer cursor.
- Single click selects, double click opens/enters, right click shows a context
  menu. Shift+click adds to a selection; actions apply to the whole selection.
- Holding the pointer still shows a tooltip with full detail.
- Keyboard: Delete acts on the selection, Escape clears/closes, Ctrl+F searches,
  Ctrl+Q quits, Ctrl+Tab cycles tabs.
- Destructive actions confirm first and prefer reversible options (Trash, not delete).

---

## 2. Project shape

Cargo workspace, two crates:

```
sophon/
  Cargo.toml                     # workspace: resolver "3", edition 2024
  .cargo/config.toml             # env RUST_FONTCONFIG_DLOPEN = "on"
  crates/sophon-core/            # UI-free sampling engine (dep: libc only)
  crates/sophon-app/             # Slint GUI binary "sophon" (dep: sophon-core, libc, slint, fontique)
```

**Workspace `Cargo.toml`:**
```toml
[workspace]
resolver = "3"
members = ["crates/sophon-core", "crates/sophon-app"]
[workspace.package]
version = "0.1.0"
edition = "2024"
license = "GPL-3.0-or-later"
[profile.release]
opt-level = 3
lto = "fat"
codegen-units = 1
panic = "abort"
strip = true
[profile.dev.package."*"]
opt-level = 2
```

**`.cargo/config.toml`:**
```toml
[env]
RUST_FONTCONFIG_DLOPEN = "on"
```

**sophon-core deps:** `libc = "0.2"`. Nothing else. No async, no tokio.

**sophon-app deps:**
```toml
sophon-core = { path = "../sophon-core" }
libc = "0.2"
slint = { version = "1.18", default-features = false, features = [
  "std", "compat-1-2", "backend-winit", "renderer-software",
  "renderer-femtovg", "unstable-winit-030", "system-tray" ] }
# Not used directly; turns on runtime-loaded fontconfig inside Slint:
fontique = { version = "0.11", default-features = false, features = ["system", "fontconfig-dlopen"] }
[build-dependencies]
slint-build = "1.18"
```

**Build with no `-devel` packages.** fontconfig, Wayland and X11 are loaded at
runtime. The two mechanisms that make this work and must be preserved:
`RUST_FONTCONFIG_DLOPEN=on` (env), and fontique's `fontconfig-dlopen` feature
(fontique is pulled in only to flip that feature inside Slint's font stack).
Minimum Rust 1.88 (uses stable let-chains: `if let Some(x) = a && cond`).

**`crates/sophon-app/build.rs`:**
```rust
fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("Slint build failed");
}
```
Slint's `.slint` files are compiled to Rust at build time; `slint::include_modules!()`
in `main.rs` pulls in the generated code. Style is "fluent".

Rendering defaults to Slint's **software renderer with partial repaints**
(`SLINT_BACKEND=winit-software`), the lightest option for a UI that changes ~1
Hz. A setting can switch to `winit-femtovg` (GPU). `main` sets `SLINT_BACKEND`
from settings before creating the window unless already set in the environment.

---

## 3. Architecture overview

**One sampler thread** produces an immutable `Snapshot` per tick and hands it to
the UI thread via `slint::invoke_from_event_loop`. The UI thread never blocks on
I/O. All sampling state lives on the sampler thread; all UI/model state lives on
the UI thread in a `thread_local!` `RefCell<Option<Ui>>`.

**Data flow:**
```
sampler thread: Sampler::sample(with_processes) -> Snapshot
      | invoke_from_event_loop
UI thread: ui::apply(Snapshot) -> build Slint models -> render
```

The sampler is paced by a channel `recv_timeout`: it sleeps for the update
interval, but a `wake` sender lets the UI thread kick it early (e.g. when a tab
becomes visible, or settings change the interval).

**Shared atomics** (`struct Shared`, an `Arc` held by both threads):
- `tab: AtomicU8` — current tab (0 Processes, 1 Performance, 2 Settings).
- `hidden: AtomicBool` — window not visible.
- `in_tray: AtomicBool` — closed to tray (unmapped).
- `interval_ms: AtomicU64`, `tray_interval_ms: AtomicU64` — sampling cadence.
- `wake: mpsc::Sender<()>`.

**Sampler loop logic (`main.rs`):**
- `with_processes = hidden || tab==0 || tick % 2 == 0`. The process table (the
  costliest part) refreshes every tick while its tab is visible, every other
  tick otherwise (so Rewind can still name spike culprits), and at the slow
  hidden cadence.
- Interval: `interval_ms` when visible, `tray_interval_ms` when hidden. `0` =
  paused (sleep 1 h until woken).

---

## 4. `sophon-core` — the sampling engine

No Slint, no UI. Reusable by a future TUI. Everything reads `/proc` and `/sys`
directly and parses bytes by hand.

### 4.1 `sys.rs` — syscall layer (zero-alloc)
- **`CPath`** — a NUL-terminated path built on a 256-byte stack buffer.
  `CPath::pid(pid, b"/stat")` builds `"<pid>/stat"`. Methods: `push`,
  `push_u32` (manual integer formatting, no `format!`), `as_cstr`.
- **`Dir`** — an owned directory fd. All reads are `openat`-relative to it:
  `read(path, &mut Vec<u8>)` (clears and refills the buffer, growing only if
  needed), `read_u64`, `read_link`, `owner` (uid via `fstatat`), `exists`
  (`faccessat`), `entries()` -> `DirStream`.
- **`DirStream`** — wraps `fdopendir`; `rewind()` + `next_name()` iterate
  entries reusing one buffer; `for_each_number(f)` calls `f` for each numeric
  entry (pids in `/proc`, fds in `fd/`). Marked `unsafe impl Send` (used only by
  the owning thread).
- Helpers: `clk_tck()`, `page_size()`, `find_hwmon(names)` (locate a hwmon dir
  by its `name` file, e.g. `k10temp`), `device_hwmon(dir, sub)` (a device's own
  hwmon, handling the nested `device/hwmon/hwmonN` layout), `read_string`,
  `thread_cpu_us()` (`clock_gettime(CLOCK_THREAD_CPUTIME_ID)`, for self-profiling).

The point of `Dir`/`openat`: hold `/proc` open once, then read `<pid>/stat` etc.
relative to it, avoiding repeated path resolution.

### 4.2 `parse.rs` — byte parsers
`trim`, `try_u64`/`u64` (leading digits, wrapping), `f32` (decimal), `fields`
(whitespace split, non-empty), `lines`, `key_value` (`"Key: value"`),
`size_with_unit` (`"114844 KiB"`, `"2 MiB"`, `"0"` -> bytes). No regex, no alloc.

### 4.3 `history.rs` — ring buffers
`Ring<const N: usize = HISTORY>` — fixed-capacity circular buffer of `f32`.
`push`, `iter()` (oldest→newest, `ExactSizeIterator`), `last()`, `back(k)` (k-th
newest), `max()`. **`HISTORY = 600`** (10 min at 1 Hz). Per-process sparkline
rings use `Ring<SPARK>`, **`SPARK = 60`**. This type is `Copy`; snapshots clone
freely.

### 4.4 Per-subsystem collectors
Each owns a `State` struct (cloned into the Snapshot) and a `sample()` that
reuses a shared scratch `Vec<u8>`.

**`cpu.rs`** — `/proc/stat`:
- Parse `cpu` and `cpuN` lines; each is `user nice system idle iowait irq
  softirq steal ...`. Busy = total − idle − iowait. Usage = Δbusy/Δtotal ×100.
- Per-thread usage (one `Ring` each), overall usage + history, ctxt switches/s,
  procs_running/blocked from `/proc/stat`.
- Model name + core count from `/proc/cpuinfo`; thread count from counting
  `cpuN` lines.
- **`core_of: Vec<u32>`** — physical core id per thread, from
  `/sys/devices/system/cpu/cpuN/topology/core_id` (for the per-core view).
- Clocks: `cpuN/cpufreq/scaling_cur_freq`; max from `cpuinfo_max_freq`.
- Temp: hwmon `k10temp`/`zenpower`/`coretemp`/`cpu_thermal`, `temp1_input`/1000.
- Package power: `/sys/class/powercap/intel-rapl:0/energy_uj` (µJ, wraps at
  `max_energy_range_uj`); watts = Δenergy/Δt. Often root-only → optional.
- Uptime from `/proc/uptime`.

**`mem.rs`** — `/proc/meminfo` (all values ×1024 to bytes): total, free,
available, buffers, cached (+SReclaimable), shmem, dirty, swap total/free,
Committed_AS, CommitLimit. `used = total − available`. zram from
`/sys/block/zram*/mm_stat` (fields: orig_data_size, compr_data_size,
mem_used_total): reports original vs used (compression ratio). History = used%.

**`disk.rs`** — `/proc/diskstats` per whole physical device (skip partitions,
`loop`, `ram`, `zram`, `dm-`, `sr`; discovered via `/sys/block`):
- Fields used: reads, sectors read, ms reading, writes, sectors written, ms
  writing, ms doing I/O. Throughput = Δsectors×512/Δt. **Active time** = Δ(ms
  doing I/O)/(Δt×10), capped 100% (Windows' "active time"). **Response time** =
  Δms/Δios. Model from `/sys/block/<d>/device/model`; rotational, removable.
  Temp from the device's hwmon `temp1_input`, read only every 30 ticks (SMART
  cost). Kind: NVMe / SSD / HDD / Removable.

**`net.rs`** — `/proc/net/dev` per interface (skip `lo`): rx/tx bytes since boot
→ rate = Δbytes/Δt. Physical if `/sys/class/net/<i>/device` exists; wireless if
`.../wireless` exists; up/speed from `operstate`/`speed` when physical & up.

### 4.5 `apps.rs` — pretty names
Maps processes to installed applications and friendly names.
- **`AppIndex::load()`** scans XDG application dirs (`$XDG_DATA_HOME/applications`,
  each of `$XDG_DATA_DIRS/applications`, flatpak exports). Parses each `.desktop`
  `[Desktop Entry]`: Name, Icon, Exec (→ basename, skipping wrappers like `env`,
  `sh`, `python`, `flatpak`, `wine`, `gamemoderun`, …; entries with `://` in Exec
  are launcher shortcuts and don't claim an executable), StartupWMClass, Type,
  Hidden. Indexed by desktop-file id, by exec basename, and by WM class. An entry
  named after its own executable (`steam.desktop` → `steam`) wins ties.
- **`for_process(exe_base, comm)`** — best app match by executable/command name.
- **`for_unit(unit)`** — app from a systemd unit name (see below).
- **`known(comm)`** — a hand table of friendly names for well-known system procs
  (systemd, kwin_wayland, plasmashell, pipewire, NetworkManager, reaper="Steam
  game launcher", …).
- **`unit_app_id(unit)`** — extracts the app id from an XDG systemd unit:
  `app-com.anthropic.Claude-85404.scope` → `com.anthropic.Claude`,
  `app-steam@autostart.service` → `steam`. Handles `\x2d` escapes.
- **`kind_for_cgroup(path)`** — section from cgroup path: `App` (leaf starts
  `app-` and in `app.slice`/`.scope`), `Background` (under `user@`/`session-`),
  else `System`. `enum Kind { App, Background, System }` derives `Hash, Ord`.

**Why systemd scopes:** KDE/GNOME launch each app in its own
`app-<id>-<n>.scope`, so grouping by cgroup gives an accurate "Firefox = 14
processes" rollup for free.

### 4.6 `process.rs` — the process table
One pass over `/proc` per refresh. **Per process per refresh reads only `stat`
(+`statm` when RSS changed)**; everything invariant is read once when the pid is
first seen or re-execs.

`struct Process` (cloned into the Snapshot) fields: pid, ppid, uid, user (Arc<str>),
name (kernel comm), exe, cmdline, pretty (Option), icon, unit, group (display
name of the app/unit), kind, kernel_thread, state, threads, started_s, cpu,
cpu_history (`Ring<SPARK>`), mem_private, mem_rss, gpu, gpu_mem, net_rx, net_tx.

- **`/proc/<pid>/stat` parse:** comm is between the first `(` and the **last**
  `)` (it can contain spaces/parens). After the last `)`, fields by index:
  0 state, 1 ppid, 6 flags (`PF_KTHREAD = 0x00200000`), 11 utime, 12 stime,
  17 num_threads, 19 starttime, 21 rss (pages). CPU% = Δ(utime+stime)/clk_tck /
  Δt / threads ×100 (whole-machine mode).
- **`statm`** read only when the `stat` rss field changed since last time.
  `mem_private = (resident − shared) × page_size` (anonymous RSS, ≈ Windows'
  private working set); `mem_rss = resident × page_size`.
- Invariant fields (read once): exe (`readlink /proc/<pid>/exe`, strip
  " (deleted)"), cmdline (NUL→space), cgroup (`0::/…` line → unit = leaf),
  owner uid, then resolve pretty name / app group / kind / icon via `AppIndex`.
- **Adaptive polling:** a process idle (no CPU used) for `IDLE_AFTER = 3`
  refreshes is then re-read only every `IDLE_EVERY = 3` refreshes, staggered by
  `(tick + pid) % IDLE_EVERY`, pushing 0 to its sparkline in between. Kernel
  threads re-read every `KTHREAD_EVERY = 5`. CPU% math uses the actual elapsed
  time between *this process's* reads, so backoff doesn't distort it.
- `apply_gpu(usage)` and `apply_net(usage)` merge the per-process GPU/network
  maps in. `user_pids(out)` returns sorted non-kernel pids (candidates for fd
  scanning).

### 4.7 `gpu/` — GPUs (vendor-neutral)
- **`GpuDevice` trait**: `state()`, `state_mut()`, `sample(buf)`. `detect()`
  walks `/sys/class/drm/card*` (cardN only, not connectors/render nodes), reads
  the driver via the `device/driver` symlink, and constructs a backend.
- **`amdgpu.rs`** — device-wide stats from amdgpu sysfs: `gpu_busy_percent`,
  `mem_busy_percent`, `mem_info_vram_used/total`, `mem_info_gtt_used`. hwmon:
  temps `temp1/2/3_input` (edge/junction/mem), `power1_average`, `fan1_input`,
  `freq1_input`/`freq2_input` (core/mem MHz), `power1_cap`. Marketing name from
  `/usr/share/hwdata/pci.ids` via vendor/device ids (`"Navi 21 [Radeon RX 6950
  XT]"` → `"Radeon RX 6950 XT"`).
- NVIDIA/Intel are future backends behind the same trait (NVML via dlopen, i915/xe).
- **`fdinfo.rs`** — **per-process GPU usage from DRM fdinfo**, vendor-neutral.
  For each GPU fd (found by the shared fd walker, §4.8), read
  `/proc/<pid>/fdinfo/<fd>`: `drm-client-id`, `drm-pdev` (matches device
  `pci_slot`), `drm-engine-<class>` (cumulative ns; classes mapped to
  3D/Compute/Copy/Video-decode/Video-encode), `drm-resident-vram/gtt` (fallback
  `drm-memory-*`). **De-duplicate by (device, client-id)** — the same client is
  reachable through dup'd fds; counting each once avoids the classic double-count
  bug. Busy% per engine = Δns/Δt_ns ×100. Idle clients (`IDLE_AFTER=3`) re-read
  every `IDLE_EVERY=5` ticks. Produces `usage: HashMap<pid, ProcGpu{busy, vram,
  gtt}>` and per-device engine totals (fed into `GpuState.engines`).

### 4.8 `fdwalk.rs` — one shared fd walk
Reading every fd link of every process is the expensive part, and **both** GPU
attribution (needs `/dev/dri/*` fds) and network attribution (needs
`socket:[inode]` fds) need it. So it's done **once**, on a rolling schedule:
- Each process rescanned every `RESCAN_EVERY = 30` ticks normally, `RESCAN_HOT_EVERY
  = 5` if "hot" (a GPU client or recently network-active). The caller sets the
  `hot` set each tick. `hot_now` forces an immediate hot rescan; `sweep` forces
  everyone (used when network traffic can't yet be attributed).
- Output each tick: `dri: Vec<(pid, fd)>` and `sockets: Vec<(inode, pid)>`.
The GPU scanner takes the dri fds; netproc takes the socket inode→pid map.

### 4.9 `netproc.rs` — per-process network
- **Default (no admin): TCP only.** Uses the kernel's **sock_diag netlink**
  interface (`NETLINK_SOCK_DIAG`, `SOCK_DIAG_BY_FAMILY`, request
  `inet_diag_req_v2`). A full dump every `DUMP_EVERY = 5` ticks finds all TCP
  sockets in data-carrying states; between dumps, sockets with recent traffic
  are looked up individually by their `inet_diag_sockid`. From each reply parse:
  `idiag_inode` (offset 68 in `inet_diag_msg`), `idiag_uid` (offset 64; only our
  own uid is attributable), the socket **cookie** (last 8 bytes of the 48-byte
  sockid, msg offset 44), and from the `INET_DIAG_INFO` attribute the
  `tcp_info` fields **`tcpi_bytes_received`** (offset 128) and
  **`tcpi_bytes_acked`** (offset 120). Rates = Δbytes/Δt. Sockets → pids via the
  fd walker's inode→pid map. UDP has no byte counters here, so UDP isn't counted
  in this mode (the UI header tooltip says so). Full dumps walk the whole TCP
  hash table (can be milliseconds), hence the 5-tick spacing + targeted lookups.
- **With admin: eBPF byte counting for everything.** When
  `set_ebpf(true)`, sample() only does identification dumps (TCP **and** UDP,
  no byte attrs) to keep cookie→inode current, and `apply_ebpf(counters, dt)`
  attributes per-socket byte deltas (keyed by cookie) to pids via cookie→pid
  (built in `learn()` from the fd-walk inode→pid + each socket's cookie).

### 4.10 `space.rs` — disk space scanner (treemap data)
- **`scan(root, &Progress, &ScanOptions) -> ScanResult`** — parallel directory
  walk that does not cross mount points (mounts under root become skip dirs;
  plus user-excluded folders). `ScanOptions { threads, idle, include_hidden,
  exclude }`. `idle` sets the calling thread to nice 19 + `ioprio` idle class
  (via `setpriority`/`ioprio_set` syscalls) so a scan yields to games. Uses
  `std::thread::scope`; a shared atomic slot count caps live threads; subdirs
  spawn a thread when a slot is free, else recurse inline. `Progress` has atomic
  files/dirs/bytes/denied counters + a `cancel` flag for live UI.
- **Node tree kept small:** each folder keeps its `KEEP_FILES = 48` largest
  files ≥ `KEEP_FILE = 1 MiB`; the rest fold into one "N smaller items" node.
  Folders smaller than `KEEP_DIR = 1 MiB` fold into their parent's summary. So
  1.4M files → ~40k nodes (~12 MB). Sizes are **allocated** bytes
  (`blocks×512`, like `du`). Hard links counted once (dev+inode set).
- Each `Node`: name, size, kind (Dir/File/SmallFiles/Unreadable/Unaccounted),
  files, **`cats: [u32; 7]`** (KiB per file-type category, for "colour by type"),
  **`newest: u32`** (newest mtime, for "colour by age"), children (largest first).
  `category(name)` classifies by extension into Video/Audio/Images/Archives/
  Documents&code/Programs&game-data/Other. `dominant()` = biggest category.
- At the mount root, compares scanned size to `statvfs` used space; any gap
  becomes an "Unaccounted" node (snapshots, unreadable dirs, other subvolumes).
- **`Node::remove(path)`** removes a node by child-index path, subtracting its
  size/files/cats from every ancestor (so the map updates after a Trash without
  a rescan).
- **`mounts()`** parses `/proc/self/mountinfo` for block-device-backed mounts:
  path (with `\040` unescaping), fs type, device, whole disk, total/free from
  `statvfs`.
- **`save`/`load`** — compact binary cache of a ScanResult (magic `SOPHSP01`,
  timestamp, then a recursive node dump), so scans can be remembered between
  runs.

### 4.11 `sampler.rs` — orchestration
`Sampler::sample(with_processes) -> Snapshot`:
1. Compute `dt` from a monotonic clock.
2. Sample cpu, mem, net, disk, each GPU device.
3. Refresh the process table if `with_processes` (always on tick 1).
4. **One fd walk** (`fds.walk`) with hot = GPU-client pids ∪ network-active pids.
5. GPU: feed dri fds to the fdinfo scanner, sample it, roll each device's engine
   history.
6. Network: sample netproc, `learn()` socket owners; if an eBPF counter source
   is set (admin), `apply_ebpf` else the tcp_info path.
7. Merge GPU + net usage into the process table.
8. Push `now` onto the shared `times` ring (so every history ring shares one
   timeline for Rewind).
9. Measure Sophon's own CPU/RSS (`self/stat`, `self/statm`) into `OwnUsage`.
10. Clone all state into an immutable `Snapshot` (processes as `Arc<Vec<Process>>`
    so cloning is cheap).

`set_ebpf_source(f)` installs a `Fn() -> Option<Vec<(cookie, rx, tx)>>` the
sampler calls each tick; the admin client provides it.

---

## 5. `sophon-app` — the GUI

Slint UI defined in `ui/*.slint` (compiled at build time), driven from Rust in
`src/*.rs`. Global singletons in `.slint` (`Theme`, `Hover`, `Space`,
`SettingsPage`, `Cols`) are the bridge: Rust sets their properties and installs
their callbacks.

### 5.1 Slint files (`ui/`)
- **`theme.slint`** — `export global Theme`. `mode` (0 system/1 light/2 dark),
  `dark` (derived, follows `Palette.color-scheme` in system mode). Colour tokens
  (bg, surface, surface-2, hover, selected, border, grid, graph-bg, text, dim,
  faint, accent, danger, heat, and per-metric cpu/mem/gpu/disk/net) as light/dark
  pairs. **Customisation `in` properties set from Rust:** `accent-custom` +
  `accent-is-custom`, `heat`, `heat-intensity`, per-metric colours, `font-scale`,
  `round` (corner multiplier), `anim` (animation-speed multiplier: 1/0.5/0),
  `row-height`, `graph-style` (0 gradient/1 filled/2 line/3 bars), `line-scale`,
  `graph-grid`. Every literal `font-size`, `border-radius`, and animation
  `duration` in all `.slint` files is multiplied by the matching Theme value, so
  appearance settings apply live.
- **`widgets.slint`** — reusable components: `Graph` (the interactive time-series
  graph, §5.5), `Avatar`, `Icon`/`IconButton` (24-viewbox stroked SVG paths),
  `Btn`, `Toggle`, `SearchBox`, `Tabs`, `Logo`; and globals `Hover` (Rewind) and
  struct `Series {line, area}` / `Marker {x, tip, pid}`.
- **`processes.slint`** — the process table: `ProcRow`/`ProcDetail`/`Totals`/
  `Link`/`HeadInfo` structs, `global Cols` (column geometry set from Rust), the
  `RowView` (absolutely-positioned cells so columns can reorder/resize/hide),
  header cells (drag to reorder, drag left edge to resize, right-click menu, hold
  for hint tooltip), the `DetailPane`, and `ProcessesView`.
- **`performance.slint`** — `Tile`/`Mini`/`Segment`/`Stat`/`Chart`/`PerfDetail`
  structs, device sidebar with live mini-graphs (each a `Graph` with a `key`),
  `RangeChip`s, `ChartView`, `MiniGrid` (per-thread/core/engine), `SegmentBar`
  (memory composition), `StatGrid`, and `PerformanceView`. Includes
  `SpaceSection` from space.slint.
- **`space.slint`** — the treemap: `Block`/`LegendItem` structs, `global Space`,
  `TreeMap` (blocks with hover glow that fades over 500 ms, hold-for-tooltip,
  left-click select, double-click zoom, right-click context menu, drag select),
  breadcrumbs, drive chips, scan/progress states, the Trash confirmation overlay,
  and the legend.
- **`settings.slint`** — `SettingItem` struct (a tagged union: kind 0 toggle,
  1 choice, 2 colour, 3 slider, 4 folder-list, 5 order-list, 6 button,
  7 heading), `global SettingsPage`, generic widgets (`Switch`, `Segmented`,
  `Swatches`, `Slider`, `OrderList` with drag handles + checkboxes, `PathList`),
  `SettingRow` (grows to content), and `SettingsView` (category sidebar + search).
- **`app.slint`** — the `AppWindow` (header with logo, `Tabs`, `SearchBox`,
  theme toggle; the three tab bodies; status bar; toast; confirmation dialog) and
  the `Tray` (`SystemTrayIcon` with a live-summary tooltip and Open/Quit menu).
  Re-exports every struct/global Rust needs. Keyboard handling (Ctrl+F/Q/Tab,
  Delete, Escape) lives in a root `FocusScope`.

### 5.2 `main.rs`
- CLI: `--helper` (run the privileged helper, §7), `--install`/`--uninstall`
  (launcher entry + icon), `--tray` (start hidden), `--help`.
- Sets `SLINT_BACKEND` (software/femtovg from settings) and `SLINT_SCALE_FACTOR`
  (UI scale) before creating the window. Calls `slint::set_xdg_app_id("sophon")`
  **after** `AppWindow::new()` (platform must be initialised) and before showing
  (so the compositor matches the window to `sophon.desktop` for taskbar
  name/icon). Sets the window icon from `icon::image(64)`.
- Installs `ui`, `space_ui`, `settings_page`. Wires the **close-to-tray**
  behaviour: `on_close_requested` → if `close_to_tray`, `ui::to_tray` +
  `CloseRequestResponse::HideWindow` (unmaps — no taskbar entry), else quit. The
  `Tray` `on_open` re-maps the window (Wayland apps can't raise themselves, so
  `hide()`+`show()` brings a fresh, fronted window; it also restores the saved
  size). `Focused(true)` and `Occluded` window events, plus a 1 Hz stall check
  (Wayland stops presenting frames for hidden windows), drive `hidden`.
- Spawns the sampler thread; `set_ebpf_source(admin::net_counters)`. Uses
  `run_event_loop_until_quit()` so closing the window keeps the process alive in
  the tray. Quit only via Ctrl+Q or the tray menu.

### 5.3 `ui.rs` — the biggest module
Owns `struct Ui` (UI-thread state) in a thread-local. `apply(Snapshot)` stores
the snapshot, records a "moment" (top CPU/GPU processes, for spike culprits),
feeds long-history tiers, and renders the current tab. Key responsibilities:
- **Processes render:** filter (kernel/other-users/search), then group by the
  `grouping` setting: **By app** (sections → app groups → processes, with
  aggregate rows), **Tree** (parent→child, collapsible), or **Flat**. Sort by
  the active column. Build `ProcRow`s with heat-mapped, formatted cells honouring
  unit/CPU-mode/name-style settings; build the details pane (`detail`/
  `group_detail`) with parent, clickable children, exe, cmdline, network, etc.
- **Column geometry:** `update_columns`/`update_heads` compute per-column widths
  and right-offsets from the settings (order, hidden, widths) and push them to
  the `Cols` global and `HeadInfo` model; `drop_column` handles header drag-drop.
- **Selection & actions:** single-click selects, double-click does the
  configured action, right-click menu; Delete does the configured action.
  Destructive actions gate through a confirm dialog per the `confirm_end`
  setting. `run_action` calls `actions::signal`/`tree`/`open_location`; denials
  retry through the admin helper.
- **Performance render:** builds the device tile list (respecting
  order/hidden/virtual-NIC settings), reports it to `settings_page::set_tiles`,
  builds the selected device's `PerfDetail` (main + optional second chart, minis
  grid for CPU threads/cores or GPU engines, memory composition segments, stats),
  and computes spike **markers**.
- **Rewind:** a `Timeline{times, start, end}` maps the visible time window over
  the shared sample ring. `Hover` global callbacks: `probe(key, x)` returns the
  tooltip for any graph at a fraction (looked up by nearest sample, with clock
  time), `zoom(from,to)` pauses into a sub-range, `set_range`/`reset`,
  `marker(pid)` jumps to a process. Charts carry a `key` string; every graph on
  the page shares the crosshair via `Hover.x`. Long ranges (1 h/24 h) read from
  coarse **tiers** (`Tier` at 10 s and 60 s buckets, max-pooled) built in
  `feed_long_history`, kept only when the history setting asks.
- **Tray/status text:** builds the tray tooltip + (while hidden) the window
  title from the enabled tray-summary metrics.

### 5.4 `graph.rs` — graph geometry
Turns rings into SVG path commands (`Series{line, area}`) in a 1000×1000
viewbox, right-aligned (newest at the right). `Timeline::series` supports
arbitrary time windows with **max-pooling** into ≤300 buckets (spikes survive),
`value_at` for hover, `samples` for markers. Bar style (`graph_style==3`) emits
rectangles in the `area` path. Colours: `rgb(hex)`, `tint(name)` (stable
per-name hue via FNV hash → HSV) for app avatars.

### 5.5 The interactive `Graph` (in widgets.slint)
Props: `key` (empty = non-interactive), `a`/`b` series, colours, `markers`.
Draws area+line (or bars) per Theme style. Overlays: a **crosshair** at
`Hover.x` on every graph sharing the page; a **drag-selection** rectangle;
**spike markers** (triangles, hover shows the culprit tooltip, click →
`Hover.marker(pid)`); a **probe tooltip** at the crosshair (`Hover.probe`). A
`TouchArea` sets `Hover.x`/`Hover.graph` on move, starts drag on press, calls
`Hover.zoom` on release if dragged, `Hover.reset` on double-click.

### 5.6 `space_ui.rs` — treemap UI
Owns the scan lifecycle (start on the sampler-independent scan thread at idle
priority, live progress via the atomic `Progress`, cancel), the **squarified
treemap layout** (Bruls/Huizing/van Wijk algorithm: lay children into a rect
keeping blocks near-square; nested folders draw their children inside a header
strip), colour modes (folder hue / file-type / age, with a legend), zoom +
breadcrumbs, multi-select (click, Shift+click, select-roots dedup), and Trash
(via `gio trash`, with confirmation, refusing home/drives/already-trashed;
updates the tree in place via `Node::remove`). Remembers scans via
`space::save`/`load` under `~/.cache/sophon/space` when enabled.

### 5.7 `settings.rs` + `settings_page.rs`
- **`settings.rs`** — `struct Settings` (every option), `Default`, and a
  plain-text config at `~/.config/sophon/settings.conf` (`section.key = value`
  per line; unknown keys ignored, missing keep defaults, so it survives version
  changes both ways). `get(f)` / `update(f)` (thread-local, autosaves).
- **`settings_page.rs`** — builds the `SettingItem` rows for each of 7 categories
  (General, Appearance, Resource use, Processes tab, Performance tab, Space map,
  Tray) from the current `Settings`, with per-item hints and a `modified` flag
  (shows a reset button). `apply_all()` pushes settings into the `Theme` global,
  the sampler intervals (via `Shared` atomics), formatter flags (°F), and graph
  style, then re-renders. Colour pickers use a fixed swatch palette. Handles
  drag-reorder/hide for columns and tiles, folder exclusion (native
  kdialog/zenity picker), and the desktop accent lookup (KDE `kdeglobals`
  AccentColor/Selection, or GNOME `gsettings` accent-color). Startup toggle
  writes `~/.config/autostart/sophon.desktop`.

### 5.8 `actions.rs`, `fmt.rs`, `icon.rs`/`png.rs`
- **`actions.rs`** — `signal(pids, Term|Kill)` (SIGTERM/SIGKILL; `EPERM` retries
  via the admin helper); `tree(procs, roots)` (roots + all descendants, deepest
  first, so children die before parents); `open_location(path)` (D-Bus
  `org.freedesktop.FileManager1.ShowItems` to highlight the file, else `xdg-open`
  the folder).
- **`fmt.rs`** — human formatting: bytes (KB/MB/GB), bytes_short, bytes_mb,
  rate_bytes/rate_bytes_short, rate_bits, pct, ghz, temp (°C/°F toggle), watts,
  duration, ago, thousands.
- **`icon.rs`** — draws the Sophon mark (a glowing violet particle in a thin
  ring) at any size by supersampled software rendering; `install(remove)` writes
  `sophon.desktop` + hicolor PNG icons (48/128/256) so the taskbar shows the
  name+icon. **`png.rs`** — a tiny dependency-free PNG writer (uncompressed
  DEFLATE blocks + CRC), used for icons and the snapshot dev tool.

### 5.9 `snapshot.rs` (dev only)
`SOPHON_SNAPSHOT=<dir> sophon` scripts a sequence of UI actions (switch tabs,
select rows, hover graphs, open settings categories, change settings) and writes
each view to a PNG via `Window::take_snapshot`, then exits. This is the primary
way to verify visuals without a human at the mouse. `SOPHON_SNAPSHOT_FILTER`
pre-searches the process list.

---

## 6. UI/UX details that matter

- **Three tabs:** Processes, Performance, Settings. Header: logo, tab switcher,
  search box (Processes only), theme toggle. Status bar: process/thread counts +
  uptime on the left, Sophon's own CPU/RAM + sample cost on the right (toggleable).
- **Colours** are tokenised in `Theme` and defined as light/dark pairs; the whole
  UI recolours instantly on theme change. Metric colours are user-configurable.
- **Heat maps:** busy cells tint toward the heat colour, intensity configurable.
- **Toasts** (bottom pill) report action outcomes. A modal confirm dialog gates
  destructive actions.
- **Tray:** closing the window sends it to the tray with no taskbar entry (like
  Steam). Tray hover shows a live one-line summary (configurable metrics). Click
  opens; menu has Open + Quit.

---

## 7. Admin features (privileged helper)

Sophon itself never runs as root. `admin::enable()` runs
`pkexec /path/to/sophon --helper` (one desktop password prompt). The helper
(`helper.rs`) runs as root, sets `PR_SET_PDEATHSIG` so it dies with Sophon,
prints `ready`, then serves one request per stdin line:
- `signal <pid> <TERM|KILL>` → `ok`/`err …` (validates pid > 1 and the signal).
- `net-start` → starts the eBPF counters. `net-read` → `net <cookie>:<rx>:<tx> …`.
- `quit` / EOF → exit.
It runs no shell, opens no files, understands nothing else. `admin.rs` (the
client) talks to it over the pipe, exposes `is_active()`, `signal(pid, kill)`,
and `net_counters()`, and turns everything off if the helper dies.

**eBPF (`ebpf.rs`):** two `cgroup_skb` programs (ingress + egress) attached to
the root cgroup via BPF **links** (closing the link fd detaches; so counters
stop when the helper exits). Each program is hand-assembled BPF bytecode that
does `map[bpf_get_socket_cookie(skb)] += skb->len; return 1;` into an LRU hash
map (key = 64-bit socket cookie, value = bytes). **Every path returns 1
(allow)** — the counter can never drop traffic; a unit test asserts the only
`EXIT` is preceded by `r0 = 1` and all jumps land in range. `read()` dumps both
maps (via `BPF_MAP_GET_NEXT_KEY`/`LOOKUP_ELEM`) as `(cookie, rx, tx)`. netproc
maps cookies → pids (§4.9) so this yields per-app totals including **UDP**.
The BPF syscall is invoked directly via `libc::syscall(SYS_bpf, …)` with
hand-built `bpf_attr` (no libbpf dependency).

---

## 8. Efficiency techniques (summary of how the budget is met)
1. Read `/proc`+`/sys` via `openat` into reused buffers; parse bytes by hand.
2. Read each process's invariant data once; re-read on re-exec only.
3. `statm` only when RSS changed; disk temps every 30 ticks; RAPL optional.
4. Idle processes, idle GPU clients, and kernel threads polled less often, with
   exact deltas across the gaps.
5. One shared fd walk feeds both GPU and network attribution, on a rolling
   per-process schedule (hot processes more often).
6. sock_diag full dumps only every 5 ticks + targeted lookups between.
7. The process table is not sampled unless its tab is visible (every other tick
   elsewhere for spike culprits; slow cadence in the tray).
8. Snapshots share `Arc<Vec<Process>>`; history is fixed-capacity rings.
9. Slint software renderer with partial repaints; **no idle animation** (only
   brief, input-triggered ones); the sampler pauses when set to Paused.
10. Disk scans run off-thread at idle CPU+IO priority with progress + cancel.
11. Closed-to-tray unmaps the window (no compositor cost) and slows sampling.

Result on a Ryzen 7 5800X / RX 6950 XT, ~500 processes, a game running:
~25 MB PSS; ~1.2% of one core visible (~0.3% sampling, ~0.9% drawing);
~0.13% of one core in the tray; sampler ~2.6 ms CPU/tick.

---

## 9. Build, run, verify
```bash
cargo build --release
./target/release/sophon                     # run
./target/release/sophon --install           # add launcher entry + icon
cargo test --release                         # unit tests (parsers, layout, settings, bpf, cache)
cargo run --release -p sophon-core --example bench      # sampler cost + a data dump
cargo run --release -p sophon-core --example space -- /home   # time a scan
SOPHON_SNAPSHOT=/tmp/shots ./target/release/sophon      # render every view to PNG, exit
SOPHON_DEBUG=1 ./target/release/sophon                  # log visibility/frames/UI cost
```

## 10. What is deliberately not done yet
Steam/Proton game grouping with cover art; app icons from the icon theme;
pause/resume an app (`cgroup.freeze`) and atomic `cgroup.kill`; priority/affinity
controls; per-process disk I/O; NVIDIA/Intel GPU backends; keyboard navigation
for tree/space; startup & services tabs; a `--tui` mode on `sophon-core`; RPM
packaging. (Flatpak is unsuitable: its pid namespace hides host processes.)
