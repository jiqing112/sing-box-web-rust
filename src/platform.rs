// 平台杂项：进程 RSS、TUN 权限探测、开机自启（systemd / 计划任务）。

/* ------------------------------------------------------------ RSS */

#[cfg(unix)]
pub fn proc_rss(pid: u32) -> i64 {
    if pid == 0 {
        return 0;
    }
    let Ok(b) = std::fs::read_to_string(format!("/proc/{pid}/status")) else { return 0 };
    for line in b.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            if let Some(kb) = rest.split_whitespace().next() {
                return kb.parse::<i64>().unwrap_or(0) * 1024;
            }
        }
    }
    0
}

#[cfg(windows)]
pub fn proc_rss(pid: u32) -> i64 {
    if pid == 0 {
        return 0;
    }
    // psapi GetProcessMemoryInfo 的工作集，与任务管理器一致。
    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(desiredaccess: u32, inherithandle: i32, processid: u32) -> isize;
        fn CloseHandle(handle: isize) -> i32;
    }
    #[link(name = "psapi")]
    extern "system" {
        fn GetProcessMemoryInfo(
            process: isize,
            psmemcounters: *mut ProcessMemoryCounters,
            cb: u32,
        ) -> i32;
    }
    #[repr(C)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const PROCESS_VM_READ: u32 = 0x0010;
    unsafe {
        let h = OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
            0,
            pid,
        );
        if h == 0 {
            return 0;
        }
        let mut pmc = ProcessMemoryCounters {
            cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
            page_fault_count: 0,
            peak_working_set_size: 0,
            working_set_size: 0,
            quota_peak_paged_pool_usage: 0,
            quota_paged_pool_usage: 0,
            quota_peak_non_paged_pool_usage: 0,
            quota_non_paged_pool_usage: 0,
            pagefile_usage: 0,
            peak_pagefile_usage: 0,
        };
        let r = GetProcessMemoryInfo(h, &mut pmc, pmc.cb);
        CloseHandle(h);
        if r == 0 {
            0
        } else {
            pmc.working_set_size as i64
        }
    }
}

pub fn self_rss() -> i64 {
    proc_rss(std::process::id())
}

/* ------------------------------------------------------------ TUN 权限 */

#[cfg(unix)]
pub fn tun_supported() -> bool {
    unsafe { libc::geteuid() == 0 }
}

#[cfg(windows)]
pub fn tun_supported() -> bool {
    // shell32 IsUserAnAdmin：面板是 sing-box 的父进程，子进程继承令牌。
    #[link(name = "shell32")]
    extern "system" {
        fn IsUserAnAdmin() -> i32;
    }
    unsafe { IsUserAnAdmin() != 0 }
}

pub fn tun_unsupported_reason() -> &'static str {
    #[cfg(unix)]
    {
        "面板未以 root 身份运行，无法创建 TUN 设备"
    }
    #[cfg(windows)]
    {
        "面板未以管理员身份运行，无法创建 TUN 虚拟网卡（请右键“以管理员身份运行”）"
    }
}

/* ------------------------------------------------------------ 开机自启 */

#[cfg(unix)]
mod autostart {
    use std::process::Command;

    const UNIT: &str = "sing-box-panel";
    const PROCD_SCRIPT: &str = "/etc/init.d/sing-box-panel";

    /// 面板可用的初始化系统。
    #[derive(PartialEq, Clone, Copy)]
    enum InitKind {
        Systemd,
        /// OpenWrt 的 procd（/etc/init.d + rc.common）。
        Procd,
    }

    /// 识别初始化系统：/run/systemd/system 是 systemd 的标准标志；
    /// 否则认 OpenWrt（procd）——只有它有 rc.common 体系，写 init 脚本才有意义。
    fn init_kind() -> Option<InitKind> {
        if std::path::Path::new("/run/systemd/system").exists() {
            return Some(InitKind::Systemd);
        }
        if std::path::Path::new("/etc/openwrt_release").exists()
            || std::path::Path::new("/lib/functions/procd.sh").exists()
        {
            return Some(InitKind::Procd);
        }
        None
    }

