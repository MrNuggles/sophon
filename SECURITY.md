# Sophon — Security Notes

Sophon has **no network listener** and opens **no ports**. Its attack surface is
local: the data it reads, the programs it launches, and one optional privileged
helper. This document states the trust model and the one thing a packager must
do before shipping Sophon to other people.

## Trust model

- **The GUI runs as the normal user.** It never runs as root.
- Sophon reads `/proc` and `/sys`, which contain **attacker-controllable
  strings** (any local user can name a process or a file anything). Sophon treats
  these purely as data:
  - It never passes them to a shell. Every external program is launched with an
    argv array (`Command::new(...).arg(...)`), never `sh -c`, so process names,
    command lines and file paths cannot inject commands.
  - They are rendered by the Slint GUI as text (not a terminal), so escape /
    control sequences have no effect.
  - Parsing is bounds-checked; `panic = "abort"`, so the worst a malformed
    kernel/procfs value can do is cleanly crash Sophon, not corrupt memory.
- The disk scanner **does not follow symlinks** (symlinked files and directories
  are skipped), so a crafted symlink cannot redirect a scan or the Trash action
  outside the tree being viewed.
- Destructive actions confirm first and prefer reversible options (Trash via
  `gio trash`, not delete). "Move to Trash" refuses your home directory, a whole
  drive, or anything already in the Trash.

## The privileged helper (opt-in)

Admin features are **off by default**. Turning them on runs, once:

```
pkexec /path/to/sophon --helper
```

which starts a small root process (`src/helper.rs`). Properties:

- It talks to the GUI over the **anonymous stdin/stdout pipe pkexec inherits**.
  No socket, no named pipe: only the parent Sophon holds the other end, so no
  other local process can send it commands.
- It accepts a **fixed request set** and nothing else: `signal <pid> <TERM|KILL>`
  (pid validated `> 1`, which blocks `kill(0)`/`kill(-1)` group and broadcast
  kills), `net-start`, `net-read`, `quit`. It runs no shell and opens no files.
- It exits when the pipe closes (i.e. when Sophon exits) and sets
  `PR_SET_PDEATHSIG`, so it never outlives the GUI.
- Its eBPF byte counters are `cgroup_skb` programs that only **add packet
  lengths to a map keyed by socket cookie** — they never read packet contents and
  every path returns "allow", so they cannot drop traffic. The in-kernel BPF
  verifier is the safety backstop: a malformed program is rejected, never run.
  The programs detach when the helper's link fds close.

Known accepted limitations (shared with every task manager): a **PID-reuse race**
exists between choosing a PID and the helper acting on it; and `net-read` exposes
aggregate byte counts of *unidentifiable* system sockets to the user's GUI (no
contents, no attribution) which are ignored.

## Optional network tabs (Docker, Proxy)

Both network tabs talk plain HTTP to services on your trusted LAN. They are
compiled in by default; `cargo build --release --no-default-features` removes
**all** network code, and each tab can also be hidden in Settings → Tabs (a
hidden tab fetches nothing at all).

### Docker

The Docker tab makes Sophon a **network client to a root-equivalent API**, so
the risk moves to the servers:

- Sophon connects **out** to `host:port`; it still opens no listener itself.
- The container ids it sends are validated (ASCII alphanumeric only) and only the
  three needed endpoints are ever called (list, start/stop/restart). The JSON and
  HTTP parsers are bounds-checked with recursion/size caps, and `panic = "abort"`
  keeps a malformed response to a clean crash.
- **You must run a filtered socket proxy** (`CONTAINERS=1 POST=1`, everything else
  denied) and firewall its port to Sophon's machine — see `docs/docker.md`.
  Exposing the raw Docker socket over TCP is a full host compromise.
- Server addresses live in `~/.config/sophon/settings.conf`; nothing is hardcoded
  and there are no credentials (the proxy is unauthenticated on the trusted LAN,
  which is why it must be firewalled).

### Proxy (Nginx Proxy Manager)

- Talks plain HTTP to the NPM **admin port** (default 81) on your LAN. NPM's
  login crosses the LAN unencrypted — like every service on a trusted home
  network. A **dedicated NPM user** for Sophon is recommended over your admin
  login.
- The email and password are stored **only** in
  `~/.config/sophon/settings.conf`, which Sophon writes **owner-only (0600)**.
  The session token is kept in memory only and re-queried when it expires.
- The client is **read-only**: it calls exactly three endpoints — token login,
  list proxy hosts, list certificates — and never enables, disables or deletes
  anything in NPM.

## Packaging requirement (before distributing to others)

pkexec runs **the binary at the path Sophon was launched from**. When that binary
lives in a **user-writable** location (e.g. a local `target/release/sophon`
build), anything already running as the user could replace it before the user
authenticates the admin prompt, and thereby run code as root. This is inherent to
pkexec-ing a non-root-owned binary.

For a distributed build, the packager **must**:

1. Install the binary to a **root-owned, non-user-writable** path
   (`/usr/bin/sophon`, owned by root, mode 0755).
2. Ship a **polkit action** (`org.sophon.helper.policy` in
   `/usr/share/polkit-1/actions/`) that authorizes running a **fixed** helper
   path, instead of relying on pkexec-any-binary.

Until then, admin features are safe **for the user who built it on their own
machine** (they already control that binary), but the build should not be
distributed with admin features presented as safe for multi-user systems.

## Reporting

This is a personal project. Open an issue for anything that looks wrong.
