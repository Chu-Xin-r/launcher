//! 默认音频设备枚举与切换。
//!
//! Windows 没有任何公开 API 能"设置默认播放设备"：`IMMDeviceEnumerator`
//! 只提供读能力，写必须走未公开的 `IPolicyConfig`（CLSID = CPolicyConfigClient）。
//! 这里手写它的 vtable 直接调 `SetDefaultEndpoint`，所以不需要 nircmd /
//! SoundVolumeView 之类的第三方工具，切换耗时在毫秒级。
//!
//! 设备列表来自运行时枚举（不是写死的"扬声器/耳机"），因此在任何机器上
//! 都自动适配用户实际的声卡与蓝牙耳机名称。

use std::ffi::c_void;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use windows::core::{GUID, HRESULT, IUnknown, Interface, PCWSTR, PWSTR};
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL, STGM_READ};
use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;

/// CPolicyConfigClient
const CLSID_CPOLICYCONFIGCLIENT: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

/// IPolicyConfig（Win7+）。
///
/// 必须显式 QueryInterface 到这个 IID：`CoCreateInstance` 请求 IID_IUnknown
/// 拿到的是对象的默认接口指针，而在这个 coclass 上它与 IPolicyConfig 指针
/// **不是同一个地址**（多重继承 thunk，两者相差一个字长）。把 IUnknown 指针
/// 直接当 IPolicyConfig 用，或拿 IUnknown 的 vtable 去 Release IPolicyConfig
/// 指针，都会因为 this 调整错位而破坏引用计数、最终访问已释放内存。
const IID_IPOLICYCONFIG: GUID = GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);

/// PKEY_Device_FriendlyName（设备友好名，如 "扬声器 (Realtek(R) Audio)"）
const PKEY_DEVICE_FRIENDLYNAME: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
    pid: 14,
};

/// "来回切"的历史状态。
///
/// 记录**最近被切换离开**过的设备 ID（最新在前），落盘保存。
///
/// 用列表而不是单个值：单值一旦与实际不符（比如那次切换其实失败了，
/// 或者用户从系统托盘手动改了默认设备），判定条件就不成立，只能退化成
/// "按枚举顺序切下一个"，用户会感觉 qb 切到了莫名其妙的设备。
#[derive(Serialize, Deserialize, Default)]
struct AudioState {
    /// 最近被切换离开的设备（最新在前）
    #[serde(default)]
    recent: Vec<String>,
    /// 0.2.1 的旧单值格式，只读用于迁移，不再写回
    #[serde(default, skip_serializing)]
    last: Option<String>,
}

fn audio_state_file() -> PathBuf {
    std::env::var("LOCALAPPDATA")
        .map(|p| PathBuf::from(p).join("launcher").join("audio.json"))
        .unwrap_or_else(|_| PathBuf::from("audio.json"))
}

