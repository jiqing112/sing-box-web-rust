// 「从已下发的配置里读回事实」的查询：mixed 端口、规则条数、有没有 TUN、
// 有没有监听透明代理端口。sing-box（JSON）与 mihomo（YAML）字段不同，集中在这里。

use serde_json::Value;

use crate::engine;

fn parse_doc(raw: &[u8], eng: &str) -> Result<Value, String> {
    if raw.iter().all(|b| b.is_ascii_whitespace()) {
        return Err("配置为空".into());
    }
    let m: Value = if engine::is_mihomo(eng) {
        serde_yaml::from_slice(raw).map_err(|e| format!("配置不是合法 YAML: {e}"))?
    } else {
        serde_json::from_slice(raw).map_err(|e| format!("配置不是合法 JSON: {e}"))?
    };
    if !m.is_object() {
        return Err("配置为空".into());
    }
    Ok(m)
}

/// 只做「是不是这份格式」的粗校验，真正的语义校验交给内核的 check。
pub fn validate_doc(raw: &[u8], eng: &str) -> Result<(), String> {
    parse_doc(raw, eng).map(|_| ())
}

/// 本机 mixed 入口的端口；没有该入口时返回 0。
pub fn mixed_port(raw: &[u8], eng: &str) -> i64 {
    let Ok(d) = parse_doc(raw, eng) else { return 0 };
    if engine::is_mihomo(eng) {
        return int_val(d.get("mixed-port"));
    }
    for it in d.get("inbounds").and_then(|v| v.as_array()).unwrap_or(&vec![]) {
        let Some(m) = it.as_object() else { continue };
        if m.get("type").and_then(|v| v.as_str()) == Some("mixed") {
            let p = int_val(m.get("listen_port"));
            if p > 0 {
                return p;
            }
        }
    }
    0
}

/// 分流规则条数，用于概览页展示。
pub fn rule_count(raw: &[u8], eng: &str) -> usize {
    let Ok(d) = parse_doc(raw, eng) else { return 0 };
    if engine::is_mihomo(eng) {
        return d.get("rules").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
    }
    d.get("route")
        .and_then(|r| r.get("rules"))
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0)
}

/// 已下发的配置里是否真的带 TUN 接管。
pub fn tun_running(raw: &[u8], eng: &str) -> bool {
    let Ok(d) = parse_doc(raw, eng) else { return false };
    if engine::is_mihomo(eng) {
        return d
            .get("tun")
            .and_then(|t| t.get("enable"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
    }
    for it in d.get("inbounds").and_then(|v| v.as_array()).unwrap_or(&vec![]) {
        let Some(m) = it.as_object() else { continue };
        if m.get("type").and_then(|v| v.as_str()) == Some("tun") {
            return true;
        }
    }
    false
}

/// 确认配置里确实监听了透明代理要重定向到的端口。
/// 不通过时不能下发 nft 规则，否则流量会被送到没人监听的端口，整网断网。
pub fn verify_tproxy(raw: &[u8], eng: &str, port: i64, hijack_dns: bool) -> Result<(), String> {
    let d = parse_doc(raw, eng).map_err(|e| format!("配置解析失败: {e}"))?;
    if engine::is_mihomo(eng) {
        let got = mihomo_redir_port(&d);
        if got != port {
            return Err(format!(
                "当前配置没有监听 :{port} 的透明代理入站（当前为 {got}），请先应用包含该端口的配置"
            ));
        }
        if hijack_dns {
            let listen = d
                .get("dns")
                .and_then(|x| x.get("listen"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !listen.ends_with(&format!(":{LAN_DNS_PORT}")) {
                return Err(format!(
                    "当前配置的 dns.listen 不是 :{LAN_DNS_PORT}，无法接管局域网 DNS"
                ));
            }
        }
        return Ok(());
    }
    let mut has_redirect = false;
    let mut has_dns = false;
    for it in d.get("inbounds").and_then(|v| v.as_array()).unwrap_or(&vec![]) {
        let Some(m) = it.as_object() else { continue };
        let t = m.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if t == "redirect" && int_val(m.get("listen_port")) == port {
            has_redirect = true;
        }
        if int_val(m.get("listen_port")) == LAN_DNS_PORT {
            has_dns = true;
        }
    }
    if !has_redirect {
        return Err(format!(
            "当前配置没有监听 :{port} 的 redirect 入站，请先应用包含该入站的配置"
        ));
    }
    if hijack_dns && !has_dns {
        return Err(format!(
            "当前配置没有监听 :{LAN_DNS_PORT} 的 DNS 入站，无法接管局域网 DNS"
        ));
    }
    Ok(())
}

use super::clash::LAN_DNS_PORT;

/// 配置里真正用于透明代理的端口，没有则返回 0。
/// listeners 里 type: redir 优先；退回顶层 redir-port（老写法）。
fn mihomo_redir_port(d: &Value) -> i64 {
    if let Some(ls) = d.get("listeners").and_then(|v| v.as_array()) {
        for it in ls {
            let Some(m) = it.as_object() else { continue };
            if m.get("type").and_then(|v| v.as_str()) != Some("redir") {
                continue;
            }
            let p = int_val(m.get("port"));
            if p > 0 {
                return p;
            }
        }
    }
    int_val(d.get("redir-port"))
}

/// 透明代理入站是否绑在局域网客户端连得到的地址上。
/// 配置里没写 listen（= 监听全部地址）时返回 true，不制造假警告。
pub fn redir_reachable_from_lan(raw: &[u8], eng: &str, port: i64) -> bool {
    let Ok(d) = parse_doc(raw, eng) else { return true };
    if engine::is_mihomo(eng) {
        if let Some(ls) = d.get("listeners").and_then(|v| v.as_array()) {
            for it in ls {
                let Some(m) = it.as_object() else { continue };
                if m.get("type").and_then(|v| v.as_str()) != Some("redir")
                    || int_val(m.get("port")) != port
                {
                    continue;
                }
                let listen = m.get("listen").and_then(|v| v.as_str()).unwrap_or("");
                return !is_loopback_listen(listen);
            }
        }
        // 老写法：redir-port 的监听地址由 bind-address / allow-lan 推导，不做猜测。
        return true;
    }
    for it in d.get("inbounds").and_then(|v| v.as_array()).unwrap_or(&vec![]) {
        let Some(m) = it.as_object() else { continue };
        if m.get("type").and_then(|v| v.as_str()) != Some("redirect")
            || int_val(m.get("listen_port")) != port
        {
            continue;
        }
        let listen = m.get("listen").and_then(|v| v.as_str()).unwrap_or("");
        return !is_loopback_listen(listen);
    }
    true
}

fn is_loopback_listen(addr: &str) -> bool {
    matches!(
        addr.trim().to_lowercase().as_str(),
        "127.0.0.1" | "::1" | "localhost"
    )
}

fn int_val(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n.as_i64().unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}
