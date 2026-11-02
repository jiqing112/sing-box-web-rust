// mihomo 的 geo 数据库（GeoSite.dat / geoip.metadb）准备：三层兜底
//   1. 编译期：assets/ 下的 gzip 副本 embed 进二进制，离线也能用；
//   2. 运行期：面板启动、以及切到 sing-box 时后台预取；
//   3. 切换前：Ensure 兜底，文件已在就直接返回。
// 并发调用共享同一次下载；单源 8 秒内低于 48 KB/s 自动换镜像。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::Mutex;

const JSDELIVR: &str = "https://cdn.jsdelivr.net/gh/MetaCubeX/meta-rules-dat@release/";
const GHPROXY: &str = "https://gh-proxy.com/https://github.com/MetaCubeX/meta-rules-dat/raw/release/";

/// 慢速切源的判定窗口与阈值。
const SLOW_WINDOW: Duration = Duration::from_secs(8);
const SLOW_SPEED: f64 = 48.0 * 1024.0; // 48 KB/s

#[derive(Clone, Debug)]
pub struct GeoFile {
    /// mihomo 期望的落盘文件名（注意 GeoSite.dat 的大小写）
    pub name: &'static str,
    pub url: String,
    pub min_size: i64,
}

/// 第二个文件的映射关系是实测出来的：mihomo 工作目录里叫 geoip.metadb，
/// 装的其实是 country.mmdb 的内容。按名字下 geoip.dat 塞进去会报
/// `MMDB invalid, remove and download` 然后重新下 12 秒。
pub fn default_files() -> Vec<GeoFile> {
    vec![
        GeoFile {
            name: "GeoSite.dat",
            url: format!("{JSDELIVR}geosite.dat"),
            min_size: 1 << 20,
        },
        GeoFile {
            name: "geoip.metadb",
            url: format!("{JSDELIVR}country.mmdb"),
            min_size: 1 << 20,
        },
    ]
}

#[derive(Clone, Serialize)]
pub struct Status {
    pub state: String,
    pub dir: String,
    pub done: i64,
    pub total: i64,
    pub current: String,
    pub bytes: i64,
    pub total_bytes: i64,
    pub missing: Vec<String>,
    pub embedded: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error: String,
    pub at: String,
}

pub const STATE_IDLE: &str = "idle";
pub const STATE_DOWNLOADING: &str = "downloading";
pub const STATE_READY: &str = "ready";
pub const STATE_FAILED: &str = "failed";

struct Inner {
    st: Status,
    err: Option<String>,
}

pub struct Manager {
    dir: PathBuf,
    files: Vec<GeoFile>,
    inner: Mutex<Inner>,
    /// 串行化下载：并发调用共享同一次下载，不会重复拉 12 MB。
    download_lock: tokio::sync::Mutex<()>,
}

#[cfg(feature = "embed-geo")]
fn embedded(name: &str) -> Option<&'static [u8]> {
    match name {
        "GeoSite.dat" => Some(include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/GeoSite.dat.gz")).as_slice()),
        "geoip.metadb" => Some(include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/geoip.metadb.gz")).as_slice()),
        _ => None,
    }
}

#[cfg(not(feature = "embed-geo"))]
fn embedded(_name: &str) -> Option<&'static [u8]> {
    None
}

fn has_embedded(files: &[GeoFile]) -> bool {
    files.iter().any(|f| embedded(f.name).is_some())
}

impl Manager {
    pub fn new(dir: PathBuf) -> Arc<Manager> {
        let files = default_files();
        let st = Status {
            state: STATE_IDLE.into(),
            dir: dir.to_string_lossy().into_owned(),
            done: 0,
            total: files.len() as i64,
            current: String::new(),
            bytes: 0,
            total_bytes: 0,
            missing: Vec::new(),
            embedded: has_embedded(&files),
            error: String::new(),
            at: crate::util::now_rfc3339(),
        };
        Arc::new(Manager {
            dir,
            files,
            inner: Mutex::new(Inner { st, err: None }),
            download_lock: tokio::sync::Mutex::new(()),
        })
    }

    fn missing_files(&self) -> Vec<GeoFile> {
        self.files
            .iter()
            .filter(|f| !file_ok(&self.dir.join(f.name), f.min_size))
            .cloned()
            .collect()
    }

    /// 当前状态（不触发任何工作）。
    pub async fn status(&self) -> Status {
        let mut g = self.inner.lock().await;
        let missing = self.missing_files();
        let st = &mut g.st;
        st.missing = missing.iter().map(|f| f.name.to_string()).collect();
        st.embedded = has_embedded(&self.files);
        // 状态以磁盘为准：用户手动拷进/删掉文件都要立刻反映出来。
        // 只有「正在下载」和「上次失败」保留原状态。
        if st.state != STATE_DOWNLOADING {
            if st.missing.is_empty() {
                st.state = STATE_READY.into();
                st.done = st.total;
                st.current.clear();
                st.error.clear();
            } else if st.state == STATE_READY {
                st.state = STATE_IDLE.into();
                st.done = 0;
            }
        }
        st.clone()
    }

