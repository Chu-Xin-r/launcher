//! 索引快照：把内存索引原子落盘，重启时先载快照再追 USN 增量，
//! 实现"重启 2 秒内可搜"。文件尾带 FNV-1a 校验和防损坏。

use std::fs;
use std::io::Read;
use std::path::Path;

use crate::index::{Index, VolumeIndex};

const MAGIC: u32 = u32::from_le_bytes(*b"QIDX");
const VERSION: u16 = 2;

#[inline]
fn fnv1a(data: &[u8], seed: u64) -> u64 {
    let mut h = if seed == 0 { 0xcbf2_9ce4_8422_2325 } else { seed };
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

/// 序列化索引为快照字节流（不含尾部校验和）。
///
/// 调用方持有索引读锁期间执行：只做内存拷贝，不做哈希与磁盘 IO。
pub fn encode(index: &Index) -> Vec<u8> {
    // 预估容量，避免序列化过程中的多次扩容拷贝
    let cap = 16
        + index
            .volumes
            .iter()
            .map(|v| {
                32 + v.names.len() * 2 + v.pys.len() * 2 + v.entries.len() * 24 + v.frns.len() * 8
            })
            .sum::<usize>();
    let mut body: Vec<u8> = Vec::with_capacity(cap);
    put_u32(&mut body, MAGIC);
    put_u16(&mut body, VERSION);
    put_u16(&mut body, index.volumes.len() as u16);
    for v in &index.volumes {
        body.push(v.letter as u8);
        body.push(v.is_ntfs as u8);
        put_u64(&mut body, v.journal_id);
        put_i64(&mut body, v.next_usn);
        put_u32(&mut body, v.names.len() as u32);
        put_u32(&mut body, v.pys.len() as u32);
        put_u32(&mut body, v.entries.len() as u32);
        put_u32(&mut body, v.frns.len() as u32);
        // 名字池 / 拼音池（u16 流）
        body.extend_from_slice(unsafe {
            std::slice::from_raw_parts(v.names.as_ptr() as *const u8, v.names.len() * 2)
        });
        body.extend_from_slice(unsafe {
            std::slice::from_raw_parts(v.pys.as_ptr() as *const u8, v.pys.len() * 2)
        });
        // 条目数组（24 字节/条）
        body.extend_from_slice(unsafe {
            std::slice::from_raw_parts(v.entries.as_ptr() as *const u8, v.entries.len() * 24)
        });
        // FRN 数组
        body.extend_from_slice(unsafe {
            std::slice::from_raw_parts(v.frns.as_ptr() as *const u8, v.frns.len() * 8)
        });
    }
    body
}

/// 追加校验和并原子写盘。**不持有索引锁**：校验和（269MB 全量哈希）与磁盘
/// 写入是秒级操作，放在锁内会把并发搜索排到写锁后面一起卡住。
pub fn write(mut body: Vec<u8>, path: &Path) -> Result<(), String> {
    let t0 = std::time::Instant::now();
    let sum = fnv1a(&body, 0);
    put_u64(&mut body, sum);

    let tmp = path.with_extension("tmp");
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("建目录失败: {e}"))?;
    }
    fs::write(&tmp, &body).map_err(|e| format!("写快照失败: {e}"))?;
    fs::rename(&tmp, path).map_err(|e| format!("替换快照失败: {e}"))?;
    eprintln!(
        "快照已保存: {} ({:.1} MB, {:.2}s)",
        path.display(),
        body.len() as f64 / 1048576.0,
        t0.elapsed().as_secs_f64()
    );
    Ok(())
}

/// 保存到 `path`（先写 .tmp 再原子替换）。
///
/// 注意：调用方持有索引读锁时，整个哈希+写盘过程都在锁内。启动/重建/退出
/// 等非热路径可直接用；周期保存（USN 线程）请改用 `encode` + `write` 分离。
pub fn save_snapshot(index: &Index, path: &Path) -> Result<(), String> {
    write(encode(index), path)
}

