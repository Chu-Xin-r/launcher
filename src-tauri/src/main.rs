//! launcher：Listary 风格毫秒级文件搜索启动器。
//!
//! 启动流程：托盘 + 隐藏弹窗 → 引擎后台建索引（快照秒启 / 全量构建）→
//! 双击 Ctrl 呼出弹窗即输即搜。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod hotkey;
mod settings;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, Manager, WindowEvent};

use commands::AppState;
use searchcore::engine::{Engine, EngineStats};

#[derive(Serialize, Clone)]
pub struct IndexReadyPayload {
    pub entries: usize,
    pub startup_ms: u128,
    pub from_snapshot: bool,
    pub is_admin: bool,
    pub usn_live: Vec<char>,
}

impl From<&EngineStats> for IndexReadyPayload {
    fn from(s: &EngineStats) -> Self {
        Self {
            entries: s.entries,
            startup_ms: s.startup_ms,
            from_snapshot: s.from_snapshot,
            is_admin: s.is_admin,
            usn_live: s.usn_live.clone(),
        }
    }
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            hotkey::show_popup(app);
        }))
        .setup(|app| {
            let handle = app.handle().clone();

            // 托盘
            setup_tray(&handle)?;

            // 引擎：先注册状态（搜索立即可用、返回空），后台线程建索引
            let engine = Engine::new();
            let cfg = settings::load_settings();
            let (custom_active, hotkey_str) = match cfg.hotkey_mode.as_str() {
                "alt_space" => (true, "alt+space".to_string()),
                "custom" => (true, cfg.custom_hotkey.clone()),
                _ => (false, String::new()),
            };
            hotkey::set_double_ctrl_enabled(if custom_active { false } else { cfg.double_ctrl });
            hotkey::set_custom_hotkey(custom_active, &hotkey_str);
            app.manage(AppState {
                engine: Arc::clone(&engine),
                icon_cache: Mutex::new(std::collections::HashMap::new()),
                usage: Mutex::new(commands::load_usage()),
                excluded: Mutex::new(commands::normalize_excluded(&cfg.excluded_dirs)),
            });

            let h2 = handle.clone();
            std::thread::spawn(move || {
                let stats = engine.startup();
                let _ = h2.emit(
                    "index-ready",
                    serde_json::to_value(IndexReadyPayload::from(&stats)).unwrap_or_default(),
                );
            });

            // 双击 Ctrl 热键
            hotkey::run(handle.clone());

            // 弹窗常驻改为透明隐藏方案：初始化为透明+鼠标穿透（无 show/hide 闪烁）
            if let Some(w) = app.get_webview_window("popup") {
                hotkey::init_hidden(&w);
            }

            // 开机自启自愈：任务计划指向当前 exe（防止指向旧路径/调试版）
            settings::heal_autostart_if_enabled();

            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::Focused(false) = event {
                if window.label() == "popup" {
                    let w = window.clone();
                    let app = window.app_handle().clone();
                    let generation = hotkey::popup_generation();
                    // 保护期内的失焦（呼出瞬间被中间窗口抢焦点）走慢速兜底复查，
                    // 保护期外的失焦（用户点开其他窗口）走 120ms 快速隐藏。
                    let fast = !hotkey::popup_focus_guard_active();
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_millis(if fast { 120 } else { 950 }));
                        if hotkey::popup_generation() == generation
                            && hotkey::popup_visible()
                            && !w.is_focused().unwrap_or(true)
                        {
                            // 走透明度隐藏，禁止真 hide（真隐藏后透明度呼出会失效）
                            hotkey::hide_popup(&app);
                        }
                    });
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::search,
            commands::open_path,
            commands::reveal_path,
            commands::copy_path,
            commands::get_icon,
            commands::hide_window,
            commands::engine_status,
            commands::get_settings,
            commands::set_settings,
            commands::open_settings,
            commands::get_windows_theme,
        ])
        .run(tauri::generate_context!())
        .expect("launcher 运行失败");
}

fn setup_tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "显示启动器 (双击 Ctrl)", true, None::<&str>)?;
    let conf = MenuItem::with_id(app, "settings", "设置", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &conf, &quit])?;

    let mut tray = TrayIconBuilder::with_id("main-tray");
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray
        .tooltip("启动器 · 双击 Ctrl 呼出")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, ev| match ev.id().as_ref() {
            "show" => hotkey::show_popup(app),
            "settings" => commands::open_settings(app.clone()),
            "quit" => {
                // 后台线程收尾（保存快照），立即请求退出；
                // 另设 1.5s 硬杀兜底，防止优雅退出路径被卡住导致退不出去
                if let Some(state) = app.try_state::<AppState>() {
                    let engine = Arc::clone(&state.engine);
                    std::thread::spawn(move || engine.shutdown());
                }
                std::thread::spawn(|| {
                    std::thread::sleep(std::time::Duration::from_millis(1500));
                    std::process::exit(0);
                });
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, ev| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = ev
            {
                hotkey::show_popup(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}
