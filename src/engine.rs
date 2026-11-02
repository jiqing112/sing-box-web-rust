// 内核抽象层：sing-box 与 mihomo 的差异全部收敛在这一个文件里，
// 上层（store / supervisor / config / api）只问「当前引擎是谁」。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

pub const SING_BOX: &str = "sing-box";
pub const MIHOMO: &str = "mihomo";

/// 前端下拉框顺序即此顺序。
pub fn supported() -> &'static [&'static str] {
    &[SING_BOX, MIHOMO]
}

/// 空字符串视为 sing-box：老 settings.json 没有 engine 字段。
pub fn normalize(s: &str) -> String {
    match s.trim().to_lowercase().as_str() {
        "mihomo" | "clash" | "clash-meta" | "clashmeta" | "meta" => MIHOMO.into(),
        _ => SING_BOX.into(),
    }
}

pub fn valid(s: &str) -> bool {
    matches!(
        s.trim().to_lowercase().as_str(),
        "sing-box" | "mihomo" | "clash" | "clash-meta" | "clashmeta" | "meta"
    )
}

pub fn is_mihomo(s: &str) -> bool {
    normalize(s) == MIHOMO
}

pub fn label(s: &str) -> &'static str {
    if is_mihomo(s) {
        "mihomo"
    } else {
        "sing-box"
    }
}

pub fn config_file(s: &str) -> &'static str {
    if is_mihomo(s) {
        "config.yaml"
    } else {
        "config.json"
    }
}

pub fn template_file(s: &str) -> &'static str {
    if is_mihomo(s) {
        "template.yaml"
    } else {
        "template.json"
    }
}

pub fn ext(s: &str) -> &'static str {
    if is_mihomo(s) {
        ".yaml"
    } else {
        ".json"
    }
}

/// 内核的工作目录。mihomo 单独用 data/mihomo 子目录：
/// 两个内核的 cache.db 都是 bbolt 文件，共用会互相锁死。
pub fn home(data_dir: &Path, s: &str) -> PathBuf {
    if is_mihomo(s) {
        data_dir.join("mihomo")
    } else {
        data_dir.to_path_buf()
    }
}

/// sing-box: run -c <conf>；mihomo: -d <home> -f <conf>
pub fn run_args(s: &str, conf: &Path, home_dir: &Path) -> Vec<String> {
    if is_mihomo(s) {
        vec![
            "-d".into(),
            home_dir.to_string_lossy().into_owned(),
            "-f".into(),
            conf.to_string_lossy().into_owned(),
        ]
    } else {
        vec![
            "run".into(),
            "-c".into(),
            conf.to_string_lossy().into_owned(),
        ]
    }
}

/// sing-box 用 check -c，mihomo 用 -t（test）。
pub fn check_args(s: &str, conf: &Path, home_dir: &Path) -> Vec<String> {
    if is_mihomo(s) {
        vec![
            "-t".into(),
            "-d".into(),
            home_dir.to_string_lossy().into_owned(),
            "-f".into(),
            conf.to_string_lossy().into_owned(),
        ]
    } else {
        vec![
            "check".into(),
            "-c".into(),
            conf.to_string_lossy().into_owned(),
        ]
    }
}

pub fn version_args(s: &str) -> &'static [&'static str] {
    if is_mihomo(s) {
        &["-v"]
    } else {
        &["version"]
    }
}

/// Probe 执行一次版本查询，确认二进制存在且能跑起来。
/// 返回版本号（原始输出首行），失败时返回 Err(输出或错误)。
pub async fn probe(bin: &str, eng: &str) -> Result<String, String> {
    let mut cmd = Command::new(bin);
    cmd.args(version_args(eng)).stdin(Stdio::null());
    let fut = cmd.output();
    let out = match tokio::time::timeout(Duration::from_secs(5), fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(e.to_string()),
        Err(_) => return Err("context deadline exceeded".into()),
    };
    let mut text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    text = text.trim().to_string();
    if !out.status.success() {
        if text.is_empty() {
            return Err(format!("exit status {}", out.status.code().unwrap_or(-1)));
        }
        return Err(text);
    }
    if let Some(i) = text.find('\n') {
        if i > 0 {
            text.truncate(i);
        }
    }
    Ok(text)
}

/// 把内核的完整版本串压成短标识。
/// sing-box: "sing-box version 1.14.3" → "1.14.3"
/// mihomo:   "Mihomo Meta v1.19.33 ..." → "v1.19.33"
pub fn short_version(s: &str, ver: &str) -> String {
    let ver = ver.trim();
    if ver.is_empty() {
        return String::new();
    }
    let fields: Vec<&str> = ver.split_whitespace().collect();
    if !is_mihomo(s) && fields.len() >= 3 && fields[0] == "sing-box" && fields[1] == "version" {
        return fields[2].to_string();
    }
    if is_mihomo(s) {
        for f in &fields {
            let b = f.as_bytes();
            if b.len() > 1 && (b[0] == b'v' || b[0] == b'V') && b[1].is_ascii_digit() {
                return (*f).to_string();
            }
        }
    }
    for f in &fields {
        if looks_like_version(f) {
            return (*f).to_string();
        }
    }
    ver.to_string()
}

fn looks_like_version(f: &str) -> bool {
    let f = f.trim_start_matches(['v', 'V']);
    let parts: Vec<&str> = f.splitn(3, '.').collect();
    if parts.len() < 2 {
        return false;
    }
    parts.iter().all(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit()))
}