fn load_state() -> AudioState {
    std::fs::read_to_string(audio_state_file())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_state(st: &AudioState) {
    let path = audio_state_file();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(s) = serde_json::to_string_pretty(st) {
        let _ = std::fs::write(path, s);
    }
}

/// 读取历史（兼容 0.2.1 的单值格式：`last` 当作 `recent[0]`）。
fn load_recent() -> Vec<String> {
    let st = load_state();
    let mut v: Vec<String> = st
        .recent
        .into_iter()
        .filter(|s| !s.trim().is_empty())
        .collect();
    if v.is_empty() {
        if let Some(old) = st.last.filter(|s| !s.trim().is_empty()) {
            v.push(old);
        }
    }
    v
}

/// 记录"被切换离开"的设备。
///
/// **只在切换成功后调用**：切换失败还写历史的话，`recent` 会指向一个
/// 从未真正生效的设备，把后续的来回切带偏。
fn push_recent(id: &str) {
    if id.trim().is_empty() {
        return;
    }
    let mut st = load_state();
    st.recent.retain(|x| x != id);
    st.recent.insert(0, id.to_string());
    st.recent.truncate(6);
    save_state(&st);
}

/// 当前 Console 角色的默认设备 ID（`exclude` 用于排除某个 ID）。
fn current_default_id(exclude: Option<&str>) -> Option<String> {
    let devices = list_devices(false).ok()?;
    devices
        .iter()
        .find(|d| d.is_default && exclude.map_or(true, |e| d.id != e))
        .map(|d| d.id.clone())
}

/// IUnknown 三个方法（所有 COM 接口 vtable 的前缀）。
#[repr(C)]
struct IUnknownVtbl {
    query_interface:
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
}

/// IPolicyConfig 的 vtable。
///
/// 布局：IUnknown 3 项，随后是 GetMixFormat、GetDeviceFormat、ResetDeviceFormat、
/// SetDeviceFormat、GetProcessingPeriod、SetProcessingPeriod、GetShareMode、
/// SetShareMode、GetPropertyValue、SetPropertyValue 共 10 个方法（只占位，
/// 从不调用），第 14 项（索引 13）是 SetDefaultEndpoint。
///
/// 索引 13 已在本机实测确认（调用返回 S_OK）。
#[repr(C)]
struct PolicyConfigVtbl {
    unk: IUnknownVtbl,
    reserved: [usize; 10],
    set_default_endpoint: unsafe extern "system" fn(*mut c_void, PCWSTR, i32) -> HRESULT,
}

#[derive(Serialize, Clone, Debug)]
pub struct AudioDevice {
    /// 端点 ID，形如 `{0.0.0.00000000}.{guid}`
    pub id: String,
    /// 友好名
    pub name: String,
    /// 是否为当前默认设备（Console 角色）
    pub is_default: bool,
}

fn to_pcw(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 取出 PWSTR 内容并释放 COM 任务内存。
unsafe fn take_pwstr(p: PWSTR) -> String {
    if p.0.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    while *p.0.add(len) != 0 {
        len += 1;
    }
    let s = String::from_utf16_lossy(std::slice::from_raw_parts(p.0, len));
    CoTaskMemFree(Some(p.0 as *const c_void));
    s
}

/// 端点 ID（SetDefaultEndpoint 需要的正是这个字符串）。
unsafe fn device_id(dev: &IMMDevice) -> Option<String> {
    let p = dev.GetId().ok()?;
    Some(take_pwstr(p))
}

/// 设备友好名（属性存储）。
unsafe fn device_name(dev: &IMMDevice) -> Option<String> {
    let store = dev.OpenPropertyStore(STGM_READ).ok()?;
    let pv = store.GetValue(&PKEY_DEVICE_FRIENDLYNAME).ok()?;
    let p = PropVariantToStringAlloc(&pv).ok()?;
    let s = take_pwstr(p);
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// 枚举当前处于"活动"状态的音频端点。`capture = true` 取录音设备。
pub fn list_devices(capture: bool) -> Result<Vec<AudioDevice>, String> {
    crate::commands::init_com();
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None::<&IUnknown>, CLSCTX_ALL)
                .map_err(|e| format!("音频设备枚举器初始化失败: {e}"))?;
        let flow = if capture { eCapture } else { eRender };
        let coll = enumerator
            .EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)
            .map_err(|e| format!("枚举音频设备失败: {e}"))?;
        let count = coll.GetCount().map_err(|e| format!("读取设备数量失败: {e}"))?;
        // 当前默认设备（Console 角色）
        let default_id = enumerator
            .GetDefaultAudioEndpoint(flow, eConsole)
            .ok()
            .and_then(|d| device_id(&d));
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count {
            let dev = match coll.Item(i) {
                Ok(d) => d,
                Err(_) => continue,
            };
            let id = match device_id(&dev) {
                Some(v) if !v.is_empty() => v,
                _ => continue,
            };
            let name = device_name(&dev).unwrap_or_else(|| id.clone());
            let is_default = default_id.as_deref() == Some(id.as_str());
            out.push(AudioDevice {
                id,
                name,
                is_default,
            });
        }
        Ok(out)
    }
}

