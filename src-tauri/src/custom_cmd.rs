//! 自定义命令：存储 / 匹配 / 执行。
//!
//! 命令与设置分离存放（`%LOCALAPPDATA%\launcher\commands.json`），
//! 目的是可以整份导出、贴给别人、一键导入 —— 作者调好的一套命令
//! 分发给用户时不需要他们手工重配。
//!
//! 支持的命令类型：
//! - `shell`      cmd 命令行（如 `nircmd setdefaultsounddevice "耳机"`）
//! - `powershell` PowerShell 命令行
//! - `exe`        直接运行程序 + 参数
//! - `url`        打开网址或自定义协议
//! - `builtin`    编译进程序的原生动作（如音频设备切换，零依赖）
//!
//! 占位符 `{arg}`：搜索框里触发词之后的剩余文本。
//! 例如关键词 `yt`、命令 `mpv.exe {arg}`，输入 `yt 猫和老鼠` 即执行
//! `mpv.exe 猫和老鼠`。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CustomCommand {
    /// 唯一 ID（导入导出 / 执行分发用；留空会自动补）
    #[serde(default)]
    pub id: String,
    /// 显示名
    #[serde(default)]
    pub name: String,
    /// 空格分隔的多个触发词（拼音缩写也行），如 "音频 声音 yp"
    #[serde(default)]
    pub keyword: String,
    /// shell | powershell | exe | url | builtin
    #[serde(default = "default_kind")]
    pub kind: String,
    /// 命令内容 / 程序路径 / 网址 / 内置动作 ID
    #[serde(default)]
    pub command: String,
    /// exe 参数（支持 {arg}）
    #[serde(default)]
    pub args: Vec<String>,
    /// 工作目录（留空 = 继承启动器当前目录）
    #[serde(default)]
    pub workdir: String,
    /// 以管理员身份运行（会弹 UAC）
    #[serde(default)]
    pub admin: bool,
    /// 隐藏控制台黑框（shell / powershell 默认隐藏）
    #[serde(default = "default_true")]
    pub hidden: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_kind() -> String {
    "shell".to_string()
}
fn default_true() -> bool {
    true
}

impl Default for CustomCommand {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            keyword: String::new(),
            kind: default_kind(),
            command: String::new(),
            args: Vec::new(),
            workdir: String::new(),
            admin: false,
            hidden: true,
            enabled: true,
        }
    }
}

pub fn commands_file() -> PathBuf {
    std::env::var("LOCALAPPDATA")
        .map(|p| PathBuf::from(p).join("launcher").join("commands.json"))
        .unwrap_or_else(|_| PathBuf::from("commands.json"))
}

/// 首次运行写入的预设：全是 `builtin` 原生动作，用户不需要装任何东西。
pub fn default_presets() -> Vec<CustomCommand> {
    vec![
        CustomCommand {
            id: "builtin-audio-switch".into(),
            name: "切换音频设备".into(),
            keyword: "音频 声音 音响 audio sound 播放设备 yp".into(),
            kind: "builtin".into(),
            command: "audio.switch".into(),
            ..Default::default()
        },
        CustomCommand {
            id: "builtin-audio-toggle".into(),
            name: "音频设备来回切".into(),
            keyword: "来回切 切换音频 qb toggle-audio".into(),
            kind: "builtin".into(),
            command: "audio.toggle".into(),
            ..Default::default()
        },
        CustomCommand {
            id: "builtin-audio-capture".into(),
            name: "切换录音设备".into(),
            keyword: "录音 麦克风 mic microphone".into(),
            kind: "builtin".into(),
            command: "audio.capture".into(),
            ..Default::default()
        },
    ]
}

/// 读取命令表。文件不存在时写入默认预设并返回（配置自愈）。
pub fn load_commands() -> Vec<CustomCommand> {
    match std::fs::read_to_string(commands_file()) {
        Ok(s) => match serde_json::from_str::<Vec<CustomCommand>>(&s) {
            Ok(list) => list,
            Err(_) => default_presets(),
        },
        Err(_) => {
            let presets = default_presets();
            let _ = save_commands(&presets);
            presets
        }
    }
}

