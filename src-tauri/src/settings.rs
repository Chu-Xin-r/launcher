//! 应用设置：读写 settings.json + 开机自启（HKCU Run 注册表）。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AppSettings {
    /// 开机自启
    #[serde(default)]
    pub autostart: bool,
    /// 双击 Ctrl 呼出（仅在 hotkey_mode = double_ctrl 时生效）
    #[serde(default = "default_true")]
    pub double_ctrl: bool,
    /// 呼出快捷键模式："double_ctrl" | "custom"
    #[serde(default = "default_hotkey_mode")]
    pub hotkey_mode: String,
    /// 自定义快捷键，如 "alt+space"、"ctrl+alt+l"
    #[serde(default = "default_custom_hotkey")]
    pub custom_hotkey: String,
    /// 主题："auto" | "dark" | "light"
    #[serde(default)]
    pub theme: String,
    /// 禁用的磁盘盘符
    #[serde(default)]
    pub disabled_volumes: Vec<char>,
    /// 排除目录（绝对路径前缀，不区分大小写）
    #[serde(default)]
    pub excluded_dirs: Vec<String>,
}

fn default_true() -> bool {
    true
}
fn default_hotkey_mode() -> String {
    "double_ctrl".into()
}
fn default_custom_hotkey() -> String {
    "alt+space".into()
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            autostart: false,
            double_ctrl: true,
            hotkey_mode: default_hotkey_mode(),
            custom_hotkey: default_custom_hotkey(),
            theme: "auto".into(),
            disabled_volumes: Vec::new(),
            excluded_dirs: Vec::new(),
        }
    }
}

pub fn settings_file() -> PathBuf {
    std::env::var("LOCALAPPDATA")
        .map(|p| PathBuf::from(p).join("launcher").join("settings.json"))
        .unwrap_or_else(|_| PathBuf::from("settings.json"))
}

pub fn load_settings() -> AppSettings {
    std::fs::read_to_string(settings_file())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_settings(s: &AppSettings) -> Result<(), String> {
    let json = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    std::fs::write(settings_file(), json).map_err(|e| e.to_string())
}

const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "com.chuxinr.launcher";
const THEME_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";

fn to_pcw(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 读注册表字符串值（进程内直读，不产生子进程/控制台窗口）
pub fn reg_get_string(subkey: &str, value: &str) -> Option<String> {
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};
    let sk = to_pcw(subkey);
    let vn = to_pcw(value);
    let mut buf = [0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    unsafe {
        let r = RegGetValueW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(sk.as_ptr()),
            windows::core::PCWSTR(vn.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut size),
        );
        if r.is_err() {
            return None;
        }
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(0);
    Some(String::from_utf16_lossy(&buf[..len]))
}

/// 读注册表 DWORD 值
pub fn reg_get_dword(subkey: &str, value: &str) -> Option<u32> {
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let sk = to_pcw(subkey);
    let vn = to_pcw(value);
    let mut out: u32 = 0;
    let mut size = 4u32;
    unsafe {
        let r = RegGetValueW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(sk.as_ptr()),
            windows::core::PCWSTR(vn.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut out as *mut _ as *mut _),
            Some(&mut size),
        );
        if r.is_err() {
            return None;
        }
    }
    Some(out)
}

fn reg_set_string(subkey: &str, value: &str, data: &str) -> Result<(), String> {
    use windows::Win32::System::Registry::{
        RegOpenKeyExW, RegSetValueExW, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ,
    };
    let sk = to_pcw(subkey);
    let vn = to_pcw(value);
    let mut data_u16 = to_pcw(data);
    let mut hk = windows::Win32::System::Registry::HKEY::default();
    unsafe {
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(sk.as_ptr()),
            0,
            KEY_SET_VALUE,
            &mut hk,
        )
        .is_err()
        {
            return Err("打开注册表失败".into());
        }
        let bytes: &[u8] = std::slice::from_raw_parts(
            data_u16.as_ptr() as *const u8,
            data_u16.len() * 2,
        );
        if RegSetValueExW(hk, windows::core::PCWSTR(vn.as_ptr()), 0, REG_SZ, Some(bytes))
            .is_err()
        {
            return Err("写入注册表失败".into());
        }
    }
    Ok(())
}

fn reg_delete_value(subkey: &str, value: &str) -> Result<(), String> {
    use windows::Win32::System::Registry::{
        RegDeleteValueW, RegOpenKeyExW, HKEY_CURRENT_USER, KEY_SET_VALUE,
    };
    let sk = to_pcw(subkey);
    let vn = to_pcw(value);
    let mut hk = windows::Win32::System::Registry::HKEY::default();
    unsafe {
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(sk.as_ptr()),
            0,
            KEY_SET_VALUE,
            &mut hk,
        )
        .is_err()
        {
            return Err("打开注册表失败".into());
        }
        if RegDeleteValueW(hk, windows::core::PCWSTR(vn.as_ptr())).is_err() {
            return Err("删除注册表值失败".into());
        }
    }
    Ok(())
}

/// 查询当前是否已配置开机自启。
pub fn autostart_enabled() -> bool {
    reg_get_string(RUN_SUBKEY, RUN_VALUE).is_some_and(|s| !s.is_empty())
}

/// 设置/取消开机自启（写入当前 exe 路径）。
pub fn set_autostart(enable: bool) -> Result<(), String> {
    if enable {
        let exe = std::env::current_exe()
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .to_string();
        reg_set_string(RUN_SUBKEY, RUN_VALUE, &exe)
    } else {
        reg_delete_value(RUN_SUBKEY, RUN_VALUE)
            .map_err(|e| format!("{e}（可能本就未启用）"))
    }
}

/// Windows 应用主题是否为浅色（读 AppsUseLightTheme，进程内直读）。
pub fn apps_use_light_theme() -> Option<bool> {
    reg_get_dword(THEME_SUBKEY, "AppsUseLightTheme").map(|v| v == 1)
}

/// 排除目录是否命中（前缀匹配，`/` 与 `\` 等价，大小写不敏感）。
pub fn is_excluded(path: &str, excluded: &[String]) -> bool {
    if excluded.is_empty() {
        return false;
    }
    let lower = path.replace('/', "\\").to_ascii_lowercase();
    excluded
        .iter()
        .any(|e| lower.starts_with(&e.replace('/', "\\").to_ascii_lowercase()))
}
