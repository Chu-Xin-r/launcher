//! 内存索引：紧凑条目数组 + UTF-16 名字池 + 拼音池 + FRN→槽位映射。
//!
//! 每个卷一个 `VolumeIndex`：
//! - NTFS 卷条目带 FRN（`frns` 平行数组），支持 USN 实时增量和父链路径重建；
//! - 降级卷（非 NTFS / 无权限）存完整路径（`FLAG_IS_PATH`）；
//! - 含汉字的名字额外存全拼连写（`pys` 池），支持拼音搜索。
//! 搜索时跨卷并行扫描；删除用 swap_remove + O(1) 映射修正；压缩 O(n) 重建。

use rustc_hash::{FxHashMap, FxHashSet};

use crate::py::pinyin_of;
use crate::types::{Entry, FLAG_IS_CJK, FLAG_IS_DIR, FLAG_IS_PATH, FLAG_START_MENU, FLAG_IS_SYSTEM};

pub struct VolumeIndex {
    pub letter: char,
    pub is_ntfs: bool,
    /// USN 日志 ID（快照续传用），未知为 0
    pub journal_id: u64,
    /// 上次已消费的 USN 位置
    pub next_usn: i64,
    /// UTF-16 名字池（路径条目存完整路径）
    pub names: Vec<u16>,
    /// 全拼连写池（ASCII 小写），条目无汉字时无对应片段
    pub pys: Vec<u16>,
    /// 紧凑条目数组（搜索的热路径只扫这一个数组）
    pub entries: Vec<Entry>,
    /// 与 entries 平行的 FRN 数组（路径条目为 0）
    pub frns: Vec<u64>,
    /// FRN → 槽位（仅 NTFS 卷）
    pub frn_map: FxHashMap<u64, u32>,
    /// 开始菜单 Programs 子树的 FRN 集合（含自身），USN 新增条目据此继承标志
    pub start_menu_frns: FxHashSet<u64>,
    /// 累计删除数，达到条目数 25% 时触发压缩
    deleted: usize,
}

