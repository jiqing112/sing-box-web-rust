// panel.conf：exe 旁边的端口/账户/密码配置文件。
// UsernameSet/PasswordSet 用于区分「没写这一项」和「写了但留空」：
// 后者同时出现在两项上时表示明确关闭登录。

use std::fs;
use std::path::{Path, PathBuf};

#[derive(Default, Clone, Debug)]
pub struct PanelConf {
    pub listen: String,
    pub username: String,
    pub password: String,
    pub username_set: bool,
    pub password_set: bool,
}

impl PanelConf {
    /// 仅当两项都显式留空时成立。
    pub fn login_disabled(&self) -> bool {
        self.username_set && self.password_set && self.username.is_empty() && self.password.is_empty()
    }
}

/// 固定放在 exe 旁边，与内核的发现逻辑一致，整个目录拷走即完成迁移。
pub fn panel_conf_path() -> PathBuf {
    match std::env::current_exe() {
        Ok(exe) => exe.parent().unwrap_or(Path::new(".")).join("panel.conf"),
        Err(_) => PathBuf::from("panel.conf"),
    }
}

/// 读取 panel.conf；文件不存在时先写入一份带注释的模板再返回空值。
pub fn load_panel_conf() -> PanelConf {
    let path = panel_conf_path();
    let Ok(text) = fs::read_to_string(&path) else {
        write_panel_conf_template(&path);
        return PanelConf::default();
    };
    let mut c = PanelConf::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let k = k.trim().to_lowercase();
        let v = v.trim();
        match k.as_str() {
            "listen" => c.listen = v.to_string(),
            "username" => {
                c.username = v.to_string();
                c.username_set = true;
            }
            "password" => {
                c.password = v.to_string();
                c.password_set = true;
            }
            _ => {}
        }
    }
    c
}

/// 把面板内修改的监听地址写回 panel.conf，保留注释与其他配置。
pub fn write_panel_conf_listen(addr: &str) -> Result<(), String> {
    update_panel_conf_listen(&panel_conf_path(), addr)
}

fn update_panel_conf_listen(path: &Path, addr: &str) -> Result<(), String> {
    let raw = fs::read(path).map_err(|e| e.to_string())?;
    let mut text = String::from_utf8_lossy(&raw).into_owned();
    let eol = if text.contains("\r\n") {
        text = text.replace("\r\n", "\n");
        "\r\n"
    } else {
        "\n"
    };
    let mut lines: Vec<String> = text.split('\n').map(|s| s.to_string()).collect();
    let mut found = false;
    for line in lines.iter_mut() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        if let Some((k, _)) = trimmed.split_once('=') {
            if k.trim().eq_ignore_ascii_case("listen") {
                *line = format!("listen={addr}");
                found = true;
                break;
            }
        }
    }
    if !found {
        let n = lines.len();
        if n > 0 && lines[n - 1].is_empty() {
            lines[n - 1] = format!("listen={addr}");
            lines.push(String::new());
        } else {
            lines.push(format!("listen={addr}"));
        }
    }
    let mut content = lines.join(eol);
    if !content.ends_with(eol) {
        content += eol;
    }
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp = dir.join(format!(".panel.conf.{}", crate::util::rand_hex(6)));
    fs::write(&tmp, content).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
    }
    fs::rename(&tmp, path).map_err(|e| e.to_string())
}

fn write_panel_conf_template(path: &Path) {
    let content = r#"# sing-box 面板配置（修改后重启面板生效；命令行参数优先于本文件）

# 面板监听地址，例如 127.0.0.1:9090（仅本机访问）或 0.0.0.0:9090（局域网可访问）
listen=127.0.0.1:9090

# 登录用户名/密码；两项都留空表示关闭登录，打开端口直接进入首页
# 只清空 password 会保持现有密码不变；修改用户名时必须同时填写 password
username=admin

# 此文件为明文，请注意保管
password=

# 内核（sing-box / mihomo）不在本文件里配：
# 面板「设置 → 内核」切换后写进数据目录的 settings.json，重启保持；
# 启动时强制指定用命令行参数 -engine mihomo。
"#;
    // Go 用 O_EXCL：已存在时不覆盖。先检查再写，避免竞态覆盖。
    if path.exists() {
        return;
    }
    if fs::write(path, content).is_ok() {
        eprintln!(
            "已生成配置模板 {}（可直接编辑监听地址/账户/密码）",
            path.display()
        );
    }
}
