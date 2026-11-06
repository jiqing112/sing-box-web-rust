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

    /// 直接传参数数组给 schtasks，不做任何字符串拼接再拆分的处理：
    /// 安装路径带空格时（`C:\Program Files\...`）拼接拆分会把 /TR 拆坏。
    fn run(args: &[String]) -> Result<std::process::Output, String> {
        Command::new("schtasks")
            .args(args)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|e| e.to_string())
    }

    fn s(v: &str) -> String {
        v.to_string()
    }

    use std::os::windows::process::CommandExt;

    pub fn enabled() -> Result<bool, String> {
        let out = run(&[s("/Query"), s("/TN"), s(TASK)])?;
        // 用退出码判断：中文系统输出的是「成功」而不是 SUCCESS，且任务不存在时退出码非 0。
        Ok(out.status.success())
    }

    /// `/Create` 的完整参数表。/TR 的整段命令**作为一个参数**传下去，
    /// 内部引号由 schtasks 自己解析；安装路径带空格也不会被拆散。
    fn create_args(exe: &std::path::Path, data_dir: &std::path::Path) -> Vec<String> {
        let tr = format!("\"{}\" -data \"{}\"", exe.display(), data_dir.display());
        vec![
            s("/Create"),
            s("/TN"),
            s(TASK),
            s("/TR"),
            tr,
            s("/SC"),
            s("ONSTART"),
            s("/RU"),
            s("SYSTEM"),
            s("/RL"),
            s("HIGHEST"),
            s("/F"),
        ]
    }

    fn out_msg(out: &std::process::Output) -> String {
        let msg = String::from_utf8_lossy(&out.stderr);
        let msg = msg.trim();
        if msg.is_empty() {
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        } else {
            msg.to_string()
        }
    }

    /// SYSTEM + 最高权限开机自启；失败 1 分钟后自动重启，无时长限制。
    ///
    /// 权限策略：先直跑（管理员/SYSTEM 面板零改动、不弹窗）；直跑失败
    /// （普通用户必然「拒绝访问」）时自动弹 UAC，由提权进程完成任务并把
    /// 结果写入临时文件闭环——普通用户连 /Query 本任务的权限都没有，
    /// 成败只能在提权进程内部判定。任务取消/超时返回明确中文报错。
    /// 安全约束：提权只服务本固定用途（固定任务名 + 本 exe/数据目录），
    /// 脚本由面板生成、参数写死，不接受任何外部传入命令。
    pub fn set(on: bool, data_dir: &std::path::Path) -> Result<(), String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let exe = exe.canonicalize().unwrap_or(exe);
        // 1) 直跑。ON 时先删再建，保证任务参数指向当前 exe/data。
        let direct: Result<(), String> = if !on {
            let out = run(&[s("/Delete"), s("/TN"), s(TASK), s("/F")])?;
            if out.status.success() {
                Ok(())
            } else {
                let msg = out_msg(&out);
                // 任务本就不存在 = 目标已达成（中英文系统文案都含这两类字样）
                if msg.contains("找不到") || msg.to_lowercase().contains("cannot find") {
                    Ok(())
                } else {
                    Err(msg)
                }
            }
        } else {
            let _ = run(&[s("/Delete"), s("/TN"), s(TASK), s("/F")]);
            let out = run(&create_args(&exe, data_dir))?;
            if out.status.success() {
                Ok(())
            } else {
                Err(out_msg(&out))
            }
        };
        if direct.is_ok() {
            return Ok(());
        }
        // 2) 权限不足 → UAC 提权闭环。
        eprintln!(
            "面板开机自启: 直接执行失败（{}），尝试 UAC 提权——请在弹出的授权窗口中点「是」",
            direct.unwrap_err()
        );
        elevate_set(on, &exe, data_dir)
    }

    /// 生成计划任务 XML（对齐 Go 版任务形状：BootTrigger + SYSTEM +
    /// 最高权限 + 失败 1 分钟后重启 3 次）。UTF-16LE BOM 写盘，中文路径无忧。
    fn build_task_xml(exe: &std::path::Path, data_dir: &std::path::Path) -> String {
        let esc = |x: &str| {
            x.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;")
        };
        let args = format!("-data \"{}\"", esc(&data_dir.to_string_lossy()));
        format!(
            r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers><BootTrigger><Enabled>true</Enabled></BootTrigger></Triggers>
  <Principals><Principal id="Author"><UserId>S-1-5-18</UserId></Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
    <RestartOnFailure><Interval>PT1M</Interval><Count>3</Count></RestartOnFailure>
  </Settings>
  <Actions Context="Author"><Exec><Command>{}</Command><Arguments>{}</Arguments></Exec></Actions>
</Task>"#,
            esc(&exe.to_string_lossy()),
            args
        )
    }

    fn utf16le_b64(s: &str) -> String {
        use base64::Engine;
        let codes: Vec<u8> = s
            .encode_utf16()
            .flat_map(|c| c.to_le_bytes())
            .collect();
        base64::engine::general_purpose::STANDARD.encode(codes)
    }

    /// 提权要执行的 PowerShell 脚本：schtasks + 内部 /Query 验证 + 结果文件。
    /// 普通用户无权 /Query SYSTEM 任务，成败判定必须在提权进程内完成。
    fn build_elevated_ps(on: bool, xml_path: &std::path::Path, result: &std::path::Path) -> String {
        let q = |x: &std::path::Path| x.to_string_lossy().replace('\'', "''");
        let (xml, result) = (q(xml_path), q(result));
        if on {
            format!(
                "schtasks /Create /TN {TASK} /XML '{xml}' /F\r\n\
                 if ($LASTEXITCODE -ne 0) {{ Set-Content -LiteralPath '{result}' -Value 'FAIL_CREATE' -Encoding ascii; exit }}\r\n\
                 schtasks /Query /TN {TASK} > $null\r\n\
                 if ($LASTEXITCODE -eq 0) {{ Set-Content -LiteralPath '{result}' -Value 'OK' -Encoding ascii }} else {{ Set-Content -LiteralPath '{result}' -Value 'FAIL_QUERY' -Encoding ascii }}"
            )
        } else {
            format!(
                "schtasks /Delete /TN {TASK} /F\r\n\
                 if ($LASTEXITCODE -eq 0) {{ Set-Content -LiteralPath '{result}' -Value 'OK' -Encoding ascii; exit }}\r\n\
                 schtasks /Query /TN {TASK} > $null 2>&1\r\n\
                 if ($LASTEXITCODE -ne 0) {{ Set-Content -LiteralPath '{result}' -Value 'OK' -Encoding ascii }} else {{ Set-Content -LiteralPath '{result}' -Value 'FAIL' -Encoding ascii }}"
            )
        }
    }

    /// UAC 提权执行：弹窗（本地交互桌面）→ 等待 → 读结果文件闭环。
    /// 用户点「否」或 UAC 约 2 分钟超时：Start-Process 抛错、外层退出非零、
    /// 结果文件缺失，统一归为授权失败。
    fn elevate_set(on: bool, exe: &std::path::Path, data_dir: &std::path::Path) -> Result<(), String> {
        let dir = std::env::temp_dir();
        let xml_path = dir.join(format!("sb-panel-task-{}.xml", crate::util::rand_hex(6)));
        let result = dir.join(format!("sb-panel-task-{}.txt", crate::util::rand_hex(6)));
        let _ = std::fs::remove_file(&result);

        let write_result = if on {
            std::fs::write(&xml_path, utf16le_bytes(&build_task_xml(exe, data_dir)))
        } else {
            Ok(()) // OFF 无需 XML
        };
        if let Err(e) = write_result {
            return Err(format!("写入计划任务 XML 失败: {e}"));
        }

        let ps = build_elevated_ps(on, &xml_path, &result);
        let encoded = utf16le_b64(&ps);
        let out = Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!(
                    "Start-Process powershell -Verb RunAs -Wait -WindowStyle Hidden -ArgumentList '-NoProfile','-EncodedCommand','{encoded}'"
                ),
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output();

        let result_state = std::fs::read_to_string(&result).unwrap_or_default();
        let _ = std::fs::remove_file(&result);
        let _ = std::fs::remove_file(&xml_path);

        if result_state.trim() == "OK" {
            eprintln!("面板开机自启: UAC 提权执行成功");
            return Ok(());
        }
        let detail = match out {
            Ok(o) if !o.status.success() => format!(
                "（提权进程异常退出: {}）",
                out_msg(&o)
            ),
            Ok(_) => format!("（提权脚本报告: {}）", result_state.trim()),
            Err(e) => format!("（{e}）"),
        };
        Err(format!(
            "需要管理员权限：UAC 未获授权或操作失败{detail}。可在本机重新点击并在弹窗中确认，或运行 panel.bat autostart-on|off"
        ))
    }

    fn utf16le_bytes(s: &str) -> Vec<u8> {
        let mut out = vec![0xFF, 0xFE]; // BOM
        for c in s.encode_utf16() {
            out.extend_from_slice(&c.to_le_bytes());
        }
        out
    }

    pub fn note() -> &'static str {
        "Windows 计划任务 sing-box-panel（SYSTEM，开机启动，最高权限）"
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// 回归：以前是「拼成一整串再 split_whitespace」，路径带空格时 /TR 会被拆成多个参数。
        #[test]
        fn create_args_keeps_path_with_spaces_whole() {
            let args = create_args(
                std::path::Path::new(r"C:\Program Files\panel\panel.exe"),
                std::path::Path::new(r"C:\Program Files\panel\data"),
            );
            let i = args.iter().position(|a| a == "/TR").expect("/TR");
            assert_eq!(
                args[i + 1],
                r#""C:\Program Files\panel\panel.exe" -data "C:\Program Files\panel\data""#
            );
            assert_eq!(args.len(), 12);
            assert_eq!(args[0], "/Create");
            assert_eq!(args.last().unwrap(), "/F");
        }

        /// 提权 XML：SYSTEM SID、命令与参数就位、XML 特殊字符已转义。
        #[test]
        fn task_xml_contains_command_and_escapes() {
            let xml = build_task_xml(
                std::path::Path::new(r"C:\Program Files\panel\panel.exe"),
                std::path::Path::new(r"C:\Program Files\panel\data"),
            );
            assert!(xml.contains("<UserId>S-1-5-18</UserId>"));
            // SYSTEM 主体不得带 RunLevel（XML schema 拒绝，实测 (4,83):RunLevel:Highest 报错）
            assert!(!xml.contains("RunLevel"));
            assert!(xml.contains(r"<Command>C:\Program Files\panel\panel.exe</Command>"));
            assert!(xml.contains(r#"<Arguments>-data "C:\Program Files\panel\data"</Arguments>"#));
            assert!(xml.contains("<BootTrigger>"));
            assert!(xml.contains("RestartOnFailure"));
            // & 转义（路径含 & 时不产生非法 XML）
            let xml2 = build_task_xml(
                std::path::Path::new(r"C:\a&b\panel.exe"),
                std::path::Path::new(r"C:\d"),
            );
            assert!(xml2.contains("C:\\a&amp;b\\panel.exe"));
        }

        /// 提权脚本：ON 用 /XML + 内部 /Query 验证 + 结果文件；OFF 用 /Delete。
        #[test]
        fn elevated_ps_covers_both_directions() {
            let xml = std::path::Path::new(r"C:\Temp\x.xml");
            let result = std::path::Path::new(r"C:\Temp\r.txt");
            let on = build_elevated_ps(true, xml, result);
            assert!(on.contains("schtasks /Create /TN sing-box-panel /XML 'C:\\Temp\\x.xml' /F"));
            assert!(on.contains("schtasks /Query /TN sing-box-panel"));
            assert!(on.contains("'C:\\Temp\\r.txt'"));
            assert!(on.contains("'OK'") && on.contains("'FAIL_CREATE'"));
            let off = build_elevated_ps(false, xml, result);
            assert!(off.contains("schtasks /Delete /TN sing-box-panel /F"));
            assert!(!off.contains("/XML"));
        }

        /// EncodedCommand 的 base64 必须是 UTF-16LE（PowerShell 约定，无 BOM），可无损往返。
        #[test]
        fn utf16le_b64_roundtrip_and_unicode() {
            use base64::Engine;
            let src = "schtasks /Create 中文路径 test";
            let b64 = utf16le_b64(src);
            let raw = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .expect("valid base64");
            let u16s: Vec<u16> = raw
                .chunks(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            assert_eq!(String::from_utf16(&u16s).unwrap(), src);
            // 对比：XML 文件用的 utf16le_bytes 才带 BOM
            assert_eq!(&utf16le_bytes(src)[..2], &[0xFF, 0xFE]);
        }
    }
}

pub use autostart::{enabled as autostart_enabled, note as autostart_note, set as set_autostart};
