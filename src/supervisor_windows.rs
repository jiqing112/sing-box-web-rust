// Windows 平台的进程组、进程树终止与孤儿清扫实现。

use std::path::Path;
use tokio::process::Command;

const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 子进程自成进程组且隐藏控制台窗口，避免每次启动核心都闪黑框。
pub fn setup_process_group(cmd: &mut Command) {
    // tokio 的 Command::creation_flags 是固有方法，不需要 CommandExt
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

/// canonicalize 在 Windows 上返回的是 `\\?\C:\...`（UNC 还多一层 `\\?\UNC\`），
/// 而进程的 ExecutablePath / CommandLine 是普通 `C:\...` 写法。
/// 不剥掉前缀，两个字符串永远不相等 —— 孤儿清扫会静默失效。
fn plain_path(p: &Path) -> String {
    let s = p.to_string_lossy().into_owned();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    match s.strip_prefix(r"\\?\") {
        Some(rest) => rest.to_string(),
        None => s,
    }
}

/// 清理上次面板强退后残留的内核。孤儿会一直占着 cache.db 和 9091。
/// 匹配条件：可执行文件完整路径一致，且命令行里引用了本面板的数据目录。
///
/// 两个内核合并成一次 PowerShell 查询：冷启动时 powershell 进程本身要 8–10 秒，
/// 每个内核各查一次会让面板 20 秒后才监听端口。
pub fn sweep_orphans(bins: &[String], data_dir: &Path) {
    // 只保留能解析到真实路径的 bin，并按 exe 全路径去重。
    let mut targets: Vec<(String, String)> = Vec::new(); // (exe 全路径, 文件名)
    for bin in bins {
        if bin.is_empty() {
            continue;
        }
        let Ok(exe) = std::fs::canonicalize(bin) else { continue };
        let path = plain_path(&exe);
        let name = exe
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        if targets.iter().any(|(p, _)| p.eq_ignore_ascii_case(&path)) {
            continue;
        }
        targets.push((path, name));
    }
    if targets.is_empty() {
        return;
    }
    let names: Vec<String> = {
        let mut v: Vec<String> = Vec::new();
        for (_, n) in &targets {
            if !v.iter().any(|x| x.eq_ignore_ascii_case(n)) {
                v.push(n.clone());
            }
        }
        v
    };

    let (procs, enum_err) = list_procs(&names);
    if let Some(e) = enum_err {
        // 静默失败最坑：用户以为面板重启过核心，其实还是上次那个进程在跑。
        eprintln!("[panel] 无法枚举残留进程，跳过孤儿清扫（{e}）");
    }
    for p in procs {
        if !targets.iter().any(|(path, _)| p.path.eq_ignore_ascii_case(path)) {
            continue;
        }
        // 读不到命令行就无法判断它是不是本数据目录的实例：宁可漏杀，
        // 也不能把用户用另一个 data 目录跑起来的同款内核误杀。
        if p.cmd.is_empty() {
            eprintln!(
                "[panel] 跳过 PID {}：读不到命令行，无法确认是否属于本数据目录",
                p.pid
            );
            continue;
        }
        if !data_dir.as_os_str().is_empty() && !cmd_touches(&p.cmd, data_dir) {
            continue;
        }
        let _ = kill_process_tree(p.pid, true);
        eprintln!("[panel] 清理残留核心进程 PID {}", p.pid);
    }
}

/// Windows 路径大小写不敏感，且同一条命令行里可能混用 / 和 \，两种写法都查。
fn cmd_touches(cmd: &str, data_dir: &Path) -> bool {
    let Ok(abs) = std::fs::canonicalize(data_dir) else { return true };
    let abs = plain_path(&abs);
    let low = cmd.to_lowercase();
    for d in [abs.clone(), abs.replace('\\', "/")] {
        if low.contains(&d.to_lowercase()) {
            return true;
        }
    }
    false
}

/// 枚举指定名字的进程（一次查询多个名字）。新版 Windows（11 24H2 起）已移除 wmic，
/// 所以先走 PowerShell CIM，失败再退回 wmic（老系统）。
///
/// 注意：PowerShell 正常执行但没匹配到进程时**不能**再退回 wmic——
/// 那会白白多起一个进程，把启动延迟翻倍。
/// 返回 (进程列表, 枚举失败原因)：两条路径都失败时列表为空、原因为错误文本，
/// 调用方据此打日志，避免「清扫没生效」这件事完全无声。
fn list_procs(names: &[String]) -> (Vec<OrphanProc>, Option<String>) {
    use std::os::windows::process::CommandExt;
    if names.is_empty() {
        return (Vec::new(), None);
    }
    let filter = names
        .iter()
        .map(|n| format!("Name='{n}'"))
        .collect::<Vec<_>>()
        .join(" OR ");
    let ps_script = format!(
        "Get-CimInstance Win32_Process -Filter \"{}\" | ForEach-Object {{ '{{0}}|{{1}}|{{2}}' -f $_.ProcessId, $_.ExecutablePath, $_.CommandLine }}",
        filter
    );
    let ps_err: String;
    match std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &ps_script])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(out) if out.status.success() => {
            // 成功即采信：空结果代表「没有残留进程」，不触发 wmic 兜底。
            return (parse_power_shell(&String::from_utf8_lossy(&out.stdout)), None);
        }
        Ok(out) => ps_err = format!("powershell 退出码 {}", out.status.code().unwrap_or(-1)),
        Err(e) => ps_err = format!("powershell 启动失败: {e}"),
    }
    let where_clause = names
        .iter()
        .map(|n| format!("name='{n}'"))
        .collect::<Vec<_>>()
        .join(" or ");
    match std::process::Command::new("wmic")
        .args([
            "process",
            "where",
            &where_clause,
            "get",
            "processid,executablepath",
            "/FORMAT:CSV",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(out) if out.status.success() => {
            (parse_wmic(&String::from_utf8_lossy(&out.stdout)), None)
        }
        Ok(out) => (
            Vec::new(),
            Some(format!("{ps_err}; wmic 退出码 {}", out.status.code().unwrap_or(-1))),
        ),
        Err(e) => (Vec::new(), Some(format!("{ps_err}; wmic 启动失败: {e}"))),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// canonicalize 的 `\\?\` 前缀必须剥掉，否则和进程列表里的普通路径永远比不相等。
    #[test]
    fn plain_path_strips_verbatim_prefix() {
        assert_eq!(plain_path(Path::new(r"\\?\C:\tools\sing-box.exe")), r"C:\tools\sing-box.exe");
        assert_eq!(
            plain_path(Path::new(r"\\?\UNC\server\share\sing-box.exe")),
            r"\\server\share\sing-box.exe"
        );
        // 普通路径原样返回
        assert_eq!(plain_path(Path::new(r"C:\tools\sing-box.exe")), r"C:\tools\sing-box.exe");
    }

    /// 孤儿判定只认「命令行里出现本面板的数据目录」：
    /// 别的面板实例、别的 data 目录一律不能命中，否则会互相残杀。
    #[test]
    fn cmd_touches_only_own_data_dir() {
        let own = std::env::temp_dir().join("panel-sweep-own");
        let other = std::env::temp_dir().join("panel-sweep-other");
        std::fs::create_dir_all(&own).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        // 面板传给内核的是规范化后的数据目录，命令行里就是这个写法
        // （temp_dir() 可能是 ADMINI~1 短名，canonicalize 后才是长名）。
        let abs = plain_path(&std::fs::canonicalize(&own).unwrap());

        // 内核真实命令行形态：run -c <data>\config.json
        let cmd = format!("\"C:\\tools\\sing-box.exe\" run -c \"{}\\config.json\"", abs);
        assert!(cmd_touches(&cmd, &own));
        // 命令行里可能混用 / 与 \
        assert!(cmd_touches(&cmd.replace('\\', "/"), &own));
        // 另一个数据目录的实例必须放行
        assert!(!cmd_touches(&cmd, &other));
    }
}
