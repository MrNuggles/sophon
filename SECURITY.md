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
