// 内核子进程的生命周期管理：启动/停止/重启、环形日志、崩溃限流自动重启、
// 孤儿进程清扫。与 Go 版行为一一对应。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use crate::engine;
use crate::model::LogLine;
use crate::util::now_rfc3339;

pub const LOG_CAPACITY: usize = 400;

/// 崩溃自动重启：10 分钟内最多 3 次，超过就放弃并通知上层降级。
const CRASH_WINDOW: Duration = Duration::from_secs(600);
const CRASH_LIMIT: usize = 3;
const RESTART_DELAY: Duration = Duration::from_secs(2);

pub struct Ring {
    buf: std::sync::Mutex<RingInner>,
}

struct RingInner {
    items: Vec<LogLine>,
    head: usize,
    size: usize,
}

impl Ring {
    pub fn new(n: usize) -> Ring {
        Ring {
            buf: std::sync::Mutex::new(RingInner {
                items: vec![
                    LogLine {
                        at: String::new(),
                        raw: String::new(),
                    };
                    n
                ],
                head: 0,
                size: 0,
            }),
        }
    }

    pub fn push(&self, raw: String) {
        let mut g = self.buf.lock().unwrap();
        let n = g.items.len();
        let head = g.head;
        g.items[head] = LogLine {
            at: now_rfc3339(),
            raw,
        };
        g.head = (head + 1) % n;
        if g.size < n {
            g.size += 1;
        }
    }

    pub fn snapshot(&self) -> Vec<LogLine> {
        let g = self.buf.lock().unwrap();
        let n = g.items.len();
        let start = (g.head + n - g.size) % n;
        let mut out = Vec::with_capacity(g.size);
        for i in 0..g.size {
            let it = &g.items[(start + i) % n];
            out.push(LogLine {
                at: it.at.clone(),
                raw: it.raw.clone(),
            });
        }
        out
    }
}

struct State {
    /// 0 = 没有在跑的子进程；启动后为其 pid。
    pid: u32,
    /// 每次启动自增；看护任务用它确认自己看护的还是当前这一代子进程。
    gen: u64,
    started: std::time::SystemTime,
    last_err: String,
    stopping: bool,
    crashes: Vec<std::time::Instant>,
}

pub struct Supervisor {
    bin: std::sync::Mutex<PathBuf>,
    conf: std::sync::Mutex<PathBuf>,
    eng: std::sync::Mutex<String>,
    st: std::sync::Mutex<State>,
    pub logs: Arc<Ring>,
    on_give_up: std::sync::Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

impl Supervisor {
    pub fn new(bin: &Path, conf: &Path, eng: &str) -> Arc<Supervisor> {
        Arc::new(Supervisor {
            bin: std::sync::Mutex::new(bin.to_path_buf()),
            conf: std::sync::Mutex::new(conf.to_path_buf()),
            eng: std::sync::Mutex::new(engine::normalize(eng)),
            st: std::sync::Mutex::new(State {
                pid: 0,
                gen: 0,
                started: std::time::SystemTime::UNIX_EPOCH,
                last_err: String::new(),
                stopping: false,
                crashes: Vec::new(),
            }),
            logs: Arc::new(Ring::new(LOG_CAPACITY)),
            on_give_up: std::sync::Mutex::new(None),
        })
    }

    /// 切换内核时同步可执行文件、配置文件与命令行格式。
    /// 调用前必须已 Stop，否则会把新旧参数混在一起。
    pub fn set_engine(&self, bin: &Path, conf: &Path, eng: &str) {
        *self.bin.lock().unwrap() = bin.to_path_buf();
        *self.conf.lock().unwrap() = conf.to_path_buf();
        *self.eng.lock().unwrap() = engine::normalize(eng);
    }

    #[allow(dead_code)]
    pub fn set_conf(&self, path: &Path) {
        *self.conf.lock().unwrap() = path.to_path_buf();
    }

    #[allow(dead_code)]
    pub fn set_on_give_up(&self, f: Box<dyn Fn() + Send + Sync>) {
        *self.on_give_up.lock().unwrap() = Some(f);
    }

    pub fn reset_crashes(&self) {
        self.st.lock().unwrap().crashes.clear();
    }

    pub fn running(&self) -> bool {
        self.st.lock().unwrap().pid != 0
    }

    /// 同步启动：state 全部是 std 锁，临界区内没有 await，
    /// 也因此不会出现「start 的 future 里含 handle_crash 的 future」的类型环。
    pub fn start(self: &Arc<Self>) -> Result<(), String> {
        let (bin, conf, eng) = {
            let b = self.bin.lock().unwrap().clone();
            let c = self.conf.lock().unwrap().clone();
            let e = self.eng.lock().unwrap().clone();
            (b, c, e)
        };
        let mut st = self.st.lock().unwrap();
        if st.pid != 0 {
            return Err("already running".into());
        }

        // 带路径分隔符的内核路径必须先转成绝对路径：子进程工作目录会改到数据目录，
        // 相对路径（如 ./mihomo.exe）会按新工作目录解析而找不到文件。
        let mut bin_abs = bin.clone();
        if bin.to_string_lossy().contains(['/', '\\']) {
            if let Ok(a) = std::fs::canonicalize(&bin) {
                bin_abs = a;
            } else if let (Ok(cwd), Some(fname)) = (std::env::current_dir(), bin.file_name()) {
                bin_abs = cwd.join(fname);
            }
        }
        let home_dir = engine::home(conf.parent().unwrap_or(Path::new(".")), &eng);
        if engine::is_mihomo(&eng) {
            // mihomo 的 -d 目录必须存在，否则它会在父目录里直接报错退出
            let _ = std::fs::create_dir_all(&home_dir);
        }

        let mut cmd = Command::new(&bin_abs);
        cmd.args(engine::run_args(&eng, &conf, &home_dir))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        crate::supervisor_sys::setup_process_group(&mut cmd);
        // sing-box 的 cache_file 相对路径以子进程工作目录为基准；
        // mihomo 的路径由 -d 决定，工作目录直接给数据目录。
        if engine::is_mihomo(&eng) {
            cmd.current_dir(&home_dir);
        } else {
            let dir = conf
                .parent()
                .and_then(|p| p.parent())
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| PathBuf::from("."));
            cmd.current_dir(&dir);
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                st.last_err = e.to_string();
                return Err(format!("start {}: {e}", engine::label(&eng)));
            }
        };
        let pid = child.id().unwrap_or(0);
        if let Some(out) = child.stdout.take() {
            let ring = self.logs.clone();
            tokio::spawn(async move { pump(out, ring).await });
        }
        if let Some(err) = child.stderr.take() {
            let ring = self.logs.clone();
            tokio::spawn(async move { pump(err, ring).await });
        }
        st.pid = pid;
        st.gen += 1;
        let gen = st.gen;
        st.started = std::time::SystemTime::now();
        st.last_err.clear();
        st.stopping = false;

