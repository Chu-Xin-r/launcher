//! 双击 Ctrl 呼出/隐藏弹窗：低级键盘钩子（WH_KEYBOARD_LL）。
//!
//! 规则：两次 Ctrl 按下间隔 < 500ms，且中间没按过任何其他键 → 触发。
//! 钩子回调必须轻量：只做时间戳判断，窗口操作全部转发主线程。

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use tauri::{AppHandle, Emitter};
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_CONTROL, VK_LCONTROL, VK_RCONTROL};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, PeekMessageW, PM_REMOVE, SetWindowsHookExW,
    TranslateMessage, UnhookWindowsHookEx, KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL,
    WM_KEYDOWN, WM_SYSKEYDOWN,
};

static APP: OnceLock<AppHandle> = OnceLock::new();
static LAST_CTRL_MS: AtomicU64 = AtomicU64::new(0);
static OTHER_KEY: AtomicBool = AtomicBool::new(false);
static POPUP_GENERATION: AtomicU64 = AtomicU64::new(0);
static POPUP_FOCUS_GUARD_UNTIL_MS: AtomicU64 = AtomicU64::new(0);
static DOUBLE_CTRL_ENABLED: AtomicBool = AtomicBool::new(true);

/// 双击 Ctrl 呼出开关（设置页控制）。
pub fn set_double_ctrl_enabled(on: bool) {
    DOUBLE_CTRL_ENABLED.store(on, Ordering::Relaxed);
}

/// 返回当前弹窗显示世代，用于丢弃隐藏/显示竞态中的旧失焦回调。
pub fn popup_generation() -> u64 {
    POPUP_GENERATION.load(Ordering::Acquire)
}

// —— 自定义呼出快捷键（低级钩子内拦截，可吞掉系统快捷键，如 Alt+空格）——
static CUSTOM_ENABLED: AtomicBool = AtomicBool::new(false);
static CUSTOM_LAST: AtomicU64 = AtomicU64::new(0);
static CUSTOM_MODS: AtomicU32 = AtomicU32::new(0); // bit0 ctrl bit1 alt bit2 shift bit3 win
static CUSTOM_VK: AtomicU32 = AtomicU32::new(0);

/// 设置自定义呼出快捷键（如 "alt+space"、"ctrl+alt+l"）。由设置页调用。
pub fn set_custom_hotkey(enabled: bool, spec: &str) {
    match parse_hotkey(spec) {
        Some((mods, vk)) => {
            CUSTOM_MODS.store(mods, Ordering::Release);
            CUSTOM_VK.store(vk, Ordering::Release);
            CUSTOM_ENABLED.store(enabled, Ordering::Release);
        }
        None => CUSTOM_ENABLED.store(false, Ordering::Release),
    }
}

/// "ctrl+alt+l" → (修饰位, 虚拟键码)；修饰位与 mods_now() 对应
pub fn parse_hotkey(s: &str) -> Option<(u32, u32)> {
    let mut mods = 0u32;
    let mut key: Option<u32> = None;
    for part in s.split('+') {
        let p = part.trim().to_ascii_lowercase();
        match p.as_str() {
            "ctrl" | "control" => mods |= 1,
            "alt" => mods |= 2,
            "shift" => mods |= 4,
            "win" | "meta" => mods |= 8,
            "space" => key = Some(0x20),
            "" => {}
            _ => {
                if p.len() == 1 {
                    let c = p.chars().next().unwrap();
                    if c.is_ascii_alphabetic() {
                        key = Some(c.to_ascii_uppercase() as u32);
                    } else if c.is_ascii_digit() {
                        key = Some(c as u32);
                    }
                } else if p.starts_with('f')
                    && p.len() >= 2
                    && p[1..].chars().all(|c| c.is_ascii_digit())
                {
                    let n: u32 = p[1..].parse().ok()?;
                    if (1..=24).contains(&n) {
                        key = Some(0x70 + n - 1); // VK_F1 = 0x70
                    }
                }
            }
        }
    }
    if mods != 0 {
        key.map(|k| (mods, k))
    } else {
        None
    }
}

