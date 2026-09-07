//! USN 变更日志：实时增量更新的核心。
//!
//! NTFS 每个卷有一份 USN 日志，内核在文件建/删/改名时自动写入记录。
//! 我们用 `FSCTL_READ_USN_JOURNAL` 阻塞读——没有新记录时线程休眠（零 CPU），
//! 一有变更内核立刻唤醒，把增量应用到内存索引。

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{
    FSCTL_CREATE_USN_JOURNAL, FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL,
};

// 变更原因位
pub const REASON_FILE_CREATE: u32 = 0x0000_0100;
pub const REASON_FILE_DELETE: u32 = 0x0000_0200;
pub const REASON_RENAME_OLD: u32 = 0x0000_1000;
pub const REASON_RENAME_NEW: u32 = 0x0000_2000;

const ERROR_JOURNAL_DELETE_DETECTED: u32 = 1178;
const ERROR_JOURNAL_NOT_ACTIVE: u32 = 1179;
const ERROR_TIMEOUT: u32 = 1460;

/// `CREATE_USN_JOURNAL_DATA`
#[repr(C)]
struct CreateUsnJournalData {
    maximum_size: u64,
    allocation_delta: u64,
}

/// `USN_JOURNAL_DATA`（FSCTL_QUERY_USN_JOURNAL 输出）
#[repr(C)]
struct UsnJournalData {
    journal_id: u64,
    first_usn: i64,
    next_usn: i64,
    lowest_valid_usn: i64,
    max_usn: i64,
    maximum_size: u64,
    allocation_delta: u64,
}

/// `READ_USN_JOURNAL_DATA_V1`
#[repr(C)]
struct ReadUsnJournalDataV1 {
    start_usn: i64,
    reason_mask: u32,
    return_only_on_close: u32,
    timeout_100ns: u64,
    bytes_to_wait_for: u64,
    journal_id: u64,
    min_major_version: u16,
    max_major_version: u16,
}

#[derive(Clone, Debug)]
pub struct UsnUpdate {
    pub frn: u64,
    pub parent: u64,
    pub usn: i64,
    pub reason: u32,
    pub is_dir: bool,
    pub name: Vec<u16>,
}

#[derive(Debug)]
pub enum UsnError {
    /// 日志被删/回绕 → 调用方应整卷重建索引
    JournalDeleted,
    /// 阻塞读超时（无新记录），重试即可
    Timeout,
    Io(String),
}

fn last_win32() -> u32 {
    unsafe { windows::Win32::Foundation::GetLastError().0 as u32 }
}

/// 确保卷的 USN 日志存在（不存在则创建；需要管理员）。
pub fn ensure_journal(volume: HANDLE) -> Result<(), UsnError> {
    unsafe {
        let input = CreateUsnJournalData {
            maximum_size: 32 * 1024 * 1024,
            allocation_delta: 8 * 1024 * 1024,
        };
        let mut ret = 0u32;
        DeviceIoControl(
            volume,
            FSCTL_CREATE_USN_JOURNAL,
            Some(&input as *const _ as *const std::ffi::c_void),
            std::mem::size_of::<CreateUsnJournalData>() as u32,
            None,
            0,
            Some(&mut ret),
            None,
        )
        .map_err(|_| UsnError::Io(format!("创建 USN 日志失败: win32 {}", last_win32())))
    }
}

/// 查询日志状态：`(journal_id, first_usn, next_usn)`
pub fn query_journal(volume: HANDLE) -> Result<(u64, i64, i64), UsnError> {
    unsafe {
        let mut out = UsnJournalData {
            journal_id: 0,
            first_usn: 0,
            next_usn: 0,
            lowest_valid_usn: 0,
            max_usn: 0,
            maximum_size: 0,
            allocation_delta: 0,
        };
        let mut ret = 0u32;
        DeviceIoControl(
            volume,
            FSCTL_QUERY_USN_JOURNAL,
            None,
            0,
            Some(&mut out as *mut _ as *mut std::ffi::c_void),
            std::mem::size_of::<UsnJournalData>() as u32,
            Some(&mut ret),
            None,
        )
        .map(|_| (out.journal_id, out.first_usn, out.next_usn))
        .map_err(|_| map_journal_err("查询 USN 日志失败"))
    }
}

