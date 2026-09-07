//! NTFS MFT 直读扫描器。
//!
//! 通过 `FSCTL_ENUM_USN_DATA` 顺序读取卷的主文件表（MFT），
//! 一次性拿到全盘文件记录（FRN / 父 FRN / 文件名 / 属性）。
//! 需要管理员权限；百万级文件 2~5 秒即可扫完。

use windows::core::PCWSTR;
use windows::Win32::Foundation::{GetLastError, GENERIC_READ, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::FSCTL_ENUM_USN_DATA;

/// `MFT_ENUM_DATA_V0`
#[repr(C)]
struct MftEnumData {
    start_frn: u64,
    low_usn: i64,
    high_usn: i64,
}

#[derive(Debug)]
pub enum MftError {
    Io(String),
}

// 手写 Display/Error，避免额外依赖
impl std::fmt::Display for MftError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MftError::Io(s) => write!(f, "{s}"),
        }
    }
}
impl std::error::Error for MftError {}

/// 打开卷的读取句柄（`\\.\C:`），需要管理员权限。
pub fn open_volume(drive: char) -> Result<HANDLE, MftError> {
    unsafe {
        let path = to_pcw(&format!(r"\\.\{drive}:"));
        CreateFileW(
            PCWSTR(path.as_ptr()),
            GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
        .map_err(|e| MftError::Io(format!("打开卷 {drive}: 失败: {e}")))
    }
}

/// 扫描整个 MFT，把每条记录交给 `sink`。
/// 返回解析出的记录总数（含系统文件、8.3 短名）。
pub fn scan_mft(
    volume: HANDLE,
    sink: &mut dyn FnMut(u64 /*frn*/, u64 /*parent*/, &[u16] /*name*/, bool /*is_dir*/),
) -> Result<usize, MftError> {
    const ERROR_HANDLE_EOF: u16 = 38;
    const BUF_WORDS: usize = 64 * 1024 / 8; // 64KB，8 字节对齐，便于安全解读字段

    unsafe {
        let mut med = MftEnumData {
            start_frn: 0,
            low_usn: 0,
            high_usn: i64::MAX,
        };
        let mut buf = vec![0u64; BUF_WORDS];
        let mut count = 0usize;
        let mut name_buf: Vec<u16> = Vec::with_capacity(280);

        loop {
            let mut ret = 0u32;
            let res = DeviceIoControl(
                volume,
                FSCTL_ENUM_USN_DATA,
                Some(&med as *const MftEnumData as *const std::ffi::c_void),
                std::mem::size_of::<MftEnumData>() as u32,
                Some(buf.as_mut_ptr() as *mut std::ffi::c_void),
                (BUF_WORDS * 8) as u32,
                Some(&mut ret),
                None,
            );
            if res.is_err() {
                let code = GetLastError().0 as u16;
                if code == ERROR_HANDLE_EOF {
                    break;
                }
                return Err(MftError::Io(format!(
                    "FSCTL_ENUM_USN_DATA 失败: win32 错误码 {code}"
                )));
            }
            if (ret as usize) < 8 {
                break;
            }
            med.start_frn = u64::from_le_bytes(to_bytes(buf.as_ptr(), 0, 8));

            // USN_RECORD_V2 布局（MSDN）：
            // 0 RecordLength(u32) | 4 Major(u16) 6 Minor(u16) | 8 FileReferenceNumber(u64)
            // 16 ParentFileReferenceNumber(u64) | 24 Usn(i64) | 32 TimeStamp(8)
            // 40 Reason(4) 44 SourceInfo(4) 48 SecurityId(4) | 52 FileAttributes(u16)
            // 54 FileNameLength(u16) | 56 FileNameOffset(u16) | 60 FileName[]
            let bytes: &[u8] = std::slice::from_raw_parts(buf.as_ptr() as *const u8, ret as usize);
            let mut off = 8usize;
            while off + 60 <= bytes.len() {
                let rec_len = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap()) as usize;
                if rec_len < 60 || off + rec_len > bytes.len() {
                    break;
                }
                let attrs = u16::from_le_bytes(bytes[off + 52..off + 54].try_into().unwrap());
                let name_len =
                    u16::from_le_bytes(bytes[off + 56..off + 58].try_into().unwrap()) as usize;
                let name_off =
                    u16::from_le_bytes(bytes[off + 58..off + 60].try_into().unwrap()) as usize;
                let parent = u64::from_le_bytes(bytes[off + 16..off + 24].try_into().unwrap());
                let frn = u64::from_le_bytes(bytes[off + 8..off + 16].try_into().unwrap());

                if name_len >= 2 && name_off >= 60 && off + name_off + name_len <= bytes.len()
                {
                    let base = off + name_off;
                    name_buf.clear();
                    if base % 2 == 0 {
                        // 对齐快速路径
                        let s = std::slice::from_raw_parts(
                            (bytes.as_ptr().add(base)) as *const u16,
                            name_len / 2,
                        );
                        name_buf.extend_from_slice(s);
                    } else {
                        for i in (0..name_len).step_by(2) {
                            name_buf.push(u16::from_le_bytes(
                                bytes[base + i..base + i + 2].try_into().unwrap(),
                            ));
                        }
                    }
                    let is_dir = attrs & 0x10 != 0; // FILE_ATTRIBUTE_DIRECTORY
                    sink(frn, parent, &name_buf, is_dir);
                    count += 1;
                }
                off += rec_len;
            }
        }
        Ok(count)
    }
}

fn to_bytes(base: *const u64, off: usize, len: usize) -> [u8; 8] {
    unsafe {
        let mut out = [0u8; 8];
        std::ptr::copy_nonoverlapping((base as *const u8).add(off), out.as_mut_ptr(), len);
        out
    }
}

fn to_pcw(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
