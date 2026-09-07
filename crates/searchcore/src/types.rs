//! 核心数据类型。

/// 条目标志位
pub const FLAG_IS_DIR: u16 = 1;
/// 名字池中存放的是完整路径（非 NTFS 卷 / 无管理员权限的降级模式），文件名部分取最后一个 `\` 之后
pub const FLAG_IS_PATH: u16 = 2;
/// NTFS 系统文件（名字以 `$` 开头），默认搜索中降权
pub const FLAG_IS_SYSTEM: u16 = 4;
/// 位于开始菜单 Programs 目录子树（已安装软件快捷方式，搜索提权）
pub const FLAG_START_MENU: u16 = 8;
/// 名字含汉字（有对应拼音，中文结果提权）
pub const FLAG_IS_CJK: u16 = 16;

/// 索引条目：紧凑布局，单条 24 字节（不计名字池/拼音池）。
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct Entry {
    /// 父目录 FRN（NTFS）；路径条目为 0
    pub parent: u64,
    /// 名字在名字池中的偏移（u16 单位）
    pub name_off: u32,
    /// 拼音在拼音池中的偏移（u16 单位，全拼小写 ASCII 连写）；无汉字时为 0
    pub py_off: u32,
    /// 名字长度（u16 单位）
    pub name_len: u16,
    /// 拼音长度（u16 单位，0 = 无拼音）
    pub py_len: u16,
    pub flags: u16,
}

impl Entry {
    #[inline]
    pub fn is_dir(&self) -> bool {
        self.flags & FLAG_IS_DIR != 0
    }
    #[inline]
    pub fn is_path_entry(&self) -> bool {
        self.flags & FLAG_IS_PATH != 0
    }
    #[inline]
    pub fn is_system(&self) -> bool {
        self.flags & FLAG_IS_SYSTEM != 0
    }
    #[inline]
    pub fn has_py(&self) -> bool {
        self.py_len > 0
    }
    #[inline]
    pub fn is_start_menu(&self) -> bool {
        self.flags & FLAG_START_MENU != 0
    }
    #[inline]
    pub fn is_cjk(&self) -> bool {
        self.flags & FLAG_IS_CJK != 0
    }
}

pub type CancelGen = std::sync::atomic::AtomicU64;
