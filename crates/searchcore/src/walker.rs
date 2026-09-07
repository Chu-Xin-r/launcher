//! 目录树遍历索引（降级路径）：非 NTFS 卷或无管理员权限时使用。
//! 慢于 MFT 直读（分钟级 vs 秒级），但建出的索引结构完全相同，搜索性能不变。

use std::os::windows::fs::MetadataExt;

use std::path::Path;

pub struct WalkStats {
    pub files: usize,
    pub dirs: usize,
}

/// 深度优先遍历 `root`，把每个文件/目录的完整路径交给 `sink`。
/// 不跟随重解析点（符号链接/junction），避免环路。
pub fn scan_directory(
    root: &str,
    skip_dirs: &[String],
    sink: &mut dyn FnMut(&str, bool),
) -> WalkStats {
    let mut stats = WalkStats { files: 0, dirs: 0 };
    // 注意：不要裁剪根路径的尾部反斜杠——"D:" 表示"D 盘当前目录"而非根目录
    let mut stack: Vec<String> = vec![root.to_string()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            let Some(path_str) = path.to_str() else { continue };
            if meta.is_dir() {
                // 跳过重解析点
                let attrs = meta.file_attributes();
                if attrs & 0x400 != 0 {
                    continue;
                }
                let name_lower = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(|s| s.to_ascii_lowercase())
                    .unwrap_or_default();
                if skip_dirs.iter().any(|s| *s == name_lower) {
                    continue;
                }
                stats.dirs += 1;
                sink(path_str, true);
                stack.push(path_str.to_string());
            } else if meta.is_file() {
                stats.files += 1;
                sink(path_str, false);
            }
        }
    }
    stats
}

/// 供外部判断某目录是否存在
pub fn dir_exists(p: &str) -> bool {
    Path::new(p).is_dir()
}
