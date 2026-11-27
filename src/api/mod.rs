// HTTP API 层：路由、认证、全部 REST 处理器与两个 WebSocket 桥。
// 接口路径与响应字段和 Go 版完全一致，前端零改动。

mod settings;

use std::collections::{HashMap, HashSet};
use portable_atomic::AtomicI64;
use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post, put};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::clashclient::Client as ClashClient;
use crate::config::{Applier, CheckResult};
use crate::engine;
use crate::geodata;
use crate::listen::ListenManager;
use crate::model::{Node, Subscription};
use crate::store::Store;
use crate::subscription;
use crate::supervisor::Supervisor;
use crate::tproxy;

/// 面板版本。前端从 /status 读取。
    pub const VERSION: &str = "1.5.0";

/// 覆盖「校验 + 重启 + 探活」全过程；mihomo 首次校验会下载 geo 数据。
/// 最坏路径 90（校验）+ 8（重启）+ 25（探活）≈ 124s，超时会把 apply 半途
/// cancel 掉，留下「配置已写、核心未启」的中间态——必须大于最坏耗时。
pub const APPLY_TIMEOUT: Duration = Duration::from_secs(150);
const TOKEN_TTL: i64 = 7 * 24 * 3600;
const MAX_TOKENS: usize = 64;
/// 限制单次测速的节点数，防止超大订阅把面板拖死。
const MAX_DELAY_TAGS: usize = 2000;

pub(crate) const DEFAULT_SUB_INTERVAL_HOURS: i64 = 24;
pub(crate) const MAX_SUB_INTERVAL_HOURS: i64 = 720;

pub(crate) fn valid_sub_interval(h: i64) -> bool {
    (1..=MAX_SUB_INTERVAL_HOURS).contains(&h)
}

pub struct Server {
    pub store: Arc<Store>,
    pub sup: Arc<Supervisor>,
    pub clash: Arc<ClashClient>,
    pub applier: Arc<Applier>,
    pub subs: subscription::Service,
    /// 「内核名 → 可执行文件路径」，切换内核时按当前引擎取。
    pub bins: HashMap<String, String>,
    pub tp: Arc<tproxy::Manager>,
    pub geo: Arc<geodata::Manager>,
    pub start: std::time::Instant,

    tokens: std::sync::Mutex<HashMap<String, i64>>,
    throttle: std::sync::Mutex<HashMap<String, Attempt>>,

    pub(crate) static_token: String,
    pub(crate) last_check: RwLock<CheckResult>,

    /// 监听地址热切换的载体：main 在绑定完成后注入。
    pub listen: tokio::sync::Mutex<Option<ListenManager>>,
    pub router: RwLock<Option<Router>>,
    pub(crate) listen_pinned: std::sync::atomic::AtomicBool,

    /// 出口 IP 缓存：(时刻, (ip, 测量路径))；None 表示上次测量失败。
    /// 经 mixed 入站访问 IP 回显服务测得，TTL 内直接复用。
    exit_ip: tokio::sync::Mutex<Option<(std::time::Instant, Option<(String, String)>)>>,

    /// 内核切换/应用的互斥：h_engine_set 要连续改 store→supervisor→applier
    /// 三处状态，并发切换（双击）或切换与应用并发会把三处指向不同内核。
    pub(crate) engine_mu: tokio::sync::Mutex<()>,

    /// 最近一次 API 请求的时间戳（UnixNano），空闲回收据此判断。
    pub last_req: AtomicI64,
}

#[derive(Clone)]
struct Attempt {
    fails: i64,
    until: std::time::Instant,
    last: std::time::Instant,
}

// 登录限速：同一来源连续失败 5 次后锁定 30 秒，之后每多失败一次翻倍，最长 15 分钟。
const LOGIN_MAX_FAILS: i64 = 5;
const LOGIN_BASE_LOCK: Duration = Duration::from_secs(30);
const LOGIN_MAX_LOCK: Duration = Duration::from_secs(900);

impl Server {
    pub fn new(
        store: Arc<Store>,
        sup: Arc<Supervisor>,
        clash: Arc<ClashClient>,
        bins: HashMap<String, String>,
        static_token: &str,
        tp: Arc<tproxy::Manager>,
    ) -> Arc<Server> {
        let applier = Arc::new(Applier::new(
            store.clone(),
            bins.get(&engine::normalize(&store.engine())).map(|s| s.as_str()).unwrap_or(""),
            sup.clone(),
            clash.clone(),
        ));
        let geo = geodata::Manager::new(engine::home(store.dir(), engine::MIHOMO));
        Arc::new(Server {
            store,
            sup,
            clash,
            applier,
            subs: subscription::Service::new(),
            bins,
            tp,
            geo,
            start: std::time::Instant::now(),
            tokens: std::sync::Mutex::new(HashMap::new()),
            throttle: std::sync::Mutex::new(HashMap::new()),
            static_token: static_token.to_string(),
            last_check: RwLock::new(CheckResult {
                ok: false,
                output: String::new(),
                elapsed_ms: 0,
            }),
            listen: tokio::sync::Mutex::new(None),
            router: RwLock::new(None),
            listen_pinned: std::sync::atomic::AtomicBool::new(false),
            exit_ip: tokio::sync::Mutex::new(None),
            engine_mu: tokio::sync::Mutex::new(()),
            last_req: AtomicI64::new(0),
        })
    }

    pub(crate) fn engine_name(&self) -> String {
        engine::normalize(&self.store.engine())
    }

    pub(crate) fn bin(&self) -> String {
        self.bins
            .get(&self.engine_name())
            .cloned()
            .unwrap_or_default()
    }

    /// 用「当前设置 + 当前引擎的模板」重新渲染配置。
    pub(crate) async fn render_config(&self, set: &crate::model::Settings) -> Result<Vec<u8>, String> {
        let tpl_raw = self.store.template_raw().map_err(|e| format!("读取模板失败: {e}"))?;
        let opts =
            crate::config::RenderOpts::from_settings(set, self.store.dir());
        crate::config::render_bytes(&tpl_raw, &self.store.all_nodes(), &opts)
    }

    /* ------------------------------------------------------------ router */

    pub fn router(self: &Arc<Self>) -> Router {
        Router::new()
            .route("/api/v1/auth/login", post(Self::h_login))
            .route("/api/v1/auth/status", get(Self::h_auth_status))
            .route("/api/v1/auth/setup", post(Self::h_auth_setup))
            .route("/api/v1/auth/access", put(Self::h_auth_access))
            .route("/api/v1/auth/check", get(Self::h_check))
            .route("/api/v1/settings/listen", put(Self::h_listen_set))
            .route("/api/v1/status", get(Self::h_status))
            .route("/api/v1/exitip", get(Self::h_exit_ip))
            .route("/api/v1/logs", get(Self::h_logs))
            .route("/api/v1/service/{action}", post(Self::h_service))
            .route("/api/v1/engine", get(Self::h_engine_get).put(Self::h_engine_set))
            .route("/api/v1/geodata", get(Self::h_geo_status))
            .route("/api/v1/geodata/prefetch", post(Self::h_geo_prefetch))
            .route(
                "/api/v1/subscriptions",
                get(Self::h_list_subs).post(Self::h_create_sub),
            )
            .route(
                "/api/v1/subscriptions/{id}",
                put(Self::h_update_sub).delete(Self::h_delete_sub),
            )
            .route("/api/v1/subscriptions/{id}/refresh", post(Self::h_refresh_sub))
            .route("/api/v1/nodes", get(Self::h_list_nodes))
            .route("/api/v1/nodes/delay", post(Self::h_delay_nodes))
            .route("/api/v1/nodes/speed", post(Self::h_speed_nodes))
            .route("/api/v1/nodes/{id}", put(Self::h_toggle_node))
            .route(
                "/api/v1/settings/speedtest",
                get(Self::h_speedtest_get).put(Self::h_speedtest_put),
            )
            .route("/api/v1/setup/finish", post(Self::h_setup_finish))
            .route("/api/v1/setup/complete", post(Self::h_setup_complete))
            .route("/api/v1/config", get(Self::h_get_config))
            .route("/api/v1/config/render", post(Self::h_render_config))
            .route("/api/v1/config/validate", post(Self::h_validate_config))
            .route("/api/v1/config/apply", post(Self::h_apply_config))
            .route("/api/v1/config/snapshots", get(Self::h_snapshots))
            .route("/api/v1/config/rollback", post(Self::h_rollback))
            .route("/api/v1/tproxy", get(Self::h_tproxy_status).put(Self::h_tproxy_set))
            .route(
                "/api/v1/lanproxy",
                get(Self::h_lan_proxy_get).put(Self::h_lan_proxy_set),
            )
            .route("/api/v1/tun", get(Self::h_tun_get).put(Self::h_tun_set))
            .route("/api/v1/sysproxy", get(Self::h_sysproxy_get).put(Self::h_sysproxy_set))
            .route("/api/v1/update", get(Self::h_update_get).post(Self::h_update_set))
            .route(
                "/api/v1/autostart",
                get(Self::h_autostart_get).put(Self::h_autostart_set),
            )
            .route(
                "/api/v1/routing",
                get(Self::h_routing_get).put(Self::h_routing_set),
            )
            .route("/api/v1/ws/connections", get(Self::h_ws_connections))
            .route("/api/v1/ws/traffic", get(Self::h_ws_traffic))
            .route("/api/v1/clash/{*path}", any(Self::h_clash_proxy))
            .layer(axum::middleware::from_fn(mark_req_mw))
            .layer(axum::extract::DefaultBodyLimit::max(8 << 20))
            .fallback(Self::h_fallback)
            .with_state(self.clone())
    }

    /* ------------------------------------------------------------ auth helpers */

    /// 面板是否启用登录。面板内开关关闭登录，或用户名和密码哈希都为空时，
    /// API 与 WebSocket 均不校验 Token。
    pub(crate) fn login_required(&self) -> bool {
        let set = self.store.settings();
        if set.login_disabled {
            return false;
        }
        !set.username.is_empty() || !set.password_hash.is_empty()
    }

    /// 全新面板、还没有任何登录凭据，需要用户在网页上完成首次账户设置。
    /// 面板内「允许免密码访问」（login_disabled）优先于本状态。
    pub(crate) fn needs_setup(&self) -> bool {
        let set = self.store.settings();
        !set.login_disabled && set.password_hash.is_empty()
    }

