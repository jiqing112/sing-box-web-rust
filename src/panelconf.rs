// panel.conf：exe 旁边的端口/账户/密码配置文件。
// password 非空 = 直接应用该凭据（自动化部署路径）；留空 = 全新面板进入
// 网页初始化流程（首次启动设置账户密码），已有账户则保持现有密码。

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

# 登录用户名/密码。
# - 首次启动打开面板网页时会要求设置登录账户密码（用户名默认用这里的 username）；
# - password 填上则跳过网页初始化，直接以该账户密码生效（适合自动化部署）；
# - 面板已经初始化过时，只填 username 不填 password 不会改动现有密码；
# - 关闭登录请用面板内「设置 → 允许免密码访问」开关，本文件不再承担。
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