/// 当前物理修饰键状态（位定义同上）
#[inline]
fn mods_now() -> u32 {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
    };
    let mut m = 0u32;
    unsafe {
        if GetAsyncKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000 != 0 {
            m |= 1;
        }
        if GetAsyncKeyState(VK_MENU.0 as i32) as u16 & 0x8000 != 0 {
            m |= 2;
        }
        if GetAsyncKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000 != 0 {
            m |= 4;
        }
        if GetAsyncKeyState(VK_LWIN.0 as i32) as u16 & 0x8000 != 0
            || GetAsyncKeyState(VK_RWIN.0 as i32) as u16 & 0x8000 != 0
        {
            m |= 8;
        }
    }
    m
}

/// 呼出后的短暂焦点保护期，避免 show/focus 的瞬时失焦事件误隐藏。
pub fn popup_focus_guard_active() -> bool {
    now_ms() < POPUP_FOCUS_GUARD_UNTIL_MS.load(Ordering::Acquire)
}

fn arm_popup_focus_guard() {
    // 仅覆盖呼出瞬间的短暂焦点抖动；随后失焦一律走 120ms 快速隐藏
    POPUP_FOCUS_GUARD_UNTIL_MS.store(now_ms().saturating_add(250), Ordering::Release);
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 在独立线程调用：安装钩子并进入消息循环（钩子依赖消息泵）。
/// 每 30 秒重装一次钩子：系统在高负载时会静默移除低级钩子，重装保证存活。
pub fn run(app: AppHandle) {
    let _ = APP.set(app);
    std::thread::spawn(|| unsafe {
        #[cfg(debug_assertions)]
        eprintln!("[hook] thread start");
        let install = || {
            SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0).map_err(|e| {
                eprintln!("[hook] install failed: {e}");
            })
        };
        let Ok(mut hook) = install() else {
            return;
        };
        #[cfg(debug_assertions)]
        eprintln!("[hook] installed");
        let mut msg = MSG::default();
        loop {
            // 等待消息或 30 秒超时；超时即重装钩子（系统高负载时会静默移除低级钩子）
            let w = windows::Win32::UI::WindowsAndMessaging::MsgWaitForMultipleObjectsEx(
                None,
                30_000,
                windows::Win32::UI::WindowsAndMessaging::QS_ALLINPUT,
                windows::Win32::UI::WindowsAndMessaging::MWMO_INPUTAVAILABLE,
            );
            if w == windows::Win32::Foundation::WAIT_TIMEOUT {
                let _ = UnhookWindowsHookEx(hook);
                match install() {
                    Ok(h) => hook = h,
                    Err(_) => continue,
                }
                #[cfg(debug_assertions)]
                eprintln!("[hook] reinstalled (watchdog)");
            }
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    });
}

extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0 {
            let info = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            let msg = wparam.0 as u32;
            let is_down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
            let is_ctrl =
                info.vkCode == VK_CONTROL.0 as u32
                    || info.vkCode == VK_LCONTROL.0 as u32
                    || info.vkCode == VK_RCONTROL.0 as u32;

            // 自定义呼出快捷键：命中即触发并吞掉按键（阻止系统快捷键，如 Alt+空格 的系统菜单）
            if CUSTOM_ENABLED.load(Ordering::Relaxed) {
                let want_vk = CUSTOM_VK.load(Ordering::Relaxed);
                let want_mods = CUSTOM_MODS.load(Ordering::Relaxed);
                if want_vk != 0 && info.vkCode == want_vk && (mods_now() & want_mods) == want_mods {
                    // 防按住 auto-repeat 反复触发
                    let now = now_ms();
                    let repeat = now.saturating_sub(CUSTOM_LAST.load(Ordering::Relaxed)) < 300;
                    CUSTOM_LAST.store(now, Ordering::Relaxed);
                    if !repeat {
                        LAST_CTRL_MS.store(0, Ordering::Relaxed);
                        if let Some(app) = APP.get() {
                            let _ = app.run_on_main_thread(|| {
                                if let Some(app) = APP.get() {
                                    toggle_popup(app);
                                }
                            });
                        }
                    }
                    return LRESULT(1); // 吞掉：不传给系统/应用
                }
            }

            if is_down && is_ctrl {
                let now = now_ms();
                #[cfg(debug_assertions)]
                eprintln!("[hook] ctrl down at {now}");
                let last = LAST_CTRL_MS.load(Ordering::Relaxed);
                if now.saturating_sub(last) < 120 {
                    // 长按重复，忽略
                    return CallNextHookEx(None, code, wparam, lparam);
                }
                if !DOUBLE_CTRL_ENABLED.load(Ordering::Relaxed) {
                    LAST_CTRL_MS.store(now, Ordering::Relaxed);
                    return CallNextHookEx(None, code, wparam, lparam);
                }
                if !OTHER_KEY.load(Ordering::Relaxed) && last != 0 && now - last < 500 {
                    // 双击 Ctrl！
                    LAST_CTRL_MS.store(0, Ordering::Relaxed);
                    OTHER_KEY.store(true, Ordering::Relaxed); // 防三连击
                    if let Some(app) = APP.get() {
                        let _ = app.run_on_main_thread(|| {
                            if let Some(app) = APP.get() {
                                toggle_popup(app);
                            }
                        });
                    }
                    return CallNextHookEx(None, code, wparam, lparam);
                }
                LAST_CTRL_MS.store(now, Ordering::Relaxed);
                OTHER_KEY.store(false, Ordering::Relaxed);
            } else if is_down && !is_ctrl {
                OTHER_KEY.store(true, Ordering::Relaxed);
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }
}

/// 将弹窗恢复到配置尺寸并居中，避免 Windows 在隐藏/重绘竞态后保留异常小矩形。
fn prepare_popup(w: &tauri::WebviewWindow) {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows::Win32::UI::HiDpi::GetDpiForWindow;
    use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER};

    // 必须在 show 之前同步定位：Tauri 的 set_size/center 是异步排队，
    // 先 show 后定位会让窗口在旧位置（屏幕左上角）闪现一帧残影。
    let Ok(tauri_hwnd) = w.hwnd() else {
        let _ = w.set_size(tauri::Size::Logical(tauri::LogicalSize::new(720.0, 500.0)));
        let _ = w.center();
        return;
    };
    unsafe {
        // tauri 的 HWND 类型来自其自带 windows crate，转回本 crate 的类型
        let hwnd = HWND(tauri_hwnd.0);
        // 逻辑 720×500 → 物理像素（按窗口 DPI）
        let dpi = GetDpiForWindow(hwnd).max(96);
        let scale = dpi as f64 / 96.0;
        let wd = (720.0 * scale) as i32;
        let hg = (500.0 * scale) as i32;

        // 居中到光标所在显示器的可用区域
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let hmon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(hmon, &mut mi).as_bool() {
            let ax = mi.rcWork.left + (mi.rcWork.right - mi.rcWork.left - wd) / 2;
            let ay = mi.rcWork.top + (mi.rcWork.bottom - mi.rcWork.top - hg) / 2;
            let _ = SetWindowPos(hwnd, None, ax, ay, wd, hg, SWP_NOZORDER | SWP_NOACTIVATE);
            return;
        }
    }
    let _ = w.set_size(tauri::Size::Logical(tauri::LogicalSize::new(720.0, 500.0)));
    let _ = w.center();
}