    fn valid_token_string(&self, tok: &str) -> bool {
        if tok.is_empty() {
            return false;
        }
        if !self.static_token.is_empty() && tok == self.static_token {
            return true;
        }
        let now = crate::util::unix_now();
        let g = self.tokens.lock().unwrap();
        g.get(tok).map(|exp| now < *exp).unwrap_or(false)
    }

    fn valid_token(&self, headers: &HeaderMap) -> bool {
        let Some(h) = headers.get(axum::http::header::AUTHORIZATION).and_then(|v| v.to_str().ok()) else {
            return false;
        };
        if let Some(tok) = h.strip_prefix("Bearer ") {
            return self.valid_token_string(tok);
        }
        false
    }

    /// 统一入口认证：返回 Some(resp) 表示未登录，直接回给客户端。
    /// 待初始化（needs_setup）时同样全部拦截，逼出首次账户设置流程。
    pub(crate) fn authed(&self, headers: &HeaderMap) -> Option<Response> {
        if (self.login_required() || self.needs_setup()) && !self.valid_token(headers) {
            return Some(err_json(StatusCode::UNAUTHORIZED, "unauthorized", "未登录或登录已过期"));
        }
        None
    }

    /// WebSocket 的认证：支持 Authorization 头或 ?token= 查询参数。
    pub(crate) fn authed_ws(&self, headers: &HeaderMap, query: &str) -> Option<Response> {
        if !self.login_required() && !self.needs_setup() {
            return None;
        }
        if self.valid_token(headers) {
            return None;
        }
        let qtok = query
            .split('&')
            .find_map(|kv| kv.strip_prefix("token="))
            .unwrap_or("");
        if qtok.is_empty() {
            return Some(err_json(StatusCode::UNAUTHORIZED, "unauthorized", "未登录"));
        }
        let decoded: String = percent_encoding::percent_decode_str(qtok)
            .decode_utf8_lossy()
            .into_owned();
        if !self.valid_token_string(&decoded) {
            return Some(err_json(StatusCode::UNAUTHORIZED, "unauthorized", "token 无效"));
        }
        None
    }

