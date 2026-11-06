// sing-box / mihomo Web 控制面板（Rust 版，面向 MT7981 / 16 MB flash 的低配设备）。
// 启动序列与 Go 版一致：参数 → panel.conf → store → 孤儿清扫 → TUN 降级 →
// 自动启动核心 → geo 预取 → 透明代理恢复 → 调度器 → 监听 → 信号退出清理。

mod api;
mod clashclient;
mod config;
mod engine;
mod geodata;
mod listen;
mod model;
mod panelconf;
mod platform;
mod store;
mod subscription;
mod sysproxy;
mod supervisor;
mod supervisor_sys;
#[cfg_attr(not(windows), allow(dead_code))]
mod tray;
mod tproxy;
mod util;
mod webfs;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clapless::Args;

mod clapless {
    // 极简 flag 解析：支持 -name value 与 -name=value，语义与 Go flag 一致。
    pub struct Args {
        pub data: Option<String>,
        pub listen: Option<String>,
        pub sing_box: Option<String>,
        pub mihomo: Option<String>,
        pub engine: Option<String>,
        pub clash: Option<String>,
        pub token: Option<String>,
        pub password: Option<String>,
        pub autostart_explicit: Option<bool>,
        /// Windows 托盘开关：-notray 关闭（会话 0 / unix 自动无托盘）
        pub notray: bool,
    }

    impl Args {
        pub fn parse() -> Args {
            let mut a = Args {
                data: None,
                listen: None,
                sing_box: None,
                mihomo: None,
                engine: None,
                clash: None,
                token: None,
                password: None,
                autostart_explicit: None,
                notray: false,
            };
            let argv: Vec<String> = std::env::args().skip(1).collect();
            let mut i = 0;
            while i < argv.len() {
                let arg = &argv[i];
                // 兼容 Go flag 的 -flag 与 --flag 两种写法
                let (name, inline): (String, Option<String>) = if arg.starts_with('-') {
                    let rest = arg.trim_start_matches('-');
                    match rest.split_once('=') {
                        Some((n, v)) => (n.to_string(), Some(v.to_string())),
                        None => (rest.to_string(), None),
                    }
                } else {
                    i += 1;
                    continue;
                };
                let take_value = |i: &mut usize| -> Option<String> {
                    if let Some(v) = &inline {
                        return Some(v.clone());
                    }
                    *i += 1;
                    argv.get(*i).cloned()
                };
                match name.as_str() {
                    "data" => a.data = take_value(&mut i),
                    "listen" => a.listen = take_value(&mut i),
                    "sing-box" => a.sing_box = take_value(&mut i),
                    "mihomo" => a.mihomo = take_value(&mut i),
                    "engine" => a.engine = take_value(&mut i),
                    "clash" => a.clash = take_value(&mut i),
                    "token" => a.token = take_value(&mut i),
                    "password" => a.password = take_value(&mut i),
                    "autostart" => {
                        let v = take_value(&mut i)
                            .map(|s| s == "true")
                            .unwrap_or(true);
                        a.autostart_explicit = Some(v);
                    }
                    "notray" => a.notray = true,
                    "h" | "help" => {
                        eprintln!(
                            "用法: panel [-data 目录] [-listen host:port] [-sing-box 路径] [-mihomo 路径] \
                             [-engine sing-box|mihomo] [-clash host:port] [-token 令牌] [-password 密码] \
                             [-autostart true|false] [-notray]"
                        );
                        std::process::exit(0);
                    }
                    _ => {}
                }
                i += 1;
            }
            a
        }
    }
}

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2) // MT7981 只有双核，别贪多
        .enable_all()
        .build()
        .expect("tokio runtime");
    let code = runtime.block_on(run());
    std::process::exit(code);
}

fn default_data_dir() -> PathBuf {
    #[cfg(windows)]
    {
        match std::env::current_exe() {
            Ok(exe) => exe
                .parent()
                .unwrap_or(Path::new("."))
                .join("panel-data"),
            Err(_) => PathBuf::from("panel-data"),
        }
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("/opt/sing-box-panel/data")
    }
}