/// 从 `start_usn` 读一批记录。`bytes_to_wait_for > 0` 时阻塞等待新记录（零轮询）。
pub fn read_usn(
    volume: HANDLE,
    start_usn: i64,
    journal_id: u64,
    timeout_100ns: u64,
    bytes_to_wait_for: u64,
    buf: &mut [u8],
) -> Result<(Vec<UsnUpdate>, i64), UsnError> {
    unsafe {
        let input = ReadUsnJournalDataV1 {
            start_usn,
            reason_mask: u32::MAX,
            // 0 = 立即返回（不等文件关闭），改名/删除即时生效
            return_only_on_close: 0,
            timeout_100ns,
            bytes_to_wait_for,
            journal_id,
            min_major_version: 2,
            max_major_version: 3,
        };
        let mut ret = 0u32;
        let r = DeviceIoControl(
            volume,
            FSCTL_READ_USN_JOURNAL,
            Some(&input as *const _ as *const std::ffi::c_void),
            std::mem::size_of::<ReadUsnJournalDataV1>() as u32,
            Some(buf.as_mut_ptr() as *mut std::ffi::c_void),
            buf.len() as u32,
            Some(&mut ret),
            None,
        );
        let n = ret as usize;
        if r.is_err() {
            return Err(map_journal_err("读取 USN 日志失败"));
        }
        if n < 16 {
            return Ok((Vec::new(), start_usn));
        }
        let next_usn = i64::from_le_bytes(buf[0..8].try_into().unwrap());
        let mut out = Vec::new();
        let mut off = 8usize;
        while off + 8 <= n {
            let rec_len = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
            if rec_len == 0 || off + rec_len > n {
                break;
            }
            let major = u16::from_le_bytes(buf[off + 4..off + 6].try_into().unwrap());
            let (frn, parent, _usn, reason, attrs, name_len, name_off) = if major == 3 {
                let frn = u64::from_le_bytes(buf[off + 8..off + 16].try_into().unwrap());
                let parent = u64::from_le_bytes(buf[off + 24..off + 32].try_into().unwrap());
                let usn = i64::from_le_bytes(buf[off + 40..off + 48].try_into().unwrap());
                let reason = u32::from_le_bytes(buf[off + 56..off + 60].try_into().unwrap());
                let attrs = u32::from_le_bytes(buf[off + 68..off + 72].try_into().unwrap());
                let nl = u16::from_le_bytes(buf[off + 72..off + 74].try_into().unwrap()) as usize;
                let no = u16::from_le_bytes(buf[off + 74..off + 76].try_into().unwrap()) as usize;
                (frn, parent, usn, reason, attrs, nl, no)
            } else {
                let frn = u64::from_le_bytes(buf[off + 8..off + 16].try_into().unwrap());
                let parent = u64::from_le_bytes(buf[off + 16..off + 24].try_into().unwrap());
                let usn = i64::from_le_bytes(buf[off + 24..off + 32].try_into().unwrap());
                let reason = u32::from_le_bytes(buf[off + 40..off + 44].try_into().unwrap());
                let attrs = u32::from_le_bytes(buf[off + 52..off + 56].try_into().unwrap());
                let nl = u16::from_le_bytes(buf[off + 56..off + 58].try_into().unwrap()) as usize;
                let no = u16::from_le_bytes(buf[off + 58..off + 60].try_into().unwrap()) as usize;
                (frn, parent, usn, reason, attrs, nl, no)
            };
            if name_len >= 2 && off + name_off + name_len <= off + rec_len {
                let base = off + name_off;
                let mut name = Vec::with_capacity(name_len / 2);
                let mut i = 0;
                while i + 1 < name_len {
                    name.push(u16::from_le_bytes([buf[base + i], buf[base + i + 1]]));
                    i += 2;
                }
                out.push(UsnUpdate {
                    frn,
                    parent,
                    usn: _usn,
                    reason,
                    is_dir: attrs & 0x10 != 0,
                    name,
                });
            }
            off += (rec_len + 7) & !7; // 记录按 8 字节对齐排列
        }
        Ok((out, next_usn))
    }
}

/// 把一批 USN 记录应用到内存索引（单个卷）。
/// 改名 = OLD（删）+ NEW（增）两条记录，顺序应用即正确；中间态记录（无相关原因位）自动忽略。
pub fn apply_updates(vi: &mut crate::index::VolumeIndex, records: &[UsnUpdate]) -> usize {
    let mut applied = 0usize;
    for r in records {
        if r.frn == 0 || r.name.is_empty() {
            continue;
        }
        if r.reason & (REASON_FILE_DELETE | REASON_RENAME_OLD) != 0 {
            if vi.remove(r.frn) {
                applied += 1;
            }
        }
        if r.reason & (REASON_FILE_CREATE | REASON_RENAME_NEW) != 0 {
            vi.upsert(r.frn, r.parent, &r.name, r.is_dir);
            applied += 1;
        }
    }
    applied
}

fn map_journal_err(prefix: &str) -> UsnError {
    let code = last_win32();
    match code {
        ERROR_JOURNAL_DELETE_DETECTED | ERROR_JOURNAL_NOT_ACTIVE => UsnError::JournalDeleted,
        ERROR_TIMEOUT => UsnError::Timeout,
        _ => UsnError::Io(format!("{prefix}: win32 {code}")),
    }
}