    // 记录登录令牌：先清过期项，再按最旧过期时间淘汰，保证 map 有界。
    fn store_token(&self, tok: String) {
        let now = crate::util::unix_now();
        let mut g = self.tokens.lock().unwrap();
        g.retain(|_, exp| now < *exp);
        while g.len() >= MAX_TOKENS {
            let oldest = g
                .iter()
                .min_by_key(|(_, exp)| **exp)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => {
                    g.remove(&k);
                }
                None => break,
            }
        }
        g.insert(tok, now + TOKEN_TTL);
    }

    pub(crate) fn hash_password(user: &str, pass: &str) -> String {
        crate::util::sha256_hex(format!("{user}:{pass}").as_bytes())
    }

    fn client_ip(addr: Option<std::net::SocketAddr>) -> String {
        addr.map(|a| a.ip().to_string()).unwrap_or_default()
    }

    /* ------------------------------------------------------------ subscriptions helpers */

    /// 收集其他订阅已占用的 tag，避免跨订阅重名。
    pub(crate) fn reserved_tags(&self, except_sub_id: &str) -> HashSet<String> {
        self.store
            .all_nodes()
            .iter()
            .filter(|n| n.sub_id != except_sub_id)
            .map(|n| n.tag.clone())
            .collect()
    }

    /// 拉取并保存单个订阅，失败时只记录错误，不影响其它订阅。
    pub(crate) async fn refresh_one(&self, sub: &Subscription) -> Result<usize, String> {
        let prev = self.store.nodes(&sub.id).unwrap_or_default();
        let mut reserved = self.reserved_tags(&sub.id);
        let (nodes, info, not_modified) = match self.subs.refresh(sub, &prev, &mut reserved).await
        {
            Ok(x) => x,
            Err(e) => {
                let mut bad = sub.clone();
                bad.last_error = e.clone();
                let _ = self.store.upsert_subscription(bad);
                return Err(e);
            }
        };
        if not_modified {
            // 机场用 204 表示「内容未变化/请求太频繁」：保留既有节点，
            // 只刷新时间戳并清掉旧错误，不算失败。
            let mut sub = sub.clone();
            sub.updated_at = crate::util::now_rfc3339();
            sub.last_error = String::new();
            if info.total > 0 {
                sub.usage_used = info.upload + info.download;
                sub.usage_total = info.total;
                sub.expire = info.expire;
            }
            let _ = self.store.upsert_subscription(sub.clone());
            self.spawn_reapply_and_test(&sub);
            return Ok(prev.len());
        }
        self.store.save_nodes(&sub.id, &nodes).map_err(|e| e.to_string())?;
        let mut sub = sub.clone();
        sub.node_count = nodes.len();
        sub.updated_at = crate::util::now_rfc3339();
        sub.last_error = String::new();
        if info.total > 0 {
            sub.usage_used = info.upload + info.download;
            sub.usage_total = info.total;
            sub.expire = info.expire;
        }
        let _ = self.store.upsert_subscription(sub.clone());
        self.spawn_reapply_and_test(&sub);
        Ok(nodes.len())
    }

    /// 订阅更新成功（含 204）后，后台补一轮「重新下发配置 → 延迟测速」。
    /// 关键点：节点库变了不等于内核变了——机场轮换 REALITY 参数后，内核
    /// 还拿着旧出站跑，探活/流量会全部失败，必须重新渲染下发。
    fn spawn_reapply_and_test(&self, sub: &Subscription) {
        let Some(app) = GLOBAL_SERVER.get() else {
            return;
        };
        let sub = sub.clone();
        tokio::spawn(async move {
            app.reapply_and_test_after_refresh(&sub).await;
        });
    }

    /// 订阅更新后的完整收尾：重新渲染并与当前生效配置对比，变了才 apply
    ///（内容未变不重启核心），最后测一轮延迟、按设置停用超时节点。
    /// apply 失败时跳过测速——内核还是旧配置，测了必然全超时，会把
    /// 刚启用的节点全部误杀。
    pub(crate) async fn reapply_and_test_after_refresh(&self, sub: &Subscription) {
        if !self.sup.running() {
            eprintln!("订阅 {} 更新：核心未运行，跳过重新下发与测速", sub.name);
            return;
        }
        // 与引擎切换互斥，避免渲染到一半内核被换掉
        let _engine_g = self.engine_mu.lock().await;
        let mut ready = false;
        match self.render_config(&self.store.settings()).await {
            Ok(raw) => {
                let cur = self.store.config().unwrap_or_default();
                if cur == raw {
                    eprintln!("订阅 {} 更新：节点配置未变化，跳过重新下发", sub.name);
                    ready = true;
                } else {
                    match tokio::time::timeout(
                        APPLY_TIMEOUT,
                        self.applier.apply(&raw, &format!("订阅 {} 更新，节点变更", sub.name)),
                    )
                    .await
                    {
                        Ok(Ok(())) => {
                            eprintln!("订阅 {} 更新：配置已重新下发", sub.name);
                            self.clear_exit_ip().await;
                            ready = true;
                        }
                        Ok(Err(e)) => {
                            eprintln!("订阅 {} 更新：重新下发配置失败，跳过测速: {e}", sub.name)
                        }
                        Err(_) => {
                            eprintln!("订阅 {} 更新：重新下发配置超时，跳过测速", sub.name)
                        }
                    }
                }
            }
            Err(e) => eprintln!("订阅 {} 更新：重新渲染失败，跳过测速: {e}", sub.name),
        }
        if ready {
            self.test_sub_nodes_after_refresh(sub).await;
        }
    }

    /// 每分钟检查一次，按每个订阅自己的间隔重新拉取。
    pub fn start_scheduler(self: &Arc<Self>) {
        let app = self.clone();
        tokio::spawn(async move {
            // 失败重试下限：订阅源故障时不至于每分钟重试、刷日志。
            const RETRY_FLOOR: Duration = Duration::from_secs(600);
            let mut last_fail: HashMap<String, std::time::Instant> = HashMap::new();
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                for sub in app.store.subscriptions() {
                    if !sub.auto {
                        continue;
                    }
                    let mut h = sub.interval_hours;
                    if !valid_sub_interval(h) {
                        h = DEFAULT_SUB_INTERVAL_HOURS;
                    }
                    if seconds_since_rfc3339(&sub.updated_at).unwrap_or(i64::MAX) < h * 3600 {
                        continue;
                    }
                    if let Some(t) = last_fail.get(&sub.id) {
                        if t.elapsed() < RETRY_FLOOR {
                            continue;
                        }
                    }
                    match app.refresh_one(&sub).await {
                        Err(e) => {
                            last_fail.insert(sub.id.clone(), std::time::Instant::now());
                            eprintln!("自动更新 {} 失败: {e}", sub.name);
                        }
                        Ok(n) => {
                            last_fail.remove(&sub.id);
                            eprintln!("自动更新 {} 完成，{n} 个节点", sub.name);
                            // 更新后默认来一遍测速：新节点立即有延迟数据，
                            // 失效节点按设置自动停用（在调度循环里直接跑，本身就是后台）
                            app.test_sub_nodes_after_refresh(&sub).await;
                        }
                    }
                }
            }
        });
    }

    /// failover（按需测优）模式的保活监控：平时只探测当前出口节点（30 秒一次、
    /// 一个 204 请求），连续两次失败才全量重测启用节点并切到最快的那个。
    /// 内核侧的定时全测在渲染时已降到 24h 兜底，日常工作由本任务承担。
    pub fn start_failover_monitor(self: &Arc<Self>) {
        let app = self.clone();
        tokio::spawn(async move {
            const PROBE_EVERY: Duration = Duration::from_secs(30);
            const FAIL_THRESHOLD: i32 = 2;
            let mut fail_count: i32 = 0;
            loop {
                tokio::time::sleep(PROBE_EVERY).await;
                if app.store.settings().auto_select_mode != "failover" || !app.sup.running() {
                    continue;
                }
                // 当前真正承载流量的节点（顺着 selector → urltest 解析到底）
                let active = tokio::time::timeout(
                    Duration::from_secs(3),
                    app.clash.resolve_active("proxy"),
                )
                .await
                .ok()
                .unwrap_or_default();
                if active.is_empty() || active == "DIRECT" {
                    // DIRECT 是用户手动选的直连，不参与自动切换
                    continue;
                }
                // 探测当前节点（结果顺带写进延迟缓存，UI 里就是实时的）
                let ms = tokio::time::timeout(
                    Duration::from_secs(6),
                    app.clash.delay(&active, 5000),
                )
                .await
                .unwrap_or(Err(String::new()))
                .unwrap_or(-1);
                if ms >= 0 {
                    fail_count = 0;
                    continue;
                }
                fail_count += 1;
                if fail_count < FAIL_THRESHOLD {
                    continue;
                }
                fail_count = 0;
                // 当前节点挂了：全量重测，切到延迟最低的可用节点
                let tags: Vec<String> = app
                    .store
                    .all_nodes()
                    .iter()
                    .filter(|n| n.enabled)
                    .map(|n| n.tag.clone())
                    .collect();
                if tags.is_empty() {
                    continue;
                }
                let measured = measure_delays(&app, tags).await;
                let best = measured
                    .iter()
                    .filter(|(_, ms)| *ms >= 0)
                    .min_by_key(|(_, ms)| *ms)
                    .map(|(t, _)| t.clone());
                match best {
                    Some(best) if best != active => {
                        if app.clash.select_proxy("proxy", &best).await.is_ok() {
                            eprintln!("failover: 当前节点 {active} 故障，已切换到 {best}");
                        } else {
                            eprintln!("failover: 切换到 {best} 失败，保持 {active}");
                        }
                    }
                    Some(_) => {} // 最快的仍是当前节点（探测抖动），保持不动
                    None => eprintln!("failover: 当前节点 {active} 故障，全量测速无可用节点"),
                }
            }
        });
    }

    /// 订阅刷新成功后默认来一遍延迟测速：实测该订阅全部启用节点（不走缓存，
    /// 刷新的初衷就是拿到新节点清单，旧缓存可能已失真）。结果进延迟缓存供
    /// UI 与 failover 使用；超时节点按「超时自动停用」设置处理。
    /// 核心未运行时跳过；调用方负责放到后台，不阻塞 HTTP 响应/调度循环。
    pub(crate) async fn test_sub_nodes_after_refresh(&self, sub: &Subscription) {
        if !self.sup.running() {
            return;
        }
        let tags: Vec<String> = match self.store.nodes(&sub.id) {
            Ok(nodes) => nodes
                .iter()
                .filter(|n| n.enabled)
                .map(|n| n.tag.clone())
                .collect(),
            Err(_) => return,
        };
        if tags.is_empty() {
            return;
        }
        let measured = measure_delays(self, tags).await;
        let ok = measured.iter().filter(|(_, ms)| *ms >= 0).count();
        eprintln!(
            "订阅 {} 更新后测速完成：{}/{} 个节点可用",
            sub.name,
            ok,
            measured.len()
        );
        if self.store.settings().auto_disable_timeout {
            let all = self.store.all_nodes();
            for (tag, ms) in measured.iter() {
                if *ms >= 0 {
                    continue;
                }
                if let Some(n) = all.iter().find(|n| &n.tag == tag) {
                    if n.enabled && self.store.set_node_enabled(&n.id, false).is_ok() {
                        eprintln!("订阅 {} 更新后测速超时，已自动停用 {}", sub.name, tag);
                    }
                }
            }
        }
    }

    /// 后台准备 mihomo 的 geo 数据，不阻塞调用方。
    pub fn prewarm_geo(self: &Arc<Self>) {
        if self.bins.get(engine::MIHOMO).map(|s| s.is_empty()).unwrap_or(true) {
            return;
        }
        let app = self.clone();
        tokio::spawn(async move {
            let _tok = tokio::time::timeout(Duration::from_secs(180), app.geo.prewarm()).await;
        });
    }

    /// 核心确认已停止时撤掉透明代理规则，避免整网断网。
    pub(crate) async fn failsafe_network(&self) {
        if !tproxy::supported() || self.sup.running() {
            return;
        }
        if !self.store.settings().tproxy_enabled {
            return;
        }
        if let Err(e) = tproxy::disable(&self.tp).await {
            eprintln!("核心已停止，撤销透明代理规则失败: {e}");
            return;
        }
        eprintln!("核心已停止，已撤销透明代理规则（避免整网断网）");
    }

    /// 把最近一次校验结果落到 settings.json，重启后仍能显示。
    pub(crate) fn record_check(&self, ok: bool) {
        self.store
            .set_last_check(ok, &crate::util::now_rfc3339());
    }

    pub(crate) fn set_last_check_result(&self, res: CheckResult) {
        *self.last_check.write().unwrap() = res;
    }

    /* ------------------------------------------------------------ fallback */

    async fn h_fallback(
        State(_app): State<Arc<Server>>,
        req: axum::extract::Request,
    ) -> Response {
        let path = req.uri().path().to_string();
        // 未知 API 路径返回 JSON 404，而不是掉进静态文件处理器返回 index.html
        if path == "/api" || path.starts_with("/api/") {
            return err_json(
                StatusCode::NOT_FOUND,
                "not_found",
                &format!("接口不存在: {} {}", req.method(), path),
            );
        }
        crate::webfs::handler(&path, req.headers())
    }

    /* ------------------------------------------------------------ auth handlers */

    async fn h_auth_status(State(app): State<Arc<Server>>) -> Response {
        let set = app.store.settings();
        json_ok(json!({
            "required": app.login_required(),
            "needs_setup": app.needs_setup(),
            "username": set.username,
            "login_disabled": set.login_disabled,
            "setup_done": set.setup_done,
            "first_run": app.first_run(),
        }))
    }

    /// 全新安装还没有任何使用痕迹（无订阅、无已下发配置）时视为首次运行，
    /// 前端据此弹设置向导；老版本升级上来的实例不会被打扰。
    pub(crate) fn first_run(&self) -> bool {
        let set = self.store.settings();
        if set.setup_done {
            return false;
        }
        self.store.subscriptions().is_empty() && self.store.config().map(|c| c.is_empty()).unwrap_or(true)
    }

    /// 首次账户设置：只在「还没有任何凭据」时可用。设置完成后即成为常规登录态。
    /// body 里带 skip=true 表示「不设置密码」：登录保持关闭，面板免密访问。
    async fn h_auth_setup(State(app): State<Arc<Server>>, body: Bytes) -> Response {
        if !app.needs_setup() {
            return err_json(
                StatusCode::BAD_REQUEST,
                "already_setup",
                "面板已完成初始化，请直接登录或在设置里修改",
            );
        }
        let inb: Value = match parse_body(&body) {
            Ok(v) => v,
            Err(resp) => return resp,
        };
        if inb.get("skip").and_then(|v| v.as_bool()).unwrap_or(false) {
            let mut set = app.store.settings();
            set.login_disabled = true;
            if let Err(e) = app.store.save_settings(set) {
                return err_json(StatusCode::INTERNAL_SERVER_ERROR, "save_failed", &e.to_string());
            }
            eprintln!("首次设置选择了免密访问：面板登录保持关闭");
            return json_ok(json!({"ok": true, "skipped": true}));
        }
        let username = inb
            .get("username")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let password = inb.get("password").and_then(|v| v.as_str()).unwrap_or("");
        if username.is_empty() {
            return err_json(StatusCode::BAD_REQUEST, "bad_request", "用户名不能为空");
        }
        if password.len() < 8 {
            return err_json(StatusCode::BAD_REQUEST, "weak_password", "密码至少 8 位");
        }
        let mut set = app.store.settings();
        set.username = username;
        set.password_hash = Server::hash_password(&set.username, password);
        set.login_disabled = false;
        if let Err(e) = app.store.save_settings(set) {
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "save_failed", &e.to_string());
        }
        json_ok(json!({"ok": true, "required": true}))
    }

    async fn h_check(State(app): State<Arc<Server>>, headers: HeaderMap) -> Response {
        if let Some(resp) = app.authed(&headers) {
            return resp;
        }
        json_ok(json!({"ok": true}))
    }

    async fn h_login(
        State(app): State<Arc<Server>>,
        connect_info: axum::extract::ConnectInfo<std::net::SocketAddr>,
        body: Bytes,
    ) -> Response {
        let inb: Value = match parse_body(&body) {
            Ok(v) => v,
            Err(resp) => return resp,
        };
        if !app.login_required() {
            let (code, kind, msg) = if app.needs_setup() {
                (
                    StatusCode::BAD_REQUEST,
                    "setup_required",
                    "面板尚未初始化，请先设置登录账户密码",
                )
            } else {
                (StatusCode::BAD_REQUEST, "auth_disabled", "面板未启用登录")
            };
            return err_json(code, kind, msg);
        }
        let ip = Server::client_ip(Some(connect_info.0));
        {
            let g = app.throttle.lock().unwrap();
            if let Some(a) = g.get(&ip) {
                if a.until > std::time::Instant::now() {
                    let wait = (a.until - std::time::Instant::now()).as_secs() + 1;
                    return err_json(
                        StatusCode::TOO_MANY_REQUESTS,
                        "too_many_attempts",
                        &format!("登录失败次数过多，请 {wait} 秒后再试"),
                    );
                }
            }
        }
        let set = app.store.settings();
        let username = inb.get("username").and_then(|v| v.as_str()).unwrap_or("");
        let password = inb.get("password").and_then(|v| v.as_str()).unwrap_or("");
        if username != set.username
            || Server::hash_password(username, password) != set.password_hash
        {
            throttle_fail(&app.throttle, &ip);
            tokio::time::sleep(Duration::from_millis(300)).await;
            return err_json(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "用户名或密码错误",
            );
        }
        throttle_succeed(&app.throttle, &ip);
        let tok = crate::util::rand_hex(24);
        app.store_token(tok.clone());
        json_ok(json!({"token": tok, "user": set.username}))
    }

    async fn h_auth_access(
        State(app): State<Arc<Server>>,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        if let Some(resp) = app.authed(&headers) {
            return resp;
        }
        let inb: Value = match parse_body(&body) {
            Ok(v) => v,
            Err(resp) => return resp,
        };
        let disabled = inb.get("disabled").and_then(|v| v.as_bool()).unwrap_or(false);
        let mut set = app.store.settings();
        if disabled {
            set.login_disabled = true;
            if let Err(e) = app.store.save_settings(set) {
                return err_json(StatusCode::INTERNAL_SERVER_ERROR, "save_failed", &e.to_string());
            }
            return json_ok(json!({"ok": true, "required": false}));
        }
        let username = inb
            .get("username")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let password = inb.get("password").and_then(|v| v.as_str()).unwrap_or("");
        if username.is_empty() {
            return err_json(StatusCode::BAD_REQUEST, "bad_request", "用户名不能为空");
        }
        if password.len() < 8 {
            return err_json(StatusCode::BAD_REQUEST, "weak_password", "密码至少 8 位");
        }
        set.username = username;
        set.password_hash = Server::hash_password(&set.username, password);
        set.login_disabled = false;
        if let Err(e) = app.store.save_settings(set) {
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "save_failed", &e.to_string());
        }
        // 凭据已变更，旧会话一律作废
        app.tokens.lock().unwrap().clear();
        json_ok(json!({"ok": true, "required": true}))
    }

    /* ------------------------------------------------------------ status / logs / service */

    /// 出口 IP 缓存只在「什么都没变」的前提下才成立：切内核、切路由模式、
    /// 改入站/局域网代理、应用或回滚配置、启停核心之后必须立刻失效，
    /// 否则概览页会拿着旧的出口 IP 骗用户 60 秒。
    pub(crate) async fn clear_exit_ip(&self) {
        *self.exit_ip.lock().await = None;
    }

    /// 当前出口 IP：面板经内核的 mixed 入站访问 IP 回显服务实测。
    /// 核心未运行或全部回显源失败时 ip 为 null；结果缓存 60 秒。
    async fn h_exit_ip(State(app): State<Arc<Server>>, headers: HeaderMap) -> Response {
        if let Some(resp) = app.authed(&headers) {
            return resp;
        }
        const TTL: Duration = Duration::from_secs(60);
        const FAIL_TTL: Duration = Duration::from_secs(15);
        let mut g = app.exit_ip.lock().await;
        if let Some((at, res)) = g.as_ref() {
            let ttl = if res.is_some() { TTL } else { FAIL_TTL };
            if at.elapsed() < ttl {
                return json_ok(exit_ip_json(res.as_ref()));
            }
        }
        let res = app.measure_exit_ip().await;
        *g = Some((std::time::Instant::now(), res.clone()));
        json_ok(exit_ip_json(res.as_ref()))
    }

    async fn measure_exit_ip(&self) -> Option<(String, String)> {
        if !self.sup.running() {
            return None;
        }
        let set = self.store.settings();
        let mixed = if set.mixed_port > 0 {
            set.mixed_port
        } else {
            7890
        };
        // 优先经 mixed 入站：测得的就是当前路由策略下真正的出口。
        if let Some(v) = fetch_exit_ip_via(&format!("http://127.0.0.1:{mixed}")).await {
            return Some(v);
        }
        // mixed 不可用（模板删掉了入站等）时退化为直连测量：
        // rule 模式下通常测的是运营商出口；开了「接管本机流量」时其实仍经内核。
        fetch_exit_ip_via("").await
    }

    async fn h_status(
        State(app): State<Arc<Server>>,
        headers: HeaderMap,
    ) -> Response {
        if let Some(resp) = app.authed(&headers) {
            return resp;
        }
        let (running, pid, started, last_err) = app.sup.status();
        let nodes = app.store.all_nodes();
        let enabled = nodes.iter().filter(|n| n.enabled).count();        let cfg_ver = app.store.settings().config_version;
        let raw = app.store.config().unwrap_or_default();
        let eng = app.engine_name();
        let rules = crate::config::inspect::rule_count(&raw, &eng);

        let mut version = String::new();
        let mut active = String::new();
        if running {
            if let Ok(tokio_out) =
                tokio::time::timeout(Duration::from_secs(2), app.clash.version()).await
            {
                if let Ok(v) = tokio_out {
                    version = v;
                }
            }
            if let Ok(tokio_out) =
                tokio::time::timeout(Duration::from_secs(2), app.clash.resolve_active("proxy"))
                    .await
            {
                if let Some(now) = tokio_out.into() {
                    if !now.is_empty() {
                        active = now;
                    }
                }
            }
        }

        let (mut last_ok, mut last_output, mut _last_elapsed) = {
            let g = app.last_check.read().unwrap();
            (g.ok, g.output.clone(), g.elapsed_ms)
        };
        let set = app.store.settings();
        if !set.last_check_at.is_empty() {
            last_ok = set.last_check_ok;
            last_output = String::new(); // 详情只在当次校验时展示
        }

        let started_str = if started == std::time::SystemTime::UNIX_EPOCH {
            "0001-01-01T00:00:00Z".to_string()
        } else {
            match started.duration_since(std::time::UNIX_EPOCH) {
                Ok(d) => crate::util::format_rfc3339(
                    time::OffsetDateTime::from_unix_timestamp(d.as_secs() as i64)
                        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH),
                ),
                Err(_) => "0001-01-01T00:00:00Z".to_string(),
            }
        };
        let uptime = started
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|_| started.elapsed().unwrap_or_default().as_secs() as i64)
            .unwrap_or(0);

        json_ok(json!({
            "running": running,
            "pid": pid,
            "started_at": started_str,
            "uptime": uptime,
            "version": version,
            "active": active,
            "panel_version": VERSION,
            "arch": format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH),
            "mixed_port": crate::config::inspect::mixed_port(&raw, &eng),
            "mixed_listen": set.mixed_listen,
            "config_version": cfg_ver,
            "nodes": nodes.len(),
            "enabled": enabled,
            // 未过期的延迟缓存：后台（订阅更新测速/failover 探测）跑完后
            // 前端轮询这里就能立即看到，不必等用户手动测速
            "delays": app.clash.delays_json(),
            "rules": rules,
            "rss": crate::platform::proc_rss(pid),
            "panel_rss": crate::platform::self_rss(),
            "listen": app.store.settings().listen,
            "listen_pinned": app.listen_pinned.load(Ordering::Relaxed),
            "data_dir": app.store.dir().to_string_lossy(),
            "login_required": app.login_required(),
            "login_disabled": set.login_disabled,
            "username": set.username,
            "keep_snapshots": set.keep_snapshots,
            "delay_workers": set.delay_workers,
            "delay_ttl": set.delay_ttl,
            "tproxy": app.store.settings().tproxy_mode,
            "subs": app.store.subscriptions().len(),
            "last_check": last_output,
            "last_check_ok": last_ok,
            "last_check_at": set.last_check_at,
            "last_error": last_err,
            "panel_uptime": app.start.elapsed().as_secs() as i64,
            "platform": std::env::consts::OS,
            "engine": eng,
            "engine_label": engine::label(&eng),
            "engine_bin": app.bin(),
            "now": crate::util::now_rfc3339(),
        }))
    }

    async fn h_logs(State(app): State<Arc<Server>>, headers: HeaderMap) -> Response {
        if let Some(resp) = app.authed(&headers) {
            return resp;
        }
        let lines = app.sup.logs_snapshot();
        json_ok(json!({"lines": lines}))
    }

    /// 「启动核心」时当前还没有任何已应用配置：渲染+应用一体完成
    /// （校验/落盘/启动/探活），让添加过订阅的部署点一下就能跑。
    /// 返回 None 表示已有配置、不适用本引导，调用方走常规 start。
    async fn first_run_start(&self) -> Option<Result<(), String>> {
        if !self
            .store
            .config()
            .map(|c| c.is_empty())
            .unwrap_or(true)
        {
            return None;
        }
        if !self.store.all_nodes().iter().any(|n| n.enabled) {
            return Some(Err(
                "尚无已应用配置且没有启用节点：请先在「订阅」页添加订阅，节点启用后再启动"
                    .into(),
            ));
        }
        let set = self.store.settings();
        let raw = match self.render_config(&set).await {
            Ok(r) => r,
            Err(e) => return Some(Err(format!("自动渲染配置失败: {e}"))),
        };
        match self.applier.apply(&raw, "首次启动自动应用").await {
            Ok(()) => {
                eprintln!("首次启动: 配置已自动应用，核心已拉起");
                Some(Ok(()))
            }
            Err(e) => Some(Err(format!(
                "自动应用配置失败（可在配置页手工重试）: {e}"
            ))),
        }
    }

    async fn h_service(
        State(app): State<Arc<Server>>,
        headers: HeaderMap,
        Path(action): Path<String>,
    ) -> Response {
        if let Some(resp) = app.authed(&headers) {
            return resp;
        }
        let set = app.store.settings();
        let res: Result<(), String> = match action.as_str() {
            "start" => {
                app.sup.reset_crashes();
                // 53 端口预检：陈旧配置里 dns-in 仍绑 53 且被占时，
                // 启动前按策略释放端口或回落，避免核心启动即 FATAL
                crate::dns53::patch_stale_config(
                    &app.store.config_path(),
                    set.dns53_takeover,
                    &|s| {
                        app.sup.logs.push(s.to_string());
                        eprintln!("{s}");
                    },
                )
                .await;
                let r = match app.first_run_start().await {
                    Some(r) => r,
                    None => match app.sup.start() {
                        Err(e) if e == "already running" => Ok(()),
                        other => other,
                    },
                };
                if r.is_ok() && set.tproxy_enabled {
                    let mut cidrs = set.lan_cidrs.clone();
                    if cidrs.is_empty() {
                        cidrs = tproxy::detect_lan();
                    }
                    if let Err(e) =
                        tproxy::apply(&app.tp, &cidrs, set.tproxy_local, set.tproxy_dns).await
                    {
                        eprintln!("恢复透明代理规则失败: {e}");
                    }
                }
                r
            }
            "stop" => {
                let r = app.sup.stop().await;
                // 核心停了就撤掉规则，否则流量会被重定向到没人监听的端口
                if r.is_ok() && set.tproxy_enabled {
                    let _ = tproxy::disable(&app.tp).await;
                }
                r
            }
            "restart" => app.sup.restart().await,
            _ => {
                return err_json(StatusCode::BAD_REQUEST, "bad_action", "未知操作");
            }
        };
        if let Err(e) = res {
            app.failsafe_network().await;
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "service_failed", &e);
        }
        // 启停/重启核心都会换掉出口，缓存立即作废。
        app.clear_exit_ip().await;
        json_ok(json!({"ok": true}))
    }

    /* ------------------------------------------------------------ listen */

    async fn h_listen_set(
        State(app): State<Arc<Server>>,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        if let Some(resp) = app.authed(&headers) {
            return resp;
        }
        let inb: Value = match parse_body(&body) {
            Ok(v) => v,
            Err(resp) => return resp,
        };
        let addr = inb
            .get("listen")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if let Err(e) = validate_listen_addr(&addr) {
            return err_json(StatusCode::BAD_REQUEST, "bad_listen", &e);
        }

        let mut warning = String::new();
        if app.listen_pinned.load(Ordering::Relaxed) {
            warning = "启动参数 -listen 已指定监听地址，重启后以该参数为准".to_string();
        }
        if addr == app.store.settings().listen {
            return json_ok(json!({"ok": true, "listen": addr, "warning": warning}));
        }
        let router = {
            let g = app.router.read().unwrap();
            match g.clone() {
                Some(r) => r,
                None => {
                    return err_json(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "unsupported",
                        "当前进程不支持热切换监听地址",
                    )
                }
            }
        };
        let prev = app.store.settings().listen;
        {
            let mut guard = app.listen.lock().await;
            match guard.as_mut() {
                Some(lm) => {
                    if let Err(e) = lm.change(&addr, router).await {
                        return err_json(
                            StatusCode::BAD_REQUEST,
                            "listen_failed",
                            &format!("切换监听地址失败: {e}"),
                        );
                    }
                }
                None => {
                    return err_json(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "unsupported",
                        "当前进程不支持热切换监听地址",
                    )
                }
            }
        }
        let mut set = app.store.settings();
        set.listen = addr.clone();
        if let Err(e) = app.store.save_settings(set) {
            // 切换失败回滚监听
            let r = app.router.read().unwrap().clone();
            if let Some(lm) = app.listen.lock().await.as_mut() {
                if let Some(r) = r {
                    let _ = lm.change(&prev, r).await;
                }
            }
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "save_failed", &e.to_string());
        }
        // 回写 panel.conf，保证重启后仍生效
        if let Err(e) = crate::panelconf::write_panel_conf_listen(&addr) {
            if !warning.is_empty() {
                warning += "；";
            }
            warning += &format!("panel.conf 写入失败：{e}，重启后监听地址可能回退");
        }
        json_ok(json!({"ok": true, "listen": addr, "warning": warning}))
    }

    /* ------------------------------------------------------------ subscriptions */

    async fn h_list_subs(State(app): State<Arc<Server>>, headers: HeaderMap) -> Response {
        if let Some(resp) = app.authed(&headers) {
            return resp;
        }
        json_ok(json!({"subscriptions": app.store.subscriptions()}))
    }

    async fn h_create_sub(
        State(app): State<Arc<Server>>,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        if let Some(resp) = app.authed(&headers) {
            return resp;
        }
        let inb: Value = match parse_body(&body) {
            Ok(v) => v,
            Err(resp) => return resp,
        };
        let url = inb.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if url.is_empty() {
            return err_json(StatusCode::BAD_REQUEST, "bad_request", "订阅链接不能为空");
        }
        let mut name = inb
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if name.is_empty() {
            let now = time::OffsetDateTime::now_utc();
            let fmt = time::format_description::parse_borrowed::<2>("[month]-[day] [hour]:[minute]").unwrap();
            name = format!("订阅 {}", now.format(&fmt).unwrap_or_default());
        }
        let mut interval = inb
            .get("interval_hours")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        if interval == 0 {
            interval = DEFAULT_SUB_INTERVAL_HOURS;
        }
        if !valid_sub_interval(interval) {
            return err_json(StatusCode::BAD_REQUEST, "bad_interval", "更新间隔需在 1-720 小时之间");
        }
        let auto = inb.get("auto").and_then(|v| v.as_bool()).unwrap_or(true);
        let sub = Subscription {
            // 毫秒时间戳会在同毫秒创建时撞 ID（互相覆盖订阅与节点文件），用随机 id
            id: format!("s{}", crate::util::rand_hex(6)),
            name,
            url,
            auto,
            interval_hours: interval,
            node_count: 0,
            updated_at: String::new(),
            usage_used: 0,
            usage_total: 0,
            expire: 0,
            last_error: String::new(),
        };
        let mut reserved = app.reserved_tags("");
        let (nodes, info, not_modified) = match app.subs.refresh(&sub, &[], &mut reserved).await {
            Ok(x) => x,
            Err(e) => return err_json(StatusCode::BAD_GATEWAY, "fetch_failed", &e),
        };
        if not_modified && nodes.is_empty() {
            // 首次添加就撞上 204：机场限流/无内容，本地没有可保留的节点。
            return err_json(
                StatusCode::BAD_GATEWAY,
                "fetch_failed",
                "订阅端返回 204（内容未变化或请求过于频繁），未获取到节点，请稍后重试",
            );
        }
        if let Err(e) = app.store.save_nodes(&sub.id, &nodes) {
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "save_failed", &e.to_string());
        }
        let mut sub = sub;
        sub.node_count = nodes.len();
        sub.updated_at = crate::util::now_rfc3339();
        sub.usage_used = info.upload + info.download;
        sub.usage_total = info.total;
        sub.expire = info.expire;
        if let Err(e) = app.store.upsert_subscription(sub.clone()) {
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "save_failed", &e.to_string());
        }
        // 导入成功即自动「渲染 → 应用 → 拉起核心」：开箱即跑，无需再点「启动核心」。
        // 核心已在运行则不打扰（避免周期刷新反复重启核心），新节点在下次应用时生效。
        let mut core_autostart = "skipped";
        if !app.sup.running() && nodes.iter().any(|n| n.enabled) {
            let set = app.store.settings();
            match app.render_config(&set).await {
                Ok(raw) => match app.applier.apply(&raw, "导入订阅自动启动").await {
                    Ok(()) => {
                        core_autostart = "started";
                        eprintln!("导入订阅后已自动应用配置并拉起核心");
                    }
                    Err(e) => {
                        core_autostart = "failed";
                        eprintln!("导入订阅后自动启动核心失败: {e}");
                    }
                },
                Err(e) => {
                    core_autostart = "failed";
                    eprintln!("导入订阅后自动渲染配置失败: {e}");
                }
            }
        }
        // 后台补一轮「重新下发配置（若变化）→ 测速」：核心在跑时导入的新节点
        // 立即生效并完成验证，而不是等下次手动生成配置
        app.spawn_reapply_and_test(&sub);
        json_ok(json!({"subscription": sub, "nodes": nodes.len(), "core_autostart": core_autostart}))
    }

    async fn h_update_sub(
        State(app): State<Arc<Server>>,
        headers: HeaderMap,
        Path(id): Path<String>,
        body: Bytes,
    ) -> Response {
        if let Some(resp) = app.authed(&headers) {
            return resp;
        }
        let Some(mut sub) = app.store.subscription(&id) else {
            return err_json(StatusCode::NOT_FOUND, "not_found", "订阅不存在");
        };
        let inb: Value = match parse_body(&body) {
            Ok(v) => v,
            Err(resp) => return resp,
        };
        if let Some(name) = inb.get("name").and_then(|v| v.as_str()) {
            if !name.is_empty() {
                sub.name = name.to_string();
            }
        }
        if let Some(url) = inb.get("url").and_then(|v| v.as_str()) {
            if !url.is_empty() {
                sub.url = url.to_string();
            }
        }
        if let Some(auto) = inb.get("auto").and_then(|v| v.as_bool()) {
            sub.auto = auto;
        }
        if let Some(hv) = inb.get("interval_hours") {
            if let Some(h) = hv
                .as_i64()
                .or_else(|| hv.as_str().and_then(|s| s.trim().parse().ok()))
            {
                if !valid_sub_interval(h) {
                    return err_json(StatusCode::BAD_REQUEST, "bad_interval", "更新间隔需在 1-720 小时之间");
                }
                sub.interval_hours = h;
            }
        }
        if let Err(e) = app.store.upsert_subscription(sub.clone()) {
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "save_failed", &e.to_string());
        }
        json_ok(serde_json::to_value(&sub).unwrap_or(Value::Null))
    }

    async fn h_delete_sub(
        State(app): State<Arc<Server>>,
        headers: HeaderMap,
        Path(id): Path<String>,
    ) -> Response {
        if let Some(resp) = app.authed(&headers) {
            return resp;
        }
        if let Err(e) = app.store.delete_subscription(&id) {
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "delete_failed", &e.to_string());
        }
        json_ok(json!({"ok": true}))
    }

    async fn h_refresh_sub(
        State(app): State<Arc<Server>>,
        headers: HeaderMap,
        Path(id): Path<String>,
    ) -> Response {
        if let Some(resp) = app.authed(&headers) {
            return resp;
        }
        let Some(sub) = app.store.subscription(&id) else {
            return err_json(StatusCode::NOT_FOUND, "not_found", "订阅不存在");
        };
        let n = match app.refresh_one(&sub).await {
            Ok(n) => n,
            Err(e) => return err_json(StatusCode::BAD_GATEWAY, "fetch_failed", &e),
        };
        let sub = app.store.subscription(&id).unwrap_or(sub);
        // 重新下发配置 + 测速已由 refresh_one 内部丢到后台，不拖住 HTTP 响应
        json_ok(json!({"subscription": sub, "nodes": n}))
    }

    /* ------------------------------------------------------------ nodes */

    async fn h_list_nodes(State(app): State<Arc<Server>>, headers: HeaderMap) -> Response {
        if let Some(resp) = app.authed(&headers) {
            return resp;
        }
        let nodes: Vec<Node> = app.store.all_nodes();
        json_ok(json!({
            "nodes": nodes,
            "delays": app.clash.delays_json(),
            "speeds": store_speeds_json(&app),
            "running": app.sup.running(),
        }))
    }

    async fn h_delay_nodes(
        State(app): State<Arc<Server>>,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        if let Some(resp) = app.authed(&headers) {
            return resp;
        }
        let inb: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let tags_in: Vec<String> = inb
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        if !app.sup.running() {
            return err_json(
                StatusCode::CONFLICT,
                "core_stopped",
                &format!("{} 未运行，无法测速", engine::label(&app.engine_name())),
            );
        }
        let Ok(tags) = normalize_delay_tags(tags_in, &app.store.all_nodes()) else {
            return err_json(StatusCode::BAD_REQUEST, "no_tags", "没有可测速的节点");
        };
        // 默认吃 5 分钟缓存：页面刷新、切页回来、脚本轮询都不该重新打一遍节点。
        // 显式传 refresh=true（前端「强制重测」勾选框）才全部重测；
        // 单节点「点一下测速」也是显式动作，前端固定传 refresh=true。
        let refresh = inb.get("refresh").and_then(|v| v.as_bool()).unwrap_or(false);
        let (cached, todo): (Vec<(String, i64)>, Vec<String>) = if refresh {
            (Vec::new(), tags.clone())
        } else {
            let mut cached = Vec::new();
            let mut todo = Vec::new();
            for tag in tags.iter() {
                match app.clash.cached_delay(tag) {
                    Some(ms) => cached.push((tag.clone(), ms)),
                    None => todo.push(tag.clone()),
                }
            }
            (cached, todo)
        };
        let measured = measure_delays(&app, todo).await;
        // 结果按请求里的节点顺序拼回去：缓存命中的 + 本次实测的。
        let measured_count = measured.len();
        let cached_count = cached.len();
        let mut merged: std::collections::HashMap<String, i64> =
            measured.iter().cloned().collect();
        for (tag, ms) in cached {
            merged.entry(tag).or_insert(ms);
        }
        let results: Vec<(String, i64)> = tags
            .iter()
            .map(|t| (t.clone(), *merged.get(t).unwrap_or(&-1)))
            .collect();
        let arr: Vec<Value> = results
            .into_iter()
            .map(|(tag, ms)| json!({"tag": tag, "ms": ms}))
            .collect();

        // 超时自动停用：只对本次实测超时（非缓存）的节点生效，
        // 避免缓存里的历史失败反复触发。停用只改节点库，下次生成配置时生效。
        let mut disabled: Vec<String> = Vec::new();
        if app.store.settings().auto_disable_timeout {
            let all = app.store.all_nodes();
            for (tag, ms) in measured.iter() {
                if *ms >= 0 {
                    continue;
                }
                if let Some(n) = all.iter().find(|n| &n.tag == tag) {
                    if n.enabled && app.store.set_node_enabled(&n.id, false).is_ok() {
                        disabled.push(tag.clone());
                    }
                }
            }
            if !disabled.is_empty() {
                eprintln!("测速超时自动停用 {} 个节点: {}", disabled.len(), disabled.join(", "));
            }
        }

        json_ok(json!({
            "results": arr,
            "delays": app.clash.delays_json(),
            "measured": measured_count,
            "cached": cached_count,
            "disabled": disabled,
        }))
    }

    /* ------------------------------------------------------------ 带宽测速 */

    /// 逐节点测带宽：临时把 proxy 组切到目标节点，经本机 mixed 入站下载测速文件，
    /// 测完恢复原选择。必须串行——selector 是全局的，并发切换会互相干扰，
    /// 测出来的速率也不可信。结果持久化到 speeds.json，作为「上次测速速度」展示。
    async fn h_speed_nodes(
        State(app): State<Arc<Server>>,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        if let Some(resp) = app.authed(&headers) {
            return resp;
        }
        static SPEED_BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        use std::sync::atomic::Ordering;
        if SPEED_BUSY
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return err_json(StatusCode::CONFLICT, "speed_busy", "已有带宽测速正在进行，请稍候");
        }
        let inb: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let tags_in: Vec<String> = inb
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let out = Self::speed_test_impl(&app, tags_in).await;
        SPEED_BUSY.store(false, Ordering::SeqCst);
        match out {
            Ok(v) => json_ok(v),
            Err((code, kind, msg)) => err_json(code, kind, &msg),
        }
    }

    async fn speed_test_impl(
        app: &Arc<Server>,
        tags_in: Vec<String>,
    ) -> Result<Value, (StatusCode, &'static str, String)> {
        if !app.sup.running() {
            return Err((
                StatusCode::CONFLICT,
                "core_stopped",
                format!("{} 未运行，无法测速", engine::label(&app.engine_name())),
            ));
        }
        let set = app.store.settings();
        let mixed_port = if set.mixed_port > 0 { set.mixed_port } else { 7890 };
        let url = if set.speed_test_url.trim().is_empty() {
            crate::model::DEFAULT_SPEED_TEST_URL.to_string()
        } else {
            set.speed_test_url.trim().to_string()
        };
        let timeout_sec = set.speed_timeout_sec.clamp(3, 60).max(1);
        let Ok(tags) = normalize_delay_tags(tags_in, &app.store.all_nodes()) else {
            return Err((StatusCode::BAD_REQUEST, "no_tags", "没有可测速的节点".into()));
        };
        let group = "proxy";
        let original = app
            .clash
            .proxy_now(group)
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, "clash_error", format!("读取当前出口失败：{e}")))?;

        let mut results: Vec<Value> = Vec::with_capacity(tags.len());
        for tag in tags.iter() {
            let switched = app.clash.select_proxy(group, tag).await.is_ok();
            let bps = if switched {
                let proxy = format!("http://127.0.0.1:{mixed_port}");
                match app
                    .clash
                    .speed_probe(&proxy, &url, Duration::from_secs(timeout_sec as u64))
                    .await
                {
                    Ok(b) => {
                        app.store.set_speed(tag, b);
                        b
                    }
                    Err(_) => {
                        app.store.set_speed(tag, -1);
                        -1
                    }
                }
            } else {
                // 节点不在策略组里（未启用）或内核拒绝切换
                app.store.set_speed(tag, -1);
                -1
            };
            results.push(json!({"tag": tag, "bps": bps}));
        }
        // 恢复原选择；原成员已失效（如刚被自动停用）时兜底切回 auto
        if app.clash.select_proxy(group, &original).await.is_err() && original != "auto" {
            let _ = app.clash.select_proxy(group, "auto").await;
        }
        Ok(json!({
            "results": results,
            "speeds": store_speeds_json(app),
        }))
    }

    /* ------------------------------------------------------------ 测速设置 */

    async fn h_speedtest_get(State(app): State<Arc<Server>>, headers: HeaderMap) -> Response {
        if let Some(resp) = app.authed(&headers) {
            return resp;
        }
        let set = app.store.settings();
        json_ok(json!({
            "auto_select_mode": if set.auto_select_mode == "failover" { "failover" } else { "interval" },
            "urltest_interval_min": if set.urltest_interval_min > 0 { set.urltest_interval_min } else { 10 },
            "urltest_tolerance": if set.urltest_tolerance > 0 { set.urltest_tolerance } else { 50 },
            "delay_test_url": if set.delay_test_url.trim().is_empty() { crate::model::DEFAULT_DELAY_TEST_URL } else { set.delay_test_url.trim() },
            "speed_test_url": if set.speed_test_url.trim().is_empty() { crate::model::DEFAULT_SPEED_TEST_URL } else { set.speed_test_url.trim() },
            "speed_timeout_sec": if set.speed_timeout_sec > 0 { set.speed_timeout_sec } else { 10 },
            "auto_disable_timeout": set.auto_disable_timeout,
            "delay_workers": set.delay_workers,
        }))
    }

    /// 保存测速设置。涉及内核策略组的字段（间隔/容差/延迟测速 URL/选优方式）
    /// 变更时自动重新渲染并下发配置；apply 失败回滚设置，渲染失败（典型：
    /// 没有启用的节点）只保存设置并返回 applied=false + 原因。
    async fn h_speedtest_put(
        State(app): State<Arc<Server>>,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        if let Some(resp) = app.authed(&headers) {
            return resp;
        }
        let inb: Value = match parse_body(&body) {
            Ok(v) => v,
            Err(resp) => return resp,
        };
        let get_i = |k: &str| -> i64 {
            inb.get(k)
                .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())))
                .unwrap_or(0)
        };
        let get_url = |k: &str| -> String {
            inb.get(k)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string()
        };
        for k in ["delay_test_url", "speed_test_url"] {
            let u = get_url(k);
            if !u.is_empty() && !u.starts_with("http://") && !u.starts_with("https://") {
                return err_json(
                    StatusCode::BAD_REQUEST,
                    "bad_url",
                    &format!("{k} 必须是 http:// 或 https:// 开头的地址"),
                );
            }
        }

        let set = app.store.settings();
        let prev = set.clone();
        let mut s2 = set;
        // 数值字段只在请求里真的带了（>0）才覆盖：部分更新不会把缺省字段
        // 意外压成下限值
        let itv = get_i("urltest_interval_min");
        if itv > 0 {
            s2.urltest_interval_min = itv.clamp(1, 1440);
        }
        let tol = get_i("urltest_tolerance");
        if tol > 0 {
            s2.urltest_tolerance = tol.clamp(1, 1000);
        }
        let sto = get_i("speed_timeout_sec");
        if sto > 0 {
            s2.speed_timeout_sec = sto.clamp(3, 60);
        }
        if let Some(b) = inb.get("auto_disable_timeout").and_then(|v| v.as_bool()) {
            s2.auto_disable_timeout = b;
        }
        if inb.get("delay_test_url").is_some() {
            s2.delay_test_url = get_url("delay_test_url");
        }
        if inb.get("speed_test_url").is_some() {
            s2.speed_test_url = get_url("speed_test_url");
        }
        let w = get_i("delay_workers");
        if w > 0 {
            s2.delay_workers = w.clamp(1, 16);
        }
        if let Some(m) = inb.get("auto_select_mode").and_then(|v| v.as_str()) {
            if m != "interval" && m != "failover" {
                return err_json(
                    StatusCode::BAD_REQUEST,
                    "bad_mode",
                    "自动选优方式只能是 interval / failover",
                );
            }
            s2.auto_select_mode = m.to_string();
        }

        let kernel_changed = s2.urltest_interval_min != prev.urltest_interval_min
            || s2.urltest_tolerance != prev.urltest_tolerance
            || s2.delay_test_url != prev.delay_test_url
            || s2.auto_select_mode != prev.auto_select_mode;

        let mut applied = false;
        let mut reason = String::new();
        if kernel_changed {
            // 与引擎切换互斥，避免渲染到一半内核被换掉
            let _engine_g = app.engine_mu.lock().await;
            match app.render_config(&s2).await {
                Err(e) => {
                    // 渲染失败（典型：还没有启用的节点）不是设置本身的错：
                    // 设置照常落盘，等启用节点后点「生成配置」时生效
                    if let Err(se) = app.store.save_settings(s2.clone()) {
                        return err_json(StatusCode::INTERNAL_SERVER_ERROR, "save_failed", &se.to_string());
                    }
                    reason = e;
                }
                Ok(raw) => {
                    if let Err(e) = app.store.save_settings(s2.clone()) {
                        return err_json(StatusCode::INTERNAL_SERVER_ERROR, "save_failed", &e.to_string());
                    }
                    let apply_res =
                        tokio::time::timeout(APPLY_TIMEOUT, app.applier.apply(&raw, "测速设置变更"))
                            .await;
                    match apply_res {
                        Err(_) | Ok(Err(_)) => {
                            let _ = app.store.save_settings(prev);
                            app.failsafe_network().await;
                            let msg = match apply_res {
                                Ok(Err(e)) => e.to_string(),
                                _ => "apply timeout".to_string(),
                            };
                            return err_json(StatusCode::BAD_REQUEST, "apply_failed", &msg);
                        }
                        Ok(Ok(())) => {}
                    }
                    app.clear_exit_ip().await;
                    applied = true;
                }
            }
        } else if let Err(e) = app.store.save_settings(s2.clone()) {
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "save_failed", &e.to_string());
        }
        app.clash.set_delay_url(&s2.delay_test_url);
        json_ok(json!({
            "ok": true,
            "applied": applied,
            "reason": reason,
            "settings": {
                "urltest_interval_min": s2.urltest_interval_min,
                "urltest_tolerance": s2.urltest_tolerance,
                "delay_test_url": if s2.delay_test_url.is_empty() { crate::model::DEFAULT_DELAY_TEST_URL } else { s2.delay_test_url.as_str() },
                "speed_test_url": if s2.speed_test_url.is_empty() { crate::model::DEFAULT_SPEED_TEST_URL } else { s2.speed_test_url.as_str() },
                "speed_timeout_sec": s2.speed_timeout_sec,
                "auto_disable_timeout": s2.auto_disable_timeout,
                "delay_workers": s2.delay_workers,
            }
        }))
    }

    /* ------------------------------------------------------------ 首次运行向导 */

    /// 向导一键完成：本机端口/监听 + 路由模式 + 局域网代理 + 系统代理，
    /// 一次渲染一次 apply；还没有可启用节点时只落盘设置并标记完成。
    async fn h_setup_finish(
        State(app): State<Arc<Server>>,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        if let Some(resp) = app.authed(&headers) {
            return resp;
        }
        let inb: Value = match parse_body(&body) {
            Ok(v) => v,
            Err(resp) => return resp,
        };
        let get_str = |k: &str| inb.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let get_i = |k: &str| -> i64 {
            inb.get(k)
                .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
                .unwrap_or(0)
        };

        let reserved: HashMap<i64, &str> = HashMap::from([
            (9090, "面板"),
            (9091, "Clash API"),
            (12345, "透明代理"),
            (5354, "局域网 DNS"),
            (53, "DNS"),
        ]);
        let check_port = |p: i64, what: &str| -> Result<(), Response> {
            if !(1024..=65535).contains(&p) {
                return Err(err_json(
                    StatusCode::BAD_REQUEST,
                    "bad_port",
                    &format!("{what}端口需在 1024-65535 之间"),
                ));
            }
            if let Some(name) = reserved.get(&p) {
                return Err(err_json(
                    StatusCode::BAD_REQUEST,
                    "port_conflict",
                    &format!("端口 {p} 已被{name}占用"),
                ));
            }
            Ok(())
        };

        let set = app.store.settings();
        let prev = set.clone();
        let mut s2 = set;

        let mixed_port = get_i("mixed_port");
        if mixed_port > 0 {
            if let Err(resp) = check_port(mixed_port, "本机代理") {
                return resp;
            }
            s2.mixed_port = mixed_port;
        }
        let mixed_listen = get_str("mixed_listen");
        if !mixed_listen.is_empty() {
            if !matches!(mixed_listen.as_str(), "127.0.0.1" | "::1" | "0.0.0.0" | "::") {
                return err_json(
                    StatusCode::BAD_REQUEST,
                    "bad_listen",
                    "监听地址只能是 127.0.0.1、::1、0.0.0.0 或 ::",
                );
            }
            s2.mixed_listen = mixed_listen;
        }
        let routing = get_str("routing_mode");
        if matches!(routing.as_str(), "rule" | "global" | "direct") {
            s2.routing_mode = routing;
        }
        let lan_proxy = inb.get("lan_proxy").and_then(|v| v.as_bool()).unwrap_or(false);
        let socks_port = get_i("lan_socks_port");
        let http_port = get_i("lan_http_port");
        if socks_port > 0 {
            if let Err(resp) = check_port(socks_port, "SOCKS5") {
                return resp;
            }
            s2.lan_socks_port = socks_port;
        }
        if http_port > 0 {
            if let Err(resp) = check_port(http_port, "HTTP") {
                return resp;
            }
            s2.lan_http_port = http_port;
        }
        if lan_proxy {
            if s2.lan_socks_port == s2.lan_http_port {
                return err_json(
                    StatusCode::BAD_REQUEST,
                    "port_conflict",
                    "SOCKS5 与 HTTP 端口不能相同",
                );
            }
            if s2.lan_socks_port == s2.mixed_port || s2.lan_http_port == s2.mixed_port {
                return err_json(
                    StatusCode::BAD_REQUEST,
                    "port_conflict",
                    "端口已被 mixed 入站占用",
                );
            }
        }
        s2.lan_proxy = lan_proxy;
        s2.lan_user = get_str("lan_user");
        s2.lan_pass = get_str("lan_pass");
        if lan_proxy {
            // 与局域网代理页同一条规则：都留空 = 开放代理；要鉴权就两项都填
            if s2.lan_user.is_empty() != s2.lan_pass.is_empty() {
                return err_json(
                    StatusCode::BAD_REQUEST,
                    "bad_creds",
                    "用户名和密码必须同时填写，或同时留空（留空=不鉴权）",
                );
            }
            if !s2.lan_user.is_empty() && s2.lan_pass.len() < 8 {
                return err_json(StatusCode::BAD_REQUEST, "weak_password", "密码至少 8 位");
            }
        }
        s2.setup_done = true;

        // 没有任何启用节点时渲染会失败（"没有启用的节点"）：设置照常落盘，
        // 标记向导完成，等用户在节点页启用后点「生成配置」。
        let raw = match app.render_config(&s2).await {
            Ok(r) => r,
            Err(e) => {
                if let Err(se) = app.store.save_settings(s2) {
                    return err_json(StatusCode::INTERNAL_SERVER_ERROR, "save_failed", &se.to_string());
                }
                return json_ok(json!({"ok": true, "applied": false, "reason": e}));
            }
        };
        if let Err(e) = app.store.save_settings(s2.clone()) {
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "save_failed", &e.to_string());
        }
        let apply_res = tokio::time::timeout(
            APPLY_TIMEOUT,
            app.applier.apply(&raw, "首次运行向导"),
        )
        .await;
        match apply_res {
            Err(_) | Ok(Err(_)) => {
                let _ = app.store.save_settings(prev);
                app.failsafe_network().await;
                let msg = match apply_res {
                    Ok(Err(e)) => e.to_string(),
                    _ => "apply timeout".to_string(),
                };
                return err_json(StatusCode::BAD_REQUEST, "apply_failed", &msg);
            }
            Ok(Ok(())) => {}
        }
        app.clear_exit_ip().await;
        if inb.get("sysproxy").and_then(|v| v.as_bool()).unwrap_or(false) && cfg!(windows) {
            let port = if s2.mixed_port > 0 { s2.mixed_port as u16 } else { 7890 };
            if let Err(e) = crate::sysproxy::set_enabled(true, port, app.store.dir()) {
                eprintln!("设置系统代理失败: {e}");
            }
        }
        json_ok(json!({"ok": true, "applied": true}))
    }

    /// 只标记向导完成（用户点了「跳过向导」）。
    async fn h_setup_complete(State(app): State<Arc<Server>>, headers: HeaderMap) -> Response {
        if let Some(resp) = app.authed(&headers) {
            return resp;
        }
        let mut set = app.store.settings();
        set.setup_done = true;
        if let Err(e) = app.store.save_settings(set) {
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "save_failed", &e.to_string());
        }
        json_ok(json!({"ok": true}))
    }

    async fn h_toggle_node(
        State(app): State<Arc<Server>>,
        headers: HeaderMap,
        Path(id): Path<String>,
        body: Bytes,
    ) -> Response {
        if let Some(resp) = app.authed(&headers) {
            return resp;
        }
        let inb: Value = match parse_body(&body) {
            Ok(v) => v,
            Err(resp) => return resp,
        };
        let enabled = inb.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
        if app.store.set_node_enabled(&id, enabled).is_err() {
            return err_json(StatusCode::NOT_FOUND, "not_found", "节点不存在");
        }
        json_ok(json!({"ok": true}))
    }

    /* ------------------------------------------------------------ ws */

    async fn h_ws_connections(
        State(app): State<Arc<Server>>,
        ws: axum::extract::ws::WebSocketUpgrade,
        headers: HeaderMap,
        RawQuery(q): RawQuery,
    ) -> Response {
        if let Some(resp) = app.authed_ws(&headers, q.as_deref().unwrap_or("")) {
            return resp;
        }
        let clash = app.clash.clone();
        ws.on_upgrade(|socket| async move {
            clash.proxy_connections(socket).await;
        })
    }

    async fn h_ws_traffic(
        State(app): State<Arc<Server>>,
        ws: axum::extract::ws::WebSocketUpgrade,
        headers: HeaderMap,
        RawQuery(q): RawQuery,
    ) -> Response {
        if let Some(resp) = app.authed_ws(&headers, q.as_deref().unwrap_or("")) {
            return resp;
        }
        let clash = app.clash.clone();
        ws.on_upgrade(move |socket| async move {
            let _ = clash.proxy_traffic(socket).await;
        })
    }

    /* ------------------------------------------------------------ clash rest proxy */

    async fn h_clash_proxy(
        State(app): State<Arc<Server>>,
        Path(path): Path<String>,
        headers: HeaderMap,
        req: axum::extract::Request,
    ) -> Response {
        if let Some(resp) = app.authed(&headers) {
            return resp;
        }
        let full = format!("/{path}");
        let allowed = ["/proxies", "/connections", "/traffic", "/version", "/configs", "/rules"];
        if !allowed
            .iter()
            .any(|p| full == *p || full.starts_with(&format!("{p}/")))
        {
            return err_json(StatusCode::FORBIDDEN, "forbidden", "该路径不在白名单内");
        }
        let mut target = format!("http://{}{}", app.clash.addr(), full);
        if let Some(q) = req.uri().query() {
            target.push('?');
            target.push_str(q);
        }
        let method = reqwest::Method::from_bytes(req.method().as_str().as_bytes())
            .unwrap_or(reqwest::Method::GET);
        let body = axum::body::to_bytes(req.into_body(), 8 << 20)
            .await
            .unwrap_or_default();
        let mut r = app_clash_client().request(method, &target);
        for (k, v) in headers.iter() {
            if k == axum::http::header::AUTHORIZATION
                || k == axum::http::header::HOST
                || k == axum::http::header::CONTENT_LENGTH
                || k == axum::http::header::ACCEPT_ENCODING
            {
                continue;
            }
            if let Ok(vs) = v.to_str() {
                r = r.header(k.as_str(), vs);
            }
        }
        if !body.is_empty() {
            r = r.body(body);
        }
        let resp = match tokio::time::timeout(Duration::from_secs(8), r.send()).await {
            Ok(Ok(resp)) => resp,
            Ok(Err(e)) => {
                return err_json(StatusCode::BAD_GATEWAY, "clash_unreachable", &e.to_string())
            }
            Err(_) => {
                return err_json(
                    StatusCode::BAD_GATEWAY,
                    "clash_unreachable",
                    "upstream timeout",
                )
            }
        };
        let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let mut builder = Response::builder().status(status);
        if let Some(ct) = resp.headers().get(reqwest::header::CONTENT_TYPE) {
            builder = builder.header(axum::http::header::CONTENT_TYPE, ct);
        }
        // 4MB 上限与 Go 一致
        let bytes = resp.bytes().await.unwrap_or_default();
        let bytes = if bytes.len() > 4 << 20 {
            bytes.slice(..4 << 20)
        } else {
            bytes
        };
        builder
            .body(axum::body::Body::from(bytes))
            .unwrap_or_else(|_| err_json(StatusCode::INTERNAL_SERVER_ERROR, "proxy_failed", "bad response"))
    }
}

