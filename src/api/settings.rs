// API 第二部分：配置渲染/校验/应用/回滚、透明代理、局域网代理、TUN、
// 路由模式、开机自启、内核切换、geo 数据。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use serde_json::{json, Value};

use super::{err_json, json_ok, parse_body, Server, APPLY_TIMEOUT};
use crate::config::{self, ApplyError};
use crate::engine;
use crate::tproxy;

/* ------------------------------------------------------------ config */

impl Server {
    pub(crate) async fn h_get_config(
        State(app): State<Arc<Server>>,
        headers: HeaderMap,
    ) -> Response {
        if let Some(resp) = app.authed(&headers) {
            return resp;
        }
        let raw = String::from_utf8_lossy(&app.store.config().unwrap_or_default()).into_owned();
        json_ok(json!({"raw": raw, "snapshots": app.store.snapshots()}))
    }

    pub(crate) async fn h_render_config(
        State(app): State<Arc<Server>>,
        headers: HeaderMap,
    ) -> Response {
        if let Some(resp) = app.authed(&headers) {
            return resp;
        }
        let set = app.store.settings();
        match app.render_config(&set).await {
            Ok(raw) => json_ok(json!({
                "raw": String::from_utf8_lossy(&raw),
                "engine": app.engine_name()
            })),
            Err(e) => err_json(StatusCode::BAD_REQUEST, "render_failed", &e),
        }
    }

    pub(crate) async fn h_validate_config(
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
        let raw = inb.get("raw").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if raw.is_empty() {
            return err_json(StatusCode::BAD_REQUEST, "bad_request", "缺少配置内容");
        }
        let res = config::check(
            &app.bin(),
            &app.engine_name(),
            app.store.dir(),
            raw.as_bytes(),
        )
        .await;
        app.set_last_check_result(res.clone());
        app.record_check(res.ok);
        json_ok(json!({"ok": res.ok, "output": res.output, "elapsed_ms": res.elapsed_ms}))
    }

    pub(crate) async fn h_apply_config(
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
        let raw = inb.get("raw").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if raw.is_empty() {
            return err_json(StatusCode::BAD_REQUEST, "bad_request", "缺少配置内容");
        }
        let mut note = inb
            .get("note")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if note.is_empty() {
            note = "手动保存".into();
        }
        match tokio::time::timeout(APPLY_TIMEOUT, app.applier.apply(raw.as_bytes(), &note)).await {
            Err(_) => {
                app.failsafe_network().await;
                err_json(StatusCode::BAD_REQUEST, "apply_failed", "apply timeout")
            }
            Ok(Err(e)) => {
                app.failsafe_network().await;
                let (status, code) = match &e {
                    ApplyError::Check { .. } => (StatusCode::UNPROCESSABLE_ENTITY, "apply_failed"),
                    _ => (StatusCode::BAD_REQUEST, "apply_failed"),
                };
                if let ApplyError::Check { detail, .. } = &e {
                    app.set_last_check_result(config::CheckResult {
                        ok: false,
                        output: detail.clone(),
                        elapsed_ms: 0,
                    });
                }
                err_json(status, code, &e.to_string())
            }
            Ok(Ok(())) => {
                app.record_check(true);
                app.clear_exit_ip().await;
                json_ok(json!({"ok": true, "snapshots": app.store.snapshots()}))
            }
        }
    }

    pub(crate) async fn h_snapshots(
        State(app): State<Arc<Server>>,
        headers: HeaderMap,
    ) -> Response {
        if let Some(resp) = app.authed(&headers) {
            return resp;
        }
        json_ok(json!({"snapshots": app.store.snapshots()}))
    }

    pub(crate) async fn h_rollback(
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
        let version = inb
            .get("version")
            .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
            .unwrap_or(0);
        if version == 0 {
            return err_json(StatusCode::BAD_REQUEST, "bad_request", "缺少版本号");
        }
        match tokio::time::timeout(APPLY_TIMEOUT, app.applier.rollback_to(version)).await {
            Err(_) => {
                app.failsafe_network().await;
                err_json(StatusCode::BAD_REQUEST, "rollback_failed", "rollback timeout")
            }
            Ok(Err(e)) => {
                app.failsafe_network().await;
                err_json(StatusCode::BAD_REQUEST, "rollback_failed", &e.to_string())
            }
            Ok(Ok(())) => {
                app.clear_exit_ip().await;
                json_ok(json!({"ok": true}))
            }
        }
    }

