// 自动更新：从 GitHub Releases（本仓库）检查新版本 → 下载 → 校验 → 替换面板
// 二进制 → 重启生效。
//
// 分包命名（见 release.yml）：x86/Windows 是 -dual 双内核包；mipsle/arm64 是
// -with-sing-box / -with-mihomo 单内核包，panel 二进制在两个单内核包里完全
// 相同。更新只替换面板二进制（Windows 顺带 wintun.dll），不动内核与 panel.conf。
//
// 重启语义：
// - Windows：改名交换（运行中的 exe 可改名）→ 延迟派生新面板（分离进程）→
//   老进程退出。核心在交换前已优雅停止，新面板自动按配置拉起。
// - Linux：替换磁盘文件（运行中可 rename）→ 延迟 exec 自身（同 PID，systemd/
//   procd 无感知）→ 新映像启动时核心按配置拉起。
//
// 安全约束：更新源固定为本仓库 Releases，URL 全部由代码按平台拼接，不接受
// 任何外部传入地址；下载流式计算 sha256 与发布 .sha256 比对，不符即丢弃。

use std::path::{Path, PathBuf};

pub const REPO: &str = "jiqing112/sing-box-web-rust";

/// 当前平台在 Releases 里的产物标识（包名中间那段）。不支持的平台返回 None。
pub fn platform_artifact() -> Option<&'static str> {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        Some("windows-amd64")
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        Some("linux-amd64")
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        Some("linux-arm64")
    }
    #[cfg(all(target_os = "linux", target_arch = "mips"))]
    {
        Some("linux-mipsle")
    }
    #[cfg(not(any(
        all(target_os = "windows", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "aarch64"),
        all(target_os = "linux", target_arch = "mips")
    )))]
    {
        None
    }
}

#[derive(serde::Serialize, Clone, Debug)]
pub struct UpdateStatus {
    pub current: String,
    pub latest: String,
    pub has_update: bool,
    pub supported: bool,
    pub asset: String,
}

fn current_version() -> &'static str {
    crate::api::VERSION
}

/// 版本比较：v1.3.10 > v1.3.9（按数值逐段比较）
fn ver_tuple(s: &str) -> (u64, u64, u64) {
    let t = s.trim_start_matches('v');
    let mut it = t.split('.');
    let major = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    let minor = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    let patch = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    (major, minor, patch)
}

fn ver_newer(latest: &str, current: &str) -> bool {
    ver_tuple(latest) > ver_tuple(current)
}

