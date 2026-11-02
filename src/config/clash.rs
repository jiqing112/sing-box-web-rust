// mihomo 渲染：模板负责「除了节点以外的一切」，
// 这里注入 proxies / proxy-groups，并按面板设置覆盖端口、TUN、路由规则。
// 节点存的是 sing-box 风格 outbound 片段，逐字段翻译成 Clash proxy。

use serde_json::{json, Map, Value};

use crate::model::Node;
use crate::config::RenderOpts;

/// 局域网 DNS 接管的目标端口，与 nft 规则里的 redirect 目标保持一致。
pub const LAN_DNS_PORT: i64 = 5354;

/// 透明代理入站在 listeners 里的固定名字。
const REDIR_LISTENER_NAME: &str = "redir-in";

/// 「内网直连」规则。用显式 IP-CIDR 而不是 GEOIP,private：
/// 即使 geo 数据库没下载完，内网流量也不会被送去节点。
const CLASH_PRIVATE_RULES: &[&str] = &[
    "IP-CIDR,127.0.0.0/8,DIRECT,no-resolve",
    "IP-CIDR,10.0.0.0/8,DIRECT,no-resolve",
    "IP-CIDR,172.16.0.0/12,DIRECT,no-resolve",
    "IP-CIDR,192.168.0.0/16,DIRECT,no-resolve",
    "IP-CIDR,100.64.0.0/10,DIRECT,no-resolve",
    "IP-CIDR,169.254.0.0/16,DIRECT,no-resolve",
];

pub fn render_clash(tpl: Value, nodes: &[Node], opts: &RenderOpts) -> Result<Vec<u8>, String> {
    let enabled: Vec<&Node> = nodes.iter().filter(|n| n.enabled).collect();
    if enabled.is_empty() {
        return Err("没有启用的节点".into());
    }

    let mut proxies: Vec<Value> = Vec::with_capacity(enabled.len());
    let mut tags: Vec<Value> = Vec::with_capacity(enabled.len());
    for n in &enabled {
        let Some(mut p) = clash_proxy_from_node(n) else { continue };
        p.insert("name".into(), json!(n.tag));
        tags.push(json!(n.tag));
        proxies.push(Value::Object(p));
    }
    if proxies.is_empty() {
        return Err("没有可转换的节点：mihomo 不支持订阅里的这些协议".into());
    }

    let mut tpl = tpl;
    if !tpl.is_object() {
        tpl = json!({});
    }
    let obj = tpl.as_object_mut().unwrap();
    obj.insert("proxies".into(), Value::Array(proxies));

    // 策略组名字与 sing-box 那套保持一致（proxy / auto）
    let mut select_members: Vec<Value> = vec![json!("auto"), json!("DIRECT")];
    select_members.extend(tags.iter().cloned());
    obj.insert(
        "proxy-groups".into(),
        json!([
            {"name": "proxy", "type": "select", "proxies": select_members},
            {
                "name": "auto",
                "type": "url-test",
                "url": "https://www.gstatic.com/generate_204",
                "interval": 600,
                "tolerance": 50,
                "proxies": tags
            }
        ]),
    );

    obj.insert("external-controller".into(), json!(opts.clash_controller()));
    apply_clash_routing(obj, &opts.routing_mode);
    apply_clash_ports(obj, opts);
    apply_clash_listeners(obj, opts);
    apply_clash_tun(obj, opts);
    apply_clash_sniffer(obj);

    serde_yaml::to_string(&tpl).map(|s| s.into_bytes()).map_err(|e| format!("marshal yaml: {e}"))
}

/// redirect/tproxy 接管进来的连接只有目标 IP，没有域名。
/// mihomo 必须开 sniffer（TLS SNI / HTTP Host）才能把这些纯 IP 连接
/// 按域名走规则分流，否则国外域名会被路由到污染 IP 上直连失败。
/// sing-box 侧由路由规则里的 {"action":"sniff"} 承担同样职责。
fn apply_clash_sniffer(tpl: &mut Map<String, Value>) {
    let sniffer = json!({
        "enable": true,
        "sniff": {
            "TLS": {"ports": [443, 8443], "override-destination": true},
            "HTTP": {"ports": [80, "8080-8880"], "override-destination": true},
            "QUIC": {"ports": [443], "override-destination": true}
        },
        "force-dns-mapping": true,
        "parse-pure-ip": true,
        "override-destination": true
    });
    match tpl.get_mut("sniffer") {
        Some(v) if v.is_object() => {
            // 模板里已有 sniffer：只保证 enable 与嗅探结果覆盖打开
            if let Some(o) = v.as_object_mut() {
                o.insert("enable".into(), json!(true));
                o.entry("parse-pure-ip".to_string()).or_insert(json!(true));
                o.entry("force-dns-mapping".to_string()).or_insert(json!(true));
                o.entry("override-destination".to_string()).or_insert(json!(true));
            }
        }
        _ => {
            tpl.insert("sniffer".into(), sniffer);
        }
    }
}