static HIDE_PID: AtomicU32 = AtomicU32::new(0);

/// 隐藏 tao 内部创建的 "Tao Thread Event Target" 可见小窗口（6×6，抢焦点闪烁）。
/// 该窗口仅用于消息投递，隐藏不影响功能。
unsafe extern "system" fn hide_tao_cb(hwnd: HWND, _: LPARAM) -> BOOL {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetWindowThreadProcessId, IsWindowVisible, ShowWindow, SW_HIDE,
    };
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == HIDE_PID.load(Ordering::Relaxed) && IsWindowVisible(hwnd).as_bool() {
        let mut buf = [0u16; 64];
        let n = GetClassNameW(hwnd, &mut buf);
        let cls = String::from_utf16_lossy(&buf[..n as usize]);
        if cls == "Tao Thread Event Target" {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
    BOOL(1)
}

fn hide_tao_flash_windows() {
    use windows::Win32::UI::WindowsAndMessaging::EnumWindows;
    HIDE_PID.store(std::process::id(), Ordering::Relaxed);
    unsafe {
        let _ = EnumWindows(Some(hide_tao_cb), LPARAM(0));
    }
}

/// 弹窗是否处于"呼出"状态（透明度方案：窗口常驻，用透明度+鼠标穿透切换）
static POPUP_VISIBLE: AtomicBool = AtomicBool::new(false);

/// 供其他模块查询弹窗当前是否呼出。
pub fn popup_visible() -> bool {
    POPUP_VISIBLE.load(Ordering::Acquire)
}

/// 应用启动时把弹窗置为"透明隐藏"常驻状态（不改变 show/hide 语义，无首帧闪烁）。
pub fn init_hidden(w: &tauri::WebviewWindow) {
    set_window_alpha(w, 0);
    let _ = w.show();
    let _ = w.set_ignore_cursor_events(true);
    POPUP_VISIBLE.store(false, Ordering::Release);
}

fn apply_visible(w: &tauri::WebviewWindow, visible: bool) {
    let _ = w.set_ignore_cursor_events(!visible);
    set_window_alpha(w, if visible { 255 } else { 0 });
    POPUP_VISIBLE.store(visible, Ordering::Release);
}

/// 用 SetLayeredWindowAttributes 设置整窗不透明度（tauri 2.11 无 set_opacity API）。
fn set_window_alpha(w: &tauri::WebviewWindow, alpha: u8) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetLayeredWindowAttributes, SetWindowLongPtrW, GWL_EXSTYLE,
        LWA_ALPHA, WS_EX_LAYERED,
    };
    let Ok(t_hwnd) = w.hwnd() else { return };
    let hwnd = HWND(t_hwnd.0);
    unsafe {
        // 确保 WS_EX_LAYERED 样式
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, (ex as u32 | WS_EX_LAYERED.0) as isize);
        let _ = SetLayeredWindowAttributes(hwnd, windows::Win32::Foundation::COLORREF(0), alpha, LWA_ALPHA);
    }
}