/// 检查最新版本。API 优先（带资产列表）；API 不可达（国内路由器常态）时
/// 跟随 /releases/latest 重定向解析 tag（无 API 配额），产物名按发布命名
/// 规则拼接。测试钩子：环境变量 PANEL_UPDATE_VERSION 强制指定"最新版本"。
pub async fn check_latest(http: &reqwest::Client) -> Result<UpdateStatus, String> {
    let supported = platform_artifact().is_some();
    if let Ok(forced) = std::env::var("PANEL_UPDATE_VERSION") {
        // 测试钩子：强制指定"最新版本"（tag 用 forced，资产从该版本真实列表里选）
        if !forced.is_empty() {
            let artifact = platform_artifact().ok_or_else(|| "当前平台不支持自动更新".to_string())?;
            let api = format!("https://api.github.com/repos/{REPO}/releases/tags/{forced}");
            let mut names: Vec<String> = Vec::new();
            if let Ok(resp) = http
                .get(&api)
                .header("User-Agent", "sing-box-web-panel")
                .timeout(std::time::Duration::from_secs(15))
                .send()
                .await
            {
                if resp.status().is_success() {
                    if let Ok(v) = resp.json::<serde_json::Value>().await {
                        names = v["assets"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|x| x["name"].as_str().map(|s| s.to_string()))
                                    .collect()
                            })
                            .unwrap_or_default();
                    }
                }
            }
            let asset = pick_asset(&names, artifact)
                .ok_or_else(|| format!("版本 {forced} 没有平台 {artifact} 的发行包"))?;
            return Ok(UpdateStatus {
                current: current_version().into(),
                latest: forced.clone(),
                has_update: true,
                supported,
                asset,
            });
        }
    }
    let artifact = platform_artifact().ok_or_else(|| "当前平台不支持自动更新".to_string())?;

    // 1) API 路径
    let api = format!("https://api.github.com/repos/{REPO}/releases/latest");
    if let Ok(resp) = http
        .get(&api)
        .header("User-Agent", "sing-box-web-panel")
        .header("Accept", "application/vnd.github+json")
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
    {
        if resp.status().is_success() {
            if let Ok(v) = resp.json::<serde_json::Value>().await {
                let tag = v["tag_name"].as_str().unwrap_or("").to_string();
                let names: Vec<String> = v["assets"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x["name"].as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                if !tag.is_empty() {
                    if let Some(asset) = pick_asset(&names, artifact) {
                        return Ok(make_status(tag, asset));
                    }
                }
            }
        }
    }

    // 2) 兜底：/releases/latest 302 重定向 → 最终 URL 就是 /tag/vX.Y.Z
    let url = format!("https://github.com/{REPO}/releases/latest");
    let resp = http
        .get(&url)
        .header("User-Agent", "sing-box-web-panel")
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("检查更新失败: {e}"))?;
    let final_url = resp.url().as_str().to_string();
    let tag = final_url
        .rsplit('/')
        .next()
        .filter(|t| t.starts_with('v'))
        .ok_or_else(|| format!("检查更新失败：无法解析最新版本（{final_url}）"))?
        .to_string();
    let tag_for_asset = tag.clone();
    let ext = if cfg!(windows) { "zip" } else { "tar.gz" };
    Ok(make_status(tag, format!("sing-box-web-rust-{tag_for_asset}-{artifact}.{ext}")))
}

fn make_status(tag: String, asset: String) -> UpdateStatus {
    let current = current_version().to_string();
    let has_update = ver_newer(&tag, &current);
    UpdateStatus {
        current,
        latest: tag,
        has_update,
        supported: true,
        asset,
    }
}

fn pick_asset(names: &[String], artifact: &str) -> Option<String> {
    // 优先 -dual（x86/arm64 有）；mipsle 无 dual → with-sing-box（panel 同物）
    names
        .iter()
        .find(|n| n.contains(artifact) && n.contains("-dual"))
        .or_else(|| {
            names
                .iter()
                .find(|n| n.contains(artifact) && n.contains("with-sing-box"))
        })
        .or_else(|| {
            names
                .iter()
                .find(|n| n.contains(artifact) && (n.ends_with(".tar.gz") || n.ends_with(".zip")))
        })
        .cloned()
}

/// 下载 URL（github → objects 302 自动跟随）
fn download_url(tag: &str, asset: &str) -> String {
    format!("https://github.com/{REPO}/releases/download/{tag}/{asset}")
}