/* ------------------------------------------------------------ helpers */

use axum::body::Bytes;

fn mark_req_mw(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>> {
    Box::pin(async move {
        // 由 State 拿不到 Server（middleware 先于 with_state），
        // 用全局表记一下活跃时间足够：main 里把 Arc<Server> 存进全局。
        if let Some(app) = GLOBAL_SERVER.get() {
            app.last_req.store(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as i64)
                    .unwrap_or(0),
                Ordering::Relaxed,
            );
        }
        next.run(req).await
    })
}

static GLOBAL_SERVER: once_cell_like::OnceCell<Server> = once_cell_like::OnceCell::new();

mod once_cell_like {
    use std::sync::atomic::{AtomicPtr, Ordering};
    use std::sync::Arc;

    pub struct OnceCell<T> {
        ptr: AtomicPtr<()>,
        _marker: std::marker::PhantomData<Arc<T>>,
    }

    impl<T> OnceCell<T> {
        pub const fn new() -> Self {
            OnceCell {
                ptr: AtomicPtr::new(std::ptr::null_mut()),
                _marker: std::marker::PhantomData,
            }
        }

        pub fn set(&self, v: Arc<T>) {
            let boxed = Arc::into_raw(v) as *mut ();
            self.ptr.store(boxed as *mut (), Ordering::SeqCst);
        }

