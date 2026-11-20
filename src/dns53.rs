// 本机 53 端口（核心 dns-in 入站）的占用检测与接管。
// 麒麟 / Debian 桌面默认启用 dnsmasq 监听 127.0.0.1:53（Windows 上 ICS、
// AdGuard、部分杀软也会占 53），会让 sing-box 的 dns-in 入站启动即 FATAL，
// 并因崩溃限流进入「无法启动核心」的死局。
// 两种策略（面板「透明代理」页可切换）：
//   avoid    渲染时避开 53，自动回落 5335（默认，安全）
//   takeover 接管：停用占用 53 的服务（如 dnsmasq），让核心绑定 53；
//            Windows 上开启时还会给配置注入 dns-in 入站（默认模板没有）
// systemd-resolved 的 stub 监听在 127.0.0.53:53，与 127.0.0.1:53 不冲突，
// 且它是系统 DNS 依赖，永远不动它。

use std::net::{TcpListener, UdpSocket};
use std::path::Path;

use serde_json::{json, Value};
use tokio::process::Command;

/// 53 被占时的回落端口（Go 版同款）。
pub const FALLBACK_PORT: i64 = 5335;
/// 核心 dns-in 入站的期望端口。
pub const DESIRED_PORT: i64 = 53;

pub fn supported() -> bool {
    cfg!(target_os = "linux") || cfg!(windows)
}

/// 接管 53 需要 root（Linux）/ 管理员（Windows）。
pub fn have_root() -> bool {
    #[cfg(target_os = "linux")]
    {
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(windows)]
    {
        // 管理员令牌检测：TokenElevation（windows-sys 0.52 的 HANDLE 是 isize）
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        use windows_sys::Win32::Security::{
            GetTokenInformation, TokenElevation, TOKEN_ELEVATION,
        };
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        const TOKEN_QUERY: u32 = 0x0008;
        unsafe {
            let mut token: HANDLE = 0;
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return false;
            }
            let mut elev: TOKEN_ELEVATION = std::mem::zeroed();
            let mut ret_len = 0u32;
            let ok = GetTokenInformation(
                token,
                TokenElevation,
                &mut elev as *mut TOKEN_ELEVATION as *mut core::ffi::c_void,
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut ret_len,
            );
            CloseHandle(token);
            ok != 0 && elev.TokenIsElevated != 0
        }
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        true
    }
}

/// 非特权身份运行时的指引文案（按平台）。
pub fn root_hint() -> &'static str {
    #[cfg(windows)]
    {
        "需要管理员权限：请以管理员身份重新运行面板（右键 → 以管理员身份运行）"
    }
    #[cfg(not(windows))]
    {
        "需要 root 权限：请用 systemd 服务运行面板（sudo systemctl restart sing-box-panel），或 sudo 启动"
    }
}

/// 兼容保留：原 const 引用点改用 root_hint()。
pub const ROOT_HINT: &str = "需要 root 权限：请用 systemd 服务运行面板（sudo systemctl restart sing-box-panel），或 sudo 启动";

/// 127.0.0.1:[port] 是否可绑定（tcp/udp 任一被占即视为占用）。
pub fn port_bindable(listen: &str, port: u16) -> bool {
    let busy_tcp = TcpListener::bind((listen, port)).is_err();
    let busy_udp = UdpSocket::bind((listen, port)).is_err();
    !(busy_tcp || busy_udp)
}

/// 列出当前监听 port 的进程 (pid, 进程名)。
/// Linux 解析 `ss -lntup`（非 root 看不到其它用户进程的 pid，列表可能为空）；
/// Windows 解析 `netstat -ano`（pid 永远可见），名字用 tasklist 补齐。
pub async fn occupiers(port: u16) -> Vec<(i32, String)> {
    if !supported() {
        return Vec::new();
    }
    #[cfg(windows)]
    {
        occupiers_windows(port).await
    }
    #[cfg(not(windows))]
    {
        occupiers_linux(port).await
    }
}