fn default_nft_path() -> String {
    #[cfg(windows)]
    {
        "nft".to_string()
    }
    #[cfg(not(windows))]
    {
        "/usr/sbin/nft".to_string()
    }
}

/// 按「标准安装路径 → 面板同目录 → 兜底路径」的顺序找可执行文件。
#[cfg(not(windows))]
fn pick_bin(name: &str, fallback: &str) -> String {
    for p in [
        fallback.to_string(),
        format!("/usr/bin/{name}"),
        format!("/opt/sing-box-panel/bin/{name}"),
    ] {
        if Path::new(&p).exists() {
            return p;
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join(name);
            if p.exists() {
                return p.to_string_lossy().into_owned();
            }
        }
    }
    fallback.to_string()
}

fn default_sing_box_bin() -> String {
    #[cfg(windows)]
    {
        if let Ok(exe) = std::env::current_exe() {
            let p = exe.parent().unwrap_or(Path::new(".")).join("sing-box.exe");
            if p.exists() {
                return p.to_string_lossy().into_owned();
            }
        }
        "sing-box.exe".to_string()
    }
    #[cfg(not(windows))]
    {
        pick_bin("sing-box", "/usr/local/bin/sing-box")
    }
}

fn default_mihomo_bin() -> String {
    #[cfg(windows)]
    {
        if let Ok(exe) = std::env::current_exe() {
            let p = exe.parent().unwrap_or(Path::new(".")).join("mihomo.exe");
            if p.exists() {
                return p.to_string_lossy().into_owned();
            }
        }
        "mihomo.exe".to_string()
    }
    #[cfg(not(windows))]
    {
        pick_bin("mihomo", "/usr/local/bin/mihomo")
    }
}