/// 载入快照。校验失败/版本不符返回 Err（调用方降级为重建）。
pub fn load_snapshot(path: &Path) -> Result<Index, String> {
    let t0 = std::time::Instant::now();
    let mut f = fs::File::open(path).map_err(|e| format!("打开快照失败: {e}"))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).map_err(|e| format!("读快照失败: {e}"))?;
    if buf.len() < 16 {
        return Err("快照过短".into());
    }
    let (body, tail) = buf.split_at(buf.len() - 8);
    let stored = u64::from_le_bytes(tail.try_into().unwrap());
    if fnv1a(body, 0) != stored {
        return Err("快照校验和不匹配".into());
    }
    let mut p = 0usize;
    let take = |p: &mut usize, n: usize| -> Result<&[u8], String> {
        if *p + n <= body.len() {
            let s = &body[*p..*p + n];
            *p += n;
            Ok(s)
        } else {
            Err("快照数据不完整".into())
        }
    };
    let magic = u32::from_le_bytes(take(&mut p, 4)?.try_into().unwrap());
    let ver = u16::from_le_bytes(take(&mut p, 2)?.try_into().unwrap());
    if magic != MAGIC || ver != VERSION {
        return Err("快照版本不兼容".into());
    }
    let vols = u16::from_le_bytes(take(&mut p, 2)?.try_into().unwrap());
    let mut index = Index::new();
    for _ in 0..vols {
        let letter = take(&mut p, 1)?[0] as char;
        let is_ntfs = take(&mut p, 1)?[0] != 0;
        let journal_id = u64::from_le_bytes(take(&mut p, 8)?.try_into().unwrap());
        let next_usn = i64::from_le_bytes(take(&mut p, 8)?.try_into().unwrap());
        let names_n = u32::from_le_bytes(take(&mut p, 4)?.try_into().unwrap()) as usize;
        let pys_n = u32::from_le_bytes(take(&mut p, 4)?.try_into().unwrap()) as usize;
        let entries_n = u32::from_le_bytes(take(&mut p, 4)?.try_into().unwrap()) as usize;
        let frns_n = u32::from_le_bytes(take(&mut p, 4)?.try_into().unwrap()) as usize;
        let names_bytes = take(&mut p, names_n * 2)?;
        let pys_bytes = take(&mut p, pys_n * 2)?;
        let entries_bytes = take(&mut p, entries_n * 24)?;
        let frns_bytes = take(&mut p, frns_n * 8)?;

        let mut vi = VolumeIndex::new(letter, is_ntfs);
        vi.journal_id = journal_id;
        vi.next_usn = next_usn;
        vi.names = names_bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        vi.pys = pys_bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        vi.entries = entries_bytes
            .chunks_exact(24)
            .map(|c| crate::types::Entry {
                parent: u64::from_le_bytes(c[0..8].try_into().unwrap()),
                name_off: u32::from_le_bytes(c[8..12].try_into().unwrap()),
                py_off: u32::from_le_bytes(c[12..16].try_into().unwrap()),
                name_len: u16::from_le_bytes(c[16..18].try_into().unwrap()),
                py_len: u16::from_le_bytes(c[18..20].try_into().unwrap()),
                flags: u16::from_le_bytes(c[20..22].try_into().unwrap()),
            })
            .collect();
        vi.frns = frns_bytes
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
            .collect();
        // 重建 FRN→槽位映射；老快照补 CJK 标志
        for (i, &frn) in vi.frns.iter().enumerate() {
            if frn != 0 {
                vi.frn_map.insert(frn, i as u32);
            }
        }
        for e in vi.entries.iter_mut() {
            if e.py_len > 0 {
                e.flags |= crate::types::FLAG_IS_CJK;
            }
        }
        index.volumes.push(vi);
    }
    eprintln!(
        "快照已载入: {} 条, 耗时 {:.2}s",
        index.total_entries(),
        t0.elapsed().as_secs_f64()
    );
    Ok(index)
}

fn put_u32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn put_u16(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn put_u64(v: &mut Vec<u8>, x: u64) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn put_i64(v: &mut Vec<u8>, x: i64) {
    v.extend_from_slice(&x.to_le_bytes());
}
