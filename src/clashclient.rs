// Clash API 客户端：面板读状态、测速、拉连接/流量都走这里。
// 延迟结果只存内存，按 TTL 过期；进程重启即清零。

use std::collections::HashMap;
use portable_atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};

pub struct Client {
    addr: String,
    http: reqwest::Client,
    delays: Mutex<HashMap<String, (i64, i64)>>, // tag -> (ms, at_unix)
    ttl: AtomicU64,                             // seconds
    /// 连通性测速 URL（面板设置里可改），启动与设置变更时更新。
    delay_url: Mutex<String>,
}

impl Client {
    pub fn new(controller: &str) -> Client {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(8))
            .pool_max_idle_per_host(8)
            .build()
            .expect("clash http client");
        Client {
            addr: controller.to_string(),
            http,
            delays: Mutex::new(HashMap::new()),
            ttl: AtomicU64::new(300),
            delay_url: Mutex::new(crate::model::DEFAULT_DELAY_TEST_URL.to_string()),
        }
    }

    /// Clash API 的 host:port，供反向代理拼接目标地址。
    pub fn addr(&self) -> &str {
        &self.addr
    }

    pub fn set_ttl(&self, secs: i64) {
        self.ttl.store(secs.max(1) as u64, Ordering::Relaxed);
    }

    /// 更新连通性测速 URL（面板设置变更时调用）。
    pub fn set_delay_url(&self, url: &str) {
        let url = url.trim();
        if url.is_empty() {
            return;
        }
        *self.delay_url.lock().unwrap() = url.to_string();
    }

    pub async fn version(&self) -> Result<String, String> {
        let v: Value = self.get_json("/version").await?;
        Ok(v.get("version")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string())
    }

    async fn get_json(&self, path: &str) -> Result<Value, String> {
        let url = format!("http://{}{}", self.addr, path);
        let resp = self.http.get(&url).send().await.map_err(|e| e.to_string())?;
        let status = resp.status();
        let body = resp.bytes().await.map_err(|e| e.to_string())?;
        if status != reqwest::StatusCode::OK {
            let head = String::from_utf8_lossy(&body);
            let head: String = head.chars().take(512).collect();
            return Err(format!("{}: {}", status.as_u16(), head));
        }
        serde_json::from_slice(&body).map_err(|e| e.to_string())
    }

    async fn get_raw(&self, path: &str) -> Result<Vec<u8>, String> {
        let url = format!("http://{}{}", self.addr, path);
        let resp = self.http.get(&url).send().await.map_err(|e| e.to_string())?;
        let status = resp.status();
        if status != reqwest::StatusCode::OK {
            return Err(format!("clash {path}: {}", status.as_u16()));
        }
        // 上限 8MB，与 Go 一致
        use futures_util::StreamExt;
        let mut out = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| e.to_string())?;
            if out.len() + chunk.len() > 8 << 20 {
                break;
            }
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    /// 通过 Clash API 测一个节点的延迟。结果写内存缓存并带过期时间。
    pub async fn delay(&self, tag: &str, timeout_ms: i64) -> Result<i64, String> {
        let url = self.delay_url.lock().unwrap().clone();
        let path = format!(
            "/proxies/{}/delay?timeout={}&url={}",
            percent_encode_path(tag),
            timeout_ms,
            percent_encode_query(&url)
        );
        let url = format!("http://{}{}", self.addr, path);
        let resp = match self.http.get(&url).send().await {
            Ok(r) => r,
            Err(e) => return Err(e.to_string()),
        };
        let status = resp.status();
        if status != reqwest::StatusCode::OK {
            self.store_delay(tag, -1);
            return Err(format!("delay {tag}: {}", status.as_u16()));
        }
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        let ms = body.get("delay").and_then(|d| d.as_i64()).unwrap_or(-1);
        self.store_delay(tag, ms);
        Ok(ms)
    }

    fn store_delay(&self, tag: &str, ms: i64) {
        self.delays
            .lock()
            .unwrap()
            .insert(tag.to_string(), (ms, crate::util::unix_now()));
    }

    /// 清空延迟缓存。切换内核后必须调用：两个内核的测量口径不同。
    pub fn reset_delays(&self) {
        self.delays.lock().unwrap().clear();
    }

    /// 读未过期的延迟缓存。None = 没测过或已过期，调用方需要实测。
    pub fn cached_delay(&self, tag: &str) -> Option<i64> {
        let ttl = self.ttl.load(Ordering::Relaxed) as i64;
        let now = crate::util::unix_now();
        let g = self.delays.lock().unwrap();
        g.get(tag)
            .filter(|(_, at)| now - at < ttl)
            .map(|(ms, _)| *ms)
    }

    /// 未过期的延迟缓存：{tag: {ms, at}}。
    pub fn delays_json(&self) -> Value {
        let now = crate::util::unix_now();
        let ttl = self.ttl.load(Ordering::Relaxed) as i64;
        let g = self.delays.lock().unwrap();
        let mut out = serde_json::Map::new();
        for (k, (ms, at)) in g.iter() {
            if now - at < ttl {
                out.insert(
                    k.clone(),
                    json!({"ms": ms, "at": at}),
                );
            }
        }
        Value::Object(out)
    }

    /// 顺着 selector → urltest → 节点 一路解析，
    /// 返回真正承载流量的出站名（而不是中间的策略组名）。
    pub async fn resolve_active(&self, group: &str) -> String {
        let mut name = group.to_string();
        for _ in 0..5 {
            let path = format!("/proxies/{}", percent_encode_path(&name));
            let Ok(p) = self.get_json(&path).await else { return String::new() };
            let now = p.get("now").and_then(|v| v.as_str()).unwrap_or("");
            if now.is_empty() {
                return name;
            }
            name = now.to_string();
        }
        name
    }

    /// 读策略组当前选中的成员名。
    pub async fn proxy_now(&self, group: &str) -> Result<String, String> {
        let path = format!("/proxies/{}", percent_encode_path(group));
        let p = self.get_json(&path).await?;
        Ok(p.get("now")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string())
    }

    /// 把 selector 策略组切到指定成员（带宽测速临时切换出口用）。
    pub async fn select_proxy(&self, group: &str, name: &str) -> Result<(), String> {
        let path = format!("/proxies/{}", percent_encode_path(group));
        let url = format!("http://{}{}", self.addr, path);
        let resp = self
            .http
            .put(&url)
            .json(&json!({"name": name}))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = resp.status();
        if status != reqwest::StatusCode::OK && status != reqwest::StatusCode::NO_CONTENT {
            let body = resp.bytes().await.unwrap_or_default();
            let head = String::from_utf8_lossy(&body);
            return Err(format!("select {group}→{name}: {}", head.chars().take(200).collect::<String>()));
        }
        Ok(())
    }

    /// 经指定 HTTP 代理下载测速文件，返回比特率（bps）。
    /// 整体超时截断：截断前收到的字节数已足够算速率；一个字节都没收到才算失败。
    pub async fn speed_probe(
        &self,
        proxy_addr: &str,
        url: &str,
        timeout: Duration,
    ) -> Result<i64, String> {
        // 代理挂在 ClientBuilder 上（reqwest 不支持按请求切代理），
        // 每次测速各建一个短生命周期 client，互不串味。
        let mut builder = reqwest::Client::builder().connect_timeout(Duration::from_secs(6));
        if !proxy_addr.is_empty() {
            builder = builder.proxy(reqwest::Proxy::http(proxy_addr).map_err(|e| e.to_string())?);
        }
        let http = builder.build().map_err(|e| e.to_string())?;
        let resp = http
            .get(url)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("speed {url}: {}", status.as_u16()));
        }
        use futures_util::StreamExt;
        let start = std::time::Instant::now();
        let mut total: u64 = 0;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(b) => total += b.len() as u64,
                // 总超时触发时连接被掐断，属于预期截断，不算失败
                Err(_) => break,
            }
        }
        let elapsed = start.elapsed().as_secs_f64().max(0.05);
        if total == 0 {
            return Err("没有收到任何数据".into());
        }
        Ok((total as f64 * 8.0 / elapsed) as i64)
    }

    /// 转发 /connections：每 2 秒从 Clash REST 拉一次连接列表，裁剪后推给浏览器。
    /// 不用上游 WebSocket：sing-box 的 /connections WS 在长连接下会静默停止推送。
    pub async fn proxy_connections<S>(&self, mut down: S)
    where
        S: futures_util::Sink<axum::extract::ws::Message> + Unpin,
        S::Error: std::fmt::Debug,
    {
        use futures_util::SinkExt;

        async fn push<S>(down: &mut S, frame: String) -> bool
        where
            S: futures_util::Sink<axum::extract::ws::Message> + Unpin,
            S::Error: std::fmt::Debug,
        {
            down.send(axum::extract::ws::Message::Text(frame.into()))
                .await
                .is_ok()
        }

        loop {
            let frame = match self.get_raw("/connections").await {
                Ok(raw) => trim_connections(&raw, 200),
                Err(_) => r#"{"connections":[]}"#.to_string(), // 核心可能正在重启，下一轮再试
            };
            if !push(&mut down, frame).await {
                return;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    /// 转发 /traffic（上游 WS 每秒一帧），在服务端节流到 2 秒一帧并平均。
    pub async fn proxy_traffic<S>(&self, mut down: S) -> Result<(), String>
    where
        S: futures_util::Sink<axum::extract::ws::Message> + Unpin,
        S::Error: std::fmt::Debug,
    {
        use futures_util::{SinkExt, StreamExt};

        let url = format!("ws://{}/traffic", self.addr);
        let (ws, _) = tokio_tungstenite::connect_async(&url)
            .await
            .map_err(|e| e.to_string())?;
        let (mut up_tx, mut up_rx) = ws.split();

        let sums = std::sync::Arc::new((
            portable_atomic::AtomicI64::new(0),
            portable_atomic::AtomicI64::new(0),
        ));
        {
            let sums = sums.clone();
            tokio::spawn(async move {
                while let Some(msg) = up_rx.next().await {
                    let Ok(msg) = msg else { break };
                    if let tokio_tungstenite::tungstenite::Message::Text(txt) = msg {
                        if let Ok(t) = serde_json::from_str::<Value>(&txt) {
                            sums.0.fetch_add(
                                t.get("up").and_then(|v| v.as_i64()).unwrap_or(0),
                                Ordering::Relaxed,
                            );
                            sums.1.fetch_add(
                                t.get("down").and_then(|v| v.as_i64()).unwrap_or(0),
                                Ordering::Relaxed,
                            );
                        }
                    }
                }
            });
        }

        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let u = sums.0.swap(0, Ordering::Relaxed) / 2;
            let d = sums.1.swap(0, Ordering::Relaxed) / 2;
            let frame = format!(
                r#"{{"up":{},"down":{},"ts":{}}}"#,
                u,
                d,
                crate::util::unix_millis_now()
            );
            if down
                .send(axum::extract::ws::Message::Text(frame.into()))
                .await
                .is_err()
            {
                return Ok(());
            }
            let _ = &mut up_tx; // 保持 sender 存活
        }
    }
}

fn percent_encode_path(s: &str) -> String {
    const SET: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~');
    percent_encoding::utf8_percent_encode(s, SET).to_string()
}

fn percent_encode_query(s: &str) -> String {
    const SET: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~');
    percent_encoding::utf8_percent_encode(s, SET).to_string()
}

/// 裁剪连接列表：只保留前端展示需要的字段，最多 limit 条。
fn trim_connections(raw: &[u8], limit: usize) -> String {
    let Ok(v) = serde_json::from_slice::<Value>(raw) else {
        return r#"{"connections":[]}"#.to_string();
    };
    let mut conns: Vec<Value> = v
        .get("connections")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    if conns.len() > limit {
        conns.truncate(limit);
    }
    serde_json::json!({"connections": conns}).to_string()
}