fn apply_clash_routing(tpl: &mut Map<String, Value>, mode: &str) {
    let private: Vec<Value> = CLASH_PRIVATE_RULES.iter().map(|r| json!(r)).collect();
    match mode {
        "global" => {
            let mut rules = private;
            rules.push(json!("MATCH,proxy"));
            tpl.insert("rules".into(), Value::Array(rules));
            tpl.insert("mode".into(), json!("global"));
        }
        "direct" => {
            let mut rules = private;
            rules.push(json!("MATCH,DIRECT"));
            tpl.insert("rules".into(), Value::Array(rules));
            tpl.insert("mode".into(), json!("direct"));
        }
        _ => {
            // mihomo 要求规则表里有兜底项，否则未命中的流量行为不确定。
            let mut rules: Vec<Value> = tpl
                .get("rules")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let has_match = rules.iter().any(|r| {
                r.as_str()
                    .map(|s| s.trim().to_uppercase().starts_with("MATCH"))
                    .unwrap_or(false)
            });
            if !has_match {
                rules.push(json!("MATCH,proxy"));
            }
            tpl.insert("rules".into(), Value::Array(rules));
            tpl.insert("mode".into(), json!("rule"));
        }
    }
}

/// 注入本机 mixed 入口与局域网 DNS 接管端口。
/// 透明代理不用顶层 redir-port（受 bind-address/allow-lan 支配会被锁回环），
/// 由 apply_clash_listeners 用 listeners 显式声明。
fn apply_clash_ports(tpl: &mut Map<String, Value>, opts: &RenderOpts) {
    let listen = if opts.mixed_listen.is_empty() {
        "127.0.0.1".to_string()
    } else {
        opts.mixed_listen.clone()
    };
    if opts.mixed_port > 0 {
        tpl.insert("mixed-port".into(), json!(opts.mixed_port));
    }
    tpl.insert("bind-address".into(), json!(listen));
    // allow-lan 与 sing-box 的语义对齐：只有监听非回环地址时才对外开放。
    tpl.insert(
        "allow-lan".into(),
        json!(listen != "127.0.0.1" && listen != "::1"),
    );
    tpl.remove("redir-port");

    // mihomo 的 redir/dns.listen 只在 Linux 上存在，Windows 上写了会启动失败。
    if cfg!(target_os = "linux") {
        let dns = tpl
            .entry("dns".to_string())
            .or_insert_with(|| json!({}));
        if !dns.is_object() {
            *dns = json!({});
        }
        let dns_obj = dns.as_object_mut().unwrap();
        dns_obj.insert("enable".into(), json!(true));
        dns_obj.insert("listen".into(), json!(format!("0.0.0.0:{LAN_DNS_PORT}")));
    }
}