#[cfg(unix)]
async fn occupiers_linux(port: u16) -> Vec<(i32, String)> {
    let out = match run_out("ss", &["-lntup"], 10).await {
        Some(o) => format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        ),
        None => return Vec::new(),
    };
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();
    let needle = format!(":{port} ");
    for line in out.lines() {
        if !line.contains(&needle) || !line.contains("users:((") {
            continue;
        }
        // users:(("dnsmasq",pid=1362,fd=4)) —— 可能多个进程共用一个 socket
        let users = match line.split_once("users:((") {
            Some((_, u)) => u,
            None => continue,
        };
        for part in users.split(',') {
            let part = part.trim_matches(|c| c == '"' || c == ')' || c == '(');
            // 形如 pid=1362 / fd=4，只认 pid= 段
            let pid_str = part.strip_prefix("pid=").unwrap_or(part);
            if let Ok(pid) = pid_str.parse::<i32>() {
                // 进程名在 pid 的前一个逗号段里
                if let Some(name) = find_name_before(users, pid) {
                    if seen.insert(pid) {
                        result.push((pid, name));
                    }
                }
            }
        }
    }
    result
}

#[cfg(windows)]
async fn occupiers_windows(port: u16) -> Vec<(i32, String)> {
    let Some(out) = run_out("netstat", &["-ano"], 10).await else {
        return Vec::new();
    };
    let out = String::from_utf8_lossy(&out.stdout).to_string();
    // netstat 控制台输出可能是 GBK/UTF-8 混排，但数字与列结构不受影响
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();
    let needle = format!(":{port} ");
    for line in out.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        // TCP: 协议 本地地址 远端地址 状态 pid
        // UDP: 协议 本地地址 远端地址 pid
        if cols.len() < 4 || (cols[0] != "TCP" && cols[0] != "UDP") {
            continue;
        }
        let is_tcp = cols[0] == "TCP";
        // TCP 要求 LISTENING 状态；UDP 无状态概念，监听即占
        if is_tcp && (cols.len() < 5 || cols[3] != "LISTENING") {
            continue;
        }
        let local = cols[1];
        if !local.contains(&needle.trim_end()) || !local.ends_with(&format!(":{port}")) {
            continue;
        }
        let pid_str = if is_tcp { cols[4] } else { cols[3] };
        if let Ok(pid) = pid_str.parse::<i32>() {
            if pid <= 0 || !seen.insert(pid) {
                continue;
            }
            let name = process_name_windows(pid).await.unwrap_or_default();
            result.push((pid, name));
        }
    }
    result
}

/// tasklist 按 pid 查进程名（CSV 输出："name","pid","..."）。
#[cfg(windows)]
async fn process_name_windows(pid: i32) -> Option<String> {
    let out = run_out(
        "tasklist",
        &["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"],
        10,
    )
    .await?;
    let text = String::from_utf8_lossy(&out.stdout);
    let first = text.lines().next()?.trim();
    // "name.exe","1234","Console","1","12,345 K"
    let name = first.split("\",\"").next()?.trim_start_matches('"');
    if name.is_empty() || name.starts_with("INFO:") {
        return None;
    }
    Some(name.to_string())
}

/// 在 users 串里找 pid 前面最近的带引号进程名。
#[cfg(unix)]
fn find_name_before(users: &str, pid: i32) -> Option<String> {
    let pid_token = format!("pid={pid}");
    let idx = users.find(&pid_token)?;
    let before = &users[..idx];
    let name_start = before.rfind('"')?;
    // 名字形如 ("dnsmasq" —— 从上个引号往前找成对引号
    let head = &before[..name_start];
    let name_open = head.rfind('"')?;
    Some(before[name_open + 1..name_start].to_string())
}

/// 进程所属的 systemd unit（Linux，读 /proc/<pid>/cgroup，cgroup v2）；
/// Windows 上返回 pid 对应的服务名（Win32_Service）。
async fn unit_of(pid: i32) -> Option<String> {
    #[cfg(windows)]
    {
        services_of_windows(pid).await
    }
    #[cfg(not(windows))]
    {
        let path = format!("/proc/{pid}/cgroup");
        let body = tokio::fs::read_to_string(path).await.ok()?;
        for line in body.lines() {
            if let Some(p) = line.strip_prefix("0::") {
                let unit = p.rsplit('/').next()?.to_string();
                if unit.ends_with(".service") {
                    return Some(unit);
                }
                return None; // .scope / 用户会话等不轻易动
            }
        }
        None
    }
}