    pub fn enabled() -> Result<bool, String> {
        match init_kind() {
            Some(InitKind::Systemd) => systemd_enabled(),
            Some(InitKind::Procd) => procd_enabled(),
            None => Err("未检测到 systemd 或 OpenWrt procd，无法管理开机自启".into()),
        }
    }

    /// 只负责 enable/disable；unit/脚本已存在时不改写（保留手工维护的环境变量与参数），
    /// 缺失时按当前 exe/data 目录生成。
    pub fn set(on: bool, data_dir: &std::path::Path) -> Result<(), String> {
        match init_kind() {
            Some(InitKind::Systemd) => systemd_set(on, data_dir),
            Some(InitKind::Procd) => procd_set(on, data_dir),
            None => Err("未检测到 systemd 或 OpenWrt procd，无法管理开机自启".into()),
        }
    }

    pub fn note() -> String {
        match init_kind() {
            Some(InitKind::Systemd) => {
                "systemd 服务 sing-box-panel；开启即 systemctl enable（unit 缺失时自动生成）".into()
            }
            Some(InitKind::Procd) => {
                "OpenWrt procd 服务 sing-box-panel；开启即生成 /etc/init.d 脚本并 enable".into()
            }
            None => "未检测到 systemd 或 OpenWrt procd，开机自启不可用".into(),
        }
    }

    /* ---------- systemd（Debian / Ubuntu / 常规发行版） ---------- */

    fn systemd_enabled() -> Result<bool, String> {
        let out = Command::new("systemctl")
            .args(["is-enabled", UNIT])
            .output()
            .map_err(|e| e.to_string())?;
        let state = String::from_utf8_lossy(&out.stdout).trim().to_string();
        match state.as_str() {
            "enabled" => Ok(true),
            "disabled" | "not-found" | "static" | "" => Ok(false),
            _ => Err(format!("无法查询 systemd 状态: {state}")),
        }
    }

    fn systemd_set(on: bool, data_dir: &std::path::Path) -> Result<(), String> {
        if unsafe { libc::geteuid() } != 0 {
            return Err("修改开机自启需要面板以 root 身份运行".into());
        }
        if !on {
            let out = Command::new("systemctl")
                .args(["disable", UNIT])
                .output()
                .map_err(|e| e.to_string())?;
            let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
            if !out.status.success() {
                return Err(format!("disable 失败: {msg}"));
            }
            return Ok(());
        }
        let self_exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let self_exe = self_exe.canonicalize().unwrap_or(self_exe);
        let unit_path = format!("/etc/systemd/system/{UNIT}.service");
        if !std::path::Path::new(&unit_path).exists() {
            let wd = self_exe.parent().unwrap_or(std::path::Path::new("."));
            let unit = format!(
                "[Unit]\nDescription=sing-box panel\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nWorkingDirectory=\"{}\"\nExecStart=\"{}\" -data \"{}\"\nRestart=on-failure\nRestartSec=3\nLimitNOFILE=65535\nKillMode=control-group\n\n[Install]\nWantedBy=multi-user.target\n",
                wd.display(),
                self_exe.display(),
                data_dir.display()
            );
            std::fs::write(&unit_path, unit).map_err(|e| format!("写入 unit 失败: {e}"))?;
            let _ = Command::new("systemctl").arg("daemon-reload").output();
        }
        let out = Command::new("systemctl")
            .args(["enable", UNIT])
            .output()
            .map_err(|e| e.to_string())?;
        let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if !out.status.success() {
            return Err(format!("enable 失败: {msg}"));
        }
        Ok(())
    }

    /* ---------- procd（OpenWrt / ImmortalWrt） ---------- */

    fn procd_enabled() -> Result<bool, String> {
        if !std::path::Path::new(PROCD_SCRIPT).exists() {
            return Ok(false);
        }
        // rc.common 的 enabled 动作：rc.d 里有 S??symlink 时返回 0
        let out = Command::new(PROCD_SCRIPT)
            .arg("enabled")
            .output()
            .map_err(|e| e.to_string())?;
        Ok(out.status.success())
    }