    /// 文件是否已全部就位（不下载）。
    pub async fn ready(&self) -> bool {
        self.missing_files().is_empty()
    }

    /// 保证 geo 文件就位，必要时下载。并发调用等待同一次下载。
    pub async fn ensure(&self) -> (Status, Result<(), String>) {
        loop {
            if self.missing_files().is_empty() {
                {
                    // 先放锁再调 status()：tokio Mutex 不可重入，否则死锁到外层超时
                    let mut g = self.inner.lock().await;
                    g.st.state = STATE_READY.into();
                    g.st.done = g.st.total;
                    g.st.current.clear();
                    g.st.at = crate::util::now_rfc3339();
                }
                return (self.status().await, Ok(()));
            }
            match self.download_lock.try_lock() {
                Ok(guard) => {
                    return self.download_all(guard).await;
                }
                Err(_) => {
                    // 已有下载在跑 → 等它结束再判断
                    let g = self.download_lock.lock().await;
                    drop(g);
                }
            }
        }
    }

    async fn download_all(
        &self,
        _guard: tokio::sync::MutexGuard<'_, ()>,
    ) -> (Status, Result<(), String>) {
        {
            let mut g = self.inner.lock().await;
            g.err = None;
            g.st.state = STATE_DOWNLOADING.into();
            g.st.done = 0;
            g.st.total = self.files.len() as i64;
            g.st.bytes = 0;
            g.st.total_bytes = 0;
            g.st.error.clear();
            g.st.at = crate::util::now_rfc3339();
        }
        let _ = std::fs::create_dir_all(&self.dir);

        let counter = Arc::new(AtomicI64::new(0));
        let result = self.fetch_all(&counter).await;

        {
            let mut g = self.inner.lock().await;
            match &result {
                Err(e) => {
                    g.err = Some(e.clone());
                    g.st.state = STATE_FAILED.into();
                    g.st.error = e.clone();
                }
                Ok(()) => {
                    g.err = None;
                    g.st.state = STATE_READY.into();
                    g.st.done = g.st.total;
                    g.st.current.clear();
                }
            }
            g.st.at = crate::util::now_rfc3339();
        }
        (self.status().await, result)
    }

    /// 后台预取，失败只记状态不返回错误。
    pub async fn prewarm(&self) {
        if self.ready().await {
            return;
        }
        let _ = self.ensure().await;
    }

    async fn fetch_all(&self, counter: &Arc<AtomicI64>) -> Result<(), String> {
        let missing = self.missing_files();
        if missing.is_empty() {
            return Ok(());
        }
        // 两个文件并行下：独立资源，串行会让总耗时翻倍。
        let mut tasks = Vec::new();
        for f in &missing {
            let f = f.clone();
            let dir = self.dir.clone();
            let counter = counter.clone();
            tasks.push(tokio::spawn(async move {
                let dst = dir.join(&f.name);
                // 先试内置副本（编译时打包进来的），再走网络
                if let Some(gz) = embedded(&f.name) {
                    if let Ok(n) = extract_embedded(gz, &dst, f.min_size) {
                        counter.fetch_add(n, Ordering::Relaxed);
                        return Ok((f.name.to_string(), n));
                    }
                }
                download_with_mirrors(&f, &dst, &counter).await?;
                let n = counter.fetch_add(0, Ordering::Relaxed);
                Ok::<(String, i64), String>((f.name.to_string(), n))
            }));
        }
        let mut errs: Vec<String> = Vec::new();
        for t in tasks {
            match t.await {
                Ok(Ok((name, _))) => {
                    let mut g = self.inner.lock().await;
                    g.st.done += 1;
                    g.st.current = name;
                    g.st.bytes = counter.load(Ordering::Relaxed);
                    g.st.at = crate::util::now_rfc3339();
                }
                Ok(Err(e)) => errs.push(e),
                Err(e) => errs.push(e.to_string()),
            }
        }
        if errs.is_empty() {
            Ok(())
        } else {
            Err(format!("geo 数据下载失败：{}", errs.join("；")))
        }
    }
}

/// mirrorsOf：URL 空格分隔多个地址时按顺序使用；否则 jsDelivr 失败退 gh-proxy。
fn mirrors_of(f: &GeoFile) -> Vec<String> {
    let fields: Vec<&str> = f.url.split_whitespace().collect();
    if fields.len() > 1 {
        return fields.iter().map(|s| s.to_string()).collect();
    }
    if let Some(base) = f.url.strip_prefix(JSDELIVR) {
        return vec![f.url.clone(), format!("{GHPROXY}{base}")];
    }
    vec![f.url.clone()]
}

