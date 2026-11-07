// Windows 系统代理实现：写注册表 Internet Settings + 广播刷新。
//
// 关键点——面板以 SYSTEM 计划任务运行时（本项目主用场景），HKCU 指向 SYSTEM
// 自己的 hive，直接写对登录用户完全无效。因此定位目标 hive 的顺序：
//   1) WTSGetActiveConsoleSessionId → WTSQueryUserToken → ImpersonateLoggedOnUser
//      → RegOpenCurrentUser（拿到真实交互用户 HKU\<SID> 的句柄，模拟期间打开的
//      句柄在还原身份后依然有效），写入动作记日志到具体用户；
//   2) 没有交互会话 / 非管理员拿不到用户令牌（普通用户面板 = 面板自身就是
//      登录用户）→ 兜底直写 HKCU。
// 写完广播 InternetSetOption(SETTINGS_CHANGED + REFRESH)，浏览器免重启生效。

use std::path::Path;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Networking::WinInet::{
    InternetSetOptionW, INTERNET_OPTION_REFRESH, INTERNET_OPTION_SETTINGS_CHANGED,
};
use windows_sys::Win32::Security::{ImpersonateLoggedOnUser, RevertToSelf};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegOpenCurrentUser, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_DWORD, REG_SZ,
};
use windows_sys::Win32::System::RemoteDesktop::{
    WTSEnumerateSessionsW, WTSFreeMemory, WTSGetActiveConsoleSessionId, WTSQuerySessionInformationW,
    WTSQueryUserToken,
    WTSDomainName, WTSUserName, WTSActive, WTS_INFO_CLASS, WTS_SESSION_INFOW,
};

const INTERNET_SETTINGS: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings";
// WTS_CURRENT_SERVER_HANDLE = NULL（本机 WTS 服务）
const WTS_CURRENT_SERVER: HANDLE = 0;

#[derive(Clone, Debug)]
pub struct Status {
    pub enabled: bool,
    pub server: String,
}

/// 读取目标 hive 当前的系统代理状态（打开 hive 失败时返回未启用兜底）。
pub fn status(_data_dir: &Path) -> Status {
    let Ok((hk, _)) = open_hive() else {
        return Status {
            enabled: false,
            server: String::new(),
        };
    };
    // 注意：先打开 Internet Settings 子键再查询（hive 根下没有这些值）
    let st = match key_path(hk) {
        Err(_) => Status {
            enabled: false,
            server: String::new(),
        },
        Ok(key) => {
            let st = Status {
                enabled: query_dword(key, "ProxyEnable").unwrap_or(0) == 1,
                server: query_sz(key, "ProxyServer").unwrap_or_default(),
            };
            unsafe {
                RegCloseKey(key);
            }
            st
        }
    };
    unsafe {
        RegCloseKey(hk);
    }
    st
}

/// 开/关系统代理。mixed_port 为代理指向端口（跟随面板设置）。
/// data_dir 用于保存「开启前的原始值」，关闭时尽量还原。
pub fn set_enabled(on: bool, mixed_port: u16, data_dir: &Path) -> Result<(), String> {
    let (hk, who) = open_hive()?;
    let r = do_set(hk, on, mixed_port, data_dir);
    unsafe {
        RegCloseKey(hk);
    }
    r?;
    broadcast();
    eprintln!(
        "系统代理: 已{}（写入 {} 的 Internet Settings，代理指向 127.0.0.1:{mixed_port}）",
        if on { "开启" } else { "关闭" },
        who
    );
    Ok(())
}

// ------------------------------------------------------------ hive 定位