        pub fn get(&self) -> Option<Arc<T>> {
            let p = self.ptr.load(Ordering::SeqCst);
            if p.is_null() {
                return None;
            }
            let arc = unsafe { Arc::from_raw(p as *const T) };
            let out = arc.clone();
            std::mem::forget(arc);
            Some(out)
        }
    }

    unsafe impl<T> Send for OnceCell<T> {}
    unsafe impl<T> Sync for OnceCell<T> {}
}

/// main 调用：让中间件能拿到 Server 记录请求活跃时间。
pub fn register_global(app: Arc<Server>) {
    GLOBAL_SERVER.set(app);
}

fn app_clash_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .expect("clash proxy client")
}

pub(crate) fn parse_body(body: &[u8]) -> Result<Value, Response> {
    if body.iter().all(|b| b.is_ascii_whitespace()) {
        return Err(err_json(StatusCode::BAD_REQUEST, "bad_request", "无法解析请求体"));
    }
    match serde_json::from_slice(body) {
        Ok(v) => Ok(v),
        Err(_) => Err(err_json(StatusCode::BAD_REQUEST, "bad_request", "无法解析请求体")),
    }
}

pub(crate) fn json_ok(v: Value) -> Response {
    (
        StatusCode::OK,
        [("content-type", "application/json; charset=utf-8")],
        Json(v),
    )
        .into_response()
}

