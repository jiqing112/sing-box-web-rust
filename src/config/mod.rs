// 配置渲染与下发的核心：模板 + 节点 → 完整配置；
// Apply 是唯一的写配置入口：check → 快照 → 原子写 → 重启 → 探活 → 失败回滚。

pub mod clash;
pub mod inspect;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::engine;
use crate::model::{normalize_outbound_type, Node, Settings};
use crate::store::Store;
use crate::supervisor::Supervisor;

/// 需要由面板注入到配置里的运行时选项（模板里不写死）。
#[derive(Clone, Debug, Default)]
pub struct RenderOpts {
    pub engine: String,
    pub data_dir: PathBuf,
    pub lan_proxy: bool,
    pub socks_port: i64,
    pub http_port: i64,
    pub user: String,
    pub pass: String,
    pub tun_enabled: bool,
    pub routing_mode: String,
    pub mixed_port: i64,
    pub mixed_listen: String,
    pub tproxy_port: i64,
    pub clash_addr: String,
}

pub const DEFAULT_CLASH_ADDR: &str = "127.0.0.1:9091";

impl RenderOpts {
    pub fn from_settings(set: &Settings, data_dir: &Path) -> RenderOpts {
        RenderOpts {
            engine: engine::normalize(&set.engine),
            data_dir: data_dir.to_path_buf(),
            lan_proxy: set.lan_proxy,
            socks_port: set.lan_socks_port,
            http_port: set.lan_http_port,
            user: set.lan_user.clone(),
            pass: set.lan_pass.clone(),
            tun_enabled: set.tun_enabled,
            routing_mode: set.routing_mode.clone(),
            mixed_port: set.mixed_port,
            mixed_listen: set.mixed_listen.clone(),
            tproxy_port: set.tproxy_port,
            clash_addr: set.clash_addr.clone(),
        }
    }

    /// 要渲染进配置的控制器地址，未配置时用默认值。
    pub fn clash_controller(&self) -> String {
        if !self.clash_addr.is_empty() {
            self.clash_addr.clone()
        } else {
            DEFAULT_CLASH_ADDR.into()
        }
    }
}

/// 按引擎解码模板：sing-box 是 JSON，mihomo 是 YAML。统一成 JSON Value 便于操作。
pub fn decode_template(raw: &[u8], eng: &str) -> Result<Value, String> {
    if raw.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(Value::Object(Map::new()));
    }
    let mut m: Value = if engine::is_mihomo(eng) {
        serde_yaml::from_slice(raw).map_err(|e| format!("模板不是合法 YAML: {e}"))?
    } else {
        serde_json::from_slice(raw).map_err(|e| format!("模板不是合法 JSON: {e}"))?
    };
    if !m.is_object() {
        m = Value::Object(Map::new());
    }
    Ok(m)
}

pub fn render_bytes(tpl_raw: &[u8], nodes: &[Node], opts: &RenderOpts) -> Result<Vec<u8>, String> {
    let tpl = decode_template(tpl_raw, &opts.engine)?;
    render(tpl, nodes, opts)
}

/// 把模板与节点片段合并成完整配置。
pub fn render(tpl: Value, nodes: &[Node], opts: &RenderOpts) -> Result<Vec<u8>, String> {
    if engine::is_mihomo(&opts.engine) {
        clash::render_clash(tpl, nodes, opts)
    } else {
        render_sing_box(tpl, nodes, opts)
    }
}

fn enabled_nodes<'a>(nodes: &'a [Node]) -> Vec<&'a Node> {
    nodes.iter().filter(|n| n.enabled).collect()
}

