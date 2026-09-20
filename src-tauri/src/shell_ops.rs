//! 右键菜单动作：终端打开 / 管理员运行 / 复制文件 / 属性 / 打开方式。
//!
//! 发布版以管理员身份运行：普通（非管理员）终端通过 explorer.exe 的
//! 用户令牌降权启动（CreateProcessWithTokenW），管理员终端走
//! ShellExecuteW "runas"（已提权则直接继承，未提权则弹 UAC）。

use std::ffi::c_void;
use std::path::{Path, PathBuf};

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows::Win32::Security::{
    DuplicateTokenEx, GetTokenInformation, SecurityImpersonation, TokenElevation, TokenPrimary,
    TOKEN_ALL_ACCESS, TOKEN_DUPLICATE, TOKEN_ELEVATION, TOKEN_QUERY,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    CreateProcessW, CreateProcessWithTokenW, GetCurrentProcess, OpenProcess, OpenProcessToken,
    CREATE_NEW_CONSOLE, CREATE_UNICODE_ENVIRONMENT, LOGON_WITH_PROFILE, PROCESS_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, STARTUPINFOW,
};
use windows::Win32::UI::Shell::{
    ShellExecuteExW, ShellExecuteW, SHOpenWithDialog, OAIF_ALLOW_REGISTRATION, OAIF_EXEC,
    OPENASINFO, SEE_MASK_INVOKEIDLIST, SHELLEXECUTEINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetShellWindow, GetWindowThreadProcessId, SW_SHOW, SW_SHOWNORMAL,
};

