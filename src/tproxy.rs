// nftables 透明代理管理：只做 redirect（TCP），不碰 UDP，
// 关闭开关即删表，流量恢复直连。规则文件落在 data/tproxy.nft 便于审计。

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use tokio::process::Command;

const TABLE_NAME: &str = "singbox";
/// 与模板里的 dns-lan 入站保持一致
const LAN_DNS_PORT: i64 = 5354;

pub struct Manager {
    nft: PathBuf,
    port: i64,
    rule_set: PathBuf,
    /// 面板自身所在 cgroup，用于排除 sing-box 自己的出站，避免回环
    cg_path: String,
    cg_level: usize,
    mu: tokio::sync::Mutex<()>,
}

#[derive(Clone, Serialize)]
pub struct Status {
    #[serde(rename = "enabled")]
    pub enabled: bool,
    #[serde(rename = "local")]
    pub local: bool,
    #[serde(rename = "dns")]
    pub dns: bool,
    #[serde(rename = "rules")]
    pub rules: i64,
    #[serde(rename = "cidrs")]
    pub cidrs: Vec<String>,
    #[serde(rename = "port")]
    pub port: i64,
    #[serde(rename = "mode")]
    pub mode: String,
}

pub fn new(nft_path: &str, rule_set_path: PathBuf, port: i64) -> Manager {
    let (cg_path, cg_level) = self_cgroup();
    Manager {
        nft: PathBuf::from(nft_path),
        port,
        rule_set: rule_set_path,
        cg_path,
        cg_level,
        mu: tokio::sync::Mutex::new(()),
    }
}

/// 读取 /proc/self/cgroup（cgroup v2），返回相对路径与层级。
/// sing-box 是面板的子进程，同一个 cgroup，排除它就不会把代理自己的流量再送进代理。
#[cfg(target_os = "linux")]
fn self_cgroup() -> (String, usize) {
    let Ok(b) = std::fs::read_to_string("/proc/self/cgroup") else { return (String::new(), 0) };
    for line in b.lines() {
        if let Some(path) = line.strip_prefix("0::") {
            let path = path.trim_matches('/');
            if path.is_empty() {
                return (String::new(), 0);
            }
            let level = path.split('/').count();
            return (path.to_string(), level);
        }
    }
    (String::new(), 0)
}

#[cfg(not(target_os = "linux"))]
fn self_cgroup() -> (String, usize) {
    (String::new(), 0)
}

pub fn supported() -> bool {
    cfg!(target_os = "linux")
}

pub fn local_supported(m: &Manager) -> bool {
    !m.cg_path.is_empty() && m.cg_level > 0
}

fn guard(_m: &Manager) -> Result<(), String> {
    if !supported() {
        return Err("nftables is only supported on Linux".into());
    }
    Ok(())
}

/// 生成完整的 nft 脚本。local_nets 是「目标为内网就放行」，
/// cidrs 是「哪些来源需要被接管」。
fn build(m: &Manager, cidrs: &[String], local: bool, hijack_dns: bool) -> String {
    let mut b = String::new();
    b.push_str(&format!("table inet {TABLE_NAME} {{\n"));
    b.push_str("  set local_nets {\n    type ipv4_addr\n    flags interval\n");
    b.push_str("    elements = { 0.0.0.0/8, 10.0.0.0/8, 100.64.0.0/10, 127.0.0.0/8, 169.254.0.0/16, 172.16.0.0/12, 192.168.0.0/16, 224.0.0.0/4, 240.0.0.0/4 }\n  }\n");

    b.push_str("  chain prerouting {\n");
    b.push_str("    type nat hook prerouting priority dstnat; policy accept;\n");
    if hijack_dns && !cidrs.is_empty() {
        let src = format!("ip saddr {{ {} }}", cidrs.join(", "));
        b.push_str(&format!(
            "    {src} udp dport 53 redirect to :{LAN_DNS_PORT}\n"
        ));
        b.push_str(&format!(
            "    {src} tcp dport 53 redirect to :{LAN_DNS_PORT}\n"
        ));
    }
    b.push_str("    ip daddr @local_nets return\n");
    if !cidrs.is_empty() {
        b.push_str(&format!(
            "    ip saddr {{ {} }} meta l4proto tcp redirect to :{}\n",
            cidrs.join(", "),
            m.port
        ));
    }
    b.push_str("  }\n");

    // 本机流量：走 output 钩子。必须先排除面板自身 cgroup，
    // 否则 sing-box 的出站会被再次重定向回自己，形成回环。
    if local && !m.cg_path.is_empty() {
        b.push_str("  chain output {\n");
        b.push_str("    type nat hook output priority -100; policy accept;\n");
        b.push_str(&format!(
            "    socket cgroupv2 level {} \"{}\" return\n",
            m.cg_level, m.cg_path
        ));
        b.push_str("    ip daddr @local_nets return\n");
        b.push_str(&format!("    meta l4proto tcp redirect to :{}\n", m.port));
        b.push_str("  }\n");
    }
    b.push_str("}\n");
    b
}