impl VolumeIndex {
    pub fn new(letter: char, is_ntfs: bool) -> Self {
        Self {
            letter,
            is_ntfs,
            journal_id: 0,
            next_usn: 0,
            names: Vec::with_capacity(1 << 20),
            pys: Vec::with_capacity(1 << 16),
            entries: Vec::with_capacity(1 << 16),
            frns: Vec::with_capacity(1 << 16),
            frn_map: FxHashMap::default(),
            start_menu_frns: FxHashSet::default(),
            deleted: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[inline]
    pub fn name_slice(&self, e: &Entry) -> &[u16] {
        &self.names[e.name_off as usize..e.name_off as usize + e.name_len as usize]
    }

    #[inline]
    pub fn py_slice(&self, e: &Entry) -> &[u16] {
        &self.pys[e.py_off as usize..e.py_off as usize + e.py_len as usize]
    }

    #[inline]
    fn push_name(&mut self, name: &[u16]) -> (u32, u16) {
        let off = self.names.len() as u32;
        self.names.extend_from_slice(name);
        (off, name.len() as u16)
    }

    /// 计算拼音并写入拼音池；无汉字返回 (0, 0)。
    fn push_py(&mut self, name: &[u16]) -> (u32, u16) {
        match pinyin_of(name) {
            Some(py) => {
                let off = self.pys.len() as u32;
                self.pys.extend_from_slice(&py);
                (off, py.len() as u16)
            }
            None => (0, 0),
        }
    }

    /// NTFS 条目插入或更新（按 FRN）。8.3 短名（含 `~`）不覆盖长名。
    pub fn upsert(&mut self, frn: u64, parent: u64, name: &[u16], is_dir: bool) {
        let mut flags: u16 = 0;
        if is_dir {
            flags |= FLAG_IS_DIR;
        }
        if name.first() == Some(&(b'$' as u16)) {
            flags |= FLAG_IS_SYSTEM;
        }
        if self.start_menu_frns.contains(&parent) || self.start_menu_frns.contains(&frn) {
            flags |= FLAG_START_MENU;
        }
        let new_is_short = name.contains(&(b'~' as u16));

        if let Some(&slot) = self.frn_map.get(&frn) {
            let old_is_short = {
                let e = &self.entries[slot as usize];
                self.names[e.name_off as usize..e.name_off as usize + e.name_len as usize]
                    .contains(&(b'~' as u16))
            };
            if !old_is_short && new_is_short {
                return; // 已有长名，忽略短名
            }
            let (off, len) = self.push_name(name);
            let (py_off, py_len) = self.push_py(name);
            if py_len > 0 {
                flags |= FLAG_IS_CJK;
            }
            let e = &mut self.entries[slot as usize];
            e.name_off = off;
            e.name_len = len;
            e.py_off = py_off;
            e.py_len = py_len;
            e.flags = flags;
            e.parent = parent;
            return;
        }

        let (off, len) = self.push_name(name);
        let (py_off, py_len) = self.push_py(name);
        if py_len > 0 {
            flags |= FLAG_IS_CJK;
        }
        self.entries.push(Entry {
            parent,
            name_off: off,
            name_len: len,
            py_off,
            py_len,
            flags,
        });
        self.frns.push(frn);
        self.frn_map.insert(frn, self.entries.len() as u32 - 1);
    }

    /// 降级路径条目：完整路径进名字池，文件名部分由匹配器取最后一个 `\` 之后；
    /// 拼音只取文件名部分（与匹配范围一致）。
    pub fn push_path_entry(&mut self, full_path: &str, is_dir: bool) {
        let name: Vec<u16> = full_path.encode_utf16().collect();
        let file_part: &[u16] = match name.iter().rposition(|&c| c == '\\' as u16) {
            Some(p) => &name[p + 1..],
            None => &name,
        };
        let mut flags: u16 = FLAG_IS_PATH;
        if is_dir {
            flags |= FLAG_IS_DIR;
        }
        let (off, len) = self.push_name(&name);
        let (py_off, py_len) = self.push_py(file_part);
        if py_len > 0 {
            flags |= FLAG_IS_CJK;
        }
        self.entries.push(Entry {
            parent: 0,
            name_off: off,
            name_len: len,
            py_off,
            py_len,
            flags,
        });
        self.frns.push(0);
    }

    /// 删除 FRN 条目：swap_remove + O(1) 映射修正。
    pub fn remove(&mut self, frn: u64) -> bool {
        let Some(slot) = self.frn_map.remove(&frn) else {
            return false;
        };
        let last = self.entries.len() - 1;
        if (slot as usize) != last {
            let moved_frn = self.frns[last];
            self.entries[slot as usize] = self.entries[last];
            self.frns[slot as usize] = moved_frn;
            if moved_frn != 0 {
                self.frn_map.insert(moved_frn, slot);
            }
        }
        self.entries.pop();
        self.frns.pop();
        self.deleted += 1;
        if self.deleted * 4 > self.entries.len() {
            self.compact();
        }
        true
    }

    /// O(n) 重建名字池/拼音池与全部映射，回收删除产生的死区。
    pub fn compact(&mut self) {
        let mut new_names = Vec::with_capacity(self.names.len());
        let mut new_pys = Vec::with_capacity(self.pys.len());
        self.frn_map.clear();
        for (i, e) in self.entries.iter_mut().enumerate() {
            let name = self.names[e.name_off as usize..e.name_off as usize + e.name_len as usize]
                .to_vec();
            e.name_off = new_names.len() as u32;
            new_names.extend_from_slice(&name);
            if e.py_len > 0 {
                let py = self.pys[e.py_off as usize..e.py_off as usize + e.py_len as usize].to_vec();
                e.py_off = new_pys.len() as u32;
                new_pys.extend_from_slice(&py);
            }
            let frn = self.frns[i];
            if frn != 0 {
                self.frn_map.insert(frn, i as u32);
            }
        }
        self.names = new_names;
        self.pys = new_pys;
        self.deleted = 0;
    }

    /// 在卷内按目录段序列定位 FRN（从根 FRN=5 逐段向下，ASCII 大小写不敏感）。
    fn frn_of_path(&self, segments: &[&str]) -> Option<u64> {
        let mut cur: u64 = 5; // NTFS 根目录 FRN
        'seg: for seg in segments {
            let seg_u: Vec<u16> = seg.to_ascii_lowercase().encode_utf16().collect();
            for (i, e) in self.entries.iter().enumerate() {
                if e.parent != cur || !e.is_dir() {
                    continue;
                }
                let name = self.name_slice(e);
                if name.len() == seg_u.len()
                    && name.iter().zip(&seg_u).all(|(a, b)| {
                        let fa = if (b'A' as u16..=b'Z' as u16).contains(a) {
                            a + 32
                        } else {
                            *a
                        };
                        fa == *b
                    })
                {
                    cur = self.frns[i];
                    continue 'seg;
                }
            }
            return None;
        }
        Some(cur)
    }

    /// 标记开始菜单 Programs 子树：定位目录 FRN 后按层级扩散标记全部后代。
    /// `paths`：若干目录的路径段序列（如 ["ProgramData","Microsoft",...]）。
    pub fn mark_start_menu(&mut self, paths: &[Vec<String>]) {
        let mut roots: Vec<u64> = Vec::new();
        for p in paths {
            let segs: Vec<&str> = p.iter().map(|s| s.as_str()).collect();
            if let Some(frn) = self.frn_of_path(&segs) {
                roots.push(frn);
            }
        }
        if roots.is_empty() {
            return;
        }
        // 层级扩散：parent 命中集合 → 自身入集合并打标志，直到不再增长
        let mut set: FxHashSet<u64> = roots.iter().copied().collect();
        loop {
            let mut grew = false;
            for (i, e) in self.entries.iter_mut().enumerate() {
                if e.flags & FLAG_START_MENU == 0 && set.contains(&e.parent) {
                    e.flags |= FLAG_START_MENU;
                    let frn = self.frns[i];
                    if frn != 0 {
                        set.insert(frn);
                    }
                    grew = true;
                }
            }
            if !grew {
                break;
            }
        }
        self.start_menu_frns = set;
    }

    /// FRN → 完整路径（沿父链回溯）。
    pub fn path_for_slot(&self, slot: u32) -> String {
        let mut parts: Vec<&[u16]> = Vec::with_capacity(16);
        let mut cur = slot as usize;
        for _ in 0..128 {
            let Some(e) = self.entries.get(cur) else { break };
            parts.push(self.name_slice(e));
            if e.parent == 0 || e.parent == 5 {
                break; // 5 = 根目录 FRN
            }
            match self.frn_map.get(&e.parent) {
                Some(&p) => cur = p as usize,
                None => break,
            }
        }
        let mut out = String::with_capacity(96);
        out.push(self.letter);
        out.push_str(":\\");
        for p in parts.iter().rev() {
            if p.is_empty() {
                continue;
            }
            if !out.ends_with('\\') {
                out.push('\\');
            }
            out.push_str(&String::from_utf16_lossy(p));
        }
        out
    }
}

pub struct Index {
    pub volumes: Vec<VolumeIndex>,
}

impl Index {
    pub fn new() -> Self {
        Self { volumes: Vec::new() }
    }

    pub fn total_entries(&self) -> usize {
        self.volumes.iter().map(|v| v.len()).sum()
    }
}

impl Default for Index {
    fn default() -> Self {
        Self::new()
    }
}