fn to_pcw(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 目标目录：目录取自身，文件取所在目录。
fn target_dir(path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_dir() {
        return p.to_path_buf();
    }
    match p.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// 系统程序完整路径（不依赖 PATH）。
fn system_exe(name: &str) -> String {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
    match name {
        "cmd" => format!("{root}\\System32\\cmd.exe"),
        "powershell" => format!("{root}\\System32\\WindowsPowerShell\\v1.0\\powershell.exe"),
        _ => name.to_string(),
    }
}

/// 当前进程是否处于提权（高完整性）状态。
fn is_elevated() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation: TOKEN_ELEVATION = std::mem::zeroed();
        let mut ret = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut TOKEN_ELEVATION as *mut c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

/// 找当前交互会话 shell（explorer.exe）的 PID。
fn find_explorer_pid() -> Option<u32> {
    unsafe {
        // 首选：桌面 Shell 窗口宿主进程
        let shell = GetShellWindow();
        if !shell.0.is_null() {
            let mut pid = 0u32;
            GetWindowThreadProcessId(shell, Some(&mut pid));
            if pid != 0 {
                return Some(pid);
            }
        }
        // 兜底：进程快照枚举 explorer.exe
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut found = None;
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                if String::from_utf16_lossy(&entry.szExeFile[..len]).eq_ignore_ascii_case("explorer.exe")
                {
                    found = Some(entry.th32ProcessID);
                    break;
                }
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        found
    }
}

/// 直接启动（继承当前进程权限；显式新建控制台窗口）。
///
/// 关键：不继承调用方句柄（bInheritHandles=FALSE）。若走 Rust Command 的
/// 句柄继承，启动器从脚本/管道环境启动时会把被重定向的 std 句柄传给子进程，
/// cmd.exe 会因 stdin 立即读到 EOF 而秒退（无窗口）。此写法与降权启动同构。
fn spawn_direct(exe: &str, dir: &Path) -> Result<(), String> {
    unsafe {
        let mut cmdline: Vec<u16> = format!("\"{exe}\"")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let dir_w = to_pcw(&dir.to_string_lossy());
        let dir_ptr = if dir.is_dir() {
            PCWSTR(dir_w.as_ptr())
        } else {
            PCWSTR::null()
        };
        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let r = CreateProcessW(
            PCWSTR::null(),
            PWSTR(cmdline.as_mut_ptr()),
            None,
            None,
            false,
            CREATE_NEW_CONSOLE | CREATE_UNICODE_ENVIRONMENT,
            None,
            dir_ptr,
            &si,
            &mut pi,
        );
        if r.is_ok() {
            let _ = CloseHandle(pi.hProcess);
            let _ = CloseHandle(pi.hThread);
        }
        r.map_err(|e| format!("启动失败: {e}"))
    }
}

/// 降权启动：借 explorer 的用户令牌拉起中完整性进程。
fn spawn_unelevated(exe: &str, dir: &Path) -> Result<(), String> {
    unsafe {
        let pid = find_explorer_pid().ok_or("未找到 explorer.exe")?;
        let proc = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
            .map_err(|e| format!("打开 explorer 进程失败: {e}"))?;
        let mut token = HANDLE::default();
        OpenProcessToken(proc, TOKEN_DUPLICATE | TOKEN_QUERY, &mut token)
            .map_err(|e| format!("读取 explorer 令牌失败: {e}"))?;
        let mut dup = HANDLE::default();
        let r = DuplicateTokenEx(
            token,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut dup,
        );
        let _ = CloseHandle(token);
        let _ = CloseHandle(proc);
        r.map_err(|e| format!("复制令牌失败: {e}"))?;

        let mut cmdline: Vec<u16> = format!("\"{exe}\"")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let dir_w = to_pcw(&dir.to_string_lossy());
        let dir_ptr = if dir.is_dir() {
            PCWSTR(dir_w.as_ptr())
        } else {
            PCWSTR::null()
        };
        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let r = CreateProcessWithTokenW(
            dup,
            LOGON_WITH_PROFILE,
            PCWSTR::null(),
            PWSTR(cmdline.as_mut_ptr()),
            CREATE_NEW_CONSOLE | CREATE_UNICODE_ENVIRONMENT,
            None,
            dir_ptr,
            &si,
            &mut pi,
        );
        let _ = CloseHandle(dup);
        if r.is_ok() {
            let _ = CloseHandle(pi.hProcess);
            let _ = CloseHandle(pi.hThread);
        }
        r.map_err(|e| format!("降权启动失败: {e}"))
    }
}

/// 提权启动（runas；未提权时弹 UAC，已提权直接继承）。
fn shell_runas(exe: &str, dir: &Path, params: Option<&str>) -> Result<(), String> {
    unsafe {
        let verb = to_pcw("runas");
        let file = to_pcw(exe);
        let p = params.map(to_pcw);
        let d = to_pcw(&dir.to_string_lossy());
        let dir_ptr = if dir.is_dir() {
            PCWSTR(d.as_ptr())
        } else {
            PCWSTR::null()
        };
        let r = ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            p.as_ref().map_or(PCWSTR::null(), |v| PCWSTR(v.as_ptr())),
            dir_ptr,
            SW_SHOWNORMAL,
        );
        if (r.0 as isize) <= 32 {
            return Err(format!(
                "提权启动失败（错误码 {}，可能被取消或权限不足）",
                r.0 as isize
            ));
        }
        Ok(())
    }
}

/// 在终端中打开目标所在目录（kind: cmd / powershell / cmd_admin / powershell_admin）。
#[tauri::command]
pub fn open_in_terminal(path: String, kind: String) -> Result<(), String> {
    let dir = target_dir(&path);
    let as_admin = kind.ends_with("_admin");
    let base = kind.trim_end_matches("_admin");
    let exe = match base {
        "cmd" => system_exe("cmd"),
        "powershell" => system_exe("powershell"),
        _ => return Err(format!("未知终端类型: {kind}")),
    };
    let dir_str = dir.to_string_lossy().to_string();
    if as_admin {
        if is_elevated() {
            // 已提权：子进程天然继承管理员，无需 UAC
            spawn_direct(&exe, &dir)
        } else {
            // 未提权：runas 提权，并用参数兜底工作目录
            let params = match base {
                "cmd" => format!("/s /k pushd \"{dir_str}\""),
                _ => format!(
                    "-NoLogo -NoExit -Command Set-Location -LiteralPath '{}'",
                    dir_str.replace('\'', "''")
                ),
            };
            shell_runas(&exe, &dir, Some(&params))
        }
    } else if is_elevated() {
        // 提权进程下启动普通终端：先降权，失败则回退直接启动
        spawn_unelevated(&exe, &dir).or_else(|_| spawn_direct(&exe, &dir))
    } else {
        spawn_direct(&exe, &dir)
    }
}

/// 以管理员身份运行（exe / lnk / bat 等）。
#[tauri::command]
pub fn run_as_admin(path: String) -> Result<(), String> {
    let dir = target_dir(&path);
    shell_runas(&path, &dir, None)
}

/// 复制文件到剪贴板（CF_HDROP 文件列表，可在资源管理器中直接粘贴）。
#[tauri::command]
pub fn copy_file(path: String) -> Result<(), String> {
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

    const CF_HDROP: u32 = 15;

    #[repr(C)]
    struct DropFiles {
        p_files: u32,
        pt_x: i32,
        pt_y: i32,
        f_nc: i32,
        f_wide: i32,
    }

    unsafe {
        OpenClipboard(HWND::default()).map_err(|e| format!("打开剪贴板失败: {e}"))?;
        let ok = (|| -> Result<(), String> {
            EmptyClipboard().map_err(|e| format!("清空剪贴板失败: {e}"))?;
            let mut wide: Vec<u16> = path.encode_utf16().collect();
            wide.push(0); // 路径结尾
            wide.push(0); // 文件列表结尾（双 NUL）
            let header = std::mem::size_of::<DropFiles>();
            let bytes = header + wide.len() * 2;
            let h = GlobalAlloc(GMEM_MOVEABLE, bytes).map_err(|e| e.to_string())?;
            let dst = GlobalLock(h) as *mut u8;
            if dst.is_null() {
                return Err("GlobalLock 失败".into());
            }
            let df = DropFiles {
                p_files: header as u32,
                pt_x: 0,
                pt_y: 0,
                f_nc: 0,
                f_wide: 1,
            };
            std::ptr::copy_nonoverlapping(&df as *const DropFiles as *const u8, dst, header);
            std::ptr::copy_nonoverlapping(wide.as_ptr() as *const u8, dst.add(header), wide.len() * 2);
            let _ = GlobalUnlock(h);
            if SetClipboardData(CF_HDROP, HANDLE(h.0)).is_err() {
                return Err("写入剪贴板失败".into());
            }
            Ok(())
        })();
        let _ = CloseClipboard();
        ok
    }
}

/// 系统「属性」对话框。
#[tauri::command]
pub fn show_properties(path: String) -> Result<(), String> {
    unsafe {
        let verb = to_pcw("properties");
        let file = to_pcw(&path);
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_INVOKEIDLIST;
        info.lpVerb = PCWSTR(verb.as_ptr());
        info.lpFile = PCWSTR(file.as_ptr());
        info.nShow = SW_SHOW.0;
        ShellExecuteExW(&mut info).map_err(|e| format!("打开属性失败: {e}"))?;
    }
    Ok(())
}

/// 「打开方式」对话框。
#[tauri::command]
pub fn open_with_dialog(path: String) -> Result<(), String> {
    unsafe {
        let file = to_pcw(&path);
        let info = OPENASINFO {
            pcszFile: PCWSTR(file.as_ptr()),
            pcszClass: PCWSTR::null(),
            oaifInFlags: OAIF_ALLOW_REGISTRATION | OAIF_EXEC,
        };
        if let Err(e) = SHOpenWithDialog(HWND::default(), &info) {
            // 用户取消（ERROR_CANCELLED）不算失败
            if e.code().0 as u32 != 0x8007_04C7 {
                return Err(format!("打开方式失败: {e}"));
            }
        }
    }
    Ok(())
}