/// 下发规则：先删旧表再整表重建，保证幂等。
pub async fn apply(m: &Manager, cidrs: &[String], local: bool, hijack_dns: bool) -> Result<(), String> {
    let _g = m.mu.lock().await;
    guard(m)?;
    if cidrs.is_empty() {
        return Err("没有配置任何需要接管的网段".into());
    }
    if local && !local_supported(m) {
        return Err("无法识别面板所在的 cgroup，接管本机流量可能导致回环，已拒绝".into());
    }
    let script = build(m, cidrs, local, hijack_dns);

    std::fs::write(&m.rule_set, script.as_bytes()).map_err(|e| e.to_string())?;

    let _ = run_nft(m, &["delete", "table", "inet", TABLE_NAME], 8).await;
    match run_nft_stdin(m, &script, 8).await {
        Ok(()) => Ok(()),
        Err(e) => Err(format!("nft: {e}")),
    }
}

/// 删除整张表，流量恢复直连。
pub async fn disable(m: &Manager) -> Result<(), String> {
    let _g = m.mu.lock().await;
    guard(m)?;
    match run_nft(m, &["delete", "table", "inet", TABLE_NAME], 8).await {
        Ok(_) => Ok(()),
        Err(msg) => {
            if msg.contains("No such file") || msg.contains("does not exist") {
                return Ok(());
            }
            Err(format!("nft: {msg}"))
        }
    }
}

/// 读回当前内核里的规则，而不是相信内存里的开关。
pub async fn status(m: &Manager, cidrs: Vec<String>) -> Status {
    let mut st = Status {
        enabled: false,
        local: false,
        dns: false,
        rules: 0,
        cidrs,
        port: m.port,
        mode: "redirect".into(),
    };
    if !supported() {
        return st;
    }
    if let Ok(out) = run_nft(m, &["list", "table", "inet", TABLE_NAME], 4).await {
        for line in out.lines() {
            if line.contains("redirect to") {
                st.rules += 1;
            }
            if line.contains("hook output") {
                st.local = true;
            }
            if line.contains("dport 53 redirect") {
                st.dns = true;
            }
        }
        st.enabled = true;
    }
    st
}

/// 下发前做最低限度检查，避免把整机网络搞坏。
pub async fn preflight(m: &Manager, port: i64) -> Result<(), String> {
    guard(m)?;
    if !m.nft.exists() {
        return Err("找不到 nft 命令".into());
    }
    if !supports_redirect(m).await {
        return Err("内核缺少 NAT redirect 支持".into());
    }
    if port <= 0 || port > 65535 {
        return Err("端口不合法".into());
    }
    Ok(())
}

