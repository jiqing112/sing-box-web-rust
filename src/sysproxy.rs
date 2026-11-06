// 系统代理开关（仅 Windows）：把当前登录用户的 Windows 系统代理指向本面板的
// mixed 入站。平台分派沿用 tray.rs 的 #[path] 范式。

#[cfg(windows)]
#[path = "sysproxy_windows.rs"]
mod imp;

#[cfg(not(windows))]
#[path = "sysproxy_unix.rs"]
mod imp;

pub use imp::{set_enabled, status};