/// 呼出弹窗（内部实现）。
fn show_popup_inner(app: &AppHandle) {
    use tauri::Manager;
    let Some(w) = app.get_webview_window("popup") else {
        return;
    };
    POPUP_GENERATION.fetch_add(1, Ordering::AcqRel);
    prepare_popup(&w);
    let _ = w.show(); // 位置已同步设好，此处显示不会有残影
    apply_visible(&w, true);
    let _ = w.set_focus();
    arm_popup_focus_guard();
    // show/focus 可能同步产生一次瞬时 Focused(false)，再推进一代使其失效。
    POPUP_GENERATION.fetch_add(1, Ordering::AcqRel);
    let _ = app.emit("popup-shown", ());
    hide_tao_flash_windows();

    // 兜底：呼出后立刻切走窗口时可能从未获得焦点（无失焦事件），
    // 200ms 后复查——仍可见且无焦点则自动收起
    let a = app.clone();
    let gen = POPUP_GENERATION.load(Ordering::Acquire);
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(200));
        if POPUP_GENERATION.load(Ordering::Acquire) == gen {
            use tauri::Manager;
            if let Some(w) = a.get_webview_window("popup") {
                if POPUP_VISIBLE.load(Ordering::Acquire) && !w.is_focused().unwrap_or(true) {
                    hide_popup(&a);
                }
            }
        }
    });
}

/// 隐藏弹窗（内部实现）：通知前端播放退场动画（~160ms）后真正隐藏窗口；
/// 退场期间若被重新呼出（世代变化）则放弃隐藏。
pub fn hide_popup(app: &AppHandle) {
    use tauri::Manager;
    POPUP_GENERATION.fetch_add(1, Ordering::AcqRel);
    let gen = POPUP_GENERATION.load(Ordering::Acquire);
    let Some(w) = app.get_webview_window("popup") else {
        return;
    };
    POPUP_VISIBLE.store(false, Ordering::Release);
    let _ = w.set_ignore_cursor_events(true); // 退场期间不再响应鼠标
    let _ = app.emit("popup-hiding", ());
    let w2 = w.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(160));
        if POPUP_GENERATION.load(Ordering::Acquire) == gen
            && !POPUP_VISIBLE.load(Ordering::Acquire)
        {
            let _ = w2.hide();
        }
    });
}

/// 呼出/隐藏弹窗（主线程调用）。
pub fn toggle_popup(app: &AppHandle) {
    if popup_visible() {
        hide_popup(app);
    } else {
        show_popup_inner(app);
    }
}

/// 仅供外部（托盘/单实例）呼出。
pub fn show_popup(app: &AppHandle) {
    use tauri::Manager;
    if popup_visible() {
        if let Some(w) = app.get_webview_window("popup") {
            let _ = w.set_focus();
        }
        return;
    }
    show_popup_inner(app);
}