/// 词法清理路径（等价 Go filepath.Abs 的 Clean）：去掉 ./ 与多余的 /，
/// 不访问文件系统（Windows canonicalize 会带 \?\ 前缀，不能用）。
fn clean_path(p: &Path) -> PathBuf {
    let text = p.to_string_lossy().into_owned();
    let bytes = text.as_bytes();
    let (drive, rest) = if bytes.len() >= 2 && bytes[1] == b':' {
        text.split_at(2)
    } else {
        ("", text.as_str())
    };
    let rooted = rest.starts_with('/') || rest.starts_with('\\');
    let mut out: Vec<&str> = Vec::new();
    const SEPARATORS: &[char] = &['/', '\\'];
    for seg in rest.split(SEPARATORS) {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    let joined = out.join("/");
    let mut result = String::new();
    result.push_str(drive);
    if rooted {
        result.push('/');
    }
    result.push_str(&joined);
    if result.is_empty() {
        return PathBuf::from(".");
    }
    PathBuf::from(result)
}

fn login_hash(user: &str, pass: &str) -> String {
    crate::util::sha256_hex(format!("{user}:{pass}").as_bytes())
}

/// 把 panel.conf/启动参数里的账户密码落到 settings。
/// 全新面板（无密码哈希）且没有提供密码时保持「待初始化」状态，由用户
/// 在网页上完成首次账户设置，不再自动生成随机密码；提供密码则直接应用
/// （自动化部署路径）。关闭登录只能用面板内开关，panel.conf 不再承担。
fn apply_login_conf(settings: &mut model::Settings, conf: &panelconf::PanelConf, flag_pass: &str) {
    let mut pass = flag_pass.to_string();
    if pass.is_empty() {
        pass = conf.password.clone();
    }
    let mut user = settings.username.clone();
    if !conf.username.is_empty() {
        user = conf.username.clone();
    }

    if settings.password_hash.is_empty() {
        // 全新面板：没有密码就交给网页初始化流程（needs_setup），
        // conf/参数预填的用户名保留下来作为默认值。
        if pass.is_empty() {
            settings.username = user;
            settings.login_disabled = false;
            return;
        }
        if user.is_empty() {
            user = "admin".into();
        }
        settings.username = user;
        settings.password_hash = login_hash(&settings.username, &pass);
        settings.login_disabled = false;
        return;
    }
    if pass.is_empty() {
        // 已有账户且未提供新密码：保持现状
        if !conf.username.is_empty() && user != settings.username {
            eprintln!("panel.conf 修改了用户名但未填 password，已忽略（改用户名需同时填写 password）");
        }
        return;
    }
    // 提供密码即视为要更新凭据，即使密码与原有哈希相同也要清掉免密开关。
    settings.login_disabled = false;
    let hash = login_hash(&user, &pass);
    if hash != settings.password_hash {
        settings.username = user;
        settings.password_hash = hash;
        eprintln!("登录账户/密码已更新（来自 panel.conf 或启动参数）");
    }
}

async fn run() -> i32 {
    let args = Args::parse();
    let env = |k: &str| std::env::var(k).unwrap_or_default();

    // 命令行显式指定 -listen 时，面板里的修改只能临时生效。
    let listen_pinned = args.listen.as_deref().map(|s| !s.is_empty()).unwrap_or(false);

    // 端口/账户/密码的来源优先级：命令行参数 > panel.conf > 内置默认。
    let conf = panelconf::load_panel_conf();
    let mut listen_addr = args.listen.clone().unwrap_or_default();
    if listen_addr.is_empty() {
        listen_addr = conf.listen.clone();
    }
    if listen_addr.is_empty() {
        listen_addr = "127.0.0.1:9090".into();
    }

    let mut data_dir = args.data.clone().map(PathBuf::from).unwrap_or_else(default_data_dir);
    // 数据目录统一成绝对路径：渲染出的 cache_file.path、mihomo 的 -d、
    // 子进程工作目录都以它为准。
    if let Ok(abs) = data_dir.canonicalize() {
        data_dir = abs;
    } else if data_dir.is_absolute() {
        // 保持原样
    } else if let Ok(cwd) = std::env::current_dir() {
        data_dir = cwd.join(&data_dir);
    }
    // Windows canonicalize 会带 \\?\ 前缀，落到配置里很难看且部分核心解析不了
    {
        const VERBATIM: &str = r"\\?\";
        let text = data_dir.to_string_lossy().into_owned();
        if let Some(stripped) = text.strip_prefix(VERBATIM) {
            data_dir = PathBuf::from(stripped);
        }
    }
    data_dir = clean_path(&data_dir);

    let st = match store::Store::open(&data_dir) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("open store: {e}");
            return 1;
        }
    };

    let mut settings = st.settings();
    settings.listen = listen_addr.clone();
    // 内核选择：命令行 -engine > settings.json > sing-box。
    if let Some(eng_flag) = &args.engine {
        if !engine::valid(eng_flag) {
            eprintln!("未知内核 {eng_flag:?}：只支持 sing-box 或 mihomo");
            return 1;
        }
        settings.engine = engine::normalize(eng_flag);
    }
    if let Err(e) = st.set_engine(&settings.engine) {
        eprintln!("switch engine: {e}");
        return 1;
    }
    let eng = engine::normalize(&settings.engine);
    let mut bins: HashMap<String, String> = HashMap::new();
    bins.insert(
        engine::SING_BOX.to_string(),
        args.sing_box.clone().unwrap_or_else(default_sing_box_bin),
    );
    bins.insert(
        engine::MIHOMO.to_string(),
        args.mihomo.clone().unwrap_or_else(default_mihomo_bin),
    );
    let core_bin = bins.get(&eng).cloned().unwrap_or_default();
    if let Some(clash_addr) = &args.clash {
        if !clash_addr.is_empty() {
            settings.clash_addr = clash_addr.clone();
        }
    }
    let flag_pass = args
        .password
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| env("PANEL_PASSWORD"));
    apply_login_conf(&mut settings, &conf, &flag_pass);
    // 已有配置但没有版本号（从旧版本升级）时，从 1 起算
    if settings.config_version == 0 {
        if let Ok(raw) = st.config() {
            if !raw.is_empty() {
                settings.config_version = 1;
            }
        }
    }
    if let Err(e) = st.save_settings(settings.clone()) {
        eprintln!("save settings: {e}");
        return 1;
    }
    if settings.login_disabled || (settings.username.is_empty() && settings.password_hash.is_empty()) {
        eprintln!("警告: 面板登录已关闭，{listen_addr} 可免密码访问");
    }

    let sup = supervisor::Supervisor::new(Path::new(&core_bin), &st.config_path(), &eng);
    // 上次面板被强杀时，子核心会变成孤儿并一直占着 cache.db/9091。
    // 清扫要起 PowerShell 查进程（冷启动 8–10 秒），所以丢到后台跑：
    // 面板先把端口监听起来，用户打开浏览器立刻能进页面。
    // 两个内核合并成一次查询；清扫结果必须在自动启动核心之前等到，
    // 否则孤儿占着 9091 会让探活误判成「已有内核在跑」而跳过启动。
    let sweep_bins: Vec<String> = bins.values().cloned().collect();
    let sweep_dir = data_dir.clone();
    let sweep = tokio::task::spawn_blocking(move || {
        supervisor::sweep_orphans(&sweep_bins, &sweep_dir);
    });

    // TUN 入站需要管理员令牌。非管理员启动时自动降级为无 TUN 配置并关掉开关。
    if settings.tun_enabled && !platform::tun_supported() {
        if let Ok(tpl_raw) = st.template_raw() {
            let mut opts = config::RenderOpts::from_settings(&settings, st.dir());
            opts.tun_enabled = false; // 本次就是要把 tun 关掉再渲染
            match config::render_bytes(&tpl_raw, &st.all_nodes(), &opts) {
                Ok(raw) => {
                    let _ = st.write_config(&raw);
                    let mut s2 = st.settings();
                    s2.tun_enabled = false;
                    settings.tun_enabled = false;
                    let _ = st.save_settings(s2);
                    eprintln!("未以管理员身份运行，TUN 已临时关闭（以管理员重启面板后可再开启）");
                }
                Err(_) => {}
            }
        }
    }

    let cl = Arc::new(clashclient::Client::new(&settings.clash_addr));
    cl.set_ttl(settings.delay_ttl);

    let tp = Arc::new(tproxy::new(
        &default_nft_path(),
        data_dir.join("tproxy.nft"),
        settings.tproxy_port,
    ));
    let static_token = args
        .token
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| env("PANEL_TOKEN"));
    let app = api::Server::new(st.clone(), sup.clone(), cl.clone(), bins.clone(), &static_token, tp.clone());

    // 先把 HTTP 服务拉起来：孤儿清扫、自动启动核心都不该挡住面板本身。
    let router = app.router();
    {
        let mut g = app.router.write().unwrap();
        *g = Some(router.clone());
    }
    let lm = match listen::ListenManager::new(&listen_addr, router).await {
        Ok(lm) => lm,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    {
        let mut g = app.listen.lock().await;
        *g = Some(lm);
    }
    app.listen_pinned
        .store(listen_pinned, std::sync::atomic::Ordering::Relaxed);
    api::register_global(app.clone());
    eprintln!(
        "panel listening on {listen_addr} (data={}, engine={eng}, bin={core_bin})",
        data_dir.display()
    );

    // 清扫完成后再决定要不要拉起核心（探活不能被残留进程带偏）。
    if let Err(e) = sweep.await {
        eprintln!("清理残留内核失败: {e}");
    }

    // 自动启动核心：先探活再决定，避免端口冲突导致启动失败。
    if args.autostart_explicit.unwrap_or(true) {
        let has_config = st.config().map(|c| !c.is_empty()).unwrap_or(false);
        if has_config {
            let mut autostart_core = true;
            if args.autostart_explicit.is_none() {
                if let Some(v) = settings.autostart_core {
                    autostart_core = v;
                }
            }
            if autostart_core {
                match tokio::time::timeout(
                    std::time::Duration::from_millis(1500),
                    cl.version(),
                )
                .await
                {
                    Ok(Ok(_)) => {
                        eprintln!(
                            "检测到运行中的 {}（Clash API 可用），跳过自动启动",
                            engine::label(&eng)
                        );
                    }
                    _ => {
                        if let Err(e) = sup.start() {
                            eprintln!("autostart {}: {e}", engine::label(&eng));
                        }
                    }
                }
            } else {
                eprintln!(
                    "{} 开机自启已在面板里关闭，跳过自动启动",
                    engine::label(&eng)
                );
            }
        } else {
            // 首次运行：还没有已应用配置时，尝试「渲染 → 应用」一体完成
            // （校验/落盘/启动核心/探活），让添加过订阅的部署开箱即跑。
            // best-effort：任何失败只记录原因，面板照常起，网页端仍可手工处理。
            let mut autostart_core = true;
            if args.autostart_explicit.is_none() {
                if let Some(v) = settings.autostart_core {
                    autostart_core = v;
                }
            }
            let has_enabled = st.all_nodes().iter().any(|n| n.enabled);
            if !autostart_core {
                eprintln!("首次启动: 核心自启已在面板里关闭，跳过自动应用");
            } else if !has_enabled {
                eprintln!("首次启动: 暂无启用节点，跳过自动应用（添加订阅后请在配置页应用或直接点启动）");
            } else {
                match app.render_config(&settings).await {
                    Err(e) => eprintln!("首次启动自动渲染失败: {e}"),
                    Ok(raw) => {
                        let r = tokio::time::timeout(
                            std::time::Duration::from_secs(150),
                            app.applier.apply(&raw, "首次启动自动应用"),
                        )
                        .await;
                        match r {
                            Ok(Ok(())) => eprintln!("首次启动: 配置已自动应用，核心已启动"),
                            Ok(Err(e)) => eprintln!(
                                "首次启动自动应用失败（可在配置页手工重试）: {e}"
                            ),
                            Err(_) => eprintln!("首次启动自动应用超时"),
                        }
                    }
                }
            }
        }
    }

    // 在后台把 mihomo 的 geo 数据（约 12 MB）备好，切内核不用等下载。
    app.prewarm_geo();

    // 透明代理规则必须在核心确认可运行之后再下发：否则流量会被重定向到
    // 一个没有监听的端口，整机（含局域网）会直接断网。
    if settings.tproxy_enabled && tproxy::supported() {
        if sup.running() {
            // 先确认已下发的配置里真有 redirect 入站。
            let raw = st.config().unwrap_or_default();
            match config::inspect::verify_tproxy(&raw, &eng, settings.tproxy_port, settings.tproxy_dns) {
                Err(e) => {
                    eprintln!("跳过恢复透明代理：{e}（已撤销残留规则）");
                    let _ = tproxy::disable(&tp).await;
                    let mut s2 = st.settings();
                    s2.tproxy_enabled = false;
                    settings.tproxy_enabled = false;
                    let _ = st.save_settings(s2);
                }
                Ok(()) => {
                    let mut cidrs = settings.lan_cidrs.clone();
                    if cidrs.is_empty() {
                        cidrs = tproxy::detect_lan();
                    }
                    match tproxy::apply(&tp, &cidrs, settings.tproxy_local, settings.tproxy_dns).await
                    {
                        Err(e) => eprintln!("恢复透明代理规则失败: {e}"),
                        Ok(()) => eprintln!("透明代理已恢复: {cidrs:?}"),
                    }
                }
            }
        } else {
            eprintln!("核心未运行，跳过透明代理规则（避免断网）");
            let _ = tproxy::disable(&tp).await;
        }
    } else {
        // 清理上次遗留的表，避免关掉开关后仍被接管
        let _ = tproxy::disable(&tp).await;
    }

    app.start_scheduler();

    // 系统托盘（仅 Windows；-notray / 会话 0 SYSTEM 自启时自动跳过）。
    // 必须放在监听起来之后：托盘的 tooltip/菜单要展示真实端口。
    let mut tray = if args.notray {
        None
    } else {
        tray::spawn(tray::TrayState {
            listen: listen_addr.clone(),
            core_running: sup.running(),
            autostart: platform::autostart_enabled().unwrap_or(false),
        })
    };
    if let Some((handle, _)) = &tray {
        let sync_handle = handle.clone();
        let sync_sup = sup.clone();
        let sync_st = st.clone();
        tokio::spawn(sync_tray_state(sync_handle, sync_sup, sync_st));
    }

    // 等待退出：Ctrl+C / SIGTERM / 托盘「退出」；托盘菜单的启停/自启就地处理
    wait_for_exit(&sup, &mut tray, &data_dir).await;

    // 优雅退出序列与 Ctrl+C 完全一致：关监听 → 杀核心树 → 撤透明代理规则
    if let Some((handle, _)) = tray.as_ref() {
        handle.remove();
    }

    {
        let mut g = app.listen.lock().await;
        if let Some(lm) = g.as_mut() {
            lm.shutdown().await;
        }
    }
    let _ = sup.stop().await;
    // 透明代理规则必须随面板退出一起撤掉：核心已死而规则残留时，
    // LAN 流量会被重定向到无人监听的端口，整网断网直到面板回来。
    if tproxy::supported() {
        if let Err(e) = tproxy::disable(&tp).await {
            eprintln!("退出时撤销透明代理规则失败: {e}");
        }
    }
    eprintln!("bye");
    0
}

