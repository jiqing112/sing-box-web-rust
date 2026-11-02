// Windows 平台的进程组、进程树终止与孤儿清扫实现。

use std::path::{Path, PathBuf};
use tokio::process::Command;

const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 子进程自成进程组且隐藏控制台窗口，避免每次启动核心都闪黑框。
pub fn setup_process_group(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

/// taskkill /T /F 终止整棵进程树。
pub fn kill_process_tree(pid: i32, _force: bool) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map(|_| ())
}

pub fn process_alive(pid: i32) -> bool {
    use std::os::windows::process::CommandExt;
    match std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout);
            text.contains(&format!("\"{pid}\""))
        }
        Err(_) => false,
    }
}

struct OrphanProc {
    path: String,
    pid: i32,
    /// 空表示这条记录没有命令行信息（wmic 回退）
    cmd: String,
}

/// 清理上次面板强退后残留的内核。孤儿会一直占着 cache.db 和 9091。
/// 匹配条件：可执行文件完整路径一致，且命令行里引用了本面板的数据目录。
/// 读不到命令行时按「是孤儿」处理：宁可多杀一个也不能漏。
pub fn sweep_orphans(bin: &str, data_dir: &Path) {
    let Ok(self_exe) = std::fs::canonicalize(bin) else { return };
    let self_path = self_exe.to_string_lossy().into_owned();
    let name = self_exe
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if name.is_empty() {
        return;
    }
    for p in list_orphans(&name) {
        if !p.path.eq_ignore_ascii_case(&self_path) {
            continue;
        }
        if !p.cmd.is_empty() && !data_dir.as_os_str().is_empty() && !cmd_touches(&p.cmd, data_dir) {
            continue;
        }
        let _ = kill_process_tree(p.pid, true);
        eprintln!("[panel] 清理残留核心进程 PID {}", p.pid);
    }
}

/// Windows 路径大小写不敏感，且同一条命令行里可能混用 / 和 \，两种写法都查。
fn cmd_touches(cmd: &str, data_dir: &Path) -> bool {
    let Ok(abs) = std::fs::canonicalize(data_dir) else { return true };
    let abs: PathBuf = abs;
    let low = cmd.to_lowercase();
    for d in [abs.to_string_lossy().into_owned(), abs.to_string_lossy().replace('\\', "/")] {
        if low.contains(&d.to_lowercase()) {
            return true;
        }
    }
    false
}

/// 枚举指定名字的进程。新版 Windows（11 24H2 起）已移除 wmic，
/// 所以先走 PowerShell CIM，失败再退回 wmic（老系统）。
fn list_orphans(name: &str) -> Vec<OrphanProc> {
    use std::os::windows::process::CommandExt;
    let ps_script = format!(
        "Get-CimInstance Win32_Process -Filter \"Name='{}'\" | ForEach-Object {{ '{{0}}|{{1}}|{{2}}' -f $_.ProcessId, $_.ExecutablePath, $_.CommandLine }}",
        name
    );
    if let Ok(out) = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &ps_script])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        if !out.stdout.is_empty() {
            let procs = parse_power_shell(&String::from_utf8_lossy(&out.stdout));
            if !procs.is_empty() {
                return procs;
            }
        }
    }
    if let Ok(out) = std::process::Command::new("wmic")
        .args([
            "process",
            "where",
            &format!("name='{name}'"),
            "get",
            "processid,executablepath",
            "/FORMAT:CSV",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        return parse_wmic(&String::from_utf8_lossy(&out.stdout));
    }
    Vec::new()
}

/// 解析 "PID|ExecutablePath|CommandLine" 行。SplitN(3)：命令行本身可能含 '|'。
fn parse_power_shell(out: &str) -> Vec<OrphanProc> {
    let mut procs = Vec::new();
    for line in out.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.splitn(3, '|').collect();
        if parts.len() < 2 {
            continue;
        }
        let Ok(pid) = parts[0].trim().parse::<i32>() else { continue };
        if pid <= 0 {
            continue;
        }
        let path = parts[1].trim().to_string();
        if path.is_empty() {
            continue;
        }
        let cmd = if parts.len() == 3 { parts[2].to_string() } else { String::new() };
        procs.push(OrphanProc { path, pid, cmd });
    }
    procs
}

/// 解析 /FORMAT:CSV 输出。路径含逗号时简单 split 会列错位，
/// 这里按「最后一段是 PID、倒数第二段是路径」处理，等价于 Go 的 csv 读取结果。
fn parse_wmic(out: &str) -> Vec<OrphanProc> {
    let mut procs = Vec::new();
    for line in out.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(',').collect();
        if fields.len() < 3 {
            continue;
        }
        let Ok(pid) = fields[fields.len() - 1].trim().parse::<i32>() else { continue };
        if pid <= 0 {
            continue;
        }
        let path = fields[fields.len() - 2].trim().to_string();
        if path.is_empty() {
            continue;
        }
        procs.push(OrphanProc { path, pid, cmd: String::new() });
    }
    procs
}