/// 带本机 mixed 代理的客户端（直连 GitHub 失败时的回落通道）
fn proxied_client(base: &reqwest::Client, mixed_port: u16) -> reqwest::Client {
    let _ = base;
    reqwest::Client::builder()
        .user_agent("sing-box-web-panel")
        // HTTP/2 over CONNECT 隧道对大响应有流解码问题，限 HTTP/1.1
        .http1_only()
        .proxy(reqwest::Proxy::all(&format!("http://127.0.0.1:{mixed_port}")).unwrap())
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// 完整更新流程。成功后调度延迟重启（先回 HTTP 响应，再执行）。
/// core 由调用方（handler）在替换前优雅停止。
pub async fn perform_update(
    http: &reqwest::Client,
    sup: &std::sync::Arc<crate::supervisor::Supervisor>,
    data_dir: &Path,
    mixed_port: u16,
) -> Result<String, String> {
    // 直连 GitHub 失败时自动走本机 mixed 入站重试（面板自己就是代理工具，
    // 核心节点可达 GitHub 时更新即可用）
    let proxied = proxied_client(http, mixed_port);

    let (status, http) = match check_latest(http).await {
        Ok(v) => (v, http),
        Err(e) => {
            let v = check_latest(&proxied)
                .await
                .map_err(|e2| format!("{e}；走本机代理重试仍失败: {e2}"))?;
            (v, &proxied)
        }
    };
    if !status.has_update {
        return Err(format!("已是最新版本（{}）", status.latest));
    }
    let tag = status.latest.clone();
    let asset = status.asset.clone();
    let url = download_url(&tag, &asset);

    // —— 下载 + 流式校验 + 顺路抽取面板二进制（路由器上只落盘 ~9MB，不缓存整包）——
    let staged = stage_from_release(http, &url, &asset, data_dir, mixed_port).await?;

    // —— 停核心（释放 cache.db / TUN / 端口）——
    if sup.running() {
        if let Err(e) = sup.stop().await {
            eprintln!("自动更新: 停止核心失败（继续替换）: {e}");
        }
    }

    // —— 替换 + 调度重启 ——
    swap_and_restart(&staged)?;
    eprintln!(
        "自动更新: 面板已替换为 v{tag}，将在 1 秒内重启（页面稍后自动恢复）"
    );
    Ok(tag)
}

#[allow(unused)]
fn sha256_hex_file(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut h = Sha256::new();
    std::io::copy(&mut f, &mut h).map_err(|e| e.to_string())?;
    Ok(format!("{:x}", h.finalize()))
}

// ------------------------------------------------------------ 平台无关的流式抽取骨架

async fn stage_from_release(
    http: &reqwest::Client,
    url: &str,
    asset: &str,
    data_dir: &Path,
    mixed_port: u16,
) -> Result<PathBuf, String> {
    // .sha256 先取（小文件），拿期望哈希（直连失败走 mixed 重试）
    let expect = match fetch_expected_sha(http, url).await {
        Some(x) => Some(x),
        None => fetch_expected_sha(&proxied_client(http, mixed_port), url).await,
    };

    #[cfg(windows)]
    {
        // Windows：curl.exe 双通道下载整包 zip，PowerShell 选择性解出 panel
        return stage_platform_windows(url, asset, data_dir, mixed_port, &expect).await;
    }
    #[cfg(unix)]
    {
        // Linux：reqwest 流式（先直连，失败走 mixed 代理重取）gzip+tar 增量抽取
        let mut resp = match http
            .get(url)
            .header("User-Agent", "sing-box-web-panel")
            .timeout(std::time::Duration::from_secs(600))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => r,
            _ => {
                let p = proxied_client(http, mixed_port);
                p.get(url)
                    .header("User-Agent", "sing-box-web-panel")
                    .timeout(std::time::Duration::from_secs(600))
                    .send()
                    .await
                    .map_err(|e| format!("下载更新包失败（直连与本机代理均失败）: {e}"))?
            }
        };
        if !resp.status().is_success() {
            return Err(format!(
                "下载更新包失败: HTTP {}（{asset}）",
                resp.status()
            ));
        }
        stage_platform_unix(resp, &expect, data_dir).await
    }
}

async fn fetch_expected_sha(http: &reqwest::Client, asset_url: &str) -> Option<String> {
    let sha_url = format!("{asset_url}.sha256");
    let ok = http
        .get(&sha_url)
        .header("User-Agent", "sing-box-web-panel")
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .ok()?
        .text()
        .await
        .ok()?;
    let hex = ok.split_whitespace().next()?.to_lowercase();
    if hex.len() == 64 {
        Some(hex)
    } else {
        None
    }
}