/// 等待退出信号：unix 是 SIGTERM/SIGINT；Windows 上额外处理托盘菜单事件
/// （启停/重启核心、开机自启切换、退出），返回即进入统一退出序列。
async fn wait_for_exit(
    sup: &Arc<supervisor::Supervisor>,
    tray: &mut Option<(
        tray::TrayHandle,
        tokio::sync::mpsc::UnboundedReceiver<tray::TrayEvent>,
    )>,
    data_dir: &Path,
) {
    #[cfg(unix)]
    {
        let _ = (sup, tray, data_dir);
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
    }
    #[cfg(windows)]
    {
        let (handle, rx) = match tray {
            Some((h, r)) => (h, r),
            None => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => return,
                ev = rx.recv() => match ev {
                    None => {
                        // 托盘线程已退出（图标被收走等）：没有可等的事件了，
                        // 退化为只等 Ctrl+C，避免 select 立即触发误退出。
                        let _ = tokio::signal::ctrl_c().await;
                        return;
                    }
                    Some(tray::TrayEvent::Exit) => return,
                    Some(tray::TrayEvent::CoreToggle) => {
                        if sup.running() {
                            if let Err(e) = sup.stop().await {
                                eprintln!("托盘停止核心失败: {e}");
                            }
                        } else if let Err(e) = sup.start() {
                            eprintln!("托盘启动核心失败: {e}");
                        }
                        handle.update(|s| s.core_running = sup.running());
                    }
                    Some(tray::TrayEvent::CoreRestart) => {
                        if let Err(e) = sup.restart().await {
                            eprintln!("托盘重启核心失败: {e}");
                        }
                        handle.update(|s| s.core_running = sup.running());
                    }
                    Some(tray::TrayEvent::AutostartToggle) => {
                        let on = !platform::autostart_enabled().unwrap_or(false);
                        match platform::set_autostart(on, data_dir) {
                            Ok(()) => eprintln!(
                                "开机自启已{}（{}）",
                                if on { "开启" } else { "关闭" },
                                platform::autostart_note()
                            ),
                            Err(e) => eprintln!("切换开机自启失败: {e}"),
                        }
                        handle.update(|s| s.autostart = platform::autostart_enabled().unwrap_or(on));
                    }
                },
            }
        }
    }
}

/// 把核心运行态/监听地址/自启开关同步进托盘（tooltip 与菜单勾选态）。
/// sup.running() 与 settings() 都是内存读，轮询零成本；自启查询要起
/// schtasks 子进程，放宽到 30 秒一次。
async fn sync_tray_state(
    handle: tray::TrayHandle,
    sup: Arc<supervisor::Supervisor>,
    st: Arc<store::Store>,
) {
    let mut n: u32 = 0;
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(3));
    loop {
        tick.tick().await;
        let core_running = sup.running();
        let listen = st.settings().listen;
        let check_autostart = n % 10 == 0;
        n = n.wrapping_add(1);
        handle.update(|s| {
            s.core_running = core_running;
            s.listen = listen.clone();
            if check_autostart {
                s.autostart = platform::autostart_enabled().unwrap_or(s.autostart);
            }
        });
    }
}