/// 查 pid 挂在哪个 Windows 服务下（svchost 共享进程可能挂多个）。
/// WMI 调用可能偶发挂起，限时 15s；卡住视为「无服务」按普通进程处理。
#[cfg(windows)]
async fn services_of_windows(pid: i32) -> Option<String> {
    let out = run_out(
        "powershell",
        &[
            "-NoProfile",
            "-Command",
            &format!(
                "(Get-CimInstance Win32_Service -Filter 'ProcessId={pid}' | Select-Object -ExpandProperty Name) -join ','"
            ),
        ],
        15,
    )
    .await?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        return None;
    }
    Some(text)
}

async fn run(cmd: &str, args: &[&str]) -> bool {
    // 外部命令必须限时：Windows 上 WMI/PowerShell 偶发挂起会把 HTTP 请求拖死
    match tokio::time::timeout(std::time::Duration::from_secs(10), async {
        Command::new(cmd).args(args).output().await
    })
    .await
    {
        Ok(Ok(o)) => o.status.success(),
        _ => false,
    }
}

/// 带超时的外部命令输出（occupiers/进程名查询用）。
async fn run_out(cmd: &str, args: &[&str], secs: u64) -> Option<std::process::Output> {
    tokio::time::timeout(std::time::Duration::from_secs(secs), async {
        Command::new(cmd).args(args).output().await
    })
    .await
    .ok()?
    .ok()
}

/// 尝试释放本机 port：
/// Linux：systemd-resolved（127.0.0.53 stub）跳过不碰；有 systemd unit 的
///        （如 dnsmasq）stop + disable 防开机自启；普通进程 SIGTERM→SIGKILL。
/// Windows：服务进程（如 ICS SharedAccess）sc stop + 改 disabled 启动类型；
///        普通进程 taskkill /F。
/// 返回 (是否已释放, 处理说明)。
pub async fn try_free(port: u16, log: &(dyn Fn(&str) + Send + Sync)) -> (bool, String) {
    let mut notes: Vec<String> = Vec::new();
    for round in 0..2 {
        let occ = occupiers(port).await;
        if occ.is_empty() {
            if port_bindable("127.0.0.1", port) {
                if round > 0 {
                    return (true, notes.join("；"));
                }
                return (true, "端口未被占用".into());
            }
            // 有监听但看不到 pid（非 root），无法接管
            return (
                false,
                format!("端口 {port} 被其它进程占用但无权限识别（{}）", root_hint()),
            );
        }
        for (pid, name) in occ {
            #[cfg(unix)]
            if name.contains("systemd-resolve") {
                notes.push(format!("跳过 systemd-resolved(pid={pid})：stub 监听 127.0.0.53 与本机 53 不冲突"));
                continue;
            }
            let unit = unit_of(pid).await;
            match unit {
                Some(u) => {
                    log(&format!("接管 53 端口：停止 {u}（pid={pid}，{name}）"));
                    stop_and_disable_service(&u).await;
                    notes.push(format!("已停用服务 {u}（原进程 {name}）"));
                }
                None => {
                    log(&format!("接管 53 端口：终止进程 {name}(pid={pid})"));
                    kill_process(pid).await;
                    notes.push(format!("已终止进程 {name}(pid={pid})"));
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    }
    let freed = port_bindable("127.0.0.1", port);
    if !freed {
        notes.push("释放失败，端口仍被占用".into());
    }
    (freed, notes.join("；"))
}

/// 进程是否存活（Linux: /proc；Windows: OpenProcess+Wait 0）。
/// Windows 路径当前由 kill_process 的 taskkill /F 直接保证，暂无调用点。
#[cfg_attr(windows, allow(dead_code))]
fn process_alive(pid: i32) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
            if h == 0 {
                return false;
            }
            let wait = WaitForSingleObject(h, 0);
            CloseHandle(h);
            wait != WAIT_OBJECT_0
        }
    }
    #[cfg(not(windows))]
    {
        Path::new(&format!("/proc/{pid}")).exists()
    }
}

