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
pub const VERSION: &str = "1.0.0";

/// 覆盖「校验 + 重启 + 探活」全过程；mihomo 首次校验会下载 geo 数据。
pub const APPLY_TIMEOUT: Duration = Duration::from_secs(120);
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
            .route("/api/v1/nodes/{id}", put(Self::h_toggle_node))
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
    pub(crate) fn authed(&self, headers: &HeaderMap) -> Option<Response> {
        if self.login_required() && !self.valid_token(headers) {
            return Some(err_json(StatusCode::UNAUTHORIZED, "unauthorized", "未登录或登录已过期"));
        }
        None
    }

    /// WebSocket 的认证：支持 Authorization 头或 ?token= 查询参数。
    pub(crate) fn authed_ws(&self, headers: &HeaderMap, query: &str) -> Option<Response> {
        if !self.login_required() {
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
            let _ = self.store.upsert_subscription(sub);
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
        let _ = self.store.upsert_subscription(sub);
        Ok(nodes.len())
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
                        }
                    }
                }
            }
        });
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
            "username": set.username,
            "login_disabled": set.login_disabled,
        }))
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
            return err_json(StatusCode::BAD_REQUEST, "auth_disabled", "面板未启用登录");
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
                let r = match app.sup.start() {
                    Err(e) if e == "already running" => Ok(()),
                    other => other,
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
            id: format!("s{}", crate::util::unix_millis_now()),
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
        json_ok(json!({"subscription": sub, "nodes": nodes.len()}))
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
        let results: Vec<(String, i64)> = futures_util::stream::iter(tags.into_iter())
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
            .await;
        let arr: Vec<Value> = results
            .into_iter()
            .map(|(tag, ms)| json!({"tag": tag, "ms": ms}))
            .collect();
        json_ok(json!({"results": arr, "delays": app.clash.delays_json()}))
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