pub(crate) fn err_json(status: StatusCode, code: &str, msg: &str) -> Response {
    (
        status,
        [("content-type", "application/json; charset=utf-8")],
        Json(json!({"error": code, "message": msg})),
    )
        .into_response()
}

fn throttle_fail(map: &std::sync::Mutex<HashMap<String, Attempt>>, ip: &str) {
    let mut g = map.lock().unwrap();
    if g.len() > 1024 {
        let now = std::time::Instant::now();
        g.retain(|_, v| now.duration_since(v.last) < Duration::from_secs(3600));
    }
    let a = g.entry(ip.to_string()).or_insert(Attempt {
        fails: 0,
        until: std::time::Instant::now(),
        last: std::time::Instant::now(),
    });
    a.fails += 1;
    a.last = std::time::Instant::now();
    if a.fails >= LOGIN_MAX_FAILS {
        let mut lock = LOGIN_BASE_LOCK;
        for _ in LOGIN_MAX_FAILS..a.fails {
            lock *= 2;
            if lock >= LOGIN_MAX_LOCK {
                lock = LOGIN_MAX_LOCK;
                break;
            }
        }
        a.until = std::time::Instant::now() + lock;
    }
}

fn throttle_succeed(map: &std::sync::Mutex<HashMap<String, Attempt>>, ip: &str) {
    map.lock().unwrap().remove(ip);
}

