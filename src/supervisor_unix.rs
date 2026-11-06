// Unix 平台的进程组与孤儿清扫实现。

use std::path::Path;
use tokio::process::Command;

pub fn setup_process_group(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// 向整个进程组发信号；force=false 发 SIGTERM，true 发 SIGKILL。
pub fn kill_process_tree(pid: i32, force: bool) -> std::io::Result<()> {
    let sig = if force { libc::SIGKILL } else { libc::SIGTERM };
    let rc = unsafe { libc::kill(-pid, sig) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

pub fn process_alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// 清理上次面板强退后残留的内核进程。
/// 匹配条件：exe 与本面板内核同路径，且命令行里引用了本面板的数据目录。
/// 读不到命令行时按「是孤儿」处理（Linux 上 /proc 扫一遍很便宜，宁可多杀也不能漏）。
///
/// 多个内核逐个扫 /proc，成本可忽略，不必像 Windows 那样合并成一次外部查询。
pub fn sweep_orphans(bins: &[String], data_dir: &Path) {
    for bin in bins {
        if !bin.is_empty() {
            sweep_one(bin, data_dir);
        }
    }
}

fn sweep_one(bin: &str, data_dir: &Path) {
    let Ok(self_exe) = std::fs::canonicalize(bin) else { return };
    let self_pid = std::process::id() as i32;
    let Ok(entries) = std::fs::read_dir("/proc") else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Ok(pid) = name.parse::<i32>() else { continue };
        if pid == self_pid {
            continue;
        }
        let Ok(exe) = std::fs::read_link(format!("/proc/{pid}/exe")) else { continue };
        if exe != self_exe {
            continue;
        }
        if !data_dir.as_os_str().is_empty() {
            match cmdline(pid) {
                Some(argv) => {
                    if !argv_touches(&argv, data_dir) {
                        continue;
                    }
                }
                None => {} // 读不到 → 按孤儿处理
            }
        }
        let _ = kill_process_tree(pid, true);
    }
}

/// 读 /proc/<pid>/cmdline（NUL 分隔）。None = 读不到或为空。
fn cmdline(pid: i32) -> Option<Vec<String>> {
    let b = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if b.is_empty() {
        return None;
    }
    let n = b.iter().rposition(|&c| c != 0).map(|i| i + 1).unwrap_or(0);
    let trimmed = &b[..n];
    Some(
        trimmed
            .split(|&c| c == 0)
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect(),
    )
}

/// 判断参数里是否有落在 dataDir 下的绝对路径。
fn argv_touches(argv: &[String], data_dir: &Path) -> bool {
    let Ok(abs) = std::fs::canonicalize(data_dir) else { return true };
    let abs_str = abs.to_string_lossy().into_owned();
    for a in argv {
        if a.is_empty() || a.starts_with('-') || !a.starts_with('/') {
            continue;
        }
        let p = Path::new(a);
        if p == abs {
            return true;
        }
        if a.starts_with(&abs_str) && a.as_bytes().get(abs_str.len()) == Some(&b'/') {
            return true;
        }
    }
    false
}
