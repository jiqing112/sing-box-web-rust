// Windows 托盘实现：Shell_NotifyIconW + 隐藏窗口消息循环，独立线程。
// 刻意不用 tray-icon/tao 这类框架：windows-sys 0.52 已是传递依赖（vendor 现成），
// 声明为直接依赖即可离线编译，零新增 crate。panic=abort 下禁用一切 unwrap。
// 注意：windows-sys 0.52 的 HWND/HICON/HMENU 均为 isize 别名。

use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use windows_sys::Win32::Foundation::{POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconW, NOTIFYICONDATAW, NIIF_INFO, NIF_INFO, NIF_ICON, NIF_MESSAGE,
    NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconFromResourceEx, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DestroyIcon, DestroyMenu, DispatchMessageW, GetCursorPos, GetMessageW, LoadIconW, PostMessageW,
    PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SetForegroundWindow,
    SetMenuDefaultItem, TrackPopupMenu, TranslateMessage, MF_CHECKED, MF_SEPARATOR, MF_STRING, MSG,
    SW_SHOWNORMAL, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WNDCLASSW, WM_CLOSE,
    WM_CONTEXTMENU, WM_DESTROY, WM_LBUTTONDBLCLK, WM_RBUTTONUP, WM_NULL,
};

use super::{panel_url, TrayEvent, TrayState};

/// 嵌入的品牌图标（tools/gen_tray_ico.py 生成，与 web 端 logo 同款）。
const ICO: &[u8] = include_bytes!("../assets/tray.ico");
const CLASS_NAME: &str = "SingBoxPanelTrayWnd";
const WM_APP_TRAY: u32 = 0x8000; // WM_APP：托盘鼠标回调
const WM_APP_REFRESH: u32 = 0x8001; // WM_APP+1：状态变了，刷 tooltip
const TRAY_ID: u32 = 1;
const MENU_OPEN: usize = 1;
const MENU_TOGGLE: usize = 2;
const MENU_RESTART: usize = 3;
const MENU_AUTOSTART: usize = 4;
const MENU_EXIT: usize = 5;

struct Shared {
    state: Mutex<TrayState>,
    events: tokio::sync::mpsc::UnboundedSender<TrayEvent>,
    hwnd: AtomicIsize,
    /// 最近一次成功 ADD 的 nid 副本，气泡/刷新在其上做 NIM_MODIFY
    nid: Mutex<Option<NOTIFYICONDATAW>>,
    taskbar_created: AtomicU32,
}

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();

/// 锁中毒时直接拿内部数据（panic=abort 下只是 debug 构建的兜底）。
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 写进 [u16; N] 定长缓冲（szTip/szInfo），保证 nul 结尾。
fn set_wide_buf(buf: &mut [u16], s: &str) {
    if buf.is_empty() {
        return;
    }
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.truncate(buf.len() - 1);
    let (head, tail) = buf.split_at_mut(v.len());
    head.copy_from_slice(&v);
    for t in tail {
        *t = 0;
    }
}

#[derive(Clone)]
pub struct TrayHandle {
    shared: Arc<Shared>,
}

impl TrayHandle {
    /// 更新共享状态；有变化时通知托盘线程刷新 tooltip。
    pub fn update(&self, f: impl FnOnce(&mut TrayState)) {
        let changed = {
            let mut s = lock(&self.shared.state);
            let before = s.clone();
            f(&mut s);
            *s != before
        };
        if changed {
            self.refresh();
        }
    }

    fn refresh(&self) {
        let hwnd = self.shared.hwnd.load(Ordering::Acquire);
        if hwnd != 0 {
            unsafe { PostMessageW(hwnd, WM_APP_REFRESH, 0, 0) };
        }
    }

    /// 退出前移除图标：WM_CLOSE → DefWindowProc DestroyWindow → WM_DESTROY 清理。
    pub fn remove(&self) {
        let hwnd = self.shared.hwnd.load(Ordering::Acquire);
        if hwnd != 0 {
            unsafe { PostMessageW(hwnd, WM_CLOSE, 0, 0) };
        }
    }
}

/// 会话 0（SYSTEM 计划任务开机自启）没有交互桌面，托盘无从谈起。
fn session_zero() -> bool {
    unsafe {
        let mut session: u32 = 0;
        let _ = ProcessIdToSessionId(GetCurrentProcessId(), &mut session);
        session == 0
    }
}