/// 维护 listeners 段：Linux 的透明代理入站 + 可选的局域网 socks/http。
fn apply_clash_listeners(tpl: &mut Map<String, Value>, opts: &RenderOpts) {
    let mut out: Vec<Value> = Vec::new();

    // 透明代理入站常驻：没有 nft 规则时不会有流量进来。
    // udp 显式关掉：redir 只能还原 TCP 的原始目标，UDP 需要 TUN。
    if cfg!(target_os = "linux") {
        let port = if opts.tproxy_port > 0 { opts.tproxy_port } else { 12345 };
        out.push(json!({
            "name": REDIR_LISTENER_NAME,
            "type": "redir",
            "port": port,
            "listen": "0.0.0.0",
            "udp": false
        }));
    }

    if opts.lan_proxy {
        let with_auth = !opts.user.is_empty() && !opts.pass.is_empty();
        let users = if with_auth {
            vec![json!({"username": opts.user, "password": opts.pass})]
        } else {
            vec![]
        };
        let mut socks = Map::new();
        socks.insert("name".into(), json!("socks-lan"));
        socks.insert("type".into(), json!("socks"));
        socks.insert("port".into(), json!(opts.socks_port));
        socks.insert("listen".into(), json!("0.0.0.0"));
        socks.insert("udp".into(), json!(true));
        let mut http_l = Map::new();
        http_l.insert("name".into(), json!("http-lan"));
        http_l.insert("type".into(), json!("http"));
        http_l.insert("port".into(), json!(opts.http_port));
        http_l.insert("listen".into(), json!("0.0.0.0"));
        if with_auth {
            socks.insert("users".into(), Value::Array(users.clone()));
            http_l.insert("users".into(), Value::Array(users));
        }
        out.push(Value::Object(socks));
        out.push(Value::Object(http_l));
    }

    if out.is_empty() {
        // 关掉开关就把 listeners 整段删掉，避免上次的开放代理留在配置里
        tpl.remove("listeners");
        return;
    }
    tpl.insert("listeners".into(), Value::Array(out));
}

/// mihomo 的 tun 是顶层对象（不是 listeners 里的一项）。
fn apply_clash_tun(tpl: &mut Map<String, Value>, opts: &RenderOpts) {
    if !opts.tun_enabled {
        if let Some(tun) = tpl.get_mut("tun").and_then(|v| v.as_object_mut()) {
            tun.insert("enable".into(), json!(false));
        }
        return;
    }
    tpl.insert(
        "tun".into(),
        json!({
            "enable": true,
            "stack": "mixed",
            "device": "mihomo-tun",
            "auto-route": true,
            "auto-detect-interface": true,
            "dns-hijack": ["any:53"],
            "mtu": 9000
        }),
    );
}

/* ------------------------------------------------------------ node -> clash proxy */