fn render_sing_box(tpl: Value, nodes: &[Node], opts: &RenderOpts) -> Result<Vec<u8>, String> {
    let enabled = enabled_nodes(nodes);
    if enabled.is_empty() {
        return Err("没有启用的节点".into());
    }
    let tags: Vec<&str> = enabled.iter().map(|n| n.tag.as_str()).collect();

    let mut outbounds: Vec<Value> = Vec::with_capacity(enabled.len() + 3);
    // 前两个是策略组：proxy（selector）与 auto（urltest）
    let mut sel_members: Vec<Value> = vec![json!("auto"), json!("direct")];
    sel_members.extend(tags.iter().map(|t| json!(t)));
    outbounds.push(json!({
        "type": "selector",
        "tag": "proxy",
        "outbounds": sel_members,
        "default": "auto"
    }));
    outbounds.push(json!({
        "type": "urltest",
        "tag": "auto",
        "outbounds": tags.iter().map(|t| json!(t)).collect::<Vec<_>>(),
        "url": "https://www.gstatic.com/generate_204",
        "interval": "10m",
        "tolerance": 50
    }));
    for n in &enabled {
        let mut ob = n.outbound.clone();
        // 老版本把 ss:// 的 scheme 直接存成 type=ss，sing-box 不认；
        // 这里再归一一次，升级后不重新拉订阅也能跑。
        if let Some(t) = ob.get("type").and_then(|v| v.as_str()).map(|s| s.to_string()) {
            ob.insert("type".into(), json!(normalize_outbound_type(&t)));
        }
        ob.insert("tag".into(), json!(n.tag));
        outbounds.push(Value::Object(ob));
    }
    outbounds.push(json!({"type": "direct", "tag": "direct"}));
    // 规则集下载专用出站：加 tcp_fast_open 是为了让这个 direct 不是"空 direct"
    // ——sing-box 拒绝把 http_client.detour 指向空 direct。它不进 selector。
    outbounds.push(json!({
        "type": "direct",
        "tag": crate::model::RULE_SET_DETOUR_TAG,
        "tcp_fast_open": true
    }));

    let mut merged = tpl;
    if !merged.is_object() {
        merged = Value::Object(Map::new());
    }
    let obj = merged.as_object_mut().unwrap();
    obj.insert("outbounds".into(), Value::Array(outbounds));

    {
        let route = obj
            .entry("route".to_string())
            .or_insert_with(|| json!({}));
        if !route.is_object() {
            *route = json!({});
        }
        let route_obj = route.as_object_mut().unwrap();
        if !route_obj
            .get("final")
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false)
        {
            route_obj.insert("final".into(), json!("proxy"));
        }
        route_obj.entry("auto_detect_interface".to_string()).or_insert(json!(true));

        apply_routing_mode(route_obj, &opts.routing_mode);
        migrate_rule_set_http_client(route_obj);
    }
    // global/direct 模式还要改 dns 段（route 已在上面处理）
    match opts.routing_mode.as_str() {
        "global" => {
            if let Some(dns) = obj.get_mut("dns").and_then(|v| v.as_object_mut()) {
                dns.insert("rules".into(), json!([]));
                dns.insert("final".into(), json!("remote"));
            }
        }
        "direct" => {
            if let Some(dns) = obj.get_mut("dns").and_then(|v| v.as_object_mut()) {
                dns.insert("rules".into(), json!([]));
                dns.insert("final".into(), json!("local"));
            }
        }
        _ => {}
    }

    let exp = obj
        .entry("experimental".to_string())
        .or_insert_with(|| json!({}));
    if !exp.is_object() {
        *exp = json!({});
    }
    let exp_obj = exp.as_object_mut().unwrap();
    // clash_api 既要在模板缺失时补上，也要保证控制器地址与面板配置一致：
    // 地址对不上时面板读不到状态、测速全失败。
    let clash_api = exp_obj
        .entry("clash_api".to_string())
        .or_insert_with(|| json!({"default_mode": "rule"}));
    if !clash_api.is_object() {
        *clash_api = json!({"default_mode": "rule"});
    }
    let api_obj = clash_api.as_object_mut().unwrap();
    api_obj
        .entry("default_mode".to_string())
        .or_insert(json!("rule"));
    api_obj.insert("external_controller".into(), json!(opts.clash_controller()));

    if let Some(cache) = exp_obj.get_mut("cache_file").and_then(|v| v.as_object_mut()) {
        if !opts.data_dir.as_os_str().is_empty() {
            if let Some(p) = cache.get("path").and_then(|v| v.as_str()).map(|s| s.to_string()) {
                if !p.is_empty() && !Path::new(&p).is_absolute() {
                    // 旧模板按「数据目录固定叫 data」写成 data/cache.db；
                    // 剥掉历史前缀再挂到真实 DataDir 下。
                    let mut p = p;
                    for prefix in ["data\\", "data/"] {
                        if p.starts_with(prefix) {
                            p = p[prefix.len()..].to_string();
                            break;
                        }
                    }
                    // data_dir 在启动时已是绝对路径，这里不再 canonicalize
                    //（Windows 的 canonicalize 会加 \?\ 前缀，核心解析不了）。
                    let abs = opts.data_dir.join(&p);
                    cache.insert("path".into(), json!(abs.to_string_lossy()));
                }
            }
        }
    }

    // 本机 mixed 入站：端口与监听范围以面板设置为准。
    // 只改已有 mixed 入站，不凭空新增。
    if opts.mixed_port > 0 || !opts.mixed_listen.is_empty() {
        if let Some(inb) = obj.get_mut("inbounds").and_then(|v| v.as_array_mut()) {
            for it in inb.iter_mut() {
                let Some(m) = it.as_object_mut() else { continue };
                if m.get("type").and_then(|v| v.as_str()) != Some("mixed") {
                    continue;
                }
                if opts.mixed_port > 0 {
                    m.insert("listen_port".into(), json!(opts.mixed_port));
                }
                if !opts.mixed_listen.is_empty() {
                    m.insert("listen".into(), json!(opts.mixed_listen));
                }
            }
        }
    }

    // 局域网独立端口：socks 与 http 分开。用户名和密码都填写才启用鉴权。
    if opts.lan_proxy {
        let with_auth = !opts.user.is_empty() && !opts.pass.is_empty();
        let users = if with_auth {
            vec![json!({"username": opts.user, "password": opts.pass})]
        } else {
            vec![]
        };
        let mut socks = Map::new();
        socks.insert("type".into(), json!("socks"));
        socks.insert("tag".into(), json!("socks-lan"));
        socks.insert("listen".into(), json!("0.0.0.0"));
        socks.insert("listen_port".into(), json!(opts.socks_port));
        let mut http = Map::new();
        http.insert("type".into(), json!("http"));
        http.insert("tag".into(), json!("http-lan"));
        http.insert("listen".into(), json!("0.0.0.0"));
        http.insert("listen_port".into(), json!(opts.http_port));
        if with_auth {
            socks.insert("users".into(), Value::Array(users.clone()));
            http.insert("users".into(), Value::Array(users));
        }
        if let Some(inb) = obj.get_mut("inbounds").and_then(|v| v.as_array_mut()) {
            inb.push(Value::Object(socks));
            inb.push(Value::Object(http));
        } else {
            obj.insert(
                "inbounds".into(),
                Value::Array(vec![Value::Object(socks), Value::Object(http)]),
            );
        }
    }

    // TUN 入站：整网卡接管（Windows 用 wintun，Linux 用 tun 设备）。
    if opts.tun_enabled {
        let tun = json!({
            "type": "tun",
            "tag": "tun-in",
            "interface_name": "singbox-tun",
            "address": ["172.19.0.1/30"],
            "mtu": 9000,
            "auto_route": true,
            "strict_route": false,
            "stack": "mixed"
        });
        if let Some(inb) = obj.get_mut("inbounds").and_then(|v| v.as_array_mut()) {
            inb.push(tun);
        } else {
            obj.insert("inbounds".into(), Value::Array(vec![tun]));
        }
    }

    serde_json::to_vec_pretty(&merged).map_err(|e| e.to_string())
}

