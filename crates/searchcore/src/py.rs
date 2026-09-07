//! 拼音转换：汉字名 → 全拼小写 ASCII 连写（`微信.png` → `weixin.png`）。
//!
//! 只对含汉字的名字生成；结果直接进拼音池，供全拼子串匹配与
//! 首字母子序列匹配（首字母查询天然是全拼的子序列）共用。

/// 名字是否值得转换（含 CJK 统一表意文字才转）。
#[inline]
fn has_cjk(s: &str) -> bool {
    s.chars().any(|c| {
        matches!(c as u32,
            0x3400..=0x9FFF | 0xF900..=0xFAFF | 0x20000..=0x2FA1F)
    })
}

/// UTF-16 名字 → 全拼连写（ASCII 小写）。不含汉字返回 None。
pub fn pinyin_of(name: &[u16]) -> Option<Vec<u16>> {
    let s = String::from_utf16_lossy(name);
    if !has_cjk(&s) {
        return None;
    }
    let mut out = String::with_capacity(s.len() * 3);
    let mut changed = false;
    for c in s.chars() {
        match c.to_pinyin() {
            Some(p) => {
                out.push_str(p.plain());
                changed = true;
            }
            None => out.push(c),
        }
    }
    if !changed {
        return None;
    }
    Some(out.encode_utf16().collect())
}

use pinyin::ToPinyin;