/// 打开应写入的 hive。返回 (句柄, 日志用的人类可读归属描述)。
/// 候选会话：console 优先，其次所有 WTSActive 会话（RDP 场景）——
/// console 会话可能存在但没有登录用户（实测 RDP 机器上 console=3 为空壳），
/// 所以逐个候选尝试 WTSQueryUserToken，谁成功用谁。
fn open_hive() -> Result<(HKEY, String), String> {
    unsafe {
        let mut candidates: Vec<u32> = Vec::new();
        let console = WTSGetActiveConsoleSessionId();
        if console != 0 {
            candidates.push(console);
        }
        if let Ok(more) = enumerate_active_sessions() {
            for id in more {
                if !candidates.contains(&id) {
                    candidates.push(id);
                }
            }
        }
        for session in candidates {
            let mut token: HANDLE = 0;
            if WTSQueryUserToken(session, &mut token) == 0 {
                continue;
            }
            let who = session_user(session);
            if ImpersonateLoggedOnUser(token) != 0 {
                let mut hk: HKEY = 0;
                let r = RegOpenCurrentUser(KEY_QUERY_VALUE | KEY_SET_VALUE, &mut hk);
                RevertToSelf();
                CloseHandle(token);
                if r == 0 {
                    return Ok((hk, who));
                }
            } else {
                CloseHandle(token);
            }
        }
        // 兜底：面板自身账户（普通用户面板时即真实登录用户）
        Ok((HKEY_CURRENT_USER, "面板自身账户（HKCU）".into()))
    }
}

/// 枚举所有 WTSActive 会话（console 无登录用户时覆盖 RDP 会话）。
fn enumerate_active_sessions() -> Result<Vec<u32>, ()> {
    unsafe {
        let mut ptr: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
        let mut count: u32 = 0;
        if WTSEnumerateSessionsW(
            WTS_CURRENT_SERVER,
            0,
            1,
            &mut ptr as *mut *mut WTS_SESSION_INFOW,
            &mut count,
        ) == 0
        {
            return Err(());
        }
        let mut found = Vec::new();
        for i in 0..count as usize {
            let si = ptr.add(i).read();
            if si.State == WTSActive && si.SessionId != 0 {
                found.push(si.SessionId);
            }
        }
        WTSFreeMemory(ptr as *mut core::ffi::c_void);
        Ok(found)
    }
}

fn session_user(session: u32) -> String {
    unsafe {
        let mut name = String::new();
        for class in [WTSDomainName, WTSUserName] {
            let mut buf: *mut u16 = std::ptr::null_mut();
            let mut len: u32 = 0;
            if WTSQuerySessionInformationW(
                WTS_CURRENT_SERVER,
                session,
                class as WTS_INFO_CLASS,
                &mut buf,
                &mut len,
            ) != 0
                && !buf.is_null()
            {
                let s = pwstr_to_string(buf);
                WTSFreeMemory(buf as *mut core::ffi::c_void);
                if !s.is_empty() {
                    if !name.is_empty() {
                        name.push('\\');
                    }
                    name.push_str(&s);
                }
            }
        }
        if name.is_empty() {
            format!("会话 {session}（用户名查询失败）")
        } else {
            format!("用户 {name}（会话 {session}）")
        }
    }
}

