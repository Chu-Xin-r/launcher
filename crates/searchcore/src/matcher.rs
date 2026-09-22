//! 搜索匹配器：纯内存扫描 + 首字符预筛 + 分级打分 + rayon 多核 TopK。
//!
//! 热路径设计要点：
//! - 全部在 `entries` 数组与 `names` 池上进行，无每条目分配；
//! - 大小写折叠仅对 ASCII（CJK 无大小写，直接 u16 相等比较）；
//! - 每个分片维护有界堆，跨分片归并，1 字符查询也不会结果爆炸；
//! - 分片边界检查取消代数，输入变化立刻弃扫。

use rayon::prelude::*;

use crate::index::Index;
use crate::types::{CancelGen, FLAG_IS_PATH};
use std::sync::atomic::Ordering;

#[derive(Clone)]
pub struct SearchOptions {
    /// 返回条数
    pub limit: usize,
    /// 每分片候选池大小（≥ limit）
    pub top_k: usize,
    /// 是否包含 `$` 系统文件（false 时系统文件仅大幅降权，输入 `$` 开头查询时自然浮现）
    pub include_system: bool,
    /// 目录相对文件的分数偏移
    pub dirs_penalty: i32,
    /// 应用类文件（.exe/.lnk）的命中加成：用户高频打开应用，结果应靠前。
    /// 按命中档位缩放：前缀命中 ×6，词首/拼音 ×2，弱命中 ÷3。
    pub app_bonus: i32,
    /// 中文名（含汉字）提权
    pub cjk_bonus: i32,
    /// 开始菜单 Programs 子树提权（已安装软件快捷方式）
    pub start_menu_bonus: i32,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            limit: 50,
            top_k: 120,
            include_system: false,
            dirs_penalty: -700,
            app_bonus: 450,
            cjk_bonus: 400,
            start_menu_bonus: 1_500,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SearchResult {
    pub volume: usize,
    pub slot: u32,
    pub score: i32,
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    /// 命中起点/长度（u16 单位，用于 UI 高亮）
    pub match_start: u16,
    pub match_len: u16,
}

pub struct SearchOutcome {
    pub items: Vec<SearchResult>,
    /// true = 扫描中途被新查询取代（结果不完整，UI 应丢弃）
    pub cancelled: bool,
}

const CHUNK: usize = 16 * 1024;

#[inline(always)]
fn fold(c: u16) -> u16 {
    if c >= b'A' as u16 && c <= b'Z' as u16 {
        c + 32
    } else {
        c
    }
}

/// 候选：分数 + 定位（分数高者优先）
type Cand = (i32, u32, u16, u16); // score, slot, match_start, match_end

/// 对单个名字做子串匹配并打分，返回最佳命中的 (score, start, end)。
#[inline]
fn match_name(name: &[u16], q: &[u16]) -> Option<(i32, u16, u16)> {
    let qlen = q.len();
    let nlen = name.len();
    if qlen == 0 || nlen == 0 || nlen < qlen {
        return None;
    }
    let first = q[0];

    // 完全相等（快速路径）
    if nlen == qlen {
        let mut ok = true;
        for i in 0..qlen {
            if fold(name[i]) != q[i] {
                ok = false;
                break;
            }
        }
        if ok {
            return Some((10_000, 0, nlen as u16));
        }
    }

    let mut best: Option<(i32, u16)> = None; // (score, pos)
    let mut i = 0usize;
    while i <= nlen - qlen {
        if fold(name[i]) == first {
            // 验证完整子串
            let mut ok = true;
            for k in 1..qlen {
                if fold(name[i + k]) != q[k] {
                    ok = false;
                    break;
                }
            }
            if ok {
                let prev = if i > 0 { name[i - 1] } else { 0 };
                let word_start = i == 0
                    || prev == 0x20
                    || prev == b'-' as u16
                    || prev == b'_' as u16
                    || prev == b'.' as u16
                    || prev == b'(' as u16
                    || prev == b'[' as u16
                    || prev == b'{' as u16
                    || prev == b'@' as u16
                    || prev == b'#' as u16
                    || prev == b'+' as u16;
                let mut score = if i == 0 {
                    8_000
                } else if word_start {
                    6_000 - (i as i32) * 4
                } else {
                    4_000 - (i as i32) * 4
                };
                score -= (nlen as i32) / 4;
                if best.map_or(true, |(s, _)| score > s) {
                    best = Some((score, i as u16));
                }
                if i == 0 && qlen == nlen {
                    break; // 不可能更高了
                }
            }
        }
        i += 1;
    }
    best.map(|(s, p)| (s, p, p + qlen as u16))
}

/// 应用类文件名（.exe/.lnk，ASCII 大小写不敏感）。
#[inline]
fn is_app_name(name: &[u16]) -> bool {
    if name.len() < 4 {
        return false;
    }
    let t = &name[name.len() - 4..];
    if t[0] != b'.' as u16 {
        return false;
    }
    let a = (fold(t[1]), fold(t[2]), fold(t[3]));
    a == (b'e' as u16, b'x' as u16, b'e' as u16) || a == (b'l' as u16, b'n' as u16, b'k' as u16)
}

#[inline]
fn is_word_start(name: &[u16], i: usize) -> bool {
    if i == 0 {
        return true;
    }
    let p = name[i - 1];
    p == 0x20
        || p == b'-' as u16
        || p == b'_' as u16
        || p == b'.' as u16
        || p == b'(' as u16
        || p == b'[' as u16
        || p == b'{' as u16
        || p == b'@' as u16
        || p == b'#' as u16
        || p == b'+' as u16
}

/// 子序列模糊匹配：q 的字符按序全部出现在 name 中（"chr" → chrome）。
/// 两遍扫描：先试"严格词首"缩写匹配（vs → Visual Studio），失败再退回逐字符贪心。
/// 先试词首可避免贪心提前吃掉中段同名字符（如 Visual 的 "s"），把缩写信号判没了。
/// 返回 (score, start, end)，分数必须低于任何子串命中档位。
fn match_fuzzy(name: &[u16], q: &[u16]) -> Option<(i32, u16, u16)> {
    let qlen = q.len();
    let nlen = name.len();
    if qlen < 2 || nlen < qlen {
        return None;
    }

    // 第一遍：严格词首（缩写命中）。每个查询字符都必须在名字的某个词首处命中。
    let mut qi = 0usize;
    let mut first = 0u16;
    let mut last = 0u16;
    for (i, &c) in name.iter().enumerate() {
        if qi < qlen && fold(c) == q[qi] && is_word_start(name, i) {
            if qi == 0 {
                first = i as u16;
            }
            last = i as u16;
            qi += 1;
        }
    }
    if qi == qlen {
        // 缩写命中：每个查询字符都是名字中某单词的首字母，是强信号；
        // 配合 exe 加成可进入最高档（PCL → Plain Craft Launcher 2.exe）
        let span = last - first + 1;
        let score = 8_200 - ((span as i32 - qlen as i32) * 25) - (nlen as i32) / 4;
        return Some((score, first, last + 1));
    }

    // 第二遍：贪心匹配（任意位置命中即可）
    let mut first: Option<u16> = None;
    let mut last: u16 = 0;
    let mut word_bonus = 0i32;
    let mut qi = 0usize;
    for (i, &c) in name.iter().enumerate() {
        if qi < qlen && fold(c) == q[qi] {
            if first.is_none() {
                first = Some(i as u16);
            }
            if is_word_start(name, i) {
                word_bonus += 60;
            }
            last = i as u16;
            qi += 1;
        }
    }
    if qi < qlen {
        return None;
    }
    // 松散跨度重罚：命中间隔越大分越低
    let start = first.unwrap();
    let span = last - start + 1;
    let score = 2_200 + word_bonus - ((span as i32 - qlen as i32) * 18) - (nlen as i32) / 4;
    Some((score, start, last + 1))
}


/// 拼音匹配：全拼子串（"weixin"）> 首字母子序列（"wx"）。
/// 从头命中是最强信号（用户以缩写找应用），给高档位分。
fn match_pinyin(py: &[u16], q: &[u16]) -> Option<i32> {
    let qlen = q.len();
    let plen = py.len();
    if qlen == 0 || plen < qlen {
        return None;
    }
    // 全拼子串：从头命中 ≈ 名字前缀命中档位
    let first = q[0];
    let mut i = 0usize;
    while i <= plen - qlen {
        if fold(py[i]) == first {
            let mut ok = true;
            for k in 1..qlen {
                if fold(py[i + k]) != q[k] {
                    ok = false;
                    break;
                }
            }
            if ok {
                let base = if i == 0 { 6_800 } else { 3_400 };
                return Some(base - (i as i32) - (plen as i32) / 4);
            }
        }
        i += 1;
    }
    // 首字母（子序列）：从头开始（每个音节首字母连写）是高频用法
    if qlen >= 2 {
        let mut qi = 0usize;
        let mut span_start = 0usize;
        let mut span_end = 0usize;
        for (i, &c) in py.iter().enumerate() {
            if qi < qlen && fold(c) == q[qi] {
                if qi == 0 {
                    span_start = i;
                }
                span_end = i;
                qi += 1;
            }
        }
        if qi == qlen {
            let span = (span_end - span_start + 1) as i32;
            if span_start == 0 {
                // 从头首字母（"wx"→微信、"ys"→原神）：最强拼音信号，仅次于名字前缀/精确命中
                return Some(8_500 - (span - qlen as i32) * 40 - (plen as i32) / 4);
            }
            return Some(1_600 - (span - qlen as i32) * 20 - (plen as i32) / 4);
        }
    }
    None
}

/// 单关键词匹配：名字子串 > （2~8 字）名字模糊 / 全拼子串 > 拼音首字母。
/// 返回 (score, 高亮起点, 高亮终点)。
#[inline]
fn match_one(name: &[u16], py: &[u16], q: &[u16]) -> Option<(i32, u16, u16)> {
    if let Some(hit) = match_name(name, q) {
        return Some(hit);
    }
    let qlen = q.len();
    if (2..=8).contains(&qlen) {
        if let Some(hit) = match_fuzzy(name, q) {
            return Some(hit);
        }
    }
    if !py.is_empty() && qlen <= 20 {
        if let Some(s) = match_pinyin(py, q) {
            return Some((s, 0, 0)); // 拼音命中不做名字高亮
        }
    }
    None
}

/// 多关键词匹配（空格分隔 = AND）：每个词都必须命中；分数与高亮取第一个词。
#[inline]
fn match_terms(name: &[u16], py: &[u16], terms: &[&[u16]]) -> Option<(i32, u16, u16)> {
    let hit = match_one(name, py, terms[0])?;
    for t in &terms[1..] {
        match_one(name, py, t)?;
    }
    Some(hit)
}

fn scan_chunk(
    entries: &[crate::types::Entry],
    base_slot: u32,
    names: &[u16],
    pys: &[u16],
    terms: &[&[u16]],
    opts: &SearchOptions,
    top_k: usize,
) -> Vec<Cand> {
    let mut heap: std::collections::BinaryHeap<std::cmp::Reverse<Cand>> =
        std::collections::BinaryHeap::with_capacity(top_k + 1);
    let q0 = terms[0][0];

    for (ci, e) in entries.iter().enumerate() {
        let full = &names[e.name_off as usize..e.name_off as usize + e.name_len as usize];
        // 路径条目：只匹配最后一个 \ 之后的文件名部分
        let name: &[u16] = if e.flags & FLAG_IS_PATH != 0 {
            match full.iter().rposition(|&c| c == b'\\' as u16) {
                Some(p) => &full[p + 1..],
                None => full,
            }
        } else {
            full
        };
        // 拼音池切片（仅带拼音的条目参与拼音匹配）
        let py: &[u16] = if e.has_py() {
            &pys[e.py_off as usize..e.py_off as usize + e.py_len as usize]
        } else {
            &[]
        };
        // 多关键词 AND：全部命中才保留；分数/高亮取第一个词
        let Some((mut score, start, end)) = match_terms(name, py, terms) else {
            continue;
        };
        if e.is_system() && !opts.include_system && q0 != b'$' as u16 {
            score -= 3_000;
        }
        if e.is_dir() {
            score += opts.dirs_penalty;
        }
        if e.is_start_menu() {
            score += opts.start_menu_bonus;
        }
        if e.is_cjk() {
            score += opts.cjk_bonus;
        }
        if is_app_name(name) {
            score += if score >= 7_900 {
                opts.app_bonus * 6 // 名字前缀命中：qq → qq.exe 压过同名文件夹
            } else if score >= 5_300 {
                opts.app_bonus * 2 // 词首 / 全拼从头 / 首字母从头 / 缩写
            } else {
                opts.app_bonus / 3 // 弱命中（模糊、串中段）
            };
        }
        heap.push(std::cmp::Reverse((score, base_slot + ci as u32, start, end)));
        if heap.len() > top_k {
            heap.pop();
        }
    }
    heap.into_iter().map(|r| r.0).collect()
}

pub fn search(
    index: &Index,
    query: &str,
    cancel: Option<(&CancelGen, u64)>,
    opts: &SearchOptions,
) -> SearchOutcome {
    let q: Vec<u16> = query
        .encode_utf16()
        .map(|c| if c < 128 { fold(c) } else { c })
        .collect();
    if q.is_empty() {
        return SearchOutcome {
            items: Vec::new(),
            cancelled: false,
        };
    }
    // 多关键词：按空白（半角空格 / 全角空格 / 制表符）拆分，全部命中才算匹配（AND）。
    // 首个词决定分数与高亮；词数封顶 6，防止超长粘贴拖慢扫描。
    let terms: Vec<&[u16]> = q
        .split(|&c| c == b' ' as u16 || c == 0x3000 || c == b'\t' as u16)
        .filter(|t| !t.is_empty())
        .take(6)
        .collect();
    if terms.is_empty() {
        return SearchOutcome {
            items: Vec::new(),
            cancelled: false,
        };
    }

    // 1) 跨卷 + 卷内分片并行扫描，各自产出有界候选
    let vol_cands: Vec<(usize, Vec<Cand>, bool)> = index
        .volumes
        .par_iter()
        .enumerate()
        .map(|(vi, v)| {
            let cancelled = cancel
                .map(|(g, gen)| g.load(Ordering::Relaxed) > gen)
                .unwrap_or(false);
            if cancelled {
                return (vi, Vec::new(), true);
            }
            let mut cands: Vec<Cand> = v
                .entries
                .par_chunks(CHUNK)
                .enumerate()
                .map(|(ci, chunk)| {
                    scan_chunk(chunk, (ci * CHUNK) as u32, &v.names, &v.pys, &terms, opts, opts.top_k)
                })
                .reduce(
                    || Vec::new(),
                    |mut a, mut b| {
                        if a.len() < b.len() {
                            std::mem::swap(&mut a, &mut b);
                        }
                        a.extend(b.drain(..));
                        if a.len() > opts.top_k {
                            a.sort_unstable_by(|x, y| y.0.cmp(&x.0));
                            a.truncate(opts.top_k);
                        }
                        a
                    },
                );
            cands.sort_unstable_by(|x, y| y.0.cmp(&x.0));
            cands.truncate(opts.top_k);
            (vi, cands, false)
        })
        .collect();

    if cancel
        .map(|(g, gen)| g.load(Ordering::Relaxed) > gen)
        .unwrap_or(false)
    {
        return SearchOutcome {
            items: Vec::new(),
            cancelled: true,
        };
    }

    // 2) 归并全部卷的候选
    let mut merged: Vec<(usize, Cand)> = Vec::with_capacity(opts.top_k);
    for (vi, cands, _c) in &vol_cands {
        for c in cands {
            merged.push((*vi, *c));
        }
    }
    merged.sort_unstable_by(|a, b| b.1 .0.cmp(&a.1 .0));
    merged.truncate(opts.limit);

    // 3) 组装结果（只为最终 limit 条构建路径）
    let items = merged
        .into_iter()
        .map(|(vi, (score, slot, start, end))| {
            let v = &index.volumes[vi];
            let e = &v.entries[slot as usize];
            let full = v.name_slice(e);
            let name: String = if e.flags & FLAG_IS_PATH != 0 {
                match full.iter().rposition(|&c| c == b'\\' as u16) {
                    Some(p) => String::from_utf16_lossy(&full[p + 1..]),
                    None => String::from_utf16_lossy(full),
                }
            } else {
                String::from_utf16_lossy(full)
            };
            let path = if e.flags & FLAG_IS_PATH != 0 {
                String::from_utf16_lossy(full)
            } else {
                v.path_for_slot(slot)
            };
            SearchResult {
                volume: vi,
                slot,
                score,
                name,
                path,
                is_dir: e.is_dir(),
                match_start: start,
                match_len: end - start,
            }
        })
        .collect();

    SearchOutcome {
        items,
        cancelled: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16()
            .map(|c| if c < 128 { fold(c) } else { c })
            .collect()
    }

    #[test]
    fn multi_term_and_semantics() {
        let name = w("Visual Studio 2022.lnk");
        let t0 = w("vs");
        let t1 = w("2022");
        assert!(
            match_terms(&name, &[], &[t0.as_slice(), t1.as_slice()]).is_some(),
            "两个词都命中应保留（vs 缩写 + 2022 子串）"
        );
        let t2 = w("2023");
        assert!(
            match_terms(&name, &[], &[t0.as_slice(), t2.as_slice()]).is_none(),
            "有一个词不命中应被过滤（AND 语义）"
        );
    }

    #[test]
    fn single_term_scoring_unchanged() {
        let name = w("Plain Craft Launcher 2.exe");
        let t = w("pcl");
        let hit = match_terms(&name, &[], &[t.as_slice()]).unwrap();
        assert!(hit.0 >= 7_900, "缩写命中应进入模糊高档位: {}", hit.0);

        let name2 = w("微信安装包.exe");
        let t2 = w("微信");
        let hit2 = match_terms(&name2, &[], &[t2.as_slice()]).unwrap();
        assert!(hit2.0 >= 7_900, "中文前缀命中应为 8000 档: {}", hit2.0);
    }

    #[test]
    fn fuzzy_prefers_word_starts() {
        // vs → Visual Studio：贪心会先在 "Visual" 的 s（中段）停下；词首优先应命中 "Studio" 的 s
        let name = w("Visual Studio Code.lnk");
        let t = w("vs");
        let hit = match_one(&name, &[], &t).unwrap();
        assert!(hit.0 >= 7_900, "vs 应按缩写命中 Visual Studio: {}", hit.0);
    }
}