/// 判断内核能否做 NAT redirect。
/// 不能只看 /sys/module/nf_nat_redirect：内核内建（CONFIG_NF_NAT_REDIRECT=y）时
/// 不生成这个目录，Debian 官方内核正是这么编的。先走快路径，不命中用 nft -c 真解析。
async fn supports_redirect(m: &Manager) -> bool {
    if Path::new("/sys/module/nf_nat_redirect").exists() {
        return true;
    }
    if Path::new("/sys/module/nft_chain_nat").exists() {
        return true;
    }
    let probe = "table inet tprobe_redirect {\n  chain c {\n    type nat hook prerouting priority dstnat; policy accept;\n    ip saddr 127.0.0.1 meta l4proto tcp redirect to :1\n  }\n}\n";
    run_nft_stdin_check(m, probe).await.is_ok()
}

async fn run_nft(m: &Manager, args: &[&str], timeout_secs: u64) -> Result<String, String> {
    let mut cmd = Command::new(&m.nft);
    cmd.args(args);
    let fut = cmd.output();
    let out = match tokio::time::timeout(Duration::from_secs(timeout_secs), fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(e.to_string()),
        Err(_) => return Err("timeout".into()),
    };
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if !out.status.success() {
        return Err(text.trim().to_string());
    }
    Ok(text)
}

async fn run_nft_stdin(m: &Manager, script: &str, _timeout_secs: u64) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    let mut child = Command::new(&m.nft)
        .arg("-f")
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(script.as_bytes()).await;
        drop(stdin);
    }
    let out = child.wait_with_output().await.map_err(|e| e.to_string())?;
    if !out.status.success() {
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        return Err(text.trim().to_string());
    }
    Ok(())
}

/// nft -c 语法检查（不落规则）：内核模块缺失会当场报错。
async fn run_nft_stdin_check(m: &Manager, script: &str) -> Result<(), String> {
    let mut cmd = Command::new(&m.nft);
    cmd.arg("-c").arg("-f").arg("-");
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    use tokio::io::AsyncWriteExt;
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(script.as_bytes()).await;
        drop(stdin);
    }
    let out = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output())
        .await
        .map_err(|_| "timeout".to_string())?
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        return Err(text.trim().to_string());
    }
    Ok(())
}

/// 自动识别需要接管的网段。不能排除默认路由所在网卡——
/// 旁路由场景下本机默认路由和 LAN 在同一张网卡。
pub fn detect_lan() -> Vec<String> {
    const SKIP: &[&str] = &["docker", "incus", "veth", "tun", "singbox", "wg", "br-wan", "lo"];
    let mut cidrs = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let Ok(ifaces) = if_addrs::get_if_addrs() else { return cidrs };
    for iface in ifaces {
        let name = iface.name.to_lowercase();
        if SKIP.iter().any(|s| name.starts_with(s)) {
            continue;
        }
        let (ip, prefix) = match iface.addr {
            if_addrs::IfAddr::V4(v4) => (v4.ip, v4.prefixlen),
            _ => continue,
        };
        if prefix == 32 {
            continue;
        }
        // 归一化成网络地址（192.168.68.28/24 → 192.168.68.0/24）
        let mask = if prefix == 0 {
            0u32
        } else {
            u32::MAX << (32 - prefix as u32)
        };
        let ip_u = u32::from(ip) & mask.to_be();
        let net = std::net::Ipv4Addr::from(ip_u);
        let cidr = format!("{net}/{prefix}");
        if !is_private(&cidr) || seen.contains(&cidr) {
            continue;
        }
        seen.insert(cidr.clone());
        cidrs.push(cidr);
    }
    cidrs
}

/// 只接管 RFC1918 网段，公网地址一律不碰。
fn is_private(cidr: &str) -> bool {
    let ip_str = cidr.split('/').next().unwrap_or("");
    let Ok(ip) = ip_str.parse::<std::net::Ipv4Addr>() else { return false };
    let o = ip.octets();
    match o[0] {
        10 => true,
        172 => (16..=31).contains(&o[1]),
        192 => o[1] == 168,
        _ => false,
    }
}