pub(crate) fn validate_listen_addr(addr: &str) -> Result<(), String> {
    if addr.is_empty() {
        return Err("监听地址不能为空".into());
    }
    if addr.len() > 255 {
        return Err("监听地址过长".into());
    }
    let Some((host, port)) = addr.rsplit_once(':') else {
        return Err("格式应为 host:port，例如 0.0.0.0:9090".into());
    };
    if host.contains([' ', '\t', '\r', '\n']) {
        return Err("监听地址不能包含空白字符".into());
    }
    match port.parse::<u16>() {
        Ok(p) if p >= 1 => Ok(()),
        _ => Err("端口必须在 1-65535 之间".into()),
    }
}

/// store 里的带宽测速结果转成 API JSON：{tag: {bps, at}}。
fn store_speeds_json(app: &Server) -> Value {
    let mut out = serde_json::Map::new();
    for (tag, r) in app.store.speeds() {
        out.insert(tag, json!({"bps": r.bps, "at": r.at}));
    }
    Value::Object(out)
}

/// 全量实测一批节点的延迟（不走缓存），并发数取设置里的延迟测速并发。
/// 手动测速接口、failover 监控与订阅更新后的自动测速共用：
/// 结果写进 clashclient 的内存延迟缓存。
async fn measure_delays(app: &Server, tags: Vec<String>) -> Vec<(String, i64)> {
    let mut workers = app.store.settings().delay_workers;
    if workers <= 0 {
        workers = 3;
    }
    if workers > 16 {
        workers = 16;
    }
    if workers as usize > tags.len() {
        workers = tags.len() as i64;
    }
    // 固定 worker 并发（buffered 保持输入顺序），避免大订阅瞬间起上千协程
    use futures_util::StreamExt;
    let clash = app.clash.clone();
    let n = workers as usize;
    futures_util::stream::iter(tags.into_iter())
        .map(|tag| {
            let clash = clash.clone();
            async move {
                let ms = tokio::time::timeout(Duration::from_secs(6), clash.delay(&tag, 5000))
                    .await
                    .ok()
                    .and_then(|r| r.ok())
                    .unwrap_or(-1);
                (tag, ms)
            }
        })
        .buffered(n.max(1))
        .collect()
        .await
}