/// 整体保存命令表（前端负责增删改，一次提交全量列表）。
pub fn save_commands(list: &[CustomCommand]) -> Result<(), String> {
    let fixed: Vec<CustomCommand> = list
        .iter()
        .cloned()
        .enumerate()
        .map(|(i, mut c)| {
            if c.id.trim().is_empty() {
                c.id = format!("c{}-{i}", now_ms());
            }
            if c.kind.trim().is_empty() {
                c.kind = default_kind();
            }
            c
        })
        .collect();
    let json = serde_json::to_string_pretty(&fixed).map_err(|e| e.to_string())?;
    if let Some(dir) = commands_file().parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(commands_file(), json).map_err(|e| e.to_string())
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// 在命令表里按 ID 找。
pub fn find(id: &str) -> Option<CustomCommand> {
    load_commands().into_iter().find(|c| c.id == id)
}

/// 判断查询是否命中某条命令，返回命中分数（越大越靠前）。
///
/// 触发词（keyword，权重高于显示名）与显示名都参与匹配；输入的第一个词
/// 用于命中，其余部分由前端作为 `{arg}` 传给命令。
pub fn match_command(c: &CustomCommand, query: &str) -> Option<i32> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return None;
    }
    let head = match q.split_once(char::is_whitespace) {
        Some((h, _)) => h.to_string(),
        None => q.clone(),
    };

    // 权重刻意高于文件名匹配（searchcore 的子串档位上限约 8200，叠加使用
    // 频次加权后约 9700）：只要触发词命中，命令就应排在文件结果之前。
    let mut candidates: Vec<(String, i32)> = vec![(c.name.to_lowercase(), 10_000)];
    for kw in c.keyword.split_whitespace() {
        candidates.push((kw.to_lowercase(), 12_000));
    }

    // 单个 ASCII 字符（如 "s"）过于宽泛：只认精确命中。否则 keyword 里的
    // "sound"、"audio" 会让用户一敲 s 就刷出一片命令。中文单字不受限
    // （"音" 命中 "音频" 符合直觉）。
    let strict = head.len() == 1 && head.is_ascii();

    let mut best = 0i32;
    for (cand, base) in candidates {
        if cand.is_empty() {
            continue;
        }
        let s = if cand == q {
            base + 500
        } else if cand == head {
            base + 400
        } else if strict {
            0
        } else if cand.starts_with(head.as_str()) {
            base
        } else if cand.contains(head.as_str()) {
            base - 3000
        } else {
            0
        };
        if s > best {
            best = s;
        }
    }

    if best == 0 {
        None
    } else {
        Some(best)
    }
}

// —— 执行 ——

fn to_pcw(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 只替换占位符，不追加。
fn expand(text: &str, arg: &str) -> String {
    text.replace("{arg}", arg)
}

/// 替换占位符；没有占位符时把剩余文本追加到末尾（`yt 猫` 这类用法）。
fn expand_append(text: &str, arg: &str) -> String {
    if text.contains("{arg}") {
        text.replace("{arg}", arg)
    } else if arg.trim().is_empty() {
        text.to_string()
    } else {
        format!("{text} {arg}")
    }
}

fn system_exe(name: &str) -> String {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
    match name {
        "cmd" => format!("{root}\\System32\\cmd.exe"),
        "powershell" => format!("{root}\\System32\\WindowsPowerShell\\v1.0\\powershell.exe"),
        _ => name.to_string(),
    }
}

/// ShellExecuteW 启动（verb 为 "runas" 时提权，为 "open" 时按关联程序打开）。
fn shell_execute(verb_name: &str, file: &str, params: &str, workdir: &str) -> Result<(), String> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::PCWSTR;

    let verb = to_pcw(verb_name);
    let f = to_pcw(file);
    let p = to_pcw(params);
    let d = to_pcw(workdir);
    let dir_ptr = if workdir.trim().is_empty() {
        PCWSTR::null()
    } else {
        PCWSTR(d.as_ptr())
    };
    let r = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(f.as_ptr()),
            PCWSTR(p.as_ptr()),
            dir_ptr,
            SW_SHOWNORMAL,
        )
    };
    if r.0 as isize <= 32 {
        return Err("启动失败（目标不存在，或取消了 UAC）".into());
    }
    Ok(())
}