/// 依次尝试候选镜像，成功后原子落盘。
async fn download_with_mirrors(
    f: &GeoFile,
    dst: &Path,
    counter: &Arc<AtomicI64>,
) -> Result<(), String> {
    let mut b = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .no_proxy(); // 预取不走用户配置的代理，避免循环依赖
    #[cfg(windows)]
    {
        b = b.use_native_tls();
    }
    #[cfg(not(windows))]
    {
        b = b.use_rustls_tls();
    }
    let client = b.build().map_err(|e| e.to_string())?;
    let mut last_err = String::new();
    for url in mirrors_of(f) {
        match download_one(&client, &url, dst, f.min_size, counter).await {
            Ok(()) => return Ok(()),
            Err(e) => last_err = e,
        }
    }
    Err(format!("{}: {last_err}", f.name))
}

async fn download_one(
    client: &reqwest::Client,
    url: &str,
    dst: &Path,
    min_size: i64,
    counter: &Arc<AtomicI64>,
) -> Result<(), String> {
    let resp = client
        .get(url)
        .header("User-Agent", "sing-box-panel/1.0 (+geodata)")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    if status != reqwest::StatusCode::OK {
        return Err(format!("HTTP {}", status.as_u16()));
    }
    if let Some(total) = resp.content_length() {
        if total > 0 && (total as i64) < min_size {
            return Err(format!("响应过小（{total} 字节）"));
        }
    }

    let tmp = dst.with_extension("tmp");
    let mut out = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
    let mut written: i64 = 0;
    // 慢速检测：窗口内平均速度低于阈值就放弃当前源，换下一个镜像。
    // 放弃时把已经计入的字节数退回去，避免进度虚高。
    let mut mark = Instant::now();
    let mut mark_bytes: i64 = 0;
    use futures_util::StreamExt;
    use std::io::Write;
    let mut stream = resp.bytes_stream();
    fn cleanup(tmp: &Path, written: i64, counter: &Arc<AtomicI64>) {
        let _ = std::fs::remove_file(tmp);
        if written > 0 {
            counter.fetch_add(-written, Ordering::Relaxed);
        }
    }
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(chunk) => {
                if let Err(e) = out.write_all(&chunk) {
                    cleanup(&tmp, written, counter);
                    return Err(e.to_string());
                }
                written += chunk.len() as i64;
                counter.fetch_add(chunk.len() as i64, Ordering::Relaxed);
                let el = mark.elapsed();
                if el >= SLOW_WINDOW {
                    let speed =
                        (written - mark_bytes) as f64 / el.as_secs_f64();
                    if speed < SLOW_SPEED {
                        let msg = format!("速度过慢（{:.0} KB/s），换源", speed / 1024.0);
                        cleanup(&tmp, written, counter);
                        return Err(msg);
                    }
                    mark = Instant::now();
                    mark_bytes = written;
                }
            }
            Err(e) => {
                cleanup(&tmp, written, counter);
                return Err(e.to_string());
            }
        }
    }
    if out.sync_all().is_err() {
        let _ = std::fs::remove_file(&tmp);
        return Err("write failed".into());
    }
    drop(out);
    if written < min_size {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("下载不完整（{written} 字节）"));
    }
    // rename 保证 mihomo 不会读到写了一半的文件
    std::fs::rename(&tmp, dst).map_err(|e| e.to_string())
}

/// 把编译期打包的 gzip 副本流式解压释放到工作目录。返回写入字节数。
/// 全程流式：不能把整份 7.6 MB 读进堆再写盘。
fn extract_embedded(gz: &[u8], dst: &Path, min_size: i64) -> Result<i64, String> {
    use flate2::read::MultiGzDecoder;
    use std::io::Read;
    let mut r = MultiGzDecoder::new(gz);
    let tmp = dst.with_extension("tmp");
    let mut out = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
    let mut buf = [0u8; 32 << 10];
    let mut written: i64 = 0;
    loop {
        let n = r.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        use std::io::Write;
        out.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        written += n as i64;
    }
    out.sync_all().map_err(|e| e.to_string())?;
    drop(out);
    // 压缩后的大小不能拿来比阈值，只能校验解压后的字节数
    if written < min_size {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("内置副本过小（{written} 字节）"));
    }
    std::fs::rename(&tmp, dst).map_err(|e| e.to_string())?;
    Ok(written)
}

/// 文件是否已就位：存在且不小于阈值（挡住上次下载失败留下的空文件）。
fn file_ok(path: &Path, min_size: i64) -> bool {
    match std::fs::metadata(path) {
        Ok(md) => md.is_file() && (md.len() as i64) >= min_size,
        Err(_) => false,
    }
}
