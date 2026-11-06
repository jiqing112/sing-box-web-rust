// 系统托盘（仅 Windows）。unix 上是空实现，保持 main.rs 的调用面一致。
// 平台分派沿用 supervisor_sys.rs 的 #[path] 范式。

#[cfg(windows)]
#[path = "tray_windows.rs"]
mod imp;

#[cfg(not(windows))]
mod imp {
    use super::{TrayEvent, TrayState};

    #[derive(Clone)]
    pub struct TrayHandle;

    impl TrayHandle {
        pub fn update(&self, _f: impl FnOnce(&mut TrayState)) {}
        pub fn remove(&self) {}
    }

    pub fn spawn(_initial: TrayState) -> Option<TrayHandle> {
        None
    }
}

/// 托盘菜单/状态共享的数据快照。
#[derive(Clone, Debug, PartialEq)]
pub struct TrayState {
    /// 面板监听地址 host:port（来自 settings.listen）
    pub listen: String,
    pub core_running: bool,
    pub autostart: bool,
}

/// 托盘菜单触发、需要主循环（tokio）处理的事件。
#[derive(Debug)]
pub enum TrayEvent {
    /// 菜单「退出」：走与 Ctrl+C 相同的优雅退出序列
    Exit,
    /// 菜单「启动/停止核心」：按当前状态取反
    CoreToggle,
    /// 菜单「重启核心」
    CoreRestart,
    /// 菜单「开机自启」：按当前状态取反
    AutostartToggle,
}

pub use imp::TrayHandle;

/// 创建托盘。Windows 上返回 (句柄, 事件接收端)；unix / 会话 0（SYSTEM 计划任务
/// 自启，无交互桌面）/ 创建失败时返回 None。
#[cfg(windows)]
pub fn spawn(initial: TrayState) -> Option<(TrayHandle, tokio::sync::mpsc::UnboundedReceiver<TrayEvent>)> {
    imp::spawn(initial)
}

#[cfg(not(windows))]
pub fn spawn(initial: TrayState) -> Option<(TrayHandle, tokio::sync::mpsc::UnboundedReceiver<TrayEvent>)> {
    let _ = initial;
    None
}

/// 托盘里「打开面板」用的 URL：监听地址归一化（0.0.0.0/::/空主机名 → 127.0.0.1）。
pub fn panel_url(listen: &str) -> String {
    let (host, port) = match listen.rsplit_once(':') {
        Some((h, p)) => (h.trim_matches(['[', ']']), p),
        None => (listen, ""),
    };
    let host = match host {
        "" | "0.0.0.0" | "::" | "[::]" => "127.0.0.1",
        _ => host,
    };
    if port.is_empty() {
        format!("http://{host}")
    } else {
        format!("http://{host}:{port}")
    }
}

#[cfg(test)]
mod tests {
    use super::panel_url;

    #[test]
    fn url_normalizes_wildcard_hosts() {
        assert_eq!(panel_url("127.0.0.1:9090"), "http://127.0.0.1:9090");
        assert_eq!(panel_url("0.0.0.0:9090"), "http://127.0.0.1:9090");
        assert_eq!(panel_url(":::9090"), "http://127.0.0.1:9090");
        assert_eq!(panel_url("192.168.1.2:8080"), "http://192.168.1.2:8080");
    }
}