pub fn spawn(
    initial: TrayState,
) -> Option<(
    TrayHandle,
    tokio::sync::mpsc::UnboundedReceiver<TrayEvent>,
)> {
    if session_zero() {
        return None;
    }
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let shared = Arc::new(Shared {
        state: Mutex::new(initial),
        events: tx,
        hwnd: AtomicIsize::new(0),
        nid: Mutex::new(None),
        taskbar_created: AtomicU32::new(0),
    });
    if SHARED.set(shared.clone()).is_err() {
        return None; // 本进程已有托盘（正常只会 spawn 一次）
    }
    let worker = shared.clone();
    if std::thread::Builder::new()
        .name("tray".into())
        .spawn(move || unsafe { pump(worker) })
        .is_err()
    {
        return None;
    }
    Some((TrayHandle { shared }, rx))
}

unsafe fn pump(shared: Arc<Shared>) {
    let hinstance = GetModuleHandleW(std::ptr::null());
    let cls = wide(CLASS_NAME);
    let mut wc: WNDCLASSW = std::mem::zeroed();
    wc.lpfnWndProc = Some(tray_wndproc);
    wc.hInstance = hinstance;
    wc.lpszClassName = cls.as_ptr();
    if RegisterClassW(&wc) == 0 {
        eprintln!("托盘: 注册窗口类失败");
        return;
    }
    // HWND_MESSAGE（0）：纯消息窗口，不显示
    let hwnd = CreateWindowExW(
        0,
        cls.as_ptr(),
        std::ptr::null(),
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        hinstance,
        std::ptr::null(),
    );
    if hwnd == 0 {
        eprintln!("托盘: 创建窗口失败");
        return;
    }
    shared.hwnd.store(hwnd, Ordering::Release);
    let tb = RegisterWindowMessageW(wide("TaskbarCreated").as_ptr());
    shared.taskbar_created.store(tb, Ordering::Relaxed);

    add_icon(&shared);
    let url = panel_url(&lock(&shared.state).listen);
    balloon(
        &shared,
        "sing-box 面板已启动",
        &format!("双击托盘图标打开 {url}"),
    );

    let mut msg: MSG = std::mem::zeroed();
    while GetMessageW(&mut msg, 0, 0, 0) > 0 {
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    shared.hwnd.store(0, Ordering::Release);
}

unsafe extern "system" fn tray_wndproc(
    hwnd: windows_sys::Win32::Foundation::HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: isize,
) -> isize {
    let Some(shared) = SHARED.get() else {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    };
    let tb = shared.taskbar_created.load(Ordering::Relaxed);
    if tb != 0 && msg == tb {
        // explorer 重启会收走图标，重新放回去
        remove_nid(shared);
        add_icon(shared);
        return 0;
    }
    match msg {
        WM_APP_TRAY => match lparam as u32 {
            WM_LBUTTONDBLCLK => {
                open_panel(shared);
                0
            }
            WM_RBUTTONUP | WM_CONTEXTMENU => {
                show_menu(hwnd, shared);
                0
            }
            _ => 0,
        },
        WM_APP_REFRESH => {
            refresh_tooltip(shared);
            0
        }
        WM_DESTROY => {
            remove_nid(shared);
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 解析 .ico 目录取最接近 32px 的条目建 HICON；失败退回系统默认图标。
unsafe fn load_tray_icon() -> isize {
    if ICO.len() >= 6 {
        let count = u16::from_le_bytes([ICO[4], ICO[5]]) as usize;
        let mut best: Option<(i32, usize, u32)> = None;
        for i in 0..count {
            let e = match ICO.get(6 + i * 16..6 + (i + 1) * 16) {
                Some(e) => e,
                None => break,
            };
            let w = if e[0] == 0 { 256 } else { e[0] as i32 };
            let len = u32::from_le_bytes([e[8], e[9], e[10], e[11]]);
            let off = u32::from_le_bytes([e[12], e[13], e[14], e[15]]) as usize;
            let score = (w - 32).abs();
            if off.saturating_add(len as usize) <= ICO.len()
                && best.as_ref().map_or(true, |(b, _, _)| score < *b)
            {
                best = Some((score, off, len));
            }
        }
        if let Some((_, off, len)) = best {
            let h = CreateIconFromResourceEx(ICO[off..].as_ptr(), len, 1, 0x0003_0000, 32, 32, 0);
            if h != 0 {
                return h;
            }
        }
    }
    LoadIconW(0, 32512 as *const u16) // IDI_APPLICATION
}

unsafe fn add_icon(shared: &Arc<Shared>) {
    let hwnd = shared.hwnd.load(Ordering::Acquire);
    if hwnd == 0 {
        return;
    }
    let hicon = load_tray_icon();
    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = TRAY_ID;
    nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    nid.uCallbackMessage = WM_APP_TRAY;
    nid.hIcon = hicon;
    fill_tip(&mut nid.szTip, &lock(&shared.state));
    if Shell_NotifyIconW(NIM_ADD, &nid) != 0 {
        *lock(&shared.nid) = Some(nid);
    } else {
        eprintln!("托盘: 图标添加失败（任务栏可能未就绪）");
    }
}

unsafe fn remove_nid(shared: &Arc<Shared>) {
    if let Some(nid) = lock(&shared.nid).take() {
        Shell_NotifyIconW(NIM_DELETE, &nid);
        if nid.hIcon != 0 {
            DestroyIcon(nid.hIcon);
        }
    }
}

unsafe fn refresh_tooltip(shared: &Arc<Shared>) {
    let nid = {
        let g = lock(&shared.nid);
        match *g {
            Some(mut n) => {
                n.uFlags = NIF_TIP;
                fill_tip(&mut n.szTip, &lock(&shared.state));
                n
            }
            None => return,
        }
    };
    Shell_NotifyIconW(NIM_MODIFY, &nid);
    *lock(&shared.nid) = Some(nid);
}

fn tip_text(state: &TrayState) -> String {
    format!(
        "sing-box 面板 {}\n核心: {}",
        panel_url(&state.listen),
        if state.core_running { "运行中" } else { "已停止" }
    )
}

fn fill_tip(buf: &mut [u16], state: &TrayState) {
    set_wide_buf(buf, &tip_text(state));
}

unsafe fn balloon(shared: &Arc<Shared>, title: &str, text: &str) {
    let Some(mut nid) = lock(&shared.nid).clone() else {
        return;
    };
    nid.uFlags = NIF_INFO;
    set_wide_buf(&mut nid.szInfo, text);
    set_wide_buf(&mut nid.szInfoTitle, title);
    nid.dwInfoFlags = NIIF_INFO;
    Shell_NotifyIconW(NIM_MODIFY, &nid);
}

unsafe fn open_panel(shared: &Arc<Shared>) {
    let url = panel_url(&lock(&shared.state).listen);
    eprintln!("托盘: 打开 {url}");
    let verb = wide("open");
    let target = wide(&url);
    ShellExecuteW(0, verb.as_ptr(), target.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL as i32);
}

unsafe fn show_menu(
    hwnd: windows_sys::Win32::Foundation::HWND,
    shared: &Arc<Shared>,
) {
    let state = lock(&shared.state).clone();
    let menu = CreatePopupMenu();
    if menu == 0 {
        return;
    }
    AppendMenuW(menu, MF_STRING, MENU_OPEN, wide("打开面板").as_ptr());
    AppendMenuW(
        menu,
        MF_STRING,
        MENU_TOGGLE,
        wide(if state.core_running { "停止核心" } else { "启动核心" }).as_ptr(),
    );
    AppendMenuW(menu, MF_STRING, MENU_RESTART, wide("重启核心").as_ptr());
    AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
    AppendMenuW(
        menu,
        MF_STRING | if state.autostart { MF_CHECKED } else { 0 },
        MENU_AUTOSTART,
        wide("开机自启").as_ptr(),
    );
    AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
    AppendMenuW(menu, MF_STRING, MENU_EXIT, wide("退出").as_ptr());
    SetMenuDefaultItem(menu, MENU_OPEN as u32, 0);

    let mut pt: POINT = std::mem::zeroed();
    GetCursorPos(&mut pt);
    // 经典焦点坑：不 SetForegroundWindow 的话菜单不响应点击、点击别处也不消失
    SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(
        menu,
        TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
        pt.x,
        pt.y,
        0,
        hwnd,
        std::ptr::null(),
    );
    PostMessageW(hwnd, WM_NULL, 0, 0);
    DestroyMenu(menu);

    let ev = match cmd as usize {
        MENU_OPEN => {
            open_panel(shared);
            None
        }
        MENU_TOGGLE => Some(TrayEvent::CoreToggle),
        MENU_RESTART => Some(TrayEvent::CoreRestart),
        MENU_AUTOSTART => Some(TrayEvent::AutostartToggle),
        MENU_EXIT => Some(TrayEvent::Exit),
        _ => None,
    };
    if let Some(ev) = ev {
        // 勾选态等下次弹菜单时按最新状态重建，这里只回传事件
        let _ = shared.events.send(ev);
    }
}