        // 看护任务独占 Child：退出时按代数核对，避免旧任务清掉新一代的状态。
        let sup = self.clone();
        tokio::spawn(async move {
            let exit = child.wait().await;
            let stopping;
            {
                let mut st = sup.st.lock().unwrap();
                if st.gen == gen {
                    st.pid = 0;
                    if let Err(e) = &exit {
                        st.last_err = e.to_string();
                    }
                }
                stopping = st.stopping;
                let err_text = match &exit {
                    Ok(s) if s.success() => "clean".to_string(),
                    Ok(s) => format!("exit status {}", s.code().unwrap_or(-1)),
                    Err(e) => e.to_string(),
                };
                drop(st);
                sup.logs
                    .push(format!("[panel] {} exited: {err_text}", engine::label(&eng)));
            }
            if !stopping {
                sup.handle_crash();
            }
        });
        Ok(())
    }

    fn handle_crash(self: &Arc<Self>) {
        let n = {
            let mut st = self.st.lock().unwrap();
            let now = std::time::Instant::now();
            st.crashes.retain(|t| now.duration_since(*t) < CRASH_WINDOW);
            st.crashes.push(now);
            st.crashes.len()
        };
        if n > CRASH_LIMIT {
            self.logs
                .push("[panel] 10 分钟内崩溃超过 3 次，停止自动重启".into());
            if let Some(f) = self.on_give_up.lock().unwrap().as_ref() {
                f();
            }
            return;
        }
        self.logs.push("[panel] 2 秒后自动重启核心".into());
        let sup = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(RESTART_DELAY).await;
            let stopping = sup.st.lock().unwrap().stopping;
            if !stopping {
                let _ = sup.start();
            }
        });
    }

    pub async fn stop(&self) -> Result<(), String> {
        let pid = {
            let mut st = self.st.lock().unwrap();
            st.stopping = true;
            st.pid
        };
        if pid == 0 {
            return Ok(());
        }
        // 杀整个进程组/树，避免子进程残留
        let _ = crate::supervisor_sys::kill_process_tree(pid as i32, false);

        // 看护任务观察到退出后会把 pid 清零
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            if self.st.lock().unwrap().pid == 0 {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let _ = crate::supervisor_sys::kill_process_tree(pid as i32, true);
        // 强杀后不能立刻拉起新核心：cache.db 的文件锁要等进程真正消失才释放
        let deadline2 = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline2 {
            if !crate::supervisor_sys::process_alive(pid as i32) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // 看护任务可能还没来得及清状态；确认进程已死后直接清，restart 才能继续
        if !crate::supervisor_sys::process_alive(pid as i32) {
            let mut st = self.st.lock().unwrap();
            if st.pid == pid {
                st.pid = 0;
            }
        }
        Ok(())
    }

    pub async fn restart(self: &Arc<Self>) -> Result<(), String> {
        let _ = self.stop().await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        self.start()
    }

    /// 返回 (running, pid, started_epoch_secs, last_err)。
    pub fn status(&self) -> (bool, u32, std::time::SystemTime, String) {
        let st = self.st.lock().unwrap();
        if st.pid == 0 {
            return (false, 0, std::time::SystemTime::UNIX_EPOCH, st.last_err.clone());
        }
        (true, st.pid, st.started, st.last_err.clone())
    }

    pub fn logs_snapshot(&self) -> Vec<LogLine> {
        self.logs.snapshot()
    }
}

async fn pump<R: tokio::io::AsyncRead + Unpin>(r: R, ring: Arc<Ring>) {
    let mut lines = BufReader::new(r).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        ring.push(strip_ansi(&line));
    }
}

/// sing-box 的日志带 ANSI 颜色转义（ESC [ ... 字母），逐字符剥掉。
pub(crate) fn strip_ansi(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            let mut j = i + 2;
            while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                j += 1;
            }
            if j < bytes.len() {
                i = j + 1;
                continue;
            }
            break;
        }
        // 保持 UTF-8 边界安全：按字符推进
        let ch_len = utf8_len(bytes[i]);
        out.push_str(&s[i..std::cmp::min(i + ch_len, s.len())]);
        i += ch_len;
    }
    out
}

fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else if b >> 3 == 0b11110 {
        4
    } else {
        1
    }
}

/// 上次面板强退后残留的内核进程清扫。bin 为空时跳过。
pub fn sweep_orphans(bin: &str, data_dir: &Path) {
    if bin.is_empty() {
        return;
    }
    crate::supervisor_sys::sweep_orphans(bin, data_dir);
}
