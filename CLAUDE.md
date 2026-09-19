# Sophon

A Windows-style task manager for Linux (Rust + Slint). See README.md for features, layout and roadmap.

## Core rules (apply to every change)

### 1. Low resource usage, at all times
Sophon runs next to games. Its footprint is a feature and is never traded for polish.

- Measure before and after any change that touches sampling or rendering:
  - `cargo run --release -p sophon-core --example bench` (sampler CPU per stage)
  - `SOPHON_DEBUG=1 ./target/release/sophon` (UI model-build and UI-thread cost per tick)
  - `/proc/<pid>/smaps_rollup` for memory (PSS)
- Budgets: sampler < 0.5% of one core; whole app ~1% of one core with the window visible; ~0 when
  minimized; < 40 MB PSS. If a change breaks a budget, say so and fix it or propose a trade-off.
- No work nobody can see: don't sample or render for hidden tabs or a hidden window.
- No continuous animation. Animate only in response to user input (hover, click), briefly.
- Hot paths don't allocate: reuse buffers, parse bytes by hand, read static data once.
- Heavy work (like disk scans) runs only on demand, off the UI thread, at idle CPU/IO priority,
  with visible progress and a way to cancel.

### 2. Everything is interactive
Anything the user can see should respond to them.

- Every item has hover feedback. Clickable things show a pointer cursor.
- Single click selects, double click opens/enters, right click shows a context menu of what can be
  done with it. Shift+click adds to a selection; actions apply to the whole selection.
- Hovering and holding shows a tooltip with the full detail (paths, exact numbers).
- Keyboard works too: Delete acts on the selection, Escape clears/closes, Ctrl+F searches.
- Destructive actions confirm first and prefer reversible options (Trash over delete).
- Interactivity must not break rule 1: no idle cost, only brief animations on input.

## Working on this repo

- Build: `cargo build --release`. No `-devel` packages are needed (fontconfig/Wayland/X11 are
  loaded at runtime). Keep it that way: fontique's `fontconfig-dlopen` feature must stay on.
- Check visuals with `SOPHON_SNAPSHOT=<dir> ./target/release/sophon`, which renders every view to
  PNG and exits (`SOPHON_SNAPSHOT_FILTER=<text>` searches first). Don't screenshot the user's
  desktop; they're often gaming.
- Hover, tooltips, context menus and drag need a real mouse: say what couldn't be verified.
- Tests: `cargo test --release`.
