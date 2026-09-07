//! IPC 命令：搜索 / 打开 / 定位 / 复制路径 / 文件图标 / 引擎状态。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};
use windows::core::PCWSTR;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
use windows::Win32::UI::Shell::{ShellExecuteW, SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_USEFILEATTRIBUTES};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, SW_SHOWNORMAL};

use searchcore::engine::Engine;
use searchcore::matcher::SearchOptions;

pub struct AppState {
    pub engine: Arc<Engine>,
    pub icon_cache: Mutex<HashMap<String, Option<CachedIcon>>>,
    /// 路径 → 打开次数（使用频次排名）
    pub usage: Mutex<HashMap<String, u32>>,
    /// 归一化后的排除目录前缀（搜索结果兜底过滤）
    pub excluded: Mutex<Vec<String>>,
}

pub struct CachedIcon {
    pub w: i32,
    pub h: i32,
    pub rgba: Vec<u8>,
}

#[derive(Serialize, Clone)]
pub struct ResultDto {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub score: i32,
    pub match_start: u16,
    pub match_len: u16,
}

#[derive(Serialize, Clone)]
pub struct IconDto {
    pub w: i32,
    pub h: i32,
    pub rgba: Vec<u8>,
}

fn to_pcw(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// —— 使用频次：path → 打开次数，落盘 usage.json ——

pub fn usage_file() -> PathBuf {
    std::env::var("LOCALAPPDATA")
        .map(|p| PathBuf::from(p).join("launcher").join("usage.json"))
        .unwrap_or_else(|_| PathBuf::from("usage.json"))
}

pub fn load_usage() -> HashMap<String, u32> {
    std::fs::read_to_string(usage_file())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_usage(usage: &HashMap<String, u32>) {
    if let Ok(s) = serde_json::to_string(usage) {
        let _ = std::fs::write(usage_file(), s);
    }
}

pub fn record_use(state: &AppState, path: &str) {
    {
        let mut usage = state.usage.lock().unwrap();
        *usage.entry(path.to_string()).or_insert(0) += 1;
        save_usage(&usage);
    }
}

/// 频次加权重排：常用项最多 +1500 分（≈ 一个词首命中的量级，不会压过精确匹配）。
fn apply_usage_boost(state: &AppState, items: &mut Vec<ResultDto>) {
    let usage = state.usage.lock().unwrap();
    if usage.is_empty() {
        return;
    }
    for r in items.iter_mut() {
        if let Some(&c) = usage.get(&r.path) {
            r.score += (c as i32 * 250).min(1_500);
        }
    }
    drop(usage);
    items.sort_unstable_by(|a, b| b.score.cmp(&a.score));
}

#[tauri::command]
pub fn search(query: String, state: State<'_, AppState>) -> Vec<ResultDto> {
    if query.trim().is_empty() {
        return Vec::new();
    }
    let mut items: Vec<ResultDto> = state
        .engine
        .search(&query, &SearchOptions { limit: 50, ..Default::default() })
        .items
        .into_iter()
        .map(|r| ResultDto {
            name: r.name,
            path: r.path,
            is_dir: r.is_dir,
            score: r.score,
            match_start: r.match_start,
            match_len: r.match_len,
        })
        .collect();
    // 排除目录兜底过滤（索引增量阶段新文件也会被挡住）
    {
        let excluded = state.excluded.lock().unwrap();
        if !excluded.is_empty() {
            items.retain(|r| !crate::settings::is_excluded(&r.path, &excluded));
        }
    }
    apply_usage_boost(&state, &mut items);
    items
}

#[tauri::command]
pub fn open_path(path: String, state: State<'_, AppState>) -> Result<(), String> {
    unsafe {
        let p = to_pcw(&path);
        let verb = to_pcw("open");
        let r = ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(p.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        );
        if r.0 as isize <= 32 {
            return Err(format!("打开失败: {path}"));
        }
    }
    record_use(&state, &path);
    Ok(())
}

#[tauri::command]
pub fn reveal_path(path: String) -> Result<(), String> {
    std::process::Command::new("explorer")
        .arg(format!("/select,{path}"))
        .spawn()
        .map_err(|e| format!("打开文件夹失败: {e}"))?;
    Ok(())
}

#[tauri::command]
pub fn copy_path(path: String) -> Result<(), String> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

    const CF_UNICODETEXT: u32 = 13;
    unsafe {
        OpenClipboard(HWND::default()).map_err(|e| format!("打开剪贴板失败: {e}"))?;
        let ok = (|| -> Result<(), String> {
            EmptyClipboard().map_err(|e| format!("清空剪贴板失败: {e}"))?;
            let mut chars: Vec<u16> = path.encode_utf16().collect();
            chars.push(0);
            let bytes = chars.len() * 2;
            let h = GlobalAlloc(GMEM_MOVEABLE, bytes).map_err(|e| e.to_string())?;
            let dst = GlobalLock(h) as *mut u16;
            if dst.is_null() {
                return Err("GlobalLock 失败".into());
            }
            std::ptr::copy_nonoverlapping(chars.as_ptr(), dst, chars.len());
            let _ = GlobalUnlock(h);
            if SetClipboardData(CF_UNICODETEXT, HANDLE(h.0)).is_err() {
                return Err("写入剪贴板失败".into());
            }
            Ok(())
        })();
        let _ = CloseClipboard();
        ok
    }
}

#[tauri::command]
pub fn hide_window(app: AppHandle) {
    crate::hotkey::hide_popup(&app);
}

#[tauri::command]
pub fn engine_status(state: State<'_, AppState>) -> serde_json::Value {
    let st = state.engine.stats();
    serde_json::json!({
        "entries": st.entries,
        "volumes": st.volumes,
        "fromSnapshot": st.from_snapshot,
        "startupMs": st.startup_ms,
        "isAdmin": st.is_admin,
        "usnLive": st.usn_live,
        "ready": st.entries > 0,
    })
}

// —— 设置 ——

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SettingsDto {
    pub autostart: bool,
    pub double_ctrl: bool,
    pub hotkey_mode: String,
    pub custom_hotkey: String,
    pub theme: String,
    pub disabled_volumes: Vec<char>,
    pub excluded_dirs: Vec<String>,
}

/// 归一化排除目录：反斜杠、小写、去尾分隔符。
pub fn normalize_excluded(dirs: &[String]) -> Vec<String> {
    dirs.iter()
        .map(|d| {
            d.replace('/', "\\")
                .trim_end_matches('\\')
                .to_ascii_lowercase()
        })
        .filter(|d| !d.is_empty())
        .collect()
}

#[derive(Serialize)]
pub struct VolumeDto {
    pub letter: char,
    pub is_ntfs: bool,
    pub entries: usize,
    pub enabled: bool,
}

#[derive(Serialize)]
pub struct SettingsPayload {
    #[serde(flatten)]
    pub settings: SettingsDto,
    pub autostart_registered: bool,
    pub volumes: Vec<VolumeDto>,
}

#[tauri::command]
pub fn get_settings(state: State<'_, AppState>) -> SettingsPayload {
    let s = crate::settings::load_settings();
    let st = state.engine.stats();
    SettingsPayload {
        autostart_registered: crate::settings::autostart_enabled(),
        volumes: st
            .volumes
            .iter()
            .map(|(letter, is_ntfs, entries)| VolumeDto {
                letter: *letter,
                is_ntfs: *is_ntfs,
                entries: *entries,
                enabled: !s.disabled_volumes.contains(letter),
            })
            .collect(),
        settings: SettingsDto {
            autostart: s.autostart,
            double_ctrl: s.double_ctrl,
            hotkey_mode: s.hotkey_mode,
            custom_hotkey: s.custom_hotkey,
            theme: s.theme,
            disabled_volumes: s.disabled_volumes,
            excluded_dirs: s.excluded_dirs,
        },
    }
}

#[tauri::command]
pub fn set_settings(
    settings: SettingsDto,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<(), String> {
    use tauri::Emitter;
    let old = crate::settings::load_settings();

    let normalized = normalize_excluded(&settings.excluded_dirs);
    let mut s = settings.clone();
    s.excluded_dirs = settings.excluded_dirs.clone();
    s.disabled_volumes = settings
        .disabled_volumes
        .iter()
        .map(|c| c.to_ascii_uppercase())
        .collect();

    let config_changed = s.disabled_volumes != old.disabled_volumes
        || normalize_excluded(&s.excluded_dirs) != normalize_excluded(&old.excluded_dirs);

    // 开机自启
    if s.autostart != old.autostart {
        crate::settings::set_autostart(s.autostart)?;
    }

    // 呼出快捷键：全部由低级钩子拦截（可吞系统快捷键）
    let (custom_active, hotkey_str) = match s.hotkey_mode.as_str() {
        "alt_space" => (true, "alt+space".to_string()),
        "custom" => (true, s.custom_hotkey.trim().to_string()),
        _ => (false, String::new()),
    };
    crate::hotkey::set_double_ctrl_enabled(if custom_active { false } else { s.double_ctrl });
    crate::hotkey::set_custom_hotkey(custom_active, &hotkey_str);

    crate::settings::save_settings(&crate::settings::AppSettings {
        autostart: s.autostart,
        double_ctrl: s.double_ctrl,
        hotkey_mode: s.hotkey_mode.clone(),
        custom_hotkey: s.custom_hotkey.clone(),
        theme: s.theme.clone(),
        disabled_volumes: s.disabled_volumes.clone(),
        excluded_dirs: s.excluded_dirs.clone(),
    })?;
    *state.excluded.lock().unwrap() = normalized;

    // 磁盘/排除目录变更 → 后台重建索引
    if config_changed {
        let engine = Arc::clone(&state.engine);
        let disabled = s.disabled_volumes.clone();
        let excluded = normalize_excluded(&s.excluded_dirs);
        std::thread::spawn(move || {
            let stats = engine.rebuild(&disabled, &excluded);
            let payload = crate::IndexReadyPayload::from(&stats);
            let _ = app.emit(
                "index-ready",
                serde_json::to_value(payload).unwrap_or_default(),
            );
        });
    }
    Ok(())
}

/// 打开设置视图（复用弹窗 WebView，避免多 WebView2 窗口的环境兼容问题）。
#[tauri::command]
pub fn open_settings(app: AppHandle) {
    use tauri::Emitter;
    crate::hotkey::show_popup(&app);
    let _ = app.emit("settings-view", ());
}

/// 读取 Windows 应用主题（AppsUseLightTheme 注册表）：返回 "light" / "dark"。
#[tauri::command]
pub fn get_windows_theme() -> String {
    match crate::settings::apps_use_light_theme() {
        Some(false) => "dark".to_string(),
        _ => "light".to_string(),
    }
}

/// 按扩展名取文件图标（RGBA）。`key`：扩展名（含点，如 ".pdf"）或 "dir"。
/// `path` 非空且为 .exe/.lnk 时提取该文件的真实应用图标，缓存按完整路径。
#[tauri::command]
pub fn get_icon(key: String, path: String, state: State<'_, AppState>) -> Result<IconDto, String> {
    let lower = path.to_ascii_lowercase();
    let is_app = lower.ends_with(".exe") || lower.ends_with(".lnk");
    let mut cache = state.icon_cache.lock().unwrap();
    if cache.len() > 2048 {
        cache.clear(); // 真实路径缓存防膨胀
    }
    let cache_key = if is_app { format!("p:{lower}") } else { key.clone() };
    let cached = cache.entry(cache_key).or_insert_with(|| {
        if is_app {
            extract_icon_from_path(&path).or_else(|| extract_icon(&key))
        } else {
            extract_icon(&key)
        }
    });
    match cached {
        Some(c) => Ok(IconDto {
            w: c.w,
            h: c.h,
            rgba: c.rgba.clone(),
        }),
        None => Err("无图标".into()),
    }
}

/// 初始化本线程 COM（Shell 图标提取依赖），已初始化则忽略。
fn init_com() {
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

/// 从真实文件路径提取图标（exe 显示应用自身图标，lnk 由 Shell 解析目标图标）。
fn extract_icon_from_path(path: &str) -> Option<CachedIcon> {
    init_com();
    unsafe {
        let p = to_pcw(path);
        let mut info = SHFILEINFOW::default();
        let r = SHGetFileInfoW(
            PCWSTR(p.as_ptr()),
            FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        );
        if r == 0 || info.hIcon.is_invalid() {
            return None;
        }
        let icon = info.hIcon;
        let out = icon_to_rgba(icon);
        let _ = DestroyIcon(icon);
        out
    }
}

fn extract_icon(key: &str) -> Option<CachedIcon> {
    unsafe {
        let (path, attrs) = if key == "dir" {
            (String::from(r"C:\Program Files\launcher-fake-dir"), 0x10u32)
        } else {
            (format!(r"C:\fake\file{}", key.to_ascii_lowercase()), 0x80u32)
        };
        let p = to_pcw(&path);
        let mut info = SHFILEINFOW::default();
        let r = SHGetFileInfoW(
            PCWSTR(p.as_ptr()),
            FILE_FLAGS_AND_ATTRIBUTES(attrs),
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON | SHGFI_USEFILEATTRIBUTES,
        );
        if r == 0 || info.hIcon.is_invalid() {
            return None;
        }
        let icon = info.hIcon;
        let out = icon_to_rgba(icon);
        let _ = DestroyIcon(icon);
        out
    }
}

/// HICON → 32 位 RGBA（GetIconInfo + GetDIBits）
fn icon_to_rgba(icon: windows::Win32::UI::WindowsAndMessaging::HICON) -> Option<CachedIcon> {
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetDC, GetObjectW, ReleaseDC,
        BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetIconInfo, ICONINFO};

    unsafe {
        let mut ii = ICONINFO::default();
        GetIconInfo(icon, &mut ii).ok()?;
        let mut bm = BITMAP::default();
        if GetObjectW(
            ii.hbmColor,
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut _ as *mut _),
        ) == 0
        {
            let _ = DeleteObject(ii.hbmColor);
            let _ = DeleteObject(ii.hbmMask);
            return None;
        }
        let (w, h) = (bm.bmWidth, bm.bmHeight);
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let mut bi = BITMAPINFO::default();
        bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = w;
        bi.bmiHeader.biHeight = -h; // 自顶向下
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = 0; // BI_RGB
        let hdc = CreateCompatibleDC(None);
        let got = GetDIBits(
            hdc,
            ii.hbmColor,
            0,
            h as u32,
            Some(buf.as_mut_ptr() as *mut _),
            &mut bi,
            DIB_RGB_COLORS,
        );
        let hdc_screen = GetDC(None);
        let _ = ReleaseDC(None, hdc_screen);
        let _ = DeleteDC(hdc);
        let _ = DeleteObject(ii.hbmColor);
        let _ = DeleteObject(ii.hbmMask);
        if got == 0 {
            return None;
        }
        // BGRA → RGBA，alpha 全零时按不透明处理（老图标）
        let mut has_alpha = false;
        for i in (3..buf.len()).step_by(4) {
            if buf[i] != 0 {
                has_alpha = true;
                break;
            }
        }
        for chunk in buf.chunks_exact_mut(4) {
            let b = chunk[0];
            chunk[0] = chunk[2];
            chunk[2] = b;
            if !has_alpha {
                chunk[3] = 255;
            }
        }
        Some(CachedIcon { w, h, rgba: buf })
    }
}