/// Windows 命令行参数引号（仅在需要时加引号，并处理结尾反斜杠）。
fn quote_arg(s: &str) -> String {
    if !s.is_empty() && !s.contains(' ') && !s.contains('\t') && !s.contains('"') {
        return s.to_string();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0usize;
    for ch in s.chars() {
        if ch == '\\' {
            backslashes += 1;
            out.push('\\');
        } else if ch == '"' {
            for _ in 0..=backslashes {
                out.push('\\');
            }
            out.push('"');
            backslashes = 0;
        } else {
            backslashes = 0;
            out.push(ch);
        }
    }
    for _ in 0..backslashes {
        out.push('\\');
    }
    out.push('"');
    out
}

/// 启动子进程（`hidden` 决定隐藏还是新建控制台窗口）。
///
/// 刻意**不用** `std::process::Command`：它默认继承调用方的标准句柄，而启动器是
/// `windows_subsystem = "windows"` 的无控制台进程（也可能由脚本/管道拉起），
/// 继承来的句柄会让 cmd.exe 立刻报"文件名、目录名或卷标语法不正确"并秒退。
/// 与 shell_ops 保持一致：CreateProcessW + bInheritHandles = FALSE。
fn spawn_detached(exe: &str, rest: &str, spec: &CustomCommand) -> Result<(), String> {
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        CreateProcessW, CREATE_NEW_CONSOLE, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT,
        PROCESS_INFORMATION, STARTUPINFOW,
    };

    let full = if rest.trim().is_empty() {
        format!("\"{exe}\"")
    } else {
        format!("\"{exe}\" {rest}")
    };
    let mut cmdline: Vec<u16> = full.encode_utf16().chain(std::iter::once(0)).collect();

    let dir_w = to_pcw(&spec.workdir);
    let dir_ptr =
        if spec.workdir.trim().is_empty() || !std::path::Path::new(&spec.workdir).is_dir() {
            PCWSTR::null()
        } else {
            PCWSTR(dir_w.as_ptr())
        };
    let flags = if spec.hidden {
        CREATE_NO_WINDOW
    } else {
        CREATE_NEW_CONSOLE
    } | CREATE_UNICODE_ENVIRONMENT;

    unsafe {
        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        CreateProcessW(
            PCWSTR::null(),
            PWSTR(cmdline.as_mut_ptr()),
            None,
            None,
            false,
            flags,
            None,
            dir_ptr,
            &si,
            &mut pi,
        )
        .map_err(|e| format!("启动失败: {e}"))?;
        let _ = CloseHandle(pi.hProcess);
        let _ = CloseHandle(pi.hThread);
    }
    Ok(())
}

/// 执行一条命令，返回给用户的提示文案。
pub fn run(c: &CustomCommand, arg: &str) -> Result<String, String> {
    if !c.enabled {
        return Err(format!("命令已禁用: {}", c.name));
    }
    match c.kind.as_str() {
        "builtin" => run_builtin(c, arg),
        "url" => {
            let url = expand_append(&c.command, arg);
            if url.trim().is_empty() {
                return Err("网址为空".into());
            }
            shell_execute("open", &url, "", "")?;
            Ok(format!("已打开 {}", c.name))
        }
        "shell" => {
            let line = expand_append(&c.command, arg);
            if line.trim().is_empty() {
                return Err("命令内容为空".into());
            }
            let comspec = std::env::var("ComSpec").unwrap_or_else(|_| system_exe("cmd"));
            let rest = format!("/c {line}");
            if c.admin {
                shell_execute("runas", &comspec, &rest, &c.workdir)?;
            } else {
                spawn_detached(&comspec, &rest, c)?;
            }
            Ok(format!("已执行 {}", c.name))
        }
        "powershell" => {
            let line = expand_append(&c.command, arg);
            if line.trim().is_empty() {
                return Err("命令内容为空".into());
            }
            let ps = system_exe("powershell");
            // 引号交给 Windows 命令行解析，PowerShell 侧再转义一次
            let rest = format!(
                "-NoProfile -ExecutionPolicy Bypass -Command \"{}\"",
                line.replace('"', "\\\"")
            );
            if c.admin {
                shell_execute("runas", &ps, &rest, &c.workdir)?;
            } else {
                spawn_detached(&ps, &rest, c)?;
            }
            Ok(format!("已执行 {}", c.name))
        }
        "exe" => {
            let exe = c.command.trim();
            if exe.is_empty() {
                return Err("程序路径为空".into());
            }
            let args: Vec<String> = c.args.iter().map(|a| expand(a, arg)).collect();
            let quoted: Vec<String> = args.iter().map(|a| quote_arg(a)).collect();
            let rest = quoted.join(" ");
            if c.admin {
                shell_execute("runas", exe, &rest, &c.workdir)?;
            } else {
                spawn_detached(exe, &rest, c)?;
            }
            Ok(format!("已启动 {}", c.name))
        }
        other => Err(format!("未知的命令类型: {other}")),
    }
}

/// 内置动作（字符串 ID → 原生实现）。
fn run_builtin(c: &CustomCommand, arg: &str) -> Result<String, String> {
    match c.command.as_str() {
        "audio.toggle" => crate::audio::toggle_default().map(|n| format!("已切换到 {n}")),
        "audio.switch" | "audio.capture" => {
            if arg.trim().is_empty() {
                Err("请在搜索结果里选择具体设备".into())
            } else {
                crate::audio::set_default_by_name(arg).map(|n| format!("已切换到 {n}"))
            }
        }
        other => Err(format!("未知的内置动作: {other}（可能是新版本新增的）")),
    }
}