unsafe fn pwstr_to_string(p: *const u16) -> String {
    let mut len = 0usize;
    while *p.add(len) != 0 {
        len += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
}

// ------------------------------------------------------------ 注册表读写

fn key_path(hk: HKEY) -> Result<HKEY, String> {
    let wide: Vec<u16> = INTERNET_SETTINGS.encode_utf16().chain(Some(0)).collect();
    let mut key: HKEY = 0;
    let r = unsafe { RegOpenKeyExW(hk, wide.as_ptr(), 0, KEY_QUERY_VALUE | KEY_SET_VALUE, &mut key) };
    if r != 0 {
        return Err(format!("打开 Internet Settings 失败 (winerror {r})"));
    }
    Ok(key)
}

fn query_sz(hk: HKEY, name: &str) -> Option<String> {
    unsafe {
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let mut ty: u32 = 0;
        let mut len: u32 = 0;
        if RegQueryValueExW(hk, wide.as_ptr(), std::ptr::null(), &mut ty, std::ptr::null_mut(), &mut len) != 0
            || len == 0
        {
            return None;
        }
        let mut buf = vec![0u8; len as usize];
        if RegQueryValueExW(hk, wide.as_ptr(), std::ptr::null(), &mut ty, buf.as_mut_ptr(), &mut len) != 0 {
            return None;
        }
        if ty != REG_SZ {
            return None;
        }
        let u16s: Vec<u16> = buf[..len as usize]
            .chunks_exact(2)
            .take_while(|c| *c != [0, 0])
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        Some(String::from_utf16_lossy(&u16s))
    }
}

fn query_dword(hk: HKEY, name: &str) -> Option<u32> {
    unsafe {
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let mut ty: u32 = 0;
        let mut v: u32 = 0;
        let mut len: u32 = 4;
        if RegQueryValueExW(hk, wide.as_ptr(), std::ptr::null(), &mut ty, (&mut v) as *mut u32 as *mut u8, &mut len) != 0
            || ty != REG_DWORD
        {
            return None;
        }
        Some(v)
    }
}

fn set_sz(hk: HKEY, name: &str, val: &str) {
    unsafe {
        let n: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let v: Vec<u16> = val.encode_utf16().chain(Some(0)).collect();
        RegSetValueExW(
            hk,
            n.as_ptr(),
            0,
            REG_SZ,
            v.as_ptr() as *const u8,
            (v.len() * 2) as u32,
        );
    }
}

fn set_dword(hk: HKEY, name: &str, val: u32) {
    unsafe {
        let n: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let v = val.to_ne_bytes();
        RegSetValueExW(hk, n.as_ptr(), 0, REG_DWORD, v.as_ptr(), 4);
    }
}

// ------------------------------------------------------------ 开关逻辑

/// 「开启前的原始值」，关闭时还原。独立状态文件，不挤占 settings.json。
#[derive(serde::Serialize, serde::Deserialize, Default, Clone, Debug)]
struct SavedState {
    enable: u32,
    server: String,
    #[serde(rename = "override")]
    override_list: String,
}

fn state_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("sysproxy-state.json")
}

fn do_set(hk: HKEY, on: bool, mixed_port: u16, data_dir: &Path) -> Result<(), String> {
    let key = key_path(hk)?;
    let r = if on {
        // 先记录原值（关闭时还原）
        let saved = SavedState {
            enable: query_dword(key, "ProxyEnable").unwrap_or(0),
            server: query_sz(key, "ProxyServer").unwrap_or_default(),
            override_list: query_sz(key, "ProxyOverride").unwrap_or_default(),
        };
        let r = std::fs::write(
            state_path(data_dir),
            serde_json::to_vec(&saved).map_err(|e| e.to_string())?,
        )
        .map_err(|e| format!("写入系统代理状态文件失败: {e}"));

        if r.is_ok() {
            set_dword(key, "ProxyEnable", 1);
            set_sz(key, "ProxyServer", &format!("127.0.0.1:{mixed_port}"));
            let cur = query_sz(key, "ProxyOverride").unwrap_or_default();
            if !cur.contains("<local>") {
                let append = if cur.trim().is_empty() {
                    "localhost;127.0.0.1;<local>".to_string()
                } else {
                    format!("{cur};localhost;127.0.0.1;<local>")
                };
                set_sz(key, "ProxyOverride", &append);
            }
        }
        r
    } else {
        // 还原开启前原值；无状态文件（非本面板开启）则仅关掉开关
        let path = state_path(data_dir);
        match std::fs::read(&path).ok().and_then(|b| serde_json::from_slice::<SavedState>(&b).ok()) {
            Some(saved) => {
                set_dword(key, "ProxyEnable", saved.enable);
                set_sz(key, "ProxyServer", &saved.server);
                set_sz(key, "ProxyOverride", &saved.override_list);
                let _ = std::fs::remove_file(&path);
                Ok(())
            }
            None => {
                set_dword(key, "ProxyEnable", 0);
                Ok(())
            }
        }
    };
    unsafe {
        RegCloseKey(key);
    }
    r
}

/// 广播刷新：WinInet 层即时感知，浏览器免重启生效。
fn broadcast() {
    unsafe {
        InternetSetOptionW(std::ptr::null(), INTERNET_OPTION_SETTINGS_CHANGED, std::ptr::null(), 0);
        InternetSetOptionW(std::ptr::null(), INTERNET_OPTION_REFRESH, std::ptr::null(), 0);
    }
}