/// 用面板设置覆盖模板里的分流规则（route 部分；dns 部分由调用方处理）。
/// rule 保持模板原样；global/direct 只保留必要的动作规则。
fn apply_routing_mode(route: &mut Map<String, Value>, mode: &str) {
    match mode {
        "global" => {
            route.insert(
                "rules".into(),
                json!([
                    {"action": "sniff"},
                    {"protocol": "dns", "action": "hijack-dns"},
                    {"ip_is_private": true, "outbound": "direct"}
                ]),
            );
            route.insert("final".into(), json!("proxy"));
        }
        "direct" => {
            route.insert(
                "rules".into(),
                json!([
                    {"action": "sniff"},
                    {"protocol": "dns", "action": "hijack-dns"}
                ]),
            );
            route.insert("final".into(), json!("direct"));
        }
        _ => {}
    }
}

/// 把 sing-box 1.14 已弃用的 download_detour 改写成 http_client。
fn migrate_rule_set_http_client(route: &mut Map<String, Value>) {
    let Some(sets) = route.get_mut("rule_set").and_then(|v| v.as_array_mut()) else {
        return;
    };
    for it in sets.iter_mut() {
        let Some(m) = it.as_object_mut() else { continue };
        let detour = m
            .get("download_detour")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if detour.is_empty() {
            continue;
        }
        if !m.contains_key("http_client") {
            m.insert("http_client".into(), json!({"detour": detour}));
        }
        m.remove("download_detour");
    }
}

