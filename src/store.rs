// 数据目录的状态层：settings / subs / nodes / config / template / snapshots。
// 所有落盘走 write_file_atomic（tmp → fsync → rename）。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde_json::{json, Value};

use crate::engine;
use crate::model::{Node, Settings, Snapshot, Subscription};
use crate::util::{marshal_indent, now_rfc3339, unix_now, write_file_atomic};

pub struct Store {
    dir: PathBuf,
    inner: RwLock<StoreInner>,
}

struct StoreInner {
    subs: Vec<Subscription>,
    settings: Settings,
    /// engine 决定 config/template 的文件名（JSON 还是 YAML）。
    engine: String,
}

impl Store {
    pub fn open(dir: &Path) -> std::io::Result<Store> {
        fs::create_dir_all(dir)?;
        fs::create_dir_all(dir.join("snapshots"))?;

        let mut settings: Settings = read_json_or_default(&dir.join("settings.json"));
        settings.engine = engine::normalize(&settings.engine);
        if settings.keep_snapshots <= 0 {
            settings.keep_snapshots = 3;
        }
        if settings.delay_workers <= 0 {
            settings.delay_workers = 3;
        }
        if settings.delay_ttl <= 0 {
            settings.delay_ttl = 300;
        }
        if settings.tproxy_port <= 0 {
            settings.tproxy_port = 12345;
        }
        if settings.clash_addr.is_empty() {
            settings.clash_addr = "127.0.0.1:9091".into();
        }
        if settings.tproxy_mode.is_empty() {
            settings.tproxy_mode = "redirect".into();
        }
        if settings.routing_mode.is_empty() {
            settings.routing_mode = "rule".into();
        }
        if settings.lan_socks_port <= 0 {
            settings.lan_socks_port = 1080;
        }
        if settings.lan_http_port <= 0 {
            settings.lan_http_port = 8118;
        }
        if settings.mixed_port <= 0 {
            settings.mixed_port = 7890;
        }
        if settings.mixed_listen.is_empty() {
            settings.mixed_listen = "127.0.0.1".into();
        }
        let subs: Vec<Subscription> = read_json_or_default(&dir.join("subs.json"));

        let s = Store {
            dir: dir.to_path_buf(),
            inner: RwLock::new(StoreInner {
                subs,
                settings,
                engine: String::new(),
            }),
        };
        {
            let mut inner = s.inner.write().unwrap();
            inner.engine = engine::normalize(&inner.settings.engine);
        }
        s.ensure_template()?;
        Ok(s)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn engine(&self) -> String {
        self.inner.read().unwrap().engine.clone()
    }

    /// 切换内核。切换后 config/template 路径指向该引擎自己的文件，
    /// 两个引擎的配置互不覆盖，切回去还能用原来那份。
    pub fn set_engine(&self, name: &str) -> std::io::Result<()> {
        {
            let mut inner = self.inner.write().unwrap();
            inner.engine = engine::normalize(name);
        }
        self.ensure_template()
    }

    fn ensure_template(&self) -> std::io::Result<()> {
        let path = self.template_path();
        if path.exists() {
            self.migrate_template_urls(&path);
            return Ok(());
        }
        let b: Vec<u8> = if engine::is_mihomo(&self.engine()) {
            default_clash_template().as_bytes().to_vec()
        } else {
            marshal_indent(&default_template())?
        };
        write_file_atomic(&path, &b)
    }

    /// 老模板把规则集指向 raw.githubusercontent.com（境内不可达，核心会 FATAL）。
    /// 只精准替换与内置默认一致的值，用户自己改过的一律不动；幂等。
    fn migrate_template_urls(&self, path: &Path) {
        if engine::is_mihomo(&self.engine()) {
            return;
        }
        let Ok(raw) = fs::read(path) else { return };
        let Ok(mut doc) = serde_json::from_slice::<Value>(&raw) else { return };
        let mut changed = false;
        if let Some(sets) = doc
            .get_mut("route")
            .and_then(|r| r.get_mut("rule_set"))
            .and_then(|v| v.as_array_mut())
        {
            for it in sets.iter_mut() {
                let Some(rs) = it.as_object_mut() else { continue };
                if let Some(url) = rs.get("url").and_then(|v| v.as_str()).map(|s| s.to_string()) {
                    match url.as_str() {
                        "https://raw.githubusercontent.com/SagerNet/sing-geosite/rule-set/geosite-cn.srs" => {
                            rs.insert("url".into(), json!("https://cdn.jsdelivr.net/gh/SagerNet/sing-geosite@rule-set/geosite-cn.srs"));
                            changed = true;
                        }
                        "https://raw.githubusercontent.com/SagerNet/sing-geoip/rule-set/geoip-cn.srs" => {
                            rs.insert("url".into(), json!("https://cdn.jsdelivr.net/gh/SagerNet/sing-geoip@rule-set/geoip-cn.srs"));
                            changed = true;
                        }
                        _ => {}
                    }
                }
                if let Some(hc) = rs.get_mut("http_client").and_then(|v| v.as_object_mut()) {
                    if let Some(d) = hc.get("detour").and_then(|v| v.as_str()) {
                        // proxy：节点全挂时下不到规则集，核心 FATAL
                        // direct：sing-box 拒绝 detour 到空 direct，同样起不来
                        if d == "proxy" || d == "direct" {
                            hc.insert("detour".into(), json!(crate::model::RULE_SET_DETOUR_TAG));
                            changed = true;
                        }
                    }
                }
                // 更老的模板用一级字段 download_detour，1.16 会被移除。
                if let Some(d) = rs.get("download_detour").and_then(|v| v.as_str()).map(|s| s.to_string()) {
                    if d == "proxy" || d == "direct" {
                        rs.remove("download_detour");
                        if !rs.contains_key("http_client") {
                            rs.insert("http_client".into(), json!({"detour": crate::model::RULE_SET_DETOUR_TAG}));
                        }
                        changed = true;
                    }
                }
            }
        }
        if !changed {
            return;
        }
        if let Ok(out) = marshal_indent(&doc) {
            if write_file_atomic(path, &out).is_ok() {
                eprintln!(
                    "已把模板里的规则集下载切到 jsDelivr 镜像并改为直连（{}）",
                    path.file_name().map(|s| s.to_string_lossy()).unwrap_or_default()
                );
            }
        }
    }

    /// 当前引擎的生效配置：sing-box 用 config.json，mihomo 用 config.yaml。
    pub fn config_path(&self) -> PathBuf {
        self.dir.join(engine::config_file(&self.engine()))
    }

    pub fn template_path(&self) -> PathBuf {
        self.dir.join(engine::template_file(&self.engine()))
    }

    fn nodes_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("nodes-{id}.json"))
    }

    /* ------------------------------------------------------------ settings */

    pub fn settings(&self) -> Settings {
        self.inner.read().unwrap().settings.clone()
    }

    pub fn save_settings(&self, v: Settings) -> std::io::Result<()> {
        let mut v = v;
        v.engine = engine::normalize(&v.engine);
        let data = marshal_indent(&v)?;
        {
            let mut inner = self.inner.write().unwrap();
            inner.settings = v.clone();
            inner.engine = v.engine.clone();
        }
        write_file_atomic(&self.dir.join("settings.json"), &data)
    }

    /// 每次成功写入配置后自增，表示「当前生效的是第几版」。
    pub fn bump_config_version(&self) -> i64 {
        let mut inner = self.inner.write().unwrap();
        inner.settings.config_version += 1;
        let v = inner.settings.config_version;
        if let Ok(data) = marshal_indent(&inner.settings) {
            let _ = write_file_atomic(&self.dir.join("settings.json"), &data);
        }
        v
    }

    pub fn set_last_check(&self, ok: bool, at: &str) {
        let mut inner = self.inner.write().unwrap();
        inner.settings.last_check_ok = ok;
        inner.settings.last_check_at = at.to_string();
        if let Ok(data) = marshal_indent(&inner.settings) {
            let _ = write_file_atomic(&self.dir.join("settings.json"), &data);
        }
    }

    /* ------------------------------------------------------------ subscriptions */

    pub fn subscriptions(&self) -> Vec<Subscription> {
        self.inner.read().unwrap().subs.clone()
    }

    pub fn subscription(&self, id: &str) -> Option<Subscription> {
        self.inner
            .read()
            .unwrap()
            .subs
            .iter()
            .find(|s| s.id == id)
            .cloned()
    }

    pub fn upsert_subscription(&self, sub: Subscription) -> std::io::Result<()> {
        let data;
        {
            let mut inner = self.inner.write().unwrap();
            if let Some(slot) = inner.subs.iter_mut().find(|s| s.id == sub.id) {
                *slot = sub.clone();
            } else {
                inner.subs.push(sub.clone());
            }
            data = marshal_indent(&inner.subs)?;
        }
        write_file_atomic(&self.dir.join("subs.json"), &data)
    }

    pub fn delete_subscription(&self, id: &str) -> std::io::Result<()> {
        let data;
        {
            let mut inner = self.inner.write().unwrap();
            inner.subs.retain(|s| s.id != id);
            data = marshal_indent(&inner.subs)?;
        }
        let _ = fs::remove_file(self.nodes_path(id));
        write_file_atomic(&self.dir.join("subs.json"), &data)
    }

    /* ------------------------------------------------------------ nodes */

    pub fn nodes(&self, sub_id: &str) -> std::io::Result<Vec<Node>> {
        let path = self.nodes_path(sub_id);
        match fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    pub fn save_nodes(&self, sub_id: &str, nodes: &[Node]) -> std::io::Result<()> {
        let data = marshal_indent(&nodes)?;
        write_file_atomic(&self.nodes_path(sub_id), &data)
    }

    pub fn all_nodes(&self) -> Vec<Node> {
        let mut all = Vec::new();
        for sub in self.subscriptions() {
            if let Ok(nodes) = self.nodes(&sub.id) {
                all.extend(nodes);
            }
        }
        all
    }

    pub fn set_node_enabled(&self, id: &str, enabled: bool) -> Result<(), crate::store::ErrNotFound> {
        for sub in self.subscriptions() {
            if let Ok(mut nodes) = self.nodes(&sub.id) {
                let mut hit = false;
                for n in nodes.iter_mut() {
                    if n.id == id {
                        n.enabled = enabled;
                        hit = true;
                    }
                }
                if hit {
                    let _ = self.save_nodes(&sub.id, &nodes);
                    return Ok(());
                }
            }
        }
        Err(ErrNotFound)
    }

    /* ------------------------------------------------------------ config */

    /// 文件不存在时返回空（与 Go 的 nil, nil 一致）。
    pub fn config(&self) -> std::io::Result<Vec<u8>> {
        match fs::read(self.config_path()) {
            Ok(b) => Ok(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    pub fn write_config(&self, b: &[u8]) -> std::io::Result<()> {
        write_file_atomic(&self.config_path(), b)
    }

    /// 当前引擎的模板原文：sing-box 是 JSON，mihomo 是 YAML。
    pub fn template_raw(&self) -> std::io::Result<Vec<u8>> {
        match fs::read(self.template_path()) {
            Ok(b) => Ok(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.ensure_template()?;
                fs::read(self.template_path())
            }
            Err(e) => Err(e),
        }
    }

    #[allow(dead_code)]
    pub fn save_template_raw(&self, b: &[u8]) -> std::io::Result<()> {
        write_file_atomic(&self.template_path(), b)
    }

    /* ------------------------------------------------------------ snapshots */

    fn snap_index_path(&self) -> PathBuf {
        self.dir.join("snapshots").join("index.json")
    }

    pub fn snapshots(&self) -> Vec<Snapshot> {
        let mut list: Vec<Snapshot> = read_json_or_default(&self.snap_index_path());
        list.sort_by(|a, b| b.version.cmp(&a.version));
        list
    }

    /// 把当前生效配置另存为一份快照，并按 keep 裁剪。
    pub fn take_snapshot(&self, note: &str) -> Result<Snapshot, String> {
        let cur = self.config().map_err(|e| e.to_string())?;
        if cur.is_empty() {
            return Err("no config to snapshot".into());
        }
        let mut list = self.snapshots();
        let mut version = 1;
        for it in &list {
            if it.version >= version {
                version = it.version + 1;
            }
        }
        let file = format!("v{}-{}{}", version, unix_now(), engine::ext(&self.engine()));
        write_file_atomic(&self.dir.join("snapshots").join(&file), &cur)
            .map_err(|e| e.to_string())?;
        let snap = Snapshot {
            version,
            time: now_rfc3339(),
            note: note.to_string(),
            size: cur.len(),
            file: file.clone(),
        };
        list.insert(0, snap.clone());

        let mut keep = self.settings().keep_snapshots;
        if keep <= 0 {
            keep = 3;
        }
        for it in list.iter().skip(keep as usize) {
            let _ = fs::remove_file(self.dir.join("snapshots").join(&it.file));
        }
        list.truncate(keep as usize);
        let data = marshal_indent(&list).map_err(|e| e.to_string())?;
        write_file_atomic(&self.snap_index_path(), &data).map_err(|e| e.to_string())?;
        self.gc_snapshots(&list);
        Ok(snap)
    }

    /// 回收索引之外的孤立快照文件，避免历史遗留文件占满 flash。
    fn gc_snapshots(&self, list: &[Snapshot]) {
        let mut keep: std::collections::HashSet<String> = ["index.json".to_string()].into_iter().collect();
        for it in list {
            if !it.file.is_empty() {
                keep.insert(it.file.clone());
            }
        }
        let Ok(entries) = fs::read_dir(self.dir.join("snapshots")) else { return };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if keep.contains(&name) {
                continue;
            }
            if name.ends_with(".json") || name.ends_with(".yaml") {
                let _ = fs::remove_file(e.path());
            }
        }
    }

    pub fn snapshot_config(&self, version: i64) -> Result<Vec<u8>, String> {
        for it in self.snapshots() {
            if it.version != version {
                continue;
            }
            if it.file.is_empty() {
                // 兼容旧索引：按 v<version>-*.{json,yaml} 兜底查找
                for ext in [".json", ".yaml"] {
                    let prefix = format!("v{version}-");
                    if let Ok(entries) = fs::read_dir(self.dir.join("snapshots")) {
                        for e in entries.flatten() {
                            let name = e.file_name().to_string_lossy().into_owned();
                            if name.starts_with(&prefix) && name.ends_with(ext) {
                                return fs::read(e.path()).map_err(|e| e.to_string());
                            }
                        }
                    }
                }
                return Err("not found".into());
            }
            return fs::read(self.dir.join("snapshots").join(&it.file)).map_err(|e| e.to_string());
        }
        Err("not found".into())
    }
}

pub struct ErrNotFound;

fn read_json_or_default<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    match fs::read(path) {
        Ok(b) => serde_json::from_slice::<T>(&b).unwrap_or_default(),
        Err(_) => T::default(),
    }
}

/* ------------------------------------------------------------ 内置模板 */

/// sing-box 默认模板。redirect/dns 入站只在 Linux 常驻：
/// Windows 上保留会导致核心启动即失败并进入崩溃循环。
pub fn default_template() -> Value {
    let mut inbounds = vec![json!({
        "type": "mixed", "tag": "mixed-in",
        "listen": "127.0.0.1", "listen_port": 7890
    })];
    if cfg!(target_os = "linux") {
        inbounds.push(json!({
            "type": "redirect", "tag": "redirect-in",
            "listen": "0.0.0.0", "listen_port": 12345
        }));
        inbounds.push(json!({
            "type": "direct", "tag": "dns-in",
            "listen": "127.0.0.1", "listen_port": 53
        }));
        inbounds.push(json!({
            "type": "direct", "tag": "dns-lan",
            "listen": "0.0.0.0", "listen_port": 5354
        }));
    }
    json!({
        "log": {"level": "info", "timestamp": true},
        "dns": {
            "servers": [
                {"tag": "local", "type": "udp", "server": "223.5.5.5"},
                {"tag": "remote", "type": "https", "server": "1.1.1.1", "detour": "proxy"}
            ],
            "rules": [
                {"rule_set": ["geosite-cn"], "server": "local"}
            ],
            "final": "remote"
        },
        "inbounds": inbounds,
        "outbounds": [],
        "route": {
            "rules": [
                {"action": "sniff"},
                {"protocol": "dns", "action": "hijack-dns"},
                {"ip_is_private": true, "outbound": "direct"},
                {"rule_set": ["geosite-cn"], "outbound": "direct"},
                {"rule_set": ["geoip-cn"], "outbound": "direct"}
            ],
            "rule_set": [
                {
                    "tag": "geosite-cn", "type": "remote", "format": "binary",
                    "url": "https://cdn.jsdelivr.net/gh/SagerNet/sing-geosite@rule-set/geosite-cn.srs",
                    "http_client": {"detour": crate::model::RULE_SET_DETOUR_TAG},
                    "update_interval": "7d"
                },
                {
                    "tag": "geoip-cn", "type": "remote", "format": "binary",
                    "url": "https://cdn.jsdelivr.net/gh/SagerNet/sing-geoip@rule-set/geoip-cn.srs",
                    "http_client": {"detour": crate::model::RULE_SET_DETOUR_TAG},
                    "update_interval": "7d"
                }
            ],
            "final": "proxy",
            "auto_detect_interface": true,
            "default_domain_resolver": {"server": "local"}
        },
        "experimental": {
            "clash_api": {
                "external_controller": "127.0.0.1:9091",
                "default_mode": "rule"
            },
            "cache_file": {"enabled": true, "path": "cache.db"}
        }
    })
}

/// mihomo 引擎的内置模板（YAML 文本），与 Go 版逐字一致。
pub fn default_clash_template() -> &'static str {
    r#"# mihomo 模板（YAML）。面板会覆盖以下字段，手改这些字段不会生效：
#   proxies / proxy-groups / rules（规则分流模式下） / mixed-port / bind-address
#   allow-lan / redir-port / dns.listen / tun / listeners
mixed-port: 7890
allow-lan: false
bind-address: 127.0.0.1
mode: rule
log-level: info
ipv6: false
external-controller: 127.0.0.1:9091
unified-delay: true
tcp-concurrent: true
find-process-mode: 'off'
profile:
  store-selected: true
  store-fake-ip: false

# geo 数据源。mihomo 默认从 github.com 下载 geosite.dat / country.mmdb，
# 国内直连经常失败（表现为首次应用配置卡住 90 秒后报错），这里换成 jsDelivr CDN。
# 下载结果会缓存到数据目录，只在第一次用到时联网；内网离线环境可以提前把
# GeoSite.dat / geoip.metadb 拷进数据目录，或把这里改成自己的镜像。
geox-url:
  geoip: https://cdn.jsdelivr.net/gh/MetaCubeX/meta-rules-dat@release/geoip.dat
  geosite: https://cdn.jsdelivr.net/gh/MetaCubeX/meta-rules-dat@release/geosite.dat
  mmdb: https://cdn.jsdelivr.net/gh/MetaCubeX/meta-rules-dat@release/country.mmdb

dns:
  enable: true
  ipv6: false
  # 局域网 DNS 接管（nft 把 LAN 的 53 端口重定向到这里）
  listen: 0.0.0.0:5354
  enhanced-mode: redir-host
  nameserver:
    - 223.5.5.5
    - 119.29.29.29
  # 解析节点域名用国内 DNS，避免「DNS 走代理、代理要解析域名」的死循环
  proxy-server-nameserver:
    - 223.5.5.5

proxies: []
proxy-groups: []
rules:
  - IP-CIDR,127.0.0.0/8,DIRECT,no-resolve
  - IP-CIDR,10.0.0.0/8,DIRECT,no-resolve
  - IP-CIDR,172.16.0.0/12,DIRECT,no-resolve
  - IP-CIDR,192.168.0.0/16,DIRECT,no-resolve
  - IP-CIDR,100.64.0.0/10,DIRECT,no-resolve
  - IP-CIDR,169.254.0.0/16,DIRECT,no-resolve
  - GEOSITE,cn,DIRECT
  - GEOIP,CN,DIRECT
  - MATCH,proxy
"#
}