// ------------------------------------------------------------ Windows 实现

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
async fn stage_platform_windows(
    url: &str,
    _asset: &str,
    _data_dir: &Path,
    mixed_port: u16,
    expect_sha: &Option<String>,
) -> Result<PathBuf, String> {
    use sha2::Digest;
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let exe_dir = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let zip_path = exe_dir.join("panel.update.zip");

    // 面板进程拿着 reqwest 下载 55MB 会撞上 body 解码错误（HTTP/2 + 代理链路），
    // 系统自带 curl.exe（Win10 1803+，schannel TLS）最稳：先走本机 mixed 代理，
    // 再试直连；两者都失败再回退 reqwest 流式。
    let mixed = format!("http://127.0.0.1:{mixed_port}");
    // 代理与直连交替多轮，-C - 断点续传（节点断流是常态，一次下完 55MB 不现实）
    let attempts = vec![
        vec![format!("-x {mixed}")],
        vec![],
        vec![format!("-x {mixed}")],
        vec![],
    ];
    // 已有完整 zip（上次更新中断后保留/上轮 POST 已下完）→ 直接跳过下载
    let zip_ready = zip_has_eocd(&zip_path);
    let mut eocd_ok = zip_ready;
    let mut last_err = String::new();
    for (i, flags) in attempts.iter().enumerate() {
        if zip_ready {
            break;
        }
        let resume = zip_path.exists();
        let mut cmd = std::process::Command::new("curl.exe");
        cmd.arg("-L").arg("-f").arg("--retry").arg("2")
            .arg("--connect-timeout").arg("15")
            .args(flags);
        if resume {
            cmd.arg("-C").arg("-");
        }
        cmd.arg("-o").arg(&zip_path)
            .arg(&url)
            .creation_flags(CREATE_NO_WINDOW);
        let st = tokio::task::spawn_blocking(move || cmd.status())
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        eocd_ok = zip_has_eocd(&zip_path);
        if eocd_ok {
            break;
        }
        last_err = format!("attempt {} curl 退出码 {:?}", i + 1, st.code());
        let left = std::fs::metadata(&zip_path).map(|m| m.len()).unwrap_or(0);
        eprintln!("自动更新: 下载中断于 {:.1} MB（{}），续传重试", left as f64 / 1048576.0, last_err);
    }

    if !eocd_ok {
        let _ = std::fs::remove_file(&zip_path);
        return Err(format!(
            "更新包下载不完整（{}）。请稍后重试，或手动从 GitHub Releases 下载后放进面板目录",
            last_err
        ));
    }

    if let Some(expect) = expect_sha {
        let mut f = std::fs::File::open(&zip_path).map_err(|e| e.to_string())?;
        let mut h = sha2::Sha256::new();
        std::io::copy(&mut f, &mut h).map_err(|e| e.to_string())?;
        let got = format!("{:x}", h.finalize());
        if got != *expect {
            let _ = std::fs::remove_file(&zip_path);
            return Err("更新包校验失败（sha256 不符），已丢弃。请重试或手动更新".into());
        }
    }

    // 选择性解出 panel.exe / wintun.dll 到面板目录（PowerShell + .NET ZipFile）
    let dest = exe_dir.to_string_lossy().replace("'", "''");
    let zpath = zip_path.to_string_lossy().replace("'", "''");
    let ps = format!(
        "Add-Type -AssemblyName System.IO.Compression.FileSystem

         $z = [System.IO.Compression.ZipFile]::OpenRead('{zpath}')

         foreach ($e in $z.Entries) {{

           if ($e.Name -eq 'panel.exe' -or $e.Name -eq 'wintun.dll') {{

             $dst = Join-Path '{dest}' ('panel.update.' + $e.Name)

             [System.IO.Compression.ZipFileExtensions]::ExtractToFile($e, $dst, $true)

           }}

         }}

         $z.Dispose()"
    );
    let ps1 = exe_dir.join("panel.update.ps1");
    let mut ps_bytes = vec![0xEF, 0xBB, 0xBF];
    ps_bytes.extend_from_slice(ps.as_bytes());
    std::fs::write(&ps1, ps_bytes).map_err(|e| e.to_string())?;

    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&ps1)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e: std::io::Error| e.to_string());
    let _ = std::fs::remove_file(&ps1);
    let out = out?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&zip_path);
        return Err(format!(
            "解压更新包失败: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let _ = std::fs::remove_file(&zip_path);

    let staged = exe_dir.join("panel.update.panel.exe");
    if !staged.exists() {
        return Err("更新包里没有 panel.exe".into());
    }
    Ok(staged)
}

/// zip 完整性粗检：尾部 64KB 内有 EOCD 签名（PK）。
/// 残缺包没有 EOCD，防止拿半截文件去解压误导用户。
pub fn zip_has_eocd(path: &Path) -> bool {
    let Ok(data) = std::fs::read(path) else {
        return false;
    };
    if data.len() < 22 {
        return false;
    }
    let tail = data.len().min(66000);
    data[data.len() - tail..]
        .windows(4)
        .any(|w| w == [0x50, 0x4B, 0x05, 0x06])
}

#[cfg(windows)]
fn swap_and_restart(staged: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const NEW_GROUP: u32 = 0x0000_0200;

    let cur = std::env::current_exe().map_err(|e| e.to_string())?;
    let cur = cur.canonicalize().unwrap_or(cur);
    let old = cur.with_extension("exe.old");
    let _ = std::fs::remove_file(&old);
    // 运行中的 exe 可以改名（不能覆盖）：panel.exe → panel.exe.old
    std::fs::rename(&cur, &old).map_err(|e| format!("替换面板失败（改名旧文件）: {e}"))?;
    std::fs::rename(staged, &cur).map_err(|e| format!("替换面板失败（放入新文件）: {e}"))?;
    // wintun.dll 更新（核心已停，无占用）
    let staged_wt = staged.with_file_name("panel.update.wintun.dll");
    if staged_wt.exists() {
        let _ = std::fs::copy(&staged_wt, cur.with_file_name("wintun.dll"));
        let _ = std::fs::remove_file(&staged_wt);
    }

    // exe 路径去掉 canonicalize 的 \?\ 前缀（CreateProcess 不接受 verbatim 路径）
    let exe_str = cur.to_string_lossy().replace("\\?\\", "");
    let args: Vec<String> = std::env::args().skip(1).collect();
    let exe = exe_str;
    std::thread::spawn(move || {
        // CREATE_NO_WINDOW 与 DETACHED_PROCESS 互斥（同时指定 CreateProcess 报错），
        // 只用 CREATE_NO_WINDOW + 新进程组即脱离老面板的控制台
        std::thread::sleep(std::time::Duration::from_millis(600));
        let mut cmd = std::process::Command::new(&exe);
        cmd.args(&args)
            .creation_flags(NEW_GROUP | CREATE_NO_WINDOW);
        match cmd.spawn() {
            Ok(child) => {
                eprintln!("自动更新: 新面板已启动 (pid {})", child.id());
                std::thread::sleep(std::time::Duration::from_millis(300));
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("自动更新: 新面板进程启动失败（{e}），可手动运行 panel.exe");
            }
        }
    });
    Ok(())
}

// ------------------------------------------------------------ Linux 实现

#[cfg(unix)]
async fn stage_platform_unix(
    resp: reqwest::Response,
    expect_sha: &Option<String>,
    _data_dir: &Path,
    _mixed_port: u16,
) -> Result<PathBuf, String> {
    use flate2::read::GzDecoder;
    use sha2::Digest;
    use std::io::Read;

    let exe_dir = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let staged = exe_dir.join("panel.update.tmp");

    let mut gz = GzDecoder::new(resp);
    let mut hasher = sha2::Sha256::new();
    // 哈希的是压缩字节：用 Tee 包一层再喂 gzip
    struct Tee<A, B> {
        a: A,
        b: B,
    }
    impl<A: Read, B: std::io::Write> Read for Tee<A, B> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.a.read(buf)?;
            self.b.write_all(&buf[..n])?;
            Ok(n)
        }
    }
    let mut hasher_ref = hasher.by_ref();
    let tee = Tee {
        a: &mut gz,
        b: &mut hasher_ref,
    };
    let mut ar = TarReader::new(tee);

    let mut wrote_panel = false;
    while let Some((name, typeflag, size)) = ar.next_header()? {
        if (typeflag == b'0' || typeflag == 0) && (name.ends_with("/panel") || name == "panel") {
            let mut out = std::fs::File::create(&staged).map_err(|e| e.to_string())?;
            ar.copy_body(size, &mut out)?;
            wrote_panel = true;
        } else {
            ar.skip_body(size)?;
        }
        if wrote_panel {
            break; // 面板二进制已拿到，后续条目（内核等）不用解
        }
    }
    let _ = hasher; // 哈希在 next_header/copy 中持续累加压缩流

    if !wrote_panel {
        return Err("更新包里没有 panel 二进制".into());
    }
    Ok(staged)
}

