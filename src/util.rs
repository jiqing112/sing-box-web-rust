// 小工具：原子写盘、随机数、hex、时间格式化。
// 与 Go 版一一对应：WriteFileAtomic 是全部落盘的唯一入口（tmp → fsync → rename），
// 断电时只可能是旧文件或新文件，不会出现半截 JSON —— 16 MB flash 的机器必须这么写。

use std::fs;
use std::io::Write;
use std::path::Path;

/// Go json.MarshalIndent(v, "", "  ") 的等价物：2 空格缩进、无尾随换行。
pub fn marshal_indent<T: serde::Serialize>(v: &T) -> std::io::Result<Vec<u8>> {
    serde_json::to_vec_pretty(v).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
}

/// tmp → fsync → rename。成功后目录里永远是完整文件。
pub fn write_file_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "no parent"))?;
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".tmp-{}", rand_hex(6)));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
    }
    let res = fs::rename(&tmp, path);
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}

pub fn rand_bytes(n: usize) -> Vec<u8> {
    let mut b = vec![0u8; n];
    let _ = getrandom::getrandom(&mut b);
    b
}

pub fn rand_hex(n: usize) -> String {
    hex_encode(&rand_bytes(n))
}

pub fn hex_encode(b: &[u8]) -> String {
    const T: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.push(T[(x >> 4) as usize] as char);
        s.push(T[(x & 0xf) as usize] as char);
    }
    s
}

pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    hex_encode(&h.finalize())
}

/// 取 md5 前 n 个 hex 字符。与 Go 版同名工具保持一致，
/// 目前只在单测/后续协议（vmess shortId 一类）里用得到。
#[allow(dead_code)]
pub fn md5_hex_first(data: &[u8], n: usize) -> String {
    use md5::{Digest, Md5};
    let mut h = Md5::new();
    h.update(data);
    let out = h.finalize();
    hex_encode(&out)[..n].to_string()
}

/// 当前时间，RFC3339（UTC）。前端 new Date(iso) 会自己转本地时区。
pub fn now_rfc3339() -> String {
    let now = time::OffsetDateTime::now_utc();
    format_rfc3339(now)
}

pub fn format_rfc3339(t: time::OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn unix_millis_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// JSON 里 null / 缺失都按默认值处理的反序列化，兼容手改过的数据文件。
pub mod null_default {
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D, T>(d: D) -> Result<T, D::Error>
    where
        D: Deserializer<'de>,
        T: Default + Deserialize<'de>,
    {
        let v = Option::<T>::deserialize(d)?;
        Ok(v.unwrap_or_default())
    }
}