    /* ------------------------------------------------------------ tproxy */

    pub(crate) async fn h_tproxy_status(
        State(app): State<Arc<Server>>,
        headers: HeaderMap,
    ) -> Response {
        if let Some(resp) = app.authed(&headers) {
            return resp;
        }
        let set = app.store.settings();
        let mut cidrs = set.lan_cidrs.clone();
        if cidrs.is_empty() {
            cidrs = tproxy::detect_lan();
        }
        let st = tproxy::status(&app.tp, cidrs.clone()).await;
        json_ok(json!({
            "enabled": st.enabled,
            "supported": tproxy::supported(),
            "local": st.local,
            "dns": st.dns,
            "local_supported": tproxy::local_supported(&app.tp),
            "mode": set.tproxy_mode,
            "port": st.port,
            "rules": st.rules,
            "cidrs": st.cidrs,
            "detected": tproxy::detect_lan(),
            "note": "redirect 仅接管 TCP；目标为内网的流量直接放行",
        }))
    }

    /// 打开/关闭透明代理。打开时下发 nftables，关闭时删表。
    pub(crate) async fn h_tproxy_set(
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
        let enabled = inb.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);

        let set = app.store.settings();
        let prev = set.clone();
        let mut cidrs: Vec<String> = inb
            .get("cidrs")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        if cidrs.is_empty() {
            cidrs = set.lan_cidrs.clone();
        }
        if cidrs.is_empty() {
            cidrs = tproxy::detect_lan();
        }
        let local = match inb.get("local") {
            Some(v) => v.as_bool().unwrap_or(set.tproxy_local),
            None => set.tproxy_local,
        };
        let hijack_dns = match inb.get("dns") {
            Some(v) => v.as_bool().unwrap_or(set.tproxy_dns),
            None => set.tproxy_dns,
        };

        if enabled {
            if !tproxy::supported() {
                return err_json(
                    StatusCode::BAD_REQUEST,
                    "unsupported",
                    "透明代理依赖 nftables，仅支持 Linux",
                );
            }
            if !app.sup.running() {
                return err_json(
                    StatusCode::CONFLICT,
                    "core_stopped",
                    "请先启动 sing-box 核心，再开启透明代理",
                );
            }
            if let Err(e) = tproxy::preflight(&app.tp, set.tproxy_port).await {
                return err_json(StatusCode::BAD_REQUEST, "preflight_failed", &e);
            }
            if let Err(e) = app.verify_tproxy_inbounds(set.tproxy_port, hijack_dns).await {
                return err_json(StatusCode::BAD_REQUEST, "inbound_missing", &e);
            }
            if let Err(e) = tproxy::apply(&app.tp, &cidrs, local, hijack_dns).await {
                return err_json(StatusCode::INTERNAL_SERVER_ERROR, "nft_failed", &e);
            }
        } else if tproxy::supported() {
            // 不支持的平台（Windows）本来就没有 nftables 规则，直接当作「已关闭」
            if let Err(e) = tproxy::disable(&app.tp).await {
                return err_json(StatusCode::INTERNAL_SERVER_ERROR, "nft_failed", &e);
            }
        }