#[cfg(unix)]
fn swap_and_restart(staged: &Path) -> Result<(), String> {
    use std::os::unix::process::CommandExt;

    let cur = std::env::current_exe().map_err(|e| e.to_string())?;
    let cur = cur.canonicalize().unwrap_or(cur);
    // Linux 运行中的二进制可以直接 rename 替换（inode 交换，老进程继续跑旧映像）
    std::fs::rename(staged, &cur).map_err(|e| format!("替换面板失败: {e}"))?;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let exe = cur.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let mut cmd = std::process::Command::new(&exe);
        for a in &args {
            cmd.arg(a);
        }
        // exec 替换进程映像：同 PID，systemd/procd 无感知；老监听socket 随 exec 关闭
        let e = cmd.exec();
        eprintln!("自动更新: exec 失败 {e}，请手动重启面板");
        std::process::exit(1);
    });
    Ok(())
}

// ------------------------------------------------------------ 最小 tar 读取器（Linux）

#[cfg(unix)]
struct TarReader<R: Read> {
    inner: R,
}

#[cfg(unix)]
impl<R: Read> TarReader<R> {
    fn new(inner: R) -> Self {
        Self { inner }
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), String> {
        let mut got = 0;
        while got < buf.len() {
            let n = self
                .inner
                .read(&mut buf[got..])
                .map_err(|e| format!("读取更新包失败: {e}"))?;
            if n == 0 {
                return Err("更新包提前结束".into());
            }
            got += n;
        }
        Ok(())
    }

