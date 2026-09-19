//! Sophon core: low-overhead system sampling straight from procfs and sysfs.
//!
//! No UI dependencies, so the GUI and a future TUI share one engine. The hot
//! path (one sample per tick) is written to avoid heap allocation: files are
//! read into reused buffers via `openat` relative to held directory handles,
//! and anything that never changes for a process is read exactly once.

pub mod apps;
pub mod cpu;
pub mod disk;
pub mod fdwalk;
pub mod gpu;
pub mod history;
pub mod mem;
pub mod net;
pub mod netproc;
pub mod parse;
pub mod process;
pub mod sampler;
pub mod space;
pub mod sys;

pub use sampler::{Sampler, Snapshot};
