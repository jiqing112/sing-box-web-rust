// 数据模型与 Go internal/model 一一对应，JSON 字段名完全一致，
// 老版本 panel-data 目录可以直接迁过来用。

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const RULE_SET_DETOUR_TAG: &str = "rule-set";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Subscription {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub auto: bool,
    /// 自动更新间隔（小时）；0 表示旧数据未设置，按 24 小时处理
    #[serde(default, deserialize_with = "crate::util::null_default::deserialize")]
    pub interval_hours: i64,
    #[serde(default)]
    pub node_count: usize,
    /// RFC3339 字符串（与 Go time.Time 的 JSON 形态兼容）
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub usage_used: i64,
    #[serde(default)]
    pub usage_total: i64,
    #[serde(default)]
    pub expire: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_error: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub tag: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub server: String,
    #[serde(default)]
    pub port: i64,
    #[serde(default)]
    pub region: String,
    #[serde(default)]
    pub sub_id: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, deserialize_with = "crate::util::null_default::deserialize")]
    pub outbound: Map<String, Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: i64,
    /// RFC3339
    pub time: String,
    #[serde(default)]
    pub note: String,
    pub size: usize,
    pub file: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub listen: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password_hash: String,
    /// sing-box（默认）或 mihomo；空值按 sing-box 处理
    #[serde(default)]
    pub engine: String,
    #[serde(default)]
    pub clash_addr: String,
    #[serde(default)]
    pub login_disabled: bool,
    #[serde(default)]
    pub keep_snapshots: i64,
    #[serde(default)]
    pub delay_ttl: i64,
    #[serde(default)]
    pub delay_workers: i64,
    #[serde(default)]
    pub tproxy_mode: String,
    #[serde(default)]
    pub tproxy_enabled: bool,
    #[serde(default)]
    pub tproxy_local: bool,
    #[serde(default)]
    pub tproxy_dns: bool,
    #[serde(default)]
    pub tun_enabled: bool,
    /// rule = 国内直连其余走节点；global = 全部走节点；direct = 全部直连
    #[serde(default)]
    pub routing_mode: String,
    #[serde(default)]
    pub lan_proxy: bool,
    #[serde(default)]
    pub lan_socks_port: i64,
    #[serde(default)]
    pub lan_http_port: i64,
    #[serde(default)]
    pub lan_user: String,
    #[serde(default)]
    pub lan_pass: String,
    #[serde(default)]
    pub tproxy_port: i64,
    #[serde(default, deserialize_with = "crate::util::null_default::deserialize")]
    pub lan_cidrs: Vec<String>,
    #[serde(default)]
    pub mixed_port: i64,
    #[serde(default)]
    pub mixed_listen: String,
    /// None = 未设置（视为 true，兼容旧配置）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autostart_core: Option<bool>,
    #[serde(default)]
    pub config_version: i64,
    #[serde(default)]
    pub last_check_at: String,
    #[serde(default)]
    pub last_check_ok: bool,
    #[serde(default)]
    pub lan_if: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            listen: "127.0.0.1:9090".into(),
            username: "admin".into(),
            password_hash: "".into(),
            engine: "sing-box".into(),
            clash_addr: "127.0.0.1:9091".into(),
            login_disabled: false,
            keep_snapshots: 3,
            delay_ttl: 300,
            delay_workers: 3,
            tproxy_mode: "redirect".into(),
            tproxy_enabled: false,
            tproxy_local: false,
            tproxy_dns: false,
            tun_enabled: false,
            routing_mode: "".into(),
            lan_proxy: false,
            lan_socks_port: 0,
            lan_http_port: 0,
            lan_user: "".into(),
            lan_pass: "".into(),
            tproxy_port: 0,
            lan_cidrs: vec![],
            mixed_port: 0,
            mixed_listen: "".into(),
            autostart_core: None,
            config_version: 0,
            last_check_at: "".into(),
            last_check_ok: false,
            lan_if: "".into(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct LogLine {
    pub at: String,
    pub raw: String,
}

/// 把订阅里常见的简写归一成 sing-box 的 outbound 类型名。
/// ss → shadowsocks 等；渲染时也要再调一次，兼容库里已存的老数据。
pub fn normalize_outbound_type(t: &str) -> String {
    match t.trim().to_lowercase().as_str() {
        "ss" | "shadowsocks" => "shadowsocks".into(),
        "hy2" | "hysteria2" => "hysteria2".into(),
        "socks5" | "socks" => "socks".into(),
        _ => t.trim().to_lowercase(),
    }
}
