//! 枚举本机卷：卷 GUID 路径、盘符、文件系统类型。

use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{
    FindFirstVolumeW, FindNextVolumeW, FindVolumeClose, GetDriveTypeW, GetVolumeInformationW,
    GetVolumePathNamesForVolumeNameW,
};

#[derive(Clone, Debug)]
pub struct VolumeInfo {
    /// `\\?\Volume{...}` 形式
    pub volume_path: String,
    /// 挂载盘符（无盘符卷为 None）
    pub drive: Option<char>,
    pub is_ntfs: bool,
}

fn to_pcw(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 调用方需要管理员权限读取卷数据；本函数只做枚举，任何权限下都可用。
pub fn list_volumes() -> Vec<VolumeInfo> {
    let mut out = Vec::new();
    unsafe {
        let mut buf = [0u16; 60];
        let handle = match FindFirstVolumeW(&mut buf) {
            Ok(h) => h,
            Err(_) => return out,
        };
        loop {
            let len = buf.iter().position(|&c| c == 0).unwrap_or(0);
            let volume_path = String::from_utf16_lossy(&buf[..len]);
            if !volume_path.is_empty() {
                let drive = mount_point(&volume_path);
                let is_ntfs = match &drive {
                    Some(d) => fs_type_is_ntfs(*d),
                    None => false,
                };
                out.push(VolumeInfo {
                    volume_path,
                    drive,
                    is_ntfs,
                });
            }
            if FindNextVolumeW(handle, &mut buf).is_err() {
                break;
            }
        }
        let _ = FindVolumeClose(handle);
    }
    out
}

/// 取卷的第一个挂载点盘符。
fn mount_point(volume_path: &str) -> Option<char> {
    unsafe {
        let vn = to_pcw(volume_path);
        let mut names = [0u16; 1024];
        let mut ret = 0u32;
        GetVolumePathNamesForVolumeNameW(
            PCWSTR(vn.as_ptr()),
            Some(&mut names),
            &mut ret,
        )
        .ok()?;
        // 返回的是一组 \0 分隔的挂载点，取第一个形如 "C:\" 的
        let mut i = 0usize;
        while i < names.len() && names[i] != 0 {
            let end = names[i..].iter().position(|&c| c == 0).unwrap() + i;
            let s = String::from_utf16_lossy(&names[i..end]);
            let bytes: Vec<char> = s.chars().collect();
            if bytes.len() >= 3 && bytes[1] == ':' && bytes[2] == '\\' {
                let c = bytes[0].to_ascii_uppercase();
                if c.is_ascii_alphabetic() {
                    return Some(c);
                }
            }
            i = end + 1;
        }
        None
    }
}

fn fs_type_is_ntfs(drive: char) -> bool {
    unsafe {
        let root = to_pcw(&format!("{drive}:\\"));
        let mut fs_name = [0u16; 32];
        GetVolumeInformationW(
            PCWSTR(root.as_ptr()),
            None,
            None,
            None,
            None,
            Some(&mut fs_name),
        )
        .is_ok_and(|_| {
            let s = String::from_utf16_lossy(&fs_name);
            s.eq_ignore_ascii_case("NTFS")
        })
    }
}

/// 固定磁盘盘符列表（GetDriveTypeW == DRIVE_FIXED(3)）。
/// 不做文件系统名校验——个别环境下 GetVolumeInformationW 对所有盘都失败，
/// 是否 NTFS 交由 MFT 枚举本身判定（非 NTFS 盘会在重建时报错跳过）。
pub fn fixed_drives() -> Vec<char> {
    (b'A'..=b'Z')
        .map(|c| c as char)
        .filter(|&d| unsafe {
            GetDriveTypeW(PCWSTR(to_pcw(&format!("{d}:\\")).as_ptr())) == 3
        })
        .collect()
}

/// MFT 重建候选盘。fixed_drives 为空（类型探测异常）时尝试全部盘符 A-Z，
/// 不存在的盘在 MFT 打开阶段快速失败并被日志记录。
pub fn candidate_drives() -> Vec<char> {
    let fixed = fixed_drives();
    if fixed.is_empty() {
        crate::engine::slog("fixed_drives 为空 → 尝试全部盘符 A-Z");
        return (b'A'..=b'Z').map(|c| c as char).collect();
    }
    fixed
}