/* ------------------------------------------------------------ check */

#[derive(Clone, Debug)]
pub struct CheckResult {
    pub ok: bool,
    pub output: String,
    pub elapsed_ms: i64,
}

/// 调用内核自己的校验命令做唯一权威校验。
/// check 是独立进程；mihomo 缺 geo 数据时会顺带下载（90 秒兜底窗口）。
pub async fn check(bin: &str, eng: &str, data_dir: &Path, raw: &[u8]) -> CheckResult {
    let start = Instant::now();
    let home_dir = engine::home(data_dir, eng);
    let ext = engine::ext(eng);
    let tmp = std::env::temp_dir().join(format!(
        "panel-check-{}{}",
        crate::util::rand_hex(8),
        ext
    ));
    if let Err(e) = std::fs::write(&tmp, raw) {
        return CheckResult {
            ok: false,
            output: e.to_string(),
            elapsed_ms: 0,
        };
    }

    let timeout = if engine::is_mihomo(eng) {
        Duration::from_secs(90)
    } else {
        Duration::from_secs(15)
    };

    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(engine::check_args(eng, &tmp, &home_dir))
        .env("GOMEMLIMIT", "64MiB")
        .stdin(std::process::Stdio::null());
    let fut = cmd.output();
    let out = match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            let _ = std::fs::remove_file(&tmp);
            return CheckResult {
                ok: false,
                output: e.to_string(),
                elapsed_ms: start.elapsed().as_millis() as i64,
            };
        }
        Err(_) => {
            let _ = std::fs::remove_file(&tmp);
            return CheckResult {
                ok: false,
                output: "check timeout".into(),
                elapsed_ms: start.elapsed().as_millis() as i64,
            };
        }
    };
    let _ = std::fs::remove_file(&tmp);

    let mut text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // 内核的报错带 ANSI 颜色码（红色 FATAL 等），API/前端展示前剥掉。
    text = crate::supervisor::strip_ansi(&text);
    text = text.trim().to_string();
    let ok = out.status.success();
    if !ok && text.is_empty() {
        text = format!("exit status {}", out.status.code().unwrap_or(-1));
    }
    CheckResult {
        ok,
        output: text,
        elapsed_ms: start.elapsed().as_millis() as i64,
    }
}

/* ------------------------------------------------------------ apply */

#[derive(Debug)]
pub enum ApplyError {
    /// 内核 check 失败（HTTP 422）
    Check { detail: String, engine: String },
    /// 其它失败（HTTP 400）
    Other(String),
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApplyError::Check { detail, engine } => {
                write!(f, "{} check failed: {detail}", engine::label(engine))
            }
            ApplyError::Other(s) => write!(f, "{s}"),
        }
    }
}

pub struct Applier {
    store: Arc<Store>,
    mu: tokio::sync::Mutex<()>,
    inner: tokio::sync::Mutex<ApplierInner>,
    sup: Arc<Supervisor>,
    clash: Arc<crate::clashclient::Client>,
}

struct ApplierInner {
    bin: String,
    eng: String,
    home: PathBuf,
}

impl Applier {
    pub fn new(store: Arc<Store>, bin: &str, sup: Arc<Supervisor>, clash: Arc<crate::clashclient::Client>) -> Applier {
        Applier {
            mu: tokio::sync::Mutex::new(()),
            inner: tokio::sync::Mutex::new(ApplierInner {
                bin: bin.to_string(),
                eng: store.engine(),
                home: store.dir().to_path_buf(),
            }),
            store,
            sup,
            clash,
        }
    }

