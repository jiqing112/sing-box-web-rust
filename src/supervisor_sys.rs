// 平台相关的进程管理原语分发。

#[cfg(unix)]
#[path = "supervisor_unix.rs"]
mod imp;

#[cfg(windows)]
#[path = "supervisor_windows.rs"]
mod imp;

pub use imp::{kill_process_tree, process_alive, setup_process_group, sweep_orphans};