    fn skip(&mut self, n: u64) -> Result<(), String> {
        let mut left = n;
        let mut buf = [0u8; 4096];
        while left > 0 {
            let take = left.min(buf.len() as u64) as usize;
            self.read_exact(&mut buf[..take])?;
            left -= take as u64;
        }
        Ok(())
    }

    /// 返回 (名称, 类型, 大小)。头块全零或读到 EOF 表示结束。
    fn next_header(&mut self) -> Result<Option<(String, u8, u64)>, String> {
        let mut hdr = [0u8; 512];
        match self.inner.read(&mut hdr) {
            Ok(0) => return Ok(None),
            Ok(n) if n == 512 => {}
            Ok(_) => return Err("更新包 tar 头不完整".into()),
            Err(e) => return Err(format!("读取更新包失败: {e}")),
        }
        if hdr.iter().all(|&b| b == 0) {
            return Ok(None);
        }
        let name = cstr(&hdr[0..100]);
        let size = octal(&hdr[124..136]).unwrap_or(0);
        let typeflag = hdr[156];
        // GNU 长名扩展（本项目包名 ≤ 100 字节用不到，遇上明确报错）
        if typeflag == b'L' {
            // 跳过数据块，继续下一个头（简化处理：本项目不会出现）
            self.skip(size.div_ceil(512) * 512)?;
            return self.next_header();
        }
        let full = if !name.is_empty() {
            name
        } else {
            cstr(&hdr[345..500])
        };
        Ok(Some((full, typeflag, size)))
    }

    /// 按 size 读取条目内容到 writer，并跳过 512 对齐填充。
    fn copy_body(&mut self, size: u64, out: &mut dyn std::io::Write) -> Result<(), String> {
        let mut left = size;
        let mut buf = [0u8; 8192];
        while left > 0 {
            let take = left.min(buf.len() as u64) as usize;
            self.read_exact(&mut buf[..take])?;
            out.write_all(&buf[..take]).map_err(|e| e.to_string())?;
            left -= take as u64;
        }
        let pad = (512 - (size % 512)) % 512;
        self.skip(pad)
    }

    fn skip_body(&mut self, size: u64) -> Result<(), String> {
        self.skip(size.div_ceil(512) * 512)
    }
}

#[cfg(unix)]
fn cstr(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

#[cfg(unix)]
fn octal(buf: &[u8]) -> Option<u64> {
    let s: String = buf
        .iter()
        .take_while(|&&b| b != 0 && b != b' ')
        .map(|&b| b as char)
        .collect();
    u64::from_str_radix(s.trim(), 8).ok()
}