/// 停止并禁用服务（Linux: systemctl stop/disable；Windows: sc stop/config start= disabled）。
async fn stop_and_disable_service(unit: &str) {
    #[cfg(windows)]
    {
        // svchost 共享进程可能挂多个服务（逗号分隔），逐个处理
        for svc in unit.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
            let _ = run("sc.exe", &["stop", svc]).await;
            let _ = run("sc.exe", &["config", svc, "start=", "disabled"]).await;
        }
    }
    #[cfg(not(windows))]
    {
        let _ = run("systemctl", &["stop", unit]).await;
        let _ = run("systemctl", &["disable", unit]).await;
    }
}

/// 终止进程（Linux: SIGTERM→SIGKILL；Windows: taskkill /F）。
async fn kill_process(pid: i32) {
    #[cfg(windows)]
    {
        let _ = run("taskkill", &["/PID", &pid.to_string(), "/F"]).await;
    }
    #[cfg(not(windows))]
    {
        let _ = run("kill", &["-TERM", &pid.to_string()]).await;
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if process_alive(pid) {
            let _ = run("kill", &["-KILL", &pid.to_string()]).await;
        }
    }
}

/// 读已应用配置里 dns-in 入站的端口（没有则 None）。
pub fn configured_port(conf: &Path) -> Option<i64> {
    let raw = std::fs::read(conf).ok()?;
    let doc: Value = serde_json::from_slice(&raw).ok()?;
    let inbounds = doc.get("inbounds")?.as_array()?;
    for inb in inbounds {
        if inb.get("tag").and_then(|v| v.as_str()) == Some("dns-in") {
            return inb.get("listen_port").and_then(|v| v.as_i64());
        }
    }
    None
}

/// 启动前修补「陈旧配置」：已应用配置里 dns-in 仍是 53 且端口已被占时，
/// 按 takeover 设置处理——takeover 先释放端口（成功则保留 53），
/// 否则把配置端口改写成回落端口，避免核心启动即 FATAL 进崩溃循环。
/// 返回是否改写了配置文件。
pub async fn patch_stale_config(
    conf: &Path,
    takeover: bool,
    log: &(dyn Fn(&str) + Send + Sync),
) -> bool {
    if !supported() || configured_port(conf) != Some(DESIRED_PORT) {
        return false;
    }
    if port_bindable("127.0.0.1", DESIRED_PORT as u16) {
        return false;
    }
    let who = occupiers(DESIRED_PORT as u16)
        .await
        .iter()
        .map(|(p, n)| format!("{n}(pid={p})"))
        .collect::<Vec<_>>()
        .join(", ");
    if takeover && have_root() {
        let (freed, note) = try_free(DESIRED_PORT as u16, log).await;
        if freed {
            log(&format!("本机 53 端口已由面板接管（{note}），核心将继续绑定 53"));
            return false;
        }
        log(&format!("53 端口接管失败：{note}，回退为避开模式"));
    }
    if !patch_config_port(conf, FALLBACK_PORT) {
        return false;
    }
    log(&format!(
        "本机 DNS 端口 {DESIRED_PORT} 已被占用（{who}），已自动改用 {FALLBACK_PORT}（如需恢复请释放该端口后重启面板）"
    ));
    true
}

fn patch_config_port(conf: &Path, port: i64) -> bool {
    let raw = match std::fs::read(conf) {
        Ok(r) => r,
        Err(_) => return false,
    };
    let mut doc: Value = match serde_json::from_slice(&raw) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let Some(inbounds) = doc.get_mut("inbounds").and_then(|v| v.as_array_mut()) else {
        return false;
    };
    let mut changed = false;
    for inb in inbounds {
        if inb.get("tag").and_then(|v| v.as_str()) == Some("dns-in") {
            if let Some(p) = inb.get_mut("listen_port") {
                *p = json!(port);
                changed = true;
            }
        }
    }
    if !changed {
        return false;
    }
    match serde_json::to_vec_pretty(&doc) {
        Ok(b) => std::fs::write(conf, b).is_ok(),
        Err(_) => false,
    }
}