    pub async fn set_engine(&self, bin: &str, eng: &str) {
        let mut g = self.inner.lock().await;
        g.bin = bin.to_string();
        g.eng = engine::normalize(eng);
        g.home = self.store.dir().to_path_buf();
    }

    /// 探活要重试：核心从 start 到 Clash API 可服务有几百毫秒的空窗。
    async fn probe_loop(&self, deadline: Duration) -> Result<(), String> {
        let start = Instant::now();
        loop {
            match self.clash.version().await {
                Ok(_) => return Ok(()),
                Err(e) => {
                    if start.elapsed() >= deadline {
                        return Err(e);
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
    }

    async fn rollback(
        &self,
        prev: &[u8],
        cause: String,
    ) -> Result<(), ApplyError> {
        if prev.is_empty() {
            return Err(ApplyError::Other(cause));
        }
        if let Err(e) = self.store.write_config(prev) {
            return Err(ApplyError::Other(format!(
                "{cause}; rollback write failed: {e}"
            )));
        }
        let _ = self.restart_core().await;
        Err(ApplyError::Other(format!("{cause}（已回滚到上一份配置）")))
    }

    async fn restart_core(&self) -> Result<(), String> {
        if !self.sup.running() {
            self.sup.start()
        } else {
            self.sup.restart().await
        }
    }

    /// 唯一的写配置入口：check → 快照 → 原子写 → 重启 → 探活 → 失败回滚。
    pub async fn apply(&self, raw: &[u8], note: &str) -> Result<(), ApplyError> {
        let _guard = self.mu.lock().await;
        let (bin, eng, home) = {
            let g = self.inner.lock().await;
            (g.bin.clone(), g.eng.clone(), g.home.clone())
        };
        if let Err(e) = inspect::validate_doc(raw, &eng) {
            return Err(ApplyError::Other(e));
        }
        let res = check(&bin, &eng, &home, raw).await;
        if !res.ok {
            return Err(ApplyError::Check {
                detail: res.output,
                engine: eng,
            });
        }

        let prev = self.store.config().unwrap_or_default();
        if !prev.is_empty() {
            if let Err(e) = self.store.take_snapshot(&format!("应用前备份 · {note}")) {
                return Err(ApplyError::Other(format!("snapshot: {e}")));
            }
        }
        if let Err(e) = self.store.write_config(raw) {
            return Err(ApplyError::Other(format!("write: {e}")));
        }
        if let Err(e) = self.restart_core().await {
            return self.rollback(&prev, format!("restart: {e}")).await;
        }
        // 探活窗口必须盖过「失败→自动重启→成功」的全过程，否则会误判回滚。
        if let Err(e) = self.probe_loop(Duration::from_secs(25)).await {
            return self.rollback(&prev, format!("probe: {e}")).await;
        }
        self.store.bump_config_version();
        Ok(())
    }

    /// 回滚到指定快照版本。
    pub async fn rollback_to(&self, version: i64) -> Result<(), ApplyError> {
        let _guard = self.mu.lock().await;
        let (bin, eng, home) = {
            let g = self.inner.lock().await;
            (g.bin.clone(), g.eng.clone(), g.home.clone())
        };
        let raw = match self.store.snapshot_config(version) {
            Ok(r) => r,
            Err(e) => return Err(ApplyError::Other(e)),
        };
        if let Err(e) = inspect::validate_doc(&raw, &eng) {
            return Err(ApplyError::Other(e));
        }
        let res = check(&bin, &eng, &home, &raw).await;
        if !res.ok {
            return Err(ApplyError::Check {
                detail: res.output,
                engine: eng,
            });
        }
        let cur = self.store.config().unwrap_or_default();
        if !cur.is_empty() {
            let _ = self.store.take_snapshot("回滚前自动备份");
        }
        if let Err(e) = self.store.write_config(&raw) {
            return Err(ApplyError::Other(e.to_string()));
        }
        if let Err(e) = self.restart_core().await {
            return self.rollback(&cur, e).await;
        }
        if let Err(e) = self.probe_loop(Duration::from_secs(25)).await {
            return self.rollback(&cur, format!("probe: {e}")).await;
        }
        self.store.bump_config_version();
        Ok(())
    }
}