        let mut set2 = set.clone();
        set2.tproxy_enabled = enabled;
        set2.tproxy_local = local;
        set2.tproxy_dns = hijack_dns;
        set2.lan_cidrs = cidrs.clone();
        if let Err(e) = app.store.save_settings(set2.clone()) {
            // 规则已变更但设置没落盘时，把 nftables 恢复到改动前的状态
            if prev.tproxy_enabled {
                let mut prev_cidrs = prev.lan_cidrs.clone();
                if prev_cidrs.is_empty() {
                    prev_cidrs = cidrs;
                }
                let _ = tproxy::apply(&app.tp, &prev_cidrs, prev.tproxy_local, prev.tproxy_dns).await;
            } else {
                let _ = tproxy::disable(&app.tp).await;
            }
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "save_failed", &e.to_string());
        }
        let st = tproxy::status(&app.tp, cidrs.clone()).await;
        // 规则下发成功不代表 LAN 真的能用：入站只绑回环时客户端会被拒绝连接。
        let mut warning = String::new();
        if st.enabled && !cidrs.is_empty() && !app.redir_reachable(set.tproxy_port).await {
            warning = "透明代理入站只监听了回环地址，局域网设备会被拒绝连接；请把入站监听到 0.0.0.0 后重新应用配置".to_string();
            eprintln!("透明代理告警：{warning}");
        }
        json_ok(json!({
            "ok": true, "enabled": st.enabled, "local": st.local, "dns": st.dns,
            "rules": st.rules, "cidrs": cidrs, "warning": warning,
        }))
    }

    async fn verify_tproxy_inbounds(&self, port: i64, hijack_dns: bool) -> Result<(), String> {
        let raw = self
            .store
            .config()
            .map_err(|e| format!("当前没有已下发的配置: {e}"))?;
        if raw.is_empty() {
            return Err("当前没有已下发的配置".into());
        }
        config::inspect::verify_tproxy(&raw, &self.engine_name(), port, hijack_dns)
    }

    async fn redir_reachable(&self, port: i64) -> bool {
        let Ok(raw) = self.store.config() else { return true };
        if raw.is_empty() {
            return true;
        }
        config::inspect::redir_reachable_from_lan(&raw, &self.engine_name(), port)
    }

    /* ------------------------------------------------------------ lan proxy */

    pub(crate) async fn h_lan_proxy_get(
        State(app): State<Arc<Server>>,
        headers: HeaderMap,
        
    ) -> Response {
        if let Some(resp) = app.authed(&headers) {
            return resp;
        }
        let set = app.store.settings();
        let host = headers.get(axum::http::header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("").split(':').next().unwrap_or("").to_string();
        let host = if host.is_empty() { "127.0.0.1".to_string() } else { host };
        let (socks_url, http_url) = lan_proxy_urls(
            &host,
            set.lan_socks_port,
            set.lan_http_port,
            &set.lan_user,
            &set.lan_pass,
        );
        json_ok(json!({
            "enabled": set.lan_proxy,
            "socks_port": set.lan_socks_port,
            "http_port": set.lan_http_port,
            "username": set.lan_user,
            "password": set.lan_pass,
            "has_password": !set.lan_pass.is_empty(),
            "open": set.lan_proxy && set.lan_user.is_empty() && set.lan_pass.is_empty(),
            "socks_url": socks_url,
            "http_url": http_url,
            "mixed_port": set.mixed_port,
            "mixed_listen": set.mixed_listen,
            "mixed_url": format!("{}:{}", set.mixed_listen, set.mixed_port),
            "mixed_open": set.mixed_listen != "127.0.0.1" && set.mixed_listen != "::1",
        }))
    }

    /// 保存并下发局域网代理配置。改端口/账号需要重启核心。
    pub(crate) async fn h_lan_proxy_set(
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
        let enabled = inb.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
        let mut socks_port = get_i("socks_port");
        let mut http_port = get_i("http_port");
        let mut mixed_port = get_i("mixed_port");
        let mut mixed_listen = get_str("mixed_listen");
        let username = get_str("username");
        let password = get_str("password");

        let set = app.store.settings();
        let prev = set.clone();
        if socks_port == 0 {
            socks_port = set.lan_socks_port;
        }
        if http_port == 0 {
            http_port = set.lan_http_port;
        }
        if mixed_port == 0 {
            mixed_port = set.mixed_port;
        }
        if mixed_listen.is_empty() {
            mixed_listen = set.mixed_listen.clone();
        }
        if mixed_listen.is_empty() {
            mixed_listen = "127.0.0.1".into();
        }

        // 面板/内核固定占用的端口
        let reserved: HashMap<i64, &str> = HashMap::from([
            (9090, "面板"),
            (9091, "Clash API"),
            (12345, "透明代理"),
            (5354, "局域网 DNS"),
            (53, "DNS"),
        ]);

        if !matches!(mixed_listen.as_str(), "127.0.0.1" | "::1" | "0.0.0.0" | "::") {
            return err_json(
                StatusCode::BAD_REQUEST,
                "bad_listen",
                "mixed 监听地址只能是 127.0.0.1、::1、0.0.0.0 或 ::",
            );
        }
        if !(1024..=65535).contains(&mixed_port) {
            return err_json(StatusCode::BAD_REQUEST, "bad_port", "mixed 端口需在 1024-65535 之间");
        }
        if let Some(name) = reserved.get(&mixed_port) {
            return err_json(
                StatusCode::BAD_REQUEST,
                "port_conflict",
                &format!("端口 {mixed_port} 已被{name}占用"),
            );
        }

        if enabled {
            for p in [socks_port, http_port] {
                if !(1024..=65535).contains(&p) {
                    return err_json(StatusCode::BAD_REQUEST, "bad_port", "端口需在 1024-65535 之间");
                }
                if let Some(name) = reserved.get(&p) {
                    return err_json(
                        StatusCode::BAD_REQUEST,
                        "port_conflict",
                        &format!("端口 {p} 已被{name}占用"),
                    );
                }
                if p == mixed_port {
                    return err_json(
                        StatusCode::BAD_REQUEST,
                        "port_conflict",
                        &format!("端口 {p} 已被 mixed 入站占用"),
                    );
                }
            }
            if socks_port == http_port {
                return err_json(
                    StatusCode::BAD_REQUEST,
                    "port_conflict",
                    "SOCKS5 与 HTTP 端口不能相同",
                );
            }
            if username.is_empty() && password.is_empty() {
                // 开放代理（无鉴权）
            } else if !username.is_empty() && !password.is_empty() {
                if password.len() < 8 {
                    return err_json(StatusCode::BAD_REQUEST, "weak_password", "密码至少 8 位");
                }
            } else {
                return err_json(
                    StatusCode::BAD_REQUEST,
                    "bad_creds",
                    "用户名和密码必须同时填写，或同时留空（留空=不鉴权）",
                );
            }
        }

        let mut set2 = set.clone();
        set2.lan_proxy = enabled;
        set2.lan_socks_port = socks_port;
        set2.lan_http_port = http_port;
        set2.mixed_port = mixed_port;
        set2.mixed_listen = mixed_listen;
        // 关闭开关时前端可能提交空账号：此时保留原凭据，避免密码被清空。
        if enabled || !username.is_empty() || !password.is_empty() {
            set2.lan_user = username;
            set2.lan_pass = password;
        }

        // 先渲染校验，成功后再落盘：否则会出现「接口返回 400 但设置已生效」。
        let raw = match app.render_config(&set2).await {
            Ok(r) => r,
            Err(e) => return err_json(StatusCode::BAD_REQUEST, "render_failed", &e),
        };
        if let Err(e) = app.store.save_settings(set2.clone()) {
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "save_failed", &e.to_string());
        }
        let apply_res =
            tokio::time::timeout(APPLY_TIMEOUT, app.applier.apply(&raw, "局域网代理设置变更")).await;
        match apply_res {
            Err(_) | Ok(Err(_)) => {
                if let Err(e) = app.store.save_settings(prev) {
                    eprintln!("回滚局域网代理设置失败: {e}");
                }
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

        let host = headers.get(axum::http::header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("").split(':').next().unwrap_or("").to_string();
        let host = if host.is_empty() { "127.0.0.1".to_string() } else { host };
        let (socks_url, http_url) = lan_proxy_urls(
            &host,
            set2.lan_socks_port,
            set2.lan_http_port,
            &set2.lan_user,
            &set2.lan_pass,
        );
        json_ok(json!({
            "ok": true,
            "enabled": set2.lan_proxy,
            "socks_url": socks_url,
            "http_url": http_url,
            "username": set2.lan_user,
            "password": set2.lan_pass,
            "open": set2.lan_proxy && set2.lan_user.is_empty() && set2.lan_pass.is_empty(),
            "mixed_port": set2.mixed_port,
            "mixed_listen": set2.mixed_listen,
            "mixed_url": format!("{}:{}", set2.mixed_listen, set2.mixed_port),
            "mixed_open": set2.mixed_listen != "127.0.0.1" && set2.mixed_listen != "::1",
        }))
    }

    /* ------------------------------------------------------------ tun */

    pub(crate) async fn h_tun_get(State(app): State<Arc<Server>>, headers: HeaderMap) -> Response {
        if let Some(resp) = app.authed(&headers) {
            return resp;
        }
        let set = app.store.settings();
        let tun_running = {
            let raw = app.store.config().unwrap_or_default();
            crate::config::inspect::tun_running(&raw, &app.engine_name())
        };
        json_ok(json!({
            "enabled": set.tun_enabled,
            "running": tun_running && app.sup.running(),
            "supported": crate::platform::tun_supported(),
            "platform": std::env::consts::OS,
            "note": "TUN 通过虚拟网卡接管整机流量（含不走系统代理的程序）；开关会重启核心",
        }))
    }

    pub(crate) async fn h_tun_set(
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
        let enabled = inb.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
        if enabled && !crate::platform::tun_supported() {
            return err_json(
                StatusCode::BAD_REQUEST,
                "unsupported",
                crate::platform::tun_unsupported_reason(),
            );
        }
        let set = app.store.settings();
        let prev = set.clone();
        let mut set2 = set;
        set2.tun_enabled = enabled;
        let raw = match app.render_config(&set2).await {
            Ok(r) => r,
            Err(e) => return err_json(StatusCode::BAD_REQUEST, "render_failed", &e),
        };
        if let Err(e) = app.store.save_settings(set2) {
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "save_failed", &e.to_string());
        }
        let note = if enabled { "TUN 模式 开启" } else { "TUN 模式 关闭" };
        let apply_res = tokio::time::timeout(APPLY_TIMEOUT, app.applier.apply(&raw, note)).await;
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
        let tun_running = {
            let raw = app.store.config().unwrap_or_default();
            crate::config::inspect::tun_running(&raw, &app.engine_name())
        };
        json_ok(json!({"ok": true, "enabled": enabled, "running": tun_running}))
    }

    /* ------------------------------------------------------------ sysproxy（Windows 系统代理） */

    pub(crate) async fn h_sysproxy_get(
        State(app): State<Arc<Server>>,
        headers: HeaderMap,
    ) -> Response {
        if let Some(resp) = app.authed(&headers) {
            return resp;
        }
        let st = crate::sysproxy::status(app.store.dir());
        json_ok(json!({
            "enabled": st.enabled,
            "server": st.server,
            "supported": cfg!(windows),
        }))
    }

    pub(crate) async fn h_sysproxy_set(
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
        let enabled = inb.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
        if !cfg!(windows) {
            return err_json(StatusCode::BAD_REQUEST, "unsupported", "系统代理仅 Windows 支持");
        }
        let set = app.store.settings();
        let port = if set.mixed_port > 0 { set.mixed_port } else { 7890 };
        if let Err(e) =
            crate::sysproxy::set_enabled(enabled, port as u16, app.store.dir())
        {
            return err_json(StatusCode::BAD_REQUEST, "sysproxy_failed", &e);
        }
        Self::h_sysproxy_get(State(app), headers).await
    }

    /* ------------------------------------------------------------ update（面板自更新） */

    pub(crate) async fn h_update_get(
        State(app): State<Arc<Server>>,
        headers: HeaderMap,
    ) -> Response {
        if let Some(resp) = app.authed(&headers) {
            return resp;
        }
        let http = reqwest::Client::builder()
            .user_agent("sing-box-web-panel")
            .build()
            .unwrap_or_default();
        let set = app.store.settings();
        let mixed_port = if set.mixed_port > 0 { set.mixed_port as u16 } else { 7890 };
        match crate::update::check_with_fallback(&http, mixed_port).await {
            Ok(st) => json_ok(json!({
                "current": st.current,
                "latest": st.latest,
                "has_update": st.has_update,
                "supported": st.supported,
                "asset": st.asset,
            })),
            Err(e) => json_ok(json!({
                "current": crate::api::VERSION,
                "latest": "",
                "has_update": false,
                "supported": crate::update::platform_artifact().is_some(),
                "error": e,
            })),
        }
    }

    pub(crate) async fn h_update_set(
        State(app): State<Arc<Server>>,
        headers: HeaderMap,
    ) -> Response {
        if let Some(resp) = app.authed(&headers) {
            return resp;
        }
        // 并发防护：同一时刻只允许一个更新任务（两个页面同时点更新会双下载并
        // 竞争交换文件）
        static UPD_BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        use std::sync::atomic::Ordering;
        if UPD_BUSY
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return err_json(
                StatusCode::CONFLICT,
                "update_busy",
                "已有更新任务正在进行，请稍候",
            );
        }
        let http = reqwest::Client::builder()
            .user_agent("sing-box-web-panel")
            .build()
            .unwrap_or_default();
        let set = app.store.settings();
        let mixed_port = if set.mixed_port > 0 { set.mixed_port as u16 } else { 7890 };
        let r = crate::update::perform_update(&http, &app.sup, app.store.dir(), mixed_port).await;
        UPD_BUSY.store(false, Ordering::SeqCst);
        match r {
            Ok(tag) => json_ok(json!({"ok": true, "restarting": true, "version": tag})),
            Err(e) => err_json(StatusCode::BAD_REQUEST, "update_failed", &e),
        }
    }

    /* ------------------------------------------------------------ autostart */

    pub(crate) async fn h_autostart_get(
        State(app): State<Arc<Server>>,
        headers: HeaderMap,
    ) -> Response {
        if let Some(resp) = app.authed(&headers) {
            return resp;
        }
        let set = app.store.settings();
        let core = set.autostart_core.unwrap_or(true);
        let mut resp = json!({
            "core": core,
            "platform": std::env::consts::OS,
            "core_note": "面板启动时自动拉起 sing-box 核心；关闭后需在「服务」页手动启动",
        });
        match crate::platform::autostart_enabled() {
            Err(e) => {
                resp["panel"] = json!(false);
                resp["panel_supported"] = json!(false);
                resp["panel_note"] = json!(e);
            }
            Ok(panel) => {
                resp["panel"] = json!(panel);
                resp["panel_supported"] = json!(true);
                resp["panel_note"] = json!(crate::platform::autostart_note());
            }
        }
        json_ok(resp)
    }

    pub(crate) async fn h_autostart_set(
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
        if let Some(panel) = inb.get("panel").and_then(|v| v.as_bool()) {
            // UAC 提权路径会阻塞最长约 2 分钟（等待用户确认弹窗），
            // 丢进 spawn_blocking 避免占死 tokio worker。
            let data_dir = app.store.dir().to_path_buf();
            let r = tokio::task::spawn_blocking(move || {
                crate::platform::set_autostart(panel, &data_dir)
            })
            .await
            .unwrap_or_else(|e| Err(format!("内部错误: {e}")));
            if let Err(e) = r {
                return err_json(StatusCode::BAD_REQUEST, "autostart_failed", &e);
            }
        }
        if let Some(core) = inb.get("core").and_then(|v| v.as_bool()) {
            let mut set = app.store.settings();
            set.autostart_core = Some(core);
            if let Err(e) = app.store.save_settings(set) {
                return err_json(StatusCode::INTERNAL_SERVER_ERROR, "save_failed", &e.to_string());
            }
        }
        Self::h_autostart_get(State(app), headers).await
    }

    /* ------------------------------------------------------------ routing */

    fn routing_mode_name(mode: &str) -> &'static str {
        match mode {
            "global" => "全局代理",
            "direct" => "全局直连",
            _ => "规则分流",
        }
    }

    pub(crate) async fn h_routing_get(
        State(app): State<Arc<Server>>,
        headers: HeaderMap,
    ) -> Response {
        if let Some(resp) = app.authed(&headers) {
            return resp;
        }
        let mut mode = app.store.settings().routing_mode;
        if !matches!(mode.as_str(), "rule" | "global" | "direct") {
            mode = "rule".into();
        }
        json_ok(json!({
            "mode": mode,
            "name": Self::routing_mode_name(&mode),
            "note": "规则分流：国内域名/IP 直连；全局代理：包括国内流量在内全部走节点；全局直连：全部不经过节点",
        }))
    }

    pub(crate) async fn h_routing_set(
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
        let mode = inb.get("mode").and_then(|v| v.as_str()).unwrap_or("");
        if !matches!(mode, "rule" | "global" | "direct") {
            return err_json(
                StatusCode::BAD_REQUEST,
                "bad_mode",
                "路由模式只能是 rule / global / direct",
            );
        }
        let set = app.store.settings();
        let prev = set.clone();
        if set.routing_mode == mode {
            return json_ok(json!({"ok": true, "mode": mode, "name": Self::routing_mode_name(mode)}));
        }
        let mut set2 = set;
        set2.routing_mode = mode.to_string();
        let raw = match app.render_config(&set2).await {
            Ok(r) => r,
            Err(e) => return err_json(StatusCode::BAD_REQUEST, "render_failed", &e),
        };
        if let Err(e) = app.store.save_settings(set2) {
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "save_failed", &e.to_string());
        }
        let note = format!("路由模式切换为{}", Self::routing_mode_name(mode));
        let apply_res = tokio::time::timeout(APPLY_TIMEOUT, app.applier.apply(&raw, &note)).await;
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
        json_ok(json!({"ok": true, "mode": mode, "name": Self::routing_mode_name(mode)}))
    }

    /* ------------------------------------------------------------ engine */

    pub(crate) async fn h_engine_get(
        State(app): State<Arc<Server>>,
        headers: HeaderMap,
    ) -> Response {
        if let Some(resp) = app.authed(&headers) {
            return resp;
        }
        let cur = app.engine_name();
        let mut list: Vec<Value> = Vec::new();
        for name in engine::supported() {
            let bin = app.bins.get(*name).cloned().unwrap_or_default();
            let mut item = json!({
                "name": name,
                "label": engine::label(name),
                "bin": bin,
                "active": *name == cur,
            });
            match tokio::time::timeout(Duration::from_secs(4), engine::probe(&bin, name)).await {
                Err(_) => {
                    item["ok"] = json!(false);
                    item["error"] = json!("context deadline exceeded");
                }
                Ok(Err(e)) => {
                    item["ok"] = json!(false);
                    item["error"] = json!(e);
                }
                Ok(Ok(ver)) => {
                    item["ok"] = json!(true);
                    item["version"] = json!(ver);
                    item["version_short"] = json!(engine::short_version(name, &ver));
                }
            }
            list.push(item);
        }
        json_ok(json!({
            "engine": cur,
            "label": engine::label(&cur),
            "config": engine::config_file(&cur),
            "available": list,
        }))
    }

    /// 面板内切换内核：sing-box ↔ mihomo。失败自动回退。
    pub(crate) async fn h_engine_set(
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
        let target_name = inb.get("engine").and_then(|v| v.as_str()).unwrap_or("");
        if !engine::valid(target_name) {
            return err_json(StatusCode::BAD_REQUEST, "bad_engine", "内核只能是 sing-box 或 mihomo");
        }
        let target = engine::normalize(target_name);
        let cur = app.engine_name();
        if target == cur {
            return json_ok(json!({"ok": true, "engine": cur, "changed": false}));
        }
        let bin = app.bins.get(&target).cloned().unwrap_or_default();
        if bin.is_empty() {
            return err_json(
                StatusCode::BAD_REQUEST,
                "engine_missing",
                &format!("没有为 {} 配置可执行文件路径", engine::label(&target)),
            );
        }
        let ver = match tokio::time::timeout(Duration::from_secs(8), engine::probe(&bin, &target))
            .await
        {
            Err(_) => {
                return err_json(
                    StatusCode::BAD_REQUEST,
                    "engine_unavailable",
                    &format!("{} 不可用: context deadline exceeded", engine::label(&target)),
                )
            }
            Ok(Err(e)) => {
                return err_json(
                    StatusCode::BAD_REQUEST,
                    "engine_unavailable",
                    &format!("{} 不可用: {e}", engine::label(&target)),
                )
            }
            Ok(Ok(v)) => v,
        };

        // 切到 mihomo 之前把 geo 数据备好（通常已由 PrewarmGeo 完成）。
        // 下载失败也不拦着切换，交给 mihomo 自己重试。
        if engine::is_mihomo(&target) {
            match tokio::time::timeout(Duration::from_secs(180), app.geo.ensure()).await {
                Err(_) => eprintln!("geo 数据准备失败，交给 mihomo 自行下载: timeout"),
                Ok((_, Err(e))) => {
                    eprintln!("geo 数据准备失败，交给 mihomo 自行下载: {e}")
                }
                Ok((_, Ok(()))) => {}
            }
        }

        let prev_set = app.store.settings();
        let prev_cfg = app.store.config().unwrap_or_default();
        let mut set = prev_set.clone();
        set.engine = target.clone();

        // 先切 store 的引擎：ConfigPath / TemplatePath 跟着指向新内核的文件。
        if let Err(e) = app.store.set_engine(&target) {
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "save_failed", &e.to_string());
        }
        let raw = match app.render_config(&set).await {
            Ok(r) => r,
            Err(e) => {
                app.restore_engine(&cur, &prev_set).await;
                return err_json(StatusCode::BAD_REQUEST, "render_failed", &e);
            }
        };
        if let Err(e) = app.store.save_settings(set) {
            app.restore_engine(&cur, &prev_set).await;
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "save_failed", &e.to_string());
        }

        // 旧核心必须先停：两个内核会抢同一个 Clash API 端口与 mixed 端口。
        let _ = app.sup.stop().await;
        app.sup.set_engine(
            std::path::Path::new(&bin),
            &app.store.config_path(),
            &target,
        );
        app.applier.set_engine(&bin, &target).await;

        let apply_res = tokio::time::timeout(
            APPLY_TIMEOUT,
            app.applier
                .apply(&raw, &format!("切换内核为 {}", engine::label(&target))),
        )
        .await;
        match apply_res {
            Err(_) | Ok(Err(_)) => {
                let _ = app.sup.stop().await;
                app.restore_engine(&cur, &prev_set).await;
                if !prev_cfg.is_empty() {
                    let _ = app.store.write_config(&prev_cfg);
                    let _ = app.sup.start();
                }
                app.failsafe_network().await;
                let msg = match apply_res {
                    Ok(Err(e)) => e.to_string(),
                    _ => "apply timeout".to_string(),
                };
                return err_json(StatusCode::BAD_REQUEST, "apply_failed", &msg);
            }
            Ok(Ok(())) => {}
        }

        // 两个内核对 /proxies/{name}/delay 的测量口径不同，切完必须清掉。
        app.clash.reset_delays();
        // 换了内核，出口 IP 一定变了。
        app.clear_exit_ip().await;

        // 新内核的入站布局与旧的不同，透明代理规则必须重新核对。
        if prev_set.tproxy_enabled && tproxy::supported() {
            if let Err(e) = config::inspect::verify_tproxy(
                &raw,
                &target,
                prev_set.tproxy_port,
                prev_set.tproxy_dns,
            ) {
                let _ = tproxy::disable(&app.tp).await;
                let mut set2 = app.store.settings();
                set2.tproxy_enabled = false;
                let _ = app.store.save_settings(set2);
                return json_ok(json!({
                    "ok": true, "engine": target, "label": engine::label(&target),
                    "version": engine::short_version(&target, &ver), "version_full": ver,
                    "changed": true,
                    "warning": format!("新内核的配置不满足透明代理接管条件，已自动关闭透明代理：{e}"),
                }));
            }
        }

        // 现在跑的是 sing-box，正好在后台把 mihomo 的 geo 数据备好。
        if !engine::is_mihomo(&target) {
            app.prewarm_geo();
        }

        json_ok(json!({
            "ok": true, "engine": target, "label": engine::label(&target),
            "version": engine::short_version(&target, &ver), "version_full": ver, "changed": true,
        }))
    }

    async fn restore_engine(&self, name: &str, set: &crate::model::Settings) {
        let _ = self.store.set_engine(name);
        let mut set = set.clone();
        set.engine = name.to_string();
        let _ = self.store.save_settings(set);
        let bin = self.bins.get(name).cloned().unwrap_or_default();
        self.sup.set_engine(std::path::Path::new(&bin), &self.store.config_path(), name);
        self.applier.set_engine(&bin, name).await;
    }

    /* ------------------------------------------------------------ geodata */

    pub(crate) async fn h_geo_status(
        State(app): State<Arc<Server>>,
        headers: HeaderMap,
    ) -> Response {
        if let Some(resp) = app.authed(&headers) {
            return resp;
        }
        let st = app.geo.status().await;
        json_ok(json!({"geo": st}))
    }

    pub(crate) async fn h_geo_prefetch(
        State(app): State<Arc<Server>>,
        headers: HeaderMap,
    ) -> Response {
        if let Some(resp) = app.authed(&headers) {
            return resp;
        }
        if app.geo.ready().await {
            let st = app.geo.status().await;
            return json_ok(json!({"ok": true, "geo": st, "message": "geo 数据已就绪"}));
        }
        let geo = app.geo.clone();
        tokio::spawn(async move {
            let _ = tokio::time::timeout(Duration::from_secs(180), geo.ensure()).await;
        });
        let st = app.geo.status().await;
        json_ok(json!({"ok": true, "geo": st, "message": "已开始后台下载"}))
    }
}

/// 生成局域网代理的连接串；用户名/密码都为空时生成无鉴权的开放代理地址。
fn lan_proxy_urls(
    host: &str,
    socks_port: i64,
    http_port: i64,
    user: &str,
    pass: &str,
) -> (String, String) {
    let auth = if !user.is_empty() && !pass.is_empty() {
        format!("{user}:{pass}@")
    } else {
        String::new()
    };
    (
        format!("socks5://{auth}{host}:{socks_port}"),
        format!("http://{auth}{host}:{http_port}"),
    )
}