    fn procd_set(on: bool, data_dir: &std::path::Path) -> Result<(), String> {
        if unsafe { libc::geteuid() } != 0 {
            return Err("修改开机自启需要面板以 root 身份运行".into());
        }
        if !on {
            if !std::path::Path::new(PROCD_SCRIPT).exists() {
                return Ok(());
            }
            let out = Command::new(PROCD_SCRIPT)
                .arg("disable")
                .output()
                .map_err(|e| e.to_string())?;
            let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
            if !out.status.success() {
                return Err(format!("disable 失败: {msg}"));
            }
            return Ok(());
        }
        let self_exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let self_exe = self_exe.canonicalize().unwrap_or(self_exe);
        if !std::path::Path::new(PROCD_SCRIPT).exists() {
            // procd 的 respawn：运行满 3600s 后崩溃则计数清零，5s 后重启，0=不限次数
            let script = format!(
                r#"#!/bin/sh /etc/rc.common
# sing-box panel 开机自启（由面板生成；改过路径后请在面板里重新开关一次自启）

START=99
STOP=10
USE_PROCD=1

PROG="{exe}"
DATA="{data}"

start_service() {{
    procd_open_instance
    procd_set_param command "$PROG" -data "$DATA"
    procd_set_param respawn 3600 5 0
    procd_set_param stdout 1
    procd_set_param stderr 1
    procd_close_instance
}}
"#,
                exe = self_exe.display(),
                data = data_dir.display()
            );
            std::fs::write(PROCD_SCRIPT, script)
                .map_err(|e| format!("写入 init 脚本失败: {e}"))?;
            use std::os::unix::fs::PermissionsExt;
            let _ =
                std::fs::set_permissions(PROCD_SCRIPT, std::fs::Permissions::from_mode(0o755));
        }
        let out = Command::new(PROCD_SCRIPT)
            .arg("enable")
            .output()
            .map_err(|e| e.to_string())?;
        let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if !out.status.success() {
            return Err(format!("enable 失败: {msg}"));
        }
        Ok(())
    }
}

#[cfg(windows)]
mod autostart {
    use std::process::Command;

    const TASK: &str = "sing-box-panel";
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    fn run(args: &[&str]) -> Result<String, String> {
        let out = Command::new("schtasks")
            .args(args)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|e| e.to_string())?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    use std::os::windows::process::CommandExt;

    pub fn enabled() -> Result<bool, String> {
        let out = run(&["/Query", "/TN", TASK]).map_err(|e| e.to_string())?;
        Ok(out.contains(TASK))
    }

    /// SYSTEM + 最高权限开机自启；失败 1 分钟后自动重启，无时长限制。
    pub fn set(on: bool, data_dir: &std::path::Path) -> Result<(), String> {
        if !on {
            run(&["/Delete", "/TN", TASK, "/F"]).map_err(|e| e.to_string())?;
            return Ok(());
        }
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let exe = exe.canonicalize().unwrap_or(exe);
        // 已存在时先删再建，保证参数指向当前 exe/data
        let _ = run(&["/Delete", "/TN", TASK, "/F"]);
        let create = format!(
            "/Create /TN {TASK} /TR \"\\\"{}\\\" -data \\\"{}\\\"\" /SC ONSTART /RU SYSTEM /RL HIGHEST /F",
            exe.display(),
            data_dir.display()
        );
        let parts: Vec<&str> = create.split_whitespace().collect();
        let out = run(&parts).map_err(|e| e.to_string())?;
        if !out.contains("SUCCESS") {
            return Err(format!("schtasks 失败: {}", out.trim()));
        }
        Ok(())
    }

    pub fn note() -> &'static str {
        "Windows 计划任务 sing-box-panel（SYSTEM，开机启动，最高权限）"
    }
}

pub use autostart::{enabled as autostart_enabled, note as autostart_note, set as set_autostart};