/// 去空白、去重并按上限截断；空列表时取全部启用节点。
fn normalize_delay_tags(tags: Vec<String>, all: &[Node]) -> Result<Vec<String>, ()> {
    let mut tags = tags;
    if tags.is_empty() {
        for n in all {
            if n.enabled {
                tags.push(n.tag.clone());
            }
        }
    }
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(tags.len());
    for tag in tags {
        let tag = tag.trim().to_string();
        if tag.is_empty() || seen.contains(&tag) {
            continue;
        }
        seen.insert(tag.clone());
        out.push(tag);
        if out.len() >= MAX_DELAY_TAGS {
            break;
        }
    }
    if out.is_empty() {
        return Err(());
    }
    Ok(out)
}

/// updated_at 距现在的秒数；解析失败返回 None（视为永不到期由调用方决定）。
fn seconds_since_rfc3339(s: &str) -> Option<i64> {
    if s.is_empty() {
        return None;
    }
    let t = time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()?;
    let now = time::OffsetDateTime::now_utc();
    Some((now - t).whole_seconds() as i64)
}

/* ------------------------------------------------------------ exit ip */

fn exit_ip_json(res: Option<&(String, String)>) -> Value {
    match res {
        Some((ip, via)) => json!({"ip": ip, "via": via}),
        None => json!({"ip": null}),
    }
}

/// 经指定 HTTP 代理（空串=直连）依次询问 IP 回显服务，返回 (出口 IP, "proxy"|"direct")。
/// 单源 5 秒超时，整体最多试 3 个源，避免单个服务挂掉时拖死面板。
async fn fetch_exit_ip_via(proxy: &str) -> Option<(String, String)> {
    use std::time::Duration;
    let via = if proxy.is_empty() { "direct" } else { "proxy" };
    // trace 返回 key=value 文本，其余返回纯 IP；放进同一循环统一解析。
    let urls = [
        ("https://api.ip.sb/ip", false),
        ("https://api.ipify.org", false),
        ("https://www.cloudflare.com/cdn-cgi/trace", true),
    ];
    for (url, is_trace) in urls {
        let mut b = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .no_proxy();
        if !proxy.is_empty() {
            match reqwest::Proxy::all(proxy) {
                Ok(p) => b = b.proxy(p),
                Err(_) => return None,
            }
        }
        let Ok(client) = b.build() else { continue };
        let fut = async {
            let resp = client.get(url).send().await.ok()?;
            if resp.status() != reqwest::StatusCode::OK {
                return None;
            }
            let text = resp.text().await.ok()?;
            let ip = if is_trace {
                text.lines()
                    .find_map(|l| l.strip_prefix("ip="))
                    .unwrap_or("")
                    .trim()
                    .to_string()
            } else {
                text.trim().to_string()
            };
            if ip.parse::<std::net::IpAddr>().is_ok() {
                Some(ip)
            } else {
                None
            }
        };
        if let Ok(Some(ip)) = tokio::time::timeout(Duration::from_secs(6), fut).await {
            return Some((ip, via.to_string()));
        }
    }
    None
}