/// 把指定端点设为默认设备。
///
/// 三个角色（Console 默认设备 / Multimedia / Communications 通信设备）一起设置，
/// 避免"切了播放设备但微信语音还走旧设备"这类迷惑行为。
pub fn set_default_device(device_id: &str, all_roles: bool) -> Result<(), String> {
    if device_id.trim().is_empty() {
        return Err("设备 ID 为空".into());
    }
    // 记下切换前的设备，但**先不落盘**：切换失败时不能污染历史
    let prev = current_default_id(Some(device_id));
    crate::commands::init_com();
    let id_w = to_pcw(device_id);
    unsafe {
        let unk: IUnknown =
            CoCreateInstance(&CLSID_CPOLICYCONFIGCLIENT, None::<&IUnknown>, CLSCTX_ALL)
                .map_err(|e| format!("音频策略对象创建失败: {e}"))?;
        let raw = unk.as_raw();
        if raw.is_null() {
            return Err("音频策略接口为空".into());
        }
        let unk_vtbl = *(raw as *mut *const IUnknownVtbl);
        // 显式查询到 IPolicyConfig：拿到的 pc 与 raw 可能相差一个字长
        let mut pc: *mut c_void = std::ptr::null_mut();
        ((*unk_vtbl).query_interface)(raw, &IID_IPOLICYCONFIG, &mut pc)
            .ok()
            .map_err(|e| format!("获取 IPolicyConfig 接口失败: {e}"))?;
        if pc.is_null() {
            return Err("IPolicyConfig 接口为空".into());
        }
        let vtbl = *(pc as *mut *const PolicyConfigVtbl);
        if vtbl.is_null() {
            return Err("IPolicyConfig 接口表为空".into());
        }
        let roles: &[i32] = if all_roles { &[0, 1, 2] } else { &[0] };
        let mut last_err: Option<String> = None;
        for &role in roles {
            if let Err(e) = ((*vtbl).set_default_endpoint)(pc, PCWSTR(id_w.as_ptr()), role).ok() {
                last_err = Some(e.to_string());
            }
        }
        // 必须用 pc 自己的 vtable 释放：thunk 要按 IPolicyConfig 的 this 做调整
        ((*vtbl).unk.release)(pc);
        match last_err {
            Some(e) => Err(format!("切换默认音频设备失败: {e}")),
            None => {
                // 只有真正切成功了才记历史
                if let Some(p) = prev {
                    push_recent(&p);
                }
                Ok(())
            }
        }
    }
}

/// 按名字模糊匹配设备并切换（名字为空时视为不匹配）。
pub fn set_default_by_name(name: &str) -> Result<String, String> {
    let key = name.trim().to_lowercase();
    if key.is_empty() {
        return Err("设备名为空".into());
    }
    let devices = list_devices(false)?;
    let hit = devices
        .iter()
        .find(|d| d.name.to_lowercase() == key)
        .or_else(|| devices.iter().find(|d| d.name.to_lowercase().contains(&key)))
        .ok_or_else(|| format!("未找到音频设备: {name}"))?;
    set_default_device(&hit.id, true)?;
    Ok(hit.name.clone())
}

/// 在"最近使用的两个播放设备"之间来回切，返回切换到的设备名。
///
/// 没有历史记录时退化为按枚举顺序取当前设备的下一个（两个设备时即为来回切）。
pub fn toggle_default() -> Result<String, String> {
    let devices = list_devices(false)?;
    if devices.is_empty() {
        return Err("没有找到可用的播放设备".into());
    }
    let current = devices.iter().find(|d| d.is_default).cloned();
    let name_of = |id: &str| {
        devices
            .iter()
            .find(|d| d.id == id)
            .map(|d| d.name.clone())
            .unwrap_or_else(|| id.to_string())
    };

    // 优先切回"最近被切换离开过、且仍然存在"的设备（列表最新在前）。
    // 用列表而非单值：即便某个历史项已失效（设备拔了/那次切换失败），
    // 也能顺延到下一个可用的，而不是直接掉进环形分支。
    let target = {
        let cur_id = current.as_ref().map(|c| c.id.as_str());
        load_recent()
            .into_iter()
            .find(|id| Some(id.as_str()) != cur_id && devices.iter().any(|d| &d.id == id))
    };

    let target_id = match target {
        Some(id) => id,
        None => {
            if devices.len() < 2 {
                return Err("只有一个可用播放设备，无需切换".into());
            }
            let idx = current
                .as_ref()
                .and_then(|c| devices.iter().position(|d| d.id == c.id))
                .unwrap_or(usize::MAX);
            // 取当前设备的下一个，环形回绕
            let start = if idx == usize::MAX { 0 } else { (idx + 1) % devices.len() };
            let mut pick = devices[start].id.clone();
            if Some(&pick) == current.as_ref().map(|c| &c.id) {
                pick = devices[(start + 1) % devices.len()].id.clone();
            }
            pick
        }
    };

    // set_default_device 内部会把"切换前"的设备记进历史，无需在这里再写
    set_default_device(&target_id, true)?;
    Ok(name_of(&target_id))
}

