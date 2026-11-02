// 订阅拉取：伪装成 Clash 客户端（多数机场只对 Clash UA 返回 YAML），
// 原始响应只在内存里存活，解析完即释放，不落盘。

pub mod parse;

use std::time::Duration;

use crate::model::{Node, Subscription};

const USER_AGENT: &str = "clash-verge/v1.7.7";
const MAX_BODY: usize = 8 << 20; // 8MB 硬上限，防止恶意订阅打爆内存

#[derive(Default, Clone, Copy, Debug)]
pub struct UserInfo {
    pub upload: i64,
    pub download: i64,
    pub total: i64,
    pub expire: i64,
}

pub struct Service {
    http: reqwest::Client,
}

impl Default for Service {
    fn default() -> Self {
        Self::new()
    }
}

impl Service {
    pub fn new() -> Service {
        let mut b = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(20));
        #[cfg(windows)]
        {
            b = b.use_native_tls();
        }
        #[cfg(not(windows))]
        {
            b = b.use_rustls_tls();
        }
        let http = b.build().expect("http client");
        Service { http }
    }

    /// 拉取订阅，返回响应体与 subscription-userinfo。
    /// 机场后端对拉取过于频繁/已失效的 token 会返回 204（No Content），
    /// 这是「内容未变化」的信号，不算错误，由调用方保留既有节点。
    pub async fn fetch(&self, url: &str) -> Result<(Vec<u8>, UserInfo, bool), String> {
        let resp = self
            .http
            .get(url)
            .header("Accept", "*/*")
            .send()
            .await
            .map_err(|e| format!("fetch: {e}"))?;
        let status = resp.status();
        let info = parse_user_info(resp.headers().get("subscription-userinfo"));
        if status == reqwest::StatusCode::NO_CONTENT {
            return Ok((Vec::new(), info, true));
        }
        if status != reqwest::StatusCode::OK {
            return Err(format!("upstream returned {}", status.as_u16()));
        }
        use futures_util::StreamExt;
        let mut body: Vec<u8> = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| e.to_string())?;
            if body.len() + chunk.len() > MAX_BODY {
                return Err("subscription body too large".into());
            }
            body.extend_from_slice(&chunk);
        }
        Ok((body, info, false))
    }

    /// 拉取 + 解析，返回 (节点, 用量信息, 是否未变化)。
    /// 未变化（204）时原样返回既有节点，调用方无需重新入库。
    pub async fn refresh(
        &self,
        sub: &Subscription,
        prev: &[Node],
        reserved: &mut std::collections::HashSet<String>,
    ) -> Result<(Vec<Node>, UserInfo, bool), String> {
        let (body, info, not_modified) = self.fetch(&sub.url).await?;
        if not_modified {
            return Ok((prev.to_vec(), info, true));
        }
        let nodes = parse::parse(&body, &sub.id, prev, reserved)?;
        Ok((nodes, info, false))
    }
}

/// subscription-userinfo: upload=1; download=2; total=3; expire=1700000000
fn parse_user_info(h: Option<&reqwest::header::HeaderValue>) -> UserInfo {
    let mut info = UserInfo::default();
    let Some(h) = h else { return info };
    let Ok(h) = h.to_str() else { return info };
    for part in h.split(';') {
        let Some((k, v)) = part.trim().split_once('=') else { continue };
        let v = v.trim().parse::<i64>().unwrap_or(0);
        match k.trim() {
            "upload" => info.upload = v,
            "download" => info.download = v,
            "total" => info.total = v,
            "expire" => info.expire = v,
            _ => {}
        }
    }
    info
}