/// 把一条 sing-box outbound 片段翻译成 Clash proxy。
/// 返回 None 表示该协议 mihomo 不支持（或字段不全），调用方跳过这条节点。
#[allow(unused_assignments)]
fn clash_proxy_from_node(n: &Node) -> Option<Map<String, Value>> {
    let ob = &n.outbound;
    let server = ob.get("server").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let port = val_i64(ob.get("server_port"));
    if server.is_empty() || port <= 0 {
        return None;
    }

    let mut p = Map::new();
    p.insert("server".into(), json!(server.clone()));
    p.insert("port".into(), json!(port));
    let typ = ob
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();

    let mut ptype = String::new();
    match typ.as_str() {
        "shadowsocks" | "ss" => {
            ptype = "ss".to_string();
            p.insert("cipher".into(), json!(val_str(ob.get("method"))));
            p.insert("password".into(), json!(val_str(ob.get("password"))));
            p.insert("udp".into(), json!(true));
        }
        "vmess" => {
            ptype = "vmess".to_string();
            p.insert("uuid".into(), json!(val_str(ob.get("uuid"))));
            p.insert("alterId".into(), json!(val_i64(ob.get("alter_id"))));
            p.insert("cipher".into(), json!("auto"));
            p.insert("udp".into(), json!(true));
        }
        "vless" => {
            ptype = "vless".to_string();
            p.insert("uuid".into(), json!(val_str(ob.get("uuid"))));
            let f = val_str(ob.get("flow"));
            if !f.is_empty() {
                p.insert("flow".into(), json!(f));
            }
            p.insert("udp".into(), json!(true));
        }
        "trojan" => {
            ptype = "trojan".to_string();
            p.insert("password".into(), json!(val_str(ob.get("password"))));
            p.insert("udp".into(), json!(true));
        }
        "hysteria2" | "hy2" => {
            ptype = "hysteria2".to_string();
            p.insert("password".into(), json!(val_str(ob.get("password"))));
            if let Some(obfs) = ob.get("obfs").and_then(|v| v.as_object()) {
                let t = obfs.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if !t.is_empty() {
                    p.insert("obfs".into(), json!(t));
                    p.insert(
                        "obfs-password".into(),
                        json!(val_str(obfs.get("password"))),
                    );
                }
            }
        }
        "tuic" => {
            ptype = "tuic".to_string();
            p.insert("uuid".into(), json!(val_str(ob.get("uuid"))));
            p.insert("password".into(), json!(val_str(ob.get("password"))));
        }
        "http" => {
            ptype = "http".to_string();
            let u = val_str(ob.get("username"));
            if !u.is_empty() {
                p.insert("username".into(), json!(u));
            }
            let pw = val_str(ob.get("password"));
            if !pw.is_empty() {
                p.insert("password".into(), json!(pw));
            }
        }
        "socks" => {
            ptype = "socks5".to_string();
            let u = val_str(ob.get("username"));
            if !u.is_empty() {
                p.insert("username".into(), json!(u));
            }
            let pw = val_str(ob.get("password"));
            if !pw.is_empty() {
                p.insert("password".into(), json!(pw));
            }
            p.insert("udp".into(), json!(true));
        }
        _ => return None,
    }
    p.insert("type".into(), json!(ptype.clone()));

    // TLS：sing-box 的 tls 对象与 Clash 的扁平字段一一对应，
    // 差别只在 sni / servername 的字段名。
    if let Some(tls) = ob.get("tls").and_then(|v| v.as_object()) {
        if tls.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false) {
            p.insert("tls".into(), json!(true));
            let mut sn = tls
                .get("server_name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if sn.is_empty() {
                sn = server.clone();
            }
            match ptype.as_str() {
                "trojan" | "hysteria2" | "tuic" => {
                    p.insert("sni".into(), json!(sn));
                }
                _ => {
                    p.insert("servername".into(), json!(sn));
                }
            }
            if tls.get("insecure").and_then(|v| v.as_bool()).unwrap_or(false) {
                p.insert("skip-cert-verify".into(), json!(true));
            }
            if let Some(utls) = tls.get("utls").and_then(|v| v.as_object()) {
                let fp = utls.get("fingerprint").and_then(|v| v.as_str()).unwrap_or("");
                if !fp.is_empty() {
                    p.insert("client-fingerprint".into(), json!(fp));
                }
            }
            if let Some(r) = tls.get("reality").and_then(|v| v.as_object()) {
                let pk = r.get("public_key").and_then(|v| v.as_str()).unwrap_or("");
                if !pk.is_empty() {
                    let mut ro = Map::new();
                    ro.insert("public-key".into(), json!(pk));
                    ro.insert(
                        "short-id".into(),
                        json!(r.get("short_id").and_then(|v| v.as_str()).unwrap_or("")),
                    );
                    p.insert("reality-opts".into(), Value::Object(ro));
                    // mihomo 的 REALITY 必须指定指纹，缺省用 chrome
                    if !p.contains_key("client-fingerprint") {
                        p.insert("client-fingerprint".into(), json!("chrome"));
                    }
                }
            }
        }
    }

    // 传输层
    if let Some(tr) = ob.get("transport").and_then(|v| v.as_object()) {
        let t = tr
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_lowercase();
        match t.as_str() {
            "ws" => {
                p.insert("network".into(), json!("ws"));
                let mut ws_opts = Map::new();
                let path = tr.get("path").and_then(|v| v.as_str()).unwrap_or("");
                ws_opts.insert(
                    "path".into(),
                    json!(if path.trim().is_empty() { "/" } else { path }),
                );
                if let Some(hd) = tr.get("headers").and_then(|v| v.as_object()) {
                    let host = hd.get("Host").and_then(|v| v.as_str()).unwrap_or("");
                    if !host.is_empty() {
                        ws_opts.insert("headers".into(), json!({"Host": host}));
                    }
                }
                p.insert("ws-opts".into(), Value::Object(ws_opts));
            }
            "grpc" => {
                p.insert("network".into(), json!("grpc"));
                p.insert(
                    "grpc-opts".into(),
                    json!({"grpc-service-name": val_str(tr.get("service_name"))}),
                );
            }
            "http" | "h2" => {
                p.insert("network".into(), json!("http"));
                let mut h2 = Map::new();
                let h = tr.get("host").and_then(|v| v.as_str()).unwrap_or("");
                if !h.is_empty() {
                    h2.insert("host".into(), json!([h]));
                }
                let pth = tr.get("path").and_then(|v| v.as_str()).unwrap_or("");
                if !pth.is_empty() {
                    h2.insert("path".into(), json!(pth));
                }
                if !h2.is_empty() {
                    p.insert("h2-opts".into(), Value::Object(h2));
                }
            }
            _ => {}
        }
    }

    Some(p)
}

fn val_str(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

fn val_i64(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n.as_i64().unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}
